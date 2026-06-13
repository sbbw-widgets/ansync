//! Sequential async RPC client over a QUIC `StreamKind::Fs` stream.
//!
//! One request at a time: the caller submits an op, the client
//! serialises a [`FsOpMessage`], writes a frame, reads the reply
//! frame, decodes, returns. A `tokio::sync::Mutex` around the stream
//! enforces sequencing so concurrent FUSE callbacks (running on
//! independent kernel threads) don't interleave half-frames.
//!
//! Concurrency in-flight = 1 by design today. The FUSE layer enforces
//! the higher-level "max 4 in-flight per device" cap by opening
//! additional Fs streams when it wants parallelism (Step 9 ships with
//! one stream; multi-stream is an optimisation noted in PLAN.md).

use std::sync::Arc;

use ansync_proto::{FsEntry, FsMeta, FsOpMessage};
use ansync_transport::TransportError;
use bytes::Bytes;
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum FsClientError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("peer error code={code}: {message}")]
    Remote { code: i32, message: String },
    #[error("protocol: expected {expected}, got {actual}")]
    Protocol { expected: &'static str, actual: String },
}

/// Async RPC handle. `Clone` is cheap (`Arc<Mutex<…>>`) so the FUSE
/// layer hands a clone to every spawned op task.
#[derive(Clone)]
pub struct FsClient<S: ansync_transport::Stream + 'static> {
    inner: Arc<Mutex<S>>,
}

impl<S: ansync_transport::Stream + 'static> FsClient<S> {
    pub fn new(stream: S) -> Self {
        Self {
            inner: Arc::new(Mutex::new(stream)),
        }
    }

    pub async fn stat(&self, path: &str) -> Result<FsMeta, FsClientError> {
        let reply = self.rpc(FsOpMessage::Stat { path: path.into() }).await?;
        match reply {
            FsOpMessage::StatReply { meta } => Ok(meta),
            other => Err(Self::unexpected("StatReply", other)),
        }
    }

    pub async fn readdir(&self, path: &str) -> Result<Vec<FsEntry>, FsClientError> {
        let reply = self
            .rpc(FsOpMessage::ReadDir { path: path.into() })
            .await?;
        match reply {
            FsOpMessage::ReadDirReply { entries } => Ok(entries),
            other => Err(Self::unexpected("ReadDirReply", other)),
        }
    }

    pub async fn open(&self, path: &str, flags: u32) -> Result<u64, FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Open {
                path: path.into(),
                flags,
            })
            .await?;
        match reply {
            FsOpMessage::OpenReply { handle } => Ok(handle),
            other => Err(Self::unexpected("OpenReply", other)),
        }
    }

    pub async fn create(&self, path: &str, mode: u32) -> Result<u64, FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Create {
                path: path.into(),
                mode,
            })
            .await?;
        match reply {
            FsOpMessage::CreateReply { handle } => Ok(handle),
            other => Err(Self::unexpected("CreateReply", other)),
        }
    }

    pub async fn read(&self, handle: u64, offset: u64, len: u32) -> Result<Bytes, FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Read { handle, offset, len })
            .await?;
        match reply {
            FsOpMessage::ReadReply { data } => Ok(Bytes::from(data)),
            other => Err(Self::unexpected("ReadReply", other)),
        }
    }

    pub async fn write(
        &self,
        handle: u64,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<u32, FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Write {
                handle,
                offset,
                data,
            })
            .await?;
        match reply {
            FsOpMessage::WriteReply { written } => Ok(written),
            other => Err(Self::unexpected("WriteReply", other)),
        }
    }

    pub async fn close(&self, handle: u64) -> Result<(), FsClientError> {
        let reply = self.rpc(FsOpMessage::Close { handle }).await?;
        match reply {
            FsOpMessage::Ok => Ok(()),
            other => Err(Self::unexpected("Ok", other)),
        }
    }

    pub async fn unlink(&self, path: &str) -> Result<(), FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Unlink { path: path.into() })
            .await?;
        match reply {
            FsOpMessage::Ok => Ok(()),
            other => Err(Self::unexpected("Ok", other)),
        }
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<(), FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Rename {
                from: from.into(),
                to: to.into(),
            })
            .await?;
        match reply {
            FsOpMessage::Ok => Ok(()),
            other => Err(Self::unexpected("Ok", other)),
        }
    }

    pub async fn truncate(&self, path: &str, size: u64) -> Result<(), FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Truncate {
                path: path.into(),
                size,
            })
            .await?;
        match reply {
            FsOpMessage::Ok => Ok(()),
            other => Err(Self::unexpected("Ok", other)),
        }
    }

    pub async fn chmod(&self, path: &str, mode: u32) -> Result<(), FsClientError> {
        let reply = self
            .rpc(FsOpMessage::Chmod {
                path: path.into(),
                mode,
            })
            .await?;
        match reply {
            FsOpMessage::Ok => Ok(()),
            other => Err(Self::unexpected("Ok", other)),
        }
    }

    async fn rpc(&self, req: FsOpMessage) -> Result<FsOpMessage, FsClientError> {
        let bytes = postcard::to_allocvec(&req)?;
        let mut guard = self.inner.lock().await;
        guard.send(Bytes::from(bytes)).await?;
        let reply_bytes = guard.recv().await?;
        let reply: FsOpMessage = postcard::from_bytes(&reply_bytes)?;
        if let FsOpMessage::Error { code, message } = reply {
            return Err(FsClientError::Remote { code, message });
        }
        Ok(reply)
    }

    fn unexpected(expected: &'static str, actual: FsOpMessage) -> FsClientError {
        FsClientError::Protocol {
            expected,
            actual: format!("{actual:?}"),
        }
    }
}
