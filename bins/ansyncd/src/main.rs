//! `ansyncd` — the ansync daemon.
//!
//! Hosts the D-Bus surface, the QUIC transport, and the screen mirror GUI
//! window (eframe + wgpu) when a client invokes `ShowScreen`.
//!
//! The `dev-playback` feature additionally exposes a `--play-file
//! PATH` flag that drives the decoder from a local Annex-B recording.
//! This is a Step-6 test affordance only; release builds must be
//! compiled without the feature.

use std::path::PathBuf;
use std::sync::Arc;

use ansync_daemon_core::{Daemon, DaemonConfig};
use ansyncd::mirror_renderer;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature = "dev-playback")]
use ansyncd::mirror_window;

#[derive(Debug, Parser)]
#[command(name = "ansyncd", version, about = "ansync daemon")]
struct Args {
    /// Override the device name advertised on the LAN.
    #[arg(long)]
    device_name: Option<String>,
    /// Override the identity key path.
    #[arg(long)]
    identity: Option<PathBuf>,
    /// Override the peers directory. Per-device permissions live in
    /// the same toml — there is no separate `--permissions-dir` flag.
    #[arg(long)]
    peers_dir: Option<PathBuf>,
    /// Bring up the mirror window and feed it from a local Annex-B
    /// recording (`.h264` / `.h265`). Dev-only — only present when
    /// compiled with `--features dev-playback`.
    #[cfg(feature = "dev-playback")]
    #[arg(long, value_name = "PATH")]
    play_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<SubCmd>,
}

#[derive(Debug, Subcommand)]
enum SubCmd {
    /// Per-window mirror renderer subprocess. Spawned by the daemon —
    /// not meant to be invoked directly. Reads `HostMsg` frames from
    /// stdin, writes `RendererMsg` frames to stdout.
    MirrorRenderer,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Detect the mirror-renderer subcommand before touching clap so we
    // can route tracing to stderr — the subprocess's stdout is the
    // postcard IPC pipe shared with the daemon, and any fmt::layer
    // write to stdout corrupts the wire (daemon reads timestamp bytes
    // as the length prefix and dies with "frame too large").
    let is_renderer = std::env::args().nth(1).as_deref() == Some("mirror-renderer");
    install_logging(is_renderer)?;
    let args = Args::parse();

    if let Some(SubCmd::MirrorRenderer) = &args.cmd {
        return mirror_renderer::run();
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    #[cfg(feature = "dev-playback")]
    if let Some(path) = args.play_file.clone() {
        return run_play_file(runtime, path);
    }

    runtime.block_on(run_daemon(args))
}

#[cfg(feature = "dev-playback")]
fn run_play_file(
    runtime: tokio::runtime::Runtime,
    path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let shared = ansync_video::sink_egui::new_slot();
    mirror_window::spawn_play_file(&runtime, path, shared.clone());
    let result = ansync_video::sink_egui::run("ansync mirror".into(), shared, None);
    drop(runtime);
    result
}

async fn run_daemon(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let device_name = args
        .device_name
        .or_else(hostname)
        .unwrap_or_else(|| "ansync-host".to_string());

    let mut config = DaemonConfig::new(device_name);
    config.identity_path = args.identity;
    config.peers_dir = args.peers_dir;

    let daemon = Arc::new(Daemon::new(config));
    daemon.run().await?;
    Ok(())
}

fn install_logging(to_stderr: bool) -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_journald::layer().ok());
    if to_stderr {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_writer(std::io::stderr),
            )
            .try_init()?;
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_target(true))
            .try_init()?;
    }
    // wgpu / naga / winit emit through `log::*`; bridge so the same
    // EnvFilter applies. Without this any wgpu validation error is
    // silently discarded — exactly the kind of thing that shows up
    // as "the mirror window stays blank but no error is logged".
    // Errors are non-fatal: if a global tracing dispatcher predated
    // us (test harness) it just returns AlreadyInUse and we move on.
    let _ = tracing_log::LogTracer::init();
    Ok(())
}

/// Best-effort host name. Tries `gethostname(2)` first; falls back to
/// `$HOSTNAME` for hermetic test envs that disable the syscall.
fn hostname() -> Option<String> {
    let mut buf = [0i8; 256];
    // SAFETY: passing a stack buffer + its size; gethostname null-terminates
    // when the name fits, returns -1 + ENAMETOOLONG on overflow.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
    if rc == 0 {
        let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        if let Ok(s) = cstr.to_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty())
}
