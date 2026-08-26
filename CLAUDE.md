# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

`just` is the way in and `just --list` says what there is. The recipes are
split the way the tree is: `test`, `lint` and `core` are the root crate,
`android-test` and `android-lint` are the shim under `android/rust`, and
`everything` is what CI runs in the order it runs it.

Pass `-q` to cargo: the progress and status lines are noise, and the compiler
errors and test failures you actually want are still printed.

```bash
just everything                        # lint, core, test, android-lint, android-test
cargo build -q --release
cargo test -q                          # unit tests plus the end-to-end suites
cargo test -q --test remote            # one integration suite
cargo test -q --test remote a_tab_does_not_start_a_node   # one test
cargo test -q proto::                  # unit tests in one module
cargo fmt -q --check
cargo clippy -q --all-targets --locked -- -D warnings
cargo check -q --lib --no-default-features --locked       # the mobile client core
```

The shim is a separate crate outside the workspace, so a root `cargo test`
never builds it and it has to be asked for by name:

```bash
cd android/rust && cargo test -q                  # host target, no NDK needed
cd android/rust && cargo test -q --test ladder    # one suite
just android                                      # the debug APK, both ABIs
just android-install                              # onto whatever is plugged in
just android-log                                  # what the app is saying
just reach user@host                              # the app's client stack, from a terminal
```

CI runs the root crate's lot on Linux and macOS, plus a release build for each
shipped target (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-linux-android`), the shim on the host target,
and `assembleDebug` for the app, which is the only job that compiles any
Kotlin. The `--no-default-features` check is not optional: the
library without `desktop` is what the app links against, and it has to keep
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
  the desktop you are sitting at (`src/notify.rs`, via `osascript` or
  `notify-send`).
- `src/notify.rs` decides what is worth interrupting someone over, for both ways
  out: the desktop tools above, and `escape()`, the OSC 9 an attached client
  hands to the terminal it is sitting in. `src/settings.rs` is `settings.toml`
  beside the host list, which so far holds only `notify`.
- `src/client/attach/` is the attached client, in three parts split by what
  needs a terminal. `keys.rs` turns bytes off stdin into what the client was
  asked for and touches no terminal at all, which is why most of its 1100 lines
  of tests are there; `terminal.rs` is raw mode, escape sequences and the pump
  loop, and is desktop-only; `mod.rs` is the vocabulary they share plus
  `collect_until`, the way in for a caller with no terminal.
  `src/client/status.rs`, `src/client/switch.rs`, `src/client/picker.rs` and
  `src/client/groups.rs` are terminal-free for the same reason.
- `src/client/groups.rs` is `groups.toml`, beside the host list: which sessions
  you are treating as one piece of work, spanning machines. `src/client/picker.rs`
  is the list control mode draws, filled with sessions or with groups.
- `src/main.rs` is the CLI, and the only place that decides local versus remote:
  `open()` picks socket or ssh, `open_or_start()` is for commands that ask a
  machine to hold something new. Beside it and belonging to the binary rather
  than the library: `src/target.rs` turns a typed word into a machine and a
  session, `src/complete.rs` answers a tab, `src/completions.rs` works out
  where a shell reads its completion script from.

### Rules that are easy to break

Most of the reasoning in this codebase lives in the module it belongs to, in
the `//!` doc at the top of the file. That is where it is read: you open
`src/client/scroll.rs` to change scrolling, and the argument for what is there
is the first thing on the screen. Before changing behaviour in one of these,
read its `//!` first, and update it in the same commit:

| For | Read |
| --- | --- |
| the wire, and what a node of another age does with it | `src/proto.rs` |
| groups, and why they are the client's | `src/client/groups.rs` |
| checkpoints, restarts, and reading `/proc` | `src/client/checkpoint.rs`, `src/foreground.rs` |
| the popup and its two lists | `src/client/picker.rs` |
| where a switch key lands, and in what order | `src/client/switch.rs` |
| the two screen modes | `src/client/screen.rs` |
| the history view, selection and the mouse | `src/client/scroll.rs` |
| what a key arrives as, and the modes that respell it | `src/client/attach/keys.rs` |
| titles, bells and the modes replayed on attach | `src/node/events.rs` |
| release signing | `src/signature.rs` |
| what a bell is worth interrupting somebody over | `src/notify.rs` |

What is below is what no single module owns: the rules that hold between two
of them, the ones about a fleet rather than a process, and the handful of
things that must simply never be done.

**The fleet**

- **The protocol never negotiates a version.** A fleet is updated one machine at
  a time, so both directions have to keep working. Adding a frame kind is safe,
  both ends skipping unknown tags; adding a field to an existing message needs
  `#[serde(default)]` (`Response::Attached { paste }`, and the round-trip test
  in `proto.rs`). Changing the framing itself is not doable. Adding a `Request`
  variant is safe too: a node that cannot decode one answers with an `Error`
  naming its version, and that refusal is usable as an answer, which is what
  `Request::Version` is built on.
- **A capability is answered for, never assumed.** A node too old to know a
  request skips it in silence, so `Response::Attached` carries a flag per
  capability (`paste`, `scroll`, `rename`, `events`) and the client says "this
  host is too old" rather than leaving a key that does nothing. `read_only` is
  the one that must be checked rather than treated as a key that does nothing:
  a node too old to know it hands back an ordinary attach with a live keyboard,
  so `mm view` refuses a host that does not set the flag. A promise nobody made
  must not be assumed. Note that replacing the binary is not enough for any of
  this: the node keeps running the build it started from until `mm restart`,
  which `update::is_stale` says for this machine and nothing says for a host
  reached over ssh.
- **The socket path is worked out from the uid and nothing else**
  (`/tmp/manymux-<uid>`, `src/config.rs`). It followed `$XDG_RUNTIME_DIR` once,
  which pam_systemd sets and an embedded ssh server, a system unit or cron do
  not, so one account got a second empty node depending on how it logged in.
  Anything environmental read back into that path brings the split back. `/tmp`
  being world-writable is why `ensure_runtime_dir` checks owner and mode, and
  why `ipc` touches the socket every few hours: `systemd-tmpfiles` deletes what
  looks unused for ten days.
- **A machine that answers 127 has no `mm`, and that is the only sign there is.**
  `client::PROGRAMS` is the ladder of names to try, and it exists because an
  install without root lands in `~/.local/bin`, which is not on the PATH sshd
  hands a command. When the ladder runs out, `Stream::reopen` asks for consent
  and runs `install.sh` over ssh. Resending the request afterwards is safe
  *because* 127 means nothing ran; any other failure must not be retried that
  way. Consent is a callback the CLI supplies, so the daemon watching peers and
  tab completion pass `None` and can never install anything on their own.
  Climbing the ladder is silent, ssh's stderr held by `client::relay` and thrown
  away on the rung that answered 127, or the remote shell's `mm: command not
  found` would print on every command reaching such a machine.
- **The daemon gives up on a machine it cannot reach, and a client is what
  starts it again.** `peers::retry_after` is three growing delays and then
  nothing: a retry is an ssh process, a name to resolve and a connect to wait
  out, per watched machine. What replaces the timer is `Request::Reached`, sent
  by any client command that got an answer out of a machine (`note_reached`),
  which is the only way this node can learn the network is different: a client
  reaches another machine over its own ssh and the node never sees it. Two
  things this rests on. A watcher that gave up is a *finished task still in the
  map*, because a task cannot tidy its own entry away without racing whoever is
  replacing it, so `Peers::watched` and `sync` read `is_finished` and nothing
  may go back to `contains_key`. And the attempt count resets only for a
  subscription that lasted `peers::STEADY`, or a host that accepts ssh and hangs
  up immediately would be retried every five seconds forever.

**The node**

- **A node started on demand leaves the client's process group and session**
  (`setsid` in `node::start_node`, tested in `tests/detached.rs`). It owns every
  session on the machine from then on, while the client is a command someone
  typed: without the split, a terminal signalling its foreground group (a Ctrl-C
  at the wrong moment, the window closing) reaches the node too and takes every
  session down at once, with no line in the log because the node is killed
  before it can write one. A node started by the service unit is already clear
  of this, which is why the symptom only ever appears on the on-demand path.
- **A node on its way out hangs its sessions up itself, and outlives none of
  them** (`Node::shutdown`, from `Request::Stop` and a SIGTERM handler in
  `main`). Closing the PTY masters would end them anyway, but the kernel sends
  that hangup to the terminal's *foreground* group, which is whatever the shell
  last put there: a shell sitting behind a full-screen program is not in it and
  finds out only by reading EIO, well past writing its history. So the group is
  signalled deliberately, given `GRACE` to go, and what is left is killed. The
  kill is the child alone and never the group, because something that survived a
  hangup is ignoring hangups on purpose and `nohup` has to keep meaning what it
  means. `GRACE` also has to stay inside the window `node::stop` gives the
  socket to go quiet, or `mm stop` reports a node still listening when it is
  only still saying goodbye.
- **Watching is enforced at the node.** `mm view` is an attach with `read_only`,
  and what makes it worth pointing at a session somebody else is working in is
  that `Attachment::send_input` drops the bytes rather than the client agreeing
  not to send them. Two consequences that are choices rather than accidents: a
  viewer is left out of `State::clients`, so somebody looking on from a phone
  cannot reflow the screen of whoever is typing, and it is shown the geometry
  the session is already at; but it does count in `host_clients`, because a
  person watching is a person present and a bell should reach the terminal they
  are sitting at.
- **A peek makes no client, which is the whole reason it exists**
  (`Request::Peek`, `Session::peek`, `Registry::peek`, tested in
  `tests/persistence.rs`). Attaching read-only would answer the same question
  and is wrong twice over: a viewer counts in `host_clients`, so drawing a wall
  of thumbnails would tell the machine somebody is sitting at it and stop a bell
  reaching the desktop, and it is one attach per tile for one picture each. So
  nothing is added to `clients` and the geometry is left alone. It is
  `vt.dump()` alone rather than `repaint()`, the modes in the second half being
  for a terminal about to be typed into: a caller looking at a picture has no
  business turning on mouse reporting for a session it is not attached to.
- **A session's name can change under whatever is holding it.** `Session::name`
  is behind a lock and read through `name()`, because the registry's key moves
  with a rename and so does every event the session publishes. Anything that
  remembers a name instead of asking is wrong by the next rename: the exit
  watcher in `registry::spawn` prunes by what has exited rather than by the name
  it was spawned under, which by then may be a *different*, live session, and
  `pump_attachment` compares an event against `attachment.name()` so a client
  does not hear its own bells as the session next door's.
- **A rename moves the name, and the title stays the program's.** The name says
  which session this is and goes in `host/name`, so it is sanitised in
  `node::registry` and refused when another session already holds it: a spawn
  has nobody to tell and takes the next free counter, but a rename was typed by
  somebody who would rather hear "that name is taken" than land on `build-2`.
  Which is why `Registry::rename` answers with the name that stuck rather than
  `Ok(())`: what was typed and what the session ended up called are not always
  the same string, and the client is drawing one of them.
- **A bell goes to exactly one of two places, and the event says which.** An
  attach stream carries this machine's other sessions' events unasked
  (`pump_attachment`), so whoever is sitting in one session hears the one next
  door; over ssh that is the only route that reaches a person at all.
  `SessionEvent::host_attached` counts the clients attached anywhere on the
  machine, and a desktop notifier that sees one keeps quiet
  (`notify::for_desktop`), or one bell interrupts twice. A relayed bell is an
  OSC 9 *and* a bell (`notify::escape`): OSC 9 is seen and not heard, so a
  terminal that has it shows a banner and never rings, and one that does not
  throws the whole thing away. The OSC ends with ST rather than BEL, a
  terminator being eaten by the parser where a bell would not be. The session
  on the screen is not relayed at all; its own BEL is already in the stream.

**The attached client**

- **A connection that drops is waited out, not reported.** The session is still
  running on a machine that never noticed the client left, and outliving the
  connection is the one thing the project is for, so the clock has nothing to
  decide. `do_attach` keeps `held`: the terminal stays in raw mode showing the
  screen as the session last painted it, and only the mark row changes
  (`terminal::waiting`). `attach::reconnect_after` is short at first, then flat
  at ten seconds, and never runs out. Three things it needs. The wait reads the
  keyboard, or nobody could leave it, and it is the *only* way out: the mode
  key's detach, and Ctrl-C, which has nowhere else to go while there is no
  session to send it to. The row keeps moving, counting the delay down a second
  at a time and saying `reconnecting` while an attempt is out
  (`status::waiting_notice`) — everything above it is the session as it was
  painted before the drop, so a row written once and left there is a client that
  has died as far as anybody watching can tell. And once this run has attached
  to anything at all, a failed reattach is another lost attempt rather than an
  error (`attached` in `do_attach`); only the first attach may fail outright,
  that one being a command that did not work rather than a connection that went.
- **An attempt to get back has a deadline, because the waiting does not**
  (`main::REACH_FOR`, ten seconds, applied by `reaching` and only once
  `attached`). The wait reads the keyboard throughout; the *attempt* reads
  nothing, so one that never returns is a terminal frozen on `reconnecting` with
  a row that has stopped counting and a Ctrl-C nobody is reading. ssh gets there
  with no network down at all: `ControlMaster=auto` shares one connection and
  connecting to a control socket has no deadline of any kind, so a wedged master
  hangs every client until it expires on its own schedule. Dropping the attempt
  ends it, because `ssh::spawn` sets `kill_on_drop`. Reconnects only: the first
  attach of a run may legitimately be a cold ssh, a node starting, or an install
  being answered.
- **A wait hands the keyboard back to the session, and it is the only thing that
  does.** Control mode survives a reattach on purpose, a hop setting it so the
  key after one carries on walking (`mode` in `do_attach`). Nothing else turns
  it off, so a reconnect that kept it came back with the client holding the
  keyboard, where `Ctrl-]` *leaves* control mode and the `tab` behind it is a tab
  into somebody's shell. `wait_to_reconnect` clears it, covering both ways in,
  and the reason is that a wait is not a hop: nobody pressed anything.
- **What a reconnect goes back to is the session you were in, and the two ways
  an attach can fail mean opposite things.** The loop reattaches to
  `cycle.current()`, which a hop has already moved. `open()` failing is a
  machine that never answered and is waited for; an error out of
  `Stream::attach` is a node that answered and has no such session
  (`main::Missed`), which is a listing gone stale under the switch keys and the
  one case that forgets the entry and undoes the hop. Reading every failure the
  second way meant a closed lid mid-hop dropped a live session from the cycle
  and ended the attach. `hopped` is cleared on a landing as well as by the
  outcomes, since a stale listing is a thing that can be true of one attempt and
  not of the run: left set for the life of the attach, an hour-later node
  restart came back as `Missed::Gone`, was read as that hop going stale, and the
  run walked back to the session the command line named and waited forever.
- **A session that ends hands back the one you came from, and only the session
  you named ends the attach.** `Cycle::fall_back` is the whole decision: a run
  that has hopped goes back, while a run that has not is `mm attach host/name`
  doing what it was asked, so it prints the line and leaves with the status.
  `mm attach box/build; echo $?` is a thing people write. The exit is *carried*
  while the fall-back is attempted (`ended` in `do_attach`), because that attach
  can fail too, and answering that with the wait a dropped connection gets would
  sit forever on a session nobody is running. So a `Missed::Gone` while one is
  carried reports the exit instead of waiting.
- **Modes switched on for a session must be switched off on detach.**
  `events::REPLAYED_MODES` and the teardown in `client::attach` are a pair, and
  so are `events::Keyboard` and the pops beside them. A hop counts as a detach
  for the session being left, so the same undoing happens per attach
  (`terminal::takeover`), and the screen is cleared in the same breath: `avt`'s
  dump paints from the cursor down and never erases, so without it the session
  you left shows through. What a hop must *not* undo is the pushed title, which
  belongs to the whole run of attaches, nor the screen, which belongs to
  whichever mode is in use.
- **A client of ours that owns part of the screen owns all of it, so the session
  stops being painted while one is up.** With the session still painting, a box
  drawn over it is gone within a second, and redrawing after every chunk is
  worse: a line printed *scrolls the screen* and takes the box with it, leaving
  one copy per line. There is no third answer. A terminal composes nothing, and
  the client is not the emulator here: the node holds the screen, which is
  exactly why tmux can float a popup over live output and this cannot. What
  pauses is the picture and never the program. What that owes on the way back is
  in `terminal::given_back`: a replay can only say which modes are *on*, so the
  set it answers for is switched off first, and the client's own hold on the
  mouse is re-asserted by hand beside the erase, being spelt `?1000h` like a
  session's and given back with the rest otherwise.
- **What is drawn to the node is asked for and painted as one act**
  (`terminal::ask_for_the_screen`). A dump starts painting wherever the cursor
  is and never erases, so a screen asked for and painted where it fell walks its
  own first rows off the top. The erase and home are `REGROWN`, spent against
  `owed`. These were two statements at four call sites, and the one place the
  second was missing showed nothing until a pager exited: `less` leaves the
  cursor at the bottom, so quitting one gave back the last few lines of the
  session under a blank half screen. A resize is repainted the same way, from
  the node rather than left to the session (`SessionWriter::resync`): a shell
  that printed and went quiet has no answer to a SIGWINCH, so the screen keeps
  the old geometry. The scrolling region goes out first, or the dump's newlines
  scroll against the old fence.
- **What an attached client asks for goes on the attach stream, never down a
  second connection.** An `Attached` holds neither the socket nor the host it
  arrived by, on purpose: that is the one thing this half of the client is kept
  from knowing, and a mobile app drives the same type. So `tag::RENAME` carries
  a name the way `tag::VIEW` and `tag::FIND` carry their questions, and the node
  has the session right there at the other end. What does *not* go on the stream
  is the key that starts a session (`Action::New`, `Outcome::New`), which looks
  like the same shape and is not: a new one needs a host to start it on, a name
  back and a fresh attach, none of which this half may know. So it is handed
  back the way a switch is, and `main::start_beside` does the work.
- **Every key the client reads has three spellings, not one.** A program that
  asks for the kitty keyboard protocol (`CSI > 7 u`, which `pi` sends on
  startup) or for xterm's `modifyOtherKeys` changes how the *terminal* encodes
  every chord, so Ctrl-] stops arriving as 0x1d and starts arriving as
  `CSI 93 ; 5 u`. `attach::Encoded` reads all three and drops the repeats,
  releases and bare modifier keys those protocols add. Three consequences, each
  of which showed up only while such a program was running. Every mode's key
  table is written against the byte the ordinary encoding would have sent and
  `Encoded::byte` reads a key back to it, so the long spelling and the short one
  cannot answer differently. A key held down arrives as a *repeat* rather than
  the plain byte, so `Encoded::down` takes presses and repeats and drops
  releases alone. And the keys with no byte behind them go through `Special`,
  matched against `PICK_KEYS` and `VIEW_KEYS` with the extra parameters read off
  and dropped; Shift-Tab is answered by both lists directly, being a chord that
  means the *opposite* of the byte behind it.
- **A mode the client is holding is left by a keystroke and by nothing else.** A
  session that asked for mouse tracking or focus reporting has the terminal
  sending reports whenever the hand or the window moves, and those arrive on the
  same stdin the keys do. Read a byte at a time, the Esc in front of one is the
  Esc key: moving the mouse dropped control mode, closed the view, and typed the
  rest of the report into the session behind it. So control, scroll and both
  prompts take an escape sequence off whole before any byte of it is read.
  `attach::SHIFT_TAB` is the one exception, being spelt like a report and
  pressed like a key. Focus mode is not in this at all: there the reports are
  the session's and go through untouched.

**Things that must never be done**

- **A tab completion never starts a node, never installs anything, and never
  waits on ssh unless the word already names a machine** (`src/complete.rs`).
  All three are tested.
- **`mm agent` must leave stdout strictly alone**: the protocol is on it. Only
  the daemon opens a log file; everything else logs to stderr.
- **A group is spelled `@name`, and no bare word is ever guessed at.**
  `gpu-box/pi` cannot say whether `pi` is a session or a group, and trying one
  then the other means making a group named after a session silently changes
  where a command you have typed for weeks goes. `target.rs` has ruled against
  that twice, and `mm kill @pi` is refused for the same reason a bare machine
  name is only accepted for going somewhere.
- **`--keep-sessions` cannot be typed inside a session it is about to end.** The
  restart hangs every session up, and the client is in the process group of
  whichever one it was typed in, so it is killed partway through: the checkpoint
  is written, the node goes, and the restore never runs. Reproduced before it
  was guarded. `ran_from_a_session_here` refuses it and says where to run it
  instead, comparing `getsid` of this process against the listing's pids rather
  than reading `MM_SESSION`, which is stamped in once at spawn and cannot be
  reached by a rename.
- **A checkpoint that does not cover everything does not authorise a restart.**
  `--keep-sessions` answers the question `agreed` asks rather than overriding
  it: the sessions are not being lost, they are being written down. So a save
  that could not account for every session stops the restart instead of
  proceeding on a record with holes in it, and `--force` stays the only way to
  end sessions on purpose. A session whose directory could not be read is left
  out of the file rather than written without one: put back in `$HOME`, a
  program told to resume picks up somebody else's work.
- **Every lock is a `std` one, taken through `lock::held`, and never held across
  an await.** The three go together. `std` is right because every critical
  section here is short and synchronous, and because `Attachment::drop` takes
  one, where a `tokio` mutex could not. What makes that safe is
  `clippy::await_holding_lock`, denied in `Cargo.toml` so a local run says so
  and not only CI. `held` drops poisoning: a panic under a lock is one operation
  going wrong, while poisoning makes every later one panic too.

### Tests

`tests/remote.rs` builds a temporary world: two nodes with their own
`MM_CONFIG_DIR` and `--socket`, and a shell script on `MM_SSH` that stands in
for ssh by running the other node's agent directly. That covers CLI, ssh
invocation, agent and remote node without an sshd. `tests/persistence.rs` drives
`Node::handle` over an in-memory duplex, and uses tokio's `start_paused` to skip
the client-liveness deadline. The shim has suites of its own under
`android/rust/tests`, which a root `cargo test` does not reach.

## The Android app

`android/` is a second crate and a Gradle project: `android/rust` is a `cdylib`
shim linking `manymux` with `default-features = false`, and `android/app` is
the app around it. `android/README.md` says what it does, and the rules for the
parts that break quietly are in `android/CLAUDE.md`, which loads when you work
in there. Two of them reach back into this half of the tree and are worth
knowing from here: anything the desktop would also want belongs one directory
up (`src/client/scroll.rs` is the worked example, shared by both surfaces), and
the shim links the library without the `desktop` feature, which is why the
`--no-default-features` check above is not optional.

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
