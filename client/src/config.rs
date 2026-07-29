//! Shared host-profile store at ~/.config/gdr/config.json — "remembered
//! connections", read by both this CLI and mcp-server/src/config.ts (kept
//! in sync by hand; same JSON shape, snake_case field names on both sides
//! so the file is directly interoperable).
//!
//! SECURITY NOTE: sudo_password/user_password, if you choose to store
//! them, are kept in PLAINTEXT in this file. The file is chmod 600 on
//! write, but plaintext-on-disk is plaintext-on-disk — anyone with root
//! or access to your user account can read it. Only store these if you
//! understand and accept that, and only on machines you trust. See the
//! README security note for the tradeoffs of the MCP gdr_get_password
//! tool that reads these.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_host: Option<String>,
    #[serde(default)]
    pub hosts: HashMap<String, HostProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostProfile {
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// SSH target for admin-plane ops (token create/revoke, audit, deploy).
    /// e.g. "borys@100.118.238.2". Optional — falls back to address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_password: Option<String>,
    /// Human-friendly display name (e.g. "home computer"). Also matched by
    /// `--host` / MCP `dev=` lookups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Extra names that resolve to this device (case-insensitive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

fn default_port() -> u16 {
    common::DEFAULT_PORT
}

impl HostProfile {
    pub fn addr_port(&self) -> String {
        format!("{}:{}", normalize_address(&self.address), self.port)
    }

    pub fn ssh_target(&self) -> String {
        self.ssh.clone().unwrap_or_else(|| {
            let addr = normalize_address(&self.address);
            if is_loopback_alias(&addr) || is_loopback_alias(&self.address) {
                // Same-machine profiles: admin plane is local user, not an IP.
                whoami_user().unwrap_or_else(|| "localhost".into())
            } else {
                self.address.clone()
            }
        })
    }
}

/// True for addresses that mean "this machine" (no LAN/Tailscale IP required).
pub fn is_loopback_alias(address: &str) -> bool {
    matches!(
        address.trim().to_ascii_lowercase().as_str(),
        "" | "local" | "localhost" | "loopback" | "this" | "." | "127.0.0.1" | "::1"
    )
}

/// Chat/CLI shorthand for "this machine" as a device query (`--host=me`).
pub fn is_self_device_query(query: &str) -> bool {
    matches!(
        query.trim().to_ascii_lowercase().as_str(),
        "me" | "self" | "this-machine" | "thismachine" | "here"
    )
}

/// Prefer profile id `me`, then `local`, then any loopback-addressed host.
pub fn find_same_machine_device(cfg: &Config) -> Option<String> {
    if cfg.hosts.contains_key("me") && is_loopback_alias(&cfg.hosts["me"].address) {
        return Some("me".into());
    }
    if cfg.hosts.contains_key("local") && is_loopback_alias(&cfg.hosts["local"].address) {
        return Some("local".into());
    }
    cfg.hosts
        .iter()
        .find(|(_, p)| is_loopback_alias(&p.address))
        .map(|(name, _)| name.clone())
}

/// Map same-machine aliases to a connectable loopback host.
/// Profiles may store `local` / `localhost` so config never needs a real IP.
pub fn normalize_address(address: &str) -> String {
    let trimmed = address.trim();
    if is_loopback_alias(trimmed) {
        "127.0.0.1".to_string()
    } else {
        trimmed.to_string()
    }
}

fn whoami_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|s| !s.is_empty()))
}

/// Resolved connection parameters after applying flags → profile → default.
#[derive(Debug, Clone)]
#[allow(dead_code)] // ssh/passwords used by admin subcommands via Config directly today
pub struct ResolvedHost {
    pub name: Option<String>,
    pub address: String,
    pub port: u16,
    pub token: String,
    pub pin: Option<String>,
    pub ssh: Option<String>,
    pub sudo_password: Option<String>,
    pub user_password: Option<String>,
}

impl ResolvedHost {
    pub fn addr_port(&self) -> String {
        format!("{}:{}", normalize_address(&self.address), self.port)
    }
}

pub fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("HOME not set / could not resolve home dir")?;
    Ok(home.join(".config").join("gdr").join("config.json"))
}

pub fn load() -> Result<Config> {
    load_from(&config_path()?)
}

pub fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let data =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
}

pub fn save(cfg: &Config) -> Result<()> {
    save_to(&config_path()?, cfg)
}

pub fn save_to(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(cfg)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolve a device query to the canonical profile key.
/// Matches: exact key, case-insensitive key, `label`, or any `alias`.
/// Also: `me` / `self` / `here` → same-machine profile (`local`, etc.).
pub fn find_device_name(cfg: &Config, query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if is_self_device_query(q) {
        return find_same_machine_device(cfg);
    }
    if cfg.hosts.contains_key(q) {
        return Some(q.to_string());
    }
    let ql = q.to_ascii_lowercase();
    for (name, p) in &cfg.hosts {
        if name.to_ascii_lowercase() == ql {
            return Some(name.clone());
        }
        if p.label
            .as_deref()
            .is_some_and(|l| l.trim().eq_ignore_ascii_case(q))
        {
            return Some(name.clone());
        }
        if p.aliases
            .iter()
            .any(|a| a.trim().eq_ignore_ascii_case(q))
        {
            return Some(name.clone());
        }
    }
    None
}

pub fn known_device_names(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for (name, p) in &cfg.hosts {
        out.push(name.clone());
        if let Some(l) = &p.label {
            out.push(format!("{name} (label: {l})"));
        }
        for a in &p.aliases {
            out.push(format!("{name} (alias: {a})"));
        }
    }
    out.sort();
    out
}

/// Resolution order:
/// 1. Explicit addr + token flags (pin optional)
/// 2. Named `--host` / device query (id, label, or alias)
/// 3. `default_host`
/// 4. If exactly one profile exists, that one
pub fn resolve(
    cfg: &Config,
    host_flag: Option<&str>,
    addr_flag: Option<&str>,
    token_flag: Option<&str>,
    pin_flag: Option<&str>,
) -> Result<ResolvedHost> {
    // Explicit addr+token wins entirely (pin may still come from profile if
    // a host name is also given — but typically flags are complete).
    if let (Some(addr), Some(token)) = (addr_flag, token_flag) {
        let (address, port) = split_addr(addr)?;
        let address = normalize_address(&address);
        let mut pin = pin_flag.map(|s| s.to_string());
        let mut ssh = None;
        let mut sudo_password = None;
        let mut user_password = None;
        let mut name = None;
        if let Some(h) = host_flag {
            if let Some(canon) = find_device_name(cfg, h) {
                if let Some(p) = cfg.hosts.get(&canon) {
                    name = Some(canon);
                    if pin.is_none() {
                        pin = p.pin.clone();
                    }
                    ssh = p.ssh.clone();
                    sudo_password = p.sudo_password.clone();
                    user_password = p.user_password.clone();
                }
            }
        }
        return Ok(ResolvedHost {
            name,
            address,
            port,
            token: token.to_string(),
            pin,
            ssh,
            sudo_password,
            user_password,
        });
    }

    let name = if let Some(h) = host_flag {
        find_device_name(cfg, h).with_context(|| {
            format!(
                "unknown device '{h}'. Known: {}",
                known_device_names(cfg).join(", ")
            )
        })?
    } else if let Some(d) = &cfg.default_host {
        find_device_name(cfg, d)
            .or_else(|| Some(d.clone()))
            .filter(|n| cfg.hosts.contains_key(n))
            .with_context(|| format!("default_host '{d}' not found in hosts"))?
    } else if cfg.hosts.len() == 1 {
        cfg.hosts.keys().next().unwrap().clone()
    } else if cfg.hosts.is_empty() {
        bail!(
            "no connection configured. Pass --addr/--token, or add a device:\n  \
             gdr device add <id> --address IP --token TOKEN [--label \"home computer\"]"
        );
    } else {
        bail!(
            "multiple devices configured ({}); pass --host/--dev <id|label> or set default",
            cfg.hosts.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };

    let profile = cfg
        .hosts
        .get(&name)
        .with_context(|| format!("unknown host profile '{name}'"))?;

    let (address, port) = if let Some(addr) = addr_flag {
        split_addr(addr)?
    } else {
        (profile.address.clone(), profile.port)
    };
    let address = normalize_address(&address);
    let token = token_flag
        .map(|s| s.to_string())
        .unwrap_or_else(|| profile.token.clone());
    let pin = pin_flag
        .map(|s| s.to_string())
        .or_else(|| profile.pin.clone());

    Ok(ResolvedHost {
        name: Some(name),
        address,
        port,
        token,
        pin,
        ssh: profile.ssh.clone(),
        sudo_password: profile.sudo_password.clone(),
        user_password: profile.user_password.clone(),
    })
}

pub fn split_addr(addr: &str) -> Result<(String, u16)> {
    // Same-machine aliases (optionally with :port)
    let trimmed = addr.trim();
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if is_loopback_alias(host) && port.parse::<u16>().is_ok() {
            return Ok(("127.0.0.1".into(), port.parse()?));
        }
        if !host.is_empty() && port.parse::<u16>().is_ok() && !host.contains("://") {
            // Avoid treating IPv6 without brackets wrongly: if host has
            // multiple ':', require brackets. Simple path for IPv4/hostname.
            if host.matches(':').count() == 0 {
                return Ok((normalize_address(host), port.parse()?));
            }
        }
    }
    if is_loopback_alias(trimmed) {
        return Ok(("127.0.0.1".into(), default_port()));
    }
    // Bare host → default port
    Ok((trimmed.to_string(), default_port()))
}

pub fn upsert_host(cfg: &mut Config, name: &str, profile: HostProfile) {
    cfg.hosts.insert(name.to_string(), profile);
    if cfg.default_host.is_none() {
        cfg.default_host = Some(name.to_string());
    }
}

#[allow(dead_code)]
pub fn get_password<'a>(host: &'a HostProfile, kind: &str) -> Result<&'a str> {
    match kind {
        "sudo" => host
            .sudo_password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("No sudo password is set for this host.")),
        "user" => host
            .user_password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("No user password is set for this host.")),
        other => bail!("unknown password kind '{other}', use sudo|user"),
    }
}

/// Message form used by MCP / --json: never panics, always a clear string.
pub fn password_message(cfg: &Config, host: Option<&str>, kind: &str) -> String {
    let name = match host {
        Some(h) => match find_device_name(cfg, h) {
            Some(n) => n,
            None => return format!("Unknown device '{h}'."),
        },
        None => match &cfg.default_host {
            Some(d) => find_device_name(cfg, d).unwrap_or_else(|| d.clone()),
            None if cfg.hosts.len() == 1 => cfg.hosts.keys().next().unwrap().clone(),
            _ => return "No host specified and no default_host is set.".into(),
        },
    };
    let Some(profile) = cfg.hosts.get(&name) else {
        return format!("Unknown host profile '{name}'.");
    };
    match kind {
        "sudo" => match &profile.sudo_password {
            Some(p) if !p.is_empty() => p.clone(),
            _ => format!("No sudo password is set for host '{name}'."),
        },
        "user" => match &profile.user_password {
            Some(p) if !p.is_empty() => p.clone(),
            _ => format!("No user password is set for host '{name}'."),
        },
        other => format!("Unknown password kind '{other}'. Use sudo or user."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> Config {
        let mut cfg = Config::default();
        upsert_host(
            &mut cfg,
            "laptop",
            HostProfile {
                address: "192.168.1.50".into(),
                port: 7337,
                token: "tok".into(),
                pin: Some("abcd".into()),
                ssh: Some("borys@192.168.1.50".into()),
                sudo_password: Some("s3cret".into()),
                user_password: None,
                label: Some("home computer".into()),
                aliases: vec!["home".into()],
            },
        );
        cfg
    }

    #[test]
    fn resolve_default_host() {
        let cfg = sample();
        let r = resolve(&cfg, None, None, None, None).unwrap();
        assert_eq!(r.address, "192.168.1.50");
        assert_eq!(r.token, "tok");
        assert_eq!(r.pin.as_deref(), Some("abcd"));
    }

    #[test]
    fn resolve_flags_override() {
        let cfg = sample();
        let r = resolve(
            &cfg,
            Some("laptop"),
            Some("10.0.0.1:9000"),
            Some("other"),
            None,
        )
        .unwrap();
        assert_eq!(r.address, "10.0.0.1");
        assert_eq!(r.port, 9000);
        assert_eq!(r.token, "other");
        assert_eq!(r.pin.as_deref(), Some("abcd")); // from profile
    }

    #[test]
    fn password_message_not_set() {
        let cfg = sample();
        let msg = password_message(&cfg, Some("laptop"), "user");
        assert!(msg.contains("No user password"));
        assert_eq!(password_message(&cfg, Some("laptop"), "sudo"), "s3cret");
    }

    #[test]
    fn save_load_chmod() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = sample();
        save_to(&path, &cfg).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn split_addr_variants() {
        assert_eq!(split_addr("1.2.3.4:9").unwrap(), ("1.2.3.4".into(), 9));
        assert_eq!(split_addr("host").unwrap(), ("host".into(), 7337));
        assert_eq!(split_addr("local").unwrap(), ("127.0.0.1".into(), 7337));
        assert_eq!(split_addr("localhost:9000").unwrap(), ("127.0.0.1".into(), 9000));
    }

    #[test]
    fn resolve_local_profile_no_ip() {
        let mut cfg = Config::default();
        upsert_host(
            &mut cfg,
            "local",
            HostProfile {
                address: "local".into(),
                port: 7337,
                token: "tok".into(),
                pin: None,
                ssh: None,
                sudo_password: None,
                user_password: None,
                label: None,
                aliases: vec![],
            },
        );
        let r = resolve(&cfg, Some("local"), None, None, None).unwrap();
        assert_eq!(r.address, "127.0.0.1");
        assert_eq!(r.addr_port(), "127.0.0.1:7337");
    }

    #[test]
    fn resolve_by_label_and_alias() {
        let cfg = sample();
        let by_label = resolve(&cfg, Some("home computer"), None, None, None).unwrap();
        assert_eq!(by_label.name.as_deref(), Some("laptop"));
        let by_alias = resolve(&cfg, Some("HOME"), None, None, None).unwrap();
        assert_eq!(by_alias.name.as_deref(), Some("laptop"));
    }
}
