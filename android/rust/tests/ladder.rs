//! Climbing [`manymux::client::PROGRAMS`] on a machine reached in-process.
//!
//! ssh is stood in for by a local `/bin/sh`, the way `tests/remote.rs` stands
//! in for it one layer up: what is under test is which rung answers and what
//! the client does about a rung that does not, and neither of those is about
//! ssh. The shell is a real one, which is the point — it resolves the first
//! rung against a PATH, expands the `~` in the second, and says 127 about a
//! spelling that is neither. Those three answers are the whole of what the
//! ladder reads, and a double would just be the test agreeing with itself
//! about what they mean.
//!
//! `tests/connect.rs` asks the same questions again over a real ssh
//! connection, where the answers have to survive a channel.

#[path = "support/world.rs"]
mod world;

use manymux_android::ssh::{Exec, Remote, reach};
use world::{Mm, World};

/// What a machine whose `mm` does not run says on its way out.
const COMPLAINT: &str = "mm: error while loading shared libraries";

/// A machine whose commands go through a real `/bin/sh`.
///
/// The shell is the point. It is what expands the `~` in the second rung, what
/// resolves the first against a PATH, and what says 127 about a spelling that
/// is neither — the three things the ladder is reading, all of them answered by
/// the same shell that would answer them at the other end of an ssh.
struct Local(World);

impl Exec for Local {
    async fn open(&self, command: &str) -> anyhow::Result<Remote> {
        let mut sh = tokio::process::Command::new("/bin/sh");
        sh.arg("-c").arg(command);
        // A PATH of our own, or a machine that happens to have a real `mm`
        // installed on it would answer the first rung and the test would pass
        // for a reason that has nothing to do with the code.
        sh.env("PATH", self.0.dir.join("bin"));
        sh.env("HOME", self.0.dir.join("home"));
        if self.0.broken {
            sh.env("STUB_FAILS", COMPLAINT);
        }
        Remote::spawn(sh).await
    }
}

#[tokio::test]
async fn a_machine_with_mm_on_the_path_answers_on_the_first_rung() {
    let machine = Local(World::where_mm_is("ladder-onpath", Mm::OnPath));

    let reached = reach(&machine).await.unwrap();

    assert_eq!(reached.program, "mm");
    assert_eq!(reached.sessions.len(), 1);
    assert_eq!(reached.sessions[0].name, "build");
}

#[tokio::test]
async fn a_machine_with_mm_only_in_the_home_directory_is_found_on_the_second_rung() {
    let machine = Local(World::where_mm_is("ladder-inhome", Mm::InHome));

    let reached = reach(&machine).await.unwrap();

    assert_eq!(reached.program, "~/.local/bin/mm");
    assert_eq!(reached.sessions.len(), 1);
}

#[tokio::test]
async fn a_machine_with_no_mm_anywhere_is_reported_rather_than_installed_on() {
    let machine = Local(World::where_mm_is("ladder-missing", Mm::Missing));

    let error = match reach(&machine).await {
        Ok(_) => panic!("reached a machine with no `mm` on it"),
        Err(error) => format!("{error:#}"),
    };

    // The rungs are exhausted rather than the failure being reported as the
    // first one's: a machine that answered 127 twice has no `mm`, and saying so
    // is the whole of what this end may do about it. Nothing is installed, here
    // or anywhere: `Consent` is never constructed.
    assert!(error.contains("no `mm`"), "{error}");
}

#[tokio::test]
async fn a_rung_that_failed_for_any_other_reason_stops_the_climb_and_is_reported() {
    let machine = Local(World::where_mm_is("ladder-broken", Mm::Broken));

    let error = match reach(&machine).await {
        Ok(_) => panic!("reached a machine whose `mm` does not work"),
        Err(error) => format!("{error:#}"),
    };

    // Not "no `mm`": the ladder must not have climbed past this. 127 is a
    // spelling that was wrong, and every other status is a machine that
    // answered badly — trying the next spelling would ask a broken machine the
    // same question twice and then blame the last rung for it.
    assert!(
        !error.contains("no `mm`"),
        "climbed past a real failure: {error}"
    );
    // And what it said comes back, because that is the only account anybody
    // gets of why. The complaint is held while the ladder might still climb,
    // not thrown away.
    assert!(error.contains(COMPLAINT), "{error}");
}
