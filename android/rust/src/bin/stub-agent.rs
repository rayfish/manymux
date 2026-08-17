//! A stand-in for `mm agent`, for the ladder tests.
//!
//! Answers one `Request::List` with one session and leaves. That is the whole
//! of what climbing the ladder needs to see: the rung either answers the
//! protocol or exits 127, and telling those two apart is the thing under test.
//!
//! It is a real process rather than an in-memory double because the signal the
//! ladder reads is a real shell's exit status, and a double would be the test
//! agreeing with itself about what 127 means.

use std::time::SystemTime;

use manymux::proto::{self, Request, Response, SessionInfo, Size, tag};
use tokio::io::{stdin, stdout};

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

    let mut reader = proto::FrameReader::new(stdin());
    let mut out = stdout();

    while let Some(frame) = reader.next().await? {
        if frame.tag != tag::REQUEST {
            continue;
        }
        let answer = match proto::decode::<Request>(&frame.body)? {
            Request::List => Response::Sessions(vec![SessionInfo {
                name: "build".to_string(),
                title: "build".to_string(),
                command: "sh".to_string(),
                pid: 1,
                size: Size::new(80, 24),
                attached: 0,
                idle: 0,
                bells: 0,
                started: SystemTime::UNIX_EPOCH,
            }]),
            other => Response::Error(format!("stub-agent got {other:?}")),
        };
        proto::write_msg(&mut out, tag::RESPONSE, &answer).await?;
    }
    Ok(())
}
