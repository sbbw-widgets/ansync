//! Per-window mirror renderer subprocess.
//!
//! Daemon spawns `ansyncd mirror-renderer --sock PATH` per peer
//! ShowScreen. The subprocess connects to the Unix socket, reads a
//! `HostMsg::Config`, brings up its own `HostDecoder` + `eframe`
//! window, then loops over `HostMsg::EncodedChunk` decoding straight
//! to the window's frame slot. Closing the window exits the process.
//!
//! This puts each mirror in its own winit `EventLoop::build`, so the
//! once-per-process guard never trips — the daemon can open, close
//! and reopen as many mirror windows as the user asks for.

use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;

use ansync_video::ipc::{HostMsg, RendererMsg, read_msg, write_msg};
use ansync_video::sink_egui::{self, FrameSlot};
use ansync_video::{HostDecoder, VideoCodec, VideoDecoder};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream as TokioUnixStream;
use tokio::sync::mpsc::unbounded_channel;
use tracing::{error, info, warn};

/// Entry point for the `mirror-renderer` subcommand. Returns only
/// after the window closes (or any fatal error). Caller owns the
/// process exit code.
pub fn run(sock: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    // First contact + handshake happens on the tokio runtime so we
    // can use the same `read_msg`/`write_msg` helpers the daemon uses
    // on its side. We then synchronously block on the eframe loop on
    // the calling thread (eframe takes over the main thread).
    let (codec, width, height, title, slot, input_tx) =
        rt.block_on(async move { bootstrap(sock).await })?;

    info!(
        ?codec,
        width,
        height,
        %title,
        "mirror-renderer: bootstrap complete; opening window"
    );

    // Block this thread on eframe forever. Drop the runtime before
    // returning so any spawned tasks tear down cleanly.
    let result = sink_egui::run(title, slot, Some(input_tx));
    drop(rt);
    result
}

async fn bootstrap(
    sock: PathBuf,
) -> Result<
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
    let std_stream = StdUnixStream::connect(&sock)
        .map_err(|e| format!("connect {}: {e}", sock.display()))?;
    std_stream.set_nonblocking(true)?;
    let stream = TokioUnixStream::from_std(std_stream)?;
    let (mut read_half, write_half) = stream.into_split();

    let cfg: HostMsg = read_msg(&mut read_half)
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

    // Reader task: pull EncodedChunk frames off the socket, feed the
    // decoder, push decoded frames into the slot.
    {
        let slot = slot.clone();
        tokio::spawn(async move {
            let mut decoder = match HostDecoder::configure(codec, width, height) {
                Ok(d) => d,
                Err(e) => {
                    error!(error = %e, "decoder configure failed in renderer");
                    return;
                }
            };
            loop {
                match read_msg::<_, HostMsg>(&mut read_half).await {
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
                        info!("daemon closed mirror socket; exiting");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        warn!(error = %e, "socket read failed; exiting");
                        std::process::exit(1);
                    }
                }
            }
        });
    }

    // Writer task: drain InputMessage out of the channel sink_egui
    // feeds, forward each as a RendererMsg::Input over the socket.
    {
        tokio::spawn(async move {
            let mut writer = write_half;
            let mut rx = input_rx;
            while let Some(msg) = rx.recv().await {
                if let Err(e) = write_msg(&mut writer, &RendererMsg::Input(msg)).await {
                    warn!(error = %e, "input forward failed; closing");
                    return;
                }
            }
            let _ = writer.shutdown().await;
        });
    }

    Ok((codec, width, height, title, slot, input_tx))
}
