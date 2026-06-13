//! rustls certificate verifier that pins to an expected Ed25519 pubkey.
//!
//! The cert chain is ignored beyond the leaf — we deliberately have no
//! PKI. Trust is established at pairing time and recorded as the peer's
//! 32-byte Ed25519 pubkey. Any cert presenting that pubkey in its SPKI
//! is accepted; anything else is rejected.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use x509_parser::oid_registry::OID_SIG_ED25519;
use x509_parser::prelude::*;

#[derive(Debug, thiserror::Error)]
pub enum PinningError {
    #[error("cert parse: {0}")]
    ParseCert(String),
    #[error("cert SPKI is not Ed25519")]
    WrongAlgorithm,
    #[error("Ed25519 pubkey size: expected 32, got {0}")]
    WrongKeyLength(usize),
    #[error("Ed25519 pubkey mismatch")]
    Mismatch,
}

impl From<PinningError> for TlsError {
    fn from(value: PinningError) -> Self {
        TlsError::General(value.to_string())
    }
}

pub fn extract_ed25519_pubkey(cert_der: &[u8]) -> Result<[u8; 32], PinningError> {
    let (_, cert) =
        X509Certificate::from_der(cert_der).map_err(|e| PinningError::ParseCert(format!("{e}")))?;
    let spki = &cert.tbs_certificate.subject_pki;
    if spki.algorithm.algorithm != OID_SIG_ED25519 {
        return Err(PinningError::WrongAlgorithm);
    }
    let raw = spki.subject_public_key.data.as_ref();
    if raw.len() != 32 {
        return Err(PinningError::WrongKeyLength(raw.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(raw);
    Ok(out)
}

fn check_pinned(cert: &CertificateDer<'_>, expected: &[u8; 32]) -> Result<(), TlsError> {
    let key = extract_ed25519_pubkey(cert.as_ref())?;
    if key != *expected {
        return Err(PinningError::Mismatch.into());
    }
    Ok(())
}

#[derive(Debug)]
pub struct Ed25519ServerVerifier {
    expected: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl Ed25519ServerVerifier {
    pub fn new(expected: [u8; 32], provider: Arc<CryptoProvider>) -> Self {
        Self { expected, provider }
    }
}

impl ServerCertVerifier for Ed25519ServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        check_pinned(end_entity, &self.expected)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
pub struct Ed25519ClientVerifier {
    expected: [u8; 32],
    provider: Arc<CryptoProvider>,
    root_hint_subjects: Vec<DistinguishedName>,
}

impl Ed25519ClientVerifier {
    pub fn new(expected: [u8; 32], provider: Arc<CryptoProvider>) -> Self {
        Self {
            expected,
            provider,
            root_hint_subjects: Vec::new(),
        }
    }
}

/// Predicate that decides whether a connecting peer's Ed25519 pubkey
/// is trusted. Daemon-side wiring backs this with a `PeerStore`
/// lookup so any paired peer is accepted, anything else is rejected.
pub trait TrustedPeers: Send + Sync + std::fmt::Debug + 'static {
    fn is_trusted(&self, pubkey: &[u8; 32]) -> bool;
}

/// Multi-peer client cert verifier. Accepts any incoming cert whose
/// leaf Ed25519 pubkey passes the [`TrustedPeers`] predicate. The
/// daemon learns *which* peer connected by re-extracting the pubkey
/// from `quinn::Connection::peer_identity()` after handshake.
#[derive(Debug)]
pub struct Ed25519AnyPeerVerifier {
    trust: Arc<dyn TrustedPeers>,
    provider: Arc<CryptoProvider>,
    root_hint_subjects: Vec<DistinguishedName>,
}

impl Ed25519AnyPeerVerifier {
    pub fn new(trust: Arc<dyn TrustedPeers>, provider: Arc<CryptoProvider>) -> Self {
        Self {
            trust,
            provider,
            root_hint_subjects: Vec::new(),
        }
    }
}

impl ClientCertVerifier for Ed25519AnyPeerVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.root_hint_subjects
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        let key = extract_ed25519_pubkey(end_entity.as_ref())?;
        if !self.trust.is_trusted(&key) {
            return Err(PinningError::Mismatch.into());
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Ed25519ClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.root_hint_subjects
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        check_pinned(end_entity, &self.expected)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
