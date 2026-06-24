//! QUIC transport built on `quinn` + `rustls`.
//!
//! Self-signed leaf cert is generated from the long-term Ed25519 identity
//! at endpoint construction. The peer's cert is validated only against an
//! expected Ed25519 pubkey (see [`crate::pinning`]); the cert subject,
//! chain, and validity period are deliberately ignored.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use ansync_crypto::{IdentityKeypair, PeerIdentity};
use ansync_proto::{FrameError, MAX_FRAME_SIZE, read_frame, write_frame};
use async_trait::async_trait;
use bytes::Bytes;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::pinning::{
    Ed25519AnyPeerVerifier, Ed25519ClientVerifier, Ed25519ServerVerifier, TrustedPeers,
    extract_ed25519_pubkey,
};
use crate::{Connection, Stream, StreamKind, Transport, TransportError};

const ALPN: &[u8] = b"ansync/1";
const SNI_NAME: &str = "ansync";

fn stream_kind_tag(kind: StreamKind) -> u8 {
    match kind {
        StreamKind::Control => 0x01,
        StreamKind::Video => 0x02,
        StreamKind::Audio => 0x03,
        StreamKind::Files => 0x04,
        StreamKind::Input => 0x06,
        StreamKind::Camera => 0x07,
        StreamKind::Clipboard => 0x08,
        StreamKind::Notifications => 0x09,
        StreamKind::Hello => 0x0a,
        StreamKind::Url => 0x0b,
    }
}

fn stream_kind_from_tag(tag: u8) -> Result<StreamKind, TransportError> {
    match tag {
        0x01 => Ok(StreamKind::Control),
        0x02 => Ok(StreamKind::Video),
        0x03 => Ok(StreamKind::Audio),
        0x04 => Ok(StreamKind::Files),
        0x06 => Ok(StreamKind::Input),
        0x07 => Ok(StreamKind::Camera),
        0x08 => Ok(StreamKind::Clipboard),
        0x09 => Ok(StreamKind::Notifications),
        0x0a => Ok(StreamKind::Hello),
        0x0b => Ok(StreamKind::Url),
        _ => Err(TransportError::Handshake(format!("unknown stream tag {tag:#x}"))),
    }
}

impl From<FrameError> for TransportError {
    fn from(value: FrameError) -> Self {
        match value {
            // `UnexpectedEof` here means the peer dropped its send half
            // mid-frame — a clean transport-level FIN, not an error.
            // Bubbling it as `Closed` lets every recv loop collapse the
            // graceful-shutdown path into a single match arm and log at
            // info-level. `BrokenPipe` / `ConnectionReset` carry the
            // same "peer is gone" semantics.
            FrameError::Io(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                TransportError::Closed
            }
            FrameError::Io(e) => TransportError::Io(e),
            other => TransportError::Handshake(other.to_string()),
        }
    }
}

fn map_connect_err(e: quinn::ConnectError) -> TransportError {
    TransportError::Handshake(format!("connect: {e}"))
}

fn map_conn_err(e: quinn::ConnectionError) -> TransportError {
    // Anything that means "the connection is gone" maps to `Closed` so
    // call sites can stop with one match arm and log at info-level.
    // `TimedOut` gets its own variant — it's the signal we want for
    // Doze / wifi-handoff diagnostics without flooding the journal.
    // Genuine handshake / protocol bugs keep `Handshake`.
    match e {
        quinn::ConnectionError::LocallyClosed
        | quinn::ConnectionError::ApplicationClosed(_)
        | quinn::ConnectionError::ConnectionClosed(_)
        | quinn::ConnectionError::Reset => TransportError::Closed,
        quinn::ConnectionError::TimedOut => TransportError::TimedOut,
        other => TransportError::Handshake(format!("conn: {other}")),
    }
}

fn build_cert_chain(
    identity: &IdentityKeypair,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TransportError> {
    let signing = identity.signing();
    let pkcs8_der = signing
        .to_pkcs8_der()
        .map_err(|e| TransportError::Handshake(format!("pkcs8 der: {e}")))?;
    let pkcs8_bytes = pkcs8_der.as_bytes().to_vec();
    let key_pair = rcgen::KeyPair::try_from(pkcs8_bytes.as_slice())
        .map_err(|e| TransportError::Handshake(format!("rcgen key: {e}")))?;
    let mut params = rcgen::CertificateParams::new(vec![SNI_NAME.to_string()])
        .map_err(|e| TransportError::Handshake(format!("rcgen params: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, SNI_NAME);
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| TransportError::Handshake(format!("rcgen sign: {e}")))?;
    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8_bytes));
    Ok((vec![cert_der], key_der))
}

fn default_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Transport-layer tuning shared by every QUIC endpoint we build.
/// Idle streams pile up while the user walks through MediaProjection
/// prompts or scrolls through Settings, so the 30-second default is
/// way too aggressive. Bumping `max_idle_timeout` + adding a
/// keep-alive ping keeps connections live across normal user pauses.
fn tuned_transport_config() -> Arc<quinn::TransportConfig> {
    let mut cfg = quinn::TransportConfig::default();
    cfg.max_idle_timeout(Some(
        std::time::Duration::from_secs(120)
            .try_into()
            .expect("120s fits in VarInt"),
    ));
    cfg.keep_alive_interval(Some(std::time::Duration::from_secs(15)));
    Arc::new(cfg)
}

#[derive(Clone)]
pub struct QuicTransport {
    identity: IdentityKeypair,
    provider: Arc<CryptoProvider>,
}

impl QuicTransport {
    pub fn new(identity: IdentityKeypair) -> Self {
        Self { identity, provider: default_provider() }
    }

    pub fn identity(&self) -> &IdentityKeypair {
        &self.identity
    }

    fn make_server_config(
        &self,
        expected_client: [u8; 32],
    ) -> Result<quinn::ServerConfig, TransportError> {
        let (cert_chain, key_der) = build_cert_chain(&self.identity)?;
        let verifier = Arc::new(Ed25519ClientVerifier::new(
            expected_client,
            self.provider.clone(),
        ));
        let mut rustls_cfg = rustls::ServerConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| TransportError::Handshake(format!("tls13: {e}")))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(cert_chain, key_der)
            .map_err(|e| TransportError::Handshake(format!("server cert: {e}")))?;
        rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];
        let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
            .map_err(|e| TransportError::Handshake(format!("quic server cfg: {e}")))?;
        let mut server_cfg = quinn::ServerConfig::with_crypto(Arc::new(qsc));
        server_cfg.transport_config(tuned_transport_config());
        Ok(server_cfg)
    }

    fn make_server_config_any(
        &self,
        trust: Arc<dyn TrustedPeers>,
    ) -> Result<quinn::ServerConfig, TransportError> {
        let (cert_chain, key_der) = build_cert_chain(&self.identity)?;
        let verifier = Arc::new(Ed25519AnyPeerVerifier::new(trust, self.provider.clone()));
        let mut rustls_cfg = rustls::ServerConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| TransportError::Handshake(format!("tls13: {e}")))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(cert_chain, key_der)
            .map_err(|e| TransportError::Handshake(format!("server cert: {e}")))?;
        rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];
        let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
            .map_err(|e| TransportError::Handshake(format!("quic server cfg: {e}")))?;
        let mut server_cfg = quinn::ServerConfig::with_crypto(Arc::new(qsc));
        server_cfg.transport_config(tuned_transport_config());
        Ok(server_cfg)
    }

    fn make_client_config(
        &self,
        expected_server: [u8; 32],
    ) -> Result<quinn::ClientConfig, TransportError> {
        let (cert_chain, key_der) = build_cert_chain(&self.identity)?;
        let verifier = Arc::new(Ed25519ServerVerifier::new(
            expected_server,
            self.provider.clone(),
        ));
        let mut rustls_cfg = rustls::ClientConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| TransportError::Handshake(format!("tls13: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(cert_chain, key_der)
            .map_err(|e| TransportError::Handshake(format!("client cert: {e}")))?;
        rustls_cfg.alpn_protocols = vec![ALPN.to_vec()];
        let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
            .map_err(|e| TransportError::Handshake(format!("quic client cfg: {e}")))?;
        let mut client_cfg = quinn::ClientConfig::new(Arc::new(qcc));
        client_cfg.transport_config(tuned_transport_config());
        Ok(client_cfg)
    }

    pub fn bind(
        &self,
        addr: SocketAddr,
        expected_client: [u8; 32],
    ) -> Result<QuicServer, TransportError> {
        let server_config = self.make_server_config(expected_client)?;
        let endpoint = quinn::Endpoint::server(server_config, addr)?;
        let peer = PeerIdentity::from_bytes(expected_client)
            .map_err(|_| TransportError::IdentityMismatch)?;
        Ok(QuicServer {
            endpoint,
            pinned_peer: Some(peer),
        })
    }

    /// Bind a server that accepts any peer whose Ed25519 pubkey
    /// passes `trust`. Used by the daemon's accept loop, which trusts
    /// every entry in the `PeerStore`. The connecting peer's identity
    /// is discovered post-handshake via
    /// [`QuicConnection::peer_pubkey`].
    pub fn bind_any(
        &self,
        addr: SocketAddr,
        trust: Arc<dyn TrustedPeers>,
    ) -> Result<QuicServer, TransportError> {
        let server_config = self.make_server_config_any(trust)?;
        let endpoint = quinn::Endpoint::server(server_config, addr)?;
        Ok(QuicServer {
            endpoint,
            pinned_peer: None,
        })
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        expected_server: [u8; 32],
    ) -> Result<QuicConnection, TransportError> {
        let client_config = self.make_client_config(expected_server)?;
        let local: SocketAddr = "0.0.0.0:0"
            .parse()
            .expect("hard-coded local addr parses");
        let mut endpoint = quinn::Endpoint::client(local)?;
        endpoint.set_default_client_config(client_config);
        let connecting = endpoint.connect(addr, SNI_NAME).map_err(map_connect_err)?;
        let inner = connecting.await.map_err(map_conn_err)?;
        let peer = PeerIdentity::from_bytes(expected_server)
            .map_err(|_| TransportError::IdentityMismatch)?;
        Ok(QuicConnection { inner, peer, _endpoint: Some(endpoint) })
    }
}

pub struct QuicServer {
    endpoint: quinn::Endpoint,
    /// `Some` for single-peer `bind`, `None` for multi-peer
    /// `bind_any`; in the latter case the connecting peer's identity
    /// is recovered from the TLS leaf cert post-handshake.
    pinned_peer: Option<PeerIdentity>,
}

impl QuicServer {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub fn endpoint(&self) -> &quinn::Endpoint {
        &self.endpoint
    }

    pub async fn accept(&self) -> Result<QuicConnection, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::Closed)?;
        let inner = incoming.await.map_err(map_conn_err)?;
        let peer = match self.pinned_peer.clone() {
            Some(p) => p,
            None => peer_from_handshake(&inner)?,
        };
        Ok(QuicConnection {
            inner,
            peer,
            _endpoint: None,
        })
    }

    pub async fn close(&self, reason: &str) {
        self.endpoint.close(0u32.into(), reason.as_bytes());
        self.endpoint.wait_idle().await;
    }
}

fn peer_from_handshake(conn: &quinn::Connection) -> Result<PeerIdentity, TransportError> {
    let chain = conn
        .peer_identity()
        .ok_or_else(|| TransportError::Handshake("peer presented no identity".into()))?;
    let certs = chain
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .map_err(|_| {
            TransportError::Handshake("peer identity not a rustls cert chain".into())
        })?;
    let leaf = certs
        .first()
        .ok_or_else(|| TransportError::Handshake("peer cert chain empty".into()))?;
    let key = extract_ed25519_pubkey(leaf.as_ref())
        .map_err(|e| TransportError::Handshake(format!("extract pubkey: {e}")))?;
    PeerIdentity::from_bytes(key).map_err(|_| TransportError::IdentityMismatch)
}

pub struct QuicConnection {
    inner: quinn::Connection,
    peer: PeerIdentity,
    _endpoint: Option<quinn::Endpoint>,
}

impl QuicConnection {
    /// Resolves once the connection has been fully torn down by either
    /// side. Useful as a "flush" point in tests and shutdown paths.
    pub async fn closed(&self) {
        self.inner.closed().await;
    }

    /// Snapshot of the underlying quinn connection stats. Exposed for
    /// telemetry — `debug!`-level periodic logging in daemon-core +
    /// companion to diagnose packet loss / RTT regressions.
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.inner.stats()
    }

    pub fn rtt(&self) -> std::time::Duration {
        self.inner.rtt()
    }
}

#[async_trait]
impl Connection for QuicConnection {
    type Stream = QuicStream;

    fn peer_identity(&self) -> &PeerIdentity {
        &self.peer
    }

    async fn open(&self, kind: StreamKind) -> Result<Self::Stream, TransportError> {
        let (mut send, recv) = self.inner.open_bi().await.map_err(map_conn_err)?;
        send.write_all(&[stream_kind_tag(kind)])
            .await
            .map_err(|e| TransportError::Handshake(format!("write tag: {e}")))?;
        Ok(QuicStream { send, recv, kind })
    }

    async fn accept(&self) -> Result<(StreamKind, Self::Stream), TransportError> {
        let (send, mut recv) = self.inner.accept_bi().await.map_err(map_conn_err)?;
        let mut tag = [0u8; 1];
        recv.read_exact(&mut tag)
            .await
            .map_err(|e| TransportError::Handshake(format!("read tag: {e}")))?;
        let kind = stream_kind_from_tag(tag[0])?;
        Ok((kind, QuicStream { send, recv, kind }))
    }

    async fn close(&self, reason: &str) -> Result<(), TransportError> {
        self.inner.close(0u32.into(), reason.as_bytes());
        Ok(())
    }
}

pub struct QuicStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    kind: StreamKind,
}

impl QuicStream {
    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    pub async fn finish(&mut self) -> Result<(), TransportError> {
        self.send
            .finish()
            .map_err(|e| TransportError::Handshake(format!("finish: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl Stream for QuicStream {
    async fn send(&mut self, bytes: Bytes) -> Result<(), TransportError> {
        write_frame(&mut self.send, &bytes).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Bytes, TransportError> {
        let buf = read_frame(&mut self.recv, MAX_FRAME_SIZE).await?;
        Ok(Bytes::from(buf))
    }
}

/// Concrete [`Transport`] impl for the QUIC backend. Useful when generic
/// code wants to abstract over backends. Note that `QuicTransport` only
/// becomes an *accepting* transport once `bind` has been called — until
/// then `accept` errors with `Closed`.
#[async_trait]
impl Transport for QuicTransport {
    type Connection = QuicConnection;

    async fn connect(
        &self,
        addr: SocketAddr,
        peer: &PeerIdentity,
    ) -> Result<Self::Connection, TransportError> {
        QuicTransport::connect(self, addr, peer.as_bytes()).await
    }

    async fn accept(&self) -> Result<Self::Connection, TransportError> {
        Err(TransportError::Closed)
    }
}
