use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::future::Future;
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
use manymux::client::checkpoint::{self, Checkpoint, Kept};
use manymux::client::groups::Groups;
use manymux::client::picker::Row;
use manymux::client::switch::{Cycle, Located};
use manymux::client::{Attached, Stream};
use manymux::hosts::{Hosts, LOCAL, is_this_machine, this_machine};
use manymux::lock::held as held_lock;
use manymux::node::{Config, Node};
use manymux::proto::{Doing, HostedSession, Request, Response, SpawnSpec};
use manymux::settings::{Screen, Settings};
use manymux::update::Channel;
use manymux::{config, log, shell, style, term};

mod complete;
mod completions;
mod target;

use target::{
    Bare, Listing, Started, Unreachable, everywhere, locate, note_reached, qualified, sessions_on,
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

    /// Write down what every session is doing, so it can be started again.
    ///
    /// The problem it answers: a node keeps executing the build it started
    /// from, so `mm update` only takes effect when the node restarts, and a
    /// restart is the end of every session on that machine. `mm checkpoint
    /// save` records where each session is and what is running in it, and
    /// `mm checkpoint restore` starts them again afterwards, resuming a
    /// program that knows how to be resumed rather than starting it fresh.
    ///
    /// Take it from the machine you work at rather than from the machine being
    /// updated, when you can. The sessions come back either way, but which
    /// group each one was in is known only to the client that holds the
    /// groups, and that is the one at your desk.
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
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
        /// `notify`, `screen` or `mouse`. Left out, every setting is listed.
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
        /// Write a checkpoint first and put the sessions back afterwards.
        ///
        /// The same three commands by hand, and worth knowing as three: `mm
        /// checkpoint save`, the restart, `mm checkpoint restore`. Nothing is
        /// restarted unless every session was written down, so this never
        /// costs work it could not record. What it cannot carry is which
        /// group each session was in, unless this machine is the one holding
        /// your groups: take the checkpoint from your own machine for that.
        #[arg(long)]
        keep_sessions: bool,
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
        /// Write a checkpoint first and put the sessions back afterwards, as
        /// for `mm update`.
        #[arg(long)]
        keep_sessions: bool,
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

#[derive(Subcommand)]
enum CheckpointAction {
    /// Write down every session: its name, its group, where it is and what it
    /// is running.
    ///
    /// Replaces what it asked about and leaves the rest of the file alone, so
    /// narrowing to one machine does not throw away what is known about the
    /// others. A machine that cannot say where its sessions are is named
    /// rather than guessed at, and its sessions are left out: put back in the
    /// wrong directory, a program told to resume picks up somebody else's
    /// work.
    Save {
        /// Only this machine, or only that one.
        #[arg(long, add = complete::hosts_or_local())]
        host: Option<String>,
    },
    /// Start the saved sessions again, and put them back in their groups.
    ///
    /// A name that is already running is left alone, so this is safe to run
    /// twice and safe to run after the machine put its own sessions back.
    Restore {
        /// Only this machine, or only that one.
        #[arg(long, add = complete::hosts_or_local())]
        host: Option<String>,
        /// Say what would be started, without starting it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print what is saved, without contacting any machine.
    Show,
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
                // The command is the command: nothing here is a wrapper.
                label: None,
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
            Groups::change(|groups| {
                match &name {
                    Some(name) => {
                        groups.assign(name, &at.host, session)?;
                    }
                    None => groups.clear(&at.host, session),
                }
                Ok(())
            })?;
            Ok(OK)
        }

        Command::Groups => {
            let listing = everywhere(&socket).await?;
            let mut groups = Groups::default();
            // The listing is the only thing that says which sessions are still
            // running, so this is where the file gets tidied.
            groups.update(|groups| {
                groups.prune(&listing.answering(), &listing.sessions);
                Ok(())
            })?;
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

        Command::Checkpoint { action } => match action {
            CheckpointAction::Save { host } => save_checkpoint(&socket, host)
                .await
                .map(|saved| saved.code()),
            CheckpointAction::Restore { host, dry_run } => {
                restore_checkpoint(&socket, host, dry_run).await
            }
            CheckpointAction::Show => show_checkpoint(),
        },

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
            keep_sessions,
        } => {
            let channel = if nightly {
                Channel::Nightly
            } else {
                Channel::Stable
            };
            update(&socket, check, force, keep_sessions, channel).await
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
            if !agreed(running.sessions, force, false, "mm stop", "stopping it")? {
                return Ok(OK);
            }
            manymux::node::stop(&socket).await?;
            println!("{} stopped the node", style::green("✓"));
            Ok(OK)
        }

        Command::Restart {
            force,
            keep_sessions,
        } => {
            let Some(running) = manymux::update::running(&socket).await else {
                manymux::node::ensure_running(&socket).await?;
                println!("{} no node was running; started one", style::green("✓"));
                return Ok(OK);
            };
            restart(
                &socket,
                running.sessions,
                force,
                keep_sessions,
                "mm restart",
            )
            .await
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
async fn update(
    socket: &Path,
    check: bool,
    force: bool,
    keep: bool,
    channel: Channel,
) -> Result<u8> {
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
    restart(socket, running.sessions, force, keep, "mm update").await
}

/// Restart the node, unless that would cost sessions the caller has not agreed
/// to lose. `command` is what to suggest `--force` on, since both `mm update`
/// and `mm restart` end up here.
///
/// `keep` writes a checkpoint first and puts the sessions back afterwards,
/// which is the same three commands somebody would type. It answers the
/// question [`agreed`] asks rather than overriding it: the sessions are not
/// being lost, they are being written down. What it must not do is answer that
/// question on a checkpoint that did not cover them, so a save that could not
/// account for every session stops the restart rather than proceeding on a
/// record with holes in it.
async fn restart(
    socket: &Path,
    sessions: usize,
    force: bool,
    keep: bool,
    command: &str,
) -> Result<u8> {
    let putting_back = keep && sessions > 0;
    if putting_back {
        if let Some(ours) = ran_from_a_session_here(socket).await {
            eprintln!(
                "{} `{command} --keep-sessions` cannot run from inside a session on this \
                 machine.\n  This was typed in {}, and the restart hangs that session up \
                 before anything can be put back: the checkpoint would be written and then \
                 nothing would read it.\n  Run it from a shell that is not a session, such \
                 as a plain `ssh` in, and everything here comes back.",
                style::amber("!"),
                style::bold(&ours)
            );
            return Ok(FAILED);
        }
        let saved = save_checkpoint(socket, Some(LOCAL.to_string())).await?;
        // The count that made the question worth asking, against what was
        // written down: a listing that failed between the two would otherwise
        // write nothing, report nothing wrong, and authorise the restart.
        if saved.lost > 0 || saved.written < sessions {
            let short = format!(
                "{} of {sessions} session{} written down",
                saved.written,
                if sessions == 1 { " was" } else { "s were" }
            );
            if !force {
                eprintln!(
                    "{} not restarting: {short}.\n  `{command} --force --keep-sessions` \
                     restarts anyway and puts back what could be saved; `{command} --force` \
                     restarts and ends them all.",
                    style::amber("!")
                );
                return Ok(FAILED);
            }
            // Asked for anyway, so say what it costs rather than saying it is
            // not happening and then doing it.
            eprintln!(
                "{} only {short}; restarting anyway because `--force` was given, so the \
                 rest end here.",
                style::amber("!")
            );
        }
    } else if !agreed(sessions, force, true, command, "restarting it")? {
        return Ok(OK);
    }

    manymux::node::restart(socket).await?;
    println!("{} restarted the node", style::green("✓"));

    if putting_back {
        return restore_checkpoint(socket, Some(LOCAL.to_string()), false).await;
    }
    Ok(OK)
}

/// The session on this machine that this command was typed in, if it is one.
///
/// What makes this worth asking: the restart hangs every session up, and this
/// process is in the process group of whichever one it is running in, so it is
/// killed partway through. Reproduced before it was guarded: the checkpoint is
/// written, the node goes, the client dies with the session, and the restore
/// never runs. Nothing comes back, and the flag that promised it would is the
/// reason nobody was watching.
async fn ran_from_a_session_here(socket: &Path) -> Option<String> {
    let ours = manymux::foreground::our_session()?;
    sessions_on(socket, LOCAL)
        .await
        .ok()?
        .into_iter()
        .find(|session| session.pid == ours)
        .map(|session| session.name)
}

/// Whether to go ahead with something that takes running sessions down.
///
/// The node owns the PTYs, so its sessions are its children and go when it
/// goes. Nothing here can tell whether the work in them is finished, so with
/// someone at the keyboard it asks them, and without one it refuses and says
/// which flag repeats the command without the question.
fn agreed(
    sessions: usize,
    force: bool,
    keepable: bool,
    command: &str,
    doing: &str,
) -> Result<bool> {
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
                "{} {cost}.\n{}  `{command} --force` to do it anyway, or leave it until they \
                 are done.",
                style::amber("!"),
                keeping(keepable, command)
            );
            Ok(false)
        }
    }
}

/// The line that names the way out of the refusal above, for the two commands
/// that have one.
///
/// `mm stop` does not: it is asking for the node to be gone, and putting the
/// sessions back would be the opposite of what was typed. A checkpoint can
/// still be written by hand before it, which is what the shorter line says.
fn keeping(keepable: bool, command: &str) -> String {
    if keepable {
        format!(
            "  `{command} --keep-sessions` writes down where each session is and starts \
             them again afterwards.\n"
        )
    } else {
        "  `mm checkpoint save` writes them down first, for `mm checkpoint restore` \
         afterwards.\n"
            .to_string()
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

/// Like [`open`], but for a caller with somewhere better to put what ssh says.
///
/// Two of them. An attach paints a session over the screen and a dropped
/// connection leaves that paint exactly where it was, which is what a wait is
/// for; ssh printing its account of a machine it cannot reach lands there,
/// once per attempt, on a terminal in raw mode. And a listing asks every
/// watched machine at once, so the same account arrives interleaved with the
/// other machines' answers, from a client that may well be an attached one:
/// see [`sessions_on`]. Kept instead, the same words go into the error, which
/// reaches the person on the first attach of a run, the log on every reconnect
/// after it, and the `mm: <host>: <why>` line under a listing.
/// See [`manymux::client::Heard`].
async fn open_hushed(socket: &Path, host: &str) -> Result<Stream> {
    if is_this_machine(host) {
        return Stream::local(socket).await;
    }
    Stream::over_ssh_hushed(host, Some(Arc::new(offer_to_install))).await
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
    let mut groups = Groups::default();
    let _ = groups.update(|groups| {
        groups.prune(&listing.answering(), &listing.sessions);
        Ok(())
    });

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

/// What a save managed to write down.
///
/// Both numbers, rather than an exit code, because `--keep-sessions` has to
/// weigh them against something an exit code cannot carry: the count of
/// sessions that made the restart worth asking about in the first place.
struct Saved {
    /// Entries written for the machines this save asked about.
    written: usize,
    /// Sessions those machines are running that could not be written down.
    lost: usize,
}

impl Saved {
    fn code(&self) -> u8 {
        // A checkpoint that quietly covers half of what is running is the thing
        // to find out about before restarting anything, not after.
        if self.lost > 0 { FAILED } else { OK }
    }
}

/// `mm checkpoint save`: write down what every session is doing.
///
/// The order of the two questions matters. The listing comes first and is what
/// the file is built from, because it is the listing that carries the pid and
/// the start time a group is keyed on, and its order is the order every screen
/// shows sessions in. What each session is *doing* is asked separately and
/// matched back by name.
async fn save_checkpoint(socket: &Path, host: Option<String>) -> Result<Saved> {
    let listing = everywhere(socket).await?;
    let groups = Groups::load().unwrap_or_default();
    let wanted = |at: &str| host.as_deref().is_none_or(|only| same_machine(only, at));

    // Before anything is written, since a machine named that nothing answers
    // to is a typo far more often than it is a machine with no sessions, and
    // reporting it after the file has been rewritten leaves `mm checkpoint
    // show` calling an untouched file freshly saved.
    if let Some(only) = &host
        && !listing.answering().iter().any(|at| same_machine(only, at))
    {
        bail!("no machine answering to {only}");
    }

    let mut doing = HashMap::new();
    let mut refused = Vec::new();
    for at in listing.answering() {
        if !wanted(&at) {
            continue;
        }
        match what_is_doing(socket, &at, &listing).await {
            Ok(answers) => {
                doing.extend(
                    answers
                        .into_iter()
                        .map(|d| ((at.clone(), d.name.clone()), d)),
                );
            }
            Err(e) => refused.push(Unreachable {
                host: at,
                error: format!("{e:#}"),
            }),
        }
    }

    // The session this command was typed in, which is running this command.
    // Recorded as it stands, it would come back as `mm checkpoint save`, and a
    // restore would then rewrite the file it is walking.
    //
    // Found by the session leader's pid rather than by `MM_SESSION`: that is
    // stamped into the environment once at spawn and a rename cannot reach
    // back into it, so a session renamed since would go unrecognised and
    // record exactly the thing this is here to avoid.
    let ours = manymux::foreground::our_session();

    let mut sessions = Vec::new();
    let mut lost = 0;
    // Live sessions this save could not describe. Their host answered, so the
    // carry-over would drop whatever an earlier save knew about them; they are
    // named here so it keeps that instead.
    let mut undescribed: HashSet<(String, String)> = HashSet::new();
    for hosted in &listing.sessions {
        if !wanted(&hosted.host) {
            continue;
        }
        let mut give_up = |why: String| {
            eprintln!("mm: {why}");
            undescribed.insert((hosted.host.clone(), hosted.session.name.clone()));
            lost += 1;
        };
        let Some(found) = doing.get(&(hosted.host.clone(), hosted.session.name.clone())) else {
            // Its machine refused the question, and is reported below rather
            // than once per session on it.
            if !refused.iter().any(|r| r.host == hosted.host) {
                give_up(format!(
                    "{} said nothing about {}",
                    hosted.host, hosted.session.name
                ));
            }
            continue;
        };
        let Some(cwd) = found.cwd.clone() else {
            give_up(format!(
                "{} cannot say where {} is, so it is left out",
                hosted.host, hosted.session.name
            ));
            continue;
        };
        // The session this command was typed in. Its work is this command, and
        // it will be a prompt in the right place again the moment this returns.
        let ours_this_one = is_this_machine(&hosted.host) && ours == Some(hosted.session.pid);
        // Nothing known is never a prompt: a session sitting at one has its
        // shell in front of it, so the argv holds the shell. Empty means the
        // read found no process to describe, which happens when the process
        // group's leader has been reaped while the rest of a pipeline runs on,
        // or when it is a zombie. Recorded as a prompt, as this once did, a
        // session running a build comes back as an empty shell and nothing
        // anywhere says so.
        let unknown = found.foreground.is_empty() && !ours_this_one;
        if unknown {
            give_up(format!(
                "{} cannot say what {} is running, so it is left out",
                hosted.host, hosted.session.name
            ));
            continue;
        }
        // The pid the machine answered with, against the one the listing gave.
        // A session that ended and was replaced by another of the same name
        // between the two questions is one this would otherwise write down
        // with the wrong work against the right name.
        if found.pid != hosted.session.pid {
            give_up(format!(
                "{} changed under the question, so {} is left out",
                hosted.host, hosted.session.name
            ));
            continue;
        }
        let command = if ours_this_one {
            Vec::new()
        } else {
            checkpoint::resumed(&found.foreground)
        };
        // A wrapper from an earlier restore that nothing above could read
        // through. Asked of the answer rather than of the raw foreground,
        // which is the whole distinction: the plain wrapper shape *is* read
        // through, and it is what a machine reports in the moment between the
        // spawn and the command reaching the front of the terminal. Refusing
        // on the raw form failed a save taken straight after a restore, on a
        // runner slow enough for that moment to be the one being asked about.
        // What is left here is the shape nothing can read: quoted inside a
        // login shell's own snippet, which would gain another shell on every
        // save and restore.
        if checkpoint::still_wrapped(&command) {
            give_up(format!(
                "{} on {} is still starting up, so it is left out; ask again in \
                 a moment",
                hosted.session.name, hosted.host
            ));
            continue;
        }
        sessions.push(Kept {
            host: hosted.host.clone(),
            name: hosted.session.name.clone(),
            cwd,
            group: groups
                .group_of(&hosted.host, &hosted.session)
                .map(str::to_string),
            command,
        });
    }

    warn_about_shared_directories(&sessions);

    let machines = sessions
        .iter()
        .map(|kept| kept.host.as_str())
        .collect::<HashSet<_>>()
        .len();
    let written = sessions.len();

    // Only what this save actually learned about is replaced. A machine that
    // was not asked (`--host` elsewhere, and `--keep-sessions` always narrows
    // to this one) or that refused the question has said nothing, and reading
    // that silence as "it has no sessions" would throw away a checkpoint taken
    // at somebody's desk covering three machines the moment they updated one
    // of them. The same argument `Groups::prune` makes about a machine that is
    // asleep, for the same reason.
    let answered: Vec<String> = listing
        .answering()
        .into_iter()
        .filter(|at| wanted(at) && !refused.iter().any(|r| r.host == *at))
        .collect();
    // Read rather than shrugged at: a file broken by the hand-editing this
    // invites would otherwise be replaced by whatever this save happens to
    // cover, which is the one thing carrying over exists to prevent, and
    // `restore` already refuses to guess at the same file.
    let mut carried_over = Checkpoint::load()?.sessions;
    // Compared the way every other host name here is compared, and not as
    // strings. A file naming this machine `local` — a spelling a restore
    // accepts and a hand-edited file is likely to use — would otherwise miss
    // the `devbox` in this list, be carried over, and be written again beside
    // the fresh entry: one session listed twice, the stale copy holding a
    // stale directory. The same happens on its own to a machine whose short
    // hostname changed.
    //
    // A session that could not be described this time keeps its earlier entry
    // if there is one. Its host answered, so the rule above would drop it, and
    // dropping it is how a good checkpoint taken at ten o'clock was destroyed
    // by a save at five past that happened to catch a session mid-pipeline:
    // the record went, and the restart it was taken for was refused in the
    // same breath, leaving neither. The entry is older than this moment and is
    // said to be, and the session is still counted against the save, so
    // nothing acts on it without somebody deciding to.
    let mut kept_from_before = 0;
    carried_over.retain(|kept| {
        if undescribed.contains(&(kept.host.clone(), kept.name.clone())) {
            kept_from_before += 1;
            return true;
        }
        !answered.iter().any(|at| same_machine(at, &kept.host))
    });
    let carried = carried_over.len() - kept_from_before;
    sessions.extend(carried_over);
    if kept_from_before > 0 {
        println!(
            "  {}",
            style::amber(&format!(
                "{kept_from_before} of those kept the entry an earlier save wrote, which \
                 may name a directory they have since left"
            ))
        );
    }

    Checkpoint {
        taken: Checkpoint::now(),
        sessions,
    }
    .save()?;
    if carried > 0 {
        println!(
            "  {}",
            style::faint(&if carried == 1 {
                "1 session on a machine this did not ask about is still in the file".to_string()
            } else {
                format!(
                    "{carried} sessions on machines this did not ask about are still in the file"
                )
            })
        );
    }

    for host in &refused {
        // The node's own complaint, which names the version it is running: a
        // machine that has never been restarted since this was added is the
        // ordinary reason, and the one thing it cannot checkpoint is the
        // restart that would fix it.
        eprintln!("mm: {}: {}", host.host, host.error);
        lost += listing
            .sessions
            .iter()
            .filter(|hosted| hosted.host == host.host)
            .count();
    }
    println!(
        "{} wrote {written} session{} on {machines} machine{} to {}",
        style::green("✓"),
        if written == 1 { "" } else { "s" },
        if machines == 1 { "" } else { "s" },
        style::bold(&Checkpoint::path().display().to_string())
    );
    Ok(Saved { written, lost })
}

/// Say so when two sessions would resume the same conversation.
///
/// `claude --continue` and `pi --continue` pick up the newest session *in the
/// directory they are run in*, which is the whole of what makes a checkpoint
/// work without capturing an id. Two sessions on one machine working in one
/// directory therefore come back on the same conversation, and the second one
/// to start is the one that loses. Nothing here can tell them apart: neither
/// program leaves its transcript open, so there is no id to read.
///
/// Said at the save rather than at the restore, because the file is editable
/// and this is the moment somebody can do something about it: naming the
/// conversation by hand is a one-word change to a line they are being pointed
/// at. Not an error, since two sessions in one checkout is an ordinary way to
/// work and usually only one of them is mid-conversation.
fn warn_about_shared_directories(sessions: &[Kept]) {
    let mut seen: HashMap<(&str, &str), &Kept> = HashMap::new();
    for kept in sessions {
        if !checkpoint::resumes_by_directory(&kept.command) {
            continue;
        }
        let key = (kept.host.as_str(), kept.cwd.as_str());
        if let Some(first) = seen.get(&key) {
            eprintln!(
                "{} {} and {} are both in {}, so both come back on the same \
                 conversation.\n  Edit {} to name one of them, or leave it: the \
                 newest wins.",
                style::amber("!"),
                style::bold(&first.name),
                style::bold(&kept.name),
                kept.cwd,
                Checkpoint::path().display()
            );
        } else {
            seen.insert(key, kept);
        }
    }
}

/// What every session on one machine is doing.
///
/// Three ways to the same answer, and the two that do not ask the node are not
/// optimisations. A node only learns to answer `Request::Snapshot` in the build
/// that added it, so on any machine the *first* checkpoint is refused by the
/// very node whose sessions are worth saving, and restarting it to fix that is
/// the thing the checkpoint exists to make safe. On this machine `/proc` is
/// right there. On another, the same files are read over the ssh that is
/// already open, which needs nothing on the far side to have heard of
/// checkpoints, or of manymux at all.
async fn what_is_doing(socket: &Path, host: &str, listing: &Listing) -> Result<Vec<Doing>> {
    let here: Vec<&HostedSession> = listing
        .sessions
        .iter()
        .filter(|hosted| hosted.host == host)
        .collect();

    if is_this_machine(host) {
        return Ok(here
            .iter()
            .map(|hosted| doing(&hosted.session, manymux::foreground::of(hosted.session.pid)))
            .collect());
    }

    match open(socket, host)
        .await?
        .request(&Request::Snapshot)
        .await?
    {
        Response::Snapshot(answers) => Ok(answers),
        // The refusal of a node that predates the question, which is an answer
        // rather than a failure: it says to go and read the files instead.
        Response::Error(said) => {
            debug!("{host} cannot be asked what it is doing ({said}); reading /proc over ssh");
            over_ssh(host, &here).await
        }
        other => bail!("unexpected response to a snapshot: {other:?}"),
    }
}

/// One session's answer, however the three files were come by.
fn doing(session: &manymux::proto::SessionInfo, front: manymux::foreground::Foreground) -> Doing {
    Doing {
        name: session.name.clone(),
        pid: session.pid,
        cwd: front.cwd,
        foreground: front.argv,
    }
}

/// Read what a machine is doing out of its `/proc`, over ssh.
///
/// Two passes, because which process to describe is decided *here*: the first
/// brings back each session leader's `stat`, this end works out what is in
/// front of its terminal, and the second brings back that process's directory
/// and argv. Two round trips rather than one is the price of the far side
/// deciding nothing, and ssh shares one connection per machine so the second
/// costs a message rather than a handshake.
///
/// A machine that cannot answer at all leaves every session it holds without a
/// directory, which is reported and counted against the save the way any other
/// undescribable session is.
async fn over_ssh(host: &str, here: &[&HostedSession]) -> Result<Vec<Doing>> {
    use manymux::foreground::{Foreground, Raw};

    let leaders: Vec<u32> = here.iter().map(|hosted| hosted.session.pid).collect();
    if leaders.is_empty() {
        return Ok(Vec::new());
    }

    let stats = checkpoint::read_back(
        &manymux::ssh::ask(
            host,
            &checkpoint::read_proc(&leaders, checkpoint::Want::Stat),
        )
        .await?,
    );
    let stat_of = |pid: u32| -> String {
        stats
            .iter()
            .find(|(answered, _)| *answered == pid)
            .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    };

    // Whose directory and argv to ask for, decided by the same rule the local
    // path uses, on the same bytes.
    let fronts: Vec<u32> = leaders
        .iter()
        .map(|&leader| {
            manymux::foreground::in_front(leader, manymux::foreground::tpgid(&stat_of(leader)))
        })
        .collect();

    let cwds = checkpoint::read_back(
        &manymux::ssh::ask(host, &checkpoint::read_proc(&fronts, checkpoint::Want::Cwd)).await?,
    );
    let argvs = checkpoint::read_back(
        &manymux::ssh::ask(
            host,
            &checkpoint::read_proc(&fronts, checkpoint::Want::Cmdline),
        )
        .await?,
    );
    let bytes_of = |answers: &[(u32, Vec<u8>)], pid: u32| -> Vec<u8> {
        answers
            .iter()
            .find(|(answered, _)| *answered == pid)
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_default()
    };

    Ok(here
        .iter()
        .zip(&fronts)
        .map(|(hosted, &front)| {
            let raw = Raw {
                stat: stat_of(hosted.session.pid),
                cwd: String::from_utf8_lossy(&bytes_of(&cwds, front)).into_owned(),
                cmdline: bytes_of(&argvs, front),
            };
            // Non-UTF-8 is nothing rather than something close to it, here as
            // in the local path: `from_utf8_lossy` on a path would name a
            // directory that does not exist.
            let front = if raw.cwd.contains('\u{fffd}') {
                Foreground {
                    cwd: None,
                    argv: raw.read().argv,
                }
            } else {
                raw.read()
            };
            doing(&hosted.session, front)
        })
        .collect())
}

/// `mm checkpoint restore`: start the saved sessions again.
async fn restore_checkpoint(socket: &Path, host: Option<String>, dry_run: bool) -> Result<u8> {
    let checkpoint = Checkpoint::load()?;
    if checkpoint.is_empty() {
        println!("nothing has been checkpointed; `mm checkpoint save` writes one");
        return Ok(OK);
    }
    let wanted: Vec<&Kept> = checkpoint
        .sessions
        .iter()
        .filter(|kept| {
            host.as_deref()
                .is_none_or(|only| same_machine(only, &kept.host))
        })
        .collect();
    if let Some(only) = &host
        && wanted.is_empty()
    {
        // Narrowing to a machine the file says nothing about, which is a typo
        // far more often than it is a machine that had no sessions. Reported
        // rather than answered with "put back 0 sessions", which reads as
        // success.
        eprintln!("mm: the checkpoint has nothing on {only}");
        return Ok(FAILED);
    }

    if dry_run {
        for kept in &wanted {
            println!("{}", restoring(kept));
            if let Some(line) = spawning(kept) {
                println!("  {}", style::faint(&line));
            }
        }
        return Ok(OK);
    }

    // In file order and one at a time, because the start time is stamped at the
    // spawn: restoring them in the order they were opened in is what puts every
    // listing's rows back where they were.
    let mut started = 0;
    let mut here = Vec::new();
    let mut failed = 0;
    let mut reached = Vec::new();
    for kept in &wanted {
        let spawned = spawn_kept(socket, kept).await;
        match spawned {
            Ok(Some(name)) => {
                println!("{} {}", style::green("✓"), restoring(kept));
                if !is_this_machine(&kept.host) {
                    reached.push(kept.host.clone());
                }
                started += 1;
                here.push((kept.host.clone(), name, kept.group.clone()));
            }
            // Already running, which is what a restore run twice looks like,
            // and what a machine that put its own sessions back looks like.
            //
            // Its group is still put back, and that is the whole of the
            // cross-machine story: the machine restores the sessions and the
            // client that holds the groups restores those, so the second half
            // arrives to find every name already taken. Skipping the grouping
            // here, as this first did, left that documented sequence doing
            // nothing at all and saying it had worked.
            Ok(None) => {
                println!(
                    "  {} is already running on {}",
                    style::bold(&kept.name),
                    kept.host
                );
                here.push((kept.host.clone(), kept.name.clone(), kept.group.clone()));
            }
            Err(e) => {
                eprintln!("mm: {}: {e:#}", qualified(&kept.host, &kept.name));
                failed += 1;
            }
        }
    }

    reached.sort();
    reached.dedup();
    note_reached(socket, reached).await;

    // Only now do the new pids exist, and membership is keyed on them.
    if here.iter().any(|(_, _, group)| group.is_some()) {
        let listing = everywhere(socket).await?;
        let put_back = |groups: &mut Groups| {
            for (host, name, group) in &here {
                let Some(group) = group else { continue };
                // Matched the way every host name here is matched. A raw string
                // compare missed a file spelling this machine `local`, which is
                // the spelling `--host` documents and a hand-edited file is likely
                // to use, and the miss was silent: the line above had already said
                // the group was put back.
                let found = listing
                    .sessions
                    .iter()
                    .find(|h| same_machine(&h.host, host) && h.session.name == *name);
                let Some(hosted) = found else {
                    eprintln!(
                        "mm: {} came back but could not be found again, so it is \
                     not in {group}",
                        qualified(host, name)
                    );
                    failed += 1;
                    continue;
                };
                // Reported rather than propagated. The names come out of a file
                // somebody may have edited, so one that `assign` refuses is one
                // bad line: raised as an error here it would abandon the loop
                // after every session had already been started, save no grouping
                // at all, and fail a command whose sessions are all back.
                if let Err(e) = groups.assign(group, host, &hosted.session) {
                    eprintln!("mm: {}: {e:#}", qualified(host, name));
                    failed += 1;
                }
            }
            Ok(())
        };
        Groups::change(put_back)?;
    }

    let machines = here
        .iter()
        .map(|(host, _, _)| host.as_str())
        .collect::<HashSet<_>>()
        .len();
    println!(
        "{} put back {started} session{} on {machines} machine{}",
        style::green("✓"),
        if started == 1 { "" } else { "s" },
        if machines == 1 { "" } else { "s" }
    );
    Ok(if failed > 0 { FAILED } else { OK })
}

/// Start one saved session, or answer `None` if that name is already running.
///
/// The one caller that sends a `cwd` to another machine. `mm new` deliberately
/// does not, having no idea whether a directory of yours exists there; here the
/// directory came off that same machine a moment ago and is the whole point.
async fn spawn_kept(socket: &Path, kept: &Kept) -> Result<Option<String>> {
    let spec = SpawnSpec {
        name: Some(kept.name.clone()),
        command: checkpoint::to_spawn(&kept.command),
        cwd: Some(kept.cwd.clone()),
        size: attach::session_size(),
        // The command as somebody would say it, since the one being run is a
        // wrapper. Empty for a session that comes back as a prompt, where the
        // node's own answer is already the right one.
        label: (!kept.command.is_empty()).then(|| kept.command.join(" ")),
    };
    let mut stream = open_or_start(socket, &kept.host).await?;
    match stream.request(&Request::Spawn(spec)).await? {
        Response::Spawned { name } => Ok(Some(name)),
        // The node's way of saying the name is taken. Told apart by asking
        // rather than by parsing: a listing first would be a second round trip
        // per machine and still stale by the time the spawn landed.
        Response::Error(e) if e.contains("already exists") => Ok(None),
        Response::Error(e) => bail!(e),
        other => bail!("unexpected response to a spawn: {other:?}"),
    }
}

/// One line of what a restore is about to do, or would do.
fn restoring(kept: &Kept) -> String {
    let what = if kept.command.is_empty() {
        "a login shell".to_string()
    } else {
        kept.command.join(" ")
    };
    let group = kept
        .group
        .as_ref()
        .map(|g| format!(" {}", style::faint(&format!("@{g}"))))
        .unwrap_or_default();
    format!(
        "{}{group} {} {}",
        style::bold(&qualified(&kept.host, &kept.name)),
        what,
        style::faint(&format!("in {}", kept.cwd))
    )
}

/// The line the node will be handed for one entry, spelled the way it will
/// reach a shell.
///
/// Worth a row of its own because the row above it is not the whole story: what
/// runs is the captured command inside the wrapper that keeps the login shell
/// behind it (`checkpoint::to_spawn`), and the one question anybody asks a
/// checkpoint before restarting a machine is what it is actually going to run.
/// Built from the same `to_spawn` the restore sends and joined with the same
/// quoter the node runs it through (`shell::join`), so this cannot become a
/// second opinion about what happens.
///
/// `None` for a session that was sitting at a prompt: an empty spawn is the
/// login shell, which the row above already says in words.
fn spawning(kept: &Kept) -> Option<String> {
    let argv = checkpoint::to_spawn(&kept.command);
    (!argv.is_empty()).then(|| shell::join(&argv))
}

/// `mm checkpoint show`: what is written down, and nothing else.
///
/// Contacts no machine on purpose. This is the one command that still answers
/// when everything is being restarted, which is exactly when somebody wants to
/// know what they are about to get back.
fn show_checkpoint() -> Result<u8> {
    let checkpoint = Checkpoint::load()?;
    if checkpoint.is_empty() {
        println!("nothing has been checkpointed; `mm checkpoint save` writes one");
        return Ok(OK);
    }
    println!(
        "{}",
        style::faint(&format!(
            "{} sessions, saved {} ago",
            checkpoint.sessions.len(),
            term::duration(Checkpoint::now().saturating_sub(checkpoint.taken))
        ))
    );
    for kept in &checkpoint.sessions {
        println!("{}", restoring(kept));
        if let Some(line) = spawning(kept) {
            println!("  {}", style::faint(&line));
        }
    }
    Ok(OK)
}

/// Whether a machine named on a command line is the one an entry is about.
///
/// `local` and this machine's own name are the same machine, which is the rule
/// every other command follows.
fn same_machine(named: &str, at: &str) -> bool {
    named == at || (is_this_machine(named) && is_this_machine(at))
}

/// A machine named on a command line, under the name a listing gives it.
///
/// The two spellings of this machine are not the same string, and an attach
/// compares them as one: `mm new` with no host says `local`, `mm attach
/// local/build` says it outright, and every listing labels this machine with
/// its own short name. A [`Located`] whose host is the other spelling matches
/// no row of the listing, so the session the run is *in* was in none of it: the
/// popup opened highlighting somebody else's session with Enter over it, no row
/// wore the mark, and the group the run narrows to on the way in was read off a
/// session nothing could find. Applied where a typed word stops being one and
/// becomes the address a whole run compares against, rather than at each of the
/// places that compare.
fn as_listed(host: &str) -> &str {
    if is_this_machine(host) {
        this_machine()
    } else {
        host
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
    let mut cycle = Cycle::new(Located::new(as_listed(host), name));
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
    // What the popup's lister last found, for the loop to take back: it hands
    // its own pending listing over, so the answer arrives there and not here.
    let seen: Arc<Mutex<Option<Snapshot>>> = Arc::new(Mutex::new(None));

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
        let session = match reaching(attach_to(socket, &target, history, watching), attached).await
        {
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
        // And the hop is over. What the flag above buys is one arm: a hop onto
        // a session the listing was stale about, which is a thing that is true
        // for exactly as long as the attach it describes has not happened.
        // Left set, it was still set an hour later when the node holding the
        // session restarted, and the drop that came back as `Missed::Gone` was
        // read as that stale listing: the cycle forgot a live entry and the run
        // walked back to the session the command line named, which is the one
        // thing the arm below is written to stop.
        hopped = false;

        // Before the rows are built, because the popup is drawn from them and
        // it opens on a keystroke inside the attach, where there is no asking
        // anything. The wait is the same half second the switch keys allow and
        // for the same reason: what is already known stands rather than being
        // waited on, since a machine that is asleep must not hold up a screen.
        take_listing(&mut listing, &mut cycle, &mut snapshot, &mut groups).await;
        // Narrowing to the group of the session you arrived in, which is a
        // thing to decide on the way in and never afterwards. It used to wait
        // for a listing that said anything, and on a fleet slower than
        // `LISTING_WAIT` the first of those lands well after the run has
        // started: the block then fired on whatever session you were on by
        // then, so putting a session in a group silently narrowed the run to
        // it. Not knowing is an answer here, and the answer is to narrow to
        // nothing.
        // Read again rather than watched. `groups.toml` is shared with every
        // other client on this machine, so the copy this run started with goes
        // stale the moment another terminal moves a session, and this is where
        // the run next looks at it: the rows the popup opens on and the cycle
        // the switch keys walk are both built below. A watch would have nothing
        // to push to, since nothing in an attached screen is drawn from this
        // file while a key is not being pressed, and the file is a few hundred
        // bytes read once per keystroke that goes anywhere. Kept if it will not
        // read, a broken file being no reason to forget what is in hand.
        groups = Groups::load().unwrap_or(groups);
        // Which is only half of it until the cycle is told: the rows below are
        // built from the file just read, and the narrowing they are drawn under
        // is whatever the last listing left behind. So the entries are made
        // again, against the same snapshot and the membership as it now is,
        // which is what lets a session the window next door moved out of this
        // run's group widen the run rather than waiting on a fan-out to say so.
        // Only against a snapshot that says something: an empty one is a run
        // whose listing has never landed, and every group is missing from it.
        if !snapshot.is_empty() {
            cycle.refresh(entries(&snapshot, &groups));
        }
        if !settled {
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
        // itself. The narrowing cannot change under it, every way of changing
        // that ending the attach, so it is handed over as it stands. The groups
        // can: `groups.toml` is one file shared by every client on this machine,
        // and the other terminal moving a session while this popup is up is
        // exactly the case, so the copy below is a fallback for a file that
        // will not read rather than the answer.
        let (asks, mut wanted) = tokio::sync::mpsc::channel::<()>(1);
        let (answered, fresh) = tokio::sync::watch::channel(listed.rows.clone());
        let lister = {
            let socket = socket.to_path_buf();
            let groups = groups.clone();
            let focus = cycle.focused().map(str::to_string);
            let recent = cycle.recent();
            let latest = Arc::clone(&latest);
            let seen = Arc::clone(&seen);
            // The listing already out there, handed over rather than left to
            // finish into a variable nobody reads again until the attach ends.
            // `take_listing` gives one half a second and no more, so on a fleet
            // where a fan-out takes longer this is *always* still running when
            // the popup opens, and the popup then started a second one from
            // scratch: two ssh commands per host for one keypress, and an empty
            // box for as long as the second took.
            let mut running = listing.take();
            tokio::spawn(async move {
                while wanted.recv().await.is_some() {
                    let listing = match running.take() {
                        Some(task) => match task.await {
                            Ok(snapshot) => Ok(snapshot),
                            Err(_) => continue,
                        },
                        None => everywhere(&socket).await.map(|listing| Snapshot {
                            answered: listing.answering(),
                            sessions: listing.sessions,
                        }),
                    };
                    let Ok(snapshot) = listing else { continue };
                    if snapshot.is_empty() {
                        continue;
                    }
                    let held = Groups::load().unwrap_or_else(|_| groups.clone());
                    let listed = Listed::of(&snapshot, &held, focus.as_deref(), &recent);
                    // Kept where the caller can read it once the attach ends:
                    // the ids the popup hands back are this listing's, and
                    // looking them up in the one it opened with would name the
                    // wrong session.
                    *held_lock(&latest) = Some(listed.clone());
                    // And the snapshot behind them, because the loop's own copy
                    // is now the older of the two: it gave up waiting on the
                    // listing this task went on to collect.
                    *held_lock(&seen) = Some(snapshot);
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
        // And what those rows were built from, which the loop gave up waiting
        // for and the popup went on to collect. Without taking it back, a fleet
        // slower than `LISTING_WAIT` never fills this in at all: every attach
        // hands its pending listing to the popup, so nothing is ever left for
        // `take_listing` to find.
        if let Some(fresher) = held_lock(&seen).take() {
            let _ = groups.update(|groups| {
                groups.prune(&fresher.answered, &fresher.sessions);
                Ok(())
            });
            snapshot = fresher;
            cycle.refresh(entries(&snapshot, &groups));
        }
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
                // Back to the list, because that is where the gesture was made
                // and none of these is a gesture that goes anywhere: grouping a
                // session, naming one, narrowing to a group all leave you
                // exactly where you were, and dropping into the session
                // afterwards throws away the place you were working from. Enter
                // on a session is the one that means go, and it is the one that
                // sets this back to `Focus` below.
                mode = Mode::Control;
                match chose {
                    // A hop like any other, so the way back is recorded the
                    // same way and `Motion::Last` reaches it.
                    Chose::Go(row) => {
                        // The one that goes somewhere, and so the one that
                        // leaves you in the session rather than over it.
                        mode = Mode::Focus;
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
                    // A hop, and the narrowing that has to go with it: what
                    // has just been started is in no group. `Cycle::started`
                    // is both, and says why there rather than here.
                    cycle.started(Located::new(&target.host, &name));
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

/// How long one attempt at getting back is given before it counts as a
/// failure.
///
/// There has to be a number here, because the one thing an attempt cannot be
/// given is forever. ssh's own `ConnectTimeout` is about reaching a machine,
/// and the way this hangs is a step further in: `ControlMaster=auto` puts every
/// command on one shared connection, and connecting to a control socket has no
/// deadline of any kind. A master whose network went takes everything
/// multiplexed onto it with it, which is a whole terminal sitting on
/// `reconnecting` with a row that has stopped counting and a Ctrl-C nobody is
/// reading, until the master times out on its own schedule or somebody works
/// out what a control socket is. Two clients on one master hang and then come
/// back together, which is the shape of it from outside.
///
/// Ten seconds because it is the wrong side of every honest reattach and the
/// right side of nobody's patience: a handshake onto a machine that is there
/// takes under a second, and a retry costs a line on the row rather than
/// anything real.
const REACH_FOR: Duration = Duration::from_secs(10);

/// One attempt at reaching a session, with a deadline on it once this run has
/// been attached to something.
///
/// Not before that. The first attach of a run is a command somebody typed, and
/// it is allowed to take as long as it takes: a cold ssh, a node being started,
/// an install being offered and answered. What is bounded is a reconnect, which
/// nobody asked for and which has somewhere to go when it fails, namely round
/// the loop to a row that counts down and reads the keyboard.
///
/// Dropping the attempt is what stops it: `ssh::spawn` sets `kill_on_drop`, so
/// the ssh that was hanging goes with the future rather than being left to
/// hold the master open behind us.
async fn reaching(
    attempt: impl Future<Output = Result<Attached, Missed>>,
    bounded: bool,
) -> Result<Attached, Missed> {
    if !bounded {
        return attempt.await;
    }
    match tokio::time::timeout(REACH_FOR, attempt).await {
        Ok(reached) => reached,
        Err(_) => Err(Missed::Unreachable(anyhow!(
            "no answer in {}s",
            REACH_FOR.as_secs()
        ))),
    }
}

/// Open a stream to wherever a session is, and attach to it.
async fn attach_to(
    socket: &Path,
    target: &Located,
    history: u32,
    watching: bool,
) -> Result<Attached, Missed> {
    let stream = open_hushed(socket, &target.host)
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
        // A login shell, which the node names for itself.
        label: None,
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
    /// Which machines said so, this one included: it is what pruning is
    /// allowed to act on, and it is `Listing::answering` rather than
    /// `Listing::reached`, which leaves this machine out for a different
    /// question entirely.
    answered: Vec<String>,
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
    /// What each session row's id means, looked up by that id.
    ///
    /// [`CURRENT`] first and then the rest in row order, rather than one per
    /// row in the same order: the session the run is in has to wear the same id
    /// in every listing, and it is here even when this listing has never heard
    /// of it.
    at: Vec<Located>,
    /// One per group row. The first is `None`, which is "everything" when you
    /// are narrowing and "no group" when you are moving a session.
    groups: Vec<Option<String>>,
}

/// The id the session the run is in wears in every listing. See [`Listed::of`].
const CURRENT: usize = 0;

/// How many rows can wear a digit: `1` to `9`, and no more, because a two-digit
/// row would want a key you press twice and a moment for the client to decide
/// you had finished pressing it. A run longer than that numbers the nine
/// sessions you were in most recently; the rest are still a tab away.
const DIGITS: u8 = 9;

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
        Self::of(snapshot, groups, cycle.focused(), &cycle.recent())
    }

    /// The same, from the two things about the cycle that matter, so a task
    /// with no cycle of its own can build these while an attach is up.
    ///
    /// `recent` is [`Cycle::recent`]: the sessions this run has been in, most
    /// recent first, so its head is the session the run is in now and the rest
    /// is what the digits are handed out along. It is never empty; an empty one
    /// is a listing with nothing to look out from, and there is no list to draw.
    fn of(snapshot: &Snapshot, groups: &Groups, focus: Option<&str>, recent: &[Located]) -> Self {
        let Some(current) = recent.first() else {
            return Self::default();
        };
        let mut rows = Vec::new();
        // The session the run is in is [`CURRENT`] in every listing, whether or
        // not this one has heard of it yet. An id is an index into this, so it
        // moves the moment a machine appears or a session ends, and the popup
        // keeps its cursor across a listing that lands under it *by id*
        // (`Picker::replace`): unpinned, the one row that must never slide out
        // from under the cursor was the one that did, a fuller listing arriving
        // half a second after the box opened and taking the cursor with it.
        let mut at = vec![current.clone()];
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

        // The groups first, each one whole. A group spans machines, so it is
        // the machines that break up under it and not the other way round:
        // nested the other way, the one thing a group is for, seeing a piece of
        // work in one place, was the one thing the list would not show.
        //
        // In the order their first session appears, which is by machine and
        // then oldest first, so a group's place moves only when the oldest
        // session in it ends. Ordering by name would shuffle the list under a
        // rename, which is what every listing here is written to avoid.
        let mut drawn: Vec<&str> = Vec::new();
        for (_, group) in &showing {
            let Some(group) = *group else { continue };
            if drawn.contains(&group) {
                continue;
            }
            drawn.push(group);
            rows.push(Row::heading(format!("@{group}")));
            // The machine on a line of its own rather than in front of every
            // name. `host/name` is how a session is addressed, so it was the
            // obvious label, but a real host name is most of the column: with
            // a mesh name and a slash in front of it there was no room left to
            // tell `service-iroh-dev` from `service-iroh-debug`, and which
            // session it is is the one thing the row exists to say.
            let mut machine: Option<&str> = None;
            for (hosted, _) in showing.iter().filter(|(_, g)| *g == Some(group)) {
                if machine != Some(hosted.host.as_str()) {
                    machine = Some(&hosted.host);
                    rows.push(Row::heading(&hosted.host).indent(1));
                }
                Self::session(&mut rows, &mut at, hosted, current, 2);
            }
        }

        // Then whatever is in no group, under the machine it is on, which is
        // the only thing left to gather it by. No heading says "no group": that
        // would name the one thing a group is not, and with the groups above it
        // the rest of the list needs no introduction.
        let mut machine: Option<&str> = None;
        for (hosted, group) in &showing {
            if group.is_some() {
                continue;
            }
            if machine != Some(hosted.host.as_str()) {
                machine = Some(&hosted.host);
                rows.push(Row::heading(&hosted.host));
            }
            Self::session(&mut rows, &mut at, hosted, current, 1);
        }
        // The cursor opens on the session you are looking out from, always.
        // Where there is no row for it there is one made: a fan-out gets
        // `LISTING_WAIT` and no more, so on a fleet slower than that the box
        // opens on whatever landed first, and a machine that has not answered
        // says nothing about the session the run is in. Opening on the first
        // row instead put the cursor on a stranger's session with Enter as the
        // next key, which is the same failure `as_listed` was written for.
        //
        // A machine that *did* answer and did not mention it is the other
        // thing: the session has ended or been renamed, so there is nothing to
        // point at and the first row is all there is.
        let listed_here = rows.iter().any(|row| !row.heading && row.id == CURRENT);
        if !listed_here
            && !snapshot
                .answered
                .iter()
                .any(|host| same_machine(host, &current.host))
        {
            // Under the machine it is on, in the shape the rest of the list
            // uses: a heading, and the session a step in from it.
            let deep = u16::from(focus.is_some());
            rows.push(Row::heading(&current.host).indent(deep));
            rows.push(
                Row::new(CURRENT, &current.session)
                    // The mark every listing puts on the session you are in.
                    // What it is doing is the machine's to say, and it has not
                    // said anything yet.
                    .note("●")
                    .indent(deep + 1),
            );
        }
        // The digits, handed out along the trail rather than down the box: the
        // list is in listing order, which is the order the sessions were
        // started in, and what a digit is for is going back to where you were.
        //
        // Only to rows that are here. A session that has ended, or that the
        // narrowing is hiding, is skipped rather than spending its number, or
        // the gap would be a key doing nothing in the middle of the ones that
        // work. Which is why this counts rather than indexing `recent`.
        let mut digit = 0;
        for was in recent {
            let Some(id) = at.iter().position(|listed| listed == was) else {
                continue;
            };
            let Some(row) = rows.iter_mut().find(|row| !row.heading && row.id == id) else {
                continue;
            };
            digit += 1;
            row.number = Some(digit);
            if digit == DIGITS {
                break;
            }
        }

        let highlight = rows
            .iter()
            .position(|row| !row.heading && row.id == CURRENT)
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
                // Spelt the way it is typed and the way the session list
                // heads it, so the same thing is not two things across two
                // lists one key apart.
                Row::new(names.len(), format!("@{name}"))
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
                narrowed: focus.map(str::to_string),
            },
            at,
            groups: names,
        }
    }

    /// One session row, and the address that goes with it.
    ///
    /// The two go in together and never apart: the row's id is where the
    /// address sits in `at`, which is how the popup can hand back a row without
    /// ever being told what a machine is. The session the run is in is the one
    /// whose address is already there, under [`CURRENT`].
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
            // The name alone: whatever heading gathered it, machine included,
            // is a line above it.
            Row::new(if here { CURRENT } else { at.len() }, &hosted.session.name)
                .detail(&hosted.session.title)
                .note(note)
                .indent(indent),
        );
        if !here {
            at.push(Located::new(&hosted.host, &hosted.session.name));
        }
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
    groups.update(|groups| {
        match &group {
            Some(name) => {
                groups.assign(name, &at.host, session)?;
            }
            None => groups.clear(&at.host, session),
        }
        Ok(())
    })
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
                answered: listing.answering(),
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
/// answered count towards it. The tidying is load-bearing beyond tidiness:
/// `Groups::update` reads the file before it writes it, so the copy the entries
/// below are built from is the one on disk rather than the one this run started
/// with. That is what lets `Cycle::refresh` see a session the window next door
/// moved out of the group this run is narrowed to.
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
                let _ = groups.update(|groups| {
                    groups.prune(&landed.answered, &landed.sessions);
                    Ok(())
                });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::slice::from_ref;

    /// An attempt that never answers, which is what a client multiplexed onto
    /// a dead ssh master is: nothing fails, nothing times out, and the row
    /// stops counting because the loop never comes back round to it.
    async fn never() -> Result<Attached, Missed> {
        std::future::pending().await
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_that_never_answers_is_a_failed_attempt() {
        // tokio's clock, not the wall's: this test spends no real time at all.
        let at = tokio::time::Instant::now();
        let missed = reaching(never(), true).await;
        assert!(matches!(missed, Err(Missed::Unreachable(_))));
        assert!(at.elapsed() >= REACH_FOR, "it waited {:?}", at.elapsed());
    }

    fn hosted(name: &str) -> HostedSession {
        HostedSession {
            host: this_machine().to_string(),
            session: manymux::proto::SessionInfo {
                name: name.to_string(),
                title: name.to_string(),
                command: "zsh".into(),
                pid: 1,
                size: manymux::proto::Size::new(80, 24),
                attached: 0,
                idle: 0,
                bells: 0,
                started: std::time::SystemTime::UNIX_EPOCH,
            },
        }
    }

    /// The bug this closes: `mm new` with no host names this machine `local`,
    /// while the listing the popup is drawn from calls it by its own name, so
    /// the session the run was in matched no row. The popup opened on the first
    /// session in the list with Enter over it, and no row wore the mark.
    #[test]
    fn the_popup_opens_on_the_session_the_run_is_in_however_the_machine_was_named() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build"), hosted("test")],
            answered: vec![this_machine().to_string()],
        };
        let current = Located::new(as_listed(LOCAL), "test");
        let listed = Listed::of(&snapshot, &Groups::default(), None, from_ref(&current));
        let row = &listed.rows.sessions[listed.rows.at];
        assert_eq!(row.label, "test");
        // And the same row is the one wearing the mark, since both are the same
        // comparison and a highlight without one reads as a listing gone stale.
        assert_eq!(row.note, "●");
    }

    /// The rows wearing a digit, in the order the digits run.
    fn numbered(listed: &Listed) -> Vec<(&str, u8)> {
        let mut rows: Vec<(&str, u8)> = listed
            .rows
            .sessions
            .iter()
            .filter_map(|row| Some((row.label.as_str(), row.number?)))
            .collect();
        rows.sort_by_key(|(_, number)| *number);
        rows
    }

    fn hosted_on(host: &str, name: &str) -> HostedSession {
        HostedSession {
            host: host.to_string(),
            ..hosted(name)
        }
    }

    /// A fan-out gets half a second and no more, so on a fleet slower than that
    /// the box opens on whatever landed first, which is not necessarily the
    /// machine you are sitting on. The cursor still opens on the session you
    /// are in: without a row for it, it opened on a stranger's session with
    /// Enter as the next key.
    #[test]
    fn the_popup_opens_on_the_session_the_run_is_in_before_its_machine_has_answered() {
        let snapshot = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build")],
            answered: vec!["gpu-box".to_string()],
        };
        let current = Located::new(as_listed(LOCAL), "test");
        let listed = Listed::of(&snapshot, &Groups::default(), None, from_ref(&current));
        let row = &listed.rows.sessions[listed.rows.at];
        assert_eq!(row.label, "test");
        assert_eq!(row.note, "●");
        // And Enter on it goes to the session the run is in, rather than to
        // whichever session that row id happened to name.
        assert_eq!(listed.at[row.id], current);
    }

    /// A fuller listing arriving under an open box must not take the cursor
    /// with it: an id is an index, and a machine appearing shifts every one of
    /// them, so the row the cursor was on named a different session a moment
    /// later. The session the run is in wears the same id in both listings.
    #[test]
    fn a_listing_landing_under_the_box_keeps_the_cursor_where_the_run_is() {
        let current = Located::new(as_listed(LOCAL), "test");
        let partial = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build")],
            answered: vec!["gpu-box".to_string()],
        };
        let first = Listed::of(&partial, &Groups::default(), None, from_ref(&current));
        let mut popup = manymux::client::picker::Picker::new(
            "sessions",
            "⏎ go",
            first.rows.sessions.clone(),
            first.rows.at,
        );
        let full = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build"), hosted("api"), hosted("test")],
            answered: vec!["gpu-box".to_string(), this_machine().to_string()],
        };
        let then = Listed::of(&full, &Groups::default(), None, from_ref(&current));
        popup.replace(then.rows.sessions.clone(), then.rows.at);
        let row = popup.chosen().expect("a row to be under the cursor");
        assert_eq!(row.label, "test");
        assert_eq!(then.at[row.id], current);
    }

    /// A machine that answered and did not mention it is a session that has
    /// ended or been renamed, and a row for it would be a row Enter cannot
    /// land on.
    #[test]
    fn a_session_the_machine_no_longer_has_gets_no_row_of_its_own() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build")],
            answered: vec![this_machine().to_string()],
        };
        let current = Located::new(as_listed(LOCAL), "gone");
        let listed = Listed::of(&snapshot, &Groups::default(), None, from_ref(&current));
        let landable = listed
            .rows
            .sessions
            .iter()
            .filter(|row| !row.heading)
            .count();
        assert_eq!(landable, 1, "a session that has ended got a row");
    }

    /// The first attach of a run is a command somebody typed: a cold ssh, a
    /// node being started and an install being answered are all allowed to
    /// take longer than a reconnect is.
    #[tokio::test(start_paused = true)]
    async fn the_first_attach_of_a_run_is_given_as_long_as_it_takes() {
        assert!(
            tokio::time::timeout(REACH_FOR * 10, reaching(never(), false))
                .await
                .is_err(),
            "the first attach was cut short"
        );
    }

    /// The digit column, and the whole of what it means: the session you are in
    /// is 1 and the one you came from is 2, so going back is always the same
    /// key however far around the machines you have walked.
    #[test]
    fn the_popup_numbers_the_sessions_you_have_been_in_most_recent_first() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build"), hosted("test"), hosted("web")],
            answered: vec![this_machine().to_string()],
        };
        let recent = [
            Located::new(as_listed(LOCAL), "test"),
            Located::new(as_listed(LOCAL), "build"),
        ];
        let listed = Listed::of(&snapshot, &Groups::default(), None, &recent);
        assert_eq!(numbered(&listed), [("test", 1), ("build", 2)]);
    }

    /// A session that has since ended has no row to number, and the digits are
    /// read off the box rather than off the trail: a gap in them would be a key
    /// that does nothing sitting in the middle of the ones that work.
    #[test]
    fn a_session_that_has_ended_leaves_no_gap_in_the_numbers() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build"), hosted("test")],
            answered: vec![this_machine().to_string()],
        };
        let recent = [
            Located::new(as_listed(LOCAL), "test"),
            Located::new(as_listed(LOCAL), "gone"),
            Located::new(as_listed(LOCAL), "build"),
        ];
        let listed = Listed::of(&snapshot, &Groups::default(), None, &recent);
        assert_eq!(numbered(&listed), [("test", 1), ("build", 2)]);
    }

    /// There are nine digits, and a run long enough to walk past them numbers
    /// the nine you were in most recently. The tenth is still in the list and
    /// still reachable with tab; it just has no key of its own.
    #[test]
    fn the_numbering_stops_at_the_last_digit_there_is() {
        let names: Vec<String> = (0..12).map(|n| format!("s{n}")).collect();
        let snapshot = Snapshot {
            sessions: names.iter().map(|n| hosted(n)).collect(),
            answered: vec![this_machine().to_string()],
        };
        let recent: Vec<Located> = names
            .iter()
            .map(|n| Located::new(as_listed(LOCAL), n))
            .collect();
        let listed = Listed::of(&snapshot, &Groups::default(), None, &recent);
        let numbers = numbered(&listed);
        assert_eq!(numbers.len(), 9);
        assert_eq!(numbers.last(), Some(&("s8", 9)));
    }
}
