//! Test data feeders for the decode hot path.
//!
//! Until the Android companion lands in Step 7+, the daemon has no
//! real source of H.264 / H.265 packets. This module exposes a
//! `tokio::fs`-backed Annex-B reader so the `--play-file` flag on
//! `ansyncd` can drive [`HostDecoder`](crate::HostDecoder) from a
//! local recording. Each emitted [`AnnexBPacket`] groups one or more
//! NAL units that share a single Access Unit (AUD-delimited or
//! detected by first-VCL-of-frame heuristic), which is the shape
//! `ferricast-decoder` expects to consume per `decode()` call.

use std::path::Path;

use bytes::Bytes;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

use crate::VideoError;

/// One Access Unit's worth of NAL bytes including the `0x00 00 00 01`
/// start codes preceding each NAL — passed straight through to
/// ferricast, which accepts Annex-B framing.
#[derive(Debug, Clone)]
pub struct AnnexBPacket {
    pub data: Bytes,
}

/// Streaming reader over an Annex-B encoded `.h264` / `.h265` file.
///
/// Holds a small rolling buffer (default 1 MiB) and yields one
/// [`AnnexBPacket`] per Access Unit. Detection of AU boundaries is
/// intentionally loose: we split on every NAL start code and group
/// runs whose first NAL is an AUD (`nal_unit_type 9` for H.264,
/// `nal_unit_type 35` for H.265) or the first VCL NAL after a
/// non-VCL run. For Step 6 the goal is "feed the decoder enough to
/// produce frames"; bit-exact AU framing is the encoder's job.
pub struct AnnexBFile {
    reader: BufReader<File>,
    buf: Vec<u8>,
    eof: bool,
    is_h265: bool,
}

const READ_CHUNK: usize = 64 * 1024;

impl AnnexBFile {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, VideoError> {
        let path = path.as_ref();
        let is_h265 = matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("h265" | "hevc" | "265")
        );
        let file = File::open(path).await?;
        Ok(Self {
            reader: BufReader::new(file),
            buf: Vec::with_capacity(READ_CHUNK * 4),
            eof: false,
            is_h265,
        })
    }

    pub fn is_h265(&self) -> bool {
        self.is_h265
    }

    /// Pull the next Access Unit from the file. Returns `Ok(None)` at
    /// EOF after the trailing bytes have been drained.
    pub async fn next_packet(&mut self) -> Result<Option<AnnexBPacket>, VideoError> {
        loop {
            if let Some(packet) = self.try_extract() {
                return Ok(Some(packet));
            }
            if self.eof {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let data = Bytes::copy_from_slice(&self.buf);
                self.buf.clear();
                return Ok(Some(AnnexBPacket { data }));
            }
            let mut tmp = [0u8; READ_CHUNK];
            let n = self.reader.read(&mut tmp).await?;
            if n == 0 {
                self.eof = true;
                continue;
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    fn try_extract(&mut self) -> Option<AnnexBPacket> {
        // Need to see the start of one AU, then the start of the
        // next AU before we can cut. Walk the buffer looking for
        // start codes and apply the boundary policy.
        let starts = find_start_codes(&self.buf);
        if starts.len() < 2 {
            return None;
        }
        let cut = pick_au_boundary(&self.buf, &starts, self.is_h265)?;
        if cut == 0 {
            return None;
        }
        let bytes = Bytes::copy_from_slice(&self.buf[..cut]);
        self.buf.drain(..cut);
        Some(AnnexBPacket { data: bytes })
    }
}

/// Find every start-code offset in `buf`. Accepts both the 3-byte
/// (`00 00 01`) and 4-byte (`00 00 00 01`) variants; the offset
/// returned points at the first `0x00`.
fn find_start_codes(buf: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                out.push(i);
                i += 3;
                continue;
            }
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                out.push(i);
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Given the list of start-code offsets, return the byte index at
/// which to cut the current Access Unit. Strategy: cut just before
/// the second start code that looks like the head of a new AU
/// (either an AUD NAL, the first VCL NAL after a non-VCL run, or any
/// VCL NAL at frame boundary). Returns `None` if no boundary is
/// confident yet — caller will read more bytes and retry.
fn pick_au_boundary(buf: &[u8], starts: &[usize], is_h265: bool) -> Option<usize> {
    // Skip the first start-code (it's the head of the *current* AU).
    let mut seen_vcl = false;
    for window in starts.windows(2) {
        let nal_off = nal_header_offset(buf, window[0])?;
        if nal_off >= buf.len() {
            return None;
        }
        let nal_byte = buf[nal_off];
        let is_vcl = if is_h265 {
            // H.265 nal_unit_type lives in bits [1..7] of the first
            // header byte: `(byte >> 1) & 0x3f`. VCL: 0..31.
            let nut = (nal_byte >> 1) & 0x3f;
            nut < 32
        } else {
            // H.264 nal_unit_type is bits [0..5] of the first header
            // byte: `byte & 0x1f`. VCL: 1..5.
            let nut = nal_byte & 0x1f;
            (1..=5).contains(&nut)
        };
        if is_vcl {
            seen_vcl = true;
        }
        // Next start-code offset, candidate boundary.
        let next_start = window[1];
        let next_off = nal_header_offset(buf, next_start)?;
        if next_off >= buf.len() {
            return None;
        }
        let next_byte = buf[next_off];
        let next_is_aud = if is_h265 {
            ((next_byte >> 1) & 0x3f) == 35
        } else {
            (next_byte & 0x1f) == 9
        };
        let next_is_vcl = if is_h265 {
            ((next_byte >> 1) & 0x3f) < 32
        } else {
            let nut = next_byte & 0x1f;
            (1..=5).contains(&nut)
        };
        if next_is_aud || (seen_vcl && next_is_vcl) {
            return Some(next_start);
        }
    }
    None
}

/// Skip the 3- or 4-byte start code at `off` and return the offset of
/// the NAL header byte that follows.
fn nal_header_offset(buf: &[u8], off: usize) -> Option<usize> {
    if off + 3 <= buf.len() && buf[off] == 0 && buf[off + 1] == 0 && buf[off + 2] == 1 {
        return Some(off + 3);
    }
    if off + 4 <= buf.len()
        && buf[off] == 0
        && buf[off + 1] == 0
        && buf[off + 2] == 0
        && buf[off + 3] == 1
    {
        return Some(off + 4);
    }
    None
}
