# tiles

Persistent terminal sessions you can leave and come back to, on every machine
you already ssh into.

Start something long-running on a remote box, close the laptop, and pick it up
later from a different network. The session keeps running because the machine it
is on owns the terminal, not your connection.

```bash
tiles new gpu-box claude      # start a session there and attach
                              # Ctrl-\ d to detach; it keeps running
tiles ls                      # see what is running, everywhere
tiles attach api              # pick up where you left off, wherever that was
```

`gpu-box` is an ssh destination and nothing else. There is nothing to pair, no
key to exchange, and no host to register first: if you can `ssh` it, you can
`tiles` it.

Install it here, and on each machine you want to manage:

```bash
curl -fsSL https://raw.githubusercontent.com/rayfish/tiles/master/install.sh | sh
```

Status: early, but the whole loop works: sessions that outlive the connection,
one cross-machine view, and bells that reach the machine you are sitting at.

## Why not tmux

Tmux keeps the *session* alive, which is most of the job. But it is
server-local: `tmux ls` only lists the machine you already logged into, so with
five boxes you have five separate worlds and no way to see them at once. And
when a task finishes on a machine you are not looking at, nothing tells you.

`tiles` keeps the sessions and adds the two parts tmux has no view of: one
listing across every machine, and events (a bell, a finished build, an agent
asking for a decision) reaching the machine you are actually at.

## How it works

```
laptop                                          gpu-box
tiles ls/attach ─► ssh gpu-box tiles agent ──► tiles daemon (unix socket)
                   ▲                            │
                   your ssh config:             ├─ session "api"   → PTY → claude
                   tailscale, jump hosts,       └─ session "build" → PTY → cargo watch
                   ProxyCommand, whatever
```

- **A "node" is one `tiles daemon` process.** It owns that machine's sessions
  and listens on a Unix socket; that is the whole of it. Every machine runs one,
  including yours, and it starts itself the first time something needs it.

- **tiles does no networking.** You already have a way to reach your machines,
  and it lives in `~/.ssh/config` where it belongs. A host is whatever ssh means
  by that name, so `gpu-box`, `dario@gpu-box`, a `ProxyJump` alias and a
  tailscale name all work the same.

- **No second ACL.** sshd decides who gets in, using whatever the admin already
  configured: keys, an SSH CA, `AuthorizedKeysCommand`, PAM. tiles has no
  allowlist, no invites and no keys of its own, so there is nothing to keep in
  sync with the real policy and no way for tiles to grant access ssh would
  refuse.

- **That node owns the PTY.** A client leaving is invisible to the child: no
  SIGHUP, no EOF on stdin. That is the whole trick, and it is why a session
  outlives the ssh connection that started it.

- **One node per user, not per machine.** `ssh deploy@box` lands in deploy's
  node and `ssh alice@box` in alice's, because each socket sits in its owner's
  runtime directory. So two people on one box get separate sessions, and tiles
  never has to switch users, drop privileges, or run as root: ssh already put
  the request in the right account.

- **Nothing to install on the far side but the binary.** The node starts on
  demand the first time something asks for it, the way tmux starts its server.

- **Connections are shared.** ssh's `ControlMaster` keeps one connection per
  destination for five minutes, so the first command pays for the handshake and
  the rest are a round trip.

- **Sessions log in properly.** The shell comes from the passwd database and
  starts as a login shell, and commands run through it, so a session has your
  `PATH` and your environment even when the node was started by a service.

- **Reattaching repaints.** The node keeps a headless terminal emulator
  ([`avt`](https://github.com/asciinema/avt)) fed by every output byte, so
  attaching replays the *screen*, not the byte log. Full-screen programs like
  htop or vim come back intact rather than as scrambled scrollback.

## Getting set up

```bash
curl -fsSL https://raw.githubusercontent.com/rayfish/tiles/master/install.sh | sh
```

Linux and macOS, x86_64 and aarch64. The installer picks the right binary,
falls back to the static musl build where glibc is too old or absent, verifies
the checksum, and only reaches for `sudo` if the target directory needs it.
`INSTALL_DIR` changes where it goes (default `/usr/local/bin`), `TILES_VERSION`
pins a release.

Then run the same line on each machine you want to manage. That is the entire
remote setup: no service to install, no pairing, no keys.

```bash
tiles new gpu-box
```

`tiles` has to be on `PATH` for a *non-interactive* ssh, which reads neither
`.zshrc` nor `.bashrc`. `/usr/local/bin` normally is; `~/.local/bin` often is
not. Building it yourself instead is `cargo build --release` and copying
`target/release/tiles` over.

The first machine you use gets remembered, so `tiles ls` covers it from then on
and its bells reach you. `tiles add <host>` does the same without starting a
session, for a machine you only want to watch; `tiles rm` forgets one.

Run the node as a service so it survives logging out, and so it keeps watching
for events:

```bash
tiles service install
```

The unit is written for whatever the machine actually runs:

| Manager | Where | Root? |
|---|---|---|
| launchd (macOS) | `~/Library/LaunchAgents` | no |
| systemd | `~/.config/systemd/user` | no |
| OpenRC (Alpine, Gentoo) | `/etc/init.d` | yes |
| SysV init (Devuan, MX) | `/etc/init.d` | yes |
| rc.d (FreeBSD) | `/usr/local/etc/rc.d` | yes |

launchd and systemd have per-user services, so sessions run as you and
installing needs no root. The other three do not, so the unit is a system
service that drops to your account before running, and installing it needs
`sudo`. Under systemd, `loginctl enable-linger` is what stops your sessions
dying at logout; `tiles` tells you if it is off.

Tab completion:

```bash
tiles completions --install        # guesses your shell from $SHELL
tiles completions zsh              # or print it and place it yourself
```

bash, zsh, fish and elvish install to the directory each one already reads.

## Commands

```
tiles daemon [--no-notify]         run this machine's node
tiles agent                        what `ssh <host> tiles agent` runs
tiles service install|uninstall    run the node at boot
tiles ls [host]                    list sessions, everywhere or on one machine
tiles new [host] [-n name] [-d] [cmd]
                                   start a session (default: your login shell)
tiles attach <name|host/name>      attach to a session
tiles kill <name|host/name>        SIGHUP a session's process group
tiles rename <target> <title>      set a sticky title
tiles add <ssh-host> | hosts | rm <host>
                                   which machines to list and watch
tiles completions [shell] [--install]
```

A first argument to `tiles new` that is a command is what to run, here; anything
else is where to run it. So `tiles new claude` runs claude locally and
`tiles new gpu-box` gets a shell there, with no list to keep up to date.
`tiles new local <cmd>` forces this machine, for a program that shares a name
with one of your hosts.

A bare session name is looked for here first, then across every added machine,
so `tiles attach api` finds the session wherever you left it. Two machines with
a session of the same name is the one case you have to say which.

Each machine appears under its own hostname, so every row of `tiles ls` is a
machine name. `local` is always accepted when typing a target, so
`tiles attach api`, `tiles attach local/api` and `tiles attach thishost/api`
are the same thing.

`TILES_SSH` replaces the ssh program, if yours lives somewhere unusual or you
wrap it in a script.

Detach is `Ctrl-\` then `d`. `Ctrl-\ Ctrl-\` sends a literal `Ctrl-\` through,
so the key still works inside the session.

The default avoids tmux's `Ctrl-b` and screen's `Ctrl-a` on purpose: tiles has
no panes or tabs, so splitting a window is still their job, and taking their
prefix would mean swallowing it before it ever reached them. If you would
rather have the muscle memory, `TILES_PREFIX` takes `C-b`, `^b` or `b`:

```bash
export TILES_PREFIX=C-b     # then Ctrl-b d detaches
```

## Titles, bells and notifications

Sessions name themselves. Whatever the program sets as its terminal title
(OSC 0/1/2) shows up in `tiles ls`, so a Claude Code session appears under
whatever it is currently working on, and your local tab is titled to match.
`tiles rename` overrides it.

When a session rings the bell, or asks for a notification outright (OSC 9,
OSC 777), and nobody is attached to see it, your machine raises a desktop
notification: `osascript` on macOS, `notify-send` on Linux. A session someone
*is* watching stays quiet, and one session can only interrupt you once every 30
seconds however hard it rings.

```bash
tiles daemon --no-notify   # run the node, but stay silent
```

A clean exit is not worth interrupting you over; a non-zero one is.

## Terminal transparency

While you are attached the byte stream is untouched in both directions, so
mouse reporting, bracketed paste, focus events, title changes and terminal
queries all work exactly as they would locally.

Reattaching is the hard part. The screen comes back from the host's terminal
model, and that model has opinions only about the screen: it knows nothing
about mouse tracking, bracketed paste, the cursor style or the window title. So
those are tracked separately, straight off the byte stream, and replayed after
the repaint. Coming back to a full-screen program leaves its mouse working and
its tab correctly named, rather than dead until the program happens to
re-enable them.

Known gap: a program that queries the terminal (cursor position, background
colour) while nothing is attached gets no answer, because there is no terminal
to ask.

## Build

```bash
cargo build
cargo test        # includes end-to-end tests that a detach never signals the child
cargo clippy

cargo build --lib --no-default-features   # the client core, without PTYs or a terminal
```

The `desktop` feature carries everything that needs a PTY or a real terminal.
Without it you get the client core: the protocol and an `Attached` session that
hands out bytes. That is what a mobile app links against, and it has to keep
compiling.

The integration tests stand a second machine up by pointing `TILES_SSH` at a
stub that runs the agent directly, so they cover the whole path without needing
a real sshd or working credentials.

## Where things live

| Area | Files |
|---|---|
| Wire protocol | `src/proto.rs` |
| The node | `src/node/mod.rs` |
| Sessions, PTYs, screen state | `src/node/{session,registry,events}.rs` |
| Reaching other machines | `src/ssh.rs`, `src/node/peers.rs` |
| Notifications | `src/node/notify.rs` |
| Client core | `src/client/mod.rs` |
| Terminal client | `src/client/attach.rs` |
| Machines to watch | `src/hosts.rs` |
| Local socket, event fan-out | `src/ipc.rs` |
| Service units | `src/service.rs`, `contrib/` |
| Logging | `src/log.rs` |

## Logs

The CLI logs to stderr. `tiles daemon` also writes a daily-rotating file,
keeping the last seven: `~/Library/Logs/tiles` on macOS, `$XDG_STATE_HOME/tiles`
(usually `~/.local/state/tiles`) elsewhere. `TILES_LOG` sets the filter, e.g.
`TILES_LOG=tiles=debug`.

## License

MPL-2.0.
