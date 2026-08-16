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
  the desktop you are sitting at (`src/notify.rs`, via `osascript` or
  `notify-send`).
- `src/notify.rs` decides what is worth interrupting someone over, for both ways
  out: the desktop tools above, and `escape()`, the OSC 9 an attached client
  hands to the terminal it is sitting in. `src/settings.rs` is `settings.toml`
  beside the host list, which so far holds only `notify`.
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
- **A node started on demand leaves the client's process group and session**
  (`setsid` in `node::start_node`, tested in `tests/detached.rs`). It owns every
  session on the machine from then on, while the client is a command someone
  typed: without the split, a terminal signalling its foreground group (a Ctrl-C
  at the wrong moment, the window closing) reaches the node too and takes every
  session down at once, with no line in the log because the node is killed
  before it can write one. A node started by the service unit is already clear
  of this, which is why the symptom only ever appears on the on-demand path.
- **A node on its way out hangs its sessions up itself, and outlives none of
  them** (`Node::shutdown`, reached from `Request::Stop` and from a SIGTERM
  handler in `main`). Closing the PTY masters would end them anyway, but the
  kernel sends that hangup to the terminal's *foreground* group, which is
  whatever the shell last put there: a shell sitting behind a full-screen
  program is not in it and finds out only by reading EIO, well past writing its
  history. So the group is signalled deliberately, given `GRACE` to go, and what
  is left is killed. The kill is the child alone and never the group, because
  something that survived a hangup sent to it is ignoring hangups on purpose,
  and `nohup` has to keep meaning what it means. `GRACE` also has to stay inside
  the window `node::stop` gives the socket to go quiet, or `mm stop` reports a
  node still listening when it is only still saying goodbye.
- **The daemon gives up on a machine it cannot reach, and a client is what
  starts it again.** `peers::retry_after` is three growing delays and then
  nothing: a retry is an ssh process, a name to resolve and a connect to wait
  out, per watched machine, and a laptop that is asleep or on another network
  stays that way for hours rather than seconds. What replaces the timer is
  `Request::Reached`, sent by any client command that got an answer out of a
  machine (`note_reached`, from `everywhere` and from a remote `mm new`), which
  is the only way the node here can learn the network is different: a client
  reaches another machine over its own ssh and the node never sees it. Two
  things this rests on. A watcher that gave up is a *finished task still in the
  map*, because a task cannot tidy its own entry away without racing whoever is
  replacing it, so `Peers::watched` and `sync` both read `is_finished` and
  nothing anywhere may go back to `contains_key`. And the attempt count resets
  only for a subscription that lasted `peers::STEADY`, or a host that accepts
  ssh and hangs up immediately would be retried every five seconds forever,
  which is the thing this was written to stop.
- **Every lock is a `std` one, taken through `lock::held`, and never held across
  an await.** The three go together. `std` is right because every critical
  section here is short and synchronous, and because `Attachment::drop` takes
  one, where a `tokio` mutex could not: `Drop` cannot await. What makes that
  safe is the second half, which is not a matter of care but of
  `clippy::await_holding_lock`, denied in `Cargo.toml` so a local run says so
  and not only CI. `held` is the third, and it drops poisoning: a panic under a
  lock is one operation going wrong, while poisoning makes every later one panic
  too, so a node whose bookkeeping panicked once would answer nothing about that
  session until somebody restarted it and took the other sessions with it.
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
  never install anything on their own. Climbing the ladder is silent: ssh's
  stderr is piped and held by `client::relay`, thrown away on the rung that
  answered 127 and printed on anything else, because the remote shell's
  `mm: command not found` is the probe working and would otherwise be printed on
  every command that reaches such a machine.
- **`mm agent` must leave stdout strictly alone**: the protocol is on it. Only
  the daemon opens a log file; everything else logs to stderr.
- **A bell goes to exactly one of two places, and the event says which.**
  An attach stream carries this machine's other sessions' events unasked
  (`pump_attachment`), so whoever is sitting in one session hears the one next
  door on the terminal in front of them; over ssh that is the only route that
  reaches a person at all. `SessionEvent::host_attached` counts the clients
  attached anywhere on the machine, and a desktop notifier that sees one keeps
  quiet (`notify::for_desktop`), or one bell interrupts twice. The escape is
  written only at a `Filter::at_boundary()` moment, like the mark, and the text
  is scrubbed first: it comes from the program in the session, and a BEL in a
  title would end the sequence and leave the rest to be read as commands.
- **A relayed bell is an OSC 9 *and* a bell** (`notify::escape`). OSC 9 is seen
  and not heard: a terminal that has it shows a banner and never rings or marks
  the tab, and one that does not throws the whole thing away, so a bell relayed
  as a notification alone rang nowhere. Which is why the OSC ends with ST rather
  than the BEL it once did: a terminator is eaten by the parser and can never be
  a bell, so the sequence is closed the other way and the bell that follows it
  lands on solid ground. The session on the screen is not relayed at all; its
  own BEL is already in the stream.
- **Modes switched on for a session must be switched off on detach.**
  `events::REPLAYED_MODES` and the teardown in `client::attach` are a pair, and
  so are `events::Keyboard` and the pops in the same teardown. A hop counts as a
  detach for the session being left, so the same undoing happens per attach
  (`terminal::takeover`), and the screen is cleared in the same breath: `avt`'s
  dump paints from the cursor down to its last line with anything on it and
  never erases, so without it the session you left shows through below that line
  and beside a narrower screen. What a hop must *not* undo is the pushed title,
  which belongs to the whole run of attaches, nor the screen, which belongs to
  whichever mode is in use.
- **A dump is both buffers, so a swallowed switch owes an erase**
  (`status::SWITCHED`). `avt` paints the primary screen, names the switch to the
  alternate one, and paints that. On a screen the client owns the switch is
  swallowed, and swallowing it alone left the two painted on one surface: a
  session sitting in a full-screen program showed the scrollback of the shell
  that started it wherever the program had not painted, which no takeover can
  reach because it runs before the dump arrives. The erase goes where the switch
  was, before the next byte that paints, and it is *owed* rather than written on
  the spot because two switches with nothing painted between them are a round
  trip that owes nothing: a dump of a session on the primary screen that has
  used the alternate one names both and paints between neither, and erasing
  there blanks the screen the same dump just painted.
- **There are two screen modes, and everything that differs between them is in
  one trait** (`src/client/screen.rs`, chosen with `--screen` or the `screen`
  setting, alternate by default). `alternate` takes the terminal's second screen
  buffer for the run of attaches and erases it per attach. `inline` takes no
  screen at all: the session paints on the terminal's own, so a hop rolls the
  session you left into the terminal's scrollback instead of erasing it, because
  a line erased is a line you cannot scroll back to, and the node sends the
  lines behind the screen (`node::history`, `tag::HISTORY`) so a terminal you
  just walked up to has something to scroll. The order per attach is history,
  roll, repaint, and getting it wrong paints over the lines it was meant to
  save.
- **The alternate screen has a view of its own instead** (`src/client/scroll.rs`,
  `tag::VIEW`, `tag::FIND`). It is not tmux's copy mode and must not become one:
  no selection and no yank, because the terminal's own selection works on what
  the view is showing, on the modifier every terminal keeps for it while the
  wheel is being reported. Lines come a few screenfuls at a time and matches
  all at once, both for the same reason: a wheel notch or an `n` on a machine
  two hops away must not be a round trip. Whether the host can do either rides
  on `Response::Attached { scroll }`, and a host that cannot says so on the mark
  row rather than leaving a key that does nothing.
- **The mouse is the terminal's except while that view is up**
  (`attach::wheel_is_ours`). A terminal reporting the mouse to us is a terminal
  not selecting with it, so a client that held tracking for the whole attach was
  a client you could not copy a line out of without a modifier held down. The
  reports are worth that only where the wheel is the gesture, which is the view,
  and the view is where the key opens it: the wheel cannot open what it is not
  being reported for. Two things follow. `Alternate::setup` switches alternate
  scroll off (`?1007s` then `?1007l`, restored with `?1007r`), because with
  nobody reporting, a notch on the terminal's alternate screen becomes arrow
  keys and they land in whatever is reading the session's input. And the key has
  to be on the hints row, since the gesture that used to find it is gone. A
  session that asked for the mouse itself keeps every report as before: taking
  it would leave two readers on one wheel, and giving it back on the way out of
  the view would switch off tracking the client never switched on.
- **The wheel is the terminal's to route, not ours.** A program that asked for
  mouse tracking gets SGR reports, one on its own alternate screen without
  tracking gets the terminal's alternate-scroll arrows, and ordinary output
  leaves the wheel scrolling the terminal's own scrollback. All three are right
  only while the session's screen switches and mouse modes reach the terminal,
  which is what inline allows and what `Filter::owns_the_screen` decides.
  Nothing in the client reads a wheel event and nothing should: that way lies
  tmux's copy mode, and between `avt` on the node and the terminal's own
  scrollback there is nothing left for it to do.
- **A resize is repainted from the node, not left to the session.** Telling the
  node the new size redraws nothing: a shell that printed and went quiet has no
  answer to a SIGWINCH, so the screen keeps the old geometry, marks on rows that
  are no longer the bottom included. The client asks for the screen back
  (`SessionWriter::resync`) and paints it over an erased, homed screen
  (`terminal::REGROWN`), for the same reason a hop erases. The scrolling region
  goes out first: the dump paints with newlines, and they would scroll against
  the old fence.
- **The mode key has three spellings, not one.** A program that asks for the
  kitty keyboard protocol (`CSI > 7 u`, which `pi` sends on startup) or for
  xterm's `modifyOtherKeys` changes how the *terminal* encodes every chord, so
  Ctrl-] stops arriving as 0x1d and starts arriving as `CSI 93 ; 5 u`. Watching
  for the byte alone means a session running one of those programs cannot be
  left. `attach::Encoded` reads all three, and drops the repeats, releases and
  bare modifier keys that the same protocols add, since in control mode letting
  go of ctrl would otherwise read as a keystroke.
- **And so does every other key the client reads.** Each mode has one table
  (`KeyFilter::controlling`, `KeyFilter::scrolling`) taking the byte the
  ordinary encoding would have sent, and `Encoded::byte` reads a key back to
  that byte, so the long spelling and the short one cannot answer differently.
  A key read in only one of them is a key that stops working while a program
  like `pi` is running, and the ways that shows up are not obvious: an Esc
  spelt `CSI 27 u` left the mode without leaving the view, so the client sat
  painting history while everything typed went to the session behind it. Under
  *report all keys as escape codes* even the letters arrive that way, and the
  shifted ones arrive as the alternate the terminal reports beside the key,
  which is why `Encoded::text` reads case out of the alternate rather than the
  code.
- **A mode the client is holding is left by a keystroke and by nothing else.**
  A session that asked for mouse tracking or focus reporting has the terminal
  sending reports whenever the hand or the window moves, and those arrive on the
  same stdin the keys do. Read a byte at a time the Esc in front of one is the
  Esc key: moving the mouse dropped control mode, closed the view, and the rest
  of the report was typed into the session behind it. So control and scroll take
  an escape sequence off whole before any byte of it is read and drop the ones
  that are not keys, the same as a prompt does. `attach::SHIFT_TAB` is the one
  exception, being spelt like a report and pressed like a key. Focus mode is not
  in this at all: there the reports are the session's, and they go through
  untouched.
- **What an attached client asks for goes on the attach stream, never down a
  second connection.** An `Attached` holds neither the socket nor the host it
  arrived by, on purpose: that is the one thing this half of the client is kept
  from knowing, and a mobile app drives the same type. So `tag::RENAME` carries
  a name the way `tag::VIEW` and `tag::FIND` carry their questions, and the node
  has the session right there at the other end. What that costs is an answer: a
  node too old to know the tag skips it in silence, so `Response::Attached`
  carries a flag per capability (`paste`, `scroll`, `rename`, `events`) and the
  client says "this host is too old" rather than leaving a key that does
  nothing. `events` is the odd one out and the exception that shapes the rule:
  it has no key behind it, so the only place to say it was the mark row on every
  attach to such a host, which is a sentence about a bell that has not rung and
  may never ring. Nothing reads it now, and it is still answered because a
  client from the build that did read it is still out there: a node that stopped
  saying so would have it call every new host old. A sentence is worth writing
  only where a key was pressed. Note also that replacing the binary is not
  enough for any of these: the node keeps running the build it started from
  until `mm restart`, which `update::is_stale` says for this machine and nothing
  says for a host you reach over ssh.
- **Both prompts are one prompt** (`attach::Prompt`). The search and the rename
  are typed the same way, so the editing lives in one place and only the action
  handed back says which one is open. A prompt swallows the whole chunk, because
  typing arrives in chunks and one action per byte would drop the rest of each;
  and a rub takes a whole character, not a byte, or one press of an accented key
  leaves half of it behind. What it swallows is text, so an escape sequence is
  taken off whole before any of it is read: the Esc a sequence starts with is
  not the Esc key, and with a program holding the terminal in an extended-keys
  mode that is the common case, since letting go of the ctrl that opened the
  prompt reports itself a keystroke later and used to close the prompt again.
  The other half of that is `Encoded::typed`: the same mode spells Esc, Enter
  and Backspace the long way, and they are read back to the byte the editing is
  written against rather than handled twice. Esc is the only way out: a rub with
  nothing left to rub out does nothing, because rubbing a line out to start it
  again is how a name gets retyped and closing the prompt on the last backspace
  threw the gesture away halfway through.
- **A rename moves the name, and the title stays the program's.** The name says
  which session this is and goes in `host/name`, so it is sanitised in
  `node::registry` and refused when another session already holds it: a spawn
  has nobody to tell and takes the next free counter, but a rename was typed at
  a prompt by somebody who would rather hear "that name is taken" than land on
  `build-2`. The title is the last one the program set, else the command, and
  nothing else ever writes it. Which is why `Registry::rename` answers with the
  name that stuck rather than `Ok(())`: what was typed and what the session
  ended up called are not always the same string, and the client is drawing one
  of them on the mark row.
- **A session's name can change under whatever is holding it.** `Session::name`
  is behind a lock and read through `name()`, because the registry's key moves
  with a rename and so does every event the session publishes. Anything that
  remembers a name instead of asking is wrong by the next rename: the exit
  watcher in `registry::spawn` prunes by what has exited rather than removing
  the name it was spawned under, which by then may be a *different*, live
  session, and `pump_attachment` compares an event against `attachment.name()`
  so a client does not hear its own bells as the session next door's.
- **Every listing is ordered by when each session was opened, oldest first,
  and never by name.** `SessionInfo::started` is stamped once in
  `Session::spawn` and nothing writes it again, which is the whole reason it is
  the sort key: a name moves, so ordering by one shuffled the switch keys'
  cycle under whoever was tabbing through it, and the key that reached the
  session next door started reaching a different one as soon as anything was
  renamed. The three places that sort are `Registry::list`, `everywhere` in
  `main.rs`, and nothing in `client::switch`: a `Located` is an address rather
  than a session, so `Cycle::refresh` takes the listing in the order it came in
  and `Cycle::renamed` corrects that snapshot in place rather than waiting for
  a refresh that only a hop asks for. The field is `#[serde(default = "epoch")]`
  because `SystemTime` has no `Default`, and a host too old to send it ties all
  of its sessions at the epoch, where the name tiebreak leaves them in the order
  they have always been in.

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
