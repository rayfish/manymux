//! A stand-in for `mm agent`, for the tests.
//!
//! It is a real process rather than an in-memory double because the ladder
//! reads a real shell's exit status, and a double would be the test agreeing
//! with itself about what 127 means. Once it has answered, it is a node with no
//! PTY behind it: enough of one to attach to, to be typed at, and to go away in
//! the middle, which is the whole of what a client has to survive.
//!
//! What it does is decided by the environment, because the far end of an ssh is
//! reached through a shell and there is nowhere else to put it:
//!
//! - `STUB_FAILS`: complain and exit non-zero, which is a machine that has
//!   `mm` and cannot run it.
//! - `STUB_SCRIPT`: a comma-separated word per connection, saying what to do
//!   with each in turn. `drop` answers the attach and then goes away without a
//!   word; `vanish` answers the listing and goes away before answering the
//!   attach, which is a connection dropping mid-exchange and is not a session
//!   ending; `gone` leaves the session out of the listing and refuses the
//!   attach, which is one that ended. Anything past the end of the script, and
//!   any other word, behaves.
//! - `STUB_COUNT`: the file the script is counted through. Each connection is
//!   a fresh process, so a file is the only memory there is.

use std::time::SystemTime;

use manymux::proto::{self, Request, Response, SessionInfo, Size, tag};
use tokio::io::{AsyncWriteExt, stdin, stdout};

/// What the one session is called, and what a test attaches to.
const SESSION: &str = "build";

/// The widest this stub will let a session be, so a resize is answered with
/// something other than what was asked for.
const CLIPPED: u16 = 20;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // A machine that has `mm` on it and cannot run it: the rung is spelled
    // right and the answer is still no. Deterministic where borrowing some
    // other program's failure is not, since what a coreutils writes about a
    // name it does not know is that build's business rather than the test's.
    if let Ok(said) = std::env::var("STUB_FAILS") {
        eprintln!("{said}");
        std::process::exit(3);
    }

    let doing = step();
    let mut reader = proto::FrameReader::new(stdin());
    let mut out = stdout();
    let mut attached = false;

    while let Some(frame) = reader.next().await? {
        match frame.tag {
            tag::REQUEST => {
                let answer = match proto::decode::<Request>(&frame.body)? {
                    // A machine whose session has ended answers the listing
                    // without it, which is the answer a client reads to tell a
                    // session that went from a machine that never spoke.
                    Request::List if doing == "gone" => Response::Sessions(Vec::new()),
                    Request::List => Response::Sessions(vec![listed()]),
                    // Named after the command with a counter, the way a node
                    // names one, so a test can see that what came back is the
                    // node's answer rather than what was asked for.
                    Request::Spawn(spec) => Response::Spawned {
                        name: format!("{}-2", spec.command.first().cloned().unwrap_or_default()),
                    },
                    // Gone mid-exchange: the listing was answered and this is
                    // not going to be.
                    Request::Attach { .. } if doing == "vanish" => std::process::exit(0),
                    Request::Attach { name, .. } if doing == "gone" => {
                        Response::Error(format!("no session named {name}"))
                    }
                    Request::Attach { size, .. } => Response::Attached {
                        size,
                        paste: false,
                        scroll: false,
                        rename: false,
                        events: true,
                        read_only: false,
                    },
                    other => Response::Error(format!("stub-agent got {other:?}")),
                };
                let attaching = matches!(answer, Response::Attached { .. });
                proto::write_msg(&mut out, tag::RESPONSE, &answer).await?;

                if attaching {
                    if doing == "drop" {
                        // Gone without a detach and without an exit, which is
                        // what a connection dropping looks like from here.
                        std::process::exit(0);
                    }
                    attached = true;
                    // The repaint, then a probe. Both are what a node sends
                    // straight after an attach.
                    proto::write_frame(&mut out, tag::DATA, painted().as_bytes()).await?;
                    proto::write_frame(&mut out, tag::PING, &[]).await?;
                    out.flush().await?;
                }
            }
            // Typed at. Echoed the way a shell echoes, so a test can see that
            // what it sent reached the far end and came back.
            tag::DATA if attached => {
                proto::write_frame(&mut out, tag::DATA, &frame.body).await?;
                out.flush().await?;
            }
            // The client is alive. Saying so on the screen is how a test sees
            // that it answered without anything having asked it to draw.
            tag::PONG => {
                proto::write_frame(&mut out, tag::DATA, b"pong").await?;
                out.flush().await?;
            }
            // The client swallowed something the terminal would have redrawn,
            // or resized. Answered with a screen, which is what a node does.
            tag::RESYNC => {
                proto::write_frame(&mut out, tag::RESYNC, b"resynced").await?;
                out.flush().await?;
            }
            tag::DETACH => break,
            // Answered the way a node answers: with the size it took, which
            // is clipped here to stand in for another client holding the
            // session narrower than this one asked for.
            tag::RESIZE => {
                let asked: Size = proto::decode(&frame.body)?;
                let took = Size::new(asked.cols.min(CLIPPED), asked.rows);
                proto::write_msg(&mut out, tag::SIZE, &took).await?;
                let said = format!("size {}x{}", took.cols, took.rows);
                proto::write_frame(&mut out, tag::DATA, said.as_bytes()).await?;
                out.flush().await?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn listed() -> SessionInfo {
    SessionInfo {
        name: SESSION.to_string(),
        title: SESSION.to_string(),
        command: "sh".to_string(),
        pid: 1,
        size: Size::new(80, 24),
        attached: 0,
        idle: 0,
        bells: 0,
        started: SystemTime::UNIX_EPOCH,
    }
}

/// The screen a client is given on attach: `avt`'s own dump, which is what a
/// node sends and is nothing like a plain line of text.
fn painted() -> String {
    let mut screen = avt::Vt::builder().size(80, 24).build();
    screen.feed_str("ready");
    screen.dump()
}

/// What this connection is meant to do, from the script and the count.
fn step() -> String {
    let Ok(counting) = std::env::var("STUB_COUNT") else {
        return String::new();
    };
    let so_far: usize = std::fs::read_to_string(&counting)
        .ok()
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0);
    let _ = std::fs::write(&counting, (so_far + 1).to_string());

    std::env::var("STUB_SCRIPT")
        .unwrap_or_default()
        .split(',')
        .nth(so_far)
        .unwrap_or_default()
        .to_string()
}
