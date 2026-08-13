//! End-to-end tests of the property the whole project exists for: a client
//! going away must not disturb the process running in the session.
//!
//! These drive the real protocol over an in-memory duplex stream, so they cover
//! the same `Node::handle` path a Unix socket or an ssh agent takes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use manymux::node::{Config, Node};
use manymux::proto::{self, Request, Response, Size, SpawnSpec, tag};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf, split};

/// One client connection: a duplex pair with the server handler running on the
/// far end, exactly as the socket listener would have spawned it.
struct Client {
    read: ReadHalf<DuplexStream>,
    write: WriteHalf<DuplexStream>,
}

impl Client {
    fn connect(node: &Arc<Node>) -> Self {
        let (client_side, server_side) = tokio::io::duplex(1 << 20);
        let (server_read, server_write) = split(server_side);
        let node = Arc::clone(node);
        tokio::spawn(async move {
            let _ = node.handle(server_read, server_write).await;
        });
        let (read, write) = split(client_side);
        Self { read, write }
    }

    async fn send(&mut self, request: &Request) -> Result<Response> {
        proto::write_msg(&mut self.write, tag::REQUEST, request).await?;
        let frame = proto::read_frame(&mut self.read)
            .await?
            .expect("a response");
        assert_eq!(frame.tag, tag::RESPONSE);
        proto::decode(&frame.body)
    }

    /// Read output frames until the accumulated text contains `needle`.
    async fn read_until(&mut self, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Duration::from_secs(10);
        let found = tokio::time::timeout(deadline, async {
            loop {
                match proto::read_frame(&mut self.read).await.expect("a frame") {
                    Some(frame) if frame.tag == tag::DATA => {
                        seen.push_str(&String::from_utf8_lossy(&frame.body))
                    }
                    Some(_) => {}
                    None => panic!("stream closed while waiting for {needle:?}"),
                }
                if seen.contains(needle) {
                    return;
                }
            }
        })
        .await;
        assert!(
            found.is_ok(),
            "timed out waiting for {needle:?}; saw: {seen:?}"
        );
        seen
    }

    async fn type_line(&mut self, line: &str) -> Result<()> {
        proto::write_frame(&mut self.write, tag::DATA, format!("{line}\n").as_bytes()).await
    }
}

fn alive(pid: u32) -> bool {
    // Signal 0 checks for existence without delivering anything.
    // SAFETY: kill with signal 0 on a pid we spawned.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// A node watching nothing: this machine's sessions and nothing else, reached
/// the way the local socket does.
async fn test_node() -> Arc<Node> {
    Node::start(Config {
        peers: Vec::new(),
        hosts_file: None,
        notifications: false,
    })
    .await
}

/// A freshly spawned session: its name and the pid of the process in it.
struct Spawned {
    name: String,
    pid: u32,
}

async fn spawn_session(node: &Arc<Node>, command: &[&str]) -> Spawned {
    let spec = SpawnSpec {
        command: command.iter().map(|s| s.to_string()).collect(),
        size: Size::new(80, 24),
        ..Default::default()
    };
    let mut client = Client::connect(node);
    let Response::Spawned { name } = client.send(&Request::Spawn(spec)).await.unwrap() else {
        panic!("spawn failed");
    };
    let pid = node.registry.get(&name).expect("the new session").pid;
    Spawned { name, pid }
}

#[tokio::test]
async fn detaching_leaves_the_child_running_and_reattach_repaints() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, pid } =
        spawn_session(&node, &["/bin/sh", "-c", "echo i-am-alive; sleep 30"]).await;

    // Attach and wait for the child's output to reach us.
    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));
    client.read_until("i-am-alive").await;

    // Detach the hard way: drop the connection, as a dying network would.
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(alive(pid), "the child died when its client went away");
    assert!(
        registry.get(&name).is_some(),
        "the session vanished on detach"
    );

    // Reattach: the very first frame is the screen dump, which must already
    // contain what happened while we were gone.
    let mut client = Client::connect(&node);
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));
    let repainted = client.read_until("i-am-alive").await;
    assert!(
        repainted.contains("i-am-alive"),
        "reattach did not repaint the screen: {repainted:?}"
    );

    registry.kill(&name).unwrap();
}

#[tokio::test]
async fn output_produced_while_detached_is_on_the_screen_when_you_return() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(
        &node,
        &["/bin/sh", "-c", "sleep 0.5; echo late-arrival; sleep 30"],
    )
    .await;

    // Nobody is attached while the output happens.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));
    let dump = client.read_until("late-arrival").await;
    assert!(dump.contains("late-arrival"));

    registry.kill(&name).unwrap();
}

#[tokio::test]
async fn resizing_a_client_resizes_the_child_terminal() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(&node, &["/bin/sh", "-i"]).await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));

    proto::write_msg(&mut client.write, tag::RESIZE, &Size::new(100, 40))
        .await
        .unwrap();
    // Give the resize a moment to reach the pty before asking about it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.type_line("stty size").await.unwrap();

    // `stty size` prints "rows cols".
    let out = client.read_until("40 100").await;
    assert!(
        out.contains("40 100"),
        "child did not see the resize: {out:?}"
    );

    registry.kill(&name).unwrap();
}

#[tokio::test]
async fn a_session_disappears_from_the_list_once_its_child_exits() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(&node, &["/bin/sh", "-c", "exit 7"]).await;

    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.get(&name).is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the session should have been reaped");

    assert!(registry.list().is_empty());
}

#[tokio::test]
async fn two_clients_share_one_session_at_the_smaller_size() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(&node, &["/bin/sh", "-i"]).await;

    let mut wide = Client::connect(&node);
    let Response::Attached { size } = wide
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(120, 40),
        })
        .await
        .unwrap()
    else {
        panic!("attach failed");
    };
    assert_eq!(size, Size::new(120, 40));

    let mut narrow = Client::connect(&node);
    let Response::Attached { size } = narrow
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(80, 50),
        })
        .await
        .unwrap()
    else {
        panic!("attach failed");
    };
    assert_eq!(
        size,
        Size::new(80, 40),
        "the shared size should be the smallest of both clients"
    );

    // What one client types, the other sees.
    narrow.type_line("echo shared-echo").await.unwrap();
    let seen = wide.read_until("shared-echo").await;
    assert!(seen.contains("shared-echo"));

    registry.kill(&name).unwrap();
}

#[tokio::test]
async fn reattaching_restores_mouse_reporting_and_the_title() {
    // The gap this closes: the screen model reproduces the screen but knows
    // nothing about mouse reporting, bracketed paste or the window title, so a
    // repaint alone would leave a reattached client with a dead mouse.
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(
        &node,
        &[
            "/bin/sh",
            "-c",
            // What a full-screen program does on startup: name itself, switch
            // to the alt screen, turn on drag tracking, SGR encoding and
            // bracketed paste.
            "printf '\\033]0;fixing the parser\\007\\033[?1049h\\033[?1002h\\033[?1006h\\033[?2004h'; \
             echo ready; sleep 30",
        ],
    )
    .await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));
    client.read_until("ready").await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reattach: the repaint has to carry the modes as well as the screen.
    let mut client = Client::connect(&node);
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));
    let repaint = client.read_until("\x1b[?1006h").await;

    for expected in [
        "\x1b[?1002h",              // drag tracking
        "\x1b[?1006h",              // SGR mouse encoding
        "\x1b[?2004h",              // bracketed paste
        "\x1b]2;fixing the parser", // the title, so the tab is named right
    ] {
        assert!(
            repaint.contains(expected),
            "reattach lost {expected:?}; got {repaint:?}"
        );
    }

    registry.kill(&name).unwrap();
}
