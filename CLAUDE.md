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
- **What is signed is the checksum sidecar, and the key lives in three places
  that have to agree.** Ed25519 over the sidecar's exact bytes, published hex
  encoded as `<asset>.sha256.sig`: the signature says the sidecar is ours and
  the sidecar says which binary it covers, so both are checked or neither
  counts. The sidecar rather than the binary because it is 65 bytes, which
  takes the prehashing question off the table entirely. Verified in process
  (`src/signature.rs`) rather than by shelling out the way the rest of `update`
  does, because there is nothing to shell out to: minisign is on almost no
  machines, and the `openssl` macOS ships is a LibreSSL without `-rawin`. The
  three places are `signature::RELEASE_KEY`, `RELEASE_KEY` in `install.sh`, and
  the `RELEASE_SIGNING_KEY` secret the release workflow signs with; the first
  two are the same public key and the third is its private half. An empty key
  means no checking rather than refusing everything, or a build with no key
  configured would be one nobody could update away from. An unsigned release is
  a warning for the same reason, and that arm of the match in `update::apply`
  becomes a `bail!` once the newest release on every channel carries a
  signature.
- **Watching is enforced at the node, and answered for.** `mm view` is an
  attach with `read_only`, and what makes it worth pointing at a session
  somebody else is working in is that `Attachment::send_input` drops the bytes
  rather than the client agreeing not to send them. Two consequences that are
  choices rather than accidents. A viewer is left out of `State::clients`, so
  somebody looking on from a phone cannot reflow the screen of whoever is
  typing, and it is shown the geometry the session is already at. But it does
  count in `host_clients`, the opposite way, because a person watching is a
  person present and a bell should reach the terminal they are sitting at.
  The answering half is the rule that shapes it: `read_only` is a defaulted
  field, so a node too old to know it decodes the request without it and hands
  back an ordinary attach with a live keyboard. That is why
  `Response::Attached` carries a `read_only` flag and why the client refuses a
  host that does not set it, rather than treating it as a key that does nothing
  the way `paste`, `scroll` and `rename` are treated. A promise nobody made
  must not be assumed.
- **A connection that drops is waited out, not reported.** The session is still
  running on a machine that never noticed the client left, so putting somebody
  back at their shell over a wifi hop throws away the one thing the project is
  for. `do_attach` keeps `held`, so the terminal stays in raw mode showing the
  screen as the session last painted it, and only the mark row changes
  (`terminal::waiting`). `attach::reconnect_after` is short at first and then
  flat at ten seconds, and it never runs out: the first failures are a lid or a
  network hop, the rest are somebody who walked away, and a session that
  outlives the connection is the one thing the project is for, so the clock has
  nothing to decide. Three things this needs. The wait reads the keyboard, or
  it would be a wait nobody could leave, and it is now the *only* way out: the
  mode key's detach gets out, and so does Ctrl-C, which has nowhere else to go
  while there is no session to send it to. The mark row says so, which is why
  it no longer repeats the target sitting two columns to its right. And a
  failure to reattach is another lost attempt rather than an error, once this
  run has been attached to anything at all (`attached` in `do_attach`); only
  the first attach can fail outright, because that one is a command that did
  not work rather than a connection that went.
- **The row is the only thing that moves while a connection is gone, so it has
  to.** Everything above it is the session as it was painted before the drop,
  which is the point; a row written once and left there is a client that has
  died as far as anybody watching can tell. So `terminal::waiting` counts the
  delay down a second at a time and says `reconnecting` while an attempt is
  out (`status::waiting_notice`), and the attempt is the half worth naming:
  reaching a machine that is off takes as long as ssh takes to give up on it,
  which is most of the time spent here. It says how long ago the connection
  went as well as how long until the next try, because after the first few
  tries the delay is flat and says nothing about how long you have been away.
  `main::Lost` holds both, being older than one delay. And the dot goes hollow,
  since the one thing it means is who has the keyboard and while this is up
  nobody does.
- **A wait hands the keyboard back to the session, and it is the only thing
  that does.** Control mode survives a reattach on purpose: a hop sets it so
  the key after one carries on walking (`mode` in `do_attach`). Nothing else
  turns it off, so a reconnect that kept it came back with the client holding
  the keyboard, where `Ctrl-]` *leaves* control mode and the `tab` behind it is
  a tab into somebody's shell: after one hop and one closed lid, the session
  next door was unreachable by the gesture that reaches it for the rest of the
  run. `wait_to_reconnect` clears it, which covers both ways in, and the reason
  is that a wait is not a hop: nobody pressed anything, and the row has been
  saying the connection went rather than what the keys do.
- **What a reconnect goes back to is the session you were in, and telling that
  from a stale listing is what `main::Missed` is for.** The loop reattaches to
  `cycle.current()`, which a hop has already moved, so a drop a moment after
  `Ctrl-] tab` waits for the session hopped to rather than walking back to the
  one the command line named. That needs the two ways an attach can fail kept
  apart, since they mean opposite things: `open()` failing is a machine that
  never answered and is waited for, while an error out of `Stream::attach` is a
  node that answered and has no such session, which is a listing gone stale
  under the switch keys and the one case that forgets the entry and undoes the
  hop. Reading every failure the second way, as this once did, meant a closed
  lid mid-hop dropped a live session from the cycle and ended the attach.
- **A session that ends hands back the one you came from, and only the session
  you named ends the attach.** `Cycle::fall_back` is the whole decision: a run
  that has hopped has somewhere to go back to and goes there, while a run that
  has not is `mm attach host/name` doing what it was asked, so it prints the
  line and leaves with the status. Which is why the status is worth keeping:
  `mm attach box/build; echo $?` is a thing people write, and a client that
  landed them in whatever else was running would have thrown it away. The exit
  is *carried* while the fall-back is attempted (`ended` in `do_attach`),
  because that attach can fail too: the session you came from may have exited
  in the background while you were away from it, and answering that with the
  wait a dropped connection gets would sit forever on a session nobody is
  running. So a `Missed::Gone` while one is carried reports the exit instead of
  waiting, which also makes the fall-back safe to attempt without asking the
  listing first, where a stale answer would be wrong in both directions. What
  is left behind is left behind in `Motion::Last` as much as in the listing, or
  the key for the session you came from would reach one that has ended. And the
  row says what ended, because the line that says it on the way out belongs to
  a run that is over, and a screen that changes under somebody with nothing
  said about it reads as a client that lost its place.
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
- **A group lives in the client, and is keyed on the pid and the start time.**
  A node runs the build it started from until `mm restart`, and for a host
  reached over ssh nothing says it is stale, so a group defined at the node
  would work on some machines and silently not on others with no way to see
  which. Defined in `groups.toml` it works the moment one binary is replaced,
  against a node of any age: nothing about this touches the wire, which is the
  acceptance test. What it costs is that a group made at your desk is not one
  your phone sees, and that is the trade rather than an oversight. Membership is
  `(host, pid, started)` and never the name, because a name is the one thing
  about a session that moves: keyed by name, a rename would drop a session out
  of its group, and one typed on another machine would not even be seen.
  `started` is there for the one case the pid cannot answer, a machine that
  rebooted and reused the number. Pruning considers only the hosts that
  *answered* (`Listing::reached`), or a machine that is asleep loses its
  sessions out of their groups while you are away from it. A group is a set of
  live sessions and nothing else, so the last one ending takes the group with
  it and `Cycle::refresh` clears a focus that has emptied: left narrowed to
  nothing, every switch key would do nothing with no way to find out why.
- **The popup is what control mode looks like, and the two verbs are told apart
  by which list you came from.** `Ctrl-]` draws the sessions rather than
  changing a mode nothing shows. Keys about a session act on the highlighted
  row (Enter, `r`, `m`), keys about you act on the client (`d`, `n`, `[`, `/`,
  `p`). Enter on a group narrows, `m` then Enter assigns, and no row in either
  list means two things. `m` acting on the highlighted row rather than on the
  session you are attached to is what makes grouping possible without hopping:
  the picker already holds every session's `SessionInfo`, so assigning is a
  local write and a redraw. `r` is the exception that proves the stream rule:
  it may name a row that is *not* the session at the other end of the attach
  stream, and `tag::RENAME` renames that one by design, so it goes back to
  `main` as a `Request::Rename` to that row's machine.
- **Everything but Enter on a session comes back to the popup.** Grouping a
  session, naming one, narrowing to a group: none of them is a gesture that goes
  anywhere, so landing in the session afterwards threw away the list you were
  working from and made a second move two more keys. They all detach and
  reattach, because the write is `main`'s to make and this half of the client
  may not know what a group is, so the way back is `mode`: `Outcome::Chose`
  leaves it at `Mode::Control` and only `Chose::Go` sets it to `Mode::Focus`.
  Which needs the other half of it, since control mode *is* the popup: an attach
  that starts there opens the box itself (`greet` in `pump`), and it has to wait
  for the frame that repaints on attach, a screen dump painting by absolute
  coordinates from the top that would go straight over a box drawn before it.
  The rows it opens on are the caller's, built after the write, so the box shows
  what just happened without asking anything.
- **The session list leads with the groups, and a machine is what is left.**
  A group spans machines, so it is the machines that break up under it: nested
  the other way, the one thing a group is for, seeing a piece of work in one
  place, was the one thing the list would not show. So the groups come first,
  each whole and headed `@name` the way it is typed and the way `mm a @pi`
  spells it, then whatever is in no group under the machine it is on. No
  heading says "no group": that would name the one thing a group is not.
  Inside a group every row carries its machine, this one included, because
  there the machine is what tells two rows apart, and half-qualifying only the
  far ones leaves the eye working out which kind of row it is looking at. The
  A machine goes on a line of its own inside a group rather than in front of
  every name: `host/name` is how a session is addressed and was the obvious
  label, but a real host name is most of the column, and with `dev.box.ray/` in
  front of it there was no room left to tell `rayfish-iroh-dev` from
  `rayfish-iroh-debug`, which is the one thing the row exists to say. So every
  session sits under a machine either way and a group is a level above that.
  Sections are ordered by their first session and sessions by `started`, never
  by name, for the reason every listing here has: a name moves under a rename
  and the rows would shuffle beneath a hand walking them.
- **A group heading and a machine heading are the same thing to the picker.**
  Siblings in the list, skipped the same way, and `h` steps between them
  without caring which it landed on: with the groups leading, a key that walked
  only machines would step over most of the screen. So there is one
  `Row::heading` and the label says which it is. `Row::indent` is left doing
  nothing but drawing, and it is spent out of the label's own column, or a
  deeper row would push the detail and the note along and the box would stop
  reading as columns. The name column takes the widest name in the list between
  `MIN_LABEL` and `MAX_LABEL`, since how much room a name needs depends on how
  deep the tree is that day.
- **A client of ours that owns part of the screen owns all of it, so the
  session stops being painted while one is up.** The view already did this and
  the popup now does too: with the session still painting, a box drawn over it
  was gone within a second, and redrawing the box after every chunk is worse,
  because a line printed *scrolls the screen* and takes the rows of the box
  already drawn up with it, leaving one copy of the box per line. There is no
  third answer. A terminal composes nothing, and the client is not the emulator
  here: the node holds the screen, which is exactly why tmux can float a popup
  over live output and this cannot. What pauses is the picture and never the
  program, since the session runs on and the resync that closing the box asks
  for paints wherever it has got to.
- **What the session said while nobody was painting it is not all in the
  screen, so a dump is painted over a terminal put back to nothing**
  (`terminal::given_back`, written beside `REGROWN`). A screen is cells plus
  the replay `node::events` rides with it, and a replay can only say what the
  program has *on*: a mode it switched **off** while a client surface was up
  was dropped with the rest of that output and has no other way of ever
  reaching the terminal. It showed up as the thing that mode is for. A program
  that pops the kitty keyboard protocol on its way out popped it under an open
  popup, so the terminal went on reporting key releases at the shell that
  followed it, and every keystroke typed `[103;1:3u` into whatever was running.
  So the set the replay answers for is switched off first, which is the same
  set a hop undoes and the reason the two are one function: neither may grow a
  member the other has not heard of.
- **The cells under the box belong to the box, and it clears its own.** The
  session's screen there was painted over when the popup went up and is put
  back by the resync closing it asks for, so nothing else is going to: a box
  that changed shape, which it does the moment the first real listing lands
  under an open popup, left the top of the old one on the screen framing
  nothing. `Picker::drawn` remembers the last rectangle and only the rows given
  up are blanked, never the ones about to be painted, or every keypress would
  write each cell twice and invite the flicker the view had.
- **What is drawn to the node is asked for and painted as one act**
  (`terminal::ask_for_the_screen`). A dump starts painting wherever the cursor
  is and never erases, so a screen asked for and painted where it fell walks
  its own first rows off the top, one `\r\n` at a time. The erase and home are
  `REGROWN`, spent against `owed`. These were two statements at four call
  sites, and the one place the second was missing showed nothing until a pager
  exited: `less` leaves the cursor at the bottom, so quitting one gave back the
  last few lines of the session under a blank half screen.
- **A window with no room for the box hands the job to the mark row.** Since the
  popup *is* the mode, a terminal too short to draw one (`Picker::draw` answers
  nothing) was a client that had taken the keyboard and a `tab` that changed
  nothing anybody could see. So `Status` has three states rather than two
  (`Popped`): no popup and the row lists the keys, a popup drawn and the row
  says nothing about keys because the box already does, or a popup with nowhere
  to go and the row stands in for it. What it stands in with is the highlight
  first and the keys after, and the keys are what gets cut when the row runs
  out: a list whose keys you have to remember still works, and one you cannot
  see at all does not. The group lists bring their own two keys rather than the
  session ladder, since `d` there is not a detach.
- **`Mode::Picking` is a mode because the session keys must not reach it**, and
  `KeyFilter::after` takes the mode the key arrived in for the same reason: a
  move that answered `Mode::Control` meant the key after one in the group list
  was read with the session table, where `m` moves and `d` detaches. Both lists
  read every key through `Encoded::byte`, and the arrows and Shift-Tab go in
  `PICK_KEYS` and are matched whole, or the Esc each of them starts with is the
  Esc that closes the box.
- **A popup move is the one action you type through.** Everything else ends the
  chunk it was found in, because nobody types through a detach or a switch. Two
  writes a moment apart arrive as one read, so `tab` then `Enter` is a single
  chunk and stopping at the move throws away the key that commits;
  `Keystrokes::rest` hands the remainder back for moves alone and `pump` rounds
  the chunk until it is used up.
- **The popup asks the machines on every press and never waits on them.** It
  opens on the snapshot the switch keys already keep and is corrected when the
  answer lands, swapped in by `Picker::replace`, which holds the highlight on
  the same `Row::id` rather than the same index: a session ending three rows up
  would otherwise move what Enter takes. The row ids are the caller's, which is
  what keeps hosts, groups and pids out of this half of the client, and the ids
  the outcome carries belong to whichever listing was last drawn.
- **A group is spelled `@name`, and no bare word is ever guessed at.**
  `gpu-box/pi` cannot say whether `pi` is a session or a group, and trying one
  then the other means making a group named after a session silently changes
  where a command you have typed for weeks goes. `target.rs` has ruled against
  that twice already, and `mm kill @pi` is refused for the same reason a bare
  machine name is only accepted for going somewhere.
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
- **The wheel is the terminal's until somebody asks for it here**
  (`settings::Mouse`, `mm config mouse client`, reaching `wheel_is_ours` folded
  into `history`). Reading the wheel means asking the terminal to report the
  mouse, and a terminal reporting the mouse is not selecting with it: what is
  taken is not a notch but the bare drag, and it is taken for the whole attach.
  Selection comes back only under a modifier the *terminal* chooses, no two
  agree which, and the one in front of somebody may not offer one at all, which
  is a fact about their terminal that nothing here can see. That asymmetry is
  what settles the default, rather than any weighing of scrolling against
  copying: a wheel that does nothing on the alternate screen is what plain ssh
  and an unconfigured tmux both do, and `Ctrl-] [` is on the hints row saying
  so, while a drag that stopped selecting is this having quietly taken
  something away with nothing to press instead. What the setting must not touch
  is that key, or handing the mouse back would take the history with it.
- **Taken, it is taken for the whole attach and not just while the view is up.**
  That was the first shape and it was wrong, because of where the notch went
  instead: the client's screen *is* the terminal's alternate one, which keeps no
  scrollback of its own, and `Alternate::setup` switches alternate scroll off
  besides (`?1007s` then `?1007l`, restored with `?1007r`) so a notch cannot
  become arrow keys into whatever is reading the session. So a notch reached
  nobody at all until the view had already been opened by a key, which is a
  gesture nobody reaches for second. Now the first notch opens the view and
  moves it, which is what tmux does and what the mode was always shaped for
  (`KeyFilter::after` has said "a wheel notch in focus mode opens it and stays"
  since before one could).
- **A session that asked for the mouse keeps every report, wheel included.**
  That half of the old rule is untouched and is what the whole thing rests on:
  two readers on one wheel is one of them reading input meant for the other, and
  a full-screen program draws its own scrolling from exactly these reports. So
  `wheel_is_ours` is false while `Filter::session_mouse` is true, and inline it
  is false always, because there the terminal has the lines in its own buffer
  and its own wheel is better than anything here. Which is also why `set_wheel`
  and `set_scroll` are a pair: no history to move is no reason to take the
  mouse, so a host too old to answer for a window keeps its wheel and gets a
  sentence on the row when the key is pressed.
- **The view paints row by row and never erases the screen.** An erase and a
  repaint leaves the screen blank for as long as the lines after it take to
  arrive, which at one notch is nothing and at a wheel being spun is a flicker
  per frame. So every row is written over the one before it with a clear to the
  end of the line, which is also what keeps a long line from showing through
  under a shorter one. And a window with no block yet paints *nothing* rather
  than blanking: the session is on the screen, one frame more of it is no lie,
  and blanking to wait is the flicker again with nothing to show for it.
- **The view opens before it knows how much history there is, so the opening
  move cannot be clamped** (`Scrollback::answered`). `total` is zero until the
  host answers, and a move back clamped against zero is a move thrown away. It
  went unnoticed while a key that only opened the view was the way in, since the
  first thing that moved was the second thing you pressed. The wheel opens and
  moves in one gesture, so the lost move became the first notch, and a wheel
  whose first notch does nothing reads as a wheel that does not work. So a move
  made before the first answer stands, the block is asked for around where it
  landed, and `take` does the clamping when the answer says what exists, which
  is one round trip rather than two. `Update::View` asks again after taking one,
  because a window brought back inside a short history can leave the block
  covering somewhere else.
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
- **A key held down is a key still being pressed** (`Encoded::down`). The
  protocols that report event types stop repeating the plain byte and send
  repeats instead, so a client that took only presses was one where holding tab
  walked the list exactly once, and only while a program like `pi` had the
  terminal in that mode: the same hand on the same key worked everywhere else,
  which is the kind of difference nobody thinks to report. Releases are still
  dropped, and that is the half the rule was written for: the ctrl you were
  holding reports its own release the moment you let go of the mode key.
- **The keys with no byte behind them are read the same way, through
  `Special`.** `Encoded` speaks the `u` and `~` spellings and knows nothing of a
  sequence ending in a letter, so the arrows and the paging keys are matched
  against `PICK_KEYS` and `VIEW_KEYS` instead. Matching the plain spelling
  literally, as that once did, missed exactly what `Encoded::down` missed one
  layer up: a held arrow stops arriving as `\x1b[A` and starts arriving as
  `\x1b[1;1:2A`, so holding one walked the popup once and stopped, while holding
  tab beside it worked. So the tables hold the plain spelling alone, `Special`
  reads every longer one back to it, and the parameters an extended mode adds
  are read off and dropped: a release moves nothing, and Ctrl-Up in a list is a
  hand reaching for up. The same protocols respell Shift-Tab as tab with shift,
  which is a key that means the *opposite* of the byte behind it, so both lists
  answer for that chord themselves rather than letting `Encoded::byte` walk them
  forwards.
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
  only where a key was pressed. What does *not* go on the stream is the key
  that starts a session (`Action::New`, `Outcome::New`), which looks like the
  same shape and is not: a rename is done to the session at the other end of
  that stream, while a new one needs a host to start it on, a name back and a
  fresh attach, none of which this half of the client is allowed to know. So it
  is handed back the way a switch is, and `main::start_beside` does the work.
  Note also that replacing the binary is not
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
- **The listing already out there is handed to the popup, not left to finish
  into a variable nobody reads again.** `take_listing` gives one half a second,
  so on a fleet where a fan-out takes longer it is *always* still running when
  the popup opens. The popup used to start a second one from scratch: two ssh
  commands per host for one keypress, and an empty box for as long as the
  second took. Now the pending handle moves into the lister task, whose first
  ask awaits it. Two things follow. The task hands the snapshot back through
  `seen`, because the loop has given up its own copy and would otherwise never
  fill one in again. And the narrowing that happens on the way in
  (`settled`) can no longer wait for a listing that says something: on a slow
  fleet the first one lands after the run has started, and firing then narrowed
  the run to whatever group you had just put a session in.
- **A press is the only thing that asks the machines what they are running, so
  every press has to ask.** Nothing refreshes the switch keys' listing on a
  timer: it is asked for after a key is pressed and read by the next one
  (`spawn_listing`, `take_listing`), which is what keeps a keystroke off a
  machine that is asleep, `take_listing` giving one half a second and then
  going with what it already knows. The trap is the press that lands nowhere.
  Asking only after a landing meant the first fruitless press was the last one
  that ever asked, so a machine with one session on it when the run started
  stayed that way as far as these keys were concerned, whatever was started
  beside it afterwards. The listing is asked for either way.

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
