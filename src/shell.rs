//! Turning an argv into one line a POSIX shell reads back as those same words.
//!
//! Here rather than under `node` because both ends need it and only one of them
//! is a desktop build. The node joins a spawn's argv to hand it to `shell -lc`,
//! and `mm checkpoint show` prints the line a restore will hand over: rendered
//! by any other quoter, the printed line would be a second opinion about what
//! is going to run, and the two would part company the first time either was
//! touched.

/// Join argv into one shell command, so an argument with spaces or quotes in it
/// survives the trip through `-c`.
pub fn join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote an argument for a POSIX shell. Everything inside single quotes is
/// literal, so the only thing needing care is a single quote itself: close the
/// quoting, emit an escaped one, open it again.
pub fn quote(arg: &str) -> String {
    let safe = |b: &u8| b.is_ascii_alphanumeric() || b"-_./:=@,+".contains(b);
    if !arg.is_empty() && arg.as_bytes().iter().all(safe) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_spaces_and_quotes() {
        assert_eq!(join(&["echo".into(), "a b".into()]), "echo 'a b'");
        assert_eq!(
            join(&["sh".into(), "-c".into(), "printf 'hi'".into()]),
            r#"sh -c 'printf '\''hi'\'''"#
        );
        // A plain word is left alone, so the common case stays readable.
        assert_eq!(join(&["/usr/bin/vim".into()]), "/usr/bin/vim");
    }
}
