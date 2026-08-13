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

    // Staged beside the binary rather than in /tmp, so the rename below is
    // within one filesystem and therefore atomic. Renaming over a running
    // executable is fine on Unix: the old inode stays alive for this process.
    let staged = dir.join(format!(".{}.new", file_name(&binary)));
    let url = download_url(&available.tag, &available.asset);

    let downloaded = curl(&url, &staged).await;
    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(&staged);
        return Err(e.context(format!(
            "downloading {url}\n(is {} writable? try again with sudo)",
            dir.display()
        )));
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
    std::fs::rename(&staged, &binary).with_context(|| {
        format!(
            "replacing {}\n(not writable? try again with sudo)",
            binary.display()
        )
    })?;
    Ok(binary)
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

/// Stop the node so it comes back on the new binary, then start one again.
///
/// Every session it owns dies with it: they are its children, holding PTYs it
/// owns. That is why the caller decides, rather than this happening quietly as
/// part of every update.
pub async fn restart_node(socket: &Path) -> Result<()> {
    // A node older than this binary has never heard of `stop`: it fails to
    // decode the request and hangs up, so asking politely reports nothing more
    // useful than a closed connection. It still has to go, or the update lands
    // on the next reboot and not before, so fall back to the pid holding the
    // socket. Every machine gets this once, on the update that introduces it.
    if let Err(e) = ask_to_stop(socket).await {
        debug!("the node did not take a stop request: {e:#}");
        signal_node(socket)
            .await
            .context("stopping a node that did not answer a stop request")?;
    }

    // Wait for it to actually go, so the node we start next binds the socket
    // rather than losing a race with the one on its way out.
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(socket).await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // A service manager will have restarted it already; this covers a node that
    // was started on demand.
    crate::node::ensure_running(socket).await
}

async fn ask_to_stop(socket: &Path) -> Result<()> {
    let mut stream = crate::client::Stream::local(socket)
        .await
        .context("connecting to the node")?;
    stream.call(&crate::proto::Request::Stop).await?;
    Ok(())
}

/// Terminate the node by the pid the kernel reports for its socket.
///
/// SIGTERM rather than SIGKILL: the difference to the sessions is nil (they die
/// with the node either way, being its children), but the node still gets to
/// flush its log.
async fn signal_node(socket: &Path) -> Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .context("connecting to the node")?;
    let pid = crate::ipc::peer_pid(&stream).context("asking the kernel who holds the socket")?;
    drop(stream);

    // SAFETY: kill touches no memory. A pid that has already exited gives ESRCH,
    // which is the outcome we wanted anyway.
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if sent != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            bail!("signalling the node (pid {pid}): {e}");
        }
    }
    Ok(())
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
    let status = Command::new("curl")
        .arg("-fsSL")
        .arg(url)
        .arg("-o")
        .arg(to)
        .status()
        .await
        .context("running curl")?;
    if !status.success() {
        bail!("curl failed ({status})");
    }
    Ok(())
}

/// SHA-256 of a file, via whichever tool this machine has.
async fn sha256_of(path: &Path) -> Result<String> {
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
