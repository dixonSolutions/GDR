//! Multi-token auth store for gdrd.
//!
//! Stored at `~/.local/share/gdr/tokens.json` on the **target**. Only
//! SHA-256 hashes of tokens are persisted — the plaintext is shown once
//! at creation (via `gdr token create` / deploy.sh) and never again.
//!
//! Token management is an SSH/admin-plane concern. This module is the
//! on-disk format the daemon reads at Auth time; the CLI edits the same
//! file over SSH. Keeping mint/revoke off the live TCP protocol means a
//! leaked scoped data-plane token cannot mint itself more tokens.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use common::ScopeSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

const DEFAULT_REL: &str = ".local/share/gdr/tokens.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    pub id: String,
    pub token_hash: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    /// `null` / omitted = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Stored as string list ("screenshot", "mouse", ...) or the word "all".
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenStoreFile {
    pub tokens: Vec<TokenEntry>,
}

#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub id: String,
    pub label: String,
    pub scopes: ScopeSet,
}

/// Thread-safe view of the token store, reloadable from disk.
#[derive(Clone)]
pub struct TokenStore {
    path: PathBuf,
    inner: Arc<RwLock<TokenStoreFile>>,
    /// Optional legacy single bearer from GDR_TOKEN env — treated as
    /// scope=all, never-expire, id="env". Prefer tokens.json once present.
    legacy_token_hash: Option<String>,
}

impl TokenStore {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(DEFAULT_REL)
    }

    pub fn load(path: PathBuf, legacy_plaintext: Option<&str>) -> Result<Self> {
        let file = if path.exists() {
            let data = fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&data)
                .with_context(|| format!("parsing {}", path.display()))?
        } else {
            TokenStoreFile::default()
        };
        Ok(Self {
            path,
            inner: Arc::new(RwLock::new(file)),
            legacy_token_hash: legacy_plaintext.map(hash_token),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reload(&self) -> Result<()> {
        if !self.path.exists() {
            *self.inner.write().unwrap() = TokenStoreFile::default();
            return Ok(());
        }
        let data = fs::read_to_string(&self.path)?;
        let file: TokenStoreFile = serde_json::from_str(&data)?;
        *self.inner.write().unwrap() = file;
        Ok(())
    }

    pub fn save_file(path: &Path, file: &TokenStoreFile) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(file)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn authenticate(&self, plaintext: &str) -> Result<AuthInfo> {
        // Reload so SSH-side revoke/create takes effect without restart.
        let _ = self.reload();
        let hash = hash_token(plaintext);
        let now = Utc::now();

        {
            let mut guard = self.inner.write().unwrap();
            if let Some(entry) = guard.tokens.iter_mut().find(|t| t.token_hash == hash) {
                if entry.revoked {
                    bail!("token revoked");
                }
                if let Some(exp) = entry.expires_at {
                    if now > exp {
                        bail!("token expired at {exp}");
                    }
                }
                let scopes = ScopeSet::parse_list(&join_scopes(&entry.scopes))
                    .unwrap_or_else(|_| ScopeSet::all());
                entry.last_used_at = Some(now);
                let info = AuthInfo {
                    id: entry.id.clone(),
                    label: entry.label.clone(),
                    scopes,
                };
                // Persist last_used_at best-effort.
                let snapshot = guard.clone();
                drop(guard);
                let _ = Self::save_file(&self.path, &snapshot);
                return Ok(info);
            }
        }

        if let Some(legacy) = &self.legacy_token_hash {
            if &hash == legacy {
                return Ok(AuthInfo {
                    id: "env".into(),
                    label: "GDR_TOKEN".into(),
                    scopes: ScopeSet::all(),
                });
            }
        }

        bail!("unknown token");
    }

    /// Seed an initial never-expire, full-scope token (used by deploy).
    pub fn ensure_initial_token(path: &Path, plaintext: &str, label: &str) -> Result<String> {
        let mut file = if path.exists() {
            serde_json::from_str(&fs::read_to_string(path)?)?
        } else {
            TokenStoreFile::default()
        };

        let hash = hash_token(plaintext);
        if let Some(existing) = file.tokens.iter().find(|t| t.token_hash == hash) {
            return Ok(existing.id.clone());
        }

        let id = new_token_id();
        file.tokens.push(TokenEntry {
            id: id.clone(),
            token_hash: hash,
            label: label.to_string(),
            created_at: Utc::now(),
            expires_at: None,
            scopes: vec!["all".into()],
            last_used_at: None,
            revoked: false,
        });
        Self::save_file(path, &file)?;
        Ok(id)
    }

    pub fn create_token(
        path: &Path,
        plaintext: &str,
        label: &str,
        scopes: ScopeSet,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<TokenEntry> {
        let mut file = if path.exists() {
            serde_json::from_str(&fs::read_to_string(path)?)?
        } else {
            TokenStoreFile::default()
        };
        let entry = TokenEntry {
            id: new_token_id(),
            token_hash: hash_token(plaintext),
            label: label.to_string(),
            created_at: Utc::now(),
            expires_at,
            scopes: if scopes.is_all() {
                vec!["all".into()]
            } else {
                scopes.to_string_list()
            },
            last_used_at: None,
            revoked: false,
        };
        file.tokens.push(entry.clone());
        Self::save_file(path, &file)?;
        Ok(entry)
    }

    pub fn list(path: &Path) -> Result<Vec<TokenEntry>> {
        if !path.exists() {
            return Ok(vec![]);
        }
        let file: TokenStoreFile = serde_json::from_str(&fs::read_to_string(path)?)?;
        Ok(file.tokens)
    }

    pub fn revoke(path: &Path, id: &str) -> Result<()> {
        let mut file: TokenStoreFile = serde_json::from_str(&fs::read_to_string(path)?)?;
        let entry = file
            .tokens
            .iter_mut()
            .find(|t| t.id == id)
            .with_context(|| format!("no token with id {id}"))?;
        entry.revoked = true;
        Self::save_file(path, &file)?;
        Ok(())
    }
}

pub fn hash_token(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    hex::encode(digest)
}

pub fn generate_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn new_token_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    format!("tok_{}", hex::encode(buf))
}

fn join_scopes(scopes: &[String]) -> String {
    if scopes.iter().any(|s| s.eq_ignore_ascii_case("all")) {
        "all".into()
    } else {
        scopes.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::Scope;
    use tempfile::tempdir;

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        assert_eq!(hash_token("abc").len(), 64);
    }

    #[test]
    fn create_auth_revoke() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let plain = generate_token();
        let entry = TokenStore::create_token(
            &path,
            &plain,
            "test",
            ScopeSet::parse_list("screenshot,mouse").unwrap(),
            None,
        )
        .unwrap();

        let store = TokenStore::load(path.clone(), None).unwrap();
        let info = store.authenticate(&plain).unwrap();
        assert_eq!(info.id, entry.id);
        assert!(info.scopes.contains(Scope::Screenshot));
        assert!(!info.scopes.contains(Scope::Keyboard));

        TokenStore::revoke(&path, &entry.id).unwrap();
        let store = TokenStore::load(path.clone(), None).unwrap();
        assert!(store.authenticate(&plain).is_err());
    }

    #[test]
    fn expiry_enforced() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let plain = generate_token();
        let past = Utc::now() - chrono::Duration::hours(1);
        TokenStore::create_token(&path, &plain, "old", ScopeSet::all(), Some(past)).unwrap();
        let store = TokenStore::load(path, None).unwrap();
        let err = store.authenticate(&plain).unwrap_err().to_string();
        assert!(err.contains("expired"), "err={err}");
    }

    #[test]
    fn legacy_env_token() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let store = TokenStore::load(path, Some("legacy-secret")).unwrap();
        let info = store.authenticate("legacy-secret").unwrap();
        assert_eq!(info.id, "env");
        assert!(info.scopes.is_all());
        assert!(store.authenticate("wrong").is_err());
    }

    #[test]
    fn ensure_initial_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        let plain = "same-token";
        let id1 = TokenStore::ensure_initial_token(&path, plain, "initial").unwrap();
        let id2 = TokenStore::ensure_initial_token(&path, plain, "initial").unwrap();
        assert_eq!(id1, id2);
        assert_eq!(TokenStore::list(&path).unwrap().len(), 1);
    }

    #[test]
    fn file_is_chmod_600() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        TokenStore::ensure_initial_token(&path, "x", "l").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
