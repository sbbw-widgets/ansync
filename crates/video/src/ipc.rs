//! Length-prefixed postcard wire protocol between the daemon and a
//! per-peer mirror renderer subprocess.
//!
//! The daemon spawns one `ansyncd mirror-renderer …` subprocess per
//! open mirror window. Each subprocess gets a dedicated Unix socket
//! and brings up its own `eframe::run_native` — that way winit's
//! once-per-process `EventLoop::build` guard never trips, and closing
//! a mirror window means the subprocess just exits.
//!
//! Wire format: `u32-le length` + `postcard::to_allocvec(msg)`.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ErrorKind};

use crate::VideoCodec;
use ansync_proto::InputMessage;

/// Codec identifier flattened to wire-friendly u8 — the wire side
/// doesn't need to match the in-crate enum layout, and a future
/// codec addition shouldn't silently shift the existing tag values.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum WireCodec {
    H264,
    H265,
}

impl From<VideoCodec> for WireCodec {
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::H264 => WireCodec::H264,
            VideoCodec::H265 => WireCodec::H265,
        }
    }
}

impl From<WireCodec> for VideoCodec {
    fn from(c: WireCodec) -> Self {
        match c {
            WireCodec::H264 => VideoCodec::H264,
            WireCodec::H265 => VideoCodec::H265,
        }
    }
}

/// Messages the daemon sends to the renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    /// Always the first frame on the socket. Carries everything the
    /// renderer needs to bring up its decoder + window.
    Config {
        codec: WireCodec,
        width: u32,
        height: u32,
        title: String,
    },
    /// One encoded NAL access unit. The daemon forwards Annex-B
    /// chunks straight off the wire — the subprocess owns the
    /// decoder, so the daemon never spins a `HostDecoder` for the
    /// mirror path.
    EncodedChunk { data: Vec<u8>, pts_us: u64 },
    /// Polite request — renderer should close its window and exit.
    /// Used by D-Bus `HideScreen` to tear the subprocess down without
    /// sending SIGTERM.
    Shutdown,
}

/// Messages the renderer sends back to the daemon.
#[derive(Debug, Serialize, Deserialize)]
pub enum RendererMsg {
    /// Forwarded user input — daemon translates straight into the
    /// existing per-peer `input_writer_loop`.
    Input(InputMessage),
}

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Length-prefix + postcard encode + write. Returns `Ok` only when
/// the full frame has been flushed onto the socket.
pub async fn write_msg<W, M>(stream: &mut W, msg: &M) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    M: Serialize,
{
    let body = postcard::to_allocvec(msg)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e.to_string()))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("frame too large: {} bytes", body.len()),
        ));
    }
    let len = body.len() as u32;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// Read a length-prefixed postcard frame. Returns `None` on a clean
/// EOF (peer closed the socket), `Err` on any partial-read or
/// framing failure.
pub async fn read_msg<R, M>(stream: &mut R) -> std::io::Result<Option<M>>
where
    R: AsyncReadExt + Unpin,
    M: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("frame too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let msg: M = postcard::from_bytes(&buf)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(msg))
}
