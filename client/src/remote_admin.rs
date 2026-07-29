//! Admin-plane operations for token create/list/revoke and audit.
//!
//! Never goes through the live TLS data plane — so a leaked scoped token
//! cannot mint wider tokens.
//!
//! - **Same-machine** profiles (`localhost` / `local` / …): read/write
//!   `~/.local/share/gdr/tokens.json` directly (no SSH).
//! - **Remote** profiles: same files over SSH.

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTokenEntry {
    pub id: String,
    pub token_hash: String,
    pub label: String,
    pub created_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TokenStoreFile {
    tokens: Vec<RemoteTokenEntry>,
}

/// Where admin-plane ops run.
#[derive(Debug, Clone)]
pub enum AdminTarget {
    /// This machine — `~/.local/share/gdr/`.
    Local,
    /// `user@host` (or bare host) over SSH.
    Ssh(String),
}

const REMOTE_TOKENS: &str = "$HOME/.local/share/gdr/tokens.json";
const REMOTE_AUDIT: &str = "$HOME/.local/share/gdr/audit.log";

fn local_tokens_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".local/share/gdr/tokens.json")
}

fn local_audit_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".local/share/gdr/audit.log")
}

fn load_store(path: &Path) -> Result<TokenStoreFile> {
    if !path.exists() {
        return Ok(TokenStoreFile::default());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).context("parsing tokens.json")
}

fn save_store(path: &Path, store: &TokenStoreFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(store)? + "\n";
    fs::write(path, raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn ssh_script(target: &str, script: &str) -> Result<String> {
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg(target)
        .arg("bash")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh")?;

    {
        use std::io::Write;
        let mut stdin = child.stdin.take().context("ssh stdin")?;
        stdin.write_all(script.as_bytes())?;
    }

    let output = child.wait_with_output().context("waiting for ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("ssh {target} failed: {stderr}{stdout}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn list_tokens(target: &AdminTarget) -> Result<Vec<RemoteTokenEntry>> {
    match target {
        AdminTarget::Local => Ok(load_store(&local_tokens_path())?.tokens),
        AdminTarget::Ssh(ssh_target) => {
            let script = format!(
                r#"
set -euo pipefail
f={REMOTE_TOKENS}
if [ ! -f "$f" ]; then echo '{{"tokens":[]}}'; exit 0; fi
cat "$f"
"#
            );
            let out = ssh_script(ssh_target, &script)?;
            let file: TokenStoreFile =
                serde_json::from_str(&out).context("parsing remote tokens.json")?;
            Ok(file.tokens)
        }
    }
}

pub fn revoke_token(target: &AdminTarget, id: &str) -> Result<()> {
    match target {
        AdminTarget::Local => {
            let path = local_tokens_path();
            let mut store = load_store(&path)?;
            let mut found = false;
            for t in &mut store.tokens {
                if t.id == id {
                    t.revoked = true;
                    found = true;
                }
            }
            if !found {
                bail!("no token with id {id}");
            }
            save_store(&path, &store)?;
            eprintln!("revoked {id}");
            Ok(())
        }
        AdminTarget::Ssh(ssh_target) => {
            let id_json = serde_json::to_string(id)?;
            let script = format!(
                r#"
set -euo pipefail
f={REMOTE_TOKENS}
python3 - <<'PY'
import json, pathlib, sys
path = pathlib.Path.home() / ".local/share/gdr/tokens.json"
data = json.loads(path.read_text()) if path.exists() else {{"tokens": []}}
tid = {id_json}
found = False
for t in data.get("tokens", []):
    if t.get("id") == tid:
        t["revoked"] = True
        found = True
if not found:
    sys.stderr.write(f"no token with id {{tid}}\n")
    sys.exit(1)
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(json.dumps(data, indent=2))
path.chmod(0o600)
print("revoked", tid)
PY
"#
            );
            let out = ssh_script(ssh_target, &script)?;
            eprintln!("{out}");
            Ok(())
        }
    }
}

pub fn create_token(
    target: &AdminTarget,
    label: &str,
    scopes: &str,
    expires: &str,
) -> Result<(String, String)> {
    let plaintext = generate_token();
    let hash = hash_token(&plaintext);
    let id = format!("tok_{}", &hash[..16]);
    let now = Utc::now().to_rfc3339();
    let expires_at = parse_expires(expires)?;
    let scopes_vec: Vec<String> = if scopes.eq_ignore_ascii_case("all") {
        vec!["all".into()]
    } else {
        scopes
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    match target {
        AdminTarget::Local => {
            let path = local_tokens_path();
            let mut store = load_store(&path)?;
            store.tokens.push(RemoteTokenEntry {
                id: id.clone(),
                token_hash: hash,
                label: label.to_string(),
                created_at: now,
                expires_at,
                scopes: scopes_vec,
                last_used_at: None,
                revoked: false,
            });
            save_store(&path, &store)?;
            Ok((id, plaintext))
        }
        AdminTarget::Ssh(ssh_target) => {
            let expires_json = match &expires_at {
                Some(e) => format!("\"{e}\""),
                None => "null".into(),
            };
            let scopes_json = serde_json::to_string(&scopes_vec)?;
            let label_json = serde_json::to_string(label)?;
            let id_json = serde_json::to_string(&id)?;
            let hash_json = serde_json::to_string(&hash)?;

            let script = format!(
                r#"
set -euo pipefail
python3 - <<'PY'
import json, pathlib
path = pathlib.Path.home() / ".local/share/gdr/tokens.json"
data = json.loads(path.read_text()) if path.exists() else {{"tokens": []}}
entry = {{
  "id": {id_json},
  "token_hash": {hash_json},
  "label": {label_json},
  "created_at": "{now}",
  "expires_at": {expires_json},
  "scopes": {scopes_json},
  "last_used_at": None,
  "revoked": False,
}}
data.setdefault("tokens", []).append(entry)
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(json.dumps(data, indent=2))
path.chmod(0o600)
print(entry["id"])
PY
"#
            );
            let remote_id = ssh_script(ssh_target, &script)?;
            Ok((remote_id, plaintext))
        }
    }
}

pub fn audit_tail(
    target: &AdminTarget,
    lines: usize,
    since: Option<&str>,
    token_id: Option<&str>,
) -> Result<String> {
    match target {
        AdminTarget::Local => {
            let path = local_audit_path();
            if !path.exists() {
                return Ok("(no audit log yet)".into());
            }
            let raw = fs::read_to_string(&path)?;
            let mut out: Vec<&str> = raw.lines().collect();
            if let Some(id) = token_id {
                out.retain(|l| l.contains(id));
            }
            let _ = since; // hint only, same as remote
            let start = out.len().saturating_sub(lines);
            Ok(out[start..].join("\n"))
        }
        AdminTarget::Ssh(ssh_target) => {
            let since_filter = match since {
                Some(s) => format!("# since filter hint: {s}\n"),
                None => String::new(),
            };
            let token_filter = match token_id {
                Some(id) => format!(" | grep -F {id:?} || true"),
                None => String::new(),
            };
            let script = format!(
                r#"
set -euo pipefail
f={REMOTE_AUDIT}
{since_filter}
if [ ! -f "$f" ]; then echo "(no audit log yet)"; exit 0; fi
tail -n {lines} "$f"{token_filter}
"#
            );
            ssh_script(ssh_target, &script)
        }
    }
}

fn parse_expires(spec: &str) -> Result<Option<String>> {
    let s = spec.trim().to_ascii_lowercase();
    if s.is_empty() || s == "never" || s == "none" {
        return Ok(None);
    }
    let dur = if let Some(d) = s.strip_suffix('d') {
        Duration::days(d.parse().context("expires days")?)
    } else if let Some(h) = s.strip_suffix('h') {
        Duration::hours(h.parse().context("expires hours")?)
    } else if let Some(m) = s.strip_suffix('m') {
        Duration::minutes(m.parse().context("expires minutes")?)
    } else {
        bail!("expires must be never|30d|12h|15m, got '{spec}'");
    };
    Ok(Some((Utc::now() + dur).to_rfc3339()))
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn hash_token(plaintext: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_expires_never() {
        assert!(parse_expires("never").unwrap().is_none());
        assert!(parse_expires("30d").unwrap().is_some());
    }

    #[test]
    fn hash_len() {
        assert_eq!(hash_token("x").len(), 64);
    }
}
