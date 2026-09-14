//! Permission scopes attached to auth tokens.
//!
//! A connection that authenticates with a token inherits that token's
//! scopes for the life of the TCP session. Each subsequent request is
//! checked against those scopes; an out-of-scope request gets
//! `Response::Error` with a permission message.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Screenshot,
    Mouse,
    Keyboard,
    /// Distinct from Keyboard: `TypeText` convenience path.
    Type,
    /// Enumerate windows, watch them open and close, activate/move/close one,
    /// and launch installed apps. Deliberately separate from Screenshot: this
    /// scope reveals *titles*, not pixels, and a token can hold either alone.
    Window,
}

impl Scope {
    pub const ALL: [Scope; 5] = [
        Scope::Screenshot,
        Scope::Mouse,
        Scope::Keyboard,
        Scope::Type,
        Scope::Window,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Screenshot => "screenshot",
            Scope::Mouse => "mouse",
            Scope::Keyboard => "keyboard",
            Scope::Type => "type",
            Scope::Window => "window",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "screenshot" => Ok(Scope::Screenshot),
            "mouse" => Ok(Scope::Mouse),
            "keyboard" => Ok(Scope::Keyboard),
            "type" | "type_text" | "typetext" => Ok(Scope::Type),
            "window" | "windows" => Ok(Scope::Window),
            "all" => Err("use ScopeSet::all() for 'all'".into()),
            other => Err(format!("unknown scope '{other}'")),
        }
    }
}

/// Set of granted scopes. Empty set means "no permissions" (Ping still works).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScopeSet {
    inner: BTreeSet<Scope>,
}

impl ScopeSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all() -> Self {
        Self {
            inner: Scope::ALL.into_iter().collect(),
        }
    }

    pub fn from_scopes<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        Self {
            inner: iter.into_iter().collect(),
        }
    }

    /// Parse a comma-separated list. `"all"` (case-insensitive) expands to
    /// every scope. Empty string → empty set.
    pub fn parse_list(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::new());
        }
        if s.eq_ignore_ascii_case("all") {
            return Ok(Self::all());
        }
        let mut set = Self::new();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if part.eq_ignore_ascii_case("all") {
                return Ok(Self::all());
            }
            set.inner.insert(part.parse()?);
        }
        Ok(set)
    }

    pub fn contains(&self, scope: Scope) -> bool {
        self.inner.contains(&scope)
    }

    pub fn allows(&self, needed: Option<Scope>) -> bool {
        match needed {
            None => true,
            Some(s) => self.contains(s),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Scope> + '_ {
        self.inner.iter().copied()
    }

    pub fn to_string_list(&self) -> Vec<String> {
        self.inner.iter().map(|s| s.as_str().to_string()).collect()
    }

    pub fn is_all(&self) -> bool {
        Scope::ALL.iter().all(|s| self.inner.contains(s))
    }
}

impl fmt::Display for ScopeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_all() {
            return write!(f, "all");
        }
        let parts: Vec<&str> = self.inner.iter().map(|s| s.as_str()).collect();
        write!(f, "{}", parts.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all() {
        let s = ScopeSet::parse_list("all").unwrap();
        assert!(s.is_all());
        assert!(s.contains(Scope::Screenshot));
    }

    #[test]
    fn parse_subset() {
        let s = ScopeSet::parse_list("screenshot,mouse").unwrap();
        assert!(s.contains(Scope::Screenshot));
        assert!(s.contains(Scope::Mouse));
        assert!(!s.contains(Scope::Keyboard));
        assert!(s.allows(Some(Scope::Screenshot)));
        assert!(!s.allows(Some(Scope::Type)));
        assert!(s.allows(None)); // Ping
    }

    #[test]
    fn window_scope_parses_and_is_in_all() {
        let s = ScopeSet::parse_list("window").unwrap();
        assert!(s.contains(Scope::Window));
        assert!(!s.contains(Scope::Screenshot));
        // Tokens minted as "all" before the window plane existed must pick
        // it up, or every existing deployment loses window access on upgrade.
        assert!(ScopeSet::parse_list("all").unwrap().contains(Scope::Window));
        assert_eq!(ScopeSet::parse_list("windows").unwrap(), s);
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(ScopeSet::parse_list("screenshot,laser").is_err());
    }

    #[test]
    fn display_roundtrip_shape() {
        let s = ScopeSet::parse_list("keyboard,type").unwrap();
        let shown = s.to_string();
        assert!(shown.contains("keyboard"));
        assert!(shown.contains("type"));
    }
}
