//! The clipboard of the machine you are sitting at.
//!
//! Only the client end has one. A session runs on a machine you reached over
//! ssh, and nothing about a terminal carries an image: pressing paste in your
//! terminal sends the *text* on the clipboard and nothing at all when what is
//! on it is a screenshot. So the client reads the clipboard here, on the
//! machine with the screen, and ships the bytes to the host, which writes them
//! down and pastes the path.
//!
//! There is no library behind this on purpose. Every platform already ships a
//! program that does the job, manymux already shells out to ssh, and a
//! clipboard crate would drag in the whole windowing stack of three platforms
//! for something a pipe answers.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Result, anyhow, bail};
use tokio::process::Command;

/// An image on the clipboard, ready to go over the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub data: Vec<u8>,
    /// File extension, from the bytes rather than from what the clipboard
    /// claimed they were.
    pub kind: &'static str,
}

/// Image types worth asking a clipboard for, best first. Everything here is
/// something a program on the far end has a chance of reading; a TIFF or a BMP
/// would arrive as a file nothing opens, so they are left out.
const WANTED: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Whether the client intercepts the paste key at all.
///
/// `MM_PASTE=off` gives Ctrl-V back to the session, for anyone who wants it for
/// vim's visual block more than they want images.
pub fn enabled() -> bool {
    match std::env::var("MM_PASTE") {
        Ok(value) => is_on(&value),
        Err(_) => true,
    }
}

fn is_on(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "no" | "false"
    )
}

/// The image on the clipboard, if there is one.
///
/// `Ok(None)` is the ordinary answer when the clipboard holds text, or holds
/// nothing, or this machine has no clipboard to read: the caller passes the key
/// through and says nothing. An error is something the user could fix, such as
/// a missing helper program, and is worth showing them.
pub async fn image() -> Result<Option<Image>> {
    match tool() {
        Some(Tool::Mac) => mac().await,
        Some(Tool::Wayland) => unix("wl-paste", &["--list-types"], wayland).await,
        Some(Tool::X11) => {
            unix(
                "xclip",
                &["-selection", "clipboard", "-t", "TARGETS", "-o"],
                x11,
            )
            .await
        }
        None => Ok(None),
    }
}

/// Which clipboard this machine has, if any.
enum Tool {
    Mac,
    Wayland,
    X11,
}

fn tool() -> Option<Tool> {
    if cfg!(target_os = "macos") {
        return Some(Tool::Mac);
    }
    // A session opened from a plain console, or from inside another ssh, has
    // neither. That is not a failure, it is a machine with no clipboard.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Some(Tool::Wayland);
    }
    std::env::var_os("DISPLAY").is_some().then_some(Tool::X11)
}

fn wayland(kind: &str) -> Vec<String> {
    vec!["--no-newline".into(), "--type".into(), kind.into()]
}

fn x11(kind: &str) -> Vec<String> {
    vec![
        "-selection".into(),
        "clipboard".into(),
        "-t".into(),
        kind.into(),
        "-o".into(),
    ]
}

/// Wayland and X11, which differ only in the program and its arguments: both
/// list what is on the clipboard, then hand over one type of it.
async fn unix(
    program: &str,
    list: &[&str],
    args: fn(&str) -> Vec<String>,
) -> Result<Option<Image>> {
    let Some(listed) = run(program, list).await? else {
        return Ok(None);
    };
    let offered: Vec<String> = String::from_utf8_lossy(&listed)
        .lines()
        .map(|line| line.trim().to_ascii_lowercase())
        .collect();
    let has = |kind: &str| offered.iter().any(|offer| offer == kind);

    for kind in WANTED {
        if !has(kind) {
            continue;
        }
        if let Some(data) = run(program, &borrowed(&args(kind))).await? {
            return as_image(data);
        }
    }

    // A file manager puts the file's location on the clipboard rather than its
    // contents, which is how most people copy an image they already have.
    if has("text/uri-list")
        && let Some(list) = run(program, &borrowed(&args("text/uri-list"))).await?
    {
        return copied_file(&String::from_utf8_lossy(&list)).await;
    }
    Ok(None)
}

fn borrowed(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// macOS, where the clipboard is AppleScript's rather than a program's.
///
/// `the clipboard as «class PNGf»` answers with `«data PNGf89504…»`, the bytes
/// in hex. Roundabout, but it is on every Mac, where `pbpaste` cannot do images
/// at all and `pngpaste` is something you have to go and install.
///
/// `clipboard info` first, and always exactly one call when the answer is no,
/// because this runs on a key someone may be pressing for something else and a
/// run of osascript is a tenth of a second of a keyboard that has stopped.
async fn mac() -> Result<Option<Image>> {
    let Some(info) = run("osascript", &["-e", "clipboard info"]).await? else {
        return Ok(None);
    };
    let info = String::from_utf8_lossy(&info);

    if let Some(class) = mac_class(&info) {
        let script = format!("the clipboard as «class {class}»");
        if let Some(out) = run("osascript", &["-e", &script]).await?
            && let Some(data) = unhex(&String::from_utf8_lossy(&out))
        {
            return as_image(data);
        }
    }
    // A file copied in Finder, which is a location and not an image.
    if !info.contains("furl") {
        return Ok(None);
    }
    let script = "POSIX path of (the clipboard as «class furl»)";
    match run("osascript", &["-e", script]).await? {
        Some(path) => copied_file(&String::from_utf8_lossy(&path)).await,
        None => Ok(None),
    }
}

/// The best image flavour on the clipboard, out of what `clipboard info` says
/// is there. A screenshot offers several, and TIFF is usually one of them,
/// which is exactly why the choice is made here rather than by asking in turn.
fn mac_class(info: &str) -> Option<&'static str> {
    ["PNGf", "JPEG", "GIFf"]
        .into_iter()
        .find(|class| info.contains(class))
}

/// Read an image whose *path* was on the clipboard.
///
/// Only for something that is already an image: a copied PDF or folder is not
/// an image paste, and reading it would send the far end a file it cannot use.
async fn copied_file(list: &str) -> Result<Option<Image>> {
    let Some(path) = list.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return Ok(None);
    };
    let path = unfile(path);
    if !matches!(
        extension(Path::new(&path)).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    ) {
        return Ok(None);
    }
    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if size as usize > MAX {
        bail!(
            "{path} is {}, over the {} paste limit",
            mb(size as usize),
            mb(MAX)
        );
    }
    match std::fs::read(&path) {
        Ok(data) => as_image(data),
        // Not ours to complain about: the clipboard may point at a file on a
        // volume that has since gone away, and the key still has to work.
        Err(_) => Ok(None),
    }
}

/// The largest image worth carrying, kept in step with the protocol's own cap
/// so the refusal happens here, where it can name the file, rather than on the
/// far end.
const MAX: usize = crate::proto::MAX_PASTE;

fn as_image(data: Vec<u8>) -> Result<Option<Image>> {
    let Some(kind) = sniff(&data) else {
        return Ok(None);
    };
    if data.len() > MAX {
        bail!(
            "the clipboard holds {}, over the {} paste limit",
            mb(data.len()),
            mb(MAX)
        );
    }
    Ok(Some(Image { data, kind }))
}

/// What the bytes actually are. The clipboard's own label is a claim, and a
/// filename's extension is a guess; the first few bytes are neither.
fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpg");
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if data.starts_with(b"RIFF") && data.len() > 12 && &data[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// A size to show someone, which is a round number of megabytes and not a byte
/// count they have to count the digits of.
pub fn mb(bytes: usize) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb < 0.1 {
        return format!("{} KB", bytes.div_ceil(1024));
    }
    format!("{mb:.1} MB")
}

/// `file:///home/me/a%20shot.png` as a path. Anything that is not a `file:`
/// URL is handed back unchanged, since some clipboards put a bare path there.
fn unfile(entry: &str) -> String {
    let path = entry.strip_prefix("file://").unwrap_or(entry);
    // A `file://host/path` from another machine is not ours to read; the
    // localhost form is the only one anyone puts on a clipboard.
    let path = path.strip_prefix("localhost").unwrap_or(path);
    percent_decoded(path)
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = hex_pair(bytes[i + 1], bytes[i + 2])
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The lowercase extension of a path.
fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_string_lossy().to_ascii_lowercase())
}

/// The bytes out of AppleScript's `«data PNGf89504E47…»`.
fn unhex(text: &str) -> Option<Vec<u8>> {
    // The class code sits between the marker and the hex, and is four
    // characters however the four are spelled.
    let start = text.find("«data ")? + "«data ".len() + 4;
    let end = text[start..].find('»')? + start;
    let digits = &text.as_bytes()[start..end];
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| hex_pair(pair[0], pair[1]))
        .collect()
}

fn hex_pair(high: u8, low: u8) -> Option<u8> {
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    Some(digit(high)? << 4 | digit(low)?)
}

/// Run a clipboard program and hand back what it printed.
///
/// `None` is a non-zero exit, which is how all three say the clipboard holds
/// nothing of the kind asked for. A program that is not installed is an error,
/// because that one the user can do something about.
async fn run(program: &str, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!("{program} is not installed{}", install(program))
            }
            _ => anyhow!("running {program}: {e}"),
        })?;
    Ok(output.status.success().then_some(output.stdout))
}

/// Where the missing program comes from, since the package rarely shares its
/// name.
fn install(program: &str) -> &'static str {
    match program {
        "wl-paste" => ", and comes with wl-clipboard",
        "xclip" => ", and comes with xclip",
        _ => "",
    }
}

/// Text put on the clipboard of the terminal showing this, over OSC 52.
///
/// The other direction from everything above, and it cannot use any of it: the
/// programs up there talk to the clipboard of the machine the client is running
/// on, which is the right one when a person is sitting at that machine and the
/// wrong one whenever they are not. Somebody ssh'd into a box and running `mm`
/// there would be copying to a clipboard nobody can paste from. The terminal
/// always belongs to the person, so the copy goes to the terminal and the
/// terminal decides.
///
/// Deciding is what most of them do: this is a program on the far end of a pipe
/// asking to write your clipboard, so terminals ship with it off (iTerm2:
/// Settings > General > Selection). There is nothing to check and nothing that
/// comes back, so a refusal is silent, and the only honest answer is to say in
/// the docs where the switch is.
pub fn to_terminal(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Base64, because OSC 52 takes nothing else.
///
/// Twenty lines rather than a dependency, for the same reason the rest of this
/// module shells out rather than linking a clipboard library: this is the whole
/// of what would be used, and it has not changed since 1987.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let bits = u32::from_be_bytes([0, block[0], block[1], block[2]]);
        for i in 0..4 {
            // The last block is padded with `=` for the bytes that were not
            // there, which is what tells a decoder how long it was.
            if i > chunk.len() {
                out.push('=');
                continue;
            }
            let sextet = (bits >> (18 - 6 * i)) & 0b11_1111;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for `wl-paste`, since a test has no clipboard and a headless
    /// machine has no way to put anything on one. It answers the same two
    /// questions the real one does.
    fn fake_tool(name: &str, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Everything the reader does with what a clipboard offers, in one test:
    /// the scripts are all written before any of them runs, because a file
    /// still open for writing in one thread cannot be exec'd from another.
    #[tokio::test]
    async fn the_reader_takes_an_image_a_file_or_neither() {
        let image = std::env::temp_dir().join("mm-test-copied.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nfrom disk").unwrap();

        let has_image = fake_tool(
            "mm-test-clipboard-image",
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *--list-types*) printf 'text/plain\\nimage/png\\n' ;;\n\
             *image/png*) printf '\\211PNG\\r\\n\\032\\nbody' ;;\n\
             *) exit 1 ;;\n\
             esac\n",
        );
        let has_text = fake_tool(
            "mm-test-clipboard-text",
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *--list-types*) printf 'text/plain\\nUTF8_STRING\\n' ;;\n\
             *) exit 1 ;;\n\
             esac\n",
        );
        let has_file = fake_tool(
            "mm-test-clipboard-file",
            &format!(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                 *--list-types*) printf 'text/uri-list\\n' ;;\n\
                 *uri-list*) printf 'file://{}' ;;\n\
                 *) exit 1 ;;\n\
                 esac\n",
                image.display()
            ),
        );

        let found = read(&has_image).await.expect("an image");
        assert_eq!(found.kind, "png");
        assert_eq!(found.data, b"\x89PNG\r\n\x1a\nbody");

        // The common case, and the one that has to stay quiet: the key belongs
        // to the session unless there is genuinely a picture to send.
        assert!(read(&has_text).await.is_none(), "text is not an image");

        // Copying the file in a file manager rather than the image itself:
        // what lands on the clipboard is where it lives.
        let found = read(&has_file).await.expect("the copied file");
        assert_eq!(found.data, b"\x89PNG\r\n\x1a\nfrom disk");

        for path in [has_image, has_text, has_file, image] {
            std::fs::remove_file(path).unwrap();
        }
    }

    async fn read(tool: &std::path::Path) -> Option<Image> {
        unix(tool.to_str().unwrap(), &["--list-types"], wayland)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_missing_helper_is_worth_saying_out_loud() {
        // Not silence: someone whose clipboard tool is not installed would
        // otherwise press the key forever and never learn why.
        let e = unix("mm-no-such-clipboard-tool", &["--list-types"], wayland)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("not installed"), "{e:#}");
    }

    #[test]
    fn the_bytes_say_what_the_image_is() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a..."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        // Text on the clipboard is not an image, and neither is a truncated
        // header that only looks like the start of one.
        assert_eq!(sniff(b"hello"), None);
        assert_eq!(sniff(b"RIFF"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn the_mac_clipboard_is_asked_for_one_flavour_of_what_it_says_it_has() {
        // What a screenshot puts there: several flavours, TIFF among them,
        // which is why the best one is picked rather than tried in turn.
        let shot = "«class PNGf», 219034, «class TIFF», 1490112";
        assert_eq!(mac_class(shot), Some("PNGf"));
        assert_eq!(mac_class("«class JPEG», 4021"), Some("JPEG"));
        // Text, and a copied file, which is a path and not an image.
        assert_eq!(mac_class("string, 12, «class ut16», 26"), None);
        assert_eq!(mac_class("«class furl», 62"), None);
    }

    #[test]
    fn applescript_hex_comes_back_as_bytes() {
        let png = "«data PNGf89504E470D0A1A0A»\n";
        assert_eq!(unhex(png).unwrap(), b"\x89PNG\r\n\x1a\n");
        // Lowercase digits, which osascript also emits.
        assert_eq!(unhex("«data JPEGffd8ff»").unwrap(), vec![0xff, 0xd8, 0xff]);
        // Anything that is not that shape is not data.
        assert_eq!(unhex("hello"), None);
        assert_eq!(unhex("«data PNGf»"), None);
        assert_eq!(
            unhex("«data PNGf89504E47»x"),
            Some(vec![0x89, 0x50, 0x4e, 0x47])
        );
    }

    #[test]
    fn a_copied_files_url_becomes_a_path() {
        assert_eq!(unfile("file:///home/me/shot.png"), "/home/me/shot.png");
        assert_eq!(
            unfile("file://localhost/Users/me/a%20shot.png"),
            "/Users/me/a shot.png"
        );
        // A bare path, which some clipboards put there instead.
        assert_eq!(unfile("/tmp/shot.png"), "/tmp/shot.png");
        // A stray percent is a percent, not the start of an escape.
        assert_eq!(unfile("/tmp/100%.png"), "/tmp/100%.png");
    }

    #[test]
    fn sizes_read_as_sizes() {
        assert_eq!(mb(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(mb(40 * 1024), "40 KB");
        assert_eq!(mb(16 << 20), "16.0 MB");
    }

    /// Against the vectors in RFC 4648, since the padding is the half that is
    /// easy to get wrong and a terminal answers a bad encoding with silence.
    #[test]
    fn base64_encodes_what_rfc_4648_says_it_does() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
        // And something with bytes above ASCII in it, since a selection is
        // whatever was on the screen.
        assert_eq!(base64("café".as_bytes()), "Y2Fmw6k=");
    }

    #[test]
    fn a_copy_goes_to_the_terminal_as_an_osc_52() {
        assert_eq!(to_terminal("foo"), "\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn the_paste_key_can_be_given_back_to_the_session() {
        // Every spelling of no, because someone who has turned this off and
        // finds Ctrl-V still being taken will not try a second spelling.
        for off in ["off", "OFF", "0", "no", "false", " off "] {
            assert!(!is_on(off), "MM_PASTE={off:?} should turn it off");
        }
        for on in ["on", "1", "yes", ""] {
            assert!(is_on(on), "MM_PASTE={on:?} should leave it on");
        }
    }
}
