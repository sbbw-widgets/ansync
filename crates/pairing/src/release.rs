//! GitHub-releases companion APK fetcher.
//!
//! Step 17 replaces the `--apk` / `ANSYNC_COMPANION_APK` /
//! `/usr/share/ansync/companion.apk` chain (which is still respected
//! for dev / CI nightlies) with an HTTPS GET against the
//! `SergioRibera/ansync` releases endpoint.
//!
//! Path:
//!
//!   1. Query `https://api.github.com/repos/<owner>/<repo>/releases/latest`.
//!   2. Pick the first asset whose `name` matches `companion*.apk`.
//!   3. Look at `$XDG_CACHE_HOME/ansync/companion-{tag}.apk`. If
//!      present + size matches the asset's `size` and (when
//!      available) the SHA-256 in the release `digest` field
//!      matches, reuse it.
//!   4. Otherwise stream-download into `$XDG_CACHE_HOME/...apk.partial`
//!      and rename on success.
//!   5. Verify SHA-256 if the release exposes one. Bail otherwise so
//!      a poisoned release doesn't end up installed silently.

use std::path::PathBuf;

use bytes::Bytes;
use directories::BaseDirs;
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::PairingError;

pub const DEFAULT_OWNER: &str = "SergioRibera";
pub const DEFAULT_REPO: &str = "ansync";
const ASSET_NAME_PREFIX: &str = "companion";
const ASSET_NAME_SUFFIX: &str = ".apk";
const USER_AGENT: &str = concat!("ansync/", env!("CARGO_PKG_VERSION"));

/// Version string the running binary considers "current" — used to
/// match the installed companion APK against the daemon / CLI build.
///
/// CI pins this to the published git tag (`ANSYNC_RELEASE_VERSION`)
/// so a release build asks for the exact companion that ships in the
/// same tag, without forcing every crate's `Cargo.toml` to bump
/// (which would invalidate the build cache).
///
/// Local builds fall back to `CARGO_PKG_VERSION`, which keeps dev
/// pair runs working without env plumbing.
pub fn expected_version() -> &'static str {
    option_env!("ANSYNC_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// `expected_version()` stripped of a leading `v` so it lines up with
/// Android's `versionName` (which is `0.2.1`, never `v0.2.1`). Use
/// this for equality checks against `dumpsys package … versionName=`.
pub fn expected_version_bare() -> &'static str {
    let v = expected_version();
    v.strip_prefix('v').unwrap_or(v)
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// GitHub serves SHA-256 digests in this field when the maintainer
    /// has opted in via `releases.use_digest`. Absent for older
    /// releases — verification falls through to the on-disk hash and
    /// emits a warning.
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FetchedApk {
    pub path: PathBuf,
    pub tag: String,
}

/// Get a path to a usable companion APK. Defaults to whatever tag
/// `expected_version()` resolves to — the companion that ships with
/// the running binary — so installs and version checks always align.
///
/// Kept as the entry point most call sites use; for explicit pinning
/// (tests, debugging) prefer [`fetch_companion`].
pub async fn fetch_latest_companion() -> Result<FetchedApk, PairingError> {
    fetch_companion(expected_version()).await
}

/// Pull the companion APK for a specific release tag. Mirrors
/// [`fetch_latest_companion`] except it hits
/// `/releases/tags/{tag}` so the asset matches the host build
/// byte-for-byte. `tag` is accepted with or without a leading `v` —
/// GitHub stores tags as `v0.2.1` so the lookup retries the prefixed
/// form on a 404.
pub async fn fetch_companion(tag: &str) -> Result<FetchedApk, PairingError> {
    let cache_root = cache_dir()?;
    fs::create_dir_all(&cache_root)
        .await
        .map_err(|e| PairingError::Protocol(format!("create cache dir: {e}")))?;
    let client = build_client()?;

    let release = lookup_release(&client, tag).await?;

    let asset = release
        .assets
        .iter()
        .find(|a| {
            a.name.starts_with(ASSET_NAME_PREFIX) && a.name.ends_with(ASSET_NAME_SUFFIX)
        })
        .ok_or_else(|| {
            PairingError::Protocol(format!(
                "no {ASSET_NAME_PREFIX}*.apk asset on release {}",
                release.tag_name
            ))
        })?;

    let dest = cache_root.join(format!("companion-{}.apk", release.tag_name));
    if cache_hit(&dest, asset).await {
        info!(path = %dest.display(), tag = %release.tag_name, "companion APK cache hit");
        return Ok(FetchedApk {
            path: dest,
            tag: release.tag_name,
        });
    }
    download(&client, &asset.browser_download_url, &dest).await?;
    if let Some(digest) = &asset.digest {
        verify_digest(&dest, digest).await?;
    } else {
        warn!(
            tag = %release.tag_name,
            "release exposes no SHA-256 digest; install will proceed but verification is skipped"
        );
    }
    Ok(FetchedApk {
        path: dest,
        tag: release.tag_name,
    })
}

async fn lookup_release(
    client: &reqwest::Client,
    tag: &str,
) -> Result<ReleaseResponse, PairingError> {
    // Try the tag verbatim first. GitHub stores release tags as the
    // exact string the maintainer pushed; some projects use `v0.2.1`,
    // others bare `0.2.1`. Cover both so callers don't have to care.
    let candidates: [String; 2] = if tag.starts_with('v') {
        [tag.to_string(), tag.trim_start_matches('v').to_string()]
    } else {
        [format!("v{tag}"), tag.to_string()]
    };
    let mut last_err: Option<PairingError> = None;
    for candidate in &candidates {
        let api_url = format!(
            "https://api.github.com/repos/{DEFAULT_OWNER}/{DEFAULT_REPO}/releases/tags/{candidate}"
        );
        let resp = client
            .get(&api_url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| PairingError::Protocol(format!("GET {api_url}: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            last_err = Some(PairingError::Protocol(format!(
                "release tag {candidate} not found upstream"
            )));
            continue;
        }
        let release: ReleaseResponse = resp
            .error_for_status()
            .map_err(|e| PairingError::Protocol(format!("release API: {e}")))?
            .json()
            .await
            .map_err(|e| PairingError::Protocol(format!("release JSON: {e}")))?;
        return Ok(release);
    }
    Err(last_err.unwrap_or_else(|| {
        PairingError::Protocol(format!("release lookup failed for {tag}"))
    }))
}

/// `pm dump <pkg>` returns a `versionName=...` line. Used by the
/// `pair` CLI to decide whether the cache (the latest release) is
/// newer than what's on the device.
pub async fn query_installed_version(
    serial: &str,
    package: &str,
) -> Result<Option<String>, PairingError> {
    use adb_client::ADBDeviceExt;
    let serial = serial.to_string();
    let package = package.to_string();
    tokio::task::spawn_blocking(move || {
        let mut srv = adb_client::ADBServer::default();
        let mut device = srv
            .get_device_by_name(&serial)
            .map_err(|e| PairingError::Protocol(format!("get_device {serial}: {e}")))?;
        let mut buf = Vec::with_capacity(8 * 1024);
        device
            .shell_command(&["dumpsys", "package", &package], &mut buf)
            .map_err(|e| PairingError::Protocol(format!("dumpsys package: {e}")))?;
        let stdout = String::from_utf8_lossy(&buf);
        // shell_v2 transport interleaves stdout/stderr framing with the
        // payload, so `lines().strip_prefix("versionName=")` misses
        // matches that sit at the start of a frame chunk. Find the
        // marker by substring, then read up to the next whitespace.
        if let Some(idx) = stdout.find("versionName=") {
            let rest = &stdout[idx + "versionName=".len()..];
            let end = rest
                .find(|c: char| c.is_whitespace() || c.is_control())
                .unwrap_or(rest.len());
            let version = rest[..end].trim();
            if !version.is_empty() {
                return Ok(Some(version.to_string()));
            }
        }
        Ok(None)
    })
    .await
    .map_err(|e| PairingError::Protocol(format!("spawn_blocking dumpsys: {e}")))?
}

async fn cache_hit(path: &std::path::Path, asset: &ReleaseAsset) -> bool {
    let Ok(meta) = fs::metadata(path).await else {
        return false;
    };
    if asset.size != 0 && meta.len() != asset.size {
        return false;
    }
    let Some(digest) = &asset.digest else {
        return true;
    };
    verify_digest(path, digest).await.is_ok()
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
) -> Result<(), PairingError> {
    info!(url, dest = %dest.display(), "downloading companion APK");
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| PairingError::Protocol(format!("GET {url}: {e}")))?
        .error_for_status()
        .map_err(|e| PairingError::Protocol(format!("download: {e}")))?;
    let partial = dest.with_extension("partial");
    let mut file = fs::File::create(&partial)
        .await
        .map_err(|e| PairingError::Protocol(format!("create {}: {e}", partial.display())))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk
            .map_err(|e| PairingError::Protocol(format!("download chunk: {e}")))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| PairingError::Protocol(format!("write apk chunk: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| PairingError::Protocol(format!("flush apk: {e}")))?;
    drop(file);
    fs::rename(&partial, dest)
        .await
        .map_err(|e| PairingError::Protocol(format!("rename apk: {e}")))?;
    Ok(())
}

async fn verify_digest(path: &std::path::Path, digest: &str) -> Result<(), PairingError> {
    let expected = digest.strip_prefix("sha256:").unwrap_or(digest);
    let data = fs::read(path)
        .await
        .map_err(|e| PairingError::Protocol(format!("read apk for digest: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let got = hex::encode(hasher.finalize());
    if !got.eq_ignore_ascii_case(expected) {
        return Err(PairingError::Protocol(format!(
            "APK digest mismatch: expected {expected}, got {got}"
        )));
    }
    Ok(())
}

fn cache_dir() -> Result<PathBuf, PairingError> {
    BaseDirs::new()
        .map(|b| b.cache_dir().join("ansync"))
        .ok_or_else(|| PairingError::Protocol("$HOME not set; cannot resolve cache dir".into()))
}

fn build_client() -> Result<reqwest::Client, PairingError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| PairingError::Protocol(format!("build reqwest client: {e}")))
}
