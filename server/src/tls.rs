//! Self-signed TLS cert for the control channel. This is not meant to
//! authenticate the *server's identity* to some CA-trusting client (there's
//! no CA) - it's meant to encrypt the link so mouse/keyboard/screenshot
//! traffic (and the auth token) aren't sent in the clear. The client pins
//! the server's cert fingerprint on first connect (TOFU), similar to SSH's
//! known_hosts model.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

pub struct GeneratedCert {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivatePkcs8KeyDer<'static>,
    pub fingerprint_sha256_hex: String,
}

pub fn fingerprint_hex(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Loads a persisted self-signed cert/key pair, generating and saving a new
/// one if none exists yet at `cert_path`/`key_path`.
pub fn load_or_generate(cert_path: &Path, key_path: &Path) -> Result<GeneratedCert> {
    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read(cert_path)?;
        let key_pem = std::fs::read(key_path)?;
        let cert_der = rustls_pemfile::certs(&mut cert_pem.as_slice())
            .next()
            .context("no cert in cert file")??;
        let key_der = rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_slice())
            .next()
            .context("no key in key file")??;
        let fp = fingerprint_hex(cert_der.as_ref());
        return Ok(GeneratedCert {
            cert_der: cert_der.into_owned(),
            key_der: PrivatePkcs8KeyDer::from(key_der.secret_pkcs8_der().to_vec()),
            fingerprint_sha256_hex: fp,
        });
    }

    let params = rcgen::CertificateParams::new(vec!["gdrd".to_string()])?;
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::set_permissions(cert_path, std::fs::Permissions::from_mode(0o644));
    }

    let fp = fingerprint_hex(cert.der().as_ref());
    tracing::info!(
        "Generated new self-signed cert at {}. SHA-256 fingerprint (pin this): {fp}",
        cert_path.display()
    );
    println!("gdrd: new cert fingerprint (pin with --pin / GDR_PIN): {fp}");

    Ok(GeneratedCert {
        cert_der: cert.der().clone(),
        key_der: PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
        fingerprint_sha256_hex: fp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generate_persist_reload_same_fingerprint() {
        let dir = tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let a = load_or_generate(&cert, &key).unwrap();
        let b = load_or_generate(&cert, &key).unwrap();
        assert_eq!(a.fingerprint_sha256_hex, b.fingerprint_sha256_hex);
        assert_eq!(a.fingerprint_sha256_hex.len(), 64);
    }
}
