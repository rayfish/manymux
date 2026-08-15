//! End-to-end tests of the property the whole project exists for: a client
//! going away must not disturb the process running in the session.
//!
//! These drive the real protocol over an in-memory duplex stream, so they cover
//! the same `Node::handle` path a Unix socket or an ssh agent takes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use manymux::node::{Config, Node};
use manymux::proto::{
    self, EventKind, FrameReader, HostedEvent, Request, Response, Size, SpawnSpec, ViewRequest, tag,
};
use tokio::io::{AsyncRead, DuplexStream, WriteHalf, split};

/// One client connection: a duplex pair with the server handler running on the
/// far end, exactly as the socket listener would have spawned it.
struct Client {
    read: FrameReader<Box<dyn AsyncRead + Unpin + Send>>,
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
        Self {
            read: FrameReader::new(Box::new(read)),
            write,
        }
    }

    async fn send(&mut self, request: &Request) -> Result<Response> {
        proto::write_msg(&mut self.write, tag::REQUEST, request).await?;
        let frame = self.read.next().await?.expect("a response");
        assert_eq!(frame.tag, tag::RESPONSE);
        proto::decode(&frame.body)
    }

    /// Read output frames until the accumulated text contains `needle`.
    async fn read_until(&mut self, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Duration::from_secs(10);
        let found = tokio::time::timeout(deadline, async {
            loop {
                match self.read.next().await.expect("a frame") {
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

    /// The next frame, or `None` once the host has hung up on us.
    async fn next_frame(&mut self) -> Option<proto::Frame> {
        self.read.next().await.expect("a frame")
    }

    /// Read frames until a ping arrives, which is the host asking whether this
    /// client is still there.
    async fn read_until_ping(&mut self) {
        loop {
            match self.next_frame().await {
                Some(frame) if frame.tag == tag::PING => return,
                Some(_) => {}
                None => panic!("the host hung up instead of pinging"),
            }
        }
    }

    async fn pong(&mut self) -> Result<()> {
        proto::write_frame(&mut self.write, tag::PONG, &[]).await
    }

    /// Ask for a window of the session's history, and read past whatever else
    /// is on the stream until it comes back.
    async fn ask_for_view(&mut self, request: &ViewRequest) -> proto::View {
        proto::write_frame(
            &mut self.write,
            tag::VIEW,
            &proto::encode(request).expect("a request"),
        )
        .await
        .expect("asking");
        loop {
            match self.next_frame().await {
                Some(frame) if frame.tag == tag::VIEW => {
                    return proto::decode(&frame.body).expect("a window");
                }
                Some(_) => {}
                None => panic!("the stream closed before the window arrived"),
            }
        }
    }

    /// The next bell an attached client is told about, whichever session it
    /// came from.
    async fn read_until_bell(&mut self) -> HostedEvent {
        let deadline = Duration::from_secs(10);
        let found = tokio::time::timeout(deadline, async {
            loop {
                match self.next_frame().await {
                    Some(frame) if frame.tag == tag::EVENT => {
                        let hosted: HostedEvent = proto::decode(&frame.body).expect("an event");
                        if hosted.event.kind == EventKind::Bell {
                            return hosted;
                        }
                    }
                    Some(_) => {}
                    None => panic!("the stream closed before any bell"),
                }
            }
        })
        .await;
        found.expect("a bell within the deadline")
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

/// What puts a session's past in the scrollback of the terminal you just walked
/// up to. It has to arrive before the screen: the client scrolls it away to
/// make room for the repaint, and a line painted over is a line lost.
#[tokio::test]
async fn history_reaches_a_client_that_asks_before_the_screen_does() {
    let node = test_node().await;
    let Spawned { name, .. } = spawn_session(
        &node,
        &[
            "/bin/sh",
            "-c",
            "i=1; while [ $i -le 60 ]; do echo line $i; i=$((i+1)); done; sleep 300",
        ],
    )
    .await;

    // Attach once to let the session paint, and to know it has finished.
    let mut watcher = Client::connect(&node);
    watcher
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(80, 24),
            history: 0,
        })
        .await
        .unwrap();
    watcher.read_until("line 60").await;

    let mut client = Client::connect(&node);
    client
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(80, 24),
            history: 100,
        })
        .await
        .unwrap();

    let mut history = String::new();
    loop {
        let frame = client.next_frame().await.expect("a frame");
        if frame.tag == tag::DATA {
            break;
        }
        if frame.tag == tag::HISTORY {
            history.push_str(&String::from_utf8_lossy(&frame.body));
        }
    }
    assert!(history.contains("line 1\r\n"), "{history:?}");
    // The screen is the dump's to paint, so its lines are not in here twice.
    assert!(!history.contains("line 60"), "{history:?}");
}

/// Scrolling back on a screen the terminal keeps no scrollback for: the lines
/// come from the node's model, counted back from the newest so that a buffer
/// trimming under the reader still means the same thing.
#[tokio::test]
async fn a_client_can_scroll_back_through_what_a_session_printed() {
    let node = test_node().await;
    let Spawned { name, .. } = spawn_session(
        &node,
        &[
            "/bin/sh",
            "-c",
            "i=1; while [ $i -le 60 ]; do echo line $i; i=$((i+1)); done; sleep 300",
        ],
    )
    .await;

    let mut client = Client::connect(&node);
    client
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(80, 24),
            history: 0,
        })
        .await
        .unwrap();
    client.read_until("line 60").await;

    let view = client
        .ask_for_view(&ViewRequest { from: 30, lines: 5 })
        .await;
    assert_eq!(view.lines.len(), 5);
    assert_eq!(view.from, 30);
    assert!(view.total >= 61, "60 lines and a prompt: {}", view.total);
    // Counted back from the newest line, so 30 back from the bottom of a
    // buffer holding 1 to 60 lands in the twenties.
    assert!(
        view.lines.iter().any(|line| line.contains("line 2")),
        "{:?}",
        view.lines
    );

    // The top is clamped rather than refused, because the buffer trims from
    // the top while it is being read.
    let top = client
        .ask_for_view(&ViewRequest {
            from: u64::MAX,
            lines: 5,
        })
        .await;
    assert_eq!(top.lines.len(), 1, "one line left above the top");
}

/// And a client that asks for none gets the frames it always got.
#[tokio::test]
async fn a_client_that_asks_for_no_history_is_sent_none() {
    let node = test_node().await;
    let Spawned { name, .. } =
        spawn_session(&node, &["/bin/sh", "-c", "echo hello; sleep 300"]).await;

    let mut client = Client::connect(&node);
    client
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(80, 24),
            history: 0,
        })
        .await
        .unwrap();
    let frame = client.next_frame().await.expect("a frame");
    assert_eq!(frame.tag, tag::DATA, "the screen, with nothing before it");
}

/// The case this closes: a laptop whose lid shuts mid-attach. Its connection is
/// dead but nothing closes it, so without a probe the phantom keeps its say in
/// the session's size and keeps counting as attached for as long as the node
/// lives. Time is paused, so the deadline passes in an instant here.
#[tokio::test(start_paused = true)]
async fn a_client_that_stops_answering_is_detached() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, pid } = spawn_session(&node, &["/bin/sh", "-c", "sleep 300"]).await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
        history: 0,
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));

    // Answer once, which is what opts this client into being held to the
    // deadline at all.
    client.read_until_ping().await;
    client.pong().await.unwrap();
    assert_eq!(
        registry.get(&name).unwrap().info().attached,
        1,
        "answering a ping did not keep the attachment"
    );

    // Then go silent, still reading so that nothing blocks on a full buffer:
    // exactly what a machine that went away looks like from here.
    let mut frames = 0;
    while client.next_frame().await.is_some() {
        frames += 1;
        assert!(frames < 100, "the host never gave up on a silent client");
    }

    assert_eq!(
        registry.get(&name).unwrap().info().attached,
        0,
        "the phantom client is still counted as attached"
    );
    assert!(alive(pid), "dropping a dead client took the child with it");
    registry.kill(&name).unwrap();
}

/// A client built before pings existed skips the tag it does not know and
/// answers nothing, which must not look like a client that died. Never
/// answering is what keeps it out of the deadline entirely.
#[tokio::test(start_paused = true)]
async fn a_client_that_never_answers_is_left_alone() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(&node, &["/bin/sh", "-c", "sleep 300"]).await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
        history: 0,
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));

    // Long past the deadline, had it ever applied to this client.
    for _ in 0..10 {
        client.read_until_ping().await;
    }
    assert_eq!(
        registry.get(&name).unwrap().info().attached,
        1,
        "a client that never claimed to understand pings was dropped for not answering"
    );

    registry.kill(&name).unwrap();
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
        history: 0,
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

/// The other half of the same promise: a client that stays is told when the
/// session ends, and told what it ended with. Nothing covered this path, which
/// is the one an attached client is sitting in when you type `exit`.
///
/// The session waits to be told to go rather than exiting on its own, so that
/// the attach below cannot lose a race with it: a session that has already gone
/// is one there is nothing to attach to, and on a loaded machine that is what
/// used to happen every few runs.
#[tokio::test]
async fn a_client_watching_a_session_end_is_told_what_it_ended_with() {
    let node = test_node().await;
    let Spawned { name, .. } = spawn_session(
        &node,
        &["/bin/sh", "-c", "echo about-to-go; read go; exit 130"],
    )
    .await;

    let mut client = Client::connect(&node);
    assert!(matches!(
        client
            .send(&Request::Attach {
                name: name.clone(),
                size: Size::new(80, 24),
                history: 0,
            })
            .await
            .unwrap(),
        Response::Attached { .. }
    ));
    client.read_until("about-to-go").await;
    client.type_line("go").await.unwrap();

    let exit = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.next_frame().await {
                Some(frame) if frame.tag == tag::EXIT => {
                    return proto::decode::<i32>(&frame.body).unwrap();
                }
                Some(_) => {}
                None => panic!("the stream closed without saying the session had ended"),
            }
        }
    })
    .await;
    assert_eq!(exit.expect("an exit frame"), 130);
}

/// A client swallows the session's switch between the primary and alternate
/// screens, because that screen is the client's own. The picture on the other
/// side of the switch then exists nowhere but here.
#[tokio::test]
async fn asking_for_the_screen_again_gets_the_screen_again() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } =
        spawn_session(&node, &["/bin/sh", "-c", "echo still-here; sleep 30"]).await;

    let mut client = Client::connect(&node);
    assert!(matches!(
        client
            .send(&Request::Attach {
                name: name.clone(),
                size: Size::new(80, 24),
                history: 0,
            })
            .await
            .unwrap(),
        Response::Attached { .. }
    ));
    client.read_until("still-here").await;

    proto::write_frame(&mut client.write, tag::RESYNC, &[])
        .await
        .unwrap();
    // Tagged rather than sent as output, because a dump paints both screen
    // buffers and a client cannot tell those switches from the session's.
    let dump = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.next_frame().await {
                Some(frame) if frame.tag == tag::RESYNC => {
                    return String::from_utf8_lossy(&frame.body).into_owned();
                }
                Some(_) => {}
                None => panic!("the host hung up instead of sending the screen"),
            }
        }
    })
    .await
    .expect("a screen");
    assert!(dump.contains("still-here"), "the resync was not a screen");

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
        history: 0,
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
        history: 0,
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

/// The state `mm update` cannot see from the outside: replacing the binary
/// leaves this process running the old one, so the node has to be able to say
/// which build it started from. Hashing its own path later would answer with
/// whatever the update just wrote there, so the checksum is taken at startup.
#[tokio::test]
async fn a_node_says_which_build_it_is_running() {
    let node = test_node().await;
    let mut client = Client::connect(&node);

    let Response::Version { version, build } = client.send(&Request::Version).await.unwrap() else {
        panic!("a version request should be answered with a version");
    };
    assert_eq!(version, manymux::VERSION);
    let build = build.expect("the node should have checksummed itself at startup");
    assert_eq!(build.len(), 64, "a sha-256 digest: {build}");
    assert!(build.chars().all(|c| c.is_ascii_hexdigit()), "{build}");
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
    let Response::Attached { size, .. } = wide
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(120, 40),
            history: 0,
        })
        .await
        .unwrap()
    else {
        panic!("attach failed");
    };
    assert_eq!(size, Size::new(120, 40));

    let mut narrow = Client::connect(&node);
    let Response::Attached { size, .. } = narrow
        .send(&Request::Attach {
            name: name.clone(),
            size: Size::new(80, 50),
            history: 0,
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
        history: 0,
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

/// The whole point of forwarding a paste: the image is on the machine you are
/// sitting at and the program is on another one, so the bytes travel and what
/// the program is handed is a path to them on its own filesystem.
#[tokio::test]
async fn a_pasted_image_is_written_on_the_host_and_its_path_typed_into_the_session() {
    let node = test_node().await;
    let registry = &node.registry;
    // `cat` so the session is reading, and the terminal echoes what arrives.
    let Spawned { name, .. } = spawn_session(&node, &["/bin/sh", "-c", "echo ready; cat"]).await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(200, 24),
        history: 0,
    };
    let Response::Attached { paste, .. } = client.send(&attach).await.unwrap() else {
        panic!("attach failed");
    };
    assert!(paste, "a host that takes pastes has to say so");
    client.read_until("ready").await;

    // A PNG, as far as anything here is concerned: the client sniffed it and
    // the host only ever treats it as bytes.
    let png = b"\x89PNG\r\n\x1a\nnot really an image, but nothing here looks";
    proto::write_frame(&mut client.write, tag::PASTE, &png[..8])
        .await
        .unwrap();
    proto::write_frame(&mut client.write, tag::PASTE, &png[8..])
        .await
        .unwrap();
    proto::write_msg(
        &mut client.write,
        tag::PASTE_END,
        &proto::PasteInfo {
            kind: "png".to_string(),
        },
    )
    .await
    .unwrap();

    let seen = client.read_until(".png").await;

    let written: Vec<_> = std::fs::read_dir(manymux::node::paste::dir())
        .expect("the paste directory")
        .flatten()
        .filter(|entry| std::fs::read(entry.path()).is_ok_and(|data| data == png))
        .collect();
    assert_eq!(written.len(), 1, "the file should be on the host, once");

    let name_on_disk = written[0].file_name().to_string_lossy().into_owned();
    assert!(
        seen.contains(&name_on_disk),
        "the session was never told where the file went; saw {seen:?}"
    );
    // Two chunks, one file: nothing was written until the paste was whole.
    assert_eq!(std::fs::read(written[0].path()).unwrap(), png);

    std::fs::remove_file(written[0].path()).unwrap();
    registry.kill(&name).unwrap();
}

/// The route a bell takes to somebody sitting in another session on the same
/// machine. It is the only one that works when the machine is one you ssh into:
/// a desktop notification raised over there is raised on a desktop nobody is
/// looking at, while this comes back down the connection you are typing into.
///
/// The session on the screen is left out on purpose: its bell is already
/// audible where it happened.
#[tokio::test]
async fn a_bell_next_door_reaches_an_attached_client_and_this_ones_does_not() {
    let node = test_node().await;
    let registry = &node.registry;
    let Spawned { name, .. } = spawn_session(&node, &["/bin/sh", "-i"]).await;

    let mut client = Client::connect(&node);
    let attach = Request::Attach {
        name: name.clone(),
        size: Size::new(80, 24),
        history: 0,
    };
    assert!(matches!(
        client.send(&attach).await.unwrap(),
        Response::Attached { .. }
    ));

    // This session rings first, and must not be reported: it is on the screen.
    client.type_line("printf 'here\\a'").await.unwrap();
    client.read_until("here").await;

    let next_door = spawn_session(&node, &["/bin/sh", "-c", "printf 'x\\a'; sleep 300"]).await;

    let hosted = client.read_until_bell().await;
    assert_eq!(
        hosted.event.session, next_door.name,
        "the client was told about its own bell"
    );
    assert!(
        hosted.event.host_attached >= 1,
        "a machine with somebody attached to it says so, which is what keeps a \
         desktop notifier from saying the same thing twice"
    );

    registry.kill(&name).unwrap();
    registry.kill(&next_door.name).unwrap();
}
