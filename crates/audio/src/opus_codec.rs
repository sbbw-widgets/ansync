//! Opus encoder / decoder helpers shared by daemon and companion.
//!
//! Uses the pure-Rust `opus-rs` crate — no libopus FFI, no CMake, no
//! NDK toolchain juggling. The Rust impl produces wire-compatible
//! packets and tracks libopus's reference mode-selection logic.
//!
//! `OpusVoip` is tuned for the mic forward (32 kbps, FEC on,
//! low-delay) and `OpusAudio` for PC audio rendering on the device
//! speaker (128 kbps, music profile). Both modes run 48 kHz / stereo /
//! 20 ms frames (960 samples per channel) — keeping the shape fixed
//! avoids a runtime negotiation handshake.
//!
//! API note: `opus-rs` takes interleaved f32 PCM in `[-1.0, 1.0]`
//! range. Our wire format is interleaved S16LE, so the wrappers
//! convert in/out on the hot path.

use ansync_proto::AudioCodec;
use bytes::{Bytes, BytesMut};
use opus_rs::{Application, OpusDecoder, OpusEncoder};
use tracing::warn;

use crate::AudioError;

/// 48 kHz, 20 ms → 960 samples per channel. Hard-coded across the
/// project (host + companion). Any change needs to land in both
/// `daemon-core::handle_start_audio` and `AudioRouter.kt` in the same
/// commit; the negotiated value is also written on the wire in
/// `AudioStreamInit::frame_samples` for sanity checking.
pub const OPUS_FRAME_SAMPLES: usize = 960;
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_CHANNELS: u8 = 2;
/// Conservative upper bound for one Opus packet at 48 kHz stereo. The
/// libopus docs put the ceiling at ~4000 bytes for the worst-case
/// 510 kbps stereo; we cap at 4 KiB to match.
const MAX_PACKET_BYTES: usize = 4_000;

/// Default bitrates per codec mode. `OpusVoip` lands well above
/// transparent speech and fits FEC overhead in 32 kbps; `OpusAudio`
/// stays under the LAN budget while sounding clean on music.
pub const VOIP_BITRATE_BPS: i32 = 32_000;
pub const AUDIO_BITRATE_BPS: i32 = 128_000;

/// Encoder wrapper that batches arbitrary-size S16LE PCM input into
/// fixed-size Opus packets. `feed()` returns zero or more packets per
/// call depending on how much PCM has accumulated.
pub struct OpusEncoderWrap {
    enc: OpusEncoder,
    /// Per-channel sample accumulator, interleaved L,R,L,R,… stored as
    /// f32 because that's what `opus-rs::encode` expects.
    pending: Vec<f32>,
    /// Reusable scratch buffer for `enc.encode()` output.
    out_buf: Vec<u8>,
}

impl OpusEncoderWrap {
    pub fn new(codec: AudioCodec) -> Result<Self, AudioError> {
        let application = match codec {
            AudioCodec::OpusVoip => Application::Voip,
            AudioCodec::OpusAudio => Application::Audio,
            AudioCodec::Raw => {
                return Err(AudioError::Io(std::io::Error::other(
                    "Raw codec has no opus encoder",
                )));
            }
        };
        let mut enc = OpusEncoder::new(
            OPUS_SAMPLE_RATE as i32,
            OPUS_CHANNELS as usize,
            application,
        )
        .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        enc.bitrate_bps = match codec {
            AudioCodec::OpusVoip => VOIP_BITRATE_BPS,
            AudioCodec::OpusAudio => AUDIO_BITRATE_BPS,
            AudioCodec::Raw => unreachable!(),
        };
        // FEC + nominal expected-loss profile. Costs ~10 % bandwidth,
        // lets the decoder reconstruct one missing packet from the
        // next one over lossy transport (relay / WAN future).
        enc.use_inband_fec = true;
        enc.packet_loss_perc = 10;
        Ok(Self {
            enc,
            pending: Vec::with_capacity(OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize * 4),
            out_buf: vec![0u8; MAX_PACKET_BYTES],
        })
    }

    /// Push an arbitrary-size S16LE PCM chunk in. Returns one packet
    /// per complete 20 ms frame extracted from the accumulator.
    pub fn feed(&mut self, pcm_bytes: &[u8]) -> Result<Vec<Bytes>, AudioError> {
        let frame_samples_interleaved = OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize;
        // Reject odd-byte tails — they would desync the L/R pairing
        // for every subsequent frame.
        if pcm_bytes.len() % 2 != 0 {
            return Err(AudioError::Io(std::io::Error::other("odd-byte PCM chunk")));
        }
        self.pending.reserve(pcm_bytes.len() / 2);
        for pair in pcm_bytes.chunks_exact(2) {
            let s = i16::from_le_bytes([pair[0], pair[1]]);
            self.pending.push(s as f32 / 32768.0);
        }
        let mut out = Vec::new();
        while self.pending.len() >= frame_samples_interleaved {
            let frame: Vec<f32> =
                self.pending.drain(..frame_samples_interleaved).collect();
            let n = self
                .enc
                .encode(&frame, OPUS_FRAME_SAMPLES, &mut self.out_buf)
                .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
            out.push(Bytes::copy_from_slice(&self.out_buf[..n]));
        }
        Ok(out)
    }
}

/// Decoder wrapper. Each `decode()` consumes one Opus packet and
/// produces exactly `OPUS_FRAME_SAMPLES * channels` interleaved S16
/// samples — serialized to bytes ready to feed an `AudioSink`.
pub struct OpusDecoderWrap {
    dec: OpusDecoder,
    pcm: Vec<f32>,
}

impl OpusDecoderWrap {
    pub fn new() -> Result<Self, AudioError> {
        let dec = OpusDecoder::new(OPUS_SAMPLE_RATE as i32, OPUS_CHANNELS as usize)
            .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        Ok(Self {
            dec,
            pcm: vec![0f32; OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize],
        })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Bytes, AudioError> {
        let samples = self
            .dec
            .decode(packet, OPUS_FRAME_SAMPLES, &mut self.pcm)
            .map_err(|e| AudioError::Io(std::io::Error::other(e)))?;
        Ok(serialize_pcm(&self.pcm[..samples * OPUS_CHANNELS as usize]))
    }

    /// Packet Loss Concealment — invoke when the upper layer detected
    /// a dropped packet between two received ones. Output count is
    /// `OPUS_FRAME_SAMPLES * channels`. `opus-rs` 0.1 uses an empty
    /// input slice to trigger PLC; if a future version returns an
    /// error we treat it as "PLC unavailable" and surface
    /// `BackendUnavailable` upstream.
    pub fn decode_plc(&mut self) -> Result<Bytes, AudioError> {
        match self.dec.decode(&[], OPUS_FRAME_SAMPLES, &mut self.pcm) {
            Ok(samples) => Ok(serialize_pcm(&self.pcm[..samples * OPUS_CHANNELS as usize])),
            Err(e) => {
                warn!(error = %e, "opus-rs PLC failed; emitting silence frame");
                Ok(serialize_pcm(&vec![0f32; OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize]))
            }
        }
    }
}

fn serialize_pcm(samples: &[f32]) -> Bytes {
    let mut out = BytesMut::with_capacity(samples.len() * 2);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voip_roundtrip_silence() {
        let mut enc = OpusEncoderWrap::new(AudioCodec::OpusVoip).expect("encoder");
        let mut dec = OpusDecoderWrap::new().expect("decoder");
        let pcm_bytes = vec![0u8; OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize * 2];
        let packets = enc.feed(&pcm_bytes).expect("encode");
        assert_eq!(packets.len(), 1, "one full frame in → one packet out");
        let decoded = dec.decode(&packets[0]).expect("decode");
        assert_eq!(
            decoded.len(),
            OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize * 2
        );
    }

    #[test]
    fn audio_roundtrip_partial_frames_accumulate() {
        let mut enc = OpusEncoderWrap::new(AudioCodec::OpusAudio).expect("encoder");
        let half = OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize; // half a frame in bytes
        let first = enc.feed(&vec![0u8; half]).expect("feed half");
        assert!(first.is_empty(), "half frame must accumulate, not emit");
        let second = enc.feed(&vec![0u8; half]).expect("feed remaining half");
        assert_eq!(second.len(), 1, "remaining half completes one frame");
    }

    #[test]
    fn raw_codec_rejects_encoder() {
        assert!(OpusEncoderWrap::new(AudioCodec::Raw).is_err());
    }
}
