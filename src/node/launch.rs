//! Restricted parent jobs cannot release a child. WMI starts the node under
//! the calling account outside that job, without elevating or installing a service.
use std::env;
use std::io;
use std::mem::size_of;
use std::path::Path;
use std::process::Stdio;
use std::ptr::null_mut;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::time::timeout;
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::ssh::bootstrap::powershell;

pub(super) fn job_forbids_breakaway() -> Result<bool> {
    let mut in_job = 0;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the pseudo handle is valid and both output buffers live through
    // the calls. A null job handle asks about the current process's job.
    unsafe {
        if IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) == 0 {
            return Err(io::Error::last_os_error()).context("checking the parent job");
        }
        if in_job == 0 {
            return Ok(false);
        }
        if QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())?,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error()).context("reading parent job limits");
        }
    }
    Ok(limits.BasicLimitInformation.LimitFlags
        & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)
        == 0)
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(super) async fn outside_job(binary: &Path, socket: &Path) -> Result<()> {
    let mut script = String::from("$ErrorActionPreference='Stop'; ");
    // WMI does not inherit the caller's environment. Forward only the paths
    // and settings the node needs, never credentials from unrelated tools.
    for key in [
        "MM_CONFIG_DIR",
        "MM_LOG",
        "MM_SSH",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "COMSPEC",
        "PATH",
        "HOMEDRIVE",
        "HOMEPATH",
        "SHELL",
    ] {
        if let Ok(value) = env::var(key) {
            script.push_str(&format!("$env:{key}={}; ", quoted(&value)));
        }
    }
    script.push_str(&format!(
        "& {} --socket {} daemon; exit $LASTEXITCODE",
        quoted(&binary.to_string_lossy()),
        quoted(&socket.to_string_lossy()),
    ));
    let launcher = format!(
        "$ErrorActionPreference='Stop'; try {{ $r=Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine={}}}; if ($r.ReturnValue -ne 0) {{ throw ('Win32_Process.Create returned ' + $r.ReturnValue) }} }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}",
        quoted(&powershell(&script)),
    );
    let encoded = powershell(&launcher);
    let args = encoded.split_whitespace().skip(1);
    let output = timeout(
        Duration::from_secs(10),
        Command::new("powershell.exe")
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("starting the node outside the parent job timed out")??;
    if !output.status.success() {
        bail!(
            "starting the node outside the parent job: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
