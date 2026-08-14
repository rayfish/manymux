//! Replacing the running binary with the published one.
//!
//! manymux has to be current on every machine you touch, and re-running the
//! installer everywhere gets old. This is the same job `install.sh` does, from
//! inside the binary.
//!
//! It shells out to `curl` and `shasum`/`sha256sum` rather than linking an HTTP
//! stack and a hash: an update path is not worth doubling the dependency tree
//! over, and this is a tool that already delegates its transport to `ssh`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tracing::debug;

/// Where the release binaries are published, the same repo `install.sh` reads.
const REPO: &str = "rayfish/manymux";

/// What is published, and whether it differs from what is running.
pub struct Available {
    /// The release it came from: a version tag, or `nightly`.
    pub tag: String,
    pub asset: String,
    /// The published SHA-256 of that asset.
    pub checksum: String,
    /// SHA-256 of the binary running right now.
    pub running: String,
}

impl Available {
    /// Checksums, not versions: the nightly tag keeps one version across many
    /// builds, so comparing `--version` would never update. This also catches a
    /// half-finished install, and costs nothing on the stable channel.
    pub fn is_newer(&self) -> bool {
        self.checksum != self.running
    }
}

/// The release asset for this machine, e.g. `manymux-linux-x86_64`.
///
/// On Linux the libc flavour is a fact of the *running binary*, not the host: a
/// musl build updates to the musl asset, which runs anywhere, and a glibc build
/// to the plain one. Getting it backwards would hand a musl-only host a binary
/// that cannot start.
pub fn asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        // Termux. Android is its own asset rather than a Linux one: the binary
        // is linked against bionic, so the glibc and musl builds are both wrong
        // here even though `uname` says Linux.
        "android" => "android",
        other => bail!("no manymux release for {other}; build from source"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("no manymux release for {other}; build from source"),
    };
    let libc = if cfg!(all(target_os = "linux", target_env = "musl")) {
        "-musl"
    } else {
        ""
    };
    Ok(format!("mm-{os}-{arch}{libc}"))
}

/// What is published for this machine, and how it compares to what is running.
///
/// Prefers a stable release and falls back to the rolling nightly, matching
/// `install.sh`: GitHub excludes pre-releases from `/releases/latest`, so until
/// there is a stable release the nightly is all there is.
pub async fn check() -> Result<Available> {
    let asset = asset_name()?;
    let running = sha256_of(&std::env::current_exe().context("finding the running binary")?)
        .await
        .context("checksumming the running binary")?;

    for tag in ["latest", "nightly"] {
        let Some(checksum) = published_checksum(tag, &asset).await else {
            continue;
        };
        return Ok(Available {
            tag: tag.to_string(),
            asset,
            checksum,
            running,
        });
    }
    bail!("nothing published for {asset} yet")
}

/// Download the published asset, verify it, and swap it in.
pub async fn apply(available: &Available) -> Result<PathBuf> {
    let binary = std::env::current_exe().context("finding the running binary")?;
    let dir = binary.parent().unwrap_or(Path::new("."));

    // Before spending a download on it. The installer puts mm in
    // /usr/local/bin, which is root-owned on a stock macOS and on most Linux,
    // so this is the common case rather than an edge one, and it deserves an
    // answer rather than curl's own "failure writing output to destination".
    if !writable(dir) {
        bail!(
            "{} lives in {}, which needs root to replace: re-run with `sudo mm update`",
            file_name(&binary),
            dir.display()
        );
    }

    // Staged beside the binary rather than in /tmp, so the rename below is
    // within one filesystem and therefore atomic. Renaming over a running
    // executable is fine on Unix: the old inode stays alive for this process.
    let staged = dir.join(format!(".{}.new", file_name(&binary)));
    let url = download_url(&available.tag, &available.asset);

    let downloaded = curl(&url, &staged).await;
    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(&staged);
        return Err(e.context(format!("downloading {url}")));
    }

    let actual = sha256_of(&staged).await?;
    if actual != available.checksum {
        let _ = std::fs::remove_file(&staged);
        bail!(
            "checksum mismatch, refusing to install\n  expected: {}\n  got:      {actual}",
            available.checksum
        );
    }

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .context("making the new binary executable")?;
    std::fs::rename(&staged, &binary).with_context(|| format!("replacing {}", binary.display()))?;
    Ok(binary)
}

/// Whether this process can create and rename files in a directory.
///
/// `access` rather than reading the mode bits, because the mode is not the
/// whole answer: macOS ACLs can grant or deny past it, and root ignores it
/// entirely. Asking the kernel gets all three right.
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access reads the path and returns; the CString outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// What the node on this machine is doing, so an update knows whether
/// restarting it would cost anything.
pub struct Running {
    pub sessions: usize,
}

/// Ask the local node what it is holding, or `None` if none is running.
pub async fn running(socket: &Path) -> Option<Running> {
    let mut stream = crate::client::Stream::local(socket).await.ok()?;
    match stream.call(&crate::proto::Request::List).await.ok()? {
        crate::proto::Response::Sessions(sessions) => Some(Running {
            sessions: sessions.len(),
        }),
        _ => None,
    }
}

/// Whether the node is running a build other than `build`, the SHA-256 of the
/// binary now on disk.
///
/// This is the state that makes an update look finished when it is not:
/// replacing the file leaves the node running the old inode, so the machine
/// keeps behaving like the old build until it restarts. Nothing about the
/// binary on disk reveals it, which is why it is asked over the socket.
///
/// A node that cannot answer is behind by definition: `Version` did not exist
/// before the build that asks it, so an error or a dropped connection means the
/// node predates this binary. A node that answers without a checksum is the one
/// case left undecided, and it is reported as current: nagging about a node we
/// cannot compare would be a restart with nothing behind it.
pub async fn is_stale(socket: &Path, build: &str) -> bool {
    let Ok(mut stream) = crate::client::Stream::local(socket).await else {
        return false;
    };
    match stream.call(&crate::proto::Request::Version).await {
        Ok(crate::proto::Response::Version { build: theirs, .. }) => {
            theirs.is_some_and(|theirs| theirs != build)
        }
        Ok(other) => {
            debug!("unexpected answer to a version request: {other:?}");
            true
        }
        Err(e) => {
            debug!("the node did not answer a version request: {e:#}");
            true
        }
    }
}

fn download_url(tag: &str, asset: &str) -> String {
    match tag {
        "latest" => format!("https://github.com/{REPO}/releases/latest/download/{asset}"),
        tag => format!("https://github.com/{REPO}/releases/download/{tag}/{asset}"),
    }
}

/// The published checksum for an asset, or `None` if that release has no such
/// asset (or no such release).
async fn published_checksum(tag: &str, asset: &str) -> Option<String> {
    let url = format!("{}.sha256", download_url(tag, asset));
    let out = Command::new("curl")
        .args(["-fsSL", &url])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let digest = text.split_whitespace().next()?.to_lowercase();
    // Anything else means we are reading a redirect page rather than a sidecar.
    (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())).then_some(digest)
}

async fn curl(url: &str, to: &Path) -> Result<()> {
    // Captured rather than inherited: curl writes its own diagnosis to the
    // terminal before we get to say anything, so its half-sentence lands above
    // ours and the two read as one confused message.
    let out = Command::new("curl")
        .arg("-fsSL")
        .arg(url)
        .arg("-o")
        .arg(to)
        .output()
        .await
        .context("running curl")?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        if why.is_empty() {
            bail!("curl failed ({})", out.status);
        }
        bail!("{why}");
    }
    Ok(())
}

/// SHA-256 of a file, via whichever tool this machine has.
pub(crate) async fn sha256_of(path: &Path) -> Result<String> {
    for (program, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        let Ok(out) = Command::new(program).args(args).arg(path).output().await else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8(out.stdout).context("reading a checksum")?;
        if let Some(digest) = text.split_whitespace().next() {
            return Ok(digest.to_lowercase());
        }
    }
    bail!("no sha256sum or shasum on this machine, so a download cannot be verified")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mm".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_matches_what_ci_publishes() {
        // The names here have to be exactly the ones release.yml and
        // nightly.yml upload, or an update quietly finds nothing.
        let asset = asset_name().unwrap();
        assert!(asset.starts_with("mm-"), "{asset}");
        let published = [
            "mm-linux-x86_64",
            "mm-linux-aarch64",
            "mm-linux-x86_64-musl",
            "mm-linux-aarch64-musl",
            "mm-macos-x86_64",
            "mm-macos-aarch64",
            "mm-android-aarch64",
        ];
        assert!(published.contains(&asset.as_str()), "unpublished: {asset}");
    }

    #[test]
    fn stable_and_nightly_have_different_urls() {
        assert_eq!(
            download_url("latest", "mm-macos-aarch64"),
            "https://github.com/rayfish/manymux/releases/latest/download/mm-macos-aarch64"
        );
        assert_eq!(
            download_url("nightly", "mm-macos-aarch64"),
            "https://github.com/rayfish/manymux/releases/download/nightly/mm-macos-aarch64"
        );
    }

    #[test]
    fn a_root_owned_install_dir_is_not_writable() {
        // The case every macOS install hits: /usr/local/bin is root-owned
        // there, so `mm update` has to say so instead of downloading first and
        // failing on the write.
        assert!(writable(Path::new(std::env!("CARGO_MANIFEST_DIR"))));
        if unsafe { libc::getuid() } != 0 {
            assert!(!writable(Path::new("/")));
        }
    }

    /// The check that decides whether an update is finished or only half done.
    /// Both answers matter: a node running this binary must not be restarted
    /// for nothing, and one running anything else has to be caught, since
    /// nothing visible on disk says so.
    #[tokio::test]
    async fn a_node_is_stale_when_it_started_from_another_binary() {
        let dir = std::env::temp_dir().join(format!("mm-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");

        let node = crate::node::Node::start(crate::node::Config {
            peers: Vec::new(),
            hosts_file: None,
            notifications: false,
        })
        .await;
        let serving = tokio::spawn({
            let socket = socket.clone();
            async move { node.serve(&socket).await }
        });
        for _ in 0..100 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // The node hashed this test binary at startup, which is the binary
        // "on disk" as far as it is concerned.
        let ours = sha256_of(&std::env::current_exe().unwrap()).await.unwrap();
        assert!(!is_stale(&socket, &ours).await, "it is running this binary");
        assert!(
            is_stale(&socket, "a build it did not start from").await,
            "a node running something else has to be caught"
        );

        serving.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_update_is_decided_by_checksum_not_version() {
        // The nightly tag keeps one version across many builds, so comparing
        // versions would never update.
        let same = Available {
            tag: "nightly".into(),
            asset: "mm-macos-aarch64".into(),
            checksum: "abc".into(),
            running: "abc".into(),
        };
        assert!(!same.is_newer());
        let changed = Available {
            running: "def".into(),
            ..same
        };
        assert!(changed.is_newer());
    }
}
