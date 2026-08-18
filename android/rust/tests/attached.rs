//! Being attached, and staying attached.
//!
//! Over the real transport and against a node that answers the real protocol,
//! so what is under test is the client: which bytes reach the screen, which
//! reach the far end, and what happens when the far end goes away in the
//! middle without saying anything.

#[path = "support/sshd.rs"]
mod sshd;
#[path = "support/world.rs"]
mod world;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use manymux::proto::Size;
use manymux_android::keys::{Identity, KnownHosts, generate};
use manymux_android::machine::{Connections, Machine};
use manymux_android::session::{Session, State};
use sshd::{Serving, Sshd};
use world::{Mm, World};

/// How the machine at the other end behaves, one word per connection.
///
/// The words are the stub's, and the count is what makes them mean "the first
/// attach" rather than "every attach": each connection is a fresh process at
/// the far end.
///
/// Two per attempt, and the `ok` in front of each of these is that second one
/// being real: a node answers one request per connection, so the listing and
/// the attach are never the same far end. A script that named the attach first
/// would be describing a machine this client does not talk to.
enum How {
    /// Answers, and stays.
    Steady,
    /// Goes away without a word after the first attach, and behaves after.
    Drops,
    /// Goes away, and by the time anybody comes back the session has ended.
    Gone,
    /// Goes away, and the connection after it drops between answering the
    /// listing and answering the attach. A radio dropping mid-exchange, which
    /// is not a session ending however much the failing call looks like one.
    Vanishes,
    /// Paints the screen and then goes away, which is a connection dropping in
    /// the middle of somebody working.
    Late,
}

impl How {
    fn script(&self) -> &'static str {
        match self {
            How::Steady => "",
            How::Drops => "ok,drop",
            How::Gone => "ok,drop,gone",
            How::Vanishes => "ok,drop,ok,vanish",
            How::Late => "ok,late",
        }
    }
}

/// A session, and the far end's own count of what it has been asked.
struct Reaching {
    session: Session,
    /// The file the stub counts its connections through, which is the only
    /// place a test can see how many the client actually opened.
    connections: PathBuf,
    /// Everything needed to reach that machine again, so a test can leave a
    /// session and come back to one the way somebody does.
    at: Reachable,
    sshd: Sshd,
}

/// A machine, and what it takes to get onto it.
struct Reachable {
    machine: Machine,
    identity: Identity,
    known: KnownHosts,
    connections: Arc<Connections>,
}

impl Reachable {
    /// Attach to a session on it, the way the app does when somebody taps a
    /// row.
    fn attach(&self, name: &str) -> Session {
        Session::open(
            self.machine.clone(),
            self.identity.clone(),
            self.known.clone(),
            Arc::clone(&self.connections),
            name.to_string(),
            Size::new(40, 8),
        )
    }
}

/// A phone, a machine, and one session on it.
async fn attached(name: &str, how: How) -> Session {
    reaching(name, how, Serving::forever()).await.session
}

/// The same, where the machine stops answering on `serving`'s terms.
async fn reaching(name: &str, how: How, serving: Serving) -> Reaching {
    let world = World::where_mm_is(name, Mm::OnPath);
    // The counter goes through the server rather than through this process, or
    // it would reach every test running beside this one.
    let connections = world.dir.join("connections");
    let extra = vec![
        ("STUB_SCRIPT", how.script().to_string()),
        ("STUB_COUNT", connections.display().to_string()),
    ];
    let sshd = Sshd::listening_until(&world.dir, generate().unwrap(), &extra, serving).await;

    let at = Reachable {
        machine: Machine {
            address: "127.0.0.1".to_string(),
            port: sshd.port,
            user: "whoever".to_string(),
        },
        identity: Identity::kept_at(&world.dir.join("id_ed25519")).unwrap(),
        known: KnownHosts::at(world.dir.join("known_hosts")),
        connections: Arc::new(Connections::none()),
    };
    let session = at.attach("build");
    Reaching {
        session,
        connections,
        at,
        sshd,
    }
}

/// How many connections the far end has served.
fn served(connections: &Path) -> usize {
    std::fs::read_to_string(connections)
        .ok()
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

/// Wait for the screen to say something, or give up and show what it says.
async fn until_it_says(session: &Session, wanted: &str) -> String {
    let mut screen = String::new();
    for _ in 0..200 {
        for row in session.take_frame().changed {
            let text: String = row.runs.iter().map(|run| run.text.as_str()).collect();
            if !text.trim().is_empty() {
                screen.push_str(text.trim_end());
                screen.push('\n');
            }
        }
        if screen.contains(wanted) {
            return screen;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the screen never said {wanted:?}; it said:\n{screen}");
}

#[tokio::test]
async fn coming_back_to_a_machine_uses_the_connection_that_is_already_open() {
    let reaching = reaching("attach-again", How::Steady, Serving::forever()).await;
    until_it_says(&reaching.session, "ready").await;

    // Back to the list, and into a session again: what somebody does every
    // time they check on one thing and then another. The attach is torn down
    // and rebuilt, which is the app's model and is fine; what must not be torn
    // down with it is the ssh connection, or every glance at a list costs a key
    // exchange and an auth on a radio that has to wake up for it.
    reaching.session.detach();
    let again = reaching.at.attach("build");
    until_it_says(&again, "ready").await;

    assert_eq!(reaching.sshd.connections(), 1);
}

#[tokio::test]
async fn the_listing_and_the_attach_are_asked_on_connections_of_their_own() {
    let reaching = reaching("attach-apiece", How::Steady, Serving::forever()).await;

    until_it_says(&reaching.session, "ready").await;

    // A node answers one request per connection and hangs up, so a client that
    // listed and then attached on the same stream was writing into a far end
    // that had already gone: the listing worked, the attach came back as "the
    // host closed the connection without responding", and nothing on a phone
    // could ever be attached to. Counted rather than inferred from the screen,
    // because the screen is exactly what a stub too willing to answer twice
    // would have shown.
    assert_eq!(served(&reaching.connections), 2);
}

#[tokio::test]
async fn the_screen_comes_back_painted_with_what_the_session_had_on_it() {
    let session = attached("attach-paint", How::Steady).await;

    let screen = until_it_says(&session, "ready").await;

    assert!(screen.contains("ready"), "{screen}");
    assert_eq!(session.state(), State::Attached);
}

#[tokio::test]
async fn a_probe_is_answered_without_anything_asking_for_a_frame() {
    let session = attached("attach-ping", How::Steady).await;
    // Nothing is drawing: no `take_frame` until the assertion below, which is
    // an app in the background. The answer is the read loop's and must not
    // wait on anybody, or a phone in a pocket is detached by the host after
    // three quarters of a minute of being perfectly well.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let screen = until_it_says(&session, "pong").await;

    assert!(screen.contains("pong"), "{screen}");
}

#[tokio::test]
async fn what_is_typed_reaches_the_far_end() {
    let session = attached("attach-typed", How::Steady).await;
    until_it_says(&session, "ready").await;

    session.send(b"hello there".to_vec());

    let screen = until_it_says(&session, "hello there").await;
    assert!(screen.contains("hello there"), "{screen}");
}

#[tokio::test]
async fn a_resize_is_told_to_the_far_end() {
    let session = attached("attach-resize", How::Steady).await;
    until_it_says(&session, "ready").await;

    session.resize(Size::new(30, 6));

    // The far end took 20 columns rather than the 30 asked for, standing in
    // for another client holding the session narrower. What matters is that
    // the screen here follows what it took: a copy reflowed to a shape the
    // session never had scrolls at a different row from then on.
    for _ in 0..200 {
        let frame = session.take_frame();
        if frame.cols == 20 && frame.rows == 6 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let frame = session.take_frame();
    panic!(
        "the screen is {}x{}, not the {}x{} the far end took",
        frame.cols, frame.rows, 20, 6
    );
}

#[tokio::test]
async fn a_connection_that_drops_is_waited_out_and_taken_up_again() {
    let session = attached("attach-dropped", How::Drops).await;

    // The first attach goes away without a detach and without an exit, which
    // is a network hop rather than anything ending. The session is still
    // running on a machine that never noticed, so the client waits and comes
    // back rather than reporting anything.
    let screen = until_it_says(&session, "ready").await;

    assert!(screen.contains("ready"), "{screen}");
    assert_eq!(session.state(), State::Attached);
}

#[tokio::test]
async fn detaching_ends_the_attach_and_nothing_else() {
    let session = attached("attach-detach", How::Steady).await;
    until_it_says(&session, "ready").await;

    session.detach();

    for _ in 0..200 {
        if session.state() == State::Detached {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("still {:?} long after detaching", session.state());
}

#[tokio::test]
async fn a_resize_asks_for_the_screen_back() {
    let session = attached("attach-resync", How::Steady).await;
    until_it_says(&session, "ready").await;

    session.resize(Size::new(30, 6));

    // Telling the node the new size redraws nothing: a session that printed
    // and went quiet has no answer to a SIGWINCH, so both ends reflow on their
    // own and drift apart. What puts them back together is asking for the
    // screen, and the answer to that is a repaint.
    let screen = until_it_says(&session, "resynced").await;
    assert!(screen.contains("resynced"), "{screen}");
}

#[tokio::test]
async fn a_session_that_is_gone_when_the_connection_comes_back_is_not_waited_for() {
    let session = attached("attach-gone", How::Gone).await;

    // The connection drops, and by the time it comes back the session has
    // ended: the node answers, and says there is no such thing. That is the
    // opposite of a machine that never answered, and waiting for it is
    // reconnecting every ten seconds forever to a session nobody is running.
    for _ in 0..400 {
        match session.state() {
            State::Failed { why } => {
                assert!(why.contains("no session"), "{why}");
                return;
            }
            State::Ended { .. } => return,
            _ => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    panic!("still {:?} long after the session went", session.state());
}

#[tokio::test]
async fn a_connection_lost_in_the_middle_of_attaching_is_waited_out() {
    let session = attached("attach-vanishes", How::Vanishes).await;

    // The listing was answered and the attach was not. Read as the node's
    // answer, that is a session reported gone and a run that is over, while
    // the session sits there on a machine that never noticed the phone left.
    // It is the ordinary shape of a radio dropping.
    let screen = until_it_says(&session, "ready").await;

    assert!(screen.contains("ready"), "{screen}");
    assert_eq!(session.state(), State::Attached);
}

/// Slow on purpose: the deadline it is about is ten seconds, and a shorter one
/// would be a different deadline.
#[tokio::test]
async fn an_attempt_that_never_answers_is_given_up_on() {
    // The first connection attaches and drops; every one after it is accepted
    // and never spoken to, which is a captive portal or a wedged sshd and is
    // the one failure that never finishes by itself.
    let session = reaching("attach-wedged", How::Late, Serving::going_mid_session())
        .await
        .session;
    until_it_says(&session, "ready").await;

    // A second wait, and not the first: the first is the drop being noticed,
    // and reaching that proves nothing. Getting back to one means the attempt
    // in between ended, which left to itself it never does — it reads nothing
    // and returns never, so the bar says "reaching" with nothing counting down
    // and a back button nobody is reading.
    for _ in 0..120 {
        if matches!(session.state(), State::Waiting { tries } if tries >= 2) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "still {:?} long after the machine stopped answering",
        session.state()
    );
}
