//! Trust-on-first-use certificate verification: we don't have a CA, so
//! instead of validating a chain we just check the server's leaf cert
//! SHA-256 fingerprint against one the user pinned (from the fingerprint
//! the server printed on first run), same trust model as SSH known_hosts.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;

#[derive(Debug)]
pub struct PinnedFingerprintVerifier {
    pub expected_sha256_hex: Option<String>,
    provider: Arc<CryptoProvider>,
}

impl PinnedFingerprintVerifier {
    pub fn new(expected_sha256_hex: Option<String>) -> Self {
        let provider = CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| {
                // ring is the default feature of rustls 0.23 in this project.
                rustls::crypto::ring::default_provider().into()
            });
        Self {
            expected_sha256_hex,
            provider,
        }
    }
}

impl fmt::Display for PinnedFingerprintVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PinnedFingerprintVerifier")
    }
}

pub fn sha256_hex(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let actual = sha256_hex(end_entity.as_ref());
        match &self.expected_sha256_hex {
            Some(expected) if expected.eq_ignore_ascii_case(&actual) => {
                Ok(ServerCertVerified::assertion())
            }
            Some(_) => Err(TlsError::General(format!(
                "server cert fingerprint mismatch! got {actual}. \
                 If you re-deployed the server, re-pin with --pin {actual} \
                 (only do this if you trust that this is expected)."
            ))),
            None => {
                eprintln!(
                    "WARNING: no --pin given, trusting on first use.\n\
                     Server cert SHA-256: {actual}\n\
                     Save this and pass --pin {actual} on future connections."
                );
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
