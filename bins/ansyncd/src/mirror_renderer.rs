//! Per-window mirror renderer subprocess.
//!
//! Daemon spawns `ansyncd mirror-renderer` per peer ShowScreen and
//! talks to it via the child's own stdin / stdout pipes:
//!
//!   - stdin: postcard `HostMsg` frames (Config once, then a stream
//!     of `EncodedChunk`).
//!   - stdout: postcard `RendererMsg` frames (Input events).
//!   - stderr: inherited — tracing writes go here, so logs land in
//!     the daemon's journal alongside its own output.
//!
//! Putting the renderer in its own process gives every mirror window
//! its own `winit::EventLoop::build` (no once-per-process guard
//! issue) and a clean exit when the user closes the window.

use ansync_video::ipc::{HostMsg, RendererMsg, read_msg, write_msg};
use ansync_video::sink_egui::{self, FrameSlot};
use ansync_video::{HostDecoder, VideoCodec, VideoDecoder};
use tokio::io::{AsyncWriteExt, stdin, stdout};
use tokio::sync::mpsc::unbounded_channel;
use tracing::{error, info, warn};

/// Entry point for the `mirror-renderer` subcommand. Returns only
/// after the window closes (or any fatal error). Caller owns the
/// process exit code.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let (codec, width, height, title, slot, input_tx) =
        rt.block_on(async move { bootstrap().await })?;

    info!(
        ?codec,
        width,
        height,
        %title,
        "mirror-renderer: bootstrap complete; opening window"
    );

    let result = sink_egui::run(title, slot, Some(input_tx));
    drop(rt);
    result
}

async fn bootstrap() -> Result<
    (
        VideoCodec,
        u32,
        u32,
        String,
        FrameSlot,
        tokio::sync::mpsc::UnboundedSender<ansync_proto::InputMessage>,
    ),
    Box<dyn std::error::Error>,
> {
    let mut stdin_in = stdin();

    let cfg: HostMsg = read_msg(&mut stdin_in)
        .await?
        .ok_or("EOF before Config")?;
    let HostMsg::Config {
        codec,
        width,
        height,
        title,
    } = cfg
    else {
        return Err("expected Config as first message".into());
    };
    let codec: VideoCodec = codec.into();

    let slot = sink_egui::new_slot();
    let (input_tx, input_rx) = unbounded_channel::<ansync_proto::InputMessage>();

    // Reader task: pull EncodedChunk frames from stdin, feed the
    // decoder, push decoded frames into the slot.
    {
        let slot = slot.clone();
        tokio::spawn(async move {
            let mut decoder = match HostDecoder::configure(codec, width, height) {
                Ok(d) => d,
                Err(e) => {
                    error!(error = %e, "decoder configure failed in renderer");
                    std::process::exit(2);
                }
            };
            loop {
                match read_msg::<_, HostMsg>(&mut stdin_in).await {
                    Ok(Some(HostMsg::EncodedChunk { data, pts_us: _ })) => {
                        if let Err(e) = decoder.feed(bytes::Bytes::from(data)).await {
                            warn!(error = %e, "decoder feed failed");
                            continue;
                        }
                        match decoder.take().await {
                            Ok(Some(frame)) => slot.store(frame),
                            Ok(None) => {}
                            Err(e) => warn!(error = %e, "decoder take failed"),
                        }
                    }
                    Ok(Some(HostMsg::Config { .. })) => {
                        warn!("unexpected Config mid-stream; ignoring");
                    }
                    Ok(Some(HostMsg::Shutdown)) => {
                        info!("daemon requested shutdown");
                        std::process::exit(0);
                    }
                    Ok(None) => {
                        info!("daemon closed stdin; exiting");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        warn!(error = %e, "stdin read failed; exiting");
                        std::process::exit(1);
                    }
                }
            }
        });
    }

    // Writer task: drain InputMessage from sink_egui's channel,
    // postcard-encode onto stdout.
    {
        tokio::spawn(async move {
            let mut out = stdout();
            let mut rx = input_rx;
            while let Some(msg) = rx.recv().await {
                if let Err(e) = write_msg(&mut out, &RendererMsg::Input(msg)).await {
                    warn!(error = %e, "input forward failed; closing");
                    return;
                }
            }
            let _ = out.shutdown().await;
        });
    }

    Ok((codec, width, height, title, slot, input_tx))
}
