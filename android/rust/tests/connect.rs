//! The ssh transport, against a real ssh server in this process.
//!
//! Everything here is the genuine article except the machine: a real key
//! exchange, real public-key auth, a real channel and a real exit status. What
//! that buys is the one hazard this transport has, which is that a rung's exit
//! status arrives on the same channel as its output and after the eof — a
//! client that stops reading at the eof works perfectly on every machine with
//! `mm` on the PATH and reports a hang on exactly the machines the ladder
//! exists for.

#[path = "support/sshd.rs"]
mod sshd;
#[path = "support/world.rs"]
mod world;

use manymux::proto::{Size, SpawnSpec};
use manymux_android::keys::{Identity, KnownHosts, Verdict, generate};
use manymux_android::machine::{Connection, Machine, Rebuff};
use manymux_android::ssh::{reach, start};
use russh::keys::PrivateKey;
use sshd::Sshd;
use world::{Mm, World};

/// A machine listening on loopback, with what this device knows about it.
struct Reachable {
    machine: Machine,
    identity: Identity,
    known: KnownHosts,
    /// The server's own key, so a test can ask what the store has written down
    /// about it.
    host_key: PrivateKey,
}

impl Reachable {
    async fn with(name: &str, mm: Mm) -> Self {
        let world = World::where_mm_is(name, mm);
        let host_key = generate().unwrap();
        let sshd = Sshd::listening(&world.dir, host_key.clone(), &[]).await;

        let dir = world.dir.clone();
        Self {
            machine: Machine {
                address: "127.0.0.1".to_string(),
                port: sshd.port,
                user: "whoever".to_string(),
            },
            identity: Identity::kept_at(&dir.join("id_ed25519")).unwrap(),
            known: KnownHosts::at(dir.join("known_hosts")),
            host_key,
        }
    }

    /// The same machine, on an account nobody has given this device's key to.
    async fn unwelcoming(name: &str) -> Self {
        let world = World::where_mm_is(name, Mm::OnPath);
        let host_key = generate().unwrap();
        let sshd = Sshd::refusing(&world.dir, host_key.clone()).await;

        let dir = world.dir.clone();
        Self {
            machine: Machine {
                address: "127.0.0.1".to_string(),
                port: sshd.port,
                user: "whoever".to_string(),
            },
            identity: Identity::kept_at(&dir.join("id_ed25519")).unwrap(),
            known: KnownHosts::at(dir.join("known_hosts")),
            host_key,
        }
    }

    async fn connect(&self) -> anyhow::Result<Connection> {
        Connection::open(&self.machine, &self.identity, &self.known).await
    }
}

#[tokio::test]
async fn a_host_key_never_seen_before_is_trusted_and_written_down() {
    let reachable = Reachable::with("connect-tofu", Mm::OnPath).await;

    // Nothing is known about it yet, so this is the first-use half of trust on
    // first use.
    let seen = reachable
        .known
        .verdict(&reachable.machine.at(), reachable.host_key.public_key())
        .unwrap();
    assert_eq!(seen, Verdict::New);

    reachable.connect().await.unwrap();

    // And the trust half: the key is written down, so the *next* connection is
    // the one that would notice a change. A connection that accepted a key
    // without recording it would accept a different one just as happily every
    // time, which is trust on every use and no protection at all.
    let seen = reachable
        .known
        .verdict(&reachable.machine.at(), reachable.host_key.public_key())
        .unwrap();
    assert_eq!(seen, Verdict::Known);
}

#[tokio::test]
async fn a_host_offering_a_key_that_is_not_the_one_written_down_is_refused() {
    let reachable = Reachable::with("connect-changed", Mm::OnPath).await;
    let impostor = generate().unwrap();
    reachable
        .known
        .remember(&reachable.machine.at(), impostor.public_key())
        .unwrap();

    let failed = match reachable.connect().await {
        Ok(_) => panic!("connected to a machine whose key had changed"),
        Err(error) => error,
    };

    // Typed, and typed through the context layer `open` wraps it in: this is
    // the other failure with something to press, and the app tells the two
    // apart by which one this is rather than by reading the sentence.
    let rebuff = failed
        .downcast_ref::<Rebuff>()
        .unwrap_or_else(|| panic!("a changed host key came back untyped: {failed:#}"));
    assert!(matches!(rebuff, Rebuff::Host { .. }), "{rebuff:?}");

    let error = format!("{failed:#}");
    // Both fingerprints, because the only person who can say whether this is a
    // reinstall or somebody in the middle is the one reading the message.
    assert!(error.contains("changed"), "{error}");
    assert!(
        error.contains(
            &impostor
                .public_key()
                .fingerprint(Default::default())
                .to_string()
        ),
        "{error}"
    );
    // The stored key stands. A refusal that overwrote what it was refusing on
    // behalf of would leave the second attempt succeeding.
    let seen = reachable
        .known
        .verdict(&reachable.machine.at(), impostor.public_key())
        .unwrap();
    assert_eq!(seen, Verdict::Known);
}

#[tokio::test]
async fn a_machine_that_has_not_been_given_this_devices_key_says_which_key_to_add() {
    let reachable = Reachable::unwelcoming("connect-refused").await;

    let error = match reachable.connect().await {
        Ok(_) => panic!("got in with a key the machine had never been given"),
        Err(error) => error,
    };

    // Typed rather than a sentence to read, because this is the failure with
    // the most obvious thing to do about it and the app has to be able to
    // offer it: the key to paste is in the app and nowhere else, so a screen
    // that says "add it" without handing it over is a dead end on a device
    // with no other way to get at the file.
    let rebuff = error
        .downcast_ref::<Rebuff>()
        .unwrap_or_else(|| panic!("a refused key came back untyped: {error:#}"));
    assert!(matches!(rebuff, Rebuff::Key { .. }), "{rebuff:?}");

    // And it still reads, since the same sentence goes on the screen. The
    // fingerprint is in it because a machine somebody has pasted *a* key into
    // is the confusing case: it says which one this device is offering.
    let said = rebuff.to_string();
    assert!(said.contains(&reachable.identity.fingerprint()), "{said}");
    assert!(said.contains("authorized_keys"), "{said}");
    assert!(said.contains(&reachable.machine.user), "{said}");
}

#[tokio::test]
async fn the_ladder_is_climbed_over_a_real_ssh_connection() {
    let reachable = Reachable::with("connect-ladder", Mm::InHome).await;

    let connection = reachable.connect().await.unwrap();
    let reached = reach(&connection).await.unwrap();

    // The first rung exited 127 on a channel of its own and the second answered
    // the protocol on another: the status has to have been read after the eof
    // for this to say `~/.local/bin/mm` rather than hanging or giving up.
    assert_eq!(reached.program, "~/.local/bin/mm");
    assert_eq!(reached.sessions.len(), 1);
    assert_eq!(reached.sessions[0].name, "build");
}

#[tokio::test]
async fn an_identity_is_generated_once_and_used_again_after() {
    let world = World::where_mm_is("connect-identity", Mm::Missing);
    let path = world.dir.join("id_ed25519");

    let first = Identity::kept_at(&path).unwrap();
    let again = Identity::kept_at(&path).unwrap();

    // The public half is what somebody pastes into `authorized_keys`, so a
    // second run that generated a fresh key would lock the device out of every
    // machine it had already been let into.
    assert_eq!(first.authorized_line(), again.authorized_line());
    assert!(first.authorized_line().starts_with("ssh-ed25519 "));
}

#[tokio::test]
async fn a_generated_identity_is_readable_by_nobody_else() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::where_mm_is("connect-mode", Mm::Missing);
    let path = world.dir.join("id_ed25519");

    Identity::kept_at(&path).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
}

#[tokio::test]
async fn a_session_started_on_a_machine_answers_with_the_name_it_got() {
    let reachable = Reachable::with("connect-new", Mm::OnPath).await;
    let connection = reachable.connect().await.unwrap();

    let name = start(
        &connection,
        SpawnSpec {
            name: None,
            command: vec!["sh".to_string()],
            cwd: None,
            size: Size::new(40, 8),
            label: None,
        },
    )
    .await
    .unwrap();

    // The name back rather than the name asked for: a spawn has nobody to tell
    // that a name is taken, so it takes the next free one and says which.
    assert_eq!(name, "sh-2");
}
