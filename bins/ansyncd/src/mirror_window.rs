//! Dev-only Annex-B file feeder + window launcher.
//!
//! Step 9.5 moved the renderer (`MirrorApp`, `FrameSlot`, conversions,
//! `run`) to `ansync_video::sink_egui` so daemon-core can spawn the
//! same window when a D-Bus client calls `ShowScreen`. What stays
//! here is the test-only loop that feeds the decoder from a local
//! Annex-B recording, kept behind the `dev-playback` feature.

#[cfg(feature = "dev-playback")]
use ansync_video::sink_egui::FrameSlot;
#[cfg(feature = "dev-playback")]
use ansync_video::{
    HostDecoder, VideoCodec, VideoDecoder, VideoError, feed::AnnexBFile, local_decoder_caps,
};
#[cfg(feature = "dev-playback")]
use tracing::{error, info, warn};

#[cfg(feature = "dev-playback")]
pub async fn run_play_file_loop(
    path: std::path::PathBuf,
    shared: FrameSlot,
) -> Result<(), VideoError> {
    let mut file = AnnexBFile::open(&path).await?;
    let codec = if file.is_h265() {
        VideoCodec::H265
    } else {
        VideoCodec::H264
    };
    let caps = local_decoder_caps();
    if !caps.can_decode.contains(&codec) {
        return Err(VideoError::DecoderUnavailable(format!(
            "local host cannot decode {codec:?} (caps: {:?})",
            caps.can_decode
        )));
    }
    let mut decoder = HostDecoder::configure(codec, 1920, 1080)?;
    info!(?codec, path = %path.display(), "decoder configured");
    let mut frame_period = tokio::time::interval(std::time::Duration::from_millis(33));
    loop {
        let Some(packet) = file.next_packet().await? else {
            info!("end of Annex-B stream");
            return Ok(());
        };
        if let Err(e) = decoder.feed(packet.data).await {
            warn!(error = %e, "feed failed; continuing");
            continue;
        }
        if let Some(frame) = decoder.take().await? {
            shared.store(frame);
        }
        frame_period.tick().await;
    }
}

#[cfg(feature = "dev-playback")]
pub fn spawn_play_file(
    runtime: &tokio::runtime::Runtime,
    path: std::path::PathBuf,
    shared: FrameSlot,
) {
    runtime.spawn(async move {
        if let Err(e) = run_play_file_loop(path, shared).await {
            error!(error = %e, "play-file decode loop exited with error");
        }
    });
}
