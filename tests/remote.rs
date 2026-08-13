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

/// A temporary world: a local machine, and one reachable as `gpu-box`.
struct World {
    dir: PathBuf,
}

impl World {
    fn new(name: &str) -> Self {
        // Sockets live here, and a Unix socket path is capped around 104 bytes,
        // so keep it short rather than nesting under the target directory.
        let dir = std::env::temp_dir().join(format!("mm-t-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let world = Self { dir };
        world.write_stub_ssh();
        world
    }

    /// A stand-in for ssh: drop the ssh options, take the destination, and run
    /// that machine's agent. What a real ssh does, minus the network and the
    /// authentication, both of which are ssh's business rather than ours.
    fn write_stub_ssh(&self) {
        let script = format!(
            r#"#!/bin/sh
host=
while [ $# -gt 0 ]; do
    case "$1" in
        -T) shift ;;
        -o) shift 2 ;;
        --) shift; break ;;
        *) host="$1"; shift ;;
    esac
done
# `greet` runs `true` to get prompts out of the way; nothing to do for that.
if [ "$1" = "true" ]; then exit 0; fi
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
                // Killing sessions first stops orphaned shells outliving the test.
                for session in sessions(&self.ok(&machine, &["ls", "local"])) {
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
