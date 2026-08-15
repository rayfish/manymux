//! End-to-end tests of managing another machine.
//!
//! These drive the real `manymux` binary and stand a second machine up in the
//! same way the real thing works: a node with its own config directory and its
//! own socket, reached by running `mm agent` over "ssh". `MM_SSH` points
//! at a stub that runs the agent directly, so the tests cover the whole path
//! (CLI, ssh invocation, agent, remote node) without needing a real sshd or
//! working credentials on the test machine.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The binary under test, built by cargo for this integration test.
const MM: &str = env!("CARGO_BIN_EXE_mm");

/// Where `mm` is on the far machine, which is the one thing the stub ssh has
/// to model beyond running the agent.
///
/// A shell started by sshd for a command reads no profile, so its PATH is
/// whatever sshd hands out: a system install is on it and a per-user one is
/// not. That difference is the whole reason the client has more than one name
/// to try.
enum Mm {
    /// Installed with root, on the PATH every ssh gets.
    OnPath,
    /// Installed without root, reachable only by naming the path outright.
    InHome,
    /// Not installed, until the installer runs and leaves it in the home
    /// directory the way `install.sh` does when it cannot write `/usr/local/bin`.
    Missing,
    /// Beside the point: this machine cannot be reached at all, and ssh itself
    /// is the one complaining.
    Unreachable,
}

/// A temporary world: a local machine, and one reachable as `gpu-box`.
struct World {
    dir: PathBuf,
}

impl World {
    fn new(name: &str) -> Self {
        Self::where_mm_is(name, Mm::OnPath)
    }

    fn where_mm_is(name: &str, mm: Mm) -> Self {
        // Sockets live here, and a Unix socket path is capped around 104 bytes,
        // so keep it short rather than nesting under the target directory.
        let dir = std::env::temp_dir().join(format!("mm-t-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let world = Self { dir };
        world.write_stub_ssh(mm);
        world
    }

    /// A stand-in for ssh: drop the ssh options, take the destination, and run
    /// that machine's agent. What a real ssh does, minus the network and the
    /// authentication, both of which are ssh's business rather than ours.
    ///
    /// Unlike a real ssh it does not put the remote command through a shell, so
    /// `~` never expands and the words arrive exactly as the client sent them.
    /// That is what makes them worth matching on.
    fn write_stub_ssh(&self, mm: Mm) {
        // Exit 127 is what a shell says about a command it cannot find, and is
        // the only signal the client gets that a machine has no `mm`.
        let answers_to = match mm {
            Mm::OnPath => "mm) ;;",
            Mm::InHome => "*/mm) ;;",
            Mm::Missing => r#"*/mm) [ -f "$installed" ] || exit 127 ;;"#,
            // ssh's own failure, which has nothing to do with the ladder and
            // has to reach the terminal whatever the ladder is doing.
            Mm::Unreachable => {
                r#"*) echo "ssh: connect to host $host port 22: No route to host" >&2; exit 255 ;;"#
            }
        };
        let script = format!(
            r#"#!/bin/sh
host=
while [ $# -gt 0 ]; do
    case "$1" in
        -T|-t) shift ;;
        -o) shift 2 ;;
        --) shift; break ;;
        *) host="$1"; shift ;;
    esac
done
# `greet` runs `true` to get prompts out of the way; nothing to do for that.
if [ "$1" = "true" ]; then exit 0; fi

installed="{dir}/$host.installed"

# The installer, which lands in the home directory when it has no root.
case "$1" in
    *install.sh*) echo "$1" > "$installed"; exit 0 ;;
esac

# Which names this machine answers to. Anything else is not found, and a shell
# says so on stderr on its way out, which is the noise the client has to keep
# off the terminal while it is still working out how this machine is spelled.
case "$1" in
    {answers_to}
    *) echo "$1: command not found" >&2; exit 127 ;;
esac
exec env MM_CONFIG_DIR="{dir}/$host" "{mm}" --socket "{dir}/$host.sock" agent
"#,
            dir = self.dir.display(),
            mm = MM,
        );
        let path = self.ssh_stub();
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A stand-in for zsh, answering `print -rl -- $fpath` with the directories
    /// given and nothing else, and a PATH that finds it first.
    ///
    /// The real zsh answers for the machine the tests run on: an account that
    /// owns one of the directories its zsh was built to search, which is any
    /// Homebrew mac and most CI runners, would have the script installed there,
    /// outside the world these tests clean up.
    fn write_stub_zsh(&self, fpath: &[&Path]) -> std::ffi::OsString {
        let dir = self.dir.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "#!/bin/sh\n{}\n",
            fpath
                .iter()
                .map(|entry| format!("printf '%s\\n' '{}'", entry.display()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let path = dir.join("zsh");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut looked_in = dir.into_os_string();
        if let Some(rest) = std::env::var_os("PATH") {
            looked_in.push(":");
            looked_in.push(rest);
        }
        looked_in
    }

    /// What the installer was asked to run on `machine`, if it ran at all.
    fn installer_ran_on(&self, machine: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(format!("{machine}.installed"))).ok()
    }

    fn ssh_stub(&self) -> PathBuf {
        self.dir.join("ssh")
    }

    fn socket(&self, machine: &str) -> PathBuf {
        self.dir.join(format!("{machine}.sock"))
    }

    /// Run a manymux command as `machine`, with its own config and socket.
    fn run(&self, machine: &str, args: &[&str]) -> Output {
        Command::new(MM)
            .arg("--socket")
            .arg(self.socket(machine))
            .args(args)
            .env("MM_CONFIG_DIR", self.dir.join(machine))
            .env("MM_SSH", self.ssh_stub())
            // Keep the noise down; failures print stderr below.
            .env("MM_LOG", "manymux=warn")
            .output()
            .expect("running manymux")
    }

    /// Ask for the completions of a command line, as a shell would.
    ///
    /// Everything after `--` is the line being typed, and the last word is the
    /// one under the cursor. The fish adapter takes the index from the argument
    /// count, so nothing else has to be arranged.
    fn complete(&self, machine: &str, words: &[&str]) -> Vec<String> {
        let out = Command::new(MM)
            .arg("--")
            .arg("mm")
            .arg("--socket")
            .arg(self.socket(machine))
            .args(words)
            .env("COMPLETE", "fish")
            .env("MM_CONFIG_DIR", self.dir.join(machine))
            .env("MM_SSH", self.ssh_stub())
            .env("MM_LOG", "manymux=warn")
            .output()
            .expect("running manymux");
        assert!(
            out.status.success(),
            "completing {words:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            // Each line is `value` or `value<TAB>description`, and options are
            // offered alongside values; only the values are ours.
            .filter_map(|line| line.split('\t').next())
            .filter(|value| !value.starts_with('-'))
            .map(str::to_string)
            .collect()
    }

    /// Run a manymux command with a terminal on the other end of it, and answer
    /// whatever it asks with `typed`.
    ///
    /// A question only gets put to something that looks like a person, so a
    /// pipe cannot exercise the asking at all. Gives back whether the command
    /// succeeded and everything that appeared on the terminal.
    fn on_a_terminal(&self, machine: &str, args: &[&str], typed: &str) -> (bool, String) {
        use std::io::{Read, Write};

        let (mut pty, pts) = pty_process::blocking::open().unwrap();
        let mut child = pty_process::blocking::Command::new(MM)
            .arg("--socket")
            .arg(self.socket(machine))
            .args(args)
            .env("MM_CONFIG_DIR", self.dir.join(machine))
            .env("MM_SSH", self.ssh_stub())
            .env("MM_LOG", "manymux=warn")
            .spawn(pts)
            .expect("running manymux on a terminal");
        pty.write_all(typed.as_bytes()).unwrap();

        // A pty answers with EIO rather than EOF once nothing holds the far
        // end, so the error is how this ends rather than a problem.
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(read @ 1..) = pty.read(&mut buf) {
            seen.extend_from_slice(&buf[..read]);
        }
        let status = child.wait().expect("waiting for manymux");
        (
            status.success(),
            String::from_utf8_lossy(&seen).into_owned(),
        )
    }

    fn ok(&self, machine: &str, args: &[&str]) -> String {
        let out = self.run(machine, args);
        assert!(
            out.status.success(),
            "manymux {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Wait for a machine's node to be listening.
    fn wait_for_node(&self, machine: &str) {
        let socket = self.socket(machine);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if socket.exists() && self.run(machine, &["ls", "local"]).status.success() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{machine}'s node never came up");
    }

    /// Stop every node this world started.
    fn shut_down(&self) {
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "sock") {
                let machine = path.file_stem().unwrap().to_string_lossy().into_owned();
                // A node that is already gone leaves its socket behind, and a
                // test that ran on a terminal takes every node in the tree with
                // it when the terminal closes. Nothing left to kill, then.
                let listed = self.run(&machine, &["ls", "local"]);
                if !listed.status.success() {
                    continue;
                }
                // Killing sessions first stops orphaned shells outliving the test.
                let listed = String::from_utf8_lossy(&listed.stdout);
                for session in sessions(&listed) {
                    let _ = self.run(&machine, &["kill", &format!("local/{session}")]);
                }
            }
        }
        // The pattern must not start with `-`, or pkill reads it as a flag and
        // silently matches nothing, leaving nodes running after the test.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(self.dir.display().to_string())
            .status();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.shut_down();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The `TARGET` column of a `mm ls` table: exactly what you would type to
/// reach each session, which is the point of that column existing.
fn rows(table: &str) -> Vec<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

/// Just the session names, with any `host/` prefix taken off.
fn sessions(table: &str) -> Vec<String> {
    rows(table)
        .into_iter()
        .map(|target| match target.split_once('/') {
            Some((_, name)) => name.to_string(),
            None => target,
        })
        .collect()
}

#[test]
fn a_remote_machine_is_reached_by_running_the_agent_over_ssh() {
    let world = World::new("reach");

    // Nothing is running anywhere yet, and gpu-box has not been added. Naming
    // it should be enough, and its node has to come up on demand the way tmux
    // starts its server.
    let started = world.ok(
        "laptop",
        &["new", "-d", "-n", "api", "gpu-box", "sleep", "60"],
    );
    assert_eq!(started.trim(), "gpu-box/api");

    let listed = world.ok("laptop", &["ls", "gpu-box"]);
    assert_eq!(rows(&listed), vec!["gpu-box/api"], "{listed}");

    // And it is really running over there, not here.
    let here = world.ok("laptop", &["ls", "local"]);
    assert!(here.contains("no sessions"), "{here}");
}

/// One of these is left running on the far machine for every remote command if
/// it waits for both directions to end. The half reading ssh's stdin never ends
/// on its own, so the node hanging up has to be enough.
#[test]
fn an_agent_goes_away_when_the_node_hangs_up() {
    use std::io::Write;

    let world = World::new("agent-life");
    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);
    world.wait_for_node("laptop");

    let mut agent = Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .arg("agent")
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_LOG", "manymux=warn")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("running the agent");

    // A listing is a request the node answers and then hangs up on, which is
    // every one-shot command. Its stdin stays open throughout, exactly as an
    // ssh channel nobody has closed yet would.
    let body = manymux::proto::encode(&manymux::proto::Request::List).unwrap();
    let mut request = vec![manymux::proto::tag::REQUEST];
    request.extend_from_slice(&(body.len() as u32).to_be_bytes());
    request.extend_from_slice(&body);
    let mut stdin = agent.stdin.take().expect("the agent's stdin");
    stdin.write_all(&request).unwrap();
    stdin.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let gone = loop {
        match agent.try_wait().unwrap() {
            Some(_) => break true,
            None if Instant::now() > deadline => break false,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    if !gone {
        let _ = agent.kill();
    }
    assert!(gone, "the agent outlived the connection it was relaying");
}

#[test]
fn one_listing_covers_this_machine_and_the_added_ones() {
    let world = World::new("listing");

    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);
    world.wait_for_node("laptop");
    // Starting a session on gpu-box is what puts it in the default listing;
    // nothing was added by hand.
    world.ok(
        "laptop",
        &["new", "-d", "-n", "there", "gpu-box", "sleep", "60"],
    );

    let listed = world.ok("laptop", &["ls"]);
    let found = rows(&listed);
    assert!(
        // This machine's own sessions carry no prefix: `mm attach here`.
        found.contains(&"here".to_string()),
        "this machine's own session is missing: {listed}"
    );
    assert!(
        found.contains(&"gpu-box/there".to_string()),
        "the added machine's session is missing: {listed}"
    );
}

/// Using a machine is enough to have it remembered; `mm add` is only for
/// machines you want listed without starting anything on them.
#[test]
fn starting_a_session_somewhere_remembers_that_machine() {
    let world = World::new("remember");

    let hosts = world.ok("laptop", &["hosts"]);
    assert!(hosts.contains("no machines added"), "{hosts}");

    world.ok(
        "laptop",
        &["new", "-d", "-n", "x", "gpu-box", "sleep", "60"],
    );

    let hosts = world.ok("laptop", &["hosts"]);
    assert!(
        hosts.contains("gpu-box"),
        "the machine was not remembered: {hosts}"
    );
}

#[test]
fn a_bare_name_finds_a_session_on_another_machine() {
    let world = World::new("locate");

    world.ok(
        "laptop",
        &["new", "-d", "-n", "solo", "gpu-box", "sleep", "60"],
    );

    // No host given: it should be found on gpu-box and renamed there.
    world.ok("laptop", &["rename", "solo", "renamed remotely"]);
    let listed = world.ok("laptop", &["ls", "gpu-box"]);
    assert!(listed.contains("renamed remotely"), "{listed}");
}

#[test]
fn a_machine_that_cannot_be_reached_is_reported_not_hidden() {
    let world = World::new("unreachable");

    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);
    world.ok("laptop", &["add", "gpu-box"]);

    // Break the way out: ssh itself fails, exactly as it would for a machine
    // that is switched off or refusing us.
    std::fs::write(world.ssh_stub(), "#!/bin/sh\nexit 255\n").unwrap();

    let out = world.run("laptop", &["ls"]);
    let listed = String::from_utf8_lossy(&out.stdout);
    let complaint = String::from_utf8_lossy(&out.stderr);
    assert!(
        listed.contains("here"),
        "the machine that is up should still be listed: {listed}"
    );
    assert!(
        complaint.contains("gpu-box"),
        "the machine that is down should be named: {complaint}"
    );
}

/// Worse than a machine that refuses: one that swallows the connection. ssh
/// spends minutes on a TCP connect to an address nothing answers at, once per
/// address the name has, and every other machine's sessions used to sit behind
/// that wait.
#[test]
fn a_machine_that_never_answers_does_not_hold_up_the_others() {
    let world = World::new("silent");

    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);
    world.ok("laptop", &["add", "gpu-box"]);

    // Not a refusal, not an answer: the connection is simply taken and kept,
    // the way a machine that is asleep or off the network takes it. `exec`, so
    // that giving up on the stub is giving up on the whole of it, as it is for
    // a real ssh: a leftover grandchild would sit on the inherited stderr and
    // hold this test open long after the listing had moved on.
    std::fs::write(world.ssh_stub(), "#!/bin/sh\nexec sleep 600\n").unwrap();

    let started = Instant::now();
    let out = world.run("laptop", &["ls"]);
    let waited = started.elapsed();
    let listed = String::from_utf8_lossy(&out.stdout);
    let complaint = String::from_utf8_lossy(&out.stderr);

    assert!(
        waited < Duration::from_secs(60),
        "waited {waited:?} on a machine that never answered"
    );
    assert!(
        listed.contains("here"),
        "the machine that is up should still be listed: {listed}"
    );
    assert!(
        complaint.contains("gpu-box"),
        "the machine that never answered should be named: {complaint}"
    );
}

#[test]
fn a_session_survives_the_connection_that_started_it() {
    let world = World::new("survive");

    world.ok(
        "laptop",
        &["new", "-d", "-n", "long", "gpu-box", "sleep", "60"],
    );
    // Every ssh from that command is gone by now: `new -d` exits immediately.
    // The session is still there because the remote node owns its PTY.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(200));
        let listed = world.ok("laptop", &["ls", "gpu-box"]);
        assert!(
            listed.contains("long"),
            "the session died with ssh: {listed}"
        );
    }
}

/// The gap this closes: replacing the binary leaves the node running the old
/// one, so a machine can be up to date and still behave like the old build.
/// Nothing else restarts it, since `mm new` reuses whatever node is listening.
#[test]
fn a_restart_replaces_the_node_and_takes_its_sessions_with_it() {
    let world = World::new("restart");

    world.ok("laptop", &["new", "-d", "-n", "doomed", "sleep", "60"]);

    // Sessions are the node's children, so they cannot outlive it. Nothing is
    // on the other end of stdin here, so it refuses and names the flag rather
    // than putting a prompt into a pipe.
    let refused = world.ok("laptop", &["restart"]);
    assert!(
        refused.contains("1 running session"),
        "a restart should say what it would cost: {refused}"
    );
    let listed = world.ok("laptop", &["ls", "local"]);
    assert!(
        listed.contains("doomed"),
        "the session was killed: {listed}"
    );

    let restarted = world.ok("laptop", &["restart", "--force"]);
    assert!(
        restarted.contains("restarted the node"),
        "the restart said nothing: {restarted}"
    );

    // A node is listening again, and it is a new one: the sessions the old node
    // owned are gone with it.
    world.wait_for_node("laptop");
    let listed = world.ok("laptop", &["ls", "local"]);
    assert!(
        !listed.contains("doomed"),
        "the session outlived the node that owned it: {listed}"
    );
}

/// `stop` and `start` are the two halves of a restart, for when you want one
/// without the other: a machine to leave quiet, or one to have ready.
#[test]
fn a_stop_ends_the_node_and_a_start_brings_one_back() {
    let world = World::new("stopstart");

    let quiet = world.ok("laptop", &["stop"]);
    assert!(
        quiet.contains("no node is running"),
        "nothing to stop is not an error: {quiet}"
    );

    world.ok("laptop", &["new", "-d", "-n", "doomed", "sleep", "60"]);
    let refused = world.ok("laptop", &["stop"]);
    assert!(
        refused.contains("1 running session"),
        "a stop should say what it would cost: {refused}"
    );

    let stopped = world.ok("laptop", &["stop", "--force"]);
    assert!(stopped.contains("stopped the node"), "{stopped}");
    assert!(
        !world.run("laptop", &["ls", "local"]).status.success(),
        "the node is still answering after being stopped"
    );

    let started = world.ok("laptop", &["start"]);
    assert!(started.contains("started the node"), "{started}");
    let listed = world.ok("laptop", &["ls", "local"]);
    assert!(
        !listed.contains("doomed"),
        "the session outlived the node that owned it: {listed}"
    );

    let again = world.ok("laptop", &["start"]);
    assert!(
        again.contains("already running"),
        "starting a running node is a no-op, not a second node: {again}"
    );
}

/// A machine with no node is the ordinary state of one nobody has used yet, and
/// asking it to restart is not an error there: it has to end up with a node
/// running either way.
#[test]
fn a_restart_with_no_node_running_starts_one() {
    let world = World::new("restart-cold");

    let started = world.ok("laptop", &["restart"]);
    assert!(
        started.contains("started one"),
        "a cold restart should say it started a node: {started}"
    );
    world.wait_for_node("laptop");
}

/// The stub stands in for ssh, so this checks the thing the stub cannot: that
/// what manymux hands to ssh is a plain destination plus `mm agent`.
#[test]
fn ssh_is_invoked_with_the_destination_and_nothing_surprising() {
    let world = World::new("argv");
    let recorded = world.dir.join("argv");
    std::fs::write(
        world.ssh_stub(),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 255\n",
            recorded.display()
        ),
    )
    .unwrap();

    let _ = world.run("laptop", &["ls", "gpu-box"]);

    let argv = std::fs::read_to_string(&recorded).expect("ssh was invoked");
    let args: Vec<&str> = argv.lines().collect();
    assert!(args.contains(&"gpu-box"), "no destination: {args:?}");
    assert!(args.contains(&"mm"), "does not run mm: {args:?}");
    assert!(args.contains(&"agent"), "does not run the agent: {args:?}");
    assert!(
        args.contains(&"-T"),
        "a PTY would corrupt the protocol: {args:?}"
    );
    assert!(
        args.iter().any(|a| a.starts_with("ControlMaster")),
        "connections should be shared: {args:?}"
    );
}

/// Sanity check that the stub really is a stand-in and not the thing under
/// test: with no `MM_SSH` at all, manymux must still try to run `ssh`.
#[test]
fn the_real_ssh_is_the_default() {
    let dir = std::env::temp_dir().join(format!("mm-t-default-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(MM)
        .arg("--socket")
        .arg(dir.join("laptop.sock"))
        .args(["ls", "definitely-not-a-real-host.invalid"])
        .env("MM_CONFIG_DIR", &dir)
        .env("MM_LOG", "manymux=warn")
        .env_remove("MM_SSH")
        .output()
        .expect("running manymux");

    assert!(!out.status.success(), "an unreachable host should fail");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tab completion knows what is running, which is the whole point of it asking
/// the binary rather than reading a script written months ago.
#[test]
fn a_tab_offers_the_sessions_on_this_machine_and_a_door_to_the_others() {
    let world = World::new("tab");

    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);
    world.ok(
        "laptop",
        &["new", "-d", "-n", "there", "gpu-box", "sleep", "60"],
    );

    let offered = world.complete("laptop", &["a", ""]);
    assert!(offered.contains(&"here".to_string()), "{offered:?}");
    assert!(offered.contains(&"gpu-box/".to_string()), "{offered:?}");
    // A tab must not go out over ssh until it is told which machine.
    assert!(
        !offered.contains(&"gpu-box/there".to_string()),
        "a bare tab reached another machine: {offered:?}"
    );
}

#[test]
fn naming_a_machine_completes_the_sessions_on_it() {
    let world = World::new("tab-remote");

    world.ok(
        "laptop",
        &["new", "-d", "-n", "there", "gpu-box", "sleep", "60"],
    );

    assert_eq!(
        world.complete("laptop", &["a", "gpu-box/"]),
        ["gpu-box/there"]
    );
    assert_eq!(
        world.complete("laptop", &["a", "gpu-box/th"]),
        ["gpu-box/there"]
    );
    assert!(world.complete("laptop", &["a", "gpu-box/x"]).is_empty());
    // A machine that was never added is not one to go asking.
    assert!(world.complete("laptop", &["a", "nowhere/"]).is_empty());
}

/// The short forms are what anyone actually types.
#[test]
fn the_aliases_complete_targets_too() {
    let world = World::new("tab-alias");

    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);

    for command in ["attach", "a", "kill", "k", "rename", "r"] {
        let offered = world.complete("laptop", &[command, ""]);
        assert!(
            offered.contains(&"here".to_string()),
            "{command} offered {offered:?}"
        );
    }
}

#[test]
fn machines_complete_where_a_machine_is_wanted() {
    let world = World::new("tab-hosts");

    world.ok("laptop", &["add", "gpu-box"]);

    let listed = world.complete("laptop", &["ls", ""]);
    assert!(listed.contains(&"local".to_string()), "{listed:?}");
    assert!(listed.contains(&"gpu-box".to_string()), "{listed:?}");
    // `rm` takes a machine off the list, and this one is not on it.
    assert_eq!(world.complete("laptop", &["rm", ""]), ["gpu-box"]);
}

/// A machine that is not answering costs a pause, not the shell.
#[test]
fn a_tab_gives_up_on_a_machine_that_does_not_answer() {
    let world = World::new("tab-slow");

    world.ok("laptop", &["add", "gpu-box"]);
    std::fs::write(world.ssh_stub(), "#!/bin/sh\nsleep 60\n").unwrap();

    let started = Instant::now();
    let offered = world.complete("laptop", &["a", "gpu-box/"]);
    assert!(offered.is_empty(), "{offered:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited {:?} on a machine that never answered",
        started.elapsed()
    );
}

/// Pressing tab is not asking for a daemon.
#[test]
fn a_tab_does_not_start_a_node() {
    let world = World::new("tab-quiet");

    let offered = world.complete("laptop", &["a", ""]);
    assert!(offered.is_empty(), "{offered:?}");
    assert!(
        !world.socket("laptop").exists(),
        "a tab started this machine's node"
    );
}

/// The installed script is the stub that asks the binary, in the place each
/// shell looks for it: what zsh says it searches, and the XDG directory for the
/// shells that read one wherever they run.
#[test]
fn installing_writes_a_script_that_asks_the_binary() {
    let world = World::new("tab-install");
    let home = world.dir.join("home");
    let searched = world.dir.join("site-functions");
    let path = world.write_stub_zsh(&[&searched]);

    for (shell, at) in [
        ("zsh", searched.join("_mm")),
        ("bash", home.join("data/bash-completion/completions/mm")),
    ] {
        let out = Command::new(MM)
            .args(["completions", shell, "--install"])
            .env("PATH", &path)
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("MM_CONFIG_DIR", world.dir.join("laptop"))
            .output()
            .expect("running manymux");
        assert!(
            out.status.success(),
            "installing for {shell}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let script = std::fs::read_to_string(&at)
            .unwrap_or_else(|e| panic!("{shell} script not installed: {e}"));
        assert!(script.contains("COMPLETE"), "{shell}: {script}");
        if shell == "zsh" {
            assert!(script.starts_with("#compdef mm"), "{script}");
            // Autoloaded from fpath, so the first tab has to be answered by
            // hand rather than lost to `compdef` taking effect too late.
            assert!(script.contains("_comps[mm]"), "{script}");
            // Nothing to add to a file for a directory zsh already reads.
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(!said.contains("fpath=("), "{said}");
        }
    }
}

/// Where zsh searches nowhere this account can write, which is the usual shared
/// Linux box, the script goes in the XDG directory with the line to add.
#[test]
fn a_zsh_that_searches_nowhere_of_ours_gets_the_xdg_directory_and_a_line() {
    let world = World::new("tab-install-xdg");
    let home = world.dir.join("home");
    let path = world.write_stub_zsh(&[]);

    let out = Command::new(MM)
        .args(["completions", "zsh", "--install"])
        .env("PATH", &path)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .output()
        .expect("running manymux");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let at = home.join("data/zsh/site-functions/_mm");
    let script = std::fs::read_to_string(&at).unwrap_or_else(|e| panic!("not installed: {e}"));
    assert!(script.starts_with("#compdef mm"), "{script}");

    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains(&format!("fpath=({}", at.parent().unwrap().display())),
        "{said}"
    );
}

/// Nothing in these tests should have left a node running.
#[test]
fn nodes_do_not_outlive_their_world() {
    let dir = {
        let world = World::new("cleanup");
        world.ok("laptop", &["new", "-d", "-n", "x", "sleep", "60"]);
        world.dir.clone()
    };
    assert!(!Path::new(&dir).exists(), "the world was not cleaned up");
}

/// An install without root lands in `~/.local/bin`, which no non-interactive
/// ssh has on its PATH. Naming the path outright is the only way to reach such
/// a machine, and it costs nothing on the machines where a bare `mm` works.
#[test]
fn a_machine_with_mm_only_in_its_home_directory_is_still_reached() {
    let world = World::where_mm_is("in-home", Mm::InHome);

    let started = world.ok(
        "laptop",
        &["new", "-d", "-n", "api", "gpu-box", "sleep", "60"],
    );
    assert_eq!(started.trim(), "gpu-box/api");

    // Finding it did not involve putting anything on the machine.
    assert_eq!(world.installer_ran_on("gpu-box"), None);
}

/// Working out how a machine spells `mm` is nobody's business but the client's.
/// The first name tried comes back 127 with the remote shell saying so, and
/// that line, printed, lands on the terminal of every single command that
/// reaches such a machine, including one about to repaint a session over it.
#[test]
fn finding_mm_elsewhere_is_done_without_saying_anything() {
    let world = World::where_mm_is("quiet-ladder", Mm::InHome);

    let out = world.run("laptop", &["ls", "gpu-box"]);
    assert!(out.status.success(), "listing a reachable machine failed");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !said.contains("command not found"),
        "the probe was overheard: {said}"
    );
}

/// The other half of that, and the reason the noise is held rather than thrown
/// away at the source: ssh has failures of its own to report, and its account
/// is the only one there is.
#[test]
fn a_machine_ssh_cannot_reach_still_says_why() {
    let world = World::where_mm_is("no-route", Mm::Unreachable);

    let out = world.run("laptop", &["ls", "gpu-box"]);
    assert!(!out.status.success(), "an unreachable machine was listed");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("No route to host"),
        "ssh's reason went missing: {said}"
    );
}

/// Fetching a script onto someone else's machine is not something to do on the
/// strength of a command that never mentioned it.
#[test]
fn a_machine_without_mm_is_left_alone_when_there_is_nobody_to_ask() {
    let world = World::where_mm_is("no-mm-quiet", Mm::Missing);

    let out = world.run(
        "laptop",
        &["new", "-d", "-n", "api", "gpu-box", "sleep", "60"],
    );
    assert!(
        !out.status.success(),
        "a machine with no mm started a session"
    );
    assert_eq!(world.installer_ran_on("gpu-box"), None);

    // Nothing was asked, so say what would have done it.
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("mm setup gpu-box"), "{said}");
}

/// The point of the whole thing: a machine you can ssh into is a machine you
/// can start a session on, without a separate trip to set it up first.
#[test]
fn saying_yes_puts_mm_on_a_machine_that_has_none() {
    let world = World::where_mm_is("no-mm-yes", Mm::Missing);

    let (ok, seen) = world.on_a_terminal(
        "laptop",
        &["new", "-d", "-n", "api", "gpu-box", "sleep", "60"],
        "y\n",
    );
    assert!(ok, "{seen}");
    assert!(seen.contains("gpu-box/api"), "{seen}");

    let installer = world
        .installer_ran_on("gpu-box")
        .expect("nothing was installed");
    assert!(installer.contains("install.sh"), "{installer}");
}

/// Answering the question is what makes it a question.
#[test]
fn saying_no_leaves_the_machine_as_it_was() {
    let world = World::where_mm_is("no-mm-no", Mm::Missing);

    let (ok, seen) = world.on_a_terminal(
        "laptop",
        &["new", "-d", "-n", "api", "gpu-box", "sleep", "60"],
        "n\n",
    );
    assert!(!ok, "{seen}");
    assert_eq!(world.installer_ran_on("gpu-box"), None);
}

/// Pressing tab is not asking to have software put on anything.
#[test]
fn a_tab_never_installs_anything() {
    // Watched from back when it had mm on it, which is the only way a tab goes
    // out to a machine at all.
    let world = World::new("tab-no-mm");
    world.ok("laptop", &["add", "gpu-box"]);
    world.write_stub_ssh(Mm::Missing);

    let offered = world.complete("laptop", &["a", "gpu-box/"]);
    assert!(offered.is_empty(), "{offered:?}");
    assert_eq!(world.installer_ran_on("gpu-box"), None);
}

/// Setting a machine up before anything needs it, which is also the only way a
/// script gets it done.
#[test]
fn setup_puts_mm_on_a_machine() {
    let world = World::where_mm_is("setup", Mm::Missing);

    world.ok("laptop", &["setup", "gpu-box"]);

    let installer = world
        .installer_ran_on("gpu-box")
        .expect("nothing was installed");
    assert!(installer.contains("install.sh"), "{installer}");
}

/// A setting is worth nothing if it does not survive the command that set it,
/// and `mm config` is the only way to know a node will be quiet.
#[test]
fn a_setting_is_written_where_the_next_command_reads_it() {
    let world = World::new("config");

    assert_eq!(
        world.ok("laptop", &["config"]).trim(),
        "notify on\nscreen alternate"
    );

    world.ok("laptop", &["config", "notify", "off"]);
    assert_eq!(world.ok("laptop", &["config", "notify"]).trim(), "off");
    assert_eq!(
        world.ok("laptop", &["config"]).trim(),
        "notify off\nscreen alternate"
    );

    // A refusal rather than a file with a typo in it that changes nothing.
    let out = world.run("laptop", &["config", "notify", "maybe"]);
    assert!(!out.status.success());
    assert_eq!(world.ok("laptop", &["config", "notify"]).trim(), "off");

    // And a tab knows what there is to set, without asking a node anything,
    // including that what a setting takes depends on which one it is.
    assert_eq!(
        world.complete("laptop", &["config", ""]),
        vec!["notify", "screen"]
    );
    assert_eq!(
        world.complete("laptop", &["config", "notify", ""]),
        vec!["on", "off"]
    );
    assert_eq!(
        world.complete("laptop", &["config", "screen", ""]),
        vec!["alternate", "inline"]
    );
}

/// The whole path on a real terminal: a bell in one session, and an OSC 9 on
/// the terminal of whoever is attached to another one next door. Everything
/// under it is tested in pieces; this is the only thing that shows the pieces
/// are joined up.
#[test]
fn a_bell_next_door_lands_on_the_terminal_of_whoever_is_attached() {
    use std::io::Read;
    use std::sync::mpsc;

    let world = World::new("bell-terminal");
    world.ok("laptop", &["new", "-d", "-n", "quiet", "sh"]);
    world.wait_for_node("laptop");

    let (mut pty, pts) = pty_process::blocking::open().unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "quiet"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

    // Read the terminal on a thread of its own: the point of the test is what
    // arrives without anyone typing, and a read on a pty blocks until it does.
    let (seen_tx, seen_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(read @ 1..) = pty.read(&mut buf) {
            if seen_tx.send(buf[..read].to_vec()).is_err() {
                return;
            }
        }
    });

    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rung = false;
    let notification = loop {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        // Ring only once the client is on the screen, or the bell would be over
        // before there was anybody attached to tell about it.
        if !rung && !seen.is_empty() {
            rung = true;
            world.ok(
                "laptop",
                &[
                    "new",
                    "-d",
                    "-n",
                    "ringer",
                    "sh",
                    "-c",
                    "printf 'x\\a'; sleep 30",
                ],
            );
        }
        if let Some(at) = seen.find("\x1b]9;") {
            break seen[at..].to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no notification reached the terminal; saw: {seen:?}"
        );
    };

    let end = notification.find('\x07').expect("a finished sequence");
    assert!(
        notification[..end].contains("ringer"),
        "the notification does not name the session that rang: {notification:?}"
    );
    // And the row says so too, for a terminal that raises nothing.
    assert!(
        seen.contains("ringer"),
        "the status row never mentioned it: {seen:?}"
    );

    let _ = client.kill();
    let _ = client.wait();
}

/// Resizing the window repaints the session at the size it now has. Nothing
/// else will: a shell that printed and went quiet redraws nothing for a
/// SIGWINCH, so what the terminal keeps of the old screen is all there is, and
/// the marks the client left on rows that have moved stay where they were.
#[test]
fn a_resize_repaints_the_screen_at_the_size_it_now_has() {
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("resize-terminal");
    world.ok(
        "laptop",
        &[
            "new",
            "-d",
            "-n",
            "grown",
            "sh",
            "-c",
            "printf 'HELLO\\n'; sleep 30",
        ],
    );
    world.wait_for_node("laptop");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "grown"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

    // The reader gets a descriptor of its own, because the resize below is on
    // this thread and a read on a pty blocks until something arrives.
    let (seen_tx, seen_rx) = mpsc::channel();
    let mut reading =
        unsafe { pty_process::blocking::Pty::from_fd(pty.as_fd().try_clone_to_owned().unwrap()) };
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(read @ 1..) = reading.read(&mut buf) {
            if seen_tx.send(buf[..read].to_vec()).is_err() {
                return;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    while !seen.contains("HELLO") {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            Instant::now() < deadline,
            "the session never reached the terminal; saw: {seen:?}"
        );
    }

    // Everything from here is the answer to the resize alone.
    seen.clear();
    pty.resize(pty_process::Size::new(40, 100)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    while !seen.contains("HELLO") {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            Instant::now() < deadline,
            "the resize never repainted the session; saw: {seen:?}"
        );
    }
    // Painted onto a screen wiped first, or the mark drawn on what used to be
    // the bottom row is still sitting there.
    let painted = seen.find("HELLO").unwrap();
    assert!(
        seen[..painted].contains("\x1b[2J"),
        "the repaint did not erase what the old size left behind: {seen:?}"
    );
    // And the session is fenced into the rows above the mark at the new height.
    assert!(
        seen.contains("\x1b[1;39r"),
        "the scrolling region was not re-fenced: {seen:?}"
    );

    let _ = client.kill();
    let _ = client.wait();
}

/// `mm a devbox` reads as "put me on devbox", and there is usually one thing
/// running there. Naming the session as well repeats a lookup you have just
/// done with `mm ls`.
#[test]
fn a_machine_on_its_own_attaches_to_the_first_session_on_it() {
    let world = World::new("attach-machine");

    world.ok(
        "laptop",
        &["new", "-d", "-n", "first", "gpu-box", "sleep", "60"],
    );
    world.ok(
        "laptop",
        &["new", "-d", "-n", "second", "gpu-box", "sleep", "60"],
    );
    // A session here too, to be sure the machine is what chose between them.
    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);

    // Ctrl-] d leaves again, so the client exits on its own and says where it
    // had been.
    let (left, seen) = world.on_a_terminal("laptop", &["attach", "gpu-box"], "\x1dd");
    assert!(left, "attaching to gpu-box by name failed: {seen}");
    assert!(
        seen.contains("detached from gpu-box/first"),
        "should have taken the first session on gpu-box: {seen}"
    );
}

/// The machine has to be one that is already watched. Anything else is a
/// session name that is not running, and going out to ssh for a typo would
/// hang on a name that resolves to nothing.
#[test]
fn a_word_that_names_no_machine_is_still_a_missing_session() {
    let world = World::new("attach-typo");

    world.ok("laptop", &["new", "-d", "-n", "here", "sleep", "60"]);

    let out = world.run("laptop", &["attach", "not-a-machine"]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("no session named"), "{said}");
}

/// A machine being watched but holding nothing says so, rather than reporting
/// a session name that was never the point.
#[test]
fn a_machine_with_nothing_on_it_says_so() {
    let world = World::new("attach-empty");

    world.ok("laptop", &["add", "gpu-box"]);

    let out = world.run("laptop", &["attach", "gpu-box"]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("nothing is running on gpu-box"), "{said}");
}

/// Attaching to whatever is first is fine, because you can leave again. Killing
/// or renaming whatever is first is not, so only attach reads a bare machine
/// name that way.
#[test]
fn only_attach_takes_a_machine_where_a_session_is_expected() {
    let world = World::new("attach-only");

    world.ok(
        "laptop",
        &["new", "-d", "-n", "only", "gpu-box", "sleep", "60"],
    );

    for command in ["kill", "rename"] {
        let mut args = vec![command, "gpu-box"];
        if command == "rename" {
            args.push("whatever");
        }
        let out = world.run("laptop", &args);
        assert!(!out.status.success(), "{command} took a machine name");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains("no session named"), "{command}: {said}");
    }
}
