use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueHint};
use clap_complete::Shell;
use tokio::signal::unix::{SignalKind, signal};
use tracing::debug;

use manymux::client::attach;
use manymux::client::groups::Groups;
use manymux::client::{Consent, Stream};
use manymux::hosts::{Hosts, LOCAL, is_this_machine, qualified, this_machine};
use manymux::node::{Config, Node};
use manymux::proto::{Request, Response, SpawnSpec};
use manymux::settings::{Screen, Settings};
use manymux::update::Channel;
use manymux::{config, log, style, term};

mod attaching;
mod checkpointing;
mod complete;
mod completions;
mod target;

use attaching::{chosen, do_attach};
use checkpointing::{restore_checkpoint, save_checkpoint, show_checkpoint};

use target::{
    Bare, Listing, Started, answer_from, everywhere, locate, note_reached, sessions_on,
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
        #[command(flatten)]
        screen: ScreenArg,
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
        #[command(flatten)]
        screen: ScreenArg,
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

/// `--screen`, shared by `new` and `attach`.
#[derive(Args)]
struct ScreenArg {
    /// Whose screen to paint on. `alternate` gives the session a screen of
    /// its own; `inline` paints on the terminal's, so its scrollback keeps
    /// the session's history and the wheel scrolls it. Defaults to the
    /// `screen` setting.
    #[arg(long, value_enum)]
    screen: Option<Screen>,
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
            let name = spawn(open_or_start(&socket, &host).await?, spec).await?;
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
            do_attach(&socket, &host, &name, chosen(screen.screen), false).await
        }

        Command::Attach { target, screen } => {
            let target = locate(&socket, &target, Bare::OrMachine).await?;
            do_attach(
                &socket,
                &target.host,
                &target.session,
                chosen(screen.screen),
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
            Groups::change(|groups| groups.set(name.as_deref(), &at.host, session))?;
            Ok(OK)
        }

        Command::Groups => {
            let before = Groups::load()?;
            let listing = everywhere(&socket).await?;
            let mut groups = Groups::default();
            // The listing is the only thing that says which sessions are still
            // running, so this is where the file gets tidied.
            groups.refresh(&before, &listing.answering(), &listing.sessions)?;
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
            // Adding a name already on the list is not an error and never has
            // been, and it cannot be a *new* duplicate of anything: the check
            // below is skipped for it, or a machine would refuse to be added a
            // second time on the grounds of being itself.
            let listed_already = hosts.has(&host);
            hosts.add(&host)?;
            // Connect once with the terminal attached, so ssh can ask about an
            // unknown host key or a passphrase. Every later command carries the
            // protocol on stdin and has no way to prompt.
            manymux::ssh::greet(&host).await?;
            // Prove it works now rather than at the next listing, when the
            // failure would be harder to connect to what you just typed.
            let answer = match answer_from(&socket, &host).await {
                Ok(answer) => answer,
                Err(e) => {
                    hosts.save()?;
                    eprintln!("mm: added {host}, but could not reach it: {e:#}");
                    return Ok(FAILED);
                }
            };
            // Two ssh destinations can be one machine, and the listing that
            // results has every session on it in twice. Caught here, where the
            // answer is a name you already have, rather than left to be
            // explained by every `mm ls` from now on.
            //
            // Only when both ends said which node they are, and a node says so
            // on its session rows: a machine with nothing running on it says
            // nothing, and neither does one too old to have the field. Adding a
            // second name for an idle machine goes through, and `mm ls` says so
            // as soon as there is a session on it. The alternative is a request
            // of its own, which every listing would then pay a second ssh per
            // machine for, the popup relisting at every keypress.
            //
            // The fan-out this costs is one machine's worth of ssh apiece, once,
            // on a command that has just run `greet` on a terminal. Only for a
            // name that is actually new.
            if !listed_already
                && let Some(node) = &answer.node
                && let Some(already) = everywhere(&socket).await?.watched_as(node)
            {
                if is_this_machine(already) {
                    bail!("`{host}` reaches this machine, which is always listed");
                }
                bail!(
                    "`{host}` and `{already}` are the same machine, which is on the list already"
                );
            }
            hosts.save()?;
            println!("watching {host} ({} sessions)", answer.sessions.len());
            Ok(OK)
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

/// How [`reach`] gets to a machine.
#[derive(Clone, Copy)]
struct Reach {
    /// Start this machine's node first if it is not running. Only for asking a
    /// machine to hold something new: `tmux new` starts its server too, while
    /// `tmux ls` does not. On another machine the agent does the same thing at
    /// the far end.
    start: bool,
    /// Keep what ssh says, for a caller with somewhere better to put it.
    ///
    /// Two of them. An attach paints a session over the screen and a dropped
    /// connection leaves that paint exactly where it was, which is what a wait
    /// is for; ssh printing its account of a machine it cannot reach lands
    /// there, once per attempt, on a terminal in raw mode. And a listing asks
    /// every watched machine at once, so the same account arrives interleaved
    /// with the other machines' answers, from a client that may well be an
    /// attached one: see [`sessions_on`]. Kept instead, the same words go into
    /// the error, which reaches the person on the first attach of a run, the
    /// log on every reconnect after it, and the `mm: <host>: <why>` line under
    /// a listing. See [`manymux::client::Heard`].
    hushed: bool,
    /// Offer to put `mm` on a machine that has none ([`offer_to_install`]).
    install: bool,
}

/// Open a stream to a machine: this one's socket, or `mm agent` over ssh.
async fn reach(socket: &Path, host: &str, how: Reach) -> Result<Stream> {
    if is_this_machine(host) {
        if how.start {
            manymux::node::ensure_running(socket).await?;
        }
        return Stream::local(socket).await;
    }
    let consent: Option<Consent> = how.install.then(|| Arc::new(offer_to_install) as Consent);
    if how.hushed {
        Stream::over_ssh_hushed(host, consent).await
    } else {
        Stream::over_ssh(host, consent).await
    }
}

/// Open a stream to a machine, asking before installing anything there.
async fn open(socket: &Path, host: &str) -> Result<Stream> {
    let how = Reach {
        start: false,
        hushed: false,
        install: true,
    };
    reach(socket, host, how).await
}

/// Like [`open`], keeping what ssh says for the error: see [`Reach::hushed`].
async fn open_hushed(socket: &Path, host: &str) -> Result<Stream> {
    let how = Reach {
        start: false,
        hushed: true,
        install: true,
    };
    reach(socket, host, how).await
}

/// Like [`open`], but starts this machine's node first: see [`Reach::start`].
async fn open_or_start(socket: &Path, host: &str) -> Result<Stream> {
    let how = Reach {
        start: true,
        hushed: false,
        install: true,
    };
    reach(socket, host, how).await
}

/// Start a session on a stream, and answer with the name the node gave it.
async fn spawn(mut stream: Stream, spec: SpawnSpec) -> Result<String> {
    let Response::Spawned { name } = stream.call(&Request::Spawn(spec)).await? else {
        bail!("unexpected response to spawn");
    };
    Ok(name)
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

/// `mm ls`, over every watched machine or just one.
async fn list(socket: &Path, host: Option<String>) -> Result<u8> {
    let before = Groups::load().unwrap_or_default();
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
            listing.add(label, answer_from(socket, &name).await);
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
    let _ = groups.refresh(&before, &listing.answering(), &listing.sessions);

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
    // The rows above have every session on that machine in them twice, and
    // nothing in the table says why. This is the only thing that does.
    for both in &listing.duplicates {
        eprintln!(
            "mm: {} and {} are the same machine, so its sessions are listed \
             twice; `mm rm {}`",
            both.also, both.known_as, both.also
        );
    }
    Ok(if nothing_answered { FAILED } else { OK })
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
