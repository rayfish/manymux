//! Read-only platform discovery precedes requests, so a Windows shell's exit
//! code 1 can never be mistaken for permission to retry a side effect.
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::time::timeout;

use super::{Tty, base};

#[derive(Clone, Copy)]
pub(crate) enum Platform {
    Unix,
    Windows,
}

pub(crate) struct Detected {
    pub(crate) platform: Platform,
    pub(crate) missing: bool,
}

pub(crate) async fn detect(host: &str, quiet: bool) -> Result<Detected> {
    let unix = probe(host, "uname -s", quiet).await?;
    if unix.status.code() == Some(255) {
        bail!(
            "ssh {host}: {}",
            String::from_utf8_lossy(&unix.stderr).trim()
        );
    }
    if unix.status.success() {
        let os = String::from_utf8_lossy(&unix.stdout);
        match os.trim() {
            "Linux" | "Darwin" | "FreeBSD" | "OpenBSD" | "NetBSD" => {
                return Ok(Detected {
                    platform: Platform::Unix,
                    missing: false,
                });
            }
            name if name.starts_with("MINGW")
                || name.starts_with("MSYS")
                || name.starts_with("CYGWIN") => {}
            other => bail!("{host}: unsupported remote platform {other:?}"),
        }
    }
    let script = format!(
        "$manymuxProbe=$true;\n{}",
        include_str!("windows-agent.ps1")
    );
    let windows = probe(host, &powershell(&script), quiet).await?;
    if windows.status.success() {
        match windows.stdout.as_slice() {
            b"manymux-windows" | b"manymux-windows-missing" => {
                return Ok(Detected {
                    platform: Platform::Windows,
                    missing: windows.stdout == b"manymux-windows-missing",
                });
            }
            _ => {}
        }
    }
    bail!(
        "{host}: cannot identify the remote platform: {}",
        String::from_utf8_lossy(&windows.stderr).trim()
    )
}

async fn probe(host: &str, script: &str, quiet: bool) -> Result<Output> {
    let mut command = base(Tty::No);
    if quiet {
        command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=1"]);
    }
    command
        .arg("--")
        .arg(host)
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    timeout(Duration::from_secs(10), command.output())
        .await
        .with_context(|| format!("identifying {host} timed out"))?
        .with_context(|| format!("identifying {host}"))
}

pub(crate) fn powershell(script: &str) -> String {
    let script = format!("$ProgressPreference='SilentlyContinue';\n{script}");
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {}",
        STANDARD.encode(bytes)
    )
}

pub(super) fn agent() -> String {
    powershell(include_str!("windows-agent.ps1"))
}

pub(super) fn installer() -> String {
    powershell(
        "$ErrorActionPreference='Stop'; try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; $script = (Invoke-WebRequest -UseBasicParsing 'https://raw.githubusercontent.com/rayfish/manymux/master/install.ps1').Content; & ([ScriptBlock]::Create($script)); } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_commands_encode_utf16_without_exposing_shell_metacharacters() {
        let script = "[Console]::WriteLine('a b & $HOME ` é')";
        let command = powershell(script);
        let encoded = command
            .strip_prefix("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ")
            .unwrap();
        assert!(!encoded.contains(char::is_whitespace));
        let bytes = STANDARD.decode(encoded).unwrap();
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            format!("$ProgressPreference='SilentlyContinue';\n{script}")
        );
    }
}
