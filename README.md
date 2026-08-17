# manymux

Persistent terminal sessions on every machine you already ssh into.

Start something long-running on a remote box, close the laptop, and pick it up
later from a different network. The session survives because the machine it runs
on owns the terminal, not your connection. One listing covers every machine, and
when a session wants attention it reaches the machine you are sitting at.

```bash
curl -fsSL https://raw.githubusercontent.com/rayfish/manymux/master/install.sh | sh
```

Run that here. The machines you reach over ssh get it when you first name one:
`mm new gpu-box` offers to install it there, and `mm setup gpu-box` does it
ahead of time. There is nothing else to set up: no pairing, no keys to exchange,
no hosts to register. If you can `ssh` it, you can `mm` it.

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

While you are attached, a dim `● host/name` sits in the bottom-right
corner and the window title is prefixed with `mm`, so a session is never
mistaken for a plain shell. The row keeps to itself: the session is told the
screen is one row shorter, so nothing it draws lands there. Detaching gives the
row, the title and the terminal back.

If the connection drops rather than you leaving, the client waits instead of
dropping you back at your shell. The session is still running on a machine that
never noticed, so a wifi hop or a closed lid just puts a line on that row and
the screen comes back as it was:

```
not answering, retrying in 4s  ctrl-c to stop
```

It keeps trying, quickly at first and then every ten seconds, and it never
stops on its own: the session is still running on a machine that never noticed
you left, so a laptop shut for the weekend opens on the session you were in.
Giving up is yours to do, with `Ctrl-C` or `Ctrl-]` then `d`.

What it goes back to is the session you were in when the connection went, not
the one you started on: if you attached with `mm a gpu-box` and tabbed on from
there, that is where it puts you.

## Watching without typing

```bash
mm view gpu-box/claude
```

The same screen an attach gives you, scrolling and search included, with the
keyboard going nowhere. The mark is a hollow grey `◦` rather than a green dot,
so a window you cannot type into never looks like one you can.

The node is what enforces it, not the client, so this is safe to point at a
session somebody else is working in: their keystrokes are theirs, and yours
are dropped at the far end rather than on trust. A viewer also stays out of the
size negotiation, so watching from a phone or a narrow split cannot reflow the
screen of whoever is working. `mm view` on a machine running a manymux too old
to promise any of that says so instead of attaching.

Copying out of a session is your terminal's job, over OSC 52, which manymux
passes through untouched. Most terminals refuse clipboard writes until you say
otherwise (iTerm2: Settings > General > Selection; Terminal.app has no OSC 52 at
all). A session is drawn on a screen of its own by default, so mouse selection
covers what is on screen and not what has scrolled past it. See
[Scrollback](#scrollback) for the other way round.

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
gives it back for good. Pasted files land in `/tmp/manymux-<uid>/pastes`
on the host and are cleaned up after a day.

If it says the host is too old to take pasted files, that is the *node* holding
the session, not the binary you typed into: see [Keeping it
running](#keeping-it-running).

## Modes

Modal, like vim, and for the same reason: the keys worth having are the ones a
session wants for itself.

**Focus** is where you live. Every keystroke is the session's, and the row at
the bottom reads `● host/name` with the dot green.

**Control** is where the keys are the client's. `Ctrl-]` gets you there, the dot
goes hollow and amber, and the row spells out what the keys do. It stays on, so
one `Ctrl-]` then `tab tab tab` walks through your sessions.

| | |
|---|---|
| `tab` | next session on this machine |
| `shift-tab` | previous |
| `h` | next machine |
| `H` | previous machine |
| `l` | the one you came from |
| `n` | start a session on this machine and go to it |
| `r` | rename this session |
| `d` | detach |
| `esc`, `enter`, `Ctrl-]` | back to focus |

`n` starts a shell on the machine you are on, the way `mm new` would, and puts
you in it in focus mode: it is the one control key that does not leave the mode
on, because what follows a new session is typing rather than another hop. The
node picks the name, and `mm ls` has it from then on.

Ending a session you moved to puts you back in the one you came from, rather
than back at your shell: type `exit` in the session `n` just started and you are
where you pressed it, with the row saying what ended and with what status. Only
the session you named on the command line ends the attach when it exits, so
`mm attach gpu-box/build; echo $?` still tells you what the build did.

`r` opens a prompt on the mark row: type a name, `enter` renames the session,
`esc` leaves it alone. It is the same rename `mm rename` does, so the mark and
`mm ls` say the new name from then on, and the row says so if the host refused
it because another session is already called that.

Two levels, because that is how your sessions are arranged. `tab` stays on the
machine you are on and wraps around its sessions; `h` moves you to the next
machine you watch and lands on its first session. Both go in the order `mm ls`
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

## Names

A session is addressed by `host/name`, and the name is yours to change:

```bash
mm rename gpu-box/zsh-3 build
```

From inside the session it is `Ctrl-] r`, then the name, then `enter`. Names go
in paths and in the terminal, so they keep to letters, digits, `-`, `_` and `.`:
a space becomes a dash and anything else is dropped, and what you end up called
is what the mark row and `mm ls` show. A name another session on that machine
already has is refused rather than made unique behind your back.

## Titles, bells and notifications

Sessions name themselves. Whatever the program sets as its terminal title
(OSC 0/1/2) shows up in `mm ls`, so a Claude Code session appears under whatever
it is working on. That column follows the program and nothing else: the name is
what you set, the title is what it is doing.

When a session rings the bell or asks for a notification outright (OSC 9,
OSC 777) and nobody is attached to see it, you are told. Where depends on where
you are sitting:

- Attached to a session on that machine, a bell in one of its other sessions
  goes to the terminal in front of you, as an OSC 9 that iTerm2, kitty, ghostty,
  WezTerm, foot, VS Code and Windows Terminal raise as a desktop notification.
  The status row names the session for a few seconds as well, for a terminal
  that raises nothing. This is the route that works over ssh, where a
  notification on the far machine would be raised on a desktop nobody is at.
- Attached nowhere on it, whichever machine watches it says so on its own
  desktop: `osascript` on macOS, `notify-send` on Linux.

Somebody attached on a machine is enough for the desktop notifier to keep out of
the way, so one bell interrupts once. A session someone is watching stays quiet
altogether, and one session can interrupt you at most every 30 seconds. A clean
exit is not worth interrupting over; a non-zero one is.

To turn the lot off:

```bash
mm config notify off
```

It takes hold at once, in the session you are already attached to and in any
node already running. `MM_NOTIFY=off` does the same for one command, and
`mm daemon --no-notify` runs the node but stays silent.

## Scrollback

Attaching takes a screen of its own, the way tmux does, so your terminal's
scrollbar still shows the shell you attached from and the wheel has nothing of
the session's to move. If you would rather your terminal kept the session's
history:

```bash
mm attach --screen inline web01     # this attach only
mm config screen inline             # from now on
```

Inline paints on the terminal's own screen, so what the session prints scrolls
into your terminal's scrollback, and the wheel, the find bar and selection are
your terminal's own. Attaching also brings the last thousand lines the session
printed while you were away, so a build you were not there for is there to
scroll. A full-screen program in the session (vim, less) reaches for the
terminal's alternate screen itself, exactly as it would over plain ssh.

The catch is that the mark on the bottom row is fenced off with a scrolling
region, and a terminal that throws away what scrolls out of one keeps nothing.
iTerm2 keeps it. If yours does not, `--screen alternate` is the way back.

On the alternate screen the terminal has no scrollback to offer, so manymux
shows you its own. `Ctrl-] [` opens it, `pgup`/`pgdn`, `g` and `G` move it, and
`esc` goes back to the live session.

The mouse is your terminal's, so dragging selects and copies exactly as it does
anywhere else, with no modifier held. That is why the wheel does not open the
view: reading the wheel means asking the terminal to report the mouse, and a
terminal reporting the mouse is not selecting with it. If you would rather
scroll with the wheel than select with a bare drag, ask for it:

```bash
mm config mouse client
```

Then the mouse is manymux's for the whole attach. A notch opens the view and
moves it, and a drag selects: press, drag, let go, and what you dragged over is
on your clipboard. Double click takes the word under it, which counts a path or
a URL as one word, and triple click takes the line. Hold the drag against the
top or bottom of the window and the view moves under it a line at a time, so a
selection is not limited to what happens to be on the screen. Letting go at the
live screen gives it straight back, so the session never appears to stop;
scrolled back, the highlight stays where you left it and so does the view.

The copy goes to your terminal over OSC 52, which most terminals refuse until
you say otherwise (iTerm2: Settings > General > Selection). Without that the
highlight works and nothing lands on the clipboard, and there is no way for
manymux to tell.

Nothing here is taken from a program that asked for the mouse itself: Claude
Code, vim and htop keep every report, wheel and drag alike, under either
setting. Inside one of those, selecting is that program's business exactly as
it is over plain ssh.

`Ctrl-] /` searches everything the session has printed, all ten thousand lines
of it. `n` walks back through the matches and `N` comes back towards the live
screen. Lowercase ignores case; a capital means it. Every match comes back in
one answer, so walking them costs nothing even on a machine two hops away.

None of this exists inline, and none of those keys are taken there: your
terminal's own scrollbar and find bar are already looking at the same lines,
and they are better at it.

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
mm update --nightly   # master's last build, rather than the newest release
```

`--nightly` is not remembered: a plain `mm update` afterwards puts the release
back, which goes backwards whenever master is ahead of the tag.

Replacing the binary is only half of it. The node is a long-running process
still executing the one it started from, so until it restarts the machine keeps
behaving like the old build: a client that pastes images into a session hosted
by an older node gets told the host cannot take one, however current its own
binary is. `mm update` asks the node what it is running and offers the restart
when the two differ, including when there was nothing to download. Every session
dies with the node, so it asks first, and `--force` answers in advance:

```bash
mm restart          # asks, if there are sessions to lose
mm restart --force  # does not
```

Run from a script or over ssh there is nobody to ask, so it says what the
restart would cost and leaves it. `mm stop` and `mm start` are the same thing in
halves, for a machine to leave quiet or one to have ready before anything asks.

The sessions do go, but they are hung up first and given a couple of seconds,
so shells write their history and editors write their swap files rather than
finding the terminal gone mid-write. Whatever is still running when that time
is up is killed rather than left on a terminal that no longer exists. The same
happens on SIGTERM, so stopping the service or rebooting is no more abrupt than
`mm stop`.

The machine that has to be current is the one the *session* lives on, which for
a remote session is the far end: `ssh gpu-box mm update`.

Nightly builds all share one version number, so `mm --version` names the commit
too:

```
mm 0.1.0 (a1b2c3d4)
```

That is the string to compare when you want to know whether two machines are
actually running the same thing.

## Commands

```
mm ls [host]                         list sessions, everywhere or on one machine
mm new [host] [-n name] [-d] [cmd]   start a session (default: your login shell)
mm attach <target> [--screen ...]    attach; Ctrl-] then tab switches, d detaches
mm view <target> [--screen ...]      watch a session without typing into it
mm kill <target>                     SIGHUP a session's process group
mm rename <target> <name>            give a session a different name
mm add <host> | hosts | rm <host>    which machines to list and watch
mm config [key] [value]              show or change a setting: notify, screen, mouse
mm update [--check] [--nightly]      replace this binary with the published one
mm start | stop | restart [--force]  this machine's node: up, down, and again
mm service install|uninstall         run the node at boot
mm completions [shell] [--install]   tab completion, with your session names in it
```

The ones you type often have a short form: `l`, `n`, `a`, `v`, `k`, `r`, `h`,
`up`.

Two more exist and are hidden from `mm --help`, because nothing types them:
`mm daemon` is the node itself, run by the service unit or by a client that
found none running, and `mm agent` is what `ssh <host> mm agent` runs. See
[How it works](#how-it-works).

## Tab completion

`mm completions --install` writes the script where your shell looks for it, and
`mm update` installs it if there is none and rewrites it if there is, whether
that update downloads anything or not, so a machine set up with `install.sh`
picks it up the next time it looks. It is a stub that asks `mm` on every tab,
so `mm a <TAB>` offers the sessions running right now:

```
$ mm a <TAB>
api  build  gpu-box/  laptop/

$ mm a gpu-box/<TAB>
gpu-box/train  gpu-box/logs
```

A bare tab stays on this machine and offers `host/` as a way in, because a
keystroke should not wait on ssh. Only naming a machine goes out to it, and it
gives up rather than hanging if that machine is asleep.

For zsh, `--install` asks zsh which directories it autoloads from and writes to
the first one you can write in: `$PREFIX/share/zsh/site-functions` on Termux,
`/usr/local/share/zsh/site-functions` on a Homebrew mac. Nothing needs adding to
any file in that case, and nothing is said beyond where the script went.

Where none of them is yours, which is the usual case on a shared Linux box, the
script goes to `~/.local/share/zsh/site-functions` and that directory has to go
on the `fpath` before `compinit` runs:

```bash
fpath=(~/.local/share/zsh/site-functions $fpath)
autoload -Uz compinit && compinit
```

`mm` prints those two lines once, and stops printing them once `.zshrc` names
the directory. If it is a line you would rather not add, source the script
instead, at the end of `~/.zshrc`:

```bash
echo 'source <(mm completions zsh)' >> ~/.zshrc
```

bash under Termux is the other exception: it searches `$PREFIX` rather than
`$HOME`, so its script goes there too, and it reads any completion directory
only once `pkg install bash-completion` has happened, which is not part of the
Termux base and is not something `mm` can do for you.

## Verifying a release

Every release publishes `<asset>.sha256` beside the binary, and a signature over
that sidecar as `<asset>.sha256.sig`: Ed25519, hex encoded. The chain is the
ordinary one, and both halves count. The signature says the sidecar is ours, and
the sidecar says which binary it was written for.

`mm update` checks both by itself, with no tooling on the machine, and refuses
to install a signature that is not ours. To check one by hand:

```bash
curl -fsSLO https://github.com/rayfish/manymux/releases/latest/download/mm-linux-x86_64.sha256
curl -fsSLO https://github.com/rayfish/manymux/releases/latest/download/mm-linux-x86_64.sha256.sig
xxd -r -p mm-linux-x86_64.sha256.sig > sig.bin
openssl pkeyutl -verify -pubin -inkey release.pem -rawin \
    -in mm-linux-x86_64.sha256 -sigfile sig.bin
sha256sum -c mm-linux-x86_64.sha256
```

The checksum on its own catches a download that arrived wrong, and nothing more:
it comes from the same place as the binary, so anything that can replace one can
replace the other. The signature is what says a release was published by
somebody holding the key, which the release host does not have.

## Install details

Reaching a machine runs `ssh host mm agent` through a *non-interactive* shell,
which reads neither `.zshrc` nor `.bashrc`. Where `mm` has to live so that shell
can find it differs by platform, and the installer handles both.

On Linux it goes in `/usr/local/bin`, taking `sudo` if this account has it for
free. That is not fussiness: `/usr/local/bin` is on the PATH sshd hands out
there, and `~/.local/bin` is invisible to it. An account that would have to type
a password gets `~/.local/bin` instead of a prompt, since a deploy user reached
by key has no password to type and no sudoers entry either. A client that gets
nothing from a plain `mm` tries `~/.local/bin/mm` next, so such a machine is
still reachable, at one wasted ssh per connection. `curl ... | sudo sh` puts it
in `/usr/local/bin` and saves that.

On macOS it goes in `~/.local/bin`, no `sudo`, and the installer puts that
directory on your PATH in `~/.zshenv`. macOS gives `/usr/local/bin` no such
advantage: it reaches an interactive shell only through `path_helper` in
`/etc/zprofile`, which a non-interactive `zsh -c` never reads, so a system
install would cost a password and still leave the machine unreachable.
`~/.zshenv` is the file that works, being the one zsh reads on *every*
invocation. `MM_SKIP_PATH=1` leaves it alone if you would rather do it yourself.

On Android the same line works inside [Termux](https://termux.dev), which gets
its own binary: `mm` there is linked against bionic, so neither Linux build
runs. It lands in `$PREFIX/bin`, which is already on PATH and needs no root.
`pkg install curl` first if the installer says so.

A phone is a client like any other machine, so `mm ls` and `mm attach gpu-box/x`
work from it. Sessions started *on* the phone are the one difference: they last
as long as Termux does, and `termux-wake-lock` is what stops Android freezing it
in the background. There is no service to install, since Android has none.

`INSTALL_DIR` overrides the location and `MM_VERSION` pins a release.

## Environment

| | |
|---|---|
| `MM_PREFIX` | control-mode key, e.g. `C-b`. Default `Ctrl-]` |
| `MM_PASTE` | `off` gives `Ctrl-V` back to the session |
| `MM_SSH` | the ssh program, if yours lives somewhere unusual |
| `MM_LOG` | log filter, e.g. `manymux=debug` |
| `MM_NOTIFY` | `off` silences bells for one command; see `mm config` |
| `MM_CONFIG_DIR` | where the host list and settings live |
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
- **One node per user**, because the socket is named from the uid alone
  (`/tmp/manymux-<uid>`) and from nothing about the login. `ssh deploy@box`
  lands in deploy's node, and your own sessions are the same ones whether you
  came in over plain ssh, a mesh, a phone, or the keyboard in front of you.
  manymux never switches users or runs as root; ssh already put the request in
  the right account.
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
