//! Trusting exactly one hub: its key, installed out of band, not a certificate authority or a name.
//!
//! The hub's address can change (DHCP) and its name is advertised by anyone on the network (mDNS),
//! so neither proves identity. The SHA-256 of the hub's public key, taken from the pairing string,
//! does. Handshake signatures are still verified properly, so only the holder of that key passes.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// The reason recorded when the hub presents a different key. [`crate::ClientError`] looks for it.
pub(crate) const MISMATCH: &str = "display-hub: the server's key doesn't match the paired hub";

/// SHA-256 of a certificate's SubjectPublicKeyInfo.
pub fn spki_sha256(cert: &CertificateDer<'_>) -> Option<[u8; 32]> {
    let ee = webpki::EndEntityCert::try_from(cert).ok()?;
    let spki = ee.subject_public_key_info();
    ring::digest::digest(&ring::digest::SHA256, spki.as_ref())
        .as_ref()
        .try_into()
        .ok()
}

#[derive(Debug)]
pub struct PinnedKey {
    pins: Vec<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}

impl PinnedKey {
    /// Accepts a server presenting any of `pins` (normally one; two during a key change).
    pub fn new(pins: Vec<[u8; 32]>, provider: Arc<CryptoProvider>) -> Self {
        Self { pins, provider }
    }
}

impl ServerCertVerifier for PinnedKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match spki_sha256(end_entity) {
            Some(h) if self.pins.contains(&h) => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General(MISMATCH.into())),
            None => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadEncoding,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

/// A rustls client configuration that trusts only `pins`.
pub fn client_config(pins: Vec<[u8; 32]>) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("ring supports TLS 1.2 and 1.3")
        .dangerous() // a custom verifier: it pins the key instead of trusting certificate authorities
        .with_custom_certificate_verifier(Arc::new(PinnedKey::new(pins, provider)))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}
