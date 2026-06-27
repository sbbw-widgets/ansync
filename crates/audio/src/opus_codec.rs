//! Opus encoder / decoder helpers shared by daemon and companion.
//!
//! Topology recap (see `ansync_proto::AudioStreamInit`): the first
//! frame of every `StreamKind::Audio` declares codec + frame size; all
//! subsequent frames are one Opus packet each. `OpusVoip` is tuned for
//! the mic forward (32 kbps, FEC on, low-delay) and `OpusAudio` for PC
//! audio rendering on the device speaker (128 kbps, music profile).
//! Both modes run 48 kHz / stereo and 20 ms frames (960 samples per
//! channel) — keeping the shape fixed avoids a runtime negotiation
//! handshake.
//!
//! Caller pattern on the sending side:
//!
//! ```ignore
//! let mut enc = OpusEncoderWrap::new(AudioCodec::OpusVoip)?;
//! while let Ok(pcm) = source.read().await {
//!     for packet in enc.feed(&pcm)? {
//!         stream.send(packet).await?;
//!     }
//! }
//! ```
//!
//! On the receiving side, decode one packet per recv (or call
//! `decode_plc()` when the upper layer detects a loss).
//!
//! `audiopus` vendors libopus via `audiopus_sys`, so Android NDK
//! cross-compile works without a system libopus.

use ansync_proto::AudioCodec;
use audiopus::{
    Application, Bitrate, Channels, MutSignals, SampleRate,
    coder::{Decoder as OpusDecoder, Encoder as OpusEncoder},
    packet::Packet,
};
use core::convert::TryFrom;
use bytes::Bytes;
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
/// Maximum Opus packet bytes for 48 kHz stereo at 510 kbps — 4 000 is
/// the libopus-recommended ceiling.
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
    /// Per-channel sample accumulator, interleaved L,R,L,R,…
    pending: Vec<i16>,
    /// Reusable scratch buffer for `enc.encode()`.
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
        let sr = SampleRate::try_from(OPUS_SAMPLE_RATE as i32)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        let mut enc = OpusEncoder::new(sr, Channels::Stereo, application)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        let bps = match codec {
            AudioCodec::OpusVoip => VOIP_BITRATE_BPS,
            AudioCodec::OpusAudio => AUDIO_BITRATE_BPS,
            AudioCodec::Raw => unreachable!(),
        };
        if let Err(e) = enc.set_bitrate(Bitrate::BitsPerSecond(bps)) {
            warn!(error = %e, "opus set_bitrate failed; using default");
        }
        // FEC on for both modes — costs ~10 % bandwidth, lets the
        // decoder reconstruct one missing packet from the next one.
        if let Err(e) = enc.set_inband_fec(true) {
            warn!(error = %e, "opus inband_fec set failed");
        }
        if let Err(e) = enc.set_packet_loss_perc(10) {
            warn!(error = %e, "opus packet_loss_perc set failed");
        }
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
            return Err(AudioError::Io(std::io::Error::other(
                "odd-byte PCM chunk",
            )));
        }
        self.pending.reserve(pcm_bytes.len() / 2);
        for pair in pcm_bytes.chunks_exact(2) {
            self.pending
                .push(i16::from_le_bytes([pair[0], pair[1]]));
        }
        let mut out = Vec::new();
        while self.pending.len() >= frame_samples_interleaved {
            let frame: Vec<i16> = self.pending.drain(..frame_samples_interleaved).collect();
            let n = self
                .enc
                .encode(&frame, &mut self.out_buf)
                .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
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
    pcm: Vec<i16>,
}

impl OpusDecoderWrap {
    pub fn new() -> Result<Self, AudioError> {
        let sr = SampleRate::try_from(OPUS_SAMPLE_RATE as i32)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        let dec = OpusDecoder::new(sr, Channels::Stereo)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        Ok(Self {
            dec,
            pcm: vec![0i16; OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize],
        })
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Bytes, AudioError> {
        let pkt = Packet::try_from(packet)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        let out = MutSignals::try_from(&mut self.pcm[..])
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        let samples = self
            .dec
            .decode(Some(pkt), out, false)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        Ok(serialize_pcm(&self.pcm[..samples * OPUS_CHANNELS as usize]))
    }

    /// Packet Loss Concealment — invoke when the upper layer detected
    /// a dropped packet between two received ones. Output count is
    /// `OPUS_FRAME_SAMPLES * channels`. Tracked separately by the
    /// telemetry counter so we can compare against QUIC loss.
    pub fn decode_plc(&mut self) -> Result<Bytes, AudioError> {
        let out = MutSignals::try_from(&mut self.pcm[..])
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        let samples = self
            .dec
            .decode(None, out, false)
            .map_err(|e| AudioError::Io(std::io::Error::other(e.to_string())))?;
        Ok(serialize_pcm(&self.pcm[..samples * OPUS_CHANNELS as usize]))
    }
}

fn serialize_pcm(samples: &[i16]) -> Bytes {
    let mut out = bytes::BytesMut::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
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

    #[test]
    fn plc_emits_full_frame() {
        let mut dec = OpusDecoderWrap::new().expect("decoder");
        let out = dec.decode_plc().expect("plc");
        assert_eq!(out.len(), OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize * 2);
    }
}
