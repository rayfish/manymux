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

use std::time::Duration;

use manymux::proto::Size;
use manymux_android::keys::{Identity, KnownHosts, generate};
use manymux_android::machine::Machine;
use manymux_android::session::{Session, State};
use sshd::Sshd;
use world::{Mm, World};

/// A phone, a machine, and one session on it.
async fn attached(name: &str, drops: bool) -> Session {
    let world = World::where_mm_is(name, Mm::OnPath);
    // Named but not created: the stub makes the file the first time it drops
    // and behaves from then on. It goes through the server rather than through
    // this process, or it would reach every test running beside this one.
    let dropping = world.dir.join("dropped").display().to_string();
    let extra: Vec<(&str, String)> = if drops {
        vec![("STUB_DROPS", dropping)]
    } else {
        Vec::new()
    };
    let sshd = Sshd::listening(&world.dir, generate().unwrap(), &extra).await;

    Session::open(
        Machine {
            address: "127.0.0.1".to_string(),
            port: sshd.port,
            user: "whoever".to_string(),
        },
        Identity::kept_at(&world.dir.join("id_ed25519")).unwrap(),
        KnownHosts::at(world.dir.join("known_hosts")),
        "build".to_string(),
        Size::new(40, 8),
    )
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
async fn the_screen_comes_back_painted_with_what_the_session_had_on_it() {
    let session = attached("attach-paint", false).await;

    let screen = until_it_says(&session, "ready").await;

    assert!(screen.contains("ready"), "{screen}");
    assert_eq!(session.state(), State::Attached);
}

#[tokio::test]
async fn a_probe_is_answered_without_anything_asking_for_a_frame() {
    let session = attached("attach-ping", false).await;
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
    let session = attached("attach-typed", false).await;
    until_it_says(&session, "ready").await;

    session.send(b"hello there".to_vec());

    let screen = until_it_says(&session, "hello there").await;
    assert!(screen.contains("hello there"), "{screen}");
}

#[tokio::test]
async fn a_resize_is_told_to_the_far_end() {
    let session = attached("attach-resize", false).await;
    until_it_says(&session, "ready").await;

    session.resize(Size::new(30, 6));

    let screen = until_it_says(&session, "size 30x6").await;
    assert!(screen.contains("size 30x6"), "{screen}");
}

#[tokio::test]
async fn a_connection_that_drops_is_waited_out_and_taken_up_again() {
    let session = attached("attach-dropped", true).await;

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
    let session = attached("attach-detach", false).await;
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
