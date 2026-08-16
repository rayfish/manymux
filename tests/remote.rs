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

/// The sessions in a `mm ls` table that have a client attached, by the column
/// that says so. The dot is the whole of it: it appears nowhere else on the
/// row, and the hollow one beside it means nobody is there.
fn watched(table: &str) -> Vec<String> {
    table
        .lines()
        .skip(1)
        .filter(|line| line.contains('●'))
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
    world.ok("laptop", &["rename", "solo", "moved"]);
    let listed = world.ok("laptop", &["ls", "gpu-box"]);
    assert!(listed.contains("gpu-box/moved"), "{listed}");
    assert!(
        !listed.contains("solo"),
        "the old name is nobody's: {listed}"
    );
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
    // Without these, a connection that dies without closing is one ssh never
    // notices: it holds the pipes open and silent for as long as the process
    // lives. Worse with a shared connection than without, because the master
    // outlives the command that made it and every later command multiplexes
    // onto the corpse, where no `ConnectTimeout` applies.
    assert!(
        args.iter().any(|a| a.starts_with("ServerAliveInterval")),
        "a dead connection should be noticed: {args:?}"
    );
    assert!(
        args.iter().any(|a| a.starts_with("ServerAliveCountMax")),
        "a dead connection should be given up on: {args:?}"
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

/// Stopping a node ends its sessions either way, since the PTY masters close
/// with it. How it ends them is what this covers: a hangup sent deliberately,
/// with a moment to act on it, and no process left running against a terminal
/// that no longer exists.
///
/// Both halves are here because they trade against each other. Wait for
/// everyone and a session that ignores hangups holds the node forever; kill
/// everyone at once and a shell never reaches the line where it writes its
/// history.
#[test]
fn a_node_hangs_its_sessions_up_before_it_goes_and_outlasts_none_of_them() {
    let world = World::new("graceful-stop");
    let farewell = world.dir.join("farewell");
    let stubborn = world.dir.join("stubborn.pid");

    // One session that takes the hint, and writes on its way out so that its
    // hangup is visible at all.
    let polite = format!(
        "trap 'echo bye > {}; exit 0' HUP; sleep 300",
        farewell.display()
    );
    world.ok(
        "laptop",
        &["new", "-d", "-n", "polite", "sh", "-c", &polite],
    );

    // And one that does not, which is what the grace period ends in a kill for:
    // otherwise it is left reading EIO from a PTY whose master went with the
    // node, with nothing left to reap it.
    let deaf = format!("trap '' HUP; echo $$ > {}; sleep 300", stubborn.display());
    world.ok("laptop", &["new", "-d", "-n", "deaf", "sh", "-c", &deaf]);

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !stubborn.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = std::fs::read_to_string(&stubborn)
        .expect("the second session never started")
        .trim()
        .to_string();
    assert!(alive(&pid), "the second session was never running");

    world.ok("laptop", &["stop", "--force"]);

    assert!(
        farewell.exists(),
        "the shell was never hung up, or was given no moment to say so"
    );
    // The signal goes out before the node does; being reaped is a moment
    // behind it, and that moment is not what is being tested.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && alive(&pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(&pid),
        "a session that ignored the hangup outlived the node that held its terminal"
    );
}

/// Whether a pid is still there, asked the way a shell would.
fn alive(pid: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid} 2>/dev/null"))
        .status()
        .expect("running kill")
        .success()
}

/// `daemon` and `agent` are machinery, not commands: one is started by the
/// service unit or by a client that found no node, the other is what ssh runs.
/// Hiding them keeps `mm --help` to the things a person types, and this is the
/// test that the hiding stopped there: ssh names `mm agent` outright and the
/// service unit names `mm daemon`, so a hidden command that stopped running
/// would take every remote command on every machine with it.
#[test]
fn the_two_commands_nobody_types_are_hidden_but_still_run() {
    let world = World::new("hidden");

    let help = world.ok("laptop", &["--help"]);
    for hidden in ["daemon", "agent"] {
        assert!(
            !help.lines().any(|line| line.trim().starts_with(hidden)),
            "{hidden} is listed in --help:\n{help}"
        );
    }
    // The ones that replaced them for a person are still there.
    for shown in ["start", "stop", "restart"] {
        assert!(
            help.lines().any(|line| line.trim().starts_with(shown)),
            "{shown} is missing from --help:\n{help}"
        );
    }

    // Named outright, both still work, which is all ssh and systemd ever do.
    assert!(world.ok("laptop", &["agent", "--help"]).contains("stdin"));
    assert!(world.ok("laptop", &["daemon", "--help"]).contains("node"));

    // And a tab offers neither, since the generated script comes from the same
    // parser.
    let offered = world.complete("laptop", &[""]);
    assert!(
        !offered
            .iter()
            .any(|word| word == "daemon" || word == "agent"),
        "a tab offered machinery: {offered:?}"
    );
    assert!(offered.iter().any(|word| word == "a"), "{offered:?}");
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

    let end = notification.find("\x1b\\").expect("a finished sequence");
    assert!(
        notification[..end].contains("ringer"),
        "the notification does not name the session that rang: {notification:?}"
    );
    // The bell after it, on solid ground. A notification is seen and not heard,
    // so the terminal is rung as well as written to: an OSC ended with a BEL
    // would have that byte eaten as its terminator and ring nothing.
    assert!(
        notification[end..].starts_with("\x1b\\\x07"),
        "the sequence is closed with ST and rung after: {notification:?}"
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

/// Attaching to a session sitting in a full-screen program paints that program
/// on a screen with nothing else on it.
///
/// A screen dump is both buffers: the primary one, then the switch to the
/// alternate one, then that. The switch is the client's to swallow, and
/// swallowing it and nothing else left the two painted on one screen, so the
/// scrollback of the shell that started the program showed through wherever the
/// program had not painted.
#[test]
fn a_full_screen_program_is_painted_on_a_screen_with_nothing_under_it() {
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("both-buffers");
    world.ok(
        "laptop",
        &[
            "new",
            "-d",
            "-n",
            "editing",
            "sh",
            "-c",
            // A shell with something on its screen, and then a program on the
            // screen of its own that a dump paints second.
            "printf 'SHELL\\n\\033[?1049h\\033[HPROGRAM\\n'; sleep 30",
        ],
    );
    world.wait_for_node("laptop");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "editing"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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
    while !seen.contains("PROGRAM") {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            Instant::now() < deadline,
            "the program never reached the terminal; saw: {seen:?}"
        );
    }

    // The dump paints the shell's screen, and the program's has to land on an
    // erased one rather than on top of it.
    let shell = seen.find("SHELL").expect("the shell went unpainted");
    let program = seen.find("PROGRAM").unwrap();
    assert!(
        seen[shell..program].contains("\x1b[2J"),
        "the program was painted over the screen underneath it: {seen:?}"
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

/// The whole premise of the project is that the session outlives the
/// connection, so losing the connection must not put somebody back at their
/// shell with the session still running two hops away. The ssh dies here, the
/// way a wifi hop or a closed lid kills it; the node and the session on the
/// other side never notice.
#[test]
fn an_attach_whose_connection_drops_comes_back_by_itself() {
    use std::io::Read;
    use std::sync::mpsc;

    let world = World::new("reconnect");
    world.ok("laptop", &["add", "gpu-box"]);
    world.ok("gpu-box", &["new", "-d", "-n", "long", "sleep", "300"]);
    world.wait_for_node("gpu-box");

    let (mut pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "gpu-box/long"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cut = false;
    let mut noticed = false;
    loop {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        // Once the client is on the screen, kill the agent bridging it to
        // gpu-box. Not the node: the session has to survive, which is the
        // whole point.
        if !cut && seen.contains("gpu-box/long") {
            cut = true;
            let pattern = format!("{}/gpu-box.sock agent", world.dir.display());
            Command::new("pkill")
                .args(["-f", &pattern])
                .status()
                .expect("running pkill");
        }
        if cut && !noticed && seen.contains("retrying in") {
            noticed = true;
            // Everything from here is what happens after it says so.
            seen.clear();
        }
        // Back on the session, which is the mark row naming it again.
        if noticed && seen.contains("gpu-box/long") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cut={cut} noticed={noticed}; the terminal saw: {seen:?}"
        );
    }

    assert!(
        client.try_wait().expect("checking the client").is_none(),
        "the client should still be attached, not back at the shell"
    );
    assert!(
        world.ok("gpu-box", &["ls", "local"]).contains("long"),
        "the session outlives the connection to it"
    );

    let _ = client.kill();
    let _ = client.wait();
}

/// A switch key that finds nowhere to go has to ask where to go next time.
///
/// The listing behind the keys is only ever asked for by a press, so a press
/// that landed nowhere and did not ask was the last one that ever asked: a
/// machine with one session on it when the run started stayed a machine with
/// one session on it, however many were started beside it afterwards.
#[test]
fn a_switch_key_that_lands_nowhere_asks_where_to_go_again() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("switch-asks-again");
    world.ok("laptop", &["add", "gpu-box"]);
    world.ok("gpu-box", &["new", "-d", "-n", "only", "sleep", "300"]);
    world.wait_for_node("gpu-box");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "gpu-box/only"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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

    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut alone = false;
    let mut again = false;
    let mut taken = false;
    loop {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        // The second session is started before the key is pressed, so that the
        // listing the press asks for is one that has it. What the press cannot
        // use is the listing taken when the run started, which is the one with
        // nowhere to go in it.
        if !alone && seen.contains("gpu-box/only") {
            alone = true;
            world.ok("gpu-box", &["new", "-d", "-n", "second", "sleep", "300"]);
            (&pty).write_all(b"\x1d\t").unwrap();
            seen.clear();
        }
        // The popup is up and control mode is still on, so the next key is
        // still the client's. The second press is what asks again, and the
        // answer is what puts `second` in the list; then Enter takes it.
        // The popup is up and control mode is still on, so the next key is
        // still the client's. The second press is what asks again, and the
        // answer is what puts `second` in the list.
        if alone && !again && seen.contains("second") {
            again = true;
            (&pty).write_all(b"\t").unwrap();
        }
        // Then Enter takes the row the second press moved onto. A read of its
        // own, the way a hand sends it: an action ends the chunk it was found
        // in, here as everywhere else in this client.
        if again && !taken && seen.contains("second") {
            taken = true;
            (&pty).write_all(b"\r").unwrap();
        }
        if again && watched(&world.ok("gpu-box", &["ls", "local"])) == ["second"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "alone={alone} again={again}; gpu-box says {:?} is attached; the terminal saw: {seen:?}",
            watched(&world.ok("gpu-box", &["ls", "local"])),
        );
    }

    let _ = client.kill();
    let _ = client.wait();
}

/// The keyboard a reconnect hands back is the session's, and it is the client
/// that has to hand it back.
///
/// Control mode is turned on by a hop, so the key after one carries on walking
/// without a mode key of its own, and nothing else here ever turns it off
/// again. A connection waited out and reattached came back with it still on,
/// where `Ctrl-]` *leaves* control mode and the `tab` behind it is a tab into
/// somebody's shell: the session next door had become unreachable by the one
/// gesture that reaches it, for the rest of the run.
#[test]
fn a_reconnect_hands_the_keyboard_back_to_the_session() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("reconnect-keys");
    world.ok("laptop", &["add", "gpu-box"]);
    world.ok("gpu-box", &["new", "-d", "-n", "one", "sleep", "300"]);
    world.ok("gpu-box", &["new", "-d", "-n", "two", "sleep", "300"]);
    world.wait_for_node("gpu-box");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "gpu-box/one"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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

    let mut seen = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    // What has happened so far, in the order it has to happen in. Everything
    // waits on the terminal saying so, because the whole test is about which
    // session the client is in when a key is pressed.
    let mut opened = false;
    let mut hopped = false;
    let mut cut = false;
    let mut noticed = false;
    let mut back = false;
    let mut again = false;
    loop {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        // Open the popup, which is what control mode looks like and what
        // turns it on. The listing it asks for is still out at the other
        // machine, so the session next door appears in the box a moment later.
        if !opened && seen.contains("gpu-box/one") {
            opened = true;
            (&pty).write_all(b"\x1d").unwrap();
        }
        // There it is: move onto it and take it. Two reads, because an action
        // ends the chunk it was found in.
        if opened && !hopped && seen.contains("two") {
            hopped = true;
            (&pty).write_all(b"\t").unwrap();
            (&pty).write_all(b"\r").unwrap();
            seen.clear();
        }
        // Landed. Now kill the agent bridging the client to gpu-box, the way
        // a closed lid kills the ssh under it. Not the node: the sessions
        // have to outlive it.
        if hopped && !cut && seen.contains("gpu-box/two") {
            cut = true;
            let pattern = format!("{}/gpu-box.sock agent", world.dir.display());
            Command::new("pkill")
                .args(["-f", &pattern])
                .status()
                .expect("running pkill");
        }
        // The row says the connection went, and says it is still trying.
        if cut && !noticed && seen.contains("reconnecting") {
            noticed = true;
            seen.clear();
        }
        // Back on the session, which is the mark row naming it again. The
        // same keys as the first hop, and they have to do the same thing: a
        // wait hands the keyboard back to the session, so `Ctrl-]` has to
        // reach the client again rather than being typed into the shell.
        if noticed && !back && seen.contains("gpu-box/two") {
            back = true;
            (&pty).write_all(b"\x1d").unwrap();
            seen.clear();
        }
        if back && !again && seen.contains("one") {
            again = true;
            (&pty).write_all(b"\t").unwrap();
            (&pty).write_all(b"\r").unwrap();
        }
        if again && watched(&world.ok("gpu-box", &["ls", "local"])) == ["one"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "opened={opened} hopped={hopped} cut={cut} noticed={noticed} \
             back={back} again={again}; \
             gpu-box says {:?} is attached; the terminal saw: {seen:?}",
            watched(&world.ok("gpu-box", &["ls", "local"])),
        );
    }

    let _ = client.kill();
    let _ = client.wait();
}

/// The other half of falling back: a run that has not hopped has nowhere to
/// fall back to, so the session named on the command line ending is the command
/// ending, status and all.
#[test]
fn a_session_named_on_the_command_line_ends_the_attach_when_it_exits() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("ends-the-attach");
    world.ok("laptop", &["add", "gpu-box"]);
    world.ok("gpu-box", &["new", "-d", "-n", "quick", "sh"]);
    world.wait_for_node("gpu-box");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "gpu-box/quick"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    let mut typed = false;
    let status = loop {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        if !typed && seen.contains("gpu-box/quick") {
            typed = true;
            (&pty).write_all(b"exit 3\n").unwrap();
        }
        if let Some(status) = client.try_wait().expect("checking the client") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "typed={typed}; the terminal saw: {seen:?}"
        );
    };

    assert_eq!(status.code(), Some(3), "the session's status is the answer");
    assert!(
        seen.contains("exited with status 3"),
        "the terminal saw: {seen:?}"
    );
}

/// A session started from inside a run ends inside it too: typing `exit` in it
/// puts you back in the session you were in when you pressed the key, rather
/// than ending the whole attach and handing back a shell.
#[test]
fn a_session_that_ends_puts_you_back_in_the_one_you_came_from() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("fall-back");
    world.ok("laptop", &["add", "gpu-box"]);
    world.ok("gpu-box", &["new", "-d", "-n", "long", "sleep", "300"]);
    world.wait_for_node("gpu-box");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "gpu-box/long"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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

    let mut seen = String::new();
    let catch_up = |seen: &mut String| {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    };

    // Ctrl-] n once the client is on the screen, then wait to land in whatever
    // the node called the session it started.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut pressed = false;
    let started = loop {
        catch_up(&mut seen);
        if !pressed && seen.contains("gpu-box/long") {
            pressed = true;
            (&pty).write_all(b"\x1dn").unwrap();
            seen.clear();
        }
        if pressed {
            let listing = world.ok("gpu-box", &["ls", "local"]);
            let started: Vec<&str> = listing
                .lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next())
                .filter(|name| *name != "long")
                .collect();
            if let [name] = started[..]
                && seen.contains(&format!("gpu-box/{name}"))
            {
                break name.to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "pressed={pressed}; the terminal saw: {seen:?}"
        );
    };

    // End it the way anybody ends a shell.
    seen.clear();
    (&pty).write_all(b"exit\n").unwrap();

    // Back where it came from, and saying what happened: a screen that changes
    // under somebody with nothing said about it reads as a client that lost its
    // place.
    let said = format!("{started} exited with status 0");
    let back = Instant::now() + Duration::from_secs(30);
    while !(seen.contains("gpu-box/long") && seen.contains(&said)) {
        catch_up(&mut seen);
        assert!(
            Instant::now() < back,
            "{started} ended but the client did not come back saying so; the terminal saw: {seen:?}"
        );
    }

    assert!(
        client.try_wait().expect("checking the client").is_none(),
        "the client should be back in gpu-box/long, not at the shell"
    );
    assert!(
        !world.ok("gpu-box", &["ls", "local"]).contains(&started),
        "{started} should be gone rather than left running"
    );

    let _ = client.kill();
    let _ = client.wait();
}

/// The control key that starts a session starts it on the machine the client
/// is on, not the one it was typed from, and lands you in it. Two hops away
/// here: the key is pressed on the laptop and the session appears on gpu-box.
#[test]
fn the_new_key_starts_a_session_on_the_machine_you_are_on() {
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;

    let world = World::new("control-new");
    world.ok("laptop", &["add", "gpu-box"]);
    world.ok("gpu-box", &["new", "-d", "-n", "long", "sleep", "300"]);
    world.wait_for_node("gpu-box");

    let (pty, pts) = pty_process::blocking::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut client = pty_process::blocking::Command::new(MM)
        .arg("--socket")
        .arg(world.socket("laptop"))
        .args(["attach", "gpu-box/long"])
        .env("MM_CONFIG_DIR", world.dir.join("laptop"))
        .env("MM_SSH", world.ssh_stub())
        .env("MM_LOG", "manymux=warn")
        .env("TERM", "xterm-256color")
        .spawn(pts)
        .expect("attaching on a terminal");

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

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    let mut pressed = false;
    let started = loop {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        // Once the client is on the screen, Ctrl-] n.
        if !pressed && seen.contains("gpu-box/long") {
            pressed = true;
            (&pty).write_all(b"\x1dn").unwrap();
            seen.clear();
        }
        // The node picked the name, so the listing is what says which it is.
        if pressed {
            let listing = world.ok("gpu-box", &["ls", "local"]);
            let started: Vec<&str> = listing
                .lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next())
                .filter(|name| *name != "long")
                .collect();
            if let [name] = started[..] {
                break name.to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "pressed={pressed}; the terminal saw: {seen:?}"
        );
    };

    // And the client went with it: the mark row names the session it started,
    // which is the whole gesture rather than a session left running elsewhere.
    let landed = Instant::now() + Duration::from_secs(10);
    while !seen.contains(&format!("gpu-box/{started}")) {
        if let Ok(chunk) = seen_rx.recv_timeout(Duration::from_millis(200)) {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
        assert!(
            Instant::now() < landed,
            "started {started} but stayed put; the terminal saw: {seen:?}"
        );
    }

    let _ = client.kill();
    let _ = client.wait();
}

/// A group spans machines, and nothing on the wire carries it: this is one
/// machine's view of two machines' sessions, held in a file of its own.
#[test]
fn a_group_holds_sessions_from_more_than_one_machine() {
    let world = World::new("groups");

    world.ok("laptop", &["new", "-d", "-n", "build", "sleep", "60"]);
    world.wait_for_node("laptop");
    world.ok(
        "laptop",
        &["new", "-d", "-n", "train", "gpu-box", "sleep", "60"],
    );

    world.ok("laptop", &["group", "build", "pi"]);
    world.ok("laptop", &["group", "gpu-box/train", "pi"]);

    let listed = world.ok("laptop", &["groups"]);
    assert!(listed.contains("pi"), "{listed}");
    assert!(listed.contains("build"), "{listed}");
    assert!(listed.contains("train"), "{listed}");

    // With no name, the session comes out of whatever it was in.
    world.ok("laptop", &["group", "build"]);
    let after = world.ok("laptop", &["groups"]);
    assert!(!after.contains("build"), "cleared, so not listed: {after}");
    assert!(after.contains("train"), "and the other one stayed: {after}");
}

/// The whole reason membership is keyed on the pid and the start time: a name
/// moves, and the session must not move with it.
#[test]
fn a_renamed_session_stays_in_its_group() {
    let world = World::new("groups-rename");

    world.ok("laptop", &["new", "-d", "-n", "build", "sleep", "60"]);
    world.ok("laptop", &["group", "build", "pi"]);
    world.ok("laptop", &["rename", "build", "nightly"]);

    let listed = world.ok("laptop", &["groups"]);
    assert!(listed.contains("pi"), "{listed}");
    assert!(listed.contains("nightly"), "{listed}");
}

/// A group names more than one session, and a kill acts on exactly one and
/// cannot be undone. The same reason a bare machine name is only accepted for
/// going somewhere.
#[test]
fn a_group_is_refused_where_one_session_is_meant() {
    let world = World::new("groups-refuse");

    world.ok("laptop", &["new", "-d", "-n", "build", "sleep", "60"]);
    world.ok("laptop", &["group", "build", "pi"]);

    let out = world.run("laptop", &["kill", "@pi"]);
    assert!(!out.status.success(), "a group is not a session to kill");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("names more than one session"), "{said}");
}

/// The column is there only when there is something to put in it: a fleet with
/// no groups reads exactly as it always has.
#[test]
fn the_listing_shows_a_group_column_only_once_something_is_in_one() {
    let world = World::new("groups-column");

    world.ok("laptop", &["new", "-d", "-n", "build", "sleep", "60"]);
    let plain = world.ok("laptop", &["ls"]);
    assert!(!plain.to_lowercase().contains("group"), "{plain}");

    world.ok("laptop", &["group", "build", "pi"]);
    let grouped = world.ok("laptop", &["ls"]);
    assert!(grouped.contains("pi"), "{grouped}");
}
