//! Installing the node as a service.
//!
//! A node is useless if it stops when you close a terminal, so this writes the
//! right unit for whatever the machine runs and starts it, rather than asking
//! you to copy a template out of a README.
//!
//! Where the platform has per-user services (launchd, systemd) they are used:
//! sessions run as you, with your shell and your environment, and installing
//! needs no root. OpenRC, SysV init and BSD rc.d have no such thing, so there
//! the unit is a system service that drops to your account before running, and
//! installing it needs root.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Service, unit and log-file name, all one thing now that a machine runs one
/// node rather than a server and a daemon.
pub const NAME: &str = "tiles";

/// The subcommand the unit runs.
const COMMAND: &str = "daemon";

const DESCRIPTION: &str = "tiles node: persistent terminal sessions";

/// rc.d derives shell variable names from the file name, so it can hold no
/// dashes. `tiles` has none, but keep the intent explicit.
const RC_NAME: &str = NAME;

/// The service manager this machine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// macOS. Per-user agents, no root needed.
    Launchd,
    /// systemd user units. No root, but they stop at logout without lingering.
    Systemd,
    /// Alpine, Gentoo. System-wide, so root and a `command_user` drop.
    OpenRc,
    /// Devuan, MX, older Debian derivatives. System-wide, same as OpenRC.
    SysVInit,
    /// FreeBSD and friends. System-wide, same again.
    Rc,
}

impl Manager {
    /// What is actually running, not what happens to be installed: a host
    /// booted with systemd still has `/etc/init.d`, so the `/run` markers come
    /// first.
    pub fn detect() -> Result<Self> {
        if cfg!(target_os = "macos") {
            return Ok(Self::Launchd);
        }
        // The check systemd itself documents for "booted with systemd".
        if Path::new("/run/systemd/system").is_dir() {
            return Ok(Self::Systemd);
        }
        if Path::new("/run/openrc").exists() || Path::new("/sbin/openrc-run").exists() {
            return Ok(Self::OpenRc);
        }
        if Path::new("/etc/rc.subr").exists() {
            return Ok(Self::Rc);
        }
        if Path::new("/etc/init.d").is_dir() {
            return Ok(Self::SysVInit);
        }
        bail!(
            "no service manager found (launchd, systemd, OpenRC, SysV init, rc.d).\n\
             Run it in the foreground instead: tiles server"
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Launchd => "launchd",
            Self::Systemd => "systemd",
            Self::OpenRc => "OpenRC",
            Self::SysVInit => "SysV init",
            Self::Rc => "rc.d",
        }
    }

    /// Whether the unit is system-wide, and so needs root to install and has to
    /// drop privileges to run as the right user.
    fn system_wide(self) -> bool {
        matches!(self, Self::OpenRc | Self::SysVInit | Self::Rc)
    }

    /// Where this manager's service definition lives.
    pub fn path(self) -> Result<PathBuf> {
        Ok(match self {
            Self::Launchd => dirs::home_dir()
                .context("no home directory")?
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            Self::Systemd => dirs::config_dir()
                .context("no config directory")?
                .join("systemd/user")
                .join(format!("{NAME}.service")),
            Self::OpenRc | Self::SysVInit => PathBuf::from("/etc/init.d").join(NAME),
            Self::Rc => PathBuf::from("/usr/local/etc/rc.d").join(RC_NAME),
        })
    }

    fn template(self) -> &'static str {
        match self {
            Self::Launchd => include_str!("../contrib/launchd.plist"),
            Self::Systemd => include_str!("../contrib/systemd.service"),
            Self::OpenRc => include_str!("../contrib/openrc"),
            Self::SysVInit => include_str!("../contrib/sysvinit"),
            Self::Rc => include_str!("../contrib/rc.d"),
        }
    }
}

/// What an install did, so the caller can say something specific.
pub struct Installed {
    pub manager: Manager,
    pub path: PathBuf,
}

pub fn install() -> Result<Installed> {
    let manager = Manager::detect()?;
    let path = manager.path()?;
    let fields = Fields::new()?;

    if manager.system_wide() && uid() != 0 {
        bail!(
            "{} services are system-wide, so installing one needs root: \
             re-run with sudo",
            manager.label()
        );
    }

    std::fs::create_dir_all(&fields.logs).ok();
    write_unit(&path, &fields.render(manager.template()), manager)?;

    match manager {
        Manager::Launchd => {
            let target = format!("gui/{}", uid());
            // Replacing an agent means booting the old one out first, which
            // fails harmlessly when there is nothing loaded.
            let _ = run(
                "launchctl",
                &["bootout", &format!("{target}/{LAUNCHD_LABEL}")],
            );
            run(
                "launchctl",
                &["bootstrap", &target, &path.display().to_string()],
            )
            .context("loading the launchd agent")?;
        }
        Manager::Systemd => {
            run("systemctl", &["--user", "daemon-reload"]).context("reloading systemd")?;
            run("systemctl", &["--user", "enable", "--now", NAME])
                .context("enabling the service")?;
            warn_about_linger(&fields.user);
        }
        Manager::OpenRc => {
            run("rc-update", &["add", NAME, "default"]).context("enabling the service")?;
            run("rc-service", &[NAME, "restart"]).context("starting the service")?;
        }
        Manager::SysVInit => {
            // Debian-family and RH-family hosts register services with
            // different tools; try both and let the absent one fail quietly.
            let _ = run("update-rc.d", &[NAME, "defaults"]);
            let _ = run("chkconfig", &["--add", NAME]);
            run(&path.display().to_string(), &["restart"]).context("starting the service")?;
        }
        Manager::Rc => {
            run("sysrc", &[&format!("{RC_NAME}_enable=YES")]).context("enabling the service")?;
            run("service", &[RC_NAME, "restart"]).context("starting the service")?;
        }
    }

    Ok(Installed { manager, path })
}

pub fn uninstall() -> Result<PathBuf> {
    let manager = Manager::detect()?;
    let path = manager.path()?;

    if manager.system_wide() && uid() != 0 {
        bail!(
            "{} services are system-wide, so removing one needs root: re-run with sudo",
            manager.label()
        );
    }

    // Teardown is best-effort throughout: a unit that was never loaded, or a
    // registration tool this distro does not ship, must not stop the file being
    // removed.
    match manager {
        Manager::Launchd => {
            let _ = run(
                "launchctl",
                &["bootout", &format!("gui/{}/{LAUNCHD_LABEL}", uid())],
            );
        }
        Manager::Systemd => {
            let _ = run("systemctl", &["--user", "disable", "--now", NAME]);
        }
        Manager::OpenRc => {
            let _ = run("rc-service", &[NAME, "stop"]);
            let _ = run("rc-update", &["del", NAME, "default"]);
        }
        Manager::SysVInit => {
            let _ = run(&path.display().to_string(), &["stop"]);
            let _ = run("update-rc.d", &["-f", NAME, "remove"]);
            let _ = run("chkconfig", &["--del", NAME]);
        }
        Manager::Rc => {
            let _ = run("service", &[RC_NAME, "stop"]);
            let _ = run("sysrc", &[&format!("{RC_NAME}_enable=NO")]);
        }
    }

    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    if manager == Manager::Systemd {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(path)
}

/// Everything a unit template needs filled in.
struct Fields {
    bin: String,
    user: String,
    config: String,
    logs: PathBuf,
}

impl Fields {
    fn new() -> Result<Self> {
        Ok(Self {
            bin: std::env::current_exe()
                .context("finding the tiles binary")?
                .display()
                .to_string(),
            user: username(),
            config: crate::config::config_dir().display().to_string(),
            logs: crate::log::log_dir(),
        })
    }

    fn render(&self, template: &str) -> String {
        template
            .replace("@LABEL@", LAUNCHD_LABEL)
            .replace("@NAME@", NAME)
            .replace("@RCNAME@", RC_NAME)
            .replace("@COMMAND@", COMMAND)
            .replace("@DESC@", DESCRIPTION)
            .replace("@BIN@", &self.bin)
            .replace("@USER@", &self.user)
            .replace("@CONFIG@", &self.config)
            .replace("@LOGS@", &self.logs.display().to_string())
    }
}

fn write_unit(path: &Path, contents: &str, manager: Manager) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    // Init scripts are executed directly by the service manager, so they have
    // to carry the exec bit. Declarative units must not.
    if matches!(manager, Manager::OpenRc | Manager::SysVInit | Manager::Rc) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    Ok(())
}

/// Without lingering, a systemd user service stops when you log out, which
/// defeats the point of a session that outlives your connection.
fn warn_about_linger(user: &str) {
    let lingering = Command::new("loginctl")
        .args(["show-user", user, "--property=Linger"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "Linger=yes")
        .unwrap_or(false);
    if !lingering {
        eprintln!(
            "tiles: user services stop at logout unless lingering is on.\n\
             tiles: run `sudo loginctl enable-linger {user}` to keep sessions alive."
        );
    }
}

const LAUNCHD_LABEL: &str = "xyz.rayfish.tiles";

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn uid() -> u32 {
    // SAFETY: getuid always succeeds and touches no memory.
    unsafe { libc::getuid() }
}

/// The account the service should run as: the user who invoked us, even when
/// that was through sudo.
fn username() -> String {
    std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| uid().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A template that lost a placeholder would install a unit pointing at
    /// nothing, and the failure would only show up on the machine it was
    /// installed on.
    #[test]
    fn every_template_names_the_binary_and_the_command() {
        for manager in [
            Manager::Launchd,
            Manager::Systemd,
            Manager::OpenRc,
            Manager::SysVInit,
            Manager::Rc,
        ] {
            let template = manager.template();
            assert!(template.contains("@BIN@"), "{}", manager.label());
            assert!(template.contains("@COMMAND@"), "{}", manager.label());
            assert!(template.contains("@CONFIG@"), "{}", manager.label());
        }
    }

    #[test]
    fn rendering_leaves_no_placeholders_behind() {
        let fields = Fields::new().unwrap();
        for manager in [
            Manager::Launchd,
            Manager::Systemd,
            Manager::OpenRc,
            Manager::SysVInit,
            Manager::Rc,
        ] {
            let rendered = fields.render(manager.template());
            assert!(
                !rendered.contains('@'),
                "{} left a placeholder: {rendered}",
                manager.label()
            );
        }
    }

    #[test]
    fn system_wide_managers_are_the_ones_without_user_services() {
        assert!(!Manager::Launchd.system_wide());
        assert!(!Manager::Systemd.system_wide());
        assert!(Manager::OpenRc.system_wide());
        assert!(Manager::SysVInit.system_wide());
        assert!(Manager::Rc.system_wide());
    }

    #[test]
    fn rc_names_hold_no_dashes() {
        assert!(
            !RC_NAME.contains('-'),
            "rc.d cannot name a variable with a dash"
        );
    }
}
