# CLAUDE.md — the Android app

Loaded when working under `android/`. The repo-wide rules are in the root
`CLAUDE.md`; these are the ones specific to the app and the shim.

`android/` is a second crate and a Gradle project: `android/rust` is a `cdylib`
shim linking `manymux` with `default-features = false`, and `android/app` is
the app around it. `android/README.md` says what it does; these are the parts
that break quietly.

- **The shim is two things and nothing else**: an ssh connection made in this
  process (`ssh.rs`, `machine.rs`, `keys.rs`), and the session's screen
  emulated on this side of the wire (`screen.rs`, `scroll.rs`). Everything
  between them is the library's, linked in rather than reimplemented: the
  framing, the attach, the liveness deadline, the reconnect ladder and the
  scrollback arithmetic. Anything the desktop would also want belongs one
  directory up, and `src/client/scroll.rs` is the worked example, shared by
  both surfaces because it is not feature gated.
- **It is outside the workspace and keeps a lockfile of its own.** The empty
  `[workspace]` table in `android/rust/Cargo.toml` keeps cargo's search from
  walking up if the root ever grows one, and the separate lockfile is the
  substantive half: russh and uniffi are a couple of hundred crates `mm` never
  compiles, and one shared lockfile would have every `--locked` build of the
  binary, the four cross targets in CI included, resolving against entries
  that churn whenever russh bumps. What it costs is that root `cargo test` and
  `cargo clippy --all-targets` reach none of it, which is what `just
  everything` is for, and that the two clippy denials (`await_holding_lock`,
  `await_holding_invalid_type`) are repeated in the shim's manifest rather
  than inherited.
- **Climbing the ladder is the shim's job.** `Stream::from_halves` leaves a
  stream with `ssh: None`, so the 127 retry built into `Stream::request` never
  fires and `client::PROGRAMS` has to be walked here (`ssh::Reached`). It is
  climbed with `Request::List`, which a session list wants answered anyway, so
  looking for `mm` costs no round trip of its own. And a stream is worth one
  request: `Node::handle` answers a single one and closes, so the rung that
  answered is what is kept, not the connection.
- **The pong is answered on the read loop and nothing may get in front of it.**
  `proto::SILENT_FOR` is an absolute deadline, not a per-call timeout, and an
  app in somebody's pocket draws nothing for hours: a client that waited to be
  drawn before saying it was alive would be detached by the host for being in
  a pocket. So `session::pump` feeds the screen on a task of its own over a
  bounded channel, and a burst of output can never put `writer.pong()` behind
  a queue.
- **Kotlin never awaits Rust.** `ffi.rs` exports methods that take a lock for
  as long as it takes to copy some rows, or drop a message into a queue;
  everything that waits on a machine happens on tasks belonging to the runtime
  `Phone` owns. The two that genuinely wait, opening a connection and asking
  what is running, block and are called off the main thread. `take_frame` and
  `take_window` are the hot path, called once a frame and cheap when nothing
  changed: output arriving faster than the app draws coalesces in `avt`, which
  is the backpressure, bytes piling into a grid of a fixed size rather than
  into a queue.
- **`scrollback_limit` is zero on both surfaces** (`screen::BEHIND`). The node
  holds the history and hands over a window of it on `tag::VIEW`, so a phone
  left attached for a week to a session that prints cannot be grown out of
  memory. It is also why the view is a request rather than a buffer, and why
  the app says so where the gesture was made when a host is too old to answer
  one.
- **A drag belongs to the session while the session is reading the mouse, and
  this end has to encode the notch itself** (`mouse::Tracking`,
  `Session::drag`). The rule is the desktop's, `wheel_is_ours` being
  `history && !session_mouse`: two readers on one wheel is one of them reading
  input meant for the other, and a full-screen program draws its own scrolling
  from exactly these reports. What differs is that the desktop never has to
  spell one, its terminal doing that for it, so the two things a terminal knows
  are read out of the session's own output here: whether it asked for reports
  (9, 1000, 1002, 1003) and in which spelling (1006, 1015, else the one-byte
  form). Both arrive on an attach as well as during one, `events::REPLAYED_MODES`
  replaying them with the screen, which is why `Screen::repaint` resets the
  tracking along with the emulator. Getting this wrong does not look like a
  wheel going to the wrong place, it looks like scrolling being broken: the
  alternate screen has no scrollback at the node either (`avt` gives that
  buffer a limit of zero), so a drag inside a full-screen program opened the
  view over a buffer with nothing behind it and moved through the screen it was
  already showing. And `Session::drag` answers with which of the two happened,
  because the third answer is the one worth having: a host too old for the view
  holding a session that is not reading the mouse is a gesture with nowhere to
  go, and it says so where it was made.
- **A peek makes no client, which is the whole reason it exists**
  (`Request::Peek`, `Session::peek`, `Registry::peek`). The wall of tiles on
  the app's main screen needs every session's screen at once, and the screen
  itself is nothing new: it is the dump `Response::Attached` already carries.
  What is new is asking for one without becoming a client. Attaching read-only
  would have answered it and is wrong twice over: a viewer counts in
  `host_clients`, so drawing the wall would tell the machine somebody is
  sitting at it and stop a bell reaching the desktop, and it is one attach per
  tile for one picture each. So nothing is added to `clients` and the geometry
  is left alone, tested in `tests/persistence.rs`. Two things ride on it. The
  `Size` travels with the screen, because a dump paints by absolute position
  and never reflows: a caller that guessed the shape would be taking the
  picture apart rather than making it smaller, which is why `SnapshotView`
  scales the session's grid instead of asking for one its own shape. And it is
  `vt.dump()` alone rather than `repaint()`, the modes in the second half
  being for a terminal about to be typed into: a caller looking at a picture
  has no business turning on mouse reporting for a session it is not attached
  to.
- **A repaint is a fresh screen, and only the first output after an attach is
  one.** `avt`'s dump emits no clear and no home, so `Screen::repaint` builds a
  new `Vt` and resets the UTF-8 decoder where `Screen::feed` does not, and the
  three places one arrives are the attach, an `Update::Screen` answering a
  resync, and a reconnect. The size it is built at is `Attached.size`, the size
  the session settled on, never the size that was asked for: the node takes the
  smallest across every attached client, and reflowing this end to what was
  asked paints a screen the session never had.
- **The bindings are generated from the compiled library, not from the
  source, and are not checked in.** `android/build-rust.sh` builds both ABIs
  and then runs `uniffi-bindgen` against a host build of the same crate, so
  what the app calls and what the app links cannot disagree. The Gradle task
  declares `../src` and the root `Cargo.toml` as inputs too, or a change to
  the client core leaves the task up to date and packages the library from
  before it.
- **The app carries addresses and keys, which the rest of the project refuses
  to.** `src/hosts.rs` keeps none anywhere because `~/.ssh/config` and sshd
  already hold them; a phone has neither, so `keys.rs` holds one generated
  identity and a note of each machine's host key, trusted on first use. The
  departure stops at the app and nothing about it reaches the wire or the
  library. `agent.rs` is for the `reach` example on a desktop only: nothing on
  Android sets `SSH_AUTH_SOCK`, and the agent is passed in rather than looked
  up so a test run cannot pass or fail by what somebody had loaded that
  morning.
- **The terminal is a plain `View` with `onDraw`, not Compose.** The grid has
  no composable structure and changes sixty times a second, and
  `Canvas.drawText` on a hardware-accelerated view goes through the platform's
  glyph cache. A row crosses as runs rather than cells, and the widget advances
  by `Run::cells` and never by the font's own idea of the text's width, which
  is what keeps one CJK character from putting the rest of the row out of step
  with the grid.
- **The shim's tests stand the real thing up in the process.** `tests/` there
  runs a russh server and an ssh agent rather than doubles, because what is
  under test is the transport, and `src/bin/stub-agent.rs` is a real process
  standing in for `mm agent` because the ladder reads a real shell's exit
  status. What it does is chosen by environment variables (`STUB_FAILS`,
  `STUB_SCRIPT`), the far end of an ssh being reached through a shell and
  there being nowhere else to put it.

