//! Length-prefixed framing for postcard-encoded envelopes.
//!
//! Frames are `u32` big-endian length followed by the postcard payload.
//! All control-plane traffic between peers is framed this way.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Envelope, Message, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame exceeds limit: {0} > {1}")]
    TooLarge(usize, usize),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Hard cap on a single control frame. Anything larger is malformed.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub fn encode_envelope(envelope: &Envelope) -> Result<Vec<u8>, FrameError> {
    Ok(postcard::to_allocvec(envelope)?)
}

pub fn decode_envelope(bytes: &[u8]) -> Result<Envelope, FrameError> {
    Ok(postcard::from_bytes(bytes)?)
}

pub fn encode_message(message: Message) -> Result<Vec<u8>, FrameError> {
    encode_envelope(&Envelope { version: PROTOCOL_VERSION, message })
}

pub async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(payload.len(), MAX_FRAME_SIZE));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| FrameError::TooLarge(payload.len(), MAX_FRAME_SIZE))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    Ok(())
}

pub async fn read_frame<R>(reader: &mut R, max: usize) -> Result<Vec<u8>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut hdr = [0u8; 4];
    reader.read_exact(&mut hdr).await?;
    let len = u32::from_be_bytes(hdr) as usize;
    if len > max {
        return Err(FrameError::TooLarge(len, max));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn write_envelope<W>(writer: &mut W, envelope: &Envelope) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = encode_envelope(envelope)?;
    write_frame(writer, &bytes).await
}

pub async fn read_envelope<R>(reader: &mut R, max: usize) -> Result<Envelope, FrameError>
where
    R: AsyncRead + Unpin,
{
    let bytes = read_frame(reader, max).await?;
    decode_envelope(&bytes)
}

pub async fn write_typed<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = postcard::to_allocvec(value)?;
    write_frame(writer, &bytes).await
}

pub async fn read_typed<R, T>(reader: &mut R, max: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let bytes = read_frame(reader, max).await?;
    Ok(postcard::from_bytes(&bytes)?)
}
