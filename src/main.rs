use std::collections::HashSet;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tracing::debug;

use manymux::client::attach::{self, Mode, Outcome, Wait};
use manymux::client::switch::{Cycle, Located};
use manymux::client::{Attached, Stream};
use manymux::hosts::{Hosts, is_this_machine, this_machine};
use manymux::node::{Config, Node};
use manymux::proto::{Request, Response, SpawnSpec};
use manymux::settings::{Screen, Settings};
use manymux::update::Channel;
use manymux::{config, log, style, term};

mod complete;
mod completions;
mod target;

use target::{
    Bare, Listing, Started, everywhere, locate, note_reached, qualified, sessions_on,
    where_to_start,
};

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
    /// Watch a session without being able to type into it.
    ///
    /// The same screen an attach gives you, scrolling and search included, with
    /// the keyboard going nowhere: the node drops this client's input rather
    /// than trusting it, so this is safe to point at a session somebody else is
    /// working in. It stays out of the size negotiation too, so watching from a
    /// narrow window cannot shrink the screen they are looking at.
    #[command(visible_alias = "v")]
    View {
        #[arg(add = complete::targets())]
        target: String,
        /// Whose screen to paint on, as for `attach`.
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
            do_attach(&socket, &host, &name, chosen(screen), false).await
        }

        Command::Attach { target, screen } => {
            let target = locate(&socket, &target, Bare::OrMachine).await?;
            do_attach(
                &socket,
                &target.host,
                &target.session,
                chosen(screen),
                false,
            )
            .await
        }

        Command::View { target, screen } => {
            let target = locate(&socket, &target, Bare::OrMachine).await?;
            do_attach(&socket, &target.host, &target.session, chosen(screen), true).await
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
async fn do_attach(
    socket: &Path,
    host: &str,
    name: &str,
    screen: Screen,
    watching: bool,
) -> Result<u8> {
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
    // Failed attempts to get back to a session the connection dropped under.
    // Zero whenever the last attach worked, which is what makes a connection
    // that comes back and goes again start over at the quick delays.
    let mut lost = 0;
    // Whether this run has been attached to anything yet, which is what tells
    // a command that did not work from a connection that went.
    let mut attached = false;
    // Something the last key has to say for itself, for the mark row of the
    // attach that follows it. Nothing else here has anywhere to say it: the
    // terminal between two attaches is showing a session.
    let mut notice: Option<String> = None;
    let (outcome, where_) = loop {
        let target = cycle.current().clone();
        let mut where_ = qualified(&target.host, &target.session);
        let history = if seeded.insert(where_.clone()) {
            screen.mode().history()
        } else {
            0
        };
        let session = match attach_to(socket, &target, history, watching).await {
            Ok(session) => session,
            // A hop onto a session that has gone since the listing was taken:
            // the machine answered and has no such session. Stay where you
            // were and put the dead entry out of the cycle, rather than
            // throwing away a working attach over it.
            Err(Missed::Gone(e)) if hopped => {
                debug!("could not attach to {where_}: {e:#}");
                cycle.forget(&target);
                cycle.undo();
                hopped = false;
                continue;
            }
            // Anything else, once this run has been attached to something, is
            // waited out and never reported. The machine being unreachable and
            // the session having gone with the node that held it look the same
            // from here, and both are worth another try: a node restarting is
            // a session that can come back under the same name. Which target
            // is waited for matters, and it is this one: a drop in the moment
            // after a hop must wait for the session hopped to, not walk back
            // to the one the command line named.
            Err(e) if attached => {
                debug!("could not get back to {where_}: {e:#}");
                match wait_to_reconnect(&mut held, &where_, &mut lost).await {
                    Wait::Retry => continue,
                    Wait::GiveUp => break (Outcome::Disconnected, where_),
                }
            }
            // The first attach of the run, which is a command that did not
            // work rather than a connection that went: `mm attach` naming a
            // session nobody is running says so and gives the shell back.
            Err(e) => return Err(e.into()),
        };
        lost = 0;
        attached = true;

        let (outcome, renamed) =
            attach::run(&mut held, session, &where_, mode, notice.take().as_deref()).await?;
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
            // A session on the machine you were just on, and straight into it.
            // The listing is asked for again rather than corrected here: the
            // node picked the name, and what the switch keys walk is what the
            // machines say they are running.
            Outcome::New => match start_beside(socket, &target.host).await {
                Ok(name) => {
                    cycle.moved_to(Located::new(&target.host, &name));
                    hopped = true;
                    listing = Some(spawn_listing(socket));
                    mode = Mode::Focus;
                }
                // Stay where you were and say so on the row. A key that does
                // nothing and says nothing reads as a broken client, and there
                // is no other surface to say it on: the terminal is about to
                // show the session this key was pressed in.
                Err(e) => {
                    debug!("could not start a session on {}: {e:#}", target.host);
                    notice = Some("could not start a session here".to_string());
                    mode = Mode::Focus;
                }
            },
            // The whole point of the project is that the session is still
            // running on a machine that never noticed you left, so a wifi hop
            // or a closed lid is not a reason to put somebody back at their
            // shell. The screen stays exactly as the session painted it and
            // the mark row says what is happening.
            Outcome::Disconnected => match wait_to_reconnect(&mut held, &where_, &mut lost).await {
                Wait::Retry => continue,
                Wait::GiveUp => break (Outcome::Disconnected, where_),
            },
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
        Outcome::Switch(_) | Outcome::New => {
            unreachable!("switches never leave the loop above")
        }
    }
}

/// Count one failed attempt at a lost session and sit out the delay it earns.
///
/// There is no attempt that ends the waiting: [`Wait::GiveUp`] comes from
/// somebody pressing something, which is the only thing that should end it.
/// The session is still running on a machine that never noticed anybody left.
async fn wait_to_reconnect(held: &mut attach::Held, where_: &str, lost: &mut u32) -> Wait {
    *lost = lost.saturating_add(1);
    attach::waiting(held, where_, attach::reconnect_after(*lost)).await
}

/// Why a session could not be attached to.
///
/// Which half of the trip failed, and the two mean opposite things: a machine
/// that never answered is one to wait for, while a node that answered and has
/// no such session is a listing that went stale under the switch keys.
enum Missed {
    Unreachable(anyhow::Error),
    Gone(anyhow::Error),
}

impl From<Missed> for anyhow::Error {
    fn from(missed: Missed) -> Self {
        match missed {
            Missed::Unreachable(e) | Missed::Gone(e) => e,
        }
    }
}

impl Display for Missed {
    /// For the log line, which wants the reason and not the reading of it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Missed::Unreachable(e) | Missed::Gone(e) => write!(f, "{e:#}"),
        }
    }
}

/// Open a stream to wherever a session is, and attach to it.
async fn attach_to(
    socket: &Path,
    target: &Located,
    history: u32,
    watching: bool,
) -> Result<Attached, Missed> {
    let stream = open(socket, &target.host)
        .await
        .map_err(Missed::Unreachable)?;
    stream
        .attach(&target.session, attach::session_size(), history, watching)
        .await
        .map_err(Missed::Gone)
}

/// Start a session on the machine a client is already sitting on, for the
/// control key that asks for one.
///
/// `open` rather than `open_or_start`: there is a node on that machine and
/// this client is attached to it. A key pressed inside a session is also no
/// place to be asking anybody for consent to install anything.
async fn start_beside(socket: &Path, host: &str) -> Result<String> {
    let spec = SpawnSpec {
        // The node's counter names it, the way it does for any spawn without
        // one: there is no prompt here and nobody typed anything.
        name: None,
        // The login shell.
        command: Vec::new(),
        // Only meaningful on this machine; elsewhere the session starts in the
        // node's own working directory. The rule `mm new` follows.
        cwd: is_this_machine(host).then(current_dir).flatten(),
        size: attach::session_size(),
    };
    let mut stream = open(socket, host).await?;
    let Response::Spawned { name } = stream.call(&Request::Spawn(spec)).await? else {
        bail!("unexpected response to spawn");
    };
    Ok(name)
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
