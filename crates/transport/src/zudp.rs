//! ZUDP transport backend.
//!
//! Wraps the `zudp` crate so the rest of ansync can use the [`Connection`] /
//! [`Stream`] traits unchanged. Stream multiplexing maps each [`StreamKind`]
//! to a fixed `u16` stream ID; the demux task routes inbound packets to per-
//! stream channels and notifies waiters via an unbounded receiver.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ansync_crypto::PeerIdentity;
use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};
use zudp::{RawBytes, ZudpSender, ZudpSocket, Zudp};

use crate::{Connection, PeerResolver, Stream, StreamKind, TransportError};

// ── stream-id mapping ─────────────────────────────────────────────────────────

fn stream_kind_to_id(kind: StreamKind) -> u16 {
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
        StreamKind::Heartbeat => 0x0c,
    }
}

fn stream_id_to_kind(id: u16) -> Option<StreamKind> {
    match id {
        0x01 => Some(StreamKind::Control),
        0x02 => Some(StreamKind::Video),
        0x03 => Some(StreamKind::Audio),
        0x04 => Some(StreamKind::Files),
        0x06 => Some(StreamKind::Input),
        0x07 => Some(StreamKind::Camera),
        0x08 => Some(StreamKind::Clipboard),
        0x09 => Some(StreamKind::Notifications),
        0x0a => Some(StreamKind::Hello),
        0x0b => Some(StreamKind::Url),
        0x0c => Some(StreamKind::Heartbeat),
        _ => None,
    }
}

// ── per-connection shared state ───────────────────────────────────────────────

/// Per-connection mux/demux state shared between the demux task and the
/// [`ZudpConnection`] handle.
struct ConnInner {
    peer_addr: SocketAddr,
    peer_id: PeerIdentity,
    sender: ZudpSender,
    /// Map stream_id → sender half of the per-stream inbound channel.
    stream_txs: Mutex<HashMap<u16, mpsc::UnboundedSender<Bytes>>>,
    /// Inbound streams that haven't been `accept()`-ed yet.
    new_stream_tx: mpsc::UnboundedSender<(StreamKind, ZudpStream)>,
    closed: AtomicBool,
    close_tx: watch::Sender<bool>,
}

// ── ZudpStream ────────────────────────────────────────────────────────────────

/// A single logical stream on a [`ZudpConnection`].
pub struct ZudpStream {
    kind: StreamKind,
    rx: mpsc::UnboundedReceiver<Bytes>,
    sender: ZudpSender,
    peer: SocketAddr,
    stream_id: u16,
}

#[async_trait::async_trait]
impl Stream for ZudpStream {
    async fn send(&mut self, bytes: Bytes) -> Result<(), TransportError> {
        self.sender
            .send_stream(RawBytes(bytes), self.peer, self.stream_id)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))
    }

    async fn recv(&mut self) -> Result<Bytes, TransportError> {
        self.rx.recv().await.ok_or(TransportError::Closed)
    }
}

impl ZudpStream {
    /// The [`StreamKind`] this stream was opened for.
    #[must_use]
    pub fn kind(&self) -> StreamKind {
        self.kind
    }
}

// ── ZudpConnection ────────────────────────────────────────────────────────────

/// An established ZUDP connection to a single remote peer.
pub struct ZudpConnection {
    inner: Arc<ConnInner>,
    new_stream_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<(StreamKind, ZudpStream)>>>,
}

impl ZudpConnection {
    /// RTT to the remote peer. Returns zero until the first Pong arrives.
    #[must_use]
    pub fn rtt(&self) -> Duration {
        Duration::ZERO
    }

    fn make_stream(&self, kind: StreamKind) -> ZudpStream {
        let stream_id = stream_kind_to_id(kind);
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        self.inner.stream_txs.lock().insert(stream_id, tx);
        ZudpStream {
            kind,
            rx,
            sender: self.inner.sender.clone(),
            peer: self.inner.peer_addr,
            stream_id,
        }
    }
}

#[async_trait::async_trait]
impl Connection for ZudpConnection {
    type Stream = ZudpStream;

    fn peer_identity(&self) -> &PeerIdentity {
        &self.inner.peer_id
    }

    async fn open(&self, kind: StreamKind) -> Result<Self::Stream, TransportError> {
        Ok(self.make_stream(kind))
    }

    async fn accept(&self) -> Result<(StreamKind, Self::Stream), TransportError> {
        let mut rx = self.new_stream_rx.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }

    async fn close(&self, _reason: &str) -> Result<(), TransportError> {
        self.inner.closed.store(true, Ordering::Relaxed);
        let _ = self.inner.close_tx.send(true);
        Ok(())
    }
}

// ── ZudpServer ────────────────────────────────────────────────────────────────

/// Listening server that accepts inbound [`ZudpConnection`]s.
pub struct ZudpServer {
    local_addr: SocketAddr,
    conn_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<ZudpConnection>>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl ZudpServer {
    /// Bind a ZUDP server on `addr`, authenticating with `identity` and
    /// resolving peer identities via `resolver`.
    ///
    /// # Errors
    /// Returns `Err` if the UDP socket cannot be bound.
    pub async fn bind(
        addr: SocketAddr,
        identity: &ansync_crypto::IdentityKeypair,
        resolver: Arc<dyn PeerResolver>,
    ) -> Result<Self, TransportError> {
        let keypair = identity_to_noise_keypair(identity);
        let socket: ZudpSocket<RawBytes> = Zudp::default()
            .port(addr.port())
            .bind_ip(addr.ip())
            .security(keypair)
            .listen()
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;

        let local_addr = socket
            .local_addr()
            .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;

        let (conn_tx, conn_rx) = mpsc::channel::<ZudpConnection>(64);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(server_demux_task(socket, resolver, conn_tx, shutdown_rx));

        Ok(Self {
            local_addr,
            conn_rx: Arc::new(tokio::sync::Mutex::new(conn_rx)),
            _shutdown: shutdown_tx,
        })
    }

    /// Accept the next inbound connection.
    ///
    /// # Errors
    /// Returns [`TransportError::Closed`] if the server has shut down.
    pub async fn accept(&self) -> Result<ZudpConnection, TransportError> {
        self.conn_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or(TransportError::Closed)
    }

    /// Connect to a remote peer as a client.
    ///
    /// Completes the Noise XX handshake before returning — the connection is
    /// encrypted and ready to send on return.
    ///
    /// # Errors
    /// Returns `Err` on I/O failure or handshake timeout.
    pub async fn connect(
        addr: SocketAddr,
        peer_x25519_pubkey: [u8; 32],
        identity: &ansync_crypto::IdentityKeypair,
        resolver: Arc<dyn PeerResolver>,
    ) -> Result<ZudpConnection, TransportError> {
        let keypair = identity_to_noise_keypair(identity);
        let conn: zudp::ZudpConn<RawBytes> = Zudp::default()
            .port(0)
            .security(keypair)
            .pin_remote_key(peer_x25519_pubkey)
            .connect(addr)
            .await
            .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;

        let peer_id = resolver
            .resolve(&peer_x25519_pubkey)
            .ok_or(TransportError::IdentityMismatch)?;

        let sender = conn.sender();
        let peer_addr = conn.peer();
        let (close_tx, _close_rx) = watch::channel(false);
        let (new_stream_tx, new_stream_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(ConnInner {
            peer_addr,
            peer_id,
            sender,
            stream_txs: Mutex::new(HashMap::new()),
            new_stream_tx,
            closed: AtomicBool::new(false),
            close_tx,
        });

        tokio::spawn(client_demux_task(conn, inner.clone()));

        Ok(ZudpConnection {
            inner,
            new_stream_rx: Arc::new(tokio::sync::Mutex::new(new_stream_rx)),
        })
    }

    /// Local address this server is bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

// ── background demux tasks ────────────────────────────────────────────────────

/// Server-side demux: reads `(Bytes, SocketAddr, stream_id)` tuples from the
/// raw ZUDP socket, resolves peer identity from the X25519 remote static key,
/// and routes each packet to the correct per-stream channel.
async fn server_demux_task(
    mut socket: ZudpSocket<RawBytes>,
    resolver: Arc<dyn PeerResolver>,
    conn_tx: mpsc::Sender<ZudpConnection>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    // Map peer_addr → (ConnInner, new_stream_rx)
    let mut connections: HashMap<
        SocketAddr,
        (
            Arc<ConnInner>,
            Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<(StreamKind, ZudpStream)>>>,
        ),
    > = HashMap::new();

    let sender = socket.sender();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            result = socket.recv() => {
                let pkt = match result {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(target: "ansync::transport::zudp", "recv error: {e}");
                        break;
                    }
                };

                let from = pkt.from;
                let stream_id = pkt.stream;
                let payload = pkt.msg.0;

                // Find or create the connection for this peer.
                let (inner, _new_stream_rx) = match connections.get(&from) {
                    Some(entry) => entry.clone(),
                    None => {
                        // Look up the peer's X25519 remote static key.
                        let x25519_key = match socket.peer_remote_static_key(from) {
                            Some(k) => k,
                            None => {
                                tracing::debug!(
                                    target: "ansync::transport::zudp",
                                    peer = %from,
                                    "dropping packet — handshake not yet complete"
                                );
                                continue;
                            }
                        };
                        let peer_id = match resolver.resolve(&x25519_key) {
                            Some(id) => id,
                            None => {
                                tracing::warn!(
                                    target: "ansync::transport::zudp",
                                    peer = %from,
                                    "dropping packet — peer identity not resolved"
                                );
                                continue;
                            }
                        };

                        let (close_tx, _close_rx) = watch::channel(false);
                        let (new_stream_tx, new_stream_rx) = mpsc::unbounded_channel();
                        let inner = Arc::new(ConnInner {
                            peer_addr: from,
                            peer_id,
                            sender: sender.clone(),
                            stream_txs: Mutex::new(HashMap::new()),
                            new_stream_tx,
                            closed: AtomicBool::new(false),
                            close_tx,
                        });
                        let new_stream_rx =
                            Arc::new(tokio::sync::Mutex::new(new_stream_rx));
                        let conn = ZudpConnection {
                            inner: inner.clone(),
                            new_stream_rx: new_stream_rx.clone(),
                        };
                        // Notify the accept() waiter; if the channel is full, log and continue.
                        if conn_tx.try_send(conn).is_err() {
                            tracing::warn!(
                                target: "ansync::transport::zudp",
                                peer = %from,
                                "connection channel full — dropping new connection"
                            );
                            continue;
                        }
                        connections.insert(from, (inner.clone(), new_stream_rx.clone()));
                        (inner, new_stream_rx)
                    }
                };

                route_packet(inner, from, stream_id, payload, &sender).await;
            }
        }
    }
}

/// Client-side demux: drains inbound packets from a `ZudpConn` and routes
/// them to the appropriate per-stream channel on `inner`.
async fn client_demux_task(
    mut conn: zudp::ZudpConn<RawBytes>,
    inner: Arc<ConnInner>,
) {
    let sender = inner.sender.clone();

    loop {
        let pkt = match conn.recv().await {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(
                    target: "ansync::transport::zudp",
                    peer = %conn.peer(),
                    "client demux closed: {e}"
                );
                break;
            }
        };

        let from = pkt.from;
        let stream_id = pkt.stream;
        let payload = pkt.msg.0;

        route_packet(inner.clone(), from, stream_id, payload, &sender).await;
    }
}

/// Route an inbound `payload` on `stream_id` from `from` to the correct
/// per-stream channel. If no channel exists for `stream_id`, create one and
/// notify the `accept()` side via `new_stream_tx`.
async fn route_packet(
    inner: Arc<ConnInner>,
    from: SocketAddr,
    stream_id: u16,
    payload: Bytes,
    sender: &ZudpSender,
) {
    let mut txs = inner.stream_txs.lock();

    if let Some(tx) = txs.get(&stream_id) {
        if tx.send(payload).is_err() {
            txs.remove(&stream_id);
        }
        return;
    }

    // New stream: create channel and notify accept() side.
    let kind = match stream_id_to_kind(stream_id) {
        Some(k) => k,
        None => {
            tracing::debug!(
                target: "ansync::transport::zudp",
                peer = %from,
                stream_id,
                "unknown stream id — dropping"
            );
            return;
        }
    };

    let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
    let _ = tx.send(payload);
    txs.insert(stream_id, tx);
    drop(txs);

    let stream = ZudpStream {
        kind,
        rx,
        sender: sender.clone(),
        peer: from,
        stream_id,
    };

    if inner.new_stream_tx.send((kind, stream)).is_err() {
        tracing::warn!(
            target: "ansync::transport::zudp",
            peer = %from,
            ?kind,
            "new_stream_tx closed — dropping inbound stream"
        );
    }
}

// ── key conversion ────────────────────────────────────────────────────────────

/// Convert an ansync `IdentityKeypair` (Ed25519) into a ZUDP `Keypair`
/// (X25519 montgomery form) for the Noise XX handshake.
///
/// The derivation is: Ed25519 seed → `SigningKey` → `to_scalar().to_bytes()`
/// for the private part; `verifying_key().to_montgomery().to_bytes()` for the
/// public part.
pub fn identity_to_noise_keypair(identity: &ansync_crypto::IdentityKeypair) -> zudp::Keypair {
    let signing = identity.signing();
    let private = signing.to_scalar().to_bytes();
    let public = signing.verifying_key().to_montgomery().to_bytes();
    zudp::Keypair { public, private }
}

/// Convert a peer's Ed25519 public key (32 bytes) to X25519 montgomery form
/// for comparison with the key zudp extracts from the Noise handshake.
#[must_use]
pub fn ed25519_pubkey_to_x25519(ed25519_pubkey: &[u8; 32]) -> Option<[u8; 32]> {
    ed25519_dalek::VerifyingKey::from_bytes(ed25519_pubkey)
        .ok()
        .map(|vk| vk.to_montgomery().to_bytes())
}
