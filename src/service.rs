//! Installing the node as a service.
//!
//! A node is useless if it stops when you close a terminal, so this writes the
//! right unit for whatever the machine runs and starts it, rather than asking
//! you to copy a template out of a README.
//!
//! Where the platform has per-user services (launchd, systemd) they are used
//! when you install as yourself: sessions run as you, with your shell and your
//! environment, and it needs no root. Installing as root there is no user
//! session to hang a unit off, so it goes where the machine's units go and
//! names the account to run as. OpenRC, SysV init and BSD rc.d have no per-user
//! services at all, so they are always the second shape and always need root.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Service, unit and log-file name, all one thing now that a machine runs one
/// node rather than a server and a daemon.
pub const NAME: &str = "manymux";

/// The subcommand the unit runs.
const COMMAND: &str = "daemon";

const DESCRIPTION: &str = "manymux node: persistent terminal sessions";

/// rc.d derives shell variable names from the file name, so it can hold no
/// dashes. `manymux` has none, but keep the intent explicit.
const RC_NAME: &str = NAME;

/// The service manager this machine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// macOS. A per-user agent, or a system daemon when installed as root.
    Launchd,
    /// systemd. A user unit, which stops at logout without lingering, or a
    /// system unit when installed as root.
    Systemd,
    /// Alpine, Gentoo. System-wide, so root and a `command_user` drop.
    OpenRc,
    /// Devuan, MX, older Debian derivatives. System-wide, same as OpenRC.
    SysVInit,
    /// FreeBSD and friends. System-wide, same again.
    Rc,
}

/// Whether a unit belongs to one account or to the whole machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Installed by you, for you. No root, and it runs as you by construction.
    User,
    /// Installed by root, naming the account it runs as.
    System,
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
             Run it in the foreground instead: mm server"
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

    /// Which shape of unit to install here.
    ///
    /// Root gets the system one even where a per-user unit exists: there is no
    /// login session to bootstrap it into, and root's own node is rarely what
    /// was meant by `sudo mm service install`.
    pub fn scope(self) -> Scope {
        match self {
            Self::Launchd | Self::Systemd if uid() != 0 => Scope::User,
            _ => Scope::System,
        }
    }

    /// Where this manager's service definition lives.
    pub fn path(self, scope: Scope) -> Result<PathBuf> {
        let plist = format!("{LAUNCHD_LABEL}.plist");
        Ok(match (self, scope) {
            (Self::Launchd, Scope::User) => dirs::home_dir()
                .context("no home directory")?
                .join("Library/LaunchAgents")
                .join(plist),
            (Self::Launchd, Scope::System) => PathBuf::from("/Library/LaunchDaemons").join(plist),
            (Self::Systemd, Scope::User) => dirs::config_dir()
                .context("no config directory")?
                .join("systemd/user")
                .join(format!("{NAME}.service")),
            (Self::Systemd, Scope::System) => {
                PathBuf::from("/etc/systemd/system").join(format!("{NAME}.service"))
            }
            (Self::OpenRc | Self::SysVInit, _) => PathBuf::from("/etc/init.d").join(NAME),
            (Self::Rc, _) => PathBuf::from("/usr/local/etc/rc.d").join(RC_NAME),
        })
    }

    fn template(self, scope: Scope) -> &'static str {
        match (self, scope) {
            (Self::Launchd, Scope::User) => include_str!("../contrib/launchd.plist"),
            (Self::Launchd, Scope::System) => include_str!("../contrib/launchd-daemon.plist"),
            (Self::Systemd, Scope::User) => include_str!("../contrib/systemd.service"),
            (Self::Systemd, Scope::System) => include_str!("../contrib/systemd-system.service"),
            (Self::OpenRc, _) => include_str!("../contrib/openrc"),
            (Self::SysVInit, _) => include_str!("../contrib/sysvinit"),
            (Self::Rc, _) => include_str!("../contrib/rc.d"),
        }
    }
}

/// What an install did, so the caller can say something specific.
pub struct Installed {
    pub manager: Manager,
    pub path: PathBuf,
    pub scope: Scope,
    /// The account the node will run as, which is worth saying out loud when
    /// root installed it on someone else's behalf.
    pub user: String,
}

pub fn install() -> Result<Installed> {
    let manager = Manager::detect()?;
    let scope = manager.scope();
    let path = manager.path(scope)?;
    let fields = Fields::new(scope)?;

    if scope == Scope::System && uid() != 0 {
        bail!(
            "{} services are system-wide, so installing one needs root: \
             re-run with sudo",
            manager.label()
        );
    }
    // Fail before writing anything, rather than leaving a unit behind that
    // nothing ever loaded.
    if manager == Manager::Systemd && scope == Scope::User {
        user_runtime_dir()?;
    }

    // Only for a unit that runs as us: as root this would put a root-owned
    // directory in someone else's home, and the node makes its own anyway.
    if scope == Scope::User {
        std::fs::create_dir_all(&fields.logs).ok();
    }
    write_unit(&path, &fields.render(manager.template(scope)), manager)?;

    match manager {
        Manager::Launchd => {
            let domain = launchd_domain(scope);
            // Replacing an agent means booting the old one out first, which
            // fails harmlessly when there is nothing loaded.
            let _ = run(
                "launchctl",
                &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
            );
            run(
                "launchctl",
                &["bootstrap", &domain, &path.display().to_string()],
            )
            .context("loading the launchd job")?;
        }
        Manager::Systemd => {
            systemctl(scope, &["daemon-reload"]).context("reloading systemd")?;
            systemctl(scope, &["enable", "--now", NAME]).context("enabling the service")?;
            if scope == Scope::User {
                warn_about_linger(&fields.user);
            }
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

    Ok(Installed {
        manager,
        path,
        scope,
        user: fields.user,
    })
}

pub fn uninstall() -> Result<PathBuf> {
    let manager = Manager::detect()?;
    let scope = manager.scope();
    let path = manager.path(scope)?;

    if scope == Scope::System && uid() != 0 {
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
                &[
                    "bootout",
                    &format!("{}/{LAUNCHD_LABEL}", launchd_domain(scope)),
                ],
            );
        }
        Manager::Systemd => {
            let _ = systemctl(scope, &["disable", "--now", NAME]);
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
        let _ = systemctl(scope, &["daemon-reload"]);
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
    fn new(scope: Scope) -> Result<Self> {
        let user = username();
        // A system unit runs as someone else, so its paths are theirs. Under
        // sudo our own environment points at root's home, which would give the
        // node an empty config directory and hide the machines you added.
        let home = match scope {
            Scope::System => crate::user::named(&user).map(|user| PathBuf::from(user.home)),
            Scope::User => None,
        };
        Ok(Self {
            bin: std::env::current_exe()
                .context("finding the mm binary")?
                .display()
                .to_string(),
            user,
            config: match &home {
                Some(home) => crate::config::config_dir_for(home),
                None => crate::config::config_dir(),
            }
            .display()
            .to_string(),
            logs: match &home {
                Some(home) => crate::log::log_dir_for(home),
                None => crate::log::log_dir(),
            },
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
            "mm: user services stop at logout unless lingering is on.\n\
             mm: run `sudo loginctl enable-linger {user}` to keep sessions alive."
        );
    }
}

const LAUNCHD_LABEL: &str = "xyz.rayfish.manymux";

/// Where launchd keeps this scope's jobs.
fn launchd_domain(scope: Scope) -> String {
    match scope {
        Scope::User => format!("gui/{}", uid()),
        Scope::System => "system".to_string(),
    }
}

/// systemctl, in the right instance and with a way to reach it.
///
/// A user instance is addressed through `XDG_RUNTIME_DIR` and the session bus,
/// and an ssh login whose PAM stack has no `pam_systemd` sets neither, so the
/// command fails complaining about `$DBUS_SESSION_BUS_ADDRESS`. The directory is
/// usually there regardless, so point systemctl at it instead of giving up.
fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let mut command = Command::new("systemctl");
    if scope == Scope::System {
        command.args(args);
        return output(command, "systemctl", args);
    }
    let runtime = user_runtime_dir()?;
    command
        .arg("--user")
        .args(args)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/bus", runtime.display()),
        );
    output(command, "systemctl", args)
}

/// Where this user's systemd instance keeps its sockets.
///
/// The error is the useful part: no runtime directory means no user instance is
/// running for this account, and lingering both starts one and is what a service
/// meant to outlive your login needs anyway.
fn user_runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && dir.is_dir()
    {
        return Ok(dir);
    }
    let dir = PathBuf::from(format!("/run/user/{}", uid()));
    if dir.is_dir() {
        return Ok(dir);
    }
    bail!(
        "systemd is not running a user instance for {user}: there is no \
         $XDG_RUNTIME_DIR and no {dir}.\n\
         That happens when you log in over ssh on a host without pam_systemd, \
         and it also means the service would stop at logout. Both are fixed by:\n\
         \n    sudo loginctl enable-linger {user}\n\n\
         Then run `mm service install` again.",
        user = username(),
        dir = dir.display(),
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    output(command, program, args)
}

fn output(mut command: Command, program: &str, args: &[&str]) -> Result<()> {
    let output = command
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

    const MANAGERS: [Manager; 5] = [
        Manager::Launchd,
        Manager::Systemd,
        Manager::OpenRc,
        Manager::SysVInit,
        Manager::Rc,
    ];

    /// A template that lost a placeholder would install a unit pointing at
    /// nothing, and the failure would only show up on the machine it was
    /// installed on.
    #[test]
    fn every_template_names_the_binary_and_the_command() {
        for manager in MANAGERS {
            for scope in [Scope::User, Scope::System] {
                let template = manager.template(scope);
                assert!(template.contains("@BIN@"), "{}", manager.label());
                assert!(template.contains("@COMMAND@"), "{}", manager.label());
                assert!(template.contains("@CONFIG@"), "{}", manager.label());
            }
        }
    }

    /// A system unit runs as whoever the unit names, so it has to name them.
    #[test]
    fn system_templates_say_which_account_they_run_as() {
        for manager in MANAGERS {
            let template = manager.template(Scope::System);
            assert!(template.contains("@USER@"), "{}", manager.label());
        }
    }

    #[test]
    fn rendering_leaves_no_placeholders_behind() {
        for scope in [Scope::User, Scope::System] {
            let fields = Fields::new(scope).unwrap();
            for manager in MANAGERS {
                let rendered = fields.render(manager.template(scope));
                assert!(
                    !rendered.contains('@'),
                    "{} left a placeholder: {rendered}",
                    manager.label()
                );
            }
        }
    }

    /// Root gets a system unit everywhere; as yourself, only the managers with
    /// no per-user services make you reach for sudo.
    #[test]
    fn only_launchd_and_systemd_have_a_user_scope() {
        let as_root = uid() == 0;
        assert_eq!(Manager::Launchd.scope() == Scope::User, !as_root);
        assert_eq!(Manager::Systemd.scope() == Scope::User, !as_root);
        assert_eq!(Manager::OpenRc.scope(), Scope::System);
        assert_eq!(Manager::SysVInit.scope(), Scope::System);
        assert_eq!(Manager::Rc.scope(), Scope::System);
    }

    /// A system unit that landed in a home directory would never be loaded by
    /// the machine, and a user one outside it would need root to write.
    #[test]
    fn units_land_in_the_directory_their_scope_is_read_from() {
        for manager in [Manager::Launchd, Manager::Systemd] {
            let system = manager.path(Scope::System).unwrap();
            assert!(
                system.starts_with("/Library") || system.starts_with("/etc"),
                "{}: {}",
                manager.label(),
                system.display()
            );
            let user = manager.path(Scope::User).unwrap();
            assert!(
                user.starts_with(dirs::home_dir().unwrap()),
                "{}: {}",
                manager.label(),
                user.display()
            );
        }
    }

    #[test]
    fn rc_names_hold_no_dashes() {
        assert!(
            !RC_NAME.contains('-'),
            "rc.d cannot name a variable with a dash"
        );
    }
}
