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
}

fn default_port() -> u16 {
    common::DEFAULT_PORT
}

impl HostProfile {
    pub fn addr_port(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }

    pub fn ssh_target(&self) -> String {
        self.ssh
            .clone()
            .unwrap_or_else(|| self.address.clone())
    }
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
        format!("{}:{}", self.address, self.port)
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

/// Resolution order:
/// 1. Explicit addr + token flags (pin optional)
/// 2. Named `--host <profile>`
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
        let mut pin = pin_flag.map(|s| s.to_string());
        let mut ssh = None;
        let mut sudo_password = None;
        let mut user_password = None;
        let mut name = None;
        if let Some(h) = host_flag {
            if let Some(p) = cfg.hosts.get(h) {
                name = Some(h.to_string());
                if pin.is_none() {
                    pin = p.pin.clone();
                }
                ssh = p.ssh.clone();
                sudo_password = p.sudo_password.clone();
                user_password = p.user_password.clone();
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
        h.to_string()
    } else if let Some(d) = &cfg.default_host {
        d.clone()
    } else if cfg.hosts.len() == 1 {
        cfg.hosts.keys().next().unwrap().clone()
    } else if cfg.hosts.is_empty() {
        bail!(
            "no connection configured. Pass --addr/--token, or add a host:\n  \
             gdr host add <name> --address IP --token TOKEN [--pin FP]"
        );
    } else {
        bail!(
            "multiple hosts configured ({}); pass --host <name> or set default_host",
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
    if let Some((host, port)) = addr.rsplit_once(':') {
        if !host.is_empty() && port.parse::<u16>().is_ok() && !host.contains("://") {
            // Avoid treating IPv6 without brackets wrongly: if host has
            // multiple ':', require brackets. Simple path for IPv4/hostname.
            if host.matches(':').count() == 0 {
                return Ok((host.to_string(), port.parse()?));
            }
        }
    }
    // Bare host → default port
    Ok((addr.to_string(), default_port()))
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
        Some(h) => h.to_string(),
        None => match &cfg.default_host {
            Some(d) => d.clone(),
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
    }
}
