# manymux

Persistent terminal sessions on every machine you already ssh into.

Start something long-running on a remote box, close the laptop, and pick it up
later from a different network. The session survives because the machine it runs
on owns the terminal, not your connection. One listing covers every machine, and
when a session wants attention it reaches the machine you are sitting at.

```bash
curl -fsSL https://raw.githubusercontent.com/rayfish/manymux/master/install.sh | sh
```

Run that here and on each machine you want to manage. There is nothing else to
set up: no pairing, no keys to exchange, no hosts to register. If you can `ssh`
it, you can `mm` it.

## A first session

Start something on another machine and attach to it:

```bash
mm new gpu-box claude
```

Press `Ctrl-\` then `d` to detach. Claude keeps running on gpu-box. Close the
laptop, go somewhere else, then look at what you have:

```console
$ mm ls
TARGET          TITLE                       ATTACHED  IDLE  BELL
gpu-box/claude  claude: rename the project  ○ -       4m    *
gpu-box/build   cargo watch                 ○ -       12m
api             uvicorn app:api             ● 1       0s
```

`TARGET` is the thing to type next. Pick one back up:

```bash
mm attach gpu-box/claude
```

The screen comes back as you left it, not as scrambled scrollback. The `*` under
`BELL` is that session having asked for you while nobody was watching.

## Starting sessions

```bash
mm new                                   # login shell, here
mm new claude                            # run claude, here
mm new gpu-box                           # login shell on gpu-box
mm new gpu-box claude                    # run claude on gpu-box
mm new -d gpu-box cargo test             # start it, but stay where you are
mm new -n api gpu-box uvicorn app:api    # name it yourself
```

The first argument is *where* to run if it is a machine, and *what* to run if it
is a command: a bare word found on your `PATH` is a command, anything else is an
ssh destination. So `mm new claude` runs claude and `mm new gpu-box` gets you a
shell there. For a program that shares a name with one of your hosts,
`mm new local <cmd>` forces this machine.

Sessions are named after the command unless you pass `-n`. Using a machine also
starts listing it.

## Detaching and coming back

`Ctrl-\` then `d` detaches. The prefix is `Ctrl-\` rather than tmux's `Ctrl-b`
or screen's `Ctrl-a` because you are quite likely running one of those *inside*
a session, and taking their prefix would mean swallowing it. Set `MM_PREFIX` to
change it:

```bash
export MM_PREFIX=C-b        # or ^B, or \x02
```

While you are attached, a dim `● host/name` sits in the bottom-right corner and
the window title is prefixed with `mm`, so a session is never mistaken for a
plain shell. The mark keeps a row to itself: the session is told the screen is
one row shorter, so nothing it draws lands there. Detaching gives the row, the
title and the terminal back.

Targets are `host/name` for another machine and a bare `name` for this one. A
bare name is looked for here first, then across every machine, so this finds the
session wherever you left it:

```bash
mm attach api
```

## Machines

Any ssh destination works, including the aliases, `ProxyJump` hosts and
tailscale names in your `~/.ssh/config`. Starting a session on a machine adds it
to the listing; `mm add` is for machines you only want to watch:

```bash
mm add deploy@prod-1
mm hosts
mm rm deploy@prod-1
```

A machine is listed and addressed by the destination exactly as you added it, so
`deploy@prod-1` gives you targets like `deploy@prod-1/api`, and that same
spelling is what `mm rm` wants. A short `Host` alias in your `~/.ssh/config`
keeps the column narrow.

## Ending sessions

```bash
mm kill gpu-box/build     # SIGHUP to the process group
```

Exiting the shell inside a session ends it too, and it drops out of the listing
on its own.

## Titles, bells and notifications

Sessions name themselves. Whatever the program sets as its terminal title
(OSC 0/1/2) shows up in `mm ls`, so a Claude Code session appears under whatever
it is working on. `mm rename` overrides it with something sticky:

```bash
mm rename gpu-box/build "nightly bench"
```

When a session rings the bell or asks for a notification outright (OSC 9,
OSC 777) and nobody is attached to see it, your machine raises a desktop
notification: `osascript` on macOS, `notify-send` on Linux. A session someone is
watching stays quiet, and one session can interrupt you at most every 30
seconds. A clean exit is not worth interrupting over; a non-zero one is.

`mm daemon --no-notify` runs the node but stays silent.

## Keeping it running

The node starts on demand, the way tmux starts its server, so nothing has to be
running for `mm new` to work. To have it survive logging out and keep watching
for bells while you are away:

```bash
mm service install
```

That writes the right unit for launchd, systemd, OpenRC, SysV init or rc.d. Run
as yourself on launchd or systemd and you get a per-user unit with no root
involved. Run it with `sudo`, or on a manager with no per-user services, and it
becomes a system unit naming the account it runs as.

Keeping the binary current, on each machine:

```bash
mm update --check
mm update
```

## Commands

```
mm ls [host]                         list sessions, everywhere or on one machine
mm new [host] [-n name] [-d] [cmd]   start a session (default: your login shell)
mm attach <target>                   attach; Ctrl-\ d detaches
mm kill <target>                     SIGHUP a session's process group
mm rename <target> <title>           set a sticky title
mm add <host> | hosts | rm <host>    which machines to list and watch
mm update [--check] [--force]        replace this binary with the published one
mm service install|uninstall         run the node at boot
mm completions [shell] [--install]   tab completion for your shell
mm daemon | agent                    the node, and what ssh runs on the far side
```

The ones you type often have a short form: `l`, `n`, `a`, `k`, `r`, `h`, `up`.

## Install details

The installer puts `mm` in `/usr/local/bin`, asking for `sudo` if that is what
it takes. That is not fussiness: reaching a machine runs `ssh host mm agent`
through a *non-interactive* shell, which reads neither `.zshrc` nor `.bashrc`,
so a binary in `~/.local/bin` is invisible to it and the machine looks like it
has no manymux at all. `INSTALL_DIR` overrides the location and `MM_VERSION`
pins a release.

## Environment

| | |
|---|---|
| `MM_PREFIX` | detach key, e.g. `C-b`. Default `Ctrl-\` |
| `MM_SSH` | the ssh program, if yours lives somewhere unusual |
| `MM_LOG` | log filter, e.g. `manymux=debug` |
| `MM_CONFIG_DIR` | where the host list lives |
| `NO_COLOR` | plain output; `CLICOLOR_FORCE` colours a pipe |

The CLI logs to stderr. The node also writes a daily-rotating file, keeping
seven: `~/Library/Logs/manymux` on macOS, `$XDG_STATE_HOME/manymux` elsewhere.

## Why not tmux

Tmux keeps the session alive, which is most of the job, but it is server-local:
`tmux ls` only lists the machine you already logged into, so five boxes are five
separate worlds. And when something finishes on a machine you are not looking
at, nothing tells you.

manymux keeps the sessions and adds those two: one listing across every machine,
and events that reach the machine you are actually sitting at. It has no panes,
tabs or splits, because tmux is still there for that, inside a session.

## How it works

```
  laptop                          gpu-box
  mm ls, mm attach                mm daemon (one node per user, unix socket)
        |                         |
        +-- ssh gpu-box mm agent -+
                                  |
                                  +-- session "api"   -> PTY -> claude
                                  +-- session "build" -> PTY -> cargo watch
```

A **node** is one `mm daemon` process. It owns that machine's sessions and
listens on a Unix socket; that is all.

- **No networking of its own.** Reaching a machine is `ssh <host> mm agent`,
  using your `~/.ssh/config`, so `ProxyJump`, tailscale names and jump hosts all
  work unchanged. Connections are shared (`ControlMaster`), so the second
  command to a host is a round trip rather than a handshake.
- **No second ACL.** sshd decides who gets in with whatever you already
  configured. manymux keeps no allowlist, no invites and no keys, so there is
  nothing to drift out of sync and no way for it to grant access ssh would
  refuse.
- **The node owns the PTY**, so a client leaving is invisible to the child: no
  SIGHUP, no EOF. That is the whole trick.
- **One node per user**, because each socket sits in its owner's runtime
  directory. `ssh deploy@box` lands in deploy's. manymux never switches users or
  runs as root; ssh already put the request in the right account.
- **Reattaching repaints** from a headless terminal emulator
  ([`avt`](https://github.com/asciinema/avt)) fed every output byte, so htop and
  vim come back intact. Mouse reporting, bracketed paste, cursor style and the
  window title are tracked separately and replayed after it, since the screen
  model has no opinion on those.

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
