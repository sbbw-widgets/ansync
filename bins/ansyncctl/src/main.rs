//! `ansyncctl` — CLI front-end for the ansync daemon over D-Bus.

use std::collections::HashMap;
use std::path::PathBuf;

use ansync_crypto::IdentityKeypair;
use clap::{Parser, Subcommand};
use directories::BaseDirs;
use futures::StreamExt;
use tracing_subscriber::EnvFilter;
use zbus::zvariant::OwnedValue;

const IDENTITY_FILENAME: &str = "identity.key";
const DEVICE_IFACE: &str = "org.gameros.Ansync1.Device";
const PERMS_IFACE: &str = "org.gameros.Ansync1.Permissions";
const PAIR_IFACE: &str = "org.gameros.Ansync1.PairingSession";

#[derive(Debug, Parser)]
#[command(name = "ansyncctl", version, about = "ansync CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the long-term Ed25519 identity stored on disk.
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// List paired devices known to the daemon. Goes through
    /// `Manager.ListDevices` + the `Device` interface properties so the
    /// output reflects live state (capabilities, connectivity, address).
    Devices,
    /// Snapshot of paired devices currently observed on the LAN.
    /// Backed by `Manager.ReachableDevices`.
    Reachable,
    /// List ADB devices the daemon's local adbd sees
    /// (`Manager.ListAdbDevices`). Pure-Rust passthrough, no `adb`
    /// CLI shell-out.
    AdbDevices,
    /// Print the QUIC listen endpoints the daemon advertises
    /// (`Manager.ListenEndpoints`). Useful for direct-dial debugging.
    Endpoints,
    /// Ask the daemon to re-scan the peer store and register D-Bus
    /// paths for any newly persisted peer (`Manager.RefreshPeers`).
    Refresh,
    /// Browse the LAN for pair-ready companions via the daemon's mDNS
    /// browser (`Manager.BrowseAvailable`).
    Discover {
        /// How long the daemon listens for replies before returning.
        #[arg(long, default_value_t = 5)]
        seconds: u32,
    },
    /// Pair with an Android device. All transport selection lives in
    /// the daemon (`Manager.PairAuto` — ADB probe first, mDNS fallback).
    /// Use `--serial` to force cable or `--remote-addr` to force a
    /// specific Wi-Fi target. The user only ever types the 6-digit PIN
    /// on the Wi-Fi path; cable trusts the USB link.
    Pair {
        /// ADB serial. Forces the cable path even if a companion is
        /// also reachable over the LAN.
        #[arg(long)]
        serial: Option<String>,
        /// Explicit `ip:port` of the companion's pair listener.
        /// Skip mDNS browse — useful when multicast is blocked
        /// (corporate networks, AP isolation) and the user reads the
        /// IP off the device manually.
        #[arg(long, conflicts_with = "serial")]
        remote_addr: Option<String>,
        /// Path to the companion APK. Forwarded to the daemon for the
        /// cable path; the daemon falls back to `$ANSYNC_COMPANION_APK`,
        /// `/usr/share/ansync/companion.apk`, or a GitHub release fetch
        /// when this is unset.
        #[arg(long)]
        apk: Option<PathBuf>,
        /// How long the daemon browses mDNS for pair-ready companions
        /// before giving up. Ignored when `--serial` or `--remote-addr`
        /// is set.
        #[arg(long, default_value_t = 5)]
        discover_seconds: u32,
    },
    /// Forget a previously paired device (`Manager.ForgetDevice`).
    Forget { id: String },
    /// Open the mirror window for a device (`Device.ShowScreen`).
    Show { id: String },
    /// Close an open mirror window (`Device.HideScreen`).
    Hide { id: String },
    /// Start a camera capture session on the peer's lens
    /// (`Device.StartCamera`).
    CameraStart {
        id: String,
        /// Android `cameraId` string (`0` = back, `1` = front).
        #[arg(long, default_value = "0")]
        camera_id: String,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long, default_value_t = 60)]
        fps: u8,
        #[arg(long, default_value_t = 8000)]
        bitrate_kbps: u32,
        #[arg(long, default_value = "h264", value_parser = ["h264", "h265"])]
        codec: String,
        #[arg(long, default_value = "crop", value_parser = ["crop", "letterbox", "stretch"])]
        aspect: String,
        #[arg(long)]
        stabilization: bool,
    },
    /// Stop a running camera capture (`Device.StopCamera`).
    CameraStop { id: String },
    /// Share the peer's microphone to host audio
    /// (`Device.StartMicrophone`).
    MicStart { id: String },
    /// Stop microphone sharing (`Device.StopMicrophone`).
    MicStop { id: String },
    /// Start an audio route in either direction
    /// (`Device.StartAudioRoute`).
    AudioStart {
        id: String,
        #[arg(value_parser = ["host-to-device", "device-to-host", "both"])]
        direction: String,
    },
    /// Stop the active audio route (`Device.StopAudioRoute`).
    AudioStop { id: String },
    /// One-shot clipboard push host → peer (`Device.SyncClipboard`).
    ClipboardSync { id: String },
    /// Push one or more files to a paired device. Routes through the
    /// daemon's D-Bus `Device.SendFiles` — the daemon owns the
    /// outbound transfer (mirror window, accounting, signals).
    Push {
        id: String,
        /// One or more files. Paths are interpreted relative to the
        /// CWD of `ansyncctl`, then made absolute before the D-Bus
        /// call so the daemon sees the same bytes.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Ask a paired device to open `url`. Linux peers shell out to
    /// `xdg-open`; Android peers prompt before firing `ACTION_VIEW`.
    Url { id: String, url: String },
    /// Get or set a per-device permission flag (`Permissions.Get` /
    /// `Permissions.Set`).
    Perm {
        id: String,
        flag: String,
        value: Option<bool>,
    },
    /// Reset all per-device permissions to defaults
    /// (`Permissions.Reset`).
    PermReset { id: String },
    /// Subscribe to daemon signals and print them line-by-line until
    /// Ctrl-C. Covers `Manager.DeviceConnectivityChanged` /
    /// `DeviceReachable` / `DeviceUnreachable` and per-device
    /// `StreamStateChanged` / `FileReceived` / `FileTransferProgress` /
    /// `NotificationPosted` / `NotificationRemoved`.
    Monitor {
        /// Restrict to a single device id. Omit for all devices.
        #[arg(long)]
        id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum IdentityAction {
    /// Generate a new Ed25519 identity at the default path (will not
    /// overwrite an existing one).
    Init {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Print the device id derived from the persisted identity.
    Show {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();
    match cli.command {
        Command::Identity { action } => identity(action)?,
        Command::Devices => list_devices().await?,
        Command::Reachable => reachable().await?,
        Command::AdbDevices => adb_devices().await?,
        Command::Endpoints => endpoints().await?,
        Command::Refresh => refresh().await?,
        Command::Discover { seconds } => discover(seconds).await?,
        Command::Pair {
            serial,
            remote_addr,
            apk,
            discover_seconds,
        } => pair_dispatch(serial, remote_addr, apk, discover_seconds).await?,
        Command::Forget { id } => forget(id).await?,
        Command::Show { id } => show_screen(id).await?,
        Command::Hide { id } => hide_screen(id).await?,
        Command::CameraStart {
            id,
            camera_id,
            width,
            height,
            fps,
            bitrate_kbps,
            codec,
            aspect,
            stabilization,
        } => {
            camera_start(
                id,
                camera_id,
                width,
                height,
                fps,
                bitrate_kbps,
                codec,
                aspect,
                stabilization,
            )
            .await?
        }
        Command::CameraStop { id } => camera_stop(id).await?,
        Command::MicStart { id } => mic_start(id).await?,
        Command::MicStop { id } => mic_stop(id).await?,
        Command::AudioStart { id, direction } => audio_start(id, direction).await?,
        Command::AudioStop { id } => audio_stop(id).await?,
        Command::ClipboardSync { id } => clipboard_sync(id).await?,
        Command::Push { id, paths } => push(id, paths).await?,
        Command::Url { id, url } => send_url(id, url).await?,
        Command::Perm { id, flag, value } => perm(id, flag, value).await?,
        Command::PermReset { id } => perm_reset(id).await?,
        Command::Monitor { id } => monitor(id).await?,
    }

    Ok(())
}

fn data_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = BaseDirs::new().ok_or("$HOME not set; cannot resolve XDG paths")?;
    Ok(base.data_dir().join("ansync"))
}

fn default_identity_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(data_dir()?.join(IDENTITY_FILENAME))
}

fn identity(action: IdentityAction) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        IdentityAction::Init { path } => {
            let path = match path {
                Some(p) => p,
                None => default_identity_path()?,
            };
            if path.exists() {
                return Err(format!(
                    "identity already exists at {} — refusing to overwrite",
                    path.display()
                )
                .into());
            }
            let kp = IdentityKeypair::generate();
            kp.save(&path)?;
            println!("identity created at {}", path.display());
            println!("device id: {}", kp.device_id());
            Ok(())
        }
        IdentityAction::Show { path } => {
            let path = match path {
                Some(p) => p,
                None => default_identity_path()?,
            };
            let kp = IdentityKeypair::load(&path)?;
            println!("path: {}", path.display());
            println!("device id: {}", kp.device_id());
            Ok(())
        }
    }
}

async fn list_devices() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    let ids: Vec<String> = mgr.call("ListDevices", &()).await?;
    if ids.is_empty() {
        println!("(no paired devices — run `ansyncctl pair`)");
        return Ok(());
    }
    for id in ids {
        let props = device_props(&conn, &id).await?;
        let name = take_string(&props, "Name").unwrap_or_default();
        let state = take_string(&props, "State").unwrap_or_else(|| "unknown".into());
        let address = take_string(&props, "Address").unwrap_or_default();
        let caps = take_string_vec(&props, "Capabilities").unwrap_or_default();
        let battery = take_u8(&props, "BatteryLevel").unwrap_or(0);
        println!(
            "{id}  {name:<24}  state={state:<14}  battery={battery:>3}  addr={address:<22}  caps=[{caps}]",
            caps = caps.join(",")
        );
    }
    Ok(())
}

async fn reachable() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    let pairs: Vec<(String, String)> = mgr.call("ReachableDevices", &()).await?;
    if pairs.is_empty() {
        println!("(no reachable peers on the LAN right now)");
        return Ok(());
    }
    for (id, addr) in pairs {
        println!("{id}  {addr}");
    }
    Ok(())
}

async fn adb_devices() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    let pairs: Vec<(String, String)> = mgr.call("ListAdbDevices", &()).await?;
    if pairs.is_empty() {
        println!("(no authorized ADB devices visible to the daemon)");
        return Ok(());
    }
    for (serial, state) in pairs {
        println!("{serial}  {state}");
    }
    Ok(())
}

async fn endpoints() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    let pairs: Vec<(String, u16)> = mgr.call("ListenEndpoints", &()).await?;
    if pairs.is_empty() {
        println!("(daemon reports no listen endpoints)");
        return Ok(());
    }
    for (ip, port) in pairs {
        println!("{ip}:{port}");
    }
    Ok(())
}

async fn refresh() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    let added: Vec<String> = mgr.call("RefreshPeers", &()).await?;
    if added.is_empty() {
        println!("(daemon already had every peer registered)");
    } else {
        for id in added {
            println!("registered {id}");
        }
    }
    Ok(())
}

async fn discover(seconds: u32) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    println!("browsing for {seconds}s via daemon mDNS …");
    let found: Vec<(String, String, String)> =
        mgr.call("BrowseAvailable", &seconds).await?;
    if found.is_empty() {
        println!("(no pair-ready peers found)");
        return Ok(());
    }
    for (name, addr, pubkey) in found {
        println!("{name:<24}  {addr:<22}  {pubkey}");
    }
    Ok(())
}

async fn forget(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let mgr = manager_proxy(&conn).await?;
    mgr.call::<_, _, ()>("ForgetDevice", &id).await?;
    println!("forgot {id}");
    Ok(())
}

async fn show_screen(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("ShowScreen", &()).await?;
    println!("daemon opened mirror for {id}");
    Ok(())
}

async fn hide_screen(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("HideScreen", &()).await?;
    println!("daemon closed mirror for {id}");
    Ok(())
}

async fn perm(
    id: String,
    flag: String,
    value: Option<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = permissions_proxy(&id).await?;
    match value {
        Some(v) => {
            proxy.call::<_, _, ()>("Set", &(&flag, v)).await?;
            println!("{id}.{flag} = {v}");
        }
        None => {
            let current: bool = proxy.call("Get", &flag).await?;
            println!("{id}.{flag} = {current}");
        }
    }
    Ok(())
}

async fn perm_reset(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = permissions_proxy(&id).await?;
    proxy.call::<_, _, ()>("Reset", &()).await?;
    println!("permissions for {id} reset to defaults");
    Ok(())
}

async fn manager_proxy(
    conn: &zbus::Connection,
) -> Result<zbus::Proxy<'static>, Box<dyn std::error::Error>> {
    let proxy = zbus::Proxy::new(
        conn,
        ansync_dbus::SERVICE_NAME,
        ansync_dbus::PATH_MANAGER,
        "org.gameros.Ansync1.Manager",
    )
    .await?;
    Ok(proxy)
}

async fn permissions_proxy(
    id: &str,
) -> Result<zbus::Proxy<'static>, Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let path = format!("/org/gameros/Ansync1/Permissions/{id}");
    let proxy = zbus::Proxy::new(
        &conn,
        ansync_dbus::SERVICE_NAME,
        zbus::zvariant::ObjectPath::try_from(path)?.into_owned(),
        PERMS_IFACE,
    )
    .await?;
    Ok(proxy)
}

async fn device_props(
    conn: &zbus::Connection,
    id: &str,
) -> Result<HashMap<String, OwnedValue>, Box<dyn std::error::Error>> {
    let path = format!("/org/gameros/Ansync1/Device/{id}");
    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(ansync_dbus::SERVICE_NAME)?
        .path(zbus::zvariant::ObjectPath::try_from(path)?.into_owned())?
        .build()
        .await?;
    let dict = props.get_all(DEVICE_IFACE.try_into()?).await?;
    Ok(dict.into_iter().collect())
}

fn take_string(map: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.try_clone().ok()?.try_into().ok())
}

fn take_string_vec(map: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<String>> {
    map.get(key).and_then(|v| v.try_clone().ok()?.try_into().ok())
}

fn take_u8(map: &HashMap<String, OwnedValue>, key: &str) -> Option<u8> {
    map.get(key).and_then(|v| v.try_clone().ok()?.try_into().ok())
}

/// Dispatch every pair flavour through the daemon. The transport
/// decision (cable / Wi-Fi) lives in the daemon so the CLI stays a
/// thin client: it picks the right D-Bus entry point based on flags
/// and then drives the returned `PairingSession`.
///
///   * `--remote-addr` → `Manager.StartPairing(addr, "")`.
///   * `--serial` → `Manager.PairOverCable(serial, apk)`.
///   * Otherwise → `Manager.PairAuto(seconds, apk)` (daemon probes ADB
///     first, falls back to mDNS).
async fn pair_dispatch(
    serial: Option<String>,
    remote_addr: Option<String>,
    apk: Option<PathBuf>,
    discover_seconds: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await.map_err(|e| {
        format!("session bus: {e} — start ansyncd before running `pair`")
    })?;
    let mgr = manager_proxy(&conn).await?;
    let apk_arg = apk
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let session_path: zbus::zvariant::OwnedObjectPath = if let Some(addr) = remote_addr {
        mgr.call("StartPairing", &(addr, String::new())).await?
    } else if let Some(serial) = serial {
        mgr.call("PairOverCable", &(serial, apk_arg)).await?
    } else {
        mgr.call("PairAuto", &(discover_seconds, apk_arg)).await?
    };

    drive_pair_session(&conn, &session_path).await
}

/// Drive a `PairingSession` returned by the daemon. Subscribes to the
/// session's `Completed`/`Failed` signals and `PropertiesChanged` so
/// the PIN prompt only fires once the state hits `awaiting_pin`
/// (Wi-Fi). Cable sessions skip straight from `dialing` → `ok`.
async fn drive_pair_session(
    conn: &zbus::Connection,
    session_path: &zbus::zvariant::OwnedObjectPath,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;

    println!("pair session at {}", session_path.as_str());
    let session = zbus::Proxy::new(
        conn,
        ansync_dbus::SERVICE_NAME,
        session_path.as_ref(),
        PAIR_IFACE,
    )
    .await?;
    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(ansync_dbus::SERVICE_NAME)?
        .path(session_path.as_ref())?
        .build()
        .await?;

    let mut completed_signal = session.receive_signal("Completed").await?;
    let mut failed_signal = session.receive_signal("Failed").await?;
    let mut props_changed = props.receive_properties_changed().await?;

    let mut pin_submitted = false;
    let mut last_state = String::new();
    if read_session_state(&session).await.as_deref() == Some("awaiting_pin") {
        submit_pin_via_session(&session).await?;
        pin_submitted = true;
    }

    loop {
        tokio::select! {
            biased;
            Some(msg) = completed_signal.next() => {
                let body: (String, String) = msg.body().deserialize()?;
                println!("paired: device_id={} name={}", body.0, body.1);
                if let Err(e) = post_setup_notification(&body.1).await {
                    eprintln!("note: failed to post desktop notification: {e}");
                }
                return Ok(());
            }
            Some(msg) = failed_signal.next() => {
                let body: (String,) = msg.body().deserialize()?;
                return Err(format!("pair failed: {}", body.0).into());
            }
            Some(_) = props_changed.next() => {
                if let Some(state) = read_session_state(&session).await {
                    if state != last_state {
                        if let Some(label) = pretty_state(&state) {
                            println!("→ {label}");
                        }
                        last_state = state.clone();
                    }
                    if !pin_submitted && state == "awaiting_pin" {
                        submit_pin_via_session(&session).await?;
                        pin_submitted = true;
                    }
                }
            }
        }
    }
}

fn pretty_state(state: &str) -> Option<&'static str> {
    match state {
        "discovering" => Some("daemon scanning for transport…"),
        "dialing" => Some("dialing companion…"),
        "awaiting_pin" => Some("companion sent BootstrapAck"),
        "verifying" => Some("verifying PIN…"),
        "ok" => Some("done"),
        "failed" => Some("failed"),
        _ => None,
    }
}

/// Pop a freedesktop notification telling the user to walk through
/// the setup guide that just appeared on their phone. Survives daemon
/// restarts (lives in the user's notification daemon).
async fn post_setup_notification(
    device_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let summary = format!("Ansync paired with {device_name}");
    let body = "Finish setup on your phone. Pull down the shade and tap each \
                ansync step (notifications, microphone, accessibility, …). \
                The mirror window goes live once you're done.";
    let actions: Vec<&str> = vec![];
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let _id: u32 = proxy
        .call(
            "Notify",
            &(
                "ansync",
                0u32,
                "smartphone",
                summary.as_str(),
                body,
                actions,
                hints,
                15_000i32,
            ),
        )
        .await?;
    Ok(())
}

async fn read_session_state(session: &zbus::Proxy<'_>) -> Option<String> {
    session.get_property::<String>("State").await.ok()
}

async fn submit_pin_via_session(
    session: &zbus::Proxy<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let host_name = session
        .get_property::<String>("HostName")
        .await
        .unwrap_or_default();
    if !host_name.is_empty() {
        println!("device replied: `{host_name}`");
    }
    let pin = read_pin_interactive()?;
    let pin_str = std::str::from_utf8(&pin)?.to_string();
    session
        .call::<_, _, ()>("SubmitPin", &pin_str)
        .await
        .map_err(|e| format!("SubmitPin: {e}"))?;
    Ok(())
}

/// Interactive PIN prompt. PIN never comes from a CLI flag — keeping
/// it off the shell history means the only path a 6-digit secret can
/// be captured is from screen recorders / shoulder-surfers.
fn read_pin_interactive() -> Result<[u8; 6], Box<dyn std::error::Error>> {
    use std::io::{BufRead, Write};
    print!("enter the 6-digit PIN displayed on the device: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let digits: Vec<u8> = line
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8)
        .collect();
    if digits.len() != 6 {
        return Err(format!(
            "expected 6 digits, got {} after stripping non-digits",
            digits.len()
        )
        .into());
    }
    let mut out = [0u8; 6];
    out.copy_from_slice(&digits);
    Ok(out)
}


async fn push(id_hex: String, paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve each path to absolute so the daemon (running in its
    // own CWD / under a systemd user unit) reads the right bytes.
    let abs: Vec<String> = paths
        .into_iter()
        .map(|p| -> Result<String, Box<dyn std::error::Error>> {
            let absolute = if p.is_absolute() { p } else { std::env::current_dir()?.join(p) };
            Ok(absolute.display().to_string())
        })
        .collect::<Result<_, _>>()?;
    let proxy = device_proxy(&id_hex).await?;
    let count: u32 = proxy.call("SendFiles", &(&abs,)).await?;
    println!("daemon queued {count} files for {id_hex}");
    Ok(())
}

async fn send_url(id_hex: String, url: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id_hex).await?;
    proxy.call::<_, _, ()>("SendUrl", &(&url,)).await?;
    println!("daemon dispatched URL to {id_hex}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn camera_start(
    id: String,
    camera_id: String,
    width: u32,
    height: u32,
    fps: u8,
    bitrate_kbps: u32,
    codec: String,
    aspect: String,
    stabilization: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy
        .call::<_, _, ()>(
            "StartCamera",
            &(
                camera_id,
                width,
                height,
                fps,
                bitrate_kbps,
                codec,
                aspect,
                stabilization,
            ),
        )
        .await?;
    println!("camera start queued for {id}");
    Ok(())
}

async fn camera_stop(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("StopCamera", &()).await?;
    println!("camera stop queued for {id}");
    Ok(())
}

async fn mic_start(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("StartMicrophone", &()).await?;
    println!("microphone share started for {id}");
    Ok(())
}

async fn mic_stop(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("StopMicrophone", &()).await?;
    println!("microphone share stopped for {id}");
    Ok(())
}

async fn audio_start(
    id: String,
    direction: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy
        .call::<_, _, ()>("StartAudioRoute", &direction)
        .await?;
    println!("audio route ({direction}) started for {id}");
    Ok(())
}

async fn audio_stop(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("StopAudioRoute", &()).await?;
    println!("audio route stopped for {id}");
    Ok(())
}

async fn clipboard_sync(id: String) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = device_proxy(&id).await?;
    proxy.call::<_, _, ()>("SyncClipboard", &()).await?;
    println!("clipboard sync requested for {id}");
    Ok(())
}

/// Subscribe to every daemon signal we care about and print one line
/// per delivery. Two flavours of subscription:
///   * `Manager.{DeviceConnectivityChanged, DeviceReachable, DeviceUnreachable}`
///     — single emitter at `/org/gameros/Ansync1/Manager`.
///   * Per-device `Device.*` signals — uses an interface match without a
///     path constraint so every paired peer fans in. When `--id` is set
///     the match adds the device path so noise from other peers stays
///     filtered out.
async fn monitor(id: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    use zbus::MatchRule;

    let conn = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;

    let manager_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(ansync_dbus::SERVICE_NAME)?
        .interface("org.gameros.Ansync1.Manager")?
        .path(ansync_dbus::PATH_MANAGER)?
        .build();
    dbus.add_match_rule(manager_rule).await?;

    let mut device_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(ansync_dbus::SERVICE_NAME)?
        .interface(DEVICE_IFACE)?;
    let device_path_owned: zbus::zvariant::OwnedObjectPath;
    if let Some(ref id) = id {
        device_path_owned = zbus::zvariant::ObjectPath::try_from(format!(
            "/org/gameros/Ansync1/Device/{id}"
        ))?
        .into();
        device_rule = device_rule.path(&device_path_owned)?;
    }
    dbus.add_match_rule(device_rule.build()).await?;

    println!("watching daemon signals (Ctrl-C to exit)…");
    let mut stream = zbus::MessageStream::from(conn.clone());
    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                eprintln!("(stream error: {e})");
                continue;
            }
        };
        let hdr = msg.header();
        let Some(iface) = hdr.interface() else { continue };
        let Some(member) = hdr.member() else { continue };
        if iface.as_str() == "org.gameros.Ansync1.Manager" {
            print_manager_signal(&msg, member.as_str());
        } else if iface.as_str() == DEVICE_IFACE {
            let device_id = hdr
                .path()
                .and_then(|p| p.as_str().rsplit('/').next())
                .unwrap_or("?")
                .to_string();
            print_device_signal(&msg, &device_id, member.as_str());
        }
    }
    Ok(())
}

fn print_manager_signal(msg: &zbus::Message, member: &str) {
    match member {
        "DeviceConnectivityChanged" => {
            if let Ok((id, state)) = msg.body().deserialize::<(String, String)>() {
                println!("conn  {id}  {state}");
            }
        }
        "DeviceReachable" => {
            if let Ok((id, addr)) = msg.body().deserialize::<(String, String)>() {
                println!("up    {id}  {addr}");
            }
        }
        "DeviceUnreachable" => {
            if let Ok((id,)) = msg.body().deserialize::<(String,)>() {
                println!("down  {id}");
            }
        }
        _ => {}
    }
}

fn print_device_signal(msg: &zbus::Message, device: &str, member: &str) {
    match member {
        "StreamStateChanged" => {
            if let Ok((kind, active)) = msg.body().deserialize::<(String, bool)>() {
                println!("stream {device}  {kind}={active}");
            }
        }
        "FileReceived" => {
            if let Ok((path,)) = msg.body().deserialize::<(String,)>() {
                println!("file  {device}  {path}");
            }
        }
        "FileTransferProgress" => {
            if let Ok((batch_id, transfer_id, name, bytes, total, _bf, _bfd, _bbd, _btb, dir)) =
                msg.body()
                    .deserialize::<(u64, u64, String, u64, u64, u32, u32, u64, u64, String)>()
            {
                println!(
                    "xfer  {device}  {dir} {name} {bytes}/{total}  (batch={batch_id} id={transfer_id})"
                );
            }
        }
        "NotificationPosted" => {
            if let Ok((id, app, title, body)) =
                msg.body().deserialize::<(u64, String, String, String)>()
            {
                println!("notif {device}  [{id}] {app} · {title} — {body}");
            }
        }
        "NotificationRemoved" => {
            if let Ok((id,)) = msg.body().deserialize::<(u64,)>() {
                println!("notif {device}  [{id}] removed");
            }
        }
        _ => {}
    }
}

async fn device_proxy(
    id_hex: &str,
) -> Result<zbus::Proxy<'static>, Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let path = format!("/org/gameros/Ansync1/Device/{id_hex}");
    let proxy = zbus::Proxy::new(
        &conn,
        "org.gameros.Ansync1",
        zbus::zvariant::ObjectPath::try_from(path)?.into_owned(),
        DEVICE_IFACE,
    )
    .await?;
    Ok(proxy)
}

