//! The ssh transport, against a real ssh server in this process.
//!
//! Everything here is the genuine article except the machine: a real key
//! exchange, real public-key auth, a real channel and a real exit status. What
//! that buys is the one hazard this transport has, which is that a rung's exit
//! status arrives on the same channel as its output and after the eof — a
//! client that stops reading at the eof works perfectly on every machine with
//! `mm` on the PATH and reports a hang on exactly the machines the ladder
//! exists for.

#[path = "support/agent.rs"]
mod agent;
#[path = "support/sshd.rs"]
mod sshd;
#[path = "support/world.rs"]
mod world;

use std::slice::from_ref;

use agent::Agency;
use manymux::proto::{Size, SpawnSpec};
use manymux_android::agent::Agent;
use manymux_android::keys::{Identity, KnownHosts, Verdict, generate};
use manymux_android::machine::{Connection, Machine, Rebuff};
use manymux_android::ssh::{reach, start};
use russh::keys::{PrivateKey, PublicKey};
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
    /// Held so a test can ask the machine what it saw, which for a device with
    /// more than one key to offer is the only way to tell which was used.
    sshd: Sshd,
}

/// Whose key is the one line in a machine's `authorized_keys`.
enum Allows {
    TheAgent,
    ThisDevice,
}

impl Reachable {
    async fn with(name: &str, mm: Mm) -> Self {
        let world = World::where_mm_is(name, mm);
        let host_key = generate().unwrap();
        let sshd = Sshd::listening(&world.dir, host_key.clone(), &[]).await;
        Self::knowing(&world, host_key, sshd)
    }

    /// The same machine, on an account nobody has given this device's key to.
    async fn unwelcoming(name: &str) -> Self {
        let world = World::where_mm_is(name, Mm::OnPath);
        let host_key = generate().unwrap();
        let sshd = Sshd::refusing(&world.dir, host_key.clone()).await;
        Self::knowing(&world, host_key, sshd)
    }

    /// And the same machine again, going while the key is being offered.
    async fn going_mid_auth(name: &str) -> Self {
        let world = World::where_mm_is(name, Mm::OnPath);
        let host_key = generate().unwrap();
        let sshd = Sshd::going_mid_auth(&world.dir, host_key.clone()).await;
        Self::knowing(&world, host_key, sshd)
    }

    /// A machine reached over a mesh, which knows who this is before the ssh
    /// starts and asks for nothing.
    async fn unasking(name: &str) -> Self {
        let world = World::where_mm_is(name, Mm::OnPath);
        let host_key = generate().unwrap();
        let sshd = Sshd::unasking(&world.dir, host_key.clone()).await;
        Self::knowing(&world, host_key, sshd)
    }

    /// The same, not admitting this device.
    async fn unwanting(name: &str) -> Self {
        let world = World::where_mm_is(name, Mm::OnPath);
        let host_key = generate().unwrap();
        let sshd = Sshd::unwanting(&world.dir, host_key.clone()).await;
        Self::knowing(&world, host_key, sshd)
    }

    /// A machine with one line in its `authorized_keys`, and an agent on this
    /// device holding one key of its own.
    ///
    /// `allows` says whose key that line is, which is the whole of what the two
    /// tests using this differ by. The agent's key comes back so a test can
    /// name it.
    async fn with_an_agent(name: &str, allows: Allows) -> (Self, PrivateKey) {
        let world = World::where_mm_is(name, Mm::OnPath);
        let host_key = generate().unwrap();
        let identity = Identity::kept_at(&world.dir.join("id_ed25519")).unwrap();
        let theirs = generate().unwrap();

        let allowed = match allows {
            Allows::TheAgent => theirs.public_key().clone(),
            Allows::ThisDevice => PublicKey::from_openssh(&identity.authorized_line()).unwrap(),
        };
        let sshd = Sshd::taking(&world.dir, host_key.clone(), &allowed).await;
        let agency = Agency::holding(&world.dir.join("agent.sock"), from_ref(&theirs)).await;

        let mut reachable = Self::knowing(&world, host_key, sshd);
        reachable.identity = identity.asking(Some(Agent::on(&agency.at)));
        (reachable, theirs)
    }

    /// What this device knows about a machine.
    fn knowing(world: &World, host_key: PrivateKey, sshd: Sshd) -> Self {
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
            sshd,
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
async fn a_machine_that_goes_while_being_asked_is_not_read_as_a_refused_key() {
    let reachable = Reachable::going_mid_auth("connect-vanished").await;

    let error = match reachable.connect().await {
        Ok(_) => panic!("authenticated against a machine that went"),
        Err(error) => error,
    };

    // russh answers a session that ended under the question with the same
    // "not authenticated" it answers a refusal with, so a client reading only
    // whether it got in tells somebody to paste a key into a machine that never
    // looked at one. The cost of that is a person going to another device,
    // editing `authorized_keys` on the strength of it, coming back and being
    // told the same thing.
    assert!(
        error.downcast_ref::<Rebuff>().is_none(),
        "a connection that went was reported as a key: {error:#}"
    );

    // And what it says is that the connection went, since that is all anybody
    // knows. Spelled out because the two failures with an empty answer behind
    // them are told apart here and nowhere else: a machine that offers only
    // `none` says nothing on the wire either (RFC 4252 keeps `none` out of the
    // list of ways to continue), so a client reading the list alone calls a
    // live machine a dropped connection.
    let said = format!("{error:#}");
    assert!(said.contains(&reachable.machine.at()), "{said}");
    assert!(said.contains("went"), "{said}");
    assert!(!said.contains("authorized_keys"), "{said}");
}

#[tokio::test]
async fn a_machine_that_knows_this_device_already_is_not_asked_for_a_key() {
    let reachable = Reachable::unasking("connect-unasked").await;

    // A mesh proves who a peer is by the link the connection arrived over, so
    // its ssh offers the `none` method and nothing else: there is no
    // `authorized_keys` anywhere in it, and a client that opens by offering a
    // key is told no by a machine that would have let it straight in. Every
    // stock `ssh` asks with `none` first, which is why this works from a
    // terminal on the same phone and did not work from the app.
    let connection = reachable.connect().await.unwrap();
    let reached = reach(&connection).await.unwrap();
    assert_eq!(reached.sessions.len(), 1);
}

#[tokio::test]
async fn a_machine_that_asks_for_nothing_and_says_no_does_not_send_anybody_to_a_key_file() {
    let reachable = Reachable::unwanting("connect-unwanted").await;

    let error = match reachable.connect().await {
        Ok(_) => panic!("got in to a machine that does not admit this device"),
        Err(error) => error,
    };

    // It never asked for a key, so no key would have changed the answer and
    // there is nowhere to paste one. The machine is where the decision is.
    assert!(
        error.downcast_ref::<Rebuff>().is_none(),
        "a machine that asked for no key was reported as one refusing it: {error:#}"
    );
    let said = format!("{error:#}");
    assert!(said.contains(&reachable.machine.at()), "{said}");
    assert!(said.contains("never asked for a key"), "{said}");
    assert!(!said.contains("authorized_keys"), "{said}");
}

#[tokio::test]
async fn a_key_an_agent_is_holding_is_offered_and_gets_in() {
    let (reachable, theirs) = Reachable::with_an_agent("connect-agent", Allows::TheAgent).await;

    // This device's own key is not in that machine's `authorized_keys` and the
    // agent's is, which is the ordinary shape at a desk: the machines already
    // know the keys somebody works with, and having to paste a fresh one into
    // every account before this can be pointed at them is a poor trade.
    reachable.connect().await.unwrap();
    assert_eq!(
        reachable.sshd.admitted().as_ref(),
        Some(theirs.public_key())
    );
}

#[tokio::test]
async fn this_devices_key_is_still_offered_after_the_agent_had_nothing_that_worked() {
    let (reachable, theirs) = Reachable::with_an_agent("connect-spare", Allows::ThisDevice).await;

    // An agent is somebody's whole working set, and most of it is for machines
    // that are not this one. So a key it holds being turned down is the
    // ordinary case rather than the end of the ladder, and the key this device
    // generated for itself is the rung after it.
    reachable.connect().await.unwrap();
    let admitted = reachable.sshd.admitted().unwrap();
    assert_ne!(&admitted, theirs.public_key());
    assert_eq!(
        admitted.fingerprint(Default::default()).to_string(),
        reachable.identity.fingerprint()
    );
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
