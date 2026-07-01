//! Daemon-side glue for the per-window mirror renderer subprocess.
//!
//! Each ShowScreen spawns one `ansyncd mirror-renderer` child and
//! talks to it over the child's own stdin / stdout pipes. No Unix
//! socket, no path on disk: pipes are inherited at fork, close on
//! child exit, and need no cleanup. Stderr is inherited so renderer
//! logs land in the same journal as the daemon's.
//!
//! Wire format mirrors `ansync_video::ipc`:
//!   - Daemon → child (stdin): `HostMsg::Config` once, then
//!     `EncodedChunk` per NAL access unit, finally `Shutdown` on
//!     graceful HideScreen.
//!   - Child → daemon (stdout): `RendererMsg::Input` per pointer /
//!     keyboard / gamepad event.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ansync_proto::InputMessage;
use ansync_video::ipc::{HostMsg, RendererMsg, WireCodec, read_msg, write_msg};
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::{MirrorEntry, MirrorSubprocess};

pub async fn spawn_mirror_subprocess(
    title: String,
    entry: Arc<MirrorEntry>,
    input_tx: Option<UnboundedSender<InputMessage>>,
    on_exit: impl FnOnce() + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(&exe)
        .arg("mirror-renderer")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id().unwrap_or(0);
    let mut child_stdin = child.stdin.take().ok_or("no stdin handle")?;
    let mut child_stdout = child.stdout.take().ok_or("no stdout handle")?;
    info!(pid, "mirror renderer subprocess launched");

    // Send the initial Config so the renderer can size its decoder
    // and window before any chunks arrive. We currently hardcode
    // H.264 + 1080p; the SPS in the first IDR will refine real dims
    // inside the decoder.
    write_msg(
        &mut child_stdin,
        &HostMsg::Config {
            codec: WireCodec::H264,
            width: 1920,
            height: 1080,
            title,
        },
    )
    .await?;

    let (chunk_tx, mut chunk_rx) = unbounded_channel::<bytes::Bytes>();
    let (host_tx, mut host_rx) = unbounded_channel::<HostMsg>();

    *entry.video_tx.lock().expect("video_tx slot poisoned") = Some(chunk_tx);

    // Writer task: serialise chunks + HostMsg control messages onto
    // the child's stdin. Closes the pipe on Shutdown so the renderer
    // sees EOF and exits cleanly.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(chunk) = chunk_rx.recv() => {
                    let msg = HostMsg::EncodedChunk {
                        data: chunk.to_vec(),
                        pts_us: 0,
                    };
                    if let Err(e) = write_msg(&mut child_stdin, &msg).await {
                        warn!(error = %e, "mirror writer: chunk send failed");
                        return;
                    }
                }
                Some(msg) = host_rx.recv() => {
                    let is_shutdown = matches!(msg, HostMsg::Shutdown);
                    if let Err(e) = write_msg(&mut child_stdin, &msg).await {
                        warn!(error = %e, "mirror writer: control send failed");
                        return;
                    }
                    if is_shutdown {
                        return;
                    }
                }
                else => return,
            }
        }
    });

    // Reader task: pull RendererMsg::Input back, forward into the
    // per-peer input writer (the same channel D-Bus ShowScreen wired
    // up).
    {
        let input_tx = input_tx.clone();
        tokio::spawn(async move {
            loop {
                match read_msg::<_, RendererMsg>(&mut child_stdout).await {
                    Ok(Some(RendererMsg::Input(msg))) => {
                        if let Some(tx) = input_tx.as_ref() {
                            let _ = tx.send(msg);
                        }
                    }
                    Ok(None) => {
                        info!("mirror reader: clean EOF from renderer");
                        return;
                    }
                    Err(e) => {
                        warn!(error = %e, "mirror reader: stream errored");
                        return;
                    }
                }
            }
        });
    }

    let alive = Arc::new(AtomicBool::new(true));
    let (kill_tx, kill_rx) = oneshot::channel::<()>();

    // Wait task: races `child.wait()` against a hard-kill request.
    // Either way, flips `alive` off + fires the on_exit hook so the
    // action loop runs the rest of the teardown (companion stop +
    // state cleanup) and any future ShowScreen re-bootstraps cleanly.
    let alive_wait = alive.clone();
    tokio::spawn(async move {
        tokio::select! {
            res = child.wait() => match res {
                Ok(status) => info!(pid, ?status, "mirror renderer exited"),
                Err(e) => warn!(error = %e, "wait on mirror renderer failed"),
            },
            _ = kill_rx => {
                warn!(pid, "mirror renderer: hard kill requested");
                if let Err(e) = child.kill().await {
                    warn!(error = %e, pid, "child.kill() failed");
                }
                match child.wait().await {
                    Ok(status) => info!(pid, ?status, "mirror renderer killed"),
                    Err(e) => warn!(error = %e, "post-kill wait failed"),
                }
            }
        }
        alive_wait.store(false, Ordering::Relaxed);
        on_exit();
    });

    *entry.subprocess.lock().expect("subprocess slot poisoned") = Some(MirrorSubprocess {
        host_tx: Some(host_tx),
        pid,
        alive,
        kill_tx: Some(kill_tx),
    });

    Ok(())
}
