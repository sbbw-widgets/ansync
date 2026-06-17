//! `ansyncctl` — CLI front-end for the ansync daemon over D-Bus.

use std::path::PathBuf;
use std::time::Duration;

use ansync_crypto::IdentityKeypair;
use ansync_discovery::{Discovery, MdnsDiscovery};
use ansync_files::send_file;
use ansync_pairing::{PeerStore, list_adb_devices, pair_host_via_adb};
use ansync_permissions::FilePermissionsStore;
use ansync_transport::{Connection as _, QuicTransport, StreamKind};
use clap::{Parser, Subcommand};
use directories::BaseDirs;
use futures::StreamExt;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

const IDENTITY_FILENAME: &str = "identity.key";
const PEERS_DIRNAME: &str = "peers";

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
    /// List paired devices known to the daemon.
    Devices,
    /// Browse the LAN for ansync peers advertising over mDNS.
    Discover {
        /// How long to listen for replies before printing the table.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Pair with an Android device. Tries cable / ADB first; if no
    /// device is attached, browses mDNS for pair-ready companions on
    /// the LAN and dials the only match (or prompts when several
    /// reply). The user only ever types the 6-digit PIN displayed on
    /// the device.
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
        /// Human-readable name advertised to the peer.
        #[arg(long)]
        name: Option<String>,
        /// Path to the companion APK. Cable path only — auto-installed
        /// if the device does not already have `org.gameros.ansync`.
        /// Defaults to `$ANSYNC_COMPANION_APK` env or
        /// `/usr/share/ansync/companion.apk`.
        #[arg(long)]
        apk: Option<PathBuf>,
        /// Skip prompt and install the latest release if the
        /// companion is outdated (cable only).
        #[arg(long)]
        auto_upgrade: bool,
        /// Skip the GitHub release check entirely (cable, offline mode).
        #[arg(long)]
        skip_upgrade_check: bool,
        /// How long to browse mDNS for pair-ready companions before
        /// giving up. Ignored when `--serial` or `--remote-addr` is set.
        #[arg(long, default_value_t = 5)]
        discover_seconds: u64,
    },
    /// Forget a previously paired device.
    Forget { id: String },
    /// Open the mirror screen for a device.
    Show { id: String },
    /// Push a file to a device (direct QUIC dial — bypasses daemon
    /// D-Bus and discovers the peer's address via mDNS).
    Push {
        id: String,
        path: PathBuf,
        /// Skip mDNS browse and connect to `host:port` directly.
        #[arg(long)]
        addr: Option<String>,
        /// mDNS browse timeout if `--addr` is not supplied.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Mount the remote filesystem.
    Mount { id: String, mountpoint: String },
    /// Unmount the remote filesystem.
    Unmount { id: String },
    /// Get or set a per-device permission flag.
    Perm {
        id: String,
        flag: String,
        value: Option<bool>,
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
        Command::Devices => list_devices()?,
        Command::Discover { seconds } => discover(seconds).await?,
        Command::Pair {
            serial,
            remote_addr,
            name,
            apk,
            auto_upgrade,
            skip_upgrade_check,
            discover_seconds,
        } => {
            pair_dispatch(
                serial,
                remote_addr,
                name,
                apk,
                auto_upgrade,
                skip_upgrade_check,
                discover_seconds,
            )
            .await?;
        }
        Command::Forget { id } => println!("(skeleton) forget {id}"),
        Command::Show { id } => println!("(skeleton) show {id}"),
        Command::Push { id, path, addr, seconds } => push(id, path, addr, seconds).await?,
        Command::Mount { id, mountpoint } => println!("(skeleton) mount {id} at {mountpoint}"),
        Command::Unmount { id } => println!("(skeleton) unmount {id}"),
        Command::Perm { id, flag, value } => println!("(skeleton) perm {id} {flag} {value:?}"),
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

fn default_peers_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(data_dir()?.join(PEERS_DIRNAME))
}

fn load_identity() -> Result<IdentityKeypair, Box<dyn std::error::Error>> {
    let path = default_identity_path()?;
    Ok(IdentityKeypair::load_or_generate(&path)?)
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

fn list_devices() -> Result<(), Box<dyn std::error::Error>> {
    let store = PeerStore::open(default_peers_dir()?)?;
    let peers = store.list()?;
    if peers.is_empty() {
        println!("(no paired devices — run `ansyncctl pair`)");
        return Ok(());
    }
    for peer in peers {
        println!(
            "{id}  {name:<24}  caps={caps:#010x}  paired_at={paired_at}",
            id = peer.id,
            name = peer.name,
            caps = peer.capabilities.bits(),
            paired_at = peer.paired_at,
        );
    }
    Ok(())
}

async fn discover(seconds: u64) -> Result<(), Box<dyn std::error::Error>> {
    let identity = load_identity()?;
    let mdns = MdnsDiscovery::new(identity.public().as_bytes())?;
    let mut stream = mdns.browse()?;

    let deadline = Duration::from_secs(seconds);
    println!("browsing for {seconds}s …");
    let mut seen = std::collections::HashMap::new();
    let _ = timeout(deadline, async {
        while let Some(dev) = stream.next().await {
            seen.insert(dev.id.clone(), dev);
        }
    })
    .await;

    if seen.is_empty() {
        println!("(no peers found)");
        return Ok(());
    }
    for dev in seen.values() {
        println!(
            "{id}  {name:<24}  {addr}  caps={caps:#010x}",
            id = dev.id,
            name = dev.name,
            addr = dev.addr,
            caps = dev.capabilities.bits(),
        );
    }
    Ok(())
}

async fn pair(
    serial: Option<String>,
    name: Option<String>,
    apk: Option<PathBuf>,
    auto_upgrade: bool,
    skip_upgrade_check: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = load_identity()?;
    let local_name = name
        .or_else(hostname)
        .unwrap_or_else(|| "ansync-host".to_string());

    let serial = match serial {
        Some(s) => s,
        None => {
            let devices = list_adb_devices().await?;
            if devices.is_empty() {
                return Err(
                    "no ADB devices found — connect a device with USB debugging enabled".into(),
                );
            }
            if devices.len() > 1 {
                eprintln!("multiple devices attached — pass --serial:");
                for d in &devices {
                    eprintln!("  {}  ({})", d.serial, d.state);
                }
                return Err("--serial is required when multiple devices are attached".into());
            }
            devices.into_iter().next().expect("len == 1").serial
        }
    };

    let apk_path = apk
        .or_else(|| std::env::var_os("ANSYNC_COMPANION_APK").map(PathBuf::from))
        .or_else(|| {
            let candidate = PathBuf::from("/usr/share/ansync/companion.apk");
            candidate.exists().then_some(candidate)
        });

    // Companion version policy: the host always expects the APK
    // matching its own `ansync_pairing::expected_version()` (CI feeds
    // the git tag via `ANSYNC_RELEASE_VERSION`; dev falls back to
    // `CARGO_PKG_VERSION`). Three cases:
    //   - explicit --apk / env / /usr/share override → use as-is.
    //   - installed `versionName` already matches → skip install
    //     entirely; the pair broadcast re-wakes the service either
    //     way, which is the closest thing to a restart we need.
    //   - missing / mismatched → fetch the matching tag and install.
    //
    // `--skip-upgrade-check` still bypasses the net call (offline
    // pair). `--auto-upgrade` is a no-op now — kept for backwards CLI
    // compat but matched-version logic supersedes the old prompt.
    let _ = auto_upgrade;
    let expected = ansync_pairing::expected_version_bare();
    let installed_version = ansync_pairing::query_installed_version(
        &serial,
        ansync_pairing::COMPANION_PACKAGE,
    )
    .await
    .unwrap_or(None);
    let version_matches = installed_version
        .as_deref()
        .map(|v| v.trim().eq_ignore_ascii_case(expected))
        .unwrap_or(false);

    let resolved_apk = if apk_path.is_some() {
        apk_path
    } else if version_matches {
        println!("companion {expected} already installed; restarting service via pair broadcast");
        None
    } else if skip_upgrade_check {
        if installed_version.is_some() {
            println!(
                "companion {} installed (expected {expected}); --skip-upgrade-check honoured, keeping it",
                installed_version.as_deref().unwrap_or("?")
            );
            None
        } else {
            return Err(format!(
                "companion not installed on {serial} and --skip-upgrade-check forbids fetching {expected}"
            )
            .into());
        }
    } else {
        match ansync_pairing::fetch_companion(expected).await {
            Ok(fetched) => {
                let old = installed_version.as_deref().unwrap_or("<not installed>");
                println!(
                    "installing companion {} (was {old}) → {}",
                    fetched.tag,
                    fetched.path.display()
                );
                Some(fetched.path)
            }
            Err(e) => {
                eprintln!(
                    "warning: APK fetch for {expected} failed ({e}); install will fail unless --apk is supplied"
                );
                None
            }
        }
    };

    let lan_endpoints = query_listen_endpoints().await.unwrap_or_else(|e| {
        eprintln!(
            "warning: ListenEndpoints query failed ({e}); companion will rely on mDNS only"
        );
        Vec::new()
    });
    println!("pairing with {serial} as `{local_name}` …");
    let stored = pair_host_via_adb(
        &serial,
        &identity,
        &local_name,
        resolved_apk.as_deref(),
        lan_endpoints,
    )
    .await?;
    println!("paired: device_id={} name={}", stored.id, stored.name);

    let store = PeerStore::open(default_peers_dir()?)?;
    store.put(&stored)?;
    println!(
        "persisted to {}/{}.toml",
        store.root().display(),
        stored.id
    );
    // Nudge the running daemon (if any) to wire the D-Bus Device +
    // Permissions object paths for the freshly paired peer. No-op if
    // the daemon is not running.
    if let Err(e) = notify_daemon_refresh().await {
        eprintln!("note: daemon not running or unreachable: {e}");
    }
    if let Err(e) = post_setup_notification(&stored.name.0).await {
        // Non-fatal: pair already persisted. Just log so the user
        // knows why no desktop notif popped.
        eprintln!("note: failed to post desktop notification: {e}");
    }
    Ok(())
}

/// Pop a freedesktop notification telling the user to walk through
/// the setup guide that just appeared on their phone. Better UX than
/// dumping a wall of `println!`s on stdout — the notif persists in
/// the user's notification daemon (DMS / dunst / mako) until they
/// finish the steps.
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


/// Top-level pair dispatch. Resolves the channel (cable vs WiFi) from
/// flags + environment with this priority:
///
///   1. `--remote-addr` set → WiFi against the supplied `ip:port`.
///   2. `--serial` set OR at least one device in `adb devices` → cable.
///   3. mDNS browse for `_ansync-pair._tcp` finds 1 companion → WiFi.
///   4. mDNS browse finds N > 1 → interactive prompt → WiFi.
///   5. Nothing found → error.
///
/// WiFi paths require a running daemon: pair always goes through
/// `Manager.StartPairing` + `PairingSession` so the widget and the CLI
/// observe the same session lifecycle. No daemon → no pair (avoids
/// PeerStore writes that the daemon's `companion_watcher` would never
/// see).
async fn pair_dispatch(
    serial: Option<String>,
    remote_addr: Option<String>,
    name: Option<String>,
    apk: Option<PathBuf>,
    auto_upgrade: bool,
    skip_upgrade_check: bool,
    discover_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(addr) = remote_addr {
        return pair_wifi_via_dbus(&addr, None).await;
    }
    if serial.is_some() {
        return pair(serial, name, apk, auto_upgrade, skip_upgrade_check).await;
    }
    let adb_devices = ansync_pairing::list_adb_devices().await.unwrap_or_default();
    if !adb_devices.is_empty() {
        return pair(serial, name, apk, auto_upgrade, skip_upgrade_check).await;
    }
    println!(
        "no ADB devices; asking daemon to browse for pair-ready companions ({}s) …",
        discover_seconds
    );
    let candidates = dbus_browse_available(discover_seconds as u32).await?;
    let picked = match candidates.len() {
        0 => {
            return Err(
                "no companions on the LAN. Make sure the device is on the same Wi-Fi or run `ansyncctl pair --remote-addr <ip:port>` to bypass mDNS.".into(),
            );
        }
        1 => candidates.into_iter().next().expect("len==1"),
        _ => pick_candidate(candidates)?,
    };
    println!(
        "found `{}` at {} (pubkey={}…)",
        picked.name,
        picked.addr,
        hex_short(&picked.pubkey)
    );
    pair_wifi_via_dbus(&picked.addr.to_string(), Some(picked.pubkey)).await
}

async fn dbus_browse_available(
    seconds: u32,
) -> Result<Vec<ansync_pairing::PairCandidate>, Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await.map_err(|e| {
        format!("session bus: {e} — start ansyncd before pairing over WiFi")
    })?;
    let proxy = zbus::Proxy::new(
        &conn,
        ansync_dbus::SERVICE_NAME,
        ansync_dbus::PATH_MANAGER,
        "org.gameros.Ansync1.Manager",
    )
    .await
    .map_err(|e| {
        format!("manager proxy: {e} — start ansyncd before pairing over WiFi")
    })?;
    let raw: Vec<(String, String, String)> = proxy
        .call("BrowseAvailable", &seconds)
        .await
        .map_err(|e| format!("BrowseAvailable: {e}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for (name, addr, pubkey_hex) in raw {
        let addr: std::net::SocketAddr = match addr.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let bytes = match hex::decode(&pubkey_hex) {
            Ok(b) if b.len() == 32 => b,
            _ => continue,
        };
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(&bytes);
        out.push(ansync_pairing::PairCandidate { addr, pubkey, name });
    }
    Ok(out)
}

/// Drive the pair through `Manager.StartPairing` + the returned
/// `PairingSession` interface. Listens to `PropertiesChanged` to know
/// when to prompt for the PIN, and to `Completed`/`Failed` to know
/// when the worker terminates. Daemon-mandatory: a missing daemon
/// produces a hard error rather than silently writing to the local
/// PeerStore behind its back.
async fn pair_wifi_via_dbus(
    addr: &str,
    expected_pubkey: Option<[u8; 32]>,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::StreamExt;

    let conn = zbus::Connection::session().await.map_err(|e| {
        format!("session bus: {e} — start ansyncd before pairing over WiFi")
    })?;
    let mgr = zbus::Proxy::new(
        &conn,
        ansync_dbus::SERVICE_NAME,
        ansync_dbus::PATH_MANAGER,
        "org.gameros.Ansync1.Manager",
    )
    .await
    .map_err(|e| {
        format!("manager proxy: {e} — start ansyncd before pairing over WiFi")
    })?;

    let pubkey_hex = expected_pubkey.map(hex::encode).unwrap_or_default();
    let session_path: zbus::zvariant::OwnedObjectPath = mgr
        .call("StartPairing", &(addr.to_string(), pubkey_hex))
        .await
        .map_err(|e| format!("StartPairing: {e}"))?;

    println!("pair session at {}", session_path.as_str());

    let session = zbus::Proxy::new(
        &conn,
        ansync_dbus::SERVICE_NAME,
        session_path.as_ref(),
        "org.gameros.Ansync1.PairingSession",
    )
    .await?;

    let props = zbus::fdo::PropertiesProxy::builder(&conn)
        .destination(ansync_dbus::SERVICE_NAME)?
        .path(session_path.as_ref())?
        .build()
        .await?;

    let mut completed_signal = session.receive_signal("Completed").await?;
    let mut failed_signal = session.receive_signal("Failed").await?;
    let mut props_changed = props.receive_properties_changed().await?;

    let mut pin_submitted = false;
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
                if pin_submitted { continue; }
                let state = read_session_state(&session).await;
                if state.as_deref() == Some("awaiting_pin") {
                    submit_pin_via_session(&session).await?;
                    pin_submitted = true;
                }
            }
        }
    }
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

fn pick_candidate(
    candidates: Vec<ansync_pairing::PairCandidate>,
) -> Result<ansync_pairing::PairCandidate, Box<dyn std::error::Error>> {
    use std::io::{BufRead, Write};
    println!("multiple pair-ready devices on the LAN:");
    for (i, c) in candidates.iter().enumerate() {
        println!(
            "  [{i}] {} — {} (pubkey={}…)",
            c.name,
            c.addr,
            hex_short(&c.pubkey)
        );
    }
    print!("pick (0-{}) > ", candidates.len() - 1);
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let idx: usize = line
        .trim()
        .parse()
        .map_err(|e| format!("invalid choice: {e}"))?;
    candidates
        .into_iter()
        .nth(idx)
        .ok_or_else(|| "index out of range".into())
}

fn hex_short(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(16);
    for b in &bytes[..8] {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Ask the running daemon for its LAN endpoints so we can embed them
/// in the cable bootstrap reply. Used as a direct-dial fallback by
/// the companion when mDNS multicast doesn't reach (Wi-Fi AP
/// isolation, captive portals, etc.).
async fn query_listen_endpoints()
    -> Result<Vec<(String, u16)>, Box<dyn std::error::Error>>
{
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        ansync_dbus::SERVICE_NAME,
        ansync_dbus::PATH_MANAGER,
        "org.gameros.Ansync1.Manager",
    )
    .await?;
    let endpoints: Vec<(String, u16)> = proxy.call("ListenEndpoints", &()).await?;
    Ok(endpoints)
}

async fn notify_daemon_refresh() -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        ansync_dbus::SERVICE_NAME,
        ansync_dbus::PATH_MANAGER,
        "org.gameros.Ansync1.Manager",
    )
    .await?;
    let added: Vec<String> = proxy.call("RefreshPeers", &()).await?;
    if !added.is_empty() {
        println!("daemon registered new device paths: {added:?}");
    }
    Ok(())
}

async fn push(
    id_hex: String,
    path: PathBuf,
    addr_override: Option<String>,
    discover_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = PeerStore::open(default_peers_dir()?)?;
    let peer = store
        .list()?
        .into_iter()
        .find(|p| p.id.to_string() == id_hex)
        .ok_or_else(|| format!("no paired peer with id={id_hex}"))?;

    let addr: std::net::SocketAddr = match addr_override {
        Some(s) => s.parse()?,
        None => {
            println!("browsing mDNS for {seconds}s …", seconds = discover_seconds);
            let identity = load_identity()?;
            let mdns = MdnsDiscovery::new(identity.public().as_bytes())?;
            let mut stream = mdns.browse()?;
            let deadline = Duration::from_secs(discover_seconds);
            let mut found = None;
            let _ = timeout(deadline, async {
                while let Some(dev) = stream.next().await {
                    if dev.id.to_string() == id_hex {
                        found = Some(dev);
                        break;
                    }
                }
            })
            .await;
            let dev = found.ok_or_else(|| {
                format!("peer {id_hex} not found via mDNS within {discover_seconds}s")
            })?;
            dev.addr
        }
    };
    println!("connecting to {addr} …");

    let identity = load_identity()?;
    let transport = QuicTransport::new(identity);
    let conn = transport.connect(addr, peer.pubkey).await?;
    let mut stream = conn.open(StreamKind::Files).await?;

    let permissions = FilePermissionsStore::open(default_permissions_dir()?)?;
    let transfer_id = rand_u64();
    let final_id = send_file(&peer.id, &permissions, &mut stream, &path, transfer_id).await?;
    println!("transfer {final_id} sent ok");
    Ok(())
}

fn default_permissions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = BaseDirs::new().ok_or("$HOME not set; cannot resolve XDG paths")?;
    Ok(base.config_dir().join("ansync").join("devices"))
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // CLI-side transfer ids do not need cryptographic randomness; a
    // monotonic-ish stamp avoids collisions between back-to-back
    // pushes from the same shell.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty())
}
