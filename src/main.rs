use std::collections::HashSet;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tracing::debug;

use manymux::client::attach::{self, Chose, Mode, Motion, Outcome, Rows, Wait};
use manymux::client::groups::Groups;
use manymux::client::picker::Row;
use manymux::client::switch::{Cycle, Located};
use manymux::client::{Attached, Stream};
use manymux::hosts::{Hosts, is_this_machine, this_machine};
use manymux::lock::held as held_lock;
use manymux::node::{Config, Node};
use manymux::proto::{HostedSession, Request, Response, SpawnSpec};
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

    /// Put a session in a group, or take it out of one.
    ///
    /// A group is a subset of sessions you can narrow the client to, and it
    /// spans machines. It lives here rather than at the node, so it works
    /// against a machine running any build of mm. With no name, the session is
    /// taken out of whatever it is in.
    Group {
        #[arg(add = complete::targets())]
        target: String,
        /// The group. Omit to ungroup.
        #[arg(add = complete::group_names())]
        name: Option<String>,
    },
    /// List the groups and what is in them.
    Groups,

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

        Command::Group { target, name } => {
            let at = locate(&socket, &target, Bare::Session).await?;
            // The pid and the start time are what membership is keyed on, and
            // only the machine holding the session knows them.
            let sessions = sessions_on(&socket, &at.host).await?;
            let session = sessions
                .iter()
                .find(|s| s.name == at.session)
                .ok_or_else(|| anyhow!("no session named {} on {}", at.session, at.host))?;
            let mut groups = Groups::load()?;
            match &name {
                Some(name) => groups.assign(name, &at.host, session),
                None => groups.clear(&at.host, session),
            }
            groups.save()?;
            Ok(OK)
        }

        Command::Groups => {
            let listing = everywhere(&socket).await?;
            let mut groups = Groups::load()?;
            // The listing is the only thing that says which sessions are still
            // running, so this is where the file gets tidied.
            groups.prune(&listing.reached(), &listing.sessions);
            groups.save()?;
            for (name, count) in groups.tally(&listing.sessions) {
                println!(
                    "{} {}",
                    style::bold(&name),
                    style::faint(&format!("({count})"))
                );
                for hosted in &listing.sessions {
                    if groups.group_of(&hosted.host, &hosted.session) == Some(name.as_str()) {
                        println!("  {}", qualified(&hosted.host, &hosted.session.name));
                    }
                }
            }
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

    // The listing is the only thing that says which sessions are still running,
    // so this is where the group file gets tidied. Only the machines that
    // answered count: one that is asleep has said nothing about its sessions.
    let mut groups = Groups::load().unwrap_or_default();
    groups.prune(&listing.reached(), &listing.sessions);
    let _ = groups.save();

    let rows: Vec<_> = listing
        .sessions
        .into_iter()
        .map(|hosted| term::SessionRow {
            group: groups
                .group_of(&hosted.host, &hosted.session)
                .map(str::to_string),
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

/// Attach, and keep attaching: for as long as the switch keys ask for another
/// session, and for as long as one that ends has another to hand back to.
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
    // What the machines last said, kept for the pids a group is keyed on.
    let mut snapshot = Snapshot::default();
    let mut groups = Groups::load().unwrap_or_default();
    // Landing in a session that is in a group is landing in that work, so the
    // switch keys should walk that work. Read once here rather than derived
    // from the current session, because widening out of a group you are still
    // sitting in has to be possible.
    let mut settled = false;
    // Where the listing task leaves its answer, so the ids the popup hands back
    // are looked up in the rows it was actually showing.
    let latest: Arc<Mutex<Option<Listed>>> = Arc::new(Mutex::new(None));

    let mut held = attach::hold(screen)?;
    let mut mode = Mode::Focus;
    let mut hopped = false;
    // Sessions whose history this run has already put in the terminal's
    // scrollback. Without this, walking the list with the switch key would dump
    // a thousand lines on every hop.
    let mut seeded: HashSet<String> = HashSet::new();
    // The connection this run is without, while it is without one. Cleared
    // whenever an attach works, which is what makes one that comes back and
    // goes again start over at the quick delays and at a fresh clock.
    let mut lost: Option<Lost> = None;
    // Whether this run has been attached to anything yet, which is what tells
    // a command that did not work from a connection that went.
    let mut attached = false;
    // Something the last key has to say for itself, for the mark row of the
    // attach that follows it. Nothing else here has anywhere to say it: the
    // terminal between two attaches is showing a session.
    let mut notice: Option<String> = None;
    // The exit this run is carrying while it goes back to the session it came
    // from, kept because that fall-back can fail: a session that ended is the
    // end of the run after all if there is no longer anywhere to land.
    let mut ended: Option<(Outcome, String)> = None;
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
            // The way back out of a session that ended is gone too, and there
            // is nothing further back than the one you came from. So the run
            // is over after all, and what it has to report is the exit it was
            // carrying rather than this.
            Err(Missed::Gone(e)) if ended.is_some() => {
                debug!("could not fall back to {where_}: {e:#}");
                break ended.take().expect("carrying an exit, by the guard");
            }
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
                match wait_to_reconnect(&mut held, &where_, &mut lost, &mut mode).await {
                    Wait::Retry => continue,
                    Wait::GiveUp => break (Outcome::Disconnected, where_),
                }
            }
            // The first attach of the run, which is a command that did not
            // work rather than a connection that went: `mm attach` naming a
            // session nobody is running says so and gives the shell back.
            Err(e) => return Err(e.into()),
        };
        lost = None;
        attached = true;
        // Landed, so the exit that sent us here is somebody else's news now:
        // the next one to report is whatever this session does.
        ended = None;

        // Before the rows are built, because the popup is drawn from them and
        // it opens on a keystroke inside the attach, where there is no asking
        // anything. The wait is the same half second the switch keys allow and
        // for the same reason: what is already known stands rather than being
        // waited on, since a machine that is asleep must not hold up a screen.
        take_listing(&mut listing, &mut cycle, &mut snapshot, &mut groups).await;
        if !settled && !snapshot.is_empty() {
            settled = true;
            if let Some(info) = snapshot.info(cycle.current()) {
                let group = groups
                    .group_of(&cycle.current().host, info)
                    .map(str::to_string);
                cycle.focus(group);
            }
        }
        let listed = Listed::new(&snapshot, &groups, &cycle);
        // The popup's way of asking what the machines are running, since the
        // attach owns this task for as long as it lasts and cannot go and look
        // itself. Groups and the narrowing cannot change under it: every way of
        // changing either ends the attach, which is what makes a snapshot of
        // them safe to hand over.
        let (asks, mut wanted) = tokio::sync::mpsc::channel::<()>(1);
        let (answered, fresh) = tokio::sync::watch::channel(listed.rows.clone());
        let lister = {
            let socket = socket.to_path_buf();
            let groups = groups.clone();
            let focus = cycle.focused().map(str::to_string);
            let current = cycle.current().clone();
            let latest = Arc::clone(&latest);
            tokio::spawn(async move {
                while wanted.recv().await.is_some() {
                    let Ok(listing) = everywhere(&socket).await else {
                        continue;
                    };
                    let snapshot = Snapshot {
                        reached: listing.reached(),
                        sessions: listing.sessions,
                    };
                    let listed = Listed::of(&snapshot, &groups, focus.as_deref(), &current);
                    // Kept where the caller can read it once the attach ends:
                    // the ids the popup hands back are this listing's, and
                    // looking them up in the one it opened with would name the
                    // wrong session.
                    *held_lock(&latest) = Some(listed.clone());
                    if answered.send(listed.rows).is_err() {
                        return;
                    }
                }
            })
        };
        let (outcome, renamed) = attach::run(
            &mut held,
            session,
            &where_,
            mode,
            notice.take().as_deref(),
            attach::PopupFeed {
                rows: listed.rows.clone(),
                group: cycle.focused(),
                asks,
                fresh,
            },
        )
        .await?;
        lister.abort();
        // The rows the ids in `outcome` belong to: the last answer if one
        // landed while the popup was up, else the ones it opened with.
        let listed = held_lock(&latest).take().unwrap_or(listed);
        // Renamed from inside, so the name this run has been using is nobody's
        // now: the cycle would hop to a session that is not there, and the line
        // printed on the way out would name it too.
        if let Some(name) = renamed {
            where_ = qualified(&target.host, &name);
            cycle.renamed(&name);
        }
        match outcome {
            // Everything the popup chose, in its own row ids. The lookups are
            // here because this is the half that knows what a row means: the
            // popup is handed rows and hands back ids, and never learns what a
            // host, a group or a pid is.
            Outcome::Chose(chose) => {
                hopped = false;
                match chose {
                    // A hop like any other, so the way back is recorded the
                    // same way and `Motion::Last` reaches it.
                    Chose::Go(row) => {
                        if let Some(there) = listed.at.get(row)
                            && *there != target
                        {
                            cycle.moved_to(there.clone());
                            hopped = true;
                        }
                    }
                    // Narrowing. You move only when the session you are in is
                    // not in the group you just chose, since widening or
                    // choosing the group you are already in should leave you
                    // where you are.
                    Chose::Focus(row) => {
                        let group = listed.groups.get(row).cloned().flatten();
                        cycle.focus(group);
                        if let Some(next) = cycle.step(Motion::Next)
                            && !cycle.holds(&target)
                        {
                            cycle.moved_to(next);
                            hopped = true;
                        }
                    }
                    // A local file write and a redraw: nothing detaches for
                    // this, which is why `m` acts on the highlighted row rather
                    // than the session you are attached to.
                    Chose::Move { session, group } => {
                        let group = listed.groups.get(group).cloned().flatten();
                        notice = regroup(socket, &listed, session, group, &mut groups)
                            .await
                            .err()
                            .map(|e| format!("{e:#}"));
                        cycle.refresh(entries(&snapshot, &groups));
                    }
                    Chose::NewGroup { session, name } => {
                        notice = regroup(socket, &listed, session, Some(name), &mut groups)
                            .await
                            .err()
                            .map(|e| format!("{e:#}"));
                        cycle.refresh(entries(&snapshot, &groups));
                    }
                    // The row may not be the session at the other end of the
                    // attach stream, and `tag::RENAME` renames that one by
                    // design, so this goes over a connection to its machine.
                    Chose::Named { session, to } => {
                        let at = listed.at.get(session);
                        let pid = at.and_then(|at| snapshot.info(at)).map(|i| i.pid);
                        match rename_at(socket, at, pid.unwrap_or_default(), &to).await {
                            Ok(name) => {
                                if let Some(at) = at
                                    && *at == target
                                {
                                    where_ = qualified(&at.host, &name);
                                    cycle.renamed(&name);
                                }
                            }
                            Err(e) => notice = Some(format!("{e:#}")),
                        }
                    }
                }
                // Whatever it was, what the machines are running may have moved
                // under it, and a press is the only thing that ever asks.
                listing = Some(spawn_listing(socket));
                mode = Mode::Focus;
            }
            Outcome::Switch(motion) => {
                take_listing(&mut listing, &mut cycle, &mut snapshot, &mut groups).await;
                hopped = false;
                if let Some(next) = cycle.step(motion) {
                    cycle.moved_to(next);
                    hopped = true;
                }
                // Whether or not it landed, and this is the whole reason the
                // key that goes nowhere is worth anything: a press is the only
                // thing that ever asks, so one that found nowhere to go and did
                // not ask again was the last one that ever asked. A machine
                // with one session on it when the run started stayed a machine
                // with one session on it as far as these keys were concerned,
                // however many were started beside it afterwards. Asked rather
                // than waited on, because the press before it has already
                // waited: `take_listing` gives a listing half a second and no
                // more, since a keystroke must not sit on a machine that is
                // asleep.
                if listing.is_none() {
                    listing = Some(spawn_listing(socket));
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
            // A session this run put you in has ended, so the run has not:
            // back to the one you came from, the way a hop goes. What ended
            // is said on the row you land on, because the line that says it
            // on the way out is only printed by a run that is over, and a
            // screen that changes under somebody with nothing said about it
            // reads as a client that lost its place.
            Outcome::Exited(code) => {
                // The name and not the whole target, because the row it goes
                // on is narrow and drops a notice that does not fit outright,
                // and because the machine is the one named two columns to its
                // right in all but the rarest fall-back.
                let name = cycle.current().session.clone();
                match cycle.fall_back() {
                    Some(_) => {
                        hopped = false;
                        listing = Some(spawn_listing(socket));
                        notice = Some(format!("{name} exited with status {code}"));
                        ended = Some((Outcome::Exited(code), where_));
                        mode = Mode::Focus;
                    }
                    // Nowhere to fall back to, so this is the session somebody
                    // named on the command line: the command is over and the
                    // status it ended with is the answer.
                    None => break (Outcome::Exited(code), where_),
                }
            }
            // The whole point of the project is that the session is still
            // running on a machine that never noticed you left, so a wifi hop
            // or a closed lid is not a reason to put somebody back at their
            // shell. The screen stays exactly as the session painted it and
            // the mark row says what is happening.
            Outcome::Disconnected => {
                match wait_to_reconnect(&mut held, &where_, &mut lost, &mut mode).await {
                    Wait::Retry => continue,
                    Wait::GiveUp => break (Outcome::Disconnected, where_),
                }
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
        Outcome::Switch(_) | Outcome::New | Outcome::Chose(_) => {
            unreachable!("switches never leave the loop above")
        }
    }
}

/// Count one failed attempt at a lost session and sit out the delay it earns.
///
/// There is no attempt that ends the waiting: [`Wait::GiveUp`] comes from
/// somebody pressing something, which is the only thing that should end it.
/// The session is still running on a machine that never noticed anybody left.
///
/// The keyboard goes back to the session's here, and this is the only place
/// that does it. Control mode belongs to the hop that turned it on, which is
/// why it survives a reattach at all: what follows a hop is usually another
/// one. A wait is not a hop. Nobody pressed anything, the row has been saying
/// the connection went rather than what the keys do, and somebody coming back
/// to it presses the mode key meaning to *enter* control mode; pressed while
/// it is already on, that key leaves it, and the `tab` after it lands in their
/// shell. Left set, the mode outlived every hop for the rest of the run,
/// because nothing else here ever turned it off again.
async fn wait_to_reconnect(
    held: &mut attach::Held,
    where_: &str,
    lost: &mut Option<Lost>,
    mode: &mut Mode,
) -> Wait {
    *mode = Mode::Focus;
    let lost = lost.get_or_insert_with(Lost::new);
    lost.attempts = lost.attempts.saturating_add(1);
    attach::waiting(
        held,
        where_,
        attach::reconnect_after(lost.attempts),
        lost.since.elapsed(),
    )
    .await
}

/// A connection that has gone, for as long as it is gone.
struct Lost {
    /// When it went, which is what the mark row counts from: a screen somebody
    /// walks up to says how long it has been sitting there, and after the first
    /// few tries the delay says nothing about that at all.
    since: Instant,
    /// Attempts to get back that have failed, which is what the delay grows
    /// with.
    attempts: u32,
}

impl Lost {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            attempts: 0,
        }
    }
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

/// What every machine said it was running, and which machines said anything.
///
/// The whole `SessionInfo` rather than a name, because group membership is
/// keyed on the pid and the start time and the popup draws titles, idle times
/// and bells from it. The hosts that answered come with it because pruning a
/// group must consider only those: a machine that is asleep has said nothing
/// about its sessions, and reading that silence as "they ended" would empty its
/// groups while you were away from it.
#[derive(Default)]
struct Snapshot {
    sessions: Vec<manymux::proto::HostedSession>,
    reached: Vec<String>,
}

impl Snapshot {
    /// What a machine said about one session, for the pid a group is keyed on.
    fn info(&self, at: &Located) -> Option<&manymux::proto::SessionInfo> {
        self.sessions
            .iter()
            .find(|hosted| hosted.host == at.host && hosted.session.name == at.session)
            .map(|hosted| &hosted.session)
    }

    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// The popup's rows, and what each row id means.
///
/// The ids are indices into the two `Vec`s beside the rows, so the half of the
/// client that draws the popup never learns what a host, a group or a pid is:
/// it hands back an id and this looks it up. Built together and replaced
/// together, or an id would index the wrong session.
#[derive(Clone, Default)]
struct Listed {
    rows: Rows,
    /// One per session row, in the same order.
    at: Vec<Located>,
    /// One per group row. The first is `None`, which is "everything" when you
    /// are narrowing and "no group" when you are moving a session.
    groups: Vec<Option<String>>,
}

impl Listed {
    /// A tree two deep: a heading per machine, the groups on that machine under
    /// it with their sessions inside, and whatever is in no group last.
    ///
    /// Within every run the order is the listing's, which is oldest first and
    /// never by name, for the reason every other listing has it: a name moves
    /// under a rename and the rows would shuffle beneath a hand walking them.
    /// A group's place is where its oldest session falls, which rests on the
    /// same thing.
    ///
    /// Narrowed to the focused group when there is one, because that is what
    /// every other way of moving around is narrowed to.
    fn new(snapshot: &Snapshot, groups: &Groups, cycle: &Cycle) -> Self {
        Self::of(snapshot, groups, cycle.focused(), cycle.current())
    }

    /// The same, from the two things about the cycle that matter, so a task
    /// with no cycle of its own can build these while an attach is up.
    fn of(snapshot: &Snapshot, groups: &Groups, focus: Option<&str>, current: &Located) -> Self {
        let mut rows = Vec::new();
        let mut at = Vec::new();
        // Every session that is going to be drawn, each beside the group it is
        // in, still in the order the listing arrived: by machine, then oldest
        // first. Which makes each machine's sessions a run, and the run is what
        // the tree is built out of.
        let showing: Vec<(&HostedSession, Option<&str>)> = snapshot
            .sessions
            .iter()
            .map(|hosted| (hosted, groups.group_of(&hosted.host, &hosted.session)))
            .filter(|(_, group)| focus.is_none_or(|focus| *group == Some(focus)))
            .collect();

        let mut rest = showing.as_slice();
        while let Some((first, _)) = rest.first() {
            let machine = first.host.as_str();
            let (run, left) = rest.split_at(rest.partition_point(|(h, _)| h.host == machine));
            rest = left;
            rows.push(Row::heading(machine));

            // The groups on this machine first, each with its sessions under
            // it, in the order their first session appears: a group's place in
            // the list moves only when the oldest session in it ends, which is
            // the same thing the sessions' own order rests on.
            let mut drawn: Vec<&str> = Vec::new();
            for (_, group) in run {
                let Some(group) = *group else { continue };
                if drawn.contains(&group) {
                    continue;
                }
                drawn.push(group);
                rows.push(Row::subheading(group));
                for (hosted, _) in run.iter().filter(|(_, g)| *g == Some(group)) {
                    Self::session(&mut rows, &mut at, hosted, current, 2);
                }
            }
            // Then whatever is in no group, last and a step in, where it reads
            // as the rest of the machine rather than as another group. A
            // heading of its own would be a group called "no group", which is
            // the one thing a group is not.
            for (hosted, _) in run.iter().filter(|(_, group)| group.is_none()) {
                Self::session(&mut rows, &mut at, hosted, current, 1);
            }
        }
        let landed = current.clone();
        let on = at.iter().position(|there| *there == landed).unwrap_or(0);
        let highlight = rows
            .iter()
            .position(|row| !row.heading && row.id == on)
            .unwrap_or(0);

        // The first row is "everything" or "no group" depending on which verb
        // opened the list, which is why it carries no name.
        let mut group_rows = vec![Row::new(0, "(none)")];
        let mut names: Vec<Option<String>> = vec![None];
        let held = groups.tally(&snapshot.sessions);
        for (name, count) in held {
            let mark = if focus == Some(name.as_str()) {
                "●"
            } else {
                ""
            };
            group_rows.push(
                Row::new(names.len(), &name)
                    .detail(count.to_string())
                    .note(mark),
            );
            names.push(Some(name));
        }

        Self {
            rows: Rows {
                sessions: rows,
                groups: group_rows,
                at: highlight,
            },
            at,
            groups: names,
        }
    }

    /// One session row, and the address that goes with it.
    ///
    /// The two are pushed together and never apart: the row's id is its index
    /// in `at`, which is how the popup can hand back a row without ever being
    /// told what a machine is.
    fn session(
        rows: &mut Vec<Row>,
        at: &mut Vec<Located>,
        hosted: &HostedSession,
        current: &Located,
        indent: u16,
    ) {
        let here = *current == Located::new(&hosted.host, &hosted.session.name);
        let mut note = term::duration(hosted.session.idle);
        if hosted.session.bells > 0 {
            note = format!("{note} *");
        }
        if here {
            note = "●".to_string();
        }
        rows.push(
            Row::new(at.len(), &hosted.session.name)
                .detail(&hosted.session.title)
                .note(note)
                .indent(indent),
        );
        at.push(Located::new(&hosted.host, &hosted.session.name));
    }
}

/// Put the session on `row` in a group, or take it out of one.
///
/// The pid and the start time membership is keyed on come from the snapshot the
/// popup was drawn from, and that snapshot can be a few seconds old. One
/// `sessions_on` to the machine holding it refreshes them first, because a
/// stale pid puts the wrong session in a group, which is worse than a round
/// trip on a key nobody presses in a hurry.
async fn regroup(
    socket: &Path,
    listed: &Listed,
    row: usize,
    group: Option<String>,
    groups: &mut Groups,
) -> Result<()> {
    let at = listed
        .at
        .get(row)
        .ok_or_else(|| anyhow!("that session is no longer listed"))?;
    let sessions = sessions_on(socket, &at.host).await?;
    let session = sessions
        .iter()
        .find(|s| s.name == at.session)
        .ok_or_else(|| anyhow!("{} is no longer running", at.session))?;
    match &group {
        Some(name) => groups.assign(name, &at.host, session),
        None => groups.clear(&at.host, session),
    }
    groups.save()
}

/// Rename the session on a popup row, wherever it is running, and answer with
/// the name that stuck.
///
/// `Request::Rename` answers `Ok` and throws that name away, and it cannot be
/// made to say more without a node old enough to be out there failing to
/// decode it. So the session is looked up again afterwards by the one thing a
/// rename does not move: its pid. What was typed and what it ended up called
/// are not always the same string, since the node sanitises it, and the mark
/// row is about to draw one of them.
async fn rename_at(socket: &Path, at: Option<&Located>, pid: u32, to: &str) -> Result<String> {
    let at = at.ok_or_else(|| anyhow!("that session is no longer listed"))?;
    open(socket, &at.host)
        .await?
        .call(&Request::Rename {
            name: at.session.clone(),
            to: to.to_string(),
        })
        .await?;
    let sessions = sessions_on(socket, &at.host).await?;
    Ok(sessions
        .iter()
        .find(|s| s.pid == pid)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| to.to_string()))
}

/// The listing as the cycle wants it: an address, and the group it is in.
fn entries(snapshot: &Snapshot, groups: &Groups) -> Vec<manymux::client::switch::Entry> {
    snapshot
        .sessions
        .iter()
        .map(|hosted| {
            manymux::client::switch::Entry::new(
                Located::new(&hosted.host, &hosted.session.name),
                groups
                    .group_of(&hosted.host, &hosted.session)
                    .map(str::to_string),
            )
        })
        .collect()
}

/// Ask every machine what it is running, off to one side, so that no keystroke
/// waits on ssh.
fn spawn_listing(socket: &Path) -> JoinHandle<Snapshot> {
    let socket = socket.to_path_buf();
    tokio::spawn(async move {
        match everywhere(&socket).await {
            Ok(listing) => Snapshot {
                reached: listing.reached(),
                sessions: listing.sessions,
            },
            Err(e) => {
                debug!("could not list sessions for the switch keys: {e:#}");
                Snapshot::default()
            }
        }
    })
}

/// Hand the cycle the listing, if one has arrived or arrives shortly, and keep
/// it for the popup to draw from.
///
/// What is already known stands rather than being replaced by nothing, and a
/// listing still out there asking is left running rather than thrown away.
///
/// This is also where the group file gets tidied: the listing is the only thing
/// that says which sessions are still running, and only the machines that
/// answered count towards it.
async fn take_listing(
    pending: &mut Option<JoinHandle<Snapshot>>,
    cycle: &mut Cycle,
    snapshot: &mut Snapshot,
    groups: &mut Groups,
) {
    let Some(mut task) = pending.take() else {
        return;
    };
    match tokio::time::timeout(LISTING_WAIT, &mut task).await {
        Ok(Ok(landed)) => {
            if !landed.is_empty() {
                groups.prune(&landed.reached, &landed.sessions);
                let _ = groups.save();
                *snapshot = landed;
                cycle.refresh(entries(snapshot, groups));
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
