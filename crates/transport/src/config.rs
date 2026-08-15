use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use quinn::{ClientConfig, ServerConfig, crypto::rustls::QuicClientConfig};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

use crate::TransportError;

/// Certificate DER and its SHA-256 fingerprint used to pair an Android client.
#[derive(Clone, Debug)]
pub struct CertificateIdentity {
    pub certificate_der: Vec<u8>,
    pub sha256_fingerprint: [u8; 32],
}

/// Creates a fresh self-signed certificate. The server normally calls
/// [`load_or_create_server_config`] so the fingerprint persists across restarts;
/// this constructor is useful for tests and ephemeral deployments.
pub fn make_server_config() -> Result<(ServerConfig, CertificateIdentity), TransportError> {
    let certified = rcgen::generate_simple_self_signed(vec!["soundwave.local".to_owned()])
        .map_err(|error| TransportError::Certificate(error.to_string()))?;
    let certificate_der = certified.cert.der().to_vec();
    let private_key_der = certified.key_pair.serialize_der();
    server_config_from_der(certificate_der, private_key_der)
}

/// Loads the stable self-signed identity from `identity_dir`, creating it only
/// on first startup. The files are DER, not user-editable configuration.
pub fn load_or_create_server_config(
    identity_dir: impl AsRef<Path>,
) -> Result<(ServerConfig, CertificateIdentity), TransportError> {
    let identity_dir = identity_dir.as_ref();
    fs::create_dir_all(identity_dir).map_err(identity_io_error)?;
    let certificate_path = identity_file(identity_dir, "server-cert.der");
    let private_key_path = identity_file(identity_dir, "server-key.der");
    match (fs::read(&certificate_path), fs::read(&private_key_path)) {
        (Ok(certificate), Ok(private_key)) => server_config_from_der(certificate, private_key),
        (Err(certificate_error), Err(private_key_error))
            if certificate_error.kind() == std::io::ErrorKind::NotFound
                && private_key_error.kind() == std::io::ErrorKind::NotFound =>
        {
            let certified = rcgen::generate_simple_self_signed(vec!["soundwave.local".to_owned()])
                .map_err(|error| TransportError::Certificate(error.to_string()))?;
            let certificate = certified.cert.der().to_vec();
            let private_key = certified.key_pair.serialize_der();
            fs::write(&certificate_path, &certificate).map_err(identity_io_error)?;
            fs::write(&private_key_path, &private_key).map_err(identity_io_error)?;
            server_config_from_der(certificate, private_key)
        }
        (certificate_result, private_key_result) => Err(TransportError::Certificate(format!(
            "incomplete or unreadable server identity in {} (certificate: {}, key: {})",
            identity_dir.display(),
            describe_identity_read(&certificate_result),
            describe_identity_read(&private_key_result),
        ))),
    }
}

fn server_config_from_der(
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
) -> Result<(ServerConfig, CertificateIdentity), TransportError> {
    let private_key = PrivateKeyDer::Pkcs8(private_key_der.into());
    let certificate = CertificateDer::from(certificate_der.clone());
    let server_config = ServerConfig::with_single_cert(vec![certificate], private_key)
        .map_err(|error| TransportError::Certificate(error.to_string()))?;
    Ok((
        server_config,
        CertificateIdentity {
            sha256_fingerprint: certificate_fingerprint(&certificate_der),
            certificate_der,
        },
    ))
}

fn identity_file(identity_dir: &Path, name: &str) -> PathBuf {
    identity_dir.join(name)
}

fn identity_io_error(error: std::io::Error) -> TransportError {
    TransportError::Certificate(format!("server identity I/O error: {error}"))
}

fn describe_identity_read(result: &Result<Vec<u8>, std::io::Error>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => error.to_string(),
    }
}

pub fn certificate_fingerprint(certificate_der: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate_der).into()
}

/// Formats a SHA-256 fingerprint for terminal display and Android copy/paste.
pub fn format_fingerprint(fingerprint: &[u8; 32]) -> String {
    fingerprint
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parses colon, hyphen, or whitespace separated SHA-256 fingerprint hex.
pub fn parse_fingerprint(input: &str) -> Result<[u8; 32], TransportError> {
    let compact = input
        .chars()
        .filter(|character| !matches!(character, ':' | '-' | ' ' | '\t' | '\r' | '\n'))
        .collect::<String>();
    if compact.len() != 64 {
        return Err(TransportError::Certificate(format!(
            "certificate fingerprint needs 64 hexadecimal digits, got {}",
            compact.len()
        )));
    }

    let mut fingerprint = [0_u8; 32];
    for (index, pair) in compact.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or_else(|| {
            TransportError::Certificate(
                "certificate fingerprint contains non-hexadecimal data".to_owned(),
            )
        })?;
        let low = hex_nibble(pair[1]).ok_or_else(|| {
            TransportError::Certificate(
                "certificate fingerprint contains non-hexadecimal data".to_owned(),
            )
        })?;
        fingerprint[index] = (high << 4) | low;
    }
    Ok(fingerprint)
}

/// Builds a QUIC configuration that accepts exactly one server certificate by
/// SHA-256 fingerprint. Unlike an insecure "accept any certificate" verifier,
/// this makes a LAN MITM fail before control or audio data are exchanged.
pub fn make_pinned_client_config(fingerprint: [u8; 32]) -> Result<ClientConfig, TransportError> {
    let verifier = Arc::new(FingerprintVerifier {
        expected: fingerprint,
        provider: Arc::new(rustls::crypto::ring::default_provider()),
    });
    let client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let quic_crypto = QuicClientConfig::try_from(client_crypto)
        .map_err(|error| TransportError::Certificate(error.to_string()))?;
    Ok(ClientConfig::new(Arc::new(quic_crypto)))
}

#[derive(Debug)]
struct FingerprintVerifier {
    expected: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate fingerprint does not match the pinned fingerprint".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod fingerprint_tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn fingerprint_text_round_trips() {
        let fingerprint = [0xAB; 32];
        assert_eq!(
            parse_fingerprint(&format_fingerprint(&fingerprint)).unwrap(),
            fingerprint
        );
    }

    #[test]
    fn persisted_identity_keeps_its_fingerprint() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "soundwave-identity-{}-{unique}",
            std::process::id()
        ));
        let (_, first) = load_or_create_server_config(&directory).unwrap();
        let (_, second) = load_or_create_server_config(&directory).unwrap();
        assert_eq!(first.sha256_fingerprint, second.sha256_fingerprint);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
