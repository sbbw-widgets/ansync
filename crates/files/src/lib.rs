//! File transfer surface.
//!
//! Explicit push / pull with `TransferProgress` reporting, used by
//! `ansyncctl push` and the D-Bus `SendFile` method.

pub mod transfer;

pub use transfer::{
    AutoAcceptPolicy, CHUNK_SIZE, InboundDecision, InboundPolicy, OfferSummary, TransferError,
    receive_file, send_file,
};

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub transfer_id: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}
