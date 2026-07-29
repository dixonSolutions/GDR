//! Admin-plane operations over SSH. Token create/list/revoke and audit
//! tail all go through this module — never through the live TLS protocol —
//! so a leaked data-plane token cannot mint more tokens.

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
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

const REMOTE_TOKENS: &str = "$HOME/.local/share/gdr/tokens.json";
const REMOTE_AUDIT: &str = "$HOME/.local/share/gdr/audit.log";

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

pub fn list_tokens(ssh_target: &str) -> Result<Vec<RemoteTokenEntry>> {
    let script = format!(
        r#"
set -euo pipefail
f={REMOTE_TOKENS}
if [ ! -f "$f" ]; then echo '{{"tokens":[]}}'; exit 0; fi
cat "$f"
"#
    );
    let out = ssh_script(ssh_target, &script)?;
    let file: TokenStoreFile = serde_json::from_str(&out).context("parsing remote tokens.json")?;
    Ok(file.tokens)
}

pub fn revoke_token(ssh_target: &str, id: &str) -> Result<()> {
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

pub fn create_token(
    ssh_target: &str,
    label: &str,
    scopes: &str,
    expires: &str,
) -> Result<(String, String)> {
    // Generate plaintext locally; only hash is written remotely.
    let plaintext = generate_token();
    let hash = hash_token(&plaintext);
    let id = format!("tok_{}", &hash[..16]);
    let now = Utc::now().to_rfc3339();
    let expires_at = parse_expires(expires)?;
    let expires_json = match expires_at {
        Some(e) => format!("\"{}\"", e),
        None => "null".into(),
    };
    let scopes_vec: Vec<String> = if scopes.eq_ignore_ascii_case("all") {
        vec!["all".into()]
    } else {
        scopes
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
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

pub fn audit_tail(ssh_target: &str, lines: usize, since: Option<&str>, token_id: Option<&str>) -> Result<String> {
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
