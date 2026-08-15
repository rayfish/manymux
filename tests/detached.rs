//! The node outlives whoever started it.
//!
//! A command that needs a node starts one, and from that moment the node owns
//! every session on the machine. If it is left in the process group of the
//! client that happened to start it, then a Ctrl-C in that terminal, or the
//! terminal simply closing, signals the node along with the client and takes
//! every session on the machine down at once. There is not much of a trace
//! afterwards either: the node is killed before it can write a line, so the log
//! shows sessions that stop being mentioned and a fresh node some minutes
//! later. Leaving for a session of its own is what a daemon does, and it is the
//! difference between persistence that holds and persistence that holds until
//! you close a window.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// The binary under test, built by cargo for this integration test.
const MM: &str = env!("CARGO_BIN_EXE_mm");

/// A temporary machine: one config directory and one socket, with the node
/// stopped again however the test ends.
struct Machine {
    dir: PathBuf,
}

impl Machine {
    fn new(name: &str) -> Self {
        // A Unix socket path is capped around 104 bytes, so keep it out of the
        // target directory and short.
        let dir = std::env::temp_dir().join(format!("mm-t-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("node.sock")
    }

    fn mm(&self) -> Command {
        let mut command = Command::new(MM);
        command
            .arg("--socket")
            .arg(self.socket())
            .env("MM_CONFIG_DIR", &self.dir)
            .env("MM_LOG", "manymux=warn");
        command
    }

    /// What `mm ls` says, which is empty when nothing answers.
    fn listed(&self) -> String {
        let out = self.mm().arg("ls").output().expect("running mm ls");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.mm().args(["stop", "--force"]).output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Signal a process group, the way a terminal signals the job in front of it.
fn hangup(group: libc::pid_t) {
    // SAFETY: kill touches no memory of ours. A group with nobody left in it
    // gives ESRCH, which is the outcome the test is hoping for.
    unsafe { libc::kill(-group, libc::SIGHUP) };
}

/// Whether a node is answering on this socket at all.
fn node_is_running(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

#[test]
fn a_node_outlives_a_hangup_meant_for_the_client_that_started_it() {
    let machine = Machine::new("detach");

    // Run the client in a process group of its own, which is what a shell does
    // with a job, and is what makes the group safe to signal from a test.
    let mut client = machine
        .mm()
        .args(["new", "-d", "-n", "probe", "sleep", "600"])
        .process_group(0)
        .stdout(Stdio::null())
        .spawn()
        .expect("starting a session");
    let group = client.id() as libc::pid_t;
    let started = client.wait().expect("waiting for the client");
    assert!(started.success(), "the session should have started");
    assert!(
        machine.listed().contains("probe"),
        "the session should be running before anything is signalled"
    );

    // The client is gone, so this reaches whatever it left behind in its group.
    hangup(group);
    sleep(Duration::from_millis(500));

    assert!(
        node_is_running(&machine.socket()),
        "the node died with the process group of the client that started it"
    );
    assert!(
        machine.listed().contains("probe"),
        "the session went down with the client's process group: {}",
        machine.listed()
    );
}

#[test]
fn a_node_leaves_the_session_and_group_of_its_client() {
    let machine = Machine::new("session");

    let start = machine
        .mm()
        .args(["new", "-d", "-n", "probe", "sleep", "600"])
        .output()
        .expect("starting a session");
    assert!(start.status.success(), "the session should have started");

    let stream = UnixStream::connect(machine.socket()).expect("connecting to the node");
    let node = peer_pid(&stream).expect("asking the kernel who holds the socket");

    // SAFETY: both take a pid and touch no memory.
    let (group, session) = unsafe { (libc::getpgid(node), libc::getsid(node)) };
    assert_eq!(group, node, "the node should lead its own process group");
    assert_eq!(session, node, "the node should lead its own session");
}

/// The pid on the other end of a connection. [`manymux::ipc::peer_pid`] wants a
/// tokio stream, and these tests are not async.
fn peer_pid(stream: &UnixStream) -> Option<libc::pid_t> {
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the fd is open for the call, and the buffer matches the size
        // handed to the kernel.
        let got = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        (got == 0 && cred.pid > 0).then_some(cred.pid)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: as above.
        let got = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        (got == 0 && pid > 0).then_some(pid)
    }
}
