//! Reach a machine from a desktop, with no phone involved.
//!
//! The app's client stack, driven from a terminal: the same ssh transport, the
//! same ladder, the same screen. A phone is a slow place to find out that a key
//! was refused or that a machine runs a node too old to attach to, so this is
//! where those get found out.
//!
//! ```text
//! cargo run --example reach -- user@host            # what is running there
//! cargo run --example reach -- user@host build      # attach and print a screen
//! ```
//!
//! The identity and the note of each machine's host key go in `MM_PHONE_DIR`,
//! or `.manymux-phone` beside wherever this is run. The public half is printed
//! the first time, since a machine has to be told about it before it will let
//! this in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use manymux::proto::Size;
use manymux_android::keys::{Identity, KnownHosts};
use manymux_android::machine::{Connection, Connections, Machine};
use manymux_android::session::{Session, State};
use manymux_android::ssh::reach;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(destination) = args.next() else {
        bail!("usage: reach <user@host[:port]> [session]");
    };
    let machine = parse(&destination)?;
    let session = args.next();

    let dir = PathBuf::from(
        std::env::var("MM_PHONE_DIR").unwrap_or_else(|_| ".manymux-phone".to_string()),
    );
    let fresh = !dir.join("id_ed25519").exists();
    let identity = Identity::kept_at(&dir.join("id_ed25519"))?;
    let known = KnownHosts::at(dir.join("known_hosts"));
    if fresh {
        println!("this device's key is new. Put it in that account's authorized_keys:");
        println!("{}", identity.authorized_line());
    }

    match session {
        None => {
            let connection = Connection::open(&machine, &identity, &known).await?;
            let reached = reach(&connection).await?;
            println!("reached over `{}`", reached.program);
            for session in reached.sessions {
                println!("{:<20} {:>6}  {}", session.name, session.pid, session.title);
            }
            connection.close().await;
        }
        Some(name) => {
            let size = Size::new(80, 24);
            let session = Session::open(
                machine,
                identity,
                known,
                Arc::new(Connections::none()),
                name,
                size,
            );
            print(&session, size).await;
        }
    }
    Ok(())
}

/// Wait for the screen to arrive, print it once, and leave.
async fn print(session: &Session, size: Size) {
    let mut painted = vec![String::new(); size.rows as usize];
    for _ in 0..200 {
        for row in session.take_frame().changed {
            let at = row.at as usize;
            if at < painted.len() {
                painted[at] = row.runs.iter().map(|run| run.text.as_str()).collect();
            }
        }
        if let State::Failed { why } = session.state() {
            eprintln!("{why}");
            return;
        }
        if painted.iter().any(|row| !row.trim().is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for row in painted {
        println!("{}", row.trim_end());
    }
    session.detach();
}

/// `user@host` or `user@host:port`, where the host may be a v6 literal.
fn parse(destination: &str) -> Result<Machine> {
    let Some((user, rest)) = destination.split_once('@') else {
        bail!("that is not a destination: it wants `user@host` or `user@host:port`");
    };
    // A v6 literal is mostly colons, so a port is only a port after a `]` or
    // where there is exactly one colon to be found. Splitting on the last one
    // regardless reads `fd00::1` as the host `fd00:` on port 1.
    let (address, port) = match rest.rsplit_once(']') {
        Some((inside, after)) => {
            let address = inside.trim_start_matches('[');
            match after.strip_prefix(':') {
                Some(port) => (address, port.parse()?),
                None => (address, 22),
            }
        }
        None if rest.matches(':').count() == 1 => {
            let (address, port) = rest.rsplit_once(':').expect("one colon");
            (address, port.parse()?)
        }
        None => (rest, 22),
    };
    Ok(Machine {
        address: address.to_string(),
        port,
        user: user.to_string(),
    })
}
