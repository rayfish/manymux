use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use tokio::task::JoinHandle;
use tracing::debug;

use manymux::client::attach::{self, Mode, Outcome};
use manymux::client::switch::{Cycle, Located};
use manymux::client::{Attached, Stream};
use manymux::hosts::{Hosts, LOCAL, Target, is_this_machine, this_machine};
use manymux::node::{Config, Node};
use manymux::proto::{HostedSession, Request, Response, SessionInfo, SpawnSpec};
use manymux::{config, log, style, term};

mod complete;

#[derive(Parser)]
#[command(
    name = "mm",
    version,
    about = "Persistent terminal sessions you can leave and come back to"
)]
struct Cli {
    /// Where this machine's node listens. Defaults to the per-user runtime path.
    #[arg(long, global = true, value_hint = ValueHint::FilePath)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run this machine's node: own its sessions, and watch the machines you
    /// have added for anything wanting attention. Usually a service.
    #[command(alias = "server")]
    Daemon {
        /// Watch for events but do not raise desktop notifications.
        #[arg(long)]
        no_notify: bool,
    },
    /// Bridge stdin and stdout to this machine's node, starting it if needed.
    ///
    /// This is what `ssh <host> mm agent` runs. You do not normally type it.
    Agent,

    /// List sessions, on every added machine by default.
    #[command(visible_alias = "l", alias = "list")]
    Ls {
        /// Limit to one machine.
        #[arg(add = complete::hosts_or_local())]
        host: Option<String>,
    },
    /// Start a session and attach to it.
    ///
    /// A first argument naming a machine you have added runs it there; anything
    /// else is the command, run here. `mm new local <cmd>` forces this
    /// machine, for a program that shares a name with one of your hosts.
    #[command(visible_alias = "n")]
    New {
        /// Session name. Defaults to the command's name.
        #[arg(short, long)]
        name: Option<String>,
        /// Start the session but stay where you are.
        #[arg(short, long)]
        detached: bool,
        /// `[host] [command...]`. Defaults to your login shell, here.
        #[arg(trailing_var_arg = true, add = complete::new_args())]
        args: Vec<String>,
    },
    /// Attach to a session, as `name` or `host/name`.
    #[command(visible_alias = "a")]
    Attach {
        #[arg(add = complete::targets())]
        target: String,
    },
    /// Send SIGHUP to a session's process group.
    #[command(visible_alias = "k")]
    Kill {
        #[arg(add = complete::targets())]
        target: String,
    },
    /// Set a session's title, overriding whatever the program sets.
    #[command(visible_alias = "r")]
    Rename {
        #[arg(add = complete::targets())]
        target: String,
        title: String,
    },

    /// Watch a machine without starting a session on it. Starting one adds it
    /// anyway; this is for machines you only want to see in `mm ls`.
    Add { host: String },
    /// List the machines being watched.
    #[command(visible_alias = "h")]
    Hosts,
    /// Stop watching a machine.
    #[command(alias = "remove")]
    Rm {
        #[arg(add = complete::watched_hosts())]
        host: String,
    },

    /// Replace this binary with the published one.
    #[command(visible_alias = "up")]
    Update {
        /// Say what would change, without changing it.
        #[arg(long)]
        check: bool,
        /// Restart the node even though sessions are running. They die with it.
        #[arg(long)]
        force: bool,
    },

    /// Install or remove the background service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Set up tab completion for your shell.
    Completions {
        /// Which shell. Guessed from $SHELL when left out.
        shell: Option<Shell>,
        /// Write the script where the shell will find it, rather than printing
        /// it for you to redirect somewhere yourself.
        #[arg(long)]
        install: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service and start it.
    Install,
    /// Stop the service and remove it.
    Uninstall,
}

/// Exit codes, as plain numbers rather than `ExitCode`, because the process
/// exits by hand rather than by returning; see `main`.
const OK: u8 = 0;
const FAILED: u8 = 1;

fn main() -> ExitCode {
    // Before anything else: before stdout is touched, before the arguments are
    // parsed (a completion request is not a command line this parser accepts),
    // and outside the runtime, since the completers start one of their own.
    complete::intercept();
    cli()
}

#[tokio::main]
async fn cli() -> ExitCode {
    let cli = Cli::parse();
    // Only the node writes a log file. A command that prints and exits has the
    // terminal for that, and the agent must leave stdout strictly alone.
    let _log = log::init(match cli.command {
        Command::Daemon { .. } => Some(manymux::service::NAME),
        _ => None,
    });

    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mm: {e:#}");
            1
        }
    };

    // Exit rather than returning, because `tokio::io::stdin` reads on a
    // blocking thread that cannot be cancelled: when a session ends by itself,
    // that thread is still parked in read(2) with nobody about to type
    // anything, and dropping the runtime waits for it forever. Detaching hid
    // this, since pressing the detach key is itself the read that completes.
    //
    // Nothing here needs unwinding: the terminal was already restored, and the
    // ssh child dies with our end of its pipes. Flush first, though, since a
    // piped stdout is block-buffered and would otherwise be lost.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(i32::from(code))
}

async fn run(cli: Cli) -> Result<u8> {
    let socket = cli.socket.unwrap_or_else(config::socket);

    match cli.command {
        Command::Daemon { no_notify } => {
            let node = Node::start(Config {
                peers: Hosts::load()?.names(),
                hosts_file: Some(Hosts::path()),
                notifications: !no_notify,
            })
            .await;
            node.serve(&socket).await?;
            Ok(OK)
        }

        Command::Agent => {
            manymux::node::agent(&socket).await?;
            Ok(OK)
        }

        Command::Ls { host } => list(&socket, host).await,

        Command::New {
            name,
            detached,
            args,
        } => {
            let Started { host, command } = where_to_start(args)?;
            let spec = SpawnSpec {
                name,
                command,
                // Only meaningful on this machine; elsewhere the session starts
                // in the node's own working directory.
                cwd: is_this_machine(&host).then(current_dir).flatten(),
                size: attach::session_size(),
            };
            let mut stream = open_or_start(&socket, &host).await?;
            let Response::Spawned { name } = stream.call(&Request::Spawn(spec)).await? else {
                bail!("unexpected response to spawn");
            };
            if remember(&host)? {
                eprintln!("mm: now watching {host}; `mm rm {host}` to stop");
            }
            if detached {
                println!("{}", qualified(&host, &name));
                return Ok(OK);
            }
            do_attach(&socket, &host, &name).await
        }

        Command::Attach { target } => {
            let target = locate(&socket, &target).await?;
            do_attach(&socket, &target.host, &target.session).await
        }

        Command::Kill { target } => {
            let target = locate(&socket, &target).await?;
            let mut stream = open(&socket, &target.host).await?;
            stream
                .call(&Request::Kill {
                    name: target.session,
                })
                .await?;
            Ok(OK)
        }

        Command::Rename { target, title } => {
            let target = locate(&socket, &target).await?;
            let mut stream = open(&socket, &target.host).await?;
            stream
                .call(&Request::Rename {
                    name: target.session,
                    title,
                })
                .await?;
            Ok(OK)
        }

        Command::Add { host } => {
            let mut hosts = Hosts::load()?;
            hosts.add(&host)?;
            // Connect once with the terminal attached, so ssh can ask about an
            // unknown host key or a passphrase. Every later command carries the
            // protocol on stdin and has no way to prompt.
            manymux::ssh::greet(&host).await?;
            hosts.save()?;
            // Prove it works now rather than at the next listing, when the
            // failure would be harder to connect to what you just typed.
            match sessions_on(&socket, &host).await {
                Ok(sessions) => {
                    println!("watching {host} ({} sessions)", sessions.len());
                    Ok(OK)
                }
                Err(e) => {
                    eprintln!("mm: added {host}, but could not reach it: {e:#}");
                    Ok(FAILED)
                }
            }
        }

        Command::Hosts => {
            let hosts = Hosts::load()?;
            if hosts.is_empty() {
                println!("no machines added; `mm add <ssh-host>`");
            }
            for host in hosts.names() {
                println!("{host}");
            }
            Ok(OK)
        }

        Command::Rm { host } => {
            let mut hosts = Hosts::load()?;
            if !hosts.remove(&host) {
                bail!("{host} is not being watched");
            }
            hosts.save()?;
            Ok(OK)
        }

        Command::Update { check, force } => update(&socket, check, force).await,

        Command::Service { action } => {
            match action {
                ServiceAction::Install => {
                    let installed = manymux::service::install()?;
                    println!(
                        "installed and started manymux under {} ({})",
                        installed.manager.label(),
                        installed.path.display()
                    );
                    if installed.scope == manymux::service::Scope::System {
                        println!("it is a system service and runs as {}", installed.user);
                    }
                }
                ServiceAction::Uninstall => {
                    let path = manymux::service::uninstall()?;
                    println!("removed {}", path.display());
                }
            }
            Ok(OK)
        }

        Command::Completions { shell, install } => completions(shell, install),
    }
}

/// Replace this binary with the published one, and pick it up.
async fn update(socket: &Path, check: bool, force: bool) -> Result<u8> {
    let available = manymux::update::check().await?;
    if !available.is_newer() {
        println!(
            "{} manymux {} is the published {} build",
            style::green("✓"),
            env!("CARGO_PKG_VERSION"),
            available.tag
        );
        return Ok(OK);
    }
    if check {
        println!(
            "an update is published on the {} channel; `mm update` to take it",
            style::bold(&available.tag)
        );
        return Ok(OK);
    }

    let path = manymux::update::apply(&available).await?;
    println!(
        "{} updated {}",
        style::green("✓"),
        style::bold(&path.display().to_string())
    );
    refresh_completions();

    // The node is a long-running process still executing the old binary, so an
    // update that leaves it alone has not really landed. Restarting it kills
    // every session it owns, though, which is not something to do quietly.
    let Some(running) = manymux::update::running(socket).await else {
        return Ok(OK);
    };
    if running.sessions > 0 && !force {
        println!(
            "{} the node is still on the old binary, and restarting it would end \
             {} running session{}.\n  `mm update --force` to restart anyway, or leave it \
             until they are done.",
            style::amber("!"),
            running.sessions,
            if running.sessions == 1 { "" } else { "s" }
        );
        return Ok(OK);
    }

    manymux::update::restart_node(socket).await?;
    println!("{} restarted the node", style::green("✓"));
    Ok(OK)
}

/// Open a stream to a machine: this one's socket, or `mm agent` over ssh.
async fn open(socket: &Path, host: &str) -> Result<Stream> {
    if is_this_machine(host) {
        return Stream::local(socket).await;
    }
    Stream::over_ssh(host).await
}

/// Like [`open`], but starts this machine's node first if it is not running.
///
/// Only for asking a machine to hold something new: `tmux new` starts its
/// server too, while `tmux ls` does not. On another machine the agent does the
/// same thing at the far end.
async fn open_or_start(socket: &Path, host: &str) -> Result<Stream> {
    if is_this_machine(host) {
        manymux::node::ensure_running(socket).await?;
    }
    open(socket, host).await
}

/// Sessions found, and machines that could not be asked. One being asleep must
/// not hide the others.
#[derive(Default)]
struct Listing {
    sessions: Vec<HostedSession>,
    unreachable: Vec<Unreachable>,
}

/// A machine that could not be reached, and why.
struct Unreachable {
    host: String,
    error: String,
}

impl Listing {
    fn add(&mut self, host: &str, found: Result<Vec<SessionInfo>>) {
        match found {
            Ok(sessions) => {
                self.sessions
                    .extend(sessions.into_iter().map(|session| HostedSession {
                        host: host.to_string(),
                        session,
                    }))
            }
            Err(e) => self.unreachable.push(Unreachable {
                host: host.to_string(),
                error: format!("{e:#}"),
            }),
        }
    }
}

/// `mm ls`, over every watched machine or just one.
async fn list(socket: &Path, host: Option<String>) -> Result<u8> {
    let listing = match host {
        // One named machine: ask it directly, so an error names that machine.
        Some(name) => {
            let mut listing = Listing::default();
            // Label this machine the way a full listing would, however the
            // caller spelled it.
            let label = if is_this_machine(&name) {
                this_machine()
            } else {
                &name
            };
            listing.add(label, sessions_on(socket, &name).await);
            listing
        }
        None => everywhere(socket).await?,
    };

    let rows: Vec<_> = listing
        .sessions
        .into_iter()
        .map(|hosted| term::SessionRow {
            host: hosted.host,
            session: hosted.session,
        })
        .collect();

    // Nothing answering at all is a failure, not a report of no sessions, so
    // don't print an empty table over the top of the reason.
    let nothing_answered = rows.is_empty() && !listing.unreachable.is_empty();
    if !nothing_answered {
        print!("{}", term::session_table(&rows));
    }
    for host in &listing.unreachable {
        eprintln!("mm: {}: {}", host.host, host.error);
    }
    Ok(if nothing_answered { FAILED } else { OK })
}

/// One machine's answer, kept with which machine gave it.
struct Asked {
    host: String,
    found: Result<Vec<SessionInfo>>,
}

/// Sessions on this machine and on every watched one, asked all at once.
async fn everywhere(socket: &Path) -> Result<Listing> {
    let mut listing = Listing::default();

    // No node here is ordinary on a machine you only use to reach others, so it
    // is not worth a complaint.
    match sessions_on(socket, LOCAL).await {
        Ok(sessions) => listing.add(this_machine(), Ok(sessions)),
        Err(e) => debug!("no node here: {e:#}"),
    }

    let mut asked = tokio::task::JoinSet::new();
    for host in Hosts::load()?.names() {
        let socket = socket.to_path_buf();
        asked.spawn(async move {
            let found = sessions_on(&socket, &host).await;
            Asked { host, found }
        });
    }
    while let Some(answer) = asked.join_next().await {
        match answer {
            Ok(Asked { host, found }) => listing.add(&host, found),
            Err(e) => debug!("a host query did not finish: {e}"),
        }
    }

    listing
        .sessions
        .sort_by(|a, b| (&a.host, &a.session.name).cmp(&(&b.host, &b.session.name)));
    listing.unreachable.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(listing)
}

async fn sessions_on(socket: &Path, host: &str) -> Result<Vec<SessionInfo>> {
    // No node here means no sessions here, which is an answer rather than a
    // failure. Starting one just to be told that would be rude.
    if is_this_machine(host) && !socket.exists() {
        return Ok(Vec::new());
    }
    match open(socket, host).await?.call(&Request::List).await? {
        Response::Sessions(sessions) => Ok(sessions),
        other => bail!("unexpected response to list: {other:?}"),
    }
}

/// How long a switch key waits on a listing that has not landed yet, before
/// going with whatever it already knows. A machine that is asleep must not
/// leave the terminal sitting there.
const LISTING_WAIT: Duration = Duration::from_millis(500);

/// Attach, and keep attaching for as long as the switch keys ask for another
/// session.
///
/// The terminal is held across the whole run rather than per session, so a hop
/// is a repaint and not a trip out of the alternate screen and back.
async fn do_attach(socket: &Path, host: &str, name: &str) -> Result<u8> {
    let mut cycle = Cycle::new(Located::new(host, name));
    // Asked for before the first attach, so the first switch key has something
    // to go on.
    let mut listing = Some(spawn_listing(socket));

    let held = attach::hold()?;
    let mut mode = Mode::Focus;
    let mut hopped = false;
    let (outcome, where_) = loop {
        let target = cycle.current().clone();
        let where_ = qualified(&target.host, &target.session);
        let session = match attach_to(socket, &target).await {
            Ok(session) => session,
            // A hop onto a session that has gone since the listing. Stay where
            // you were and put the dead entry out of the cycle, rather than
            // throwing away a working attach over it. A second failure in a row
            // is the machine's, not the listing's, and is reported.
            Err(e) if hopped => {
                debug!("could not attach to {where_}: {e:#}");
                cycle.forget(&target);
                cycle.undo();
                hopped = false;
                continue;
            }
            Err(e) => return Err(e),
        };

        match attach::run(&held, session, &where_, mode).await? {
            Outcome::Switch(motion) => {
                take_listing(&mut listing, &mut cycle).await;
                hopped = false;
                if let Some(next) = cycle.step(motion) {
                    cycle.moved_to(next);
                    hopped = true;
                    if listing.is_none() {
                        listing = Some(spawn_listing(socket));
                    }
                }
                // Nowhere to go is not a reason to drop back to focus: the next
                // key carries on from wherever this one left you.
                mode = Mode::Control;
            }
            outcome => break (outcome, where_),
        }
    };
    // Before the message, which belongs on the screen the shell gets back.
    drop(held);

    match outcome {
        Outcome::Detached => {
            println!("[detached from {where_}]");
            Ok(OK)
        }
        Outcome::Exited(code) => {
            println!("[{where_} exited with status {code}]");
            Ok(u8::try_from(code).unwrap_or(1))
        }
        Outcome::Disconnected => {
            println!("[disconnected from {where_}]");
            Ok(FAILED)
        }
        Outcome::Switch(_) => unreachable!("switches never leave the loop above"),
    }
}

/// Open a stream to wherever a session is, and attach to it.
async fn attach_to(socket: &Path, target: &Located) -> Result<Attached> {
    let stream = open(socket, &target.host).await?;
    stream.attach(&target.session, attach::session_size()).await
}

/// Ask every machine what it is running, off to one side, so that no keystroke
/// waits on ssh.
fn spawn_listing(socket: &Path) -> JoinHandle<Vec<Located>> {
    let socket = socket.to_path_buf();
    tokio::spawn(async move {
        match everywhere(&socket).await {
            Ok(listing) => listing
                .sessions
                .into_iter()
                .map(|hosted| Located::new(hosted.host, hosted.session.name))
                .collect(),
            Err(e) => {
                debug!("could not list sessions for the switch keys: {e:#}");
                Vec::new()
            }
        }
    })
}

/// Hand the cycle the listing, if one has arrived or arrives shortly.
///
/// What is already known stands rather than being replaced by nothing, and a
/// listing still out there asking is left running rather than thrown away.
async fn take_listing(pending: &mut Option<JoinHandle<Vec<Located>>>, cycle: &mut Cycle) {
    let Some(mut task) = pending.take() else {
        return;
    };
    match tokio::time::timeout(LISTING_WAIT, &mut task).await {
        Ok(Ok(sessions)) => {
            if !sessions.is_empty() {
                cycle.refresh(sessions);
            }
        }
        Ok(Err(e)) => debug!("the listing task did not finish: {e}"),
        Err(_) => *pending = Some(task),
    }
}

/// Work out which machine a target is on.
///
/// `host/session` says so outright. A bare name is looked for here first, and
/// then across every watched machine, so `mm attach api` finds the session
/// wherever you left it without having to remember which machine that was.
async fn locate(socket: &Path, target: &str) -> Result<Located> {
    let target = Target::parse(target)?;
    if let Some(host) = target.host {
        return Ok(Located {
            host,
            session: target.session,
        });
    }

    // Nearly always here, and asking is one round trip on a local socket.
    let here = sessions_on(socket, LOCAL).await.unwrap_or_default();
    if here.iter().any(|session| session.name == target.session) {
        return Ok(Located {
            host: this_machine().to_string(),
            session: target.session,
        });
    }

    let listing = everywhere(socket).await?;
    let mut hosts: Vec<String> = listing
        .sessions
        .into_iter()
        .filter(|hosted| hosted.session.name == target.session)
        .map(|hosted| hosted.host)
        .collect();
    hosts.dedup();

    match hosts.len() {
        1 => Ok(Located {
            host: hosts.remove(0),
            session: target.session,
        }),
        0 => bail!("no session named {}; see `mm ls`", target.session),
        // Two machines can each have a `build`. Say which, rather than guessing.
        _ => bail!(
            "{} is on more than one machine ({}); say which, like `{}/{}`",
            target.session,
            hosts.join(", "),
            hosts[0],
            target.session
        ),
    }
}

fn qualified(host: &str, name: &str) -> String {
    if is_this_machine(host) {
        name.to_string()
    } else {
        format!("{host}/{name}")
    }
}

/// Where a `mm new` should run, and what it should run there.
struct Started {
    host: String,
    command: Vec<String>,
}

/// Split `[host] [command...]`.
///
/// The first argument is what to run if it is a command, and where to run it
/// otherwise. No registration needed: ssh does not make you declare a host
/// before using it, so neither does this, and an ssh destination that does not
/// exist gets ssh's own error rather than one of ours.
fn where_to_start(mut args: Vec<String>) -> Result<Started> {
    let is_host = match args.first() {
        Some(first) => !runnable(first),
        None => false,
    };
    Ok(Started {
        host: if is_host {
            args.remove(0)
        } else {
            LOCAL.to_string()
        },
        command: args,
    })
}

/// Whether this looks like something to run rather than somewhere to run it.
///
/// A path or a shell snippet is taken at face value; a bare word has to be on
/// `PATH`, which is what separates `mm new claude` from `mm new gpu-box`.
fn runnable(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    if program.contains(|c: char| c.is_whitespace() || "|&;<>()$`\\\"'".contains(c)) {
        return true;
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Remember a machine, so `mm ls` covers it and the node watches it for
/// bells. Starting a session somewhere is a clear enough signal of interest
/// that asking you to register it first would be ceremony.
fn remember(host: &str) -> Result<bool> {
    if is_this_machine(host) {
        return Ok(false);
    }
    let mut hosts = Hosts::load()?;
    if hosts.has(host) {
        return Ok(false);
    }
    hosts.add(host)?;
    hosts.save()?;
    Ok(true)
}

fn current_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Rewrite an installed completion script after an update.
///
/// The script and the binary talk to each other, and clap_complete makes no
/// promise that a script written by one version still works with the next. Only
/// touches a file that is already there, and says nothing either way: nobody
/// asked about completions, they asked for an update.
fn refresh_completions() {
    let Some(shell) = Shell::from_env() else {
        return;
    };
    let Some(path) = completion_path(shell).filter(|path| path.exists()) else {
        return;
    };
    match std::fs::File::create(&path).map_err(anyhow::Error::from) {
        Ok(mut file) => {
            if let Err(e) = complete::registration(shell, &mut file) {
                debug!("could not refresh {}: {e:#}", path.display());
            }
        }
        Err(e) => debug!("could not refresh {}: {e:#}", path.display()),
    }
}

/// Print a completion script, or write it where the shell will find it.
fn completions(shell: Option<Shell>, install: bool) -> Result<u8> {
    let Some(shell) = shell.or_else(Shell::from_env) else {
        bail!("could not tell which shell you use; name it: mm completions zsh");
    };
    if !install {
        complete::registration(shell, &mut std::io::stdout())?;
        return Ok(OK);
    }

    let Some(path) = completion_path(shell) else {
        bail!(
            "no install path known for {shell}; print it and place it yourself: \
             mm completions {shell}"
        );
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::File::create(&path)?;
    complete::registration(shell, &mut file)?;
    println!("wrote {}", path.display());

    if shell == Shell::Zsh {
        println!(
            "zsh only reads completions on its fpath. If tab completion does not work, add this \
             to ~/.zshrc:\n\n  fpath=({} $fpath)\n  autoload -Uz compinit && compinit\n",
            path.parent().unwrap_or(&path).display()
        );
    }
    println!("the script asks `mm` for session names as you type, so it needs mm on your PATH");
    Ok(OK)
}

/// Where each shell looks for user-installed completions.
///
/// Deliberately not `dirs::data_dir()`: shells follow the XDG layout on macOS
/// too, where that would point at `~/Library/Application Support`.
fn completion_path(shell: Shell) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let base = |var: &str, fallback: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(fallback))
    };
    let data = base("XDG_DATA_HOME", ".local/share");
    let config = base("XDG_CONFIG_HOME", ".config");
    Some(match shell {
        Shell::Bash => data.join("bash-completion/completions/mm"),
        Shell::Zsh => data.join("zsh/site-functions/_mm"),
        Shell::Fish => config.join("fish/completions/mm.fish"),
        Shell::Elvish => config.join("elvish/lib/mm.elv"),
        // PowerShell loads completions from a profile script, not a directory.
        _ => return None,
    })
}
