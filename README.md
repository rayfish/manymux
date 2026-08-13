# tiles

Persistent terminal sessions on every machine you already ssh into.

Start something long-running on a remote box, close the laptop, and pick it up
later from a different network. The session survives because the machine it runs
on owns the terminal, not your connection.

```bash
tiles new gpu-box claude   # start a session there and attach
                           # Ctrl-\ d to detach; it keeps running
tiles ls                   # everything, everywhere
tiles attach gpu-box/api   # pick up where you left off
```

`gpu-box` is an ssh destination and nothing else. Nothing to pair, no key to
exchange, no host to register: if you can `ssh` it, you can `tiles` it.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/rayfish/tiles/master/install.sh | sh
```

Run it here and on each machine you want to manage. That is the entire remote
setup: no service, no config. The node starts itself on demand, the way tmux
starts its server.

It installs to `/usr/local/bin`, asking for `sudo` if that is what it takes.
That is not fussiness: reaching a machine runs `ssh host tiles agent` through a
*non-interactive* shell, which reads neither `.zshrc` nor `.bashrc`, so a binary
in `~/.local/bin` is invisible to it and the machine looks like it has no tiles
at all. `INSTALL_DIR` overrides it, `TILES_VERSION` pins a release, and
afterwards `tiles update` replaces the binary in place.

Optionally run the node as a service, so it survives logging out and keeps
watching for bells: `tiles service install`. It writes the right unit for
launchd, systemd, OpenRC, SysV init or rc.d. The first two are per-user and need
no root; the others are system-wide and need `sudo`.

## Why not tmux

Tmux keeps the session alive, which is most of the job, but it is server-local:
`tmux ls` only lists the machine you already logged into, so five boxes are five
separate worlds. And when something finishes on a machine you are not looking
at, nothing tells you.

tiles keeps the sessions and adds those two: one listing across every machine,
and events that reach the machine you are actually sitting at.

## How it works

```
laptop                                          gpu-box
tiles ls/attach ─► ssh gpu-box tiles agent ──► tiles daemon (unix socket)
                   ▲                            │
                   your ssh config              ├─ session "api"   → PTY → claude
                                                └─ session "build" → PTY → cargo watch
```

A **node** is one `tiles daemon` process. It owns that machine's sessions and
listens on a Unix socket; that is all. One per user per machine, started on
demand.

- **No networking of its own.** Reaching a machine is `ssh <host> tiles agent`,
  using your `~/.ssh/config`, so `ProxyJump`, tailscale names and jump hosts all
  work unchanged. Connections are shared (`ControlMaster`), so the second
  command to a host is a round trip rather than a handshake.
- **No second ACL.** sshd decides who gets in with whatever you already
  configured. tiles keeps no allowlist, no invites, no keys, so there is nothing
  to drift out of sync and no way for it to grant access ssh would refuse.
- **The node owns the PTY**, so a client leaving is invisible to the child: no
  SIGHUP, no EOF. That is the whole trick.
- **One node per user**, because each socket sits in its owner's runtime
  directory. `ssh deploy@box` lands in deploy's. tiles never switches users or
  runs as root; ssh already put the request in the right account.
- **Reattaching repaints** from a headless terminal emulator
  ([`avt`](https://github.com/asciinema/avt)) fed every output byte, so htop and
  vim come back intact rather than as scrambled scrollback. Mouse reporting,
  bracketed paste, cursor style and the window title are tracked separately and
  replayed after it, since the screen model has no opinion on those.

## Commands

```
tiles ls [host]                    list sessions, everywhere or on one machine
tiles new [host] [-n name] [-d] [cmd]
                                   start a session (default: your login shell)
tiles attach <target>              attach; Ctrl-\ d detaches
tiles kill <target>                SIGHUP a session's process group
tiles rename <target> <title>      set a sticky title
tiles add <ssh-host> | hosts | rm <host>
                                   which machines to list and watch
tiles update [--check] [--force]   replace this binary with the published one
tiles service install|uninstall    run the node at boot
tiles daemon | agent               the node, and what ssh runs on the far side
tiles completions [shell] [--install]
```

A target is what the `TARGET` column shows: `gpu-box/api` elsewhere, a bare
`api` here. A bare name is looked for here first, then across every machine, so
`tiles attach api` finds it wherever you left it.

For `tiles new`, a first argument that is a command is what to run, here;
anything else is where to run it. So `tiles new claude` runs claude locally and
`tiles new gpu-box` gets a shell there. `tiles new local <cmd>` forces this
machine, for a program that shares a name with one of your hosts. Using a
machine also adds it to `tiles ls`.

## Titles, bells and notifications

Sessions name themselves: whatever the program sets as its terminal title
(OSC 0/1/2) shows up in `tiles ls`, so a Claude Code session appears under
whatever it is working on. `tiles rename` overrides it.

When a session rings the bell or asks for a notification outright (OSC 9,
OSC 777) and nobody is attached to see it, your machine raises a desktop
notification: `osascript` on macOS, `notify-send` on Linux. A session someone is
watching stays quiet, and one session can interrupt you at most every 30
seconds. A clean exit is not worth interrupting over; a non-zero one is.

`tiles daemon --no-notify` runs the node but stays silent.

## Environment

| | |
|---|---|
| `TILES_PREFIX` | detach key, e.g. `C-b`. Default `Ctrl-\`, which avoids tmux's and screen's prefixes because you are likely running one of those inside a session |
| `TILES_SSH` | the ssh program, if yours lives somewhere unusual |
| `TILES_LOG` | log filter, e.g. `tiles=debug` |
| `TILES_CONFIG_DIR` | where the host list lives |
| `NO_COLOR` | plain output; `CLICOLOR_FORCE` colours a pipe |

The CLI logs to stderr. The node also writes a daily-rotating file, keeping
seven: `~/Library/Logs/tiles` on macOS, `$XDG_STATE_HOME/tiles` elsewhere.

## Build

```bash
cargo build --release
cargo test        # includes end-to-end tests through a stub ssh
```

`cargo build --lib --no-default-features` is the client core, without PTYs or a
terminal: what a mobile app links against, and it has to keep compiling.

Known gap: a program that queries the terminal (cursor position, background
colour) while nothing is attached gets no answer, because there is no terminal
to ask.

## License

MPL-2.0.
