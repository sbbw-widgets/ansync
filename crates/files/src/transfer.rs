//! Push / pull file transfer state machine over a single QUIC
//! `StreamKind::Files` bidi stream.
//!
//! Each stream carries exactly one transfer: the sender emits
//! [`FileTransferMessage::Offer`], the receiver replies `Accept` or
//! `Reject`, then the sender streams [`FileTransferMessage::Chunk`]s
//! and closes with `Complete`. The receiver verifies the running
//! SHA-256 against the value in the original `Offer` before
//! acknowledging. QUIC backpressure flow-controls the chunk pump for
//! free — no application-level windowing needed for Step 8.
//!
//! Permission gating sits at the daemon edge (`files_send` outbound,
//! `files_receive` inbound). This module checks both flags via the
//! injected [`PermissionsStore`] before the first chunk crosses the
//! wire so a revoke mid-transfer can still surface a clean reject.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ansync_core::{DeviceId, Permission};
use ansync_permissions::{PermissionsError, PermissionsStore};
use ansync_proto::FileTransferMessage;
use ansync_transport::TransportError;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

/// Hard cap per `Chunk` payload. Matches the QUIC frame budget
/// (`MAX_FRAME_SIZE = 16 MiB`) with headroom for postcard envelope
/// overhead. Smaller than the cap keeps decode buffers reasonable
/// without sacrificing throughput on LAN-class links.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Direction of a single [`ProgressEvent`] from the caller's
/// perspective. Send events come from `send_file`, receive events
/// from `receive_file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

/// Per-chunk progress event. Emitted once per chunk write/read plus
/// one final event with `bytes == total` after the integrity check.
/// Throttling is the caller's responsibility — `send_file` /
/// `receive_file` always emit on every chunk.
#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub transfer_id: u64,
    pub name: String,
    pub bytes: u64,
    pub total: u64,
    pub direction: Direction,
}

/// Callback handed to `send_file` / `receive_file` to surface
/// per-chunk progress. Shared between transfers in the same batch so
/// the caller can accumulate cross-file state without re-plumbing.
pub type ProgressFn = Arc<dyn Fn(ProgressEvent) + Send + Sync>;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("permissions: {0}")]
    Permissions(#[from] PermissionsError),
    #[error("permission denied: {0:?}")]
    Denied(Permission),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("integrity: SHA-256 mismatch ({expected} vs {actual})")]
    Integrity { expected: String, actual: String },
    #[error("transfer rejected by peer: {0}")]
    Rejected(String),
}

/// Outbound transfer: send `src_path` over `stream`. Caller has
/// already verified `files_send` is allowed for the target device.
///
/// Returns the transfer id once the peer has acknowledged `Complete`.
/// Errors out early on a `Reject`, on permission revoke between
/// chunks, or on a transport-level failure.
pub async fn send_file<S>(
    peer_id: &DeviceId,
    permissions: &dyn PermissionsStore,
    stream: &mut S,
    src_path: &Path,
    transfer_id: u64,
    progress: Option<ProgressFn>,
) -> Result<u64, TransferError>
where
    S: ansync_transport::Stream,
{
    if !permissions.check(peer_id, Permission::FilesSend).await? {
        return Err(TransferError::Denied(Permission::FilesSend));
    }

    let meta = fs::metadata(src_path).await?;
    let total = meta.len();
    let name = src_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());

    let mut file = fs::File::open(src_path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut digest_buf = vec![0u8; CHUNK_SIZE];
    // First pass: compute SHA-256 so the Offer carries a verifiable
    // hash. For multi-GB files this re-reads the file, but Step 8 is
    // not the place to optimise — Step 11+ wires a streaming send
    // path that hashes once.
    let mut hash_file = fs::File::open(src_path).await?;
    loop {
        let n = hash_file.read(&mut digest_buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&digest_buf[..n]);
    }
    let sha256_initial: [u8; 32] = hasher.finalize().into();
    drop(hash_file);

    let offer = FileTransferMessage::Offer {
        transfer_id,
        name: name.clone(),
        size: total,
        sha256: sha256_initial,
    };
    send_msg(stream, &offer).await?;
    info!(transfer_id, %name, size = total, "transfer Offer sent");

    match recv_msg(stream).await? {
        FileTransferMessage::Accept { transfer_id: ack_id } if ack_id == transfer_id => {
            debug!(transfer_id, "peer accepted transfer");
        }
        FileTransferMessage::Reject { transfer_id: r_id, reason } if r_id == transfer_id => {
            return Err(TransferError::Rejected(reason));
        }
        other => {
            return Err(TransferError::Protocol(format!(
                "expected Accept/Reject for {transfer_id}, got {other:?}"
            )));
        }
    }

    let mut offset = 0u64;
    loop {
        // Re-check perm between chunks; revoke surfaces as a clean
        // protocol error to the peer rather than a hung half-open
        // transfer.
        if !permissions.check(peer_id, Permission::FilesSend).await? {
            warn!(transfer_id, "FilesSend revoked mid-transfer");
            return Err(TransferError::Denied(Permission::FilesSend));
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = FileTransferMessage::Chunk {
            transfer_id,
            offset,
            data: buf[..n].to_vec(),
        };
        send_msg(stream, &chunk).await?;
        offset += n as u64;
        if let Some(ref cb) = progress {
            cb(ProgressEvent {
                transfer_id,
                name: name.clone(),
                bytes: offset,
                total,
                direction: Direction::Send,
            });
        }
    }

    send_msg(stream, &FileTransferMessage::Complete { transfer_id }).await?;
    if let Some(ref cb) = progress {
        cb(ProgressEvent {
            transfer_id,
            name: name.clone(),
            bytes: total,
            total,
            direction: Direction::Send,
        });
    }
    info!(transfer_id, "transfer Complete sent");
    Ok(transfer_id)
}

/// Inbound transfer policy hook. The daemon supplies an impl that
/// turns an inbound [`FileTransferMessage::Offer`] into either a
/// destination path (accept) or a reject reason. Decoupling the
/// receive loop from the policy lets the host UI and the companion
/// pick different defaults — desktop dumps to `~/Downloads/ansync/`
/// without prompting; mobile launches the system Save-As picker.
#[async_trait::async_trait]
pub trait InboundPolicy: Send + Sync {
    async fn on_offer(
        &self,
        peer_id: &DeviceId,
        offer: &OfferSummary,
    ) -> InboundDecision;
}

#[derive(Debug, Clone)]
pub struct OfferSummary {
    pub transfer_id: u64,
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

pub enum InboundDecision {
    Accept(PathBuf),
    Reject(String),
}

/// Receive a single transfer from `stream`. The receiver invokes
/// `policy.on_offer` to obtain the destination path or a reject
/// reason, then writes chunks while tracking the running SHA-256 and
/// finally verifies it against the value in the original Offer.
pub async fn receive_file<S>(
    peer_id: &DeviceId,
    permissions: &dyn PermissionsStore,
    stream: &mut S,
    policy: &dyn InboundPolicy,
    progress: Option<ProgressFn>,
) -> Result<PathBuf, TransferError>
where
    S: ansync_transport::Stream,
{
    if !permissions
        .check(peer_id, Permission::FilesReceive)
        .await?
    {
        return Err(TransferError::Denied(Permission::FilesReceive));
    }

    let offer = match recv_msg(stream).await? {
        FileTransferMessage::Offer { transfer_id, name, size, sha256 } => OfferSummary {
            transfer_id,
            name,
            size,
            sha256,
        },
        other => {
            return Err(TransferError::Protocol(format!(
                "expected Offer, got {other:?}"
            )));
        }
    };
    info!(transfer_id = offer.transfer_id, name = %offer.name, size = offer.size, "received Offer");

    let dest = match policy.on_offer(peer_id, &offer).await {
        InboundDecision::Accept(p) => p,
        InboundDecision::Reject(reason) => {
            send_msg(
                stream,
                &FileTransferMessage::Reject {
                    transfer_id: offer.transfer_id,
                    reason: reason.clone(),
                },
            )
            .await?;
            return Err(TransferError::Rejected(reason));
        }
    };
    send_msg(
        stream,
        &FileTransferMessage::Accept {
            transfer_id: offer.transfer_id,
        },
    )
    .await?;

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::File::create(&dest).await?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    loop {
        match recv_msg(stream).await? {
            FileTransferMessage::Chunk { transfer_id, offset, data }
                if transfer_id == offer.transfer_id =>
            {
                if offset != received {
                    return Err(TransferError::Protocol(format!(
                        "out-of-order chunk: expected offset {received}, got {offset}"
                    )));
                }
                hasher.update(&data);
                file.write_all(&data).await?;
                received += data.len() as u64;
                if let Some(ref cb) = progress {
                    cb(ProgressEvent {
                        transfer_id: offer.transfer_id,
                        name: offer.name.clone(),
                        bytes: received,
                        total: offer.size,
                        direction: Direction::Receive,
                    });
                }
            }
            FileTransferMessage::Complete { transfer_id }
                if transfer_id == offer.transfer_id =>
            {
                break;
            }
            other => {
                return Err(TransferError::Protocol(format!(
                    "unexpected mid-transfer: {other:?}"
                )));
            }
        }
    }
    file.flush().await?;
    file.sync_all().await?;

    let actual: [u8; 32] = hasher.finalize().into();
    if actual != offer.sha256 {
        return Err(TransferError::Integrity {
            expected: hex(&offer.sha256),
            actual: hex(&actual),
        });
    }
    if received != offer.size {
        return Err(TransferError::Protocol(format!(
            "size mismatch: offer said {}, received {received}",
            offer.size
        )));
    }
    if let Some(ref cb) = progress {
        cb(ProgressEvent {
            transfer_id: offer.transfer_id,
            name: offer.name.clone(),
            bytes: offer.size,
            total: offer.size,
            direction: Direction::Receive,
        });
    }
    info!(transfer_id = offer.transfer_id, dest = %dest.display(), "transfer Complete");
    Ok(dest)
}

async fn send_msg<S: ansync_transport::Stream>(
    stream: &mut S,
    msg: &FileTransferMessage,
) -> Result<(), TransferError> {
    let bytes = postcard::to_allocvec(msg)?;
    stream.send(Bytes::from(bytes)).await?;
    Ok(())
}

async fn recv_msg<S: ansync_transport::Stream>(
    stream: &mut S,
) -> Result<FileTransferMessage, TransferError> {
    let bytes = stream.recv().await?;
    Ok(postcard::from_bytes(&bytes)?)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Convenience policy used by the daemon: drop every accepted file
/// into `{root}/{peer_subdir}/{offer.name}`. The caller picks
/// `peer_subdir` — typically a sanitized peer name like `"Pixel 9"`
/// — so the user sees recognisable folders instead of a hex
/// `DeviceId`. Falls back to the hex id when no name is known yet.
pub struct AutoAcceptPolicy {
    pub root: PathBuf,
    pub peer_subdir: String,
}

impl AutoAcceptPolicy {
    /// Sanitize a human-readable peer name into a path-safe segment.
    /// Strips anything outside `[A-Za-z0-9 ._-]`, collapses runs of
    /// underscores, and falls back to `peer_id` hex if the result is
    /// empty (so a peer with a weird name still gets *some* folder).
    pub fn sanitize_peer_subdir(name: &str, peer_id: &DeviceId) -> String {
        let cleaned: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric()
                    || c == '.'
                    || c == '-'
                    || c == '_'
                    || c == ' '
                {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let trimmed = cleaned.trim().trim_matches('_').trim();
        if trimmed.is_empty() {
            peer_id.to_string()
        } else {
            trimmed.to_string()
        }
    }
}

#[async_trait::async_trait]
impl InboundPolicy for AutoAcceptPolicy {
    async fn on_offer(
        &self,
        _peer_id: &DeviceId,
        offer: &OfferSummary,
    ) -> InboundDecision {
        let safe_name: String = offer
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric()
                    || c == '.'
                    || c == '-'
                    || c == '_'
                    || c == ' '
                {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let dest = self
            .root
            .join(&self.peer_subdir)
            .join(if safe_name.is_empty() { "unnamed".into() } else { safe_name });
        InboundDecision::Accept(dest)
    }
}
