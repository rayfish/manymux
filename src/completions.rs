//! Putting the completion script where the shell will find it.
//!
//! Printing one is clap_complete's job and takes no thought. Everything here is
//! the other question: which directory this shell actually reads, given that
//! the answer moves with the platform (Termux relocates the whole prefix), with
//! the shell (zsh searches an `fpath` it will tell you about, bash and fish do
//! not), and with whether that directory is writable by the person who typed
//! it. Getting it wrong writes a file nobody ever loads.
//!
//! A binary module rather than part of the library: nothing but the CLI has a
//! shell to install anything for.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Result, bail};
use clap_complete::Shell;
use tracing::debug;

use crate::{OK, complete};

/// Install the completion script after an update, or rewrite the one there.
///
/// The script and the binary talk to each other, and clap_complete makes no
/// promise that a script written by one version still works with the next, so a
/// script already installed is rewritten silently: nobody asked about
/// completions, they asked for an update. A machine that never had one gets it
/// now and is told where it went, which is the only way someone who installed
/// with `install.sh` ever ends up with working completion.
///
/// Neither is worth failing an update over, so a write that does not work is
/// left to the log.
pub fn refresh() {
    let Some(shell) = Shell::from_env() else {
        return;
    };
    let Some(at) = completion_path(shell) else {
        return;
    };
    match write_completions(shell, &at) {
        Ok(true) => println!("{}", installed_message(shell, &at)),
        Ok(false) => {}
        Err(e) => debug!("could not install {}: {e:#}", at.path.display()),
    }
}

/// Print a completion script, or write it where the shell will find it.
pub fn install(shell: Option<Shell>, install: bool) -> Result<u8> {
    let Some(shell) = shell.or_else(Shell::from_env) else {
        bail!("could not tell which shell you use; name it: mm completions zsh");
    };
    if !install {
        complete::registration(shell, &mut std::io::stdout())?;
        return Ok(OK);
    }

    let Some(at) = completion_path(shell) else {
        bail!(
            "no install path known for {shell}; print it and place it yourself: \
             mm completions {shell}"
        );
    };
    write_completions(shell, &at)?;
    // Asked for outright, so it is said whether the script was already there or
    // not; only `mm update` has a reason to keep quiet about a rewrite.
    println!("{}", installed_message(shell, &at));
    println!("the script asks `mm` for session names as you type, so it needs mm on your PATH");
    Ok(OK)
}

/// Where a completion script goes, and whether the shell looks there by itself.
struct Location {
    path: PathBuf,
    /// zsh reads completions off its `fpath` and from nowhere else, and no
    /// per-user directory is on it, so installing under `$HOME` costs a line
    /// in `.zshrc`. Where one of zsh's own `fpath` entries turns out to be
    /// ours to write in, the script goes there instead and there is nothing to
    /// say: advice about a directory that already works sends someone to fix
    /// nothing.
    searched: bool,
}

/// Where each shell looks for user-installed completions.
///
/// Deliberately not `dirs::data_dir()`: shells follow the XDG layout on macOS
/// too, where that would point at `~/Library/Application Support`.
fn completion_path(shell: Shell) -> Option<Location> {
    let home = dirs::home_dir()?;
    let base = |var: &str, fallback: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(fallback))
    };
    // Termux is a prefix installation on a system that has no `/usr`, and its
    // bash is built to search that prefix. Nowhere else has a `$PREFIX` worth
    // reading, so nowhere else reads it.
    let prefix = if cfg!(target_os = "android") {
        std::env::var_os("PREFIX")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    } else {
        None
    };
    // Only zsh has a choice to make, and asking costs a process.
    let fpath = if shell == Shell::Zsh {
        zsh_fpath()
    } else {
        Vec::new()
    };
    completion_path_in(
        shell,
        &base("XDG_DATA_HOME", ".local/share"),
        &base("XDG_CONFIG_HOME", ".config"),
        prefix.as_deref(),
        &fpath,
    )
}

fn completion_path_in(
    shell: Shell,
    data: &Path,
    config: &Path,
    prefix: Option<&Path>,
    fpath: &[PathBuf],
) -> Option<Location> {
    // bash is the one whose search path is baked in at build time and has no
    // way to be asked what it is. fish and elvish read `~/.config` wherever
    // they run, and zsh answers for itself in `zsh_fpath`.
    if let (Shell::Bash, Some(prefix)) = (shell, prefix) {
        return Some(Location {
            path: prefix.join("share/bash-completion/completions/mm"),
            searched: true,
        });
    }
    let (path, searched) = match shell {
        Shell::Bash => (data.join("bash-completion/completions/mm"), true),
        // The one shell with somewhere to choose: see `zsh_location`.
        Shell::Zsh => return Some(zsh_location(data, fpath)),
        Shell::Fish => (config.join("fish/completions/mm.fish"), true),
        Shell::Elvish => (config.join("elvish/lib/mm.elv"), true),
        // PowerShell loads completions from a profile script, not a directory.
        _ => return None,
    };
    Some(Location { path, searched })
}

/// A directory zsh already autoloads from, or the XDG one and a line to add.
fn zsh_location(data: &Path, fpath: &[PathBuf]) -> Location {
    let xdg = data.join("zsh/site-functions/_mm");
    // A script already installed stays where it is. Whoever put it there put
    // its directory on the `fpath`, and an `fpath` line prepends, so writing
    // the new script somewhere else would leave the stale copy as the one zsh
    // autoloads and the update would be invisible.
    if xdg.exists() {
        return Location {
            path: xdg,
            searched: false,
        };
    }
    match fpath.first() {
        Some(dir) => Location {
            path: dir.join("_mm"),
            searched: true,
        },
        None => Location {
            path: xdg,
            searched: false,
        },
    }
}

/// The directories zsh autoloads from, as zsh itself reports them, keeping the
/// ones this account can write in.
///
/// `-f` reads no rc file, so this is the list zsh was built with rather than
/// the one someone's `.zshrc` has already extended: it cannot hang on a prompt
/// framework and it cannot report a directory that only exists while their
/// shell is running. Most systems put nothing under `$HOME` on it, which is why
/// zsh needs an `fpath` line at all, but a prefix installation like Termux and a
/// Homebrew mac both own a directory that is on it, and a script written there
/// works with nothing added to any file.
fn zsh_fpath() -> Vec<PathBuf> {
    // Fully qualified: `Command` here is the subcommand this binary was asked
    // to run.
    let out = std::process::Command::new("zsh")
        .args(["-f", "-c", "print -rl -- $fpath"])
        .stdin(Stdio::null())
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && writable_dir(dir))
        .collect()
}

/// Whether a file could be created in `dir`: it is ours to write in, or it does
/// not exist yet and sits somewhere that is. zsh names a `site-functions`
/// directory that its own packaging has not necessarily created.
fn writable_dir(dir: &Path) -> bool {
    match dir.try_exists() {
        Ok(true) => manymux::update::writable(dir),
        Ok(false) => dir.parent().is_some_and(writable_dir),
        Err(_) => false,
    }
}

/// Write the script for `shell`, saying whether there was none there before.
fn write_completions(shell: Shell, at: &Location) -> Result<bool> {
    let fresh = !at.path.exists();
    if let Some(dir) = at.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::File::create(&at.path)?;
    complete::registration(shell, &mut file)?;
    Ok(fresh)
}

/// What to say after writing a script that was not there before.
fn installed_message(shell: Shell, at: &Location) -> String {
    let mut message = format!("wrote {}", at.path.display());
    if shell != Shell::Zsh || at.searched {
        return message;
    }
    let dir = at.path.parent().unwrap_or(&at.path);
    if zshrc_names(dir) {
        return message;
    }
    message.push_str(&format!(
        "\n\nzsh only reads completions on its fpath. If tab completion does not work, add \
         this to ~/.zshrc:\n\n  fpath=({} $fpath)\n  autoload -Uz compinit && compinit\n",
        dir.display()
    ));
    message
}

/// Whether `.zshrc` already mentions the directory the script went in.
///
/// A heuristic on purpose. Asking properly means `zsh -ic`, which runs the rc
/// file rather than reading it, with whatever that starts on this machine; a
/// file `source`d from `.zshrc` is out of reach either way. All it decides is
/// whether to repeat advice someone has already taken, so being wrong costs a
/// paragraph and nothing else.
fn zshrc_names(dir: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let Ok(rc) = std::fs::read_to_string(home.join(".zshrc")) else {
        return false;
    };
    names_the_directory(&rc, dir, &home)
}

/// The spellings of one directory that all mean it, since the line printed with
/// an absolute path is rarely the line someone types.
fn names_the_directory(rc: &str, dir: &Path, home: &Path) -> bool {
    let mut spellings = vec![dir.display().to_string()];
    if let Ok(rest) = dir.strip_prefix(home) {
        let rest = rest.display();
        spellings.extend([format!("~/{rest}"), format!("$HOME/{rest}")]);
    }
    rc.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| spellings.iter().any(|spelling| line.contains(spelling)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xdg(shell: Shell) -> Location {
        completion_path_in(
            shell,
            Path::new("/home/me/.local/share"),
            Path::new("/home/me/.config"),
            None,
            &[],
        )
        .expect("a path for this shell")
    }

    /// zsh is asked where it searches, so what it answers is an argument.
    fn searching(shell: Shell, fpath: &[&str]) -> Location {
        let fpath: Vec<PathBuf> = fpath.iter().map(PathBuf::from).collect();
        completion_path_in(
            shell,
            Path::new("/home/me/.local/share"),
            Path::new("/home/me/.config"),
            None,
            &fpath,
        )
        .expect("a path for this shell")
    }

    fn termux(shell: Shell) -> Location {
        completion_path_in(
            shell,
            Path::new("/data/data/com.termux/files/home/.local/share"),
            Path::new("/data/data/com.termux/files/home/.config"),
            Some(Path::new("/data/data/com.termux/files/usr")),
            &[PathBuf::from(
                "/data/data/com.termux/files/usr/share/zsh/site-functions",
            )],
        )
        .expect("a path for this shell")
    }

    #[test]
    fn the_xdg_directories_are_where_a_script_goes() {
        assert_eq!(
            xdg(Shell::Bash).path,
            Path::new("/home/me/.local/share/bash-completion/completions/mm")
        );
        assert_eq!(
            xdg(Shell::Zsh).path,
            Path::new("/home/me/.local/share/zsh/site-functions/_mm")
        );
        assert_eq!(
            xdg(Shell::Fish).path,
            Path::new("/home/me/.config/fish/completions/mm.fish")
        );
    }

    /// Termux is a prefix installation on a system with no `/usr`, so its bash
    /// searches under `$PREFIX` and never under `$HOME`. A script in the XDG
    /// directory there is a file nothing reads.
    #[test]
    fn bash_moves_under_the_termux_prefix() {
        assert_eq!(
            termux(Shell::Bash).path,
            Path::new("/data/data/com.termux/files/usr/share/bash-completion/completions/mm")
        );
    }

    /// zsh needs no rule of its own for the prefix: the directory it searches
    /// there is one it names itself, and one nobody has to be told about.
    #[test]
    fn zsh_takes_a_directory_it_says_it_searches() {
        let at = termux(Shell::Zsh);
        assert_eq!(
            at.path,
            Path::new("/data/data/com.termux/files/usr/share/zsh/site-functions/_mm")
        );
        assert!(at.searched);
    }

    /// The first entry wins, because that is the one zsh reads first.
    #[test]
    fn the_xdg_directory_is_where_zsh_goes_when_it_searches_nowhere_writable() {
        assert_eq!(
            searching(Shell::Zsh, &["/usr/local/share/zsh/site-functions"]).path,
            Path::new("/usr/local/share/zsh/site-functions/_mm")
        );
        let none = searching(Shell::Zsh, &[]);
        assert_eq!(
            none.path,
            Path::new("/home/me/.local/share/zsh/site-functions/_mm")
        );
        assert!(!none.searched);
    }

    /// An `fpath` line prepends, so the directory someone added for the script
    /// they already have is read before anything zsh searches by itself.
    /// Installing the new one elsewhere would leave the stale copy in charge.
    #[test]
    fn a_script_already_installed_is_not_moved() {
        let dir = std::env::temp_dir().join(format!("manymux-zsh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        std::fs::create_dir_all(data.join("zsh/site-functions")).expect("a data directory");
        std::fs::write(data.join("zsh/site-functions/_mm"), "old").expect("a script to find");

        let at = completion_path_in(
            Shell::Zsh,
            &data,
            &dir.join("config"),
            None,
            &[PathBuf::from("/usr/local/share/zsh/site-functions")],
        )
        .expect("a path for zsh");
        assert_eq!(at.path, data.join("zsh/site-functions/_mm"));
        assert!(!at.searched);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Advice someone has already taken is noise. The line they typed is
    /// rarely the absolute one that was printed at them.
    #[test]
    fn a_zshrc_that_already_names_the_directory_is_left_alone() {
        let home = Path::new("/home/me");
        let dir = Path::new("/home/me/.local/share/zsh/site-functions");
        for rc in [
            "fpath=(/home/me/.local/share/zsh/site-functions $fpath)",
            "fpath=(~/.local/share/zsh/site-functions $fpath)",
            "fpath=($HOME/.local/share/zsh/site-functions $fpath)",
        ] {
            assert!(names_the_directory(rc, dir, home), "{rc}");
        }
        assert!(!names_the_directory("fpath=(~/.zfunc $fpath)", dir, home));
        // Ours is the directory in the comment, and a comment runs nothing.
        assert!(!names_the_directory(
            "  # fpath=(~/.local/share/zsh/site-functions $fpath)",
            dir,
            home
        ));
    }

    /// fish reads its own config directory wherever it runs, Termux included,
    /// so only the two whose search path is baked into the prefix move.
    #[test]
    fn fish_reads_its_own_config_directory_under_termux() {
        assert_eq!(
            termux(Shell::Fish).path,
            Path::new("/data/data/com.termux/files/home/.config/fish/completions/mm.fish")
        );
    }

    /// The line is advice about a directory zsh does not search. Printing it
    /// for one that is already on the `fpath` sends someone to fix nothing.
    #[test]
    fn only_a_directory_zsh_does_not_search_gets_the_fpath_line() {
        let hint = "fpath=(";
        assert!(installed_message(Shell::Zsh, &xdg(Shell::Zsh)).contains(hint));
        assert!(!installed_message(Shell::Zsh, &termux(Shell::Zsh)).contains(hint));
        assert!(!installed_message(Shell::Bash, &xdg(Shell::Bash)).contains(hint));
    }

    /// `mm update` installs a script that is missing and rewrites one that is
    /// not, and only the first of those is news.
    #[test]
    fn a_script_is_installed_once_and_rewritten_afterwards() {
        let dir = std::env::temp_dir().join(format!("manymux-completions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let at = completion_path_in(
            Shell::Bash,
            &dir.join("data"),
            &dir.join("config"),
            None,
            &[],
        )
        .expect("a path for bash");

        assert!(write_completions(Shell::Bash, &at).unwrap());
        let script = std::fs::read_to_string(&at.path).expect("the script was written");
        assert!(script.contains("COMPLETE"), "{script}");

        assert!(!write_completions(Shell::Bash, &at).unwrap());
        assert_eq!(std::fs::read_to_string(&at.path).unwrap(), script);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
