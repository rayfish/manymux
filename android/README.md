# manymux for Android

A terminal you can work in, with manymux's session switching in the chrome
instead of behind a mode key. On a desktop `Ctrl-]` exists because a terminal
has one keyboard and the session wants every key of it; a phone has chrome the
session does not own, so the key goes straight through and the verbs behind it
become things you press.

Two halves:

- `rust/` is a `cdylib` shim: an ssh connection made in the process, the
  program ladder, and the session's screen emulated on this side of the wire.
  Everything between those is the library one directory up, linked in with
  `default-features = false`.
- `app/` is the Android app: a machine, its sessions, and one of them on the
  screen. Plain views rather than Compose for the terminal itself, since the
  grid has no structure to recompose and `Canvas.drawText` on a
  hardware-accelerated view goes through the platform's own glyph cache.

## What is in this version

One machine at a time. Reach it, list what is running, attach to one, type in
it, resize it, leave it, and survive a connection that drops. The drawer, the
app-bar swipes, groups, notifications and the scrollback view are the next
ones.

The phone is an ordinary writable client: it sends its own size and the session
reflows. The node applies the smallest size across every attached client, so
attaching from the sofa squeezes the session on the desk. That is the trade,
not an oversight.

## The departure this makes from the rest of the project

`src/hosts.rs` keeps no addresses, no keys and no allowlist anywhere, on the
grounds that `~/.ssh/config` and sshd already hold them. A phone has neither, so
the app carries its own: a key it generated in app-private storage, and a note
of the host key each machine presented the first time. It is confined to the
app, and the library keeps its property.

## Building

The shim compiles C, which the root crate does not: `russh` refuses to build
without either `ring` or `aws-lc-rs`, and both are C. `ring` is the one chosen,
because `aws-lc-rs` additionally wants cmake. So the NDK is needed for its
clang, not only for its linker.

```bash
cd rust
cargo test -q                      # host target, no NDK needed
```

The app:

```bash
export ANDROID_HOME=/path/to/android/sdk
./gradlew assembleDebug            # builds the shim for both ABIs first
```

`build-rust.sh` is what Gradle runs. It builds the library for `arm64-v8a` and
`x86_64` and generates the Kotlin that calls it, from the compiled library
rather than from the source: what the app calls and what the app links cannot
disagree.

API 24, matching the root crate's own Android target.

## Running it against a real machine without a phone

A phone is a slow place to find out that a key was refused. The same client
stack runs from a terminal:

```bash
cd rust
cargo run --example reach -- user@host           # what is running there
cargo run --example reach -- user@host build     # attach and print a screen
```

The first run generates this device's key and prints the line to add to that
account's `authorized_keys`. Host keys are trusted on first use and written
down, and a key that changes afterwards is refused with both fingerprints.

A machine reached through a mesh needs none of that. Where the peer is already
identified by the link the connection arrived over, its ssh offers the `none`
method alone and there is no `authorized_keys` in it anywhere, so the client
asks how to log in before it offers anything, the way every `ssh` does.
