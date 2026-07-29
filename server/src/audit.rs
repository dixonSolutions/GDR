//! Structured audit log for authenticated connections.
//!
//! JSON-lines at `~/.local/share/gdr/audit.log`. Self-rotates when the
//! file exceeds `MAX_BYTES` (keeps one `.1` backup). Token management and
//! audit *queries* happen over SSH (admin plane); this module only writes
//! from inside gdrd.

use chrono::Utc;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB

#[derive(Serialize)]
pub struct AuditEvent<'a> {
    pub ts: String,
    pub event: &'a str,
    pub peer: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'a str>,
}

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local/share/gdr/audit.log")
    }

    pub fn open(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, event: &AuditEvent<'_>) {
        let _guard = self.lock.lock().unwrap();
        if let Err(e) = self.write_inner(event) {
            tracing::warn!("audit log write failed: {e}");
        }
    }

    fn write_inner(&self, event: &AuditEvent<'_>) -> anyhow::Result<()> {
        self.maybe_rotate()?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        serde_json::to_writer(&mut f, event)?;
        f.write_all(b"\n")?;
        Ok(())
    }

    fn maybe_rotate(&self) -> anyhow::Result<()> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        if meta.len() < MAX_BYTES {
            return Ok(());
        }
        let bak = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&bak);
        std::fs::rename(&self.path, &bak)?;
        // Touch a fresh file so callers see a clean slate.
        let _ = File::create(&self.path);
        Ok(())
    }
}

pub fn now_ts() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn writes_json_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(path.clone());
        log.write(&AuditEvent {
            ts: now_ts(),
            event: "auth_ok",
            peer: "1.2.3.4:5",
            token_id: Some("tok_abc"),
            token_label: Some("test"),
            request: None,
            detail: None,
        });
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.contains("auth_ok"));
        assert!(data.contains("tok_abc"));
        assert!(data.ends_with('\n'));
    }
}
