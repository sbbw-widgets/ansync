//! Daemon-side glue for the per-window mirror renderer subprocess.
//!
//! Each ShowScreen spawns one `ansyncd mirror-renderer --sock PATH`
//! child. We open a `UnixListener` ahead of time, hand the path to
//! the child, and accept its connection. From there:
//!
//!   - Daemon → child: `HostMsg::Config` once, then `EncodedChunk`
//!     for every NAL access unit `video_stream_loop` forwards.
//!   - Child → daemon: `RendererMsg::Input` — every event becomes a
//!     postcard `InputMessage` on the per-peer input writer.
//!
//! The child owns the eframe / wgpu / winit stack. That gives every
//! window its own `EventLoop::build` (winit's once-per-process guard
//! never trips) and a real OS-level close button — closing the
//! window kills the process, which `wait_child` notices and reports
//! back to the action loop so it can run the same teardown as a
//! D-Bus HideScreen.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use ansync_proto::InputMessage;
use ansync_video::ipc::{HostMsg, RendererMsg, WireCodec, read_msg, write_msg};
use tokio::net::UnixListener;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tracing::{info, warn};

use crate::{MirrorEntry, MirrorSubprocess};

/// Bring a renderer subprocess up for `entry` and wire the IPC
/// bridge. Returns once the child has accepted the socket and the
/// daemon has installed the chunk-forwarder, so `video_stream_loop`
/// can immediately start pushing.
pub async fn spawn_mirror_subprocess(
    title: String,
    entry: Arc<MirrorEntry>,
    input_tx: Option<UnboundedSender<InputMessage>>,
    on_exit: impl FnOnce() + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sock_path = unique_sock_path()?;
    if sock_path.exists() {
        // Stale socket from a previous run sharing the same PID — bind
        // would EADDRINUSE otherwise.
        let _ = std::fs::remove_file(&sock_path);
    }
    let listener = UnixListener::bind(&sock_path)?;

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("mirror-renderer")
        .arg("--sock")
        .arg(&sock_path)
        // Inherit stdio so the renderer's logs land in the same
        // journal entry as the daemon's. Renderer never reads stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let child = cmd.spawn()?;
    let child_pid = child.id();
    info!(
        pid = child_pid,
        sock = %sock_path.display(),
        "mirror renderer subprocess launched"
    );

    // Wait for the child to connect. Give it a generous window so a
    // slow eframe init (wgpu device enumeration on Wayland can take a
    // beat) doesn't time us out.
    let stream = tokio::time::timeout(std::time::Duration::from_secs(8), listener.accept())
        .await
        .map_err(|_| "mirror renderer connect timed out")?
        .map_err(|e| format!("accept failed: {e}"))?
        .0;
    let (mut read_half, mut write_half) = stream.into_split();

    // Send the initial Config so the renderer can size its decoder
    // and window before any frames arrive. We don't know the codec
    // yet (depends on the negotiated stream — H.264 today, H.265
    // when both ends advertise it), so hardcode H.264 to match
    // `video_stream_loop`'s decoder config.
    write_msg(
        &mut write_half,
        &HostMsg::Config {
            codec: WireCodec::H264,
            width: 1920,
            height: 1080,
            title,
        },
    )
    .await?;

    // Channel the action loop / `video_stream_loop` uses to push
    // encoded NAL chunks at the renderer. Writer task drains it.
    let (chunk_tx, mut chunk_rx) = unbounded_channel::<bytes::Bytes>();
    // Channel for any further HostMsg (Shutdown). Goes through the
    // same writer to keep the postcard frames serialised.
    let (host_tx, mut host_rx) = unbounded_channel::<HostMsg>();

    *entry.video_tx.lock().expect("video_tx slot poisoned") = Some(chunk_tx);

    // Writer task: serialise chunks + HostMsg onto the socket.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(chunk) = chunk_rx.recv() => {
                    let msg = HostMsg::EncodedChunk {
                        data: chunk.to_vec(),
                        pts_us: 0,
                    };
                    if let Err(e) = write_msg(&mut write_half, &msg).await {
                        warn!(error = %e, "mirror writer: chunk send failed");
                        return;
                    }
                }
                Some(msg) = host_rx.recv() => {
                    let is_shutdown = matches!(msg, HostMsg::Shutdown);
                    if let Err(e) = write_msg(&mut write_half, &msg).await {
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

    // Reader task: pull RendererMsg::Input back, forward to the
    // per-peer input writer.
    {
        let input_tx = input_tx.clone();
        tokio::spawn(async move {
            loop {
                match read_msg::<_, RendererMsg>(&mut read_half).await {
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

    // Reap the child + signal action_loop when it exits. Using
    // `std::process::Child::wait` in a blocking thread keeps the
    // tokio runtime free; the on_exit hook is just a one-shot enum
    // send back into the action loop.
    let sock_for_cleanup = sock_path.clone();
    let mut child_handle = child;
    std::thread::Builder::new()
        .name("ansync-mirror-wait".into())
        .spawn(move || {
            match child_handle.wait() {
                Ok(status) => info!(pid = child_pid, ?status, "mirror renderer exited"),
                Err(e) => warn!(error = %e, "wait on mirror renderer failed"),
            }
            let _ = std::fs::remove_file(&sock_for_cleanup);
            on_exit();
        })?;

    // Stash the handle so HideScreen can ask the renderer to exit
    // politely. We don't keep the original `std::process::Child`
    // around because `wait` consumed it on the dedicated thread; the
    // host_tx is enough to drive a clean shutdown, and SIGKILL via
    // `pidfd` / `kill` would be the only fallback if the renderer
    // hung. For now `host_tx.send(Shutdown)` is sufficient.
    *entry.subprocess.lock().expect("subprocess slot poisoned") = Some(MirrorSubprocess {
        host_tx: Some(host_tx),
        sock_path,
        // Placeholder child so the struct keeps its existing shape.
        // The real one is consumed by the wait thread above.
        child: dummy_child(),
    });

    Ok(())
}

/// Build a unique Unix socket path under `$XDG_RUNTIME_DIR/ansync/`
/// for this peer's mirror window. Falls back to `/tmp` when the
/// runtime dir env var is missing.
fn unique_sock_path() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = base.join("ansync");
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "mirror-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    Ok(dir.join(name))
}

/// Produce a `Child` placeholder. The real child was already wait()-ed
/// on the dedicated reaper thread; we only need a typesafe slot to
/// keep `MirrorSubprocess` shape. Spawning `/bin/true` is the
/// cheapest portable way to materialise one.
fn dummy_child() -> std::process::Child {
    Command::new("/bin/true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("/bin/true unavailable")
}
