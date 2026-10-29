use std::process::{Command, Output};

fn installer_function(script: &str) -> Output {
    let definitions = include_str!("../install.sh")
        .strip_suffix("main \"$@\"\n")
        .expect("the installer calls main after defining its functions");
    Command::new("sh")
        .args(["-c", &format!("{definitions}\n{script}")])
        .output()
        .expect("a POSIX shell is available")
}

#[test]
fn the_installer_honors_an_explicit_checksum_override() {
    let output = installer_function(
        r#"
curl() {
    for last do :; done
    case "$last" in
        *.sha256) printf '%064d  fake-binary\n' 0 > "$last" ;;
        *) printf 'fake binary\n' > "$last" ;;
    esac
}
sleep() { :; }
SKIP_VERIFY=1
fetch_and_verify 'https://example.invalid/mm'
"#,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("without checksum verification"));
}

#[test]
fn an_installed_path_keeps_shell_metacharacters_literal() {
    let output = installer_function(
        r#"
OS=macos
SHELL=zsh
ZDOTDIR="$TMP"
INSTALL_DIR="$TMP/bin with \"quotes\" \$value \`command\` \\slash"
ensure_path
expected="$INSTALL_DIR:$PATH"
actual=$(sh -c '. "$1"; printf "%s" "$PATH"' sh "$ZDOTDIR/.zshenv")
[ "$actual" = "$expected" ]
"#,
    );
    assert!(output.status.success(), "{output:?}");
}
