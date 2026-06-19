//! Transport abstraction.
//!
//! Default backend (Step 2): QUIC over `quinn` + `rustls`, with a custom
//! certificate verifier that pins to the peer's Ed25519 identity. The
//! root store is intentionally empty — PKI is replaced by Trust-On-First-
//! Use during pairing, persisted in the local trust store.

use std::net::SocketAddr;

use ansync_crypto::PeerIdentity;
use async_trait::async_trait;
use bytes::Bytes;

#[cfg(feature = "quic")]
pub mod pinning;
#[cfg(feature = "quic")]
pub mod quic;

#[cfg(feature = "quic")]
pub use quic::{QuicConnection, QuicServer, QuicStream, QuicTransport};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection closed")]
    Closed,
    #[error("handshake: {0}")]
    Handshake(String),
    #[error("identity mismatch")]
    IdentityMismatch,
    #[error("stream kind unsupported: {0:?}")]
    UnsupportedStream(StreamKind),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Logical stream kinds multiplexed on a single QUIC connection.
/// One QUIC bidirectional stream per kind, opened on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Control,
    Video,
    Audio,
    Files,
    Input,
    Camera,
    Clipboard,
    Notifications,
    /// One-shot greeting stream. First and only frame is an
    /// `ansync_proto::Envelope` carrying `Message::Hello`. Both sides
    /// open one immediately after the QUIC handshake completes so the
    /// peer's human-readable name + capability bitmap are refreshed
    /// each session without relying on whatever was stamped during
    /// pairing.
    Hello,
    /// One-shot "open this URL" stream. Opener writes a single
    /// postcard `Envelope { Message::Url(UrlMessage) }` frame and
    /// drops the stream. Receiver decides (per platform) whether to
    /// open silently or prompt — see `ansync_proto::UrlMessage`.
    Url,
}

#[async_trait]
pub trait Transport: Send + Sync {
    type Connection: Connection;

    async fn connect(
        &self,
        addr: SocketAddr,
        peer: &PeerIdentity,
    ) -> Result<Self::Connection, TransportError>;

    async fn accept(&self) -> Result<Self::Connection, TransportError>;
}

#[async_trait]
pub trait Connection: Send + Sync {
    type Stream: Stream;

    fn peer_identity(&self) -> &PeerIdentity;

    async fn open(&self, kind: StreamKind) -> Result<Self::Stream, TransportError>;
    async fn accept(&self) -> Result<(StreamKind, Self::Stream), TransportError>;
    async fn close(&self, reason: &str) -> Result<(), TransportError>;
}

#[async_trait]
pub trait Stream: Send + Sync {
    async fn send(&mut self, bytes: Bytes) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Bytes, TransportError>;
}
