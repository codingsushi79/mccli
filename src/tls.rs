//! TLS for panel ↔ host connections.
//!
//! A self-hosted Minecraft box rarely has a certificate a CA would sign, so
//! the model is SSH's: the host generates a self-signed certificate once, and
//! the panel remembers its SHA-256 fingerprint the first time it connects
//! (after asking). From then on any other certificate is refused, which is
//! what stops a man in the middle from collecting passwords.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::paths;

pub const SERVER_NAME: &str = "mchost";

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(cert.as_ref())))
}

/// Short form for display: `sha256:ab12cd…ef90`.
pub fn short(fp: &str) -> String {
    let hex = fp.trim_start_matches("sha256:");
    if hex.len() <= 16 {
        return fp.to_string();
    }
    format!("sha256:{}…{}", &hex[..8], &hex[hex.len() - 8..])
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Load the host certificate, generating one on first run.
pub fn ensure_identity() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    paths::ensure_host_dirs()?;
    let cert_file = paths::data_dir().join("host.crt");
    let key_file = paths::data_dir().join("host.key");

    if cert_file.exists() && key_file.exists() {
        let cert = std::fs::read(&cert_file)
            .with_context(|| format!("reading {}", cert_file.display()))?;
        let key = std::fs::read(&key_file)
            .with_context(|| format!("reading {}", key_file.display()))?;
        return Ok((
            CertificateDer::from(cert),
            PrivateKeyDer::try_from(key).map_err(|e| anyhow::anyhow!("bad host key: {e}"))?,
        ));
    }

    let generated =
        rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string(), "localhost".into()])
            .context("generating the host certificate")?;
    let cert_der = generated.cert.der().to_vec();
    let key_der = generated.signing_key.serialize_der();
    std::fs::write(&cert_file, &cert_der)?;
    std::fs::write(&key_file, &key_der)?;
    paths::restrict_file(&key_file);

    Ok((
        CertificateDer::from(cert_der),
        PrivateKeyDer::try_from(key_der).map_err(|e| anyhow::anyhow!("bad key: {e}"))?,
    ))
}

pub fn local_fingerprint() -> Result<String> {
    let (cert, _) = ensure_identity()?;
    Ok(fingerprint(&cert))
}

pub fn server_config() -> Result<Arc<ServerConfig>> {
    let (cert, key) = ensure_identity()?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("building the TLS server config")?;
    Ok(Arc::new(config))
}

/// What the verifier saw, so the caller can show it or pin it.
#[derive(Debug, Default)]
pub struct Seen(pub Mutex<Option<String>>);

/// Accepts the one pinned certificate, or — when nothing is pinned yet —
/// any certificate, while recording its fingerprint so the user can be asked.
#[derive(Debug)]
struct Verifier {
    expected: Option<String>,
    seen: Arc<Seen>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = fingerprint(end_entity);
        *self.seen.0.lock().unwrap() = Some(presented.clone());
        match &self.expected {
            None => Ok(ServerCertVerified::assertion()),
            Some(expected) if *expected == presented => Ok(ServerCertVerified::assertion()),
            Some(expected) => Err(rustls::Error::General(format!(
                "HOST IDENTITY CHANGED: expected {}, got {} — the host was reinstalled, \
                 or something is intercepting the connection. If you trust the change, \
                 run `mchost forget <address>`",
                short(expected),
                short(&presented)
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn client_config(expected: Option<String>) -> Result<(Arc<ClientConfig>, Arc<Seen>)> {
    let seen = Arc::new(Seen::default());
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Verifier {
            expected,
            seen: seen.clone(),
        }))
        .with_no_client_auth();
    Ok((Arc::new(config), seen))
}

// --------------------------------------------------------- known hosts ---

fn known_hosts_path() -> std::path::PathBuf {
    paths::client_dir().join("known_hosts.json")
}

pub fn known_hosts() -> BTreeMap<String, String> {
    std::fs::read_to_string(known_hosts_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn known_fingerprint(address: &str) -> Option<String> {
    known_hosts().get(address).cloned()
}

pub fn trust(address: &str, fingerprint: &str) -> Result<()> {
    paths::ensure_client_dir()?;
    let mut hosts = known_hosts();
    hosts.insert(address.to_string(), fingerprint.to_string());
    paths::write_atomic(&known_hosts_path(), serde_json::to_string_pretty(&hosts)?.as_bytes())
}

pub fn forget(address: &str) -> Result<bool> {
    let mut hosts = known_hosts();
    let removed = hosts.remove(address).is_some();
    if removed {
        paths::write_atomic(&known_hosts_path(), serde_json::to_string_pretty(&hosts)?.as_bytes())?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_prefixed_and_shortened() {
        let fp = fingerprint(&CertificateDer::from(vec![1, 2, 3]));
        assert!(fp.starts_with("sha256:"));
        assert_eq!(fp.len(), 7 + 64);
        let s = short(&fp);
        assert!(s.contains('…'));
        assert!(s.len() < fp.len());
    }
}
