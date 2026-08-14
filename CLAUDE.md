# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Pass `-q` to cargo: the progress and status lines are noise, and the compiler
errors and test failures you actually want are still printed.

```bash
cargo build -q --release
cargo test -q                          # unit tests plus the end-to-end suites
cargo test -q --test remote            # one integration suite
cargo test -q --test remote a_tab_does_not_start_a_node   # one test
cargo test -q proto::                  # unit tests in one module
cargo fmt -q --check
cargo clippy -q --all-targets --locked -- -D warnings
cargo check -q --lib --no-default-features --locked       # the mobile client core
```

CI runs all of the above on Linux and macOS, plus a release build for each
shipped target (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-linux-android`). The `--no-default-features` check is not optional: the
library without `desktop` is what a mobile app links against, and it has to keep
compiling without `pty-process`, `crossterm` or `clap`.

Running the binary during development wants a socket and config of its own, or
it will talk to your real node:

```bash
MM_CONFIG_DIR=/tmp/mm-dev cargo run -- --socket /tmp/mm-dev.sock ls
MM_LOG=manymux=debug cargo run -- --socket /tmp/mm-dev.sock daemon
```

## Architecture

A **node** (`mm daemon`, `src/node/`) is one process per machine per user. It
owns that machine's PTYs and listens on a 0600 Unix socket in the per-user
runtime directory. It does no networking at all.

A **client** (`src/client/`) is everything else: `mm ls`, `mm attach`, and a
mobile app. It talks to a node over a `Stream`, which is either that Unix socket
or the stdin/stdout pipes of `ssh <host> mm agent`. `mm agent` (`node::agent`)
just bridges its own stdio to the local socket, starting a node on demand the
way tmux starts its server. A machine with no `mm` on it at all is offered one:
see the bootstrap rule below.

Three consequences run through the whole codebase and are worth keeping intact:

- **ssh is the only transport.** No addresses, keys or allowlist are stored
  anywhere. sshd decides who gets in, and `~/.ssh/config` decides how to get
  there. `src/ssh.rs` is the whole of it; connections are shared with
  `ControlMaster` so the second command to a host skips the handshake.
- **The node owns the PTY**, so a client leaving is invisible to the child: no
  SIGHUP, no EOF. `tests/persistence.rs` exists to defend this.
- **One node per user.** `ssh deploy@box` lands in deploy's node because the
  socket is under deploy's runtime directory. Nothing ever switches user or runs
  as root.

### Layers

- `src/proto.rs` is the wire protocol, shared by both halves and generic over
  `AsyncRead`/`AsyncWrite`. Frames are `[tag: u8][len: u32 BE][body]`; control
  bodies are msgpack, `DATA` bodies are raw terminal bytes so the hot path stays
  a copy. Frames are read through a `Decoder` (`FrameReader`) because both read
  loops sit in a `select!`, and a raw `read_exact` dropped mid-header
  desynchronises the stream for good.
- `src/node/session.rs` is one PTY, one child, and an `avt::Vt` fed every output
  byte so reattaching repaints the screen rather than replaying scrollback.
  `src/node/events.rs` is a second VT state machine scanning the same stream for
  what `avt` throws away: titles, bells, OSC 9/777 notifications, and the input
  modes (mouse, bracketed paste, cursor style) to replay after the repaint.
- `src/node/peers.rs` holds one long-lived `Request::Events` subscription per
  watched machine, which is how a bell on a box nobody is attached to reaches
  the desktop you are sitting at (`src/node/notify.rs`, via `osascript` or
  `notify-send`).
- `src/client/attach.rs` drives a real terminal (raw mode, focus/control modes,
  the `Ctrl-]` prefix). `src/client/status.rs` and `src/client/switch.rs` are
  deliberately terminal-free so they can be tested as string and list handling.
- `src/main.rs` is the CLI, and the only place that decides local versus remote:
  `open()` picks socket or ssh, `open_or_start()` is for commands that ask a
  machine to hold something new.

### Rules that are easy to break

- **The protocol never negotiates a version.** A fleet is updated one machine at
  a time, so both directions have to keep working. Adding a frame kind is safe
  because both ends skip unknown tags; adding a field to an existing message
  needs `#[serde(default)]` (see `Response::Attached { paste }` and the
  round-trip test in `proto.rs`). Changing the framing itself is not doable.
  Adding a `Request` variant is safe too: a node that cannot decode one answers
  with an `Error` naming its version, and that refusal is usable as an answer.
  `Request::Version` is built on it, since a node too old to say what build it
  is running is by that fact older than the build asking.
- **The socket path is worked out from the uid and nothing else**
  (`/tmp/manymux-<uid>`, `src/config.rs`). It used to follow `$XDG_RUNTIME_DIR`,
  which pam_systemd sets and an embedded ssh server, a system unit or cron do
  not, so the same account got a second empty node depending on how it logged
  in. Reading anything environmental back into that path brings the split back.
  `/tmp` being world-writable is why `ensure_runtime_dir` checks the owner and
  mode, and why `ipc` touches the socket every few hours: `systemd-tmpfiles`
  deletes what looks unused for ten days.
- **A tab completion never starts a node, never installs anything, and never
  waits on ssh unless the word already names a machine** (`src/complete.rs`).
  All three are tested.
- **A machine that answers 127 has no `mm`, and that is the only sign there is.**
  `client::PROGRAMS` is the ladder of names to try, and it exists because an
  install without root lands in `~/.local/bin`, which is not on the PATH sshd
  hands a command, so such a machine looks empty until it is named outright.
  When the ladder runs out, `Stream::reopen` asks for consent and runs
  `install.sh` over ssh (`mm setup <host>` does the same thing by hand).
  Resending the request afterwards is safe *because* 127 means nothing ran; any
  other failure must not be retried that way. Consent is a callback the CLI
  supplies, so the daemon watching peers and tab completion pass `None` and can
  never install anything on their own.
- **`mm agent` must leave stdout strictly alone**: the protocol is on it. Only
  the daemon opens a log file; everything else logs to stderr.
- **Modes switched on for a session must be switched off on detach.**
  `events::REPLAYED_MODES` and the teardown in `client::attach` are a pair, and
  so are `events::Keyboard` and the pops in the same teardown.
- **The mode key has three spellings, not one.** A program that asks for the
  kitty keyboard protocol (`CSI > 7 u`, which `pi` sends on startup) or for
  xterm's `modifyOtherKeys` changes how the *terminal* encodes every chord, so
  Ctrl-] stops arriving as 0x1d and starts arriving as `CSI 93 ; 5 u`. Watching
  for the byte alone means a session running one of those programs cannot be
  left. `attach::Encoded` reads all three, and drops the repeats, releases and
  bare modifier keys that the same protocols add, since in control mode letting
  go of ctrl would otherwise read as a keystroke.
- Session names are sanitised in `node::registry` because they appear in
  `host/name` paths and in the terminal.

### Tests

`tests/remote.rs` builds a temporary world: two nodes with their own
`MM_CONFIG_DIR` and `--socket`, and a shell script on `MM_SSH` that stands in
for ssh by running the other node's agent directly. That covers CLI, ssh
invocation, agent and remote node without an sshd. `tests/persistence.rs` drives
`Node::handle` over an in-memory duplex, and uses tokio's `start_paused` to skip
the client-liveness deadline.

## Conventions

- Comments say why, not what, and the interesting reasoning lives in module-level
  `//!` docs. When changing behaviour these docs are usually the thing that needs
  updating with it.
- Test names are sentences: `a_bare_name_finds_a_session_on_another_machine`.
- Commit subjects are conventional commits with a lowercase, plainly worded
  description: `fix(ssh): create the directory the control socket goes in`.
- `contrib/` holds the service unit templates that `src/service.rs` writes;
  `install.sh` is linted and end-to-end tested by its own workflow, and the
  platform reasoning in it (Linux `/usr/local/bin` versus macOS `~/.local/bin`)
  is load-bearing for whether `ssh host mm agent` can find the binary at all.
  It takes root only when `can_sudo` says it is free: the account being
  bootstrapped is usually a deploy user with a key, no password and no sudoers
  entry, and prompting it wastes three attempts and then fails the install.
  Falling back to `~/.local/bin` is safe because the client ladder looks there.
