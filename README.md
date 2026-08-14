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

Press `Ctrl-]` then `d` to detach. Claude keeps running on gpu-box. Close the
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

`Ctrl-]` then `d` detaches. Not tmux's `Ctrl-b` or screen's `Ctrl-a`, because
you are quite likely running one of those *inside* a session: manymux has no
panes, so splitting a window is still their job, and taking their prefix would
mean swallowing it. Set `MM_PREFIX` to change it:

```bash
export MM_PREFIX=C-b        # or ^B, or \x02
```

While you are attached, a dim `focus ● host/name` sits in the bottom-right
corner and the window title is prefixed with `mm`, so a session is never
mistaken for a plain shell. The row keeps to itself: the session is told the
screen is one row shorter, so nothing it draws lands there. Detaching gives the
row, the title and the terminal back.

Copying out of a session is your terminal's job, over OSC 52, which manymux
passes through untouched. Most terminals refuse clipboard writes until you say
otherwise (iTerm2: Settings > General > Selection; Terminal.app has no OSC 52 at
all). A session is drawn on the alternate screen, so mouse selection covers what
is on screen and not what has scrolled past it.

Targets are `host/name` for another machine and a bare `name` for this one. A
bare name is looked for here first, then across every machine, so this finds the
session wherever you left it:

```bash
mm attach api
```

## Pasting an image

`Ctrl-V` pastes the image on your clipboard into a session on another machine.
The client reads the clipboard here, sends the bytes to the host, and what the
program receives is the path they were written to, which is what `claude` wants:
it reads the file itself.

Nothing is installed for it on the far machine. On yours it uses what is already
there: AppleScript on macOS, `wl-paste` on Wayland, `xclip` on X11. Copying an
image *file* in a file manager works too. The key is only taken when there is an
image to send, so `Ctrl-V` still reaches the session otherwise; `MM_PASTE=off`
gives it back for good. Pasted files land in `$XDG_RUNTIME_DIR/manymux/pastes`
on the host and are cleaned up after a day.

## Two modes

Modal, like vim, and for the same reason: the keys worth having are the ones a
session wants for itself.

**Focus** is where you live. Every keystroke is the session's, and the row at
the bottom reads `focus ● host/name`.

**Control** is where the keys are the client's. `Ctrl-]` gets you there, the
word goes amber, and the row spells out what the keys do. It stays on, so one
`Ctrl-]` then `tab tab tab` walks through your sessions.

| | |
|---|---|
| `tab`, `n` | next session |
| `p`, `shift-tab` | previous |
| `l` | the one you came from |
| `d` | detach |
| `esc`, `enter`, `Ctrl-]` | back to focus |

The cycle covers every session on every machine you watch, in the order `mm ls`
prints them. `Ctrl-]` twice in quick succession, within three seconds, also
sends one through to the session, for whatever wants it in there. Slower than
that it is just a look at the mode and a way back out, and nothing reaches the
session. Any other key drops back to focus and goes through, so a mistyped mode
key costs you a stray keystroke rather than a swallowed line.

### About that key

`Ctrl-]` is picked for being one every terminal sends without being asked, and
one nothing much else wants. Two near misses, in case you were about to suggest
them:

- **`Ctrl-Space`** is taken by input switching: macOS binds it to the next input
  source once you have two, and fcitx5 and ibus do the same on Linux. It never
  reaches the terminal at all.
- **``Ctrl-` ``** looks like it should work, since clearing the top bits off a
  backtick gives the same NUL as `Ctrl-Space`. Terminals only do that masking
  for `@`, `A`-`Z`, `[`, `\`, `]`, `^`, `_` and space, and the backtick is
  outside the set, so you get a plain backtick and nothing happens.

What does want `Ctrl-]` is vim's jump-to-tag and telnet's escape. Press it twice
quickly to send one through, in there or anywhere else. `MM_PREFIX` takes any
of the above spellings (`C-]`, `C-b`, `^B`, `C-Space`, or the raw byte).

To see what your terminal sends for a key, run `cat -v` outside a session and
press it: `Ctrl-]` shows up as `^]`.

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
mm attach <target>                   attach; Ctrl-] then tab switches, d detaches
mm kill <target>                     SIGHUP a session's process group
mm rename <target> <title>           set a sticky title
mm add <host> | hosts | rm <host>    which machines to list and watch
mm update [--check] [--force]        replace this binary with the published one
mm service install|uninstall         run the node at boot
mm completions [shell] [--install]   tab completion, with your session names in it
mm daemon | agent                    the node, and what ssh runs on the far side
```

The ones you type often have a short form: `l`, `n`, `a`, `k`, `r`, `h`, `up`.

## Tab completion

`mm completions --install` writes the script where your shell looks for it, and
`mm update` rewrites it afterwards. It is a stub that asks `mm` on every tab, so
`mm a <TAB>` offers the sessions running right now:

```
$ mm a <TAB>
api  build  gpu-box/  laptop/

$ mm a gpu-box/<TAB>
gpu-box/train  gpu-box/logs
```

A bare tab stays on this machine and offers `host/` as a way in, because a
keystroke should not wait on ssh. Only naming a machine goes out to it, and it
gives up rather than hanging if that machine is asleep.

`--install` writes to `~/.local/share/zsh/site-functions`, which zsh does not
search unless you put it on the `fpath` before `compinit` runs. If that is a
line you would rather not add, source the script instead, at the end of
`~/.zshrc`:

```bash
echo 'source <(mm completions zsh)' >> ~/.zshrc
```

## Install details

Reaching a machine runs `ssh host mm agent` through a *non-interactive* shell,
which reads neither `.zshrc` nor `.bashrc`. Where `mm` has to live so that shell
can find it differs by platform, and the installer handles both.

On Linux it goes in `/usr/local/bin`, asking for `sudo` if that is what it
takes. That is not fussiness: `/usr/local/bin` is on the PATH sshd hands out
there, and `~/.local/bin` is invisible to it, so a machine installed that way
looks like it has no manymux at all.

On macOS it goes in `~/.local/bin`, no `sudo`, and the installer puts that
directory on your PATH in `~/.zshenv`. macOS gives `/usr/local/bin` no such
advantage: it reaches an interactive shell only through `path_helper` in
`/etc/zprofile`, which a non-interactive `zsh -c` never reads, so a system
install would cost a password and still leave the machine unreachable.
`~/.zshenv` is the file that works, being the one zsh reads on *every*
invocation. `MM_SKIP_PATH=1` leaves it alone if you would rather do it yourself.

`INSTALL_DIR` overrides the location and `MM_VERSION` pins a release.

## Environment

| | |
|---|---|
| `MM_PREFIX` | control-mode key, e.g. `C-b`. Default `Ctrl-]` |
| `MM_PASTE` | `off` gives `Ctrl-V` back to the session |
| `MM_SSH` | the ssh program, if yours lives somewhere unusual |
| `MM_LOG` | log filter, e.g. `manymux=debug` |
| `MM_CONFIG_DIR` | where the host list lives |
| `MM_COMPLETE_REMOTE` | let a bare tab reach every machine, not just this one |
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
