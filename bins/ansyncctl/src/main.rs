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
    /// Pair with an Android device. Cable / ADB by default.
    Pair {
        /// ADB serial of the device. If omitted and exactly one device
        /// is attached, that device is used; otherwise the command fails.
        #[arg(long)]
        serial: Option<String>,
        /// Human-readable name advertised to the peer.
        #[arg(long)]
        name: Option<String>,
        /// Path to the companion APK. Auto-installed if the device
        /// does not already have `org.gameros.ansync`. Defaults to
        /// `$ANSYNC_COMPANION_APK` env or
        /// `/usr/share/ansync/companion.apk`.
        #[arg(long)]
        apk: Option<PathBuf>,
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
        Command::Pair { serial, name, apk } => pair(serial, name, apk).await?,
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

    // Step 17: if no explicit APK was supplied and the companion is
    // missing, fall through to the GitHub release fetcher. The cache
    // path is reused on subsequent runs, so re-pairing the same
    // device after a clean shell is free.
    let mut auto_apk: Option<PathBuf> = None;
    let resolved_apk = if apk_path.is_some() {
        apk_path
    } else if !ansync_pairing::companion_installed(&serial).await? {
        match ansync_pairing::fetch_latest_companion().await {
            Ok(fetched) => {
                println!(
                    "fetched companion APK {} → {}",
                    fetched.tag,
                    fetched.path.display()
                );
                auto_apk = Some(fetched.path.clone());
                Some(fetched.path)
            }
            Err(e) => {
                eprintln!(
                    "warning: auto-fetch failed ({e}); install will fail unless --apk is supplied"
                );
                None
            }
        }
    } else {
        None
    };
    let _ = auto_apk;

    println!("pairing with {serial} as `{local_name}` …");
    let stored = pair_host_via_adb(&serial, &identity, &local_name, resolved_apk.as_deref()).await?;
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
    Ok(())
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
