use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::debug;

use manymux::client::attach::{self, Mode, Outcome};
use manymux::client::switch::{Cycle, Located};
use manymux::client::{Attached, Stream};
use manymux::hosts::{Hosts, LOCAL, Target, is_this_machine, this_machine};
use manymux::node::{Config, Node};
use manymux::proto::{HostedSession, Request, Response, SessionInfo, SpawnSpec};
use manymux::settings::{Screen, Settings};
use manymux::update::Channel;
use manymux::{config, log, style, term};

mod complete;
mod completions;

#[derive(Parser)]
#[command(
    name = "mm",
    version = manymux::VERSION,
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
    ///
    /// Hidden because nothing types it: the service unit runs it, and so does
    /// a client that found no node on the machine. `mm start`, `mm stop` and
    /// `mm restart` are how a person says the same things.
    #[command(alias = "server", hide = true)]
    Daemon {
        /// Watch for events but do not raise desktop notifications.
        #[arg(long)]
        no_notify: bool,
    },
    /// Bridge stdin and stdout to this machine's node, starting it if needed.
    ///
    /// This is what `ssh <host> mm agent` runs, and the only thing that ever
    /// does. Hidden for the same reason as `daemon`: it is the transport, not
    /// a command. Both still run when named, so ssh and the service unit are
    /// unaffected, and `mm agent --help` still explains itself.
    #[command(hide = true)]
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
        /// Whose screen to paint on. `alternate` gives the session a screen of
        /// its own; `inline` paints on the terminal's, so its scrollback keeps
        /// the session's history and the wheel scrolls it. Defaults to the
        /// `screen` setting.
        #[arg(long, value_enum)]
        screen: Option<Screen>,
        /// `[host] [command...]`. Defaults to your login shell, here.
        #[arg(trailing_var_arg = true, add = complete::new_args())]
        args: Vec<String>,
    },
    /// Attach to a session, as `name` or `host/name`.
    ///
    /// A machine's name on its own takes the first session on it, so `mm a
    /// gpu-box` is "put me on gpu-box" without looking up what is running
    /// there first.
    #[command(visible_alias = "a")]
    Attach {
        #[arg(add = complete::targets())]
        target: String,
        /// Whose screen to paint on. `alternate` gives the session a screen of
        /// its own; `inline` paints on the terminal's, so its scrollback keeps
        /// the session's history and the wheel scrolls it. Defaults to the
        /// `screen` setting.
        #[arg(long, value_enum)]
        screen: Option<Screen>,
    },
    /// Send SIGHUP to a session's process group.
    #[command(visible_alias = "k")]
    Kill {
        #[arg(add = complete::targets())]
        target: String,
    },
    /// Give a session a different name, the one it is addressed by.
    ///
    /// The title is the program's and stays the program's: it says what the
    /// session is doing, and the name says which session it is.
    #[command(visible_alias = "r")]
    Rename {
        #[arg(add = complete::targets())]
        target: String,
        name: String,
    },

    /// Put mm on a machine you can ssh into, by running the installer there.
    ///
    /// Nothing needs this: naming a machine that has no mm offers to do it.
    /// It is here for setting one up ahead of time, and for a script, which is
    /// never asked anything.
    Setup { host: String },

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

    /// Show or change a setting. With nothing to set, it prints.
    ///
    /// `mm config notify off` silences both ways a bell reaches you: the
    /// desktop notification a watching node raises, and the one an attached
    /// terminal is asked to show.
    Config {
        /// `notify` or `screen`. Left out, every setting is listed.
        #[arg(add = complete::settings())]
        key: Option<String>,
        /// What to set it to. Left out, the setting is printed rather than
        /// changed.
        #[arg(add = complete::setting_values())]
        value: Option<String>,
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
        /// Take the rolling nightly build instead of the newest release.
        ///
        /// Not remembered: a plain `mm update` afterwards puts the release
        /// back, which is a downgrade whenever master is ahead of the tag.
        #[arg(long)]
        nightly: bool,
    },

    /// Start this machine's node, if one is not already running.
    ///
    /// Nothing needs this: `mm new` and an incoming `mm agent` both start one
    /// on demand. It is here for when you want the node up before anything
    /// asks, and to have a name for the thing `stop` and `restart` act on.
    Start,

    /// Stop this machine's node. Every session on it dies with it.
    Stop {
        /// Stop it even though sessions are running.
        #[arg(long)]
        force: bool,
    },

    /// Restart this machine's node so it picks up the binary on disk.
    ///
    /// An update replaces the file; the node is a long-running process still
    /// executing the old one, and keeps answering like the old build until it
    /// is restarted. `mm update` does this for you when it downloads
    /// something, so this is for the times it did not: a binary installed some
    /// other way, or a restart deferred while sessions were running.
    Restart {
        /// Restart even though sessions are running. They die with the node.
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
    // blocking thread that cannot be cancelled: that thread is still parked in
    // read(2) with nobody about to type anything, and dropping the runtime
    // waits for it forever. `mm agent` is the one left relying on this, since
    // it relays stdin for as long as ssh holds the channel open; attaching
    // reads the keyboard on a thread of its own for the same reason.
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
            let signalled = Arc::clone(&node);
            tokio::spawn(async move { stop_when_signalled(signalled).await });
            node.serve(&socket).await?;
            Ok(OK)
        }

        Command::Agent => {
            manymux::node::agent(&socket).await?;
            Ok(OK)
        }

        Command::Ls { host } => list(&socket, host).await,

        Command::Setup { host } => {
            if is_this_machine(&host) {
                bail!("{host} is this machine, and mm is already on it");
            }
            manymux::ssh::install(&host).await?;
            Ok(OK)
        }

        Command::New {
            name,
            detached,
            screen,
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
            // A session started somewhere is proof that machine is up, and this
            // is the one path to it that never lists anything.
            if !is_this_machine(&host) {
                note_reached(&socket, vec![host.clone()]).await;
            }
            if detached {
                println!("{}", qualified(&host, &name));
                return Ok(OK);
            }
            do_attach(&socket, &host, &name, chosen(screen)).await
        }

        Command::Attach { target, screen } => {
            let target = locate(&socket, &target, Bare::OrMachine).await?;
            do_attach(&socket, &target.host, &target.session, chosen(screen)).await
        }

        Command::Kill { target } => {
            let target = locate(&socket, &target, Bare::Session).await?;
            let mut stream = open(&socket, &target.host).await?;
            stream
                .call(&Request::Kill {
                    name: target.session,
                })
                .await?;
            Ok(OK)
        }

        Command::Rename { target, name } => {
            let target = locate(&socket, &target, Bare::Session).await?;
            let mut stream = open(&socket, &target.host).await?;
            stream
                .call(&Request::Rename {
                    name: target.session,
                    to: name,
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

        Command::Config { key, value } => {
            let mut settings = Settings::load()?;
            match (key, value) {
                (None, _) => {
                    for (key, value) in settings.all() {
                        println!("{key} {value}");
                    }
                }
                (Some(key), None) => println!("{}", settings.get(&key)?),
                (Some(key), Some(value)) => {
                    settings.set(&key, &value)?;
                    settings.save()?;
                    // A node already running reads the file when it next has
                    // something to say, so there is nothing to restart.
                    println!("{key} {}", settings.get(&key)?);
                }
            }
            Ok(OK)
        }

        Command::Update {
            check,
            force,
            nightly,
        } => {
            let channel = if nightly {
                Channel::Nightly
            } else {
                Channel::Stable
            };
            update(&socket, check, force, channel).await
        }

        Command::Start => {
            if manymux::update::running(&socket).await.is_some() {
                println!("a node is already running");
                return Ok(OK);
            }
            manymux::node::ensure_running(&socket).await?;
            println!("{} started the node", style::green("✓"));
            Ok(OK)
        }

        Command::Stop { force } => {
            let Some(running) = manymux::update::running(&socket).await else {
                println!("no node is running");
                return Ok(OK);
            };
            if !agreed(running.sessions, force, "mm stop", "stopping it")? {
                return Ok(OK);
            }
            manymux::node::stop(&socket).await?;
            println!("{} stopped the node", style::green("✓"));
            Ok(OK)
        }

        Command::Restart { force } => {
            let Some(running) = manymux::update::running(&socket).await else {
                manymux::node::ensure_running(&socket).await?;
                println!("{} no node was running; started one", style::green("✓"));
                return Ok(OK);
            };
            restart(&socket, running.sessions, force, "mm restart").await
        }

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

        Command::Completions { shell, install } => completions::install(shell, install),
    }
}

/// Replace this binary with the published one, and pick it up.
async fn update(socket: &Path, check: bool, force: bool, channel: Channel) -> Result<u8> {
    let available = manymux::update::check(channel).await?;
    if available.is_newer() {
        if check {
            // The command to repeat, which is not always the channel that was
            // found: with no stable release yet, a plain `mm update` is what
            // lands the nightly.
            let command = match channel {
                Channel::Stable => "mm update",
                Channel::Nightly => "mm update --nightly",
            };
            println!(
                "an update is published on the {} channel; `{command}` to take it",
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
    } else {
        println!(
            "{} manymux {} is the published {} build",
            style::green("✓"),
            manymux::VERSION,
            available.tag
        );
    }

    // Whether anything was downloaded or not, for the same reason the node is
    // checked below: a machine whose binary is current can still be one that
    // has never had a completion script, and it would then have to wait for a
    // release it does not need before tab did anything. `--check` changes
    // nothing, here as everywhere.
    if !check {
        completions::refresh();
    }

    // Whether anything was downloaded or not. The node is a long-running
    // process still executing whatever it started from, so a machine whose
    // binary is current can still behave like the old build in every way that
    // matters, and saying "already current" and stopping there is how an
    // update looks finished while the machine is not.
    let Some(running) = manymux::update::running(socket).await else {
        return Ok(OK);
    };
    if !manymux::update::is_stale(socket, &available.checksum).await {
        return Ok(OK);
    }
    if check {
        println!(
            "{} the node is running an older build; `mm restart` to pick this one up",
            style::amber("!")
        );
        return Ok(OK);
    }
    restart(socket, running.sessions, force, "mm update").await
}

/// Restart the node, unless that would cost sessions the caller has not agreed
/// to lose. `command` is what to suggest `--force` on, since both `mm update`
/// and `mm restart` end up here.
async fn restart(socket: &Path, sessions: usize, force: bool, command: &str) -> Result<u8> {
    if !agreed(sessions, force, command, "restarting it")? {
        return Ok(OK);
    }

    manymux::node::restart(socket).await?;
    println!("{} restarted the node", style::green("✓"));
    Ok(OK)
}

/// Whether to go ahead with something that takes running sessions down.
///
/// The node owns the PTYs, so its sessions are its children and go when it
/// goes. Nothing here can tell whether the work in them is finished, so with
/// someone at the keyboard it asks them, and without one it refuses and says
/// which flag repeats the command without the question.
fn agreed(sessions: usize, force: bool, command: &str, doing: &str) -> Result<bool> {
    if sessions == 0 || force {
        return Ok(true);
    }
    let cost = format!(
        "{doing} would end {} running session{}",
        sessions,
        if sessions == 1 { "" } else { "s" }
    );
    match confirm(&format!("{cost}. Go ahead?"))? {
        Answer::Yes => Ok(true),
        Answer::No => {
            println!("left alone");
            Ok(false)
        }
        Answer::NobodyThere => {
            println!(
                "{} {cost}.\n  `{command} --force` to do it anyway, or leave it until they \
                 are done.",
                style::amber("!")
            );
            Ok(false)
        }
    }
}

enum Answer {
    Yes,
    No,
    /// Nothing on the other end of stdin to ask.
    NobodyThere,
}

/// Put a yes/no question to whoever is running this, defaulting to no.
///
/// A pipe is not a person: `ssh box mm update` from a script, or a service
/// unit, must not stop on a prompt nobody will ever see. Those get told what
/// the command would have cost and are left to decide with a flag.
fn confirm(question: &str) -> Result<Answer> {
    use std::io::{IsTerminal, Write};

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Ok(Answer::NobodyThere);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    stdin.read_line(&mut answer)?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Yes,
        _ => Answer::No,
    })
}

/// Open a stream to a machine: this one's socket, or `mm agent` over ssh.
async fn open(socket: &Path, host: &str) -> Result<Stream> {
    if is_this_machine(host) {
        return Stream::local(socket).await;
    }
    Stream::over_ssh(host, Some(Arc::new(offer_to_install))).await
}

/// Ask whether to put `mm` on a machine that turns out not to have it.
///
/// Reaching a machine you can ssh into should not need a separate trip to set
/// it up, but it does mean fetching a script onto someone else's box, so it is
/// asked rather than assumed. A pipe is not a person: a script gets the command
/// that would have done it and decides for itself.
fn offer_to_install(host: &str) -> bool {
    match confirm(&format!("{host} has no mm on it. Install it there?")) {
        Ok(Answer::Yes) => true,
        Ok(Answer::No) => false,
        Ok(Answer::NobodyThere) => {
            eprintln!("mm: {host} has no mm on it; `mm setup {host}` puts it there");
            false
        }
        Err(e) => {
            eprintln!("mm: {e:#}");
            false
        }
    }
}

/// End the node's sessions properly when something asks the process to go,
/// rather than by default action.
///
/// SIGTERM is how a service manager stops the unit, how a reboot starts, and
/// what `mm stop` falls back to against a node too old to answer a request.
/// SIGINT is whoever ran `mm daemon` in a terminal. Untreated, both take every
/// session down without a word to any of them; see [`Node::shutdown`] for what
/// that costs.
async fn stop_when_signalled(node: Arc<Node>) {
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        // Nothing to fall back to, and a node that cannot listen for a signal
        // is still a working node. It just goes the abrupt way when it goes.
        Err(e) => {
            debug!("cannot listen for SIGTERM: {e}");
            return;
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(interrupt) => interrupt,
        Err(e) => {
            debug!("cannot listen for SIGINT: {e}");
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => debug!("SIGTERM"),
        _ = interrupt.recv() => debug!("SIGINT"),
    }
    node.shutdown(manymux::node::GRACE).await;
    std::process::exit(0);
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
    /// Every machine that answered, whether or not it had anything to say. Kept
    /// separately because a machine with no sessions on it puts nothing in
    /// `sessions` and is every bit as reached as a busy one.
    answered: Vec<String>,
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
                self.answered.push(host.to_string());
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

    /// The machines worth telling this machine's node about, so it can
    /// resubscribe to any it had given up on. This machine is not one of them:
    /// it is never watched, being where the watching happens.
    fn reached(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .answered
            .iter()
            .filter(|host| !is_this_machine(host))
            .cloned()
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }
}

/// Tell this machine's node which machines just answered, so it can subscribe
/// again to any it had given up on.
///
/// Nothing here is worth failing a command over, so every way it can go wrong
/// is the same as it going right: no node running here (the common case on a
/// machine used only to reach others), a node too old to know the request, or a
/// socket that has gone. There is nothing to do about any of them, and nothing
/// the person who typed `mm ls` would want said about it.
async fn note_reached(socket: &Path, hosts: Vec<String>) {
    if hosts.is_empty() {
        return;
    }
    let told = async {
        Stream::local(socket)
            .await?
            .call(&Request::Reached { hosts })
            .await
    };
    if let Err(e) = told.await {
        debug!("could not tell the node what answered: {e:#}");
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
            note_reached(socket, listing.reached()).await;
            listing
        }
        // Reports for itself, since it is also what an attach and a bare
        // target go through.
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

/// How long one machine may take over a full listing before it is reported as
/// not answering.
///
/// Without this a single machine that is asleep or off the network holds up
/// every other machine's answer for as long as the kernel spends on a TCP
/// connect, which is minutes and once per address the name resolves to. Naming
/// a machine yourself still waits as long as it takes: you asked about that one
/// and an error about it is the answer. It is a fan-out that needs the bound.
///
/// Generous enough for a cold ssh handshake through a `ProxyCommand` and a node
/// starting at the far end, and short enough that a listing stays something you
/// wait for rather than something you go away from.
const HOST_DEADLINE: Duration = Duration::from_secs(5);

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
            // Giving up drops the query, and with it the ssh carrying it, so a
            // machine that never answers leaves nothing behind.
            let found = match timeout(HOST_DEADLINE, sessions_on(&socket, &host)).await {
                Ok(found) => found,
                Err(_) => Err(anyhow!("no answer in {}s", HOST_DEADLINE.as_secs())),
            };
            Asked { host, found }
        });
    }
    while let Some(answer) = asked.join_next().await {
        match answer {
            Ok(Asked { host, found }) => listing.add(&host, found),
            Err(e) => debug!("a host query did not finish: {e}"),
        }
    }
    note_reached(socket, listing.reached()).await;

    // By machine first, which is what makes each one's sessions a run in the
    // table and in the switch keys' cycle, then oldest first within it.
    listing.sessions.sort_by(|a, b| {
        (&a.host, a.session.started, &a.session.name).cmp(&(
            &b.host,
            b.session.started,
            &b.session.name,
        ))
    });
    listing.unreachable.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(listing)
}

async fn sessions_on(socket: &Path, host: &str) -> Result<Vec<SessionInfo>> {
    // No node here means no sessions here, which is an answer rather than a
    // failure. Starting one just to be told that would be rude.
    if is_this_machine(host) && !socket.exists() {
        // Unless the sessions are all sitting in a node an older build left
        // somewhere else, in which case an empty table is a lie.
        manymux::node::note_a_node_left_behind(socket).await;
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

/// The screen to paint on: what was asked for, else what the settings say.
///
/// A flag rather than only a setting because the choice is per attach: trying
/// inline costs nothing and going back is closing the window.
fn chosen(screen: Option<Screen>) -> Screen {
    screen.unwrap_or_else(|| Settings::or_default().screen)
}

/// Attach, and keep attaching for as long as the switch keys ask for another
/// session.
///
/// The terminal is held across the whole run rather than per session, so a hop
/// is a repaint and not a trip through the whole setup and back.
async fn do_attach(socket: &Path, host: &str, name: &str, screen: Screen) -> Result<u8> {
    let mut cycle = Cycle::new(Located::new(host, name));
    // Asked for before the first attach, so the first switch key has something
    // to go on.
    let mut listing = Some(spawn_listing(socket));

    let mut held = attach::hold(screen)?;
    let mut mode = Mode::Focus;
    let mut hopped = false;
    // Sessions whose history this run has already put in the terminal's
    // scrollback. Without this, walking the list with the switch key would dump
    // a thousand lines on every hop.
    let mut seeded: HashSet<String> = HashSet::new();
    let (outcome, where_) = loop {
        let target = cycle.current().clone();
        let mut where_ = qualified(&target.host, &target.session);
        let history = if seeded.insert(where_.clone()) {
            screen.mode().history()
        } else {
            0
        };
        let session = match attach_to(socket, &target, history).await {
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

        let (outcome, renamed) = attach::run(&mut held, session, &where_, mode).await?;
        // Renamed from inside, so the name this run has been using is nobody's
        // now: the cycle would hop to a session that is not there, and the line
        // printed on the way out would name it too.
        if let Some(name) = renamed {
            where_ = qualified(&target.host, &name);
            cycle.renamed(&name);
        }
        match outcome {
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
async fn attach_to(socket: &Path, target: &Located, history: u32) -> Result<Attached> {
    let stream = open(socket, &target.host).await?;
    stream
        .attach(&target.session, attach::session_size(), history)
        .await
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

/// What a bare word is allowed to mean besides a session name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bare {
    /// A session and nothing else. A word that happens to name a machine is a
    /// session name that is not running.
    Session,
    /// A machine too, standing for whatever is running on it. Only for going
    /// somewhere: picking a session out of a list is fine to attach to and not
    /// fine to kill or rename, where the wrong guess is not undoable.
    OrMachine,
}

/// Work out which machine a target is on.
///
/// `host/session` says so outright. A bare name is looked for here first, and
/// then across every watched machine, so `mm attach api` finds the session
/// wherever you left it without having to remember which machine that was.
async fn locate(socket: &Path, target: &str, bare: Bare) -> Result<Located> {
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
        .iter()
        .filter(|hosted| hosted.session.name == target.session)
        .map(|hosted| hosted.host.clone())
        .collect();
    hosts.dedup();

    match hosts.len() {
        1 => Ok(Located {
            host: hosts.remove(0),
            session: target.session,
        }),
        0 if bare == Bare::OrMachine && names_a_machine(&target.session) => {
            on_that_machine(&listing, &target.session)
        }
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

/// Whether a word names a machine rather than a session.
///
/// Only this machine and one already being watched count. A word nobody has
/// added is a mistyped session name, and treating it as a machine would turn
/// every typo into an ssh connection to a host that does not exist.
fn names_a_machine(word: &str) -> bool {
    is_this_machine(word) || Hosts::load().is_ok_and(|hosts| hosts.has(word))
}

/// The first session on a machine named on its own.
///
/// `mm a devbox` reads as "put me on devbox", and on a machine with one session
/// naming it as well is repeating a lookup you have just done. First is first as
/// `mm ls` prints it, so what you get is the row you were looking at.
fn on_that_machine(listing: &Listing, machine: &str) -> Result<Located> {
    let here = is_this_machine(machine);
    let found = listing
        .sessions
        .iter()
        .find(|hosted| hosted.host == machine || (here && is_this_machine(&hosted.host)));
    if let Some(hosted) = found {
        return Ok(Located {
            host: hosted.host.clone(),
            session: hosted.session.name.clone(),
        });
    }
    // A machine that could not be asked has not said it is empty, and saying it
    // is would send someone looking for sessions that are still there.
    if let Some(down) = listing.unreachable.iter().find(|had| had.host == machine) {
        bail!("{}: {}", down.host, down.error);
    }
    bail!("nothing is running on {machine}; `mm n {machine} <command>` starts something there")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            title: name.to_string(),
            command: "zsh".into(),
            pid: 1,
            size: manymux::proto::Size::new(80, 24),
            attached: 0,
            idle: 0,
            bells: 0,
            started: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// What the node is told answered, so it can resubscribe to a machine it
    /// gave up on. A machine that did not answer must not be in it, or giving
    /// up would be undone by the very listing that proved the host is still
    /// unreachable.
    #[test]
    fn only_the_machines_that_answered_count_as_reached() {
        let mut listing = Listing::default();
        listing.add(this_machine(), Ok(vec![session("here")]));
        listing.add("gpu-box", Ok(vec![session("build")]));
        // Reachable and idle. Nothing in the table, and still reached.
        listing.add("api", Ok(Vec::new()));
        listing.add("asleep", Err(anyhow!("no answer in 5s")));

        assert_eq!(listing.reached(), vec!["api", "gpu-box"]);
    }
}
