//! Windows account identity and native handles used by the local transport.
//! Pipe names use the token SID, so console and SSH logins reach the same node.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::null_mut;
#[cfg(feature = "desktop")]
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn token_sid(process: HANDLE) -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: process is a live process handle; token is writable storage.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned an owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    // SAFETY: the null buffer requests the required size only.
    unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
    }
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and holds at least size bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful TokenUser query writes a TOKEN_USER and its SID.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    // SAFETY: the SID lives in buffer; the API allocates text on success.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated UTF-16 allocation.
    let result = unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        result
    };
    Ok(result)
}

fn sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle, not to be closed.
    token_sid(unsafe { GetCurrentProcess() })
}

pub(crate) fn pipe_name(path: &Path) -> io::Result<String> {
    // Stable across builds, unlike DefaultHasher. The SID is the security
    // boundary; this hash only distinguishes explicit --socket names.
    let mut hash = 0xcbf29ce484222325u64;
    for unit in path.as_os_str().encode_wide() {
        for byte in unit.to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    Ok(format!(r"\\.\pipe\manymux-{}-{hash:016x}", sid()?))
}

#[cfg(feature = "desktop")]
pub(crate) fn pipe_server(
    path: &Path,
    first: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let name = pipe_name(path)?;
    let descriptor: Vec<u16> = format!("D:P(A;;GA;;;{})\0", sid()?)
        .encode_utf16()
        .collect();
    let mut security = null_mut();
    // SAFETY: descriptor is NUL-terminated; security receives the allocation.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut security,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security,
        bInheritHandle: 0,
    };
    // SAFETY: attributes and its descriptor remain live until creation returns.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(&name, (&raw mut attributes).cast())
    };
    // SAFETY: the descriptor is a LocalAlloc allocation, copied by CreateNamedPipe.
    unsafe {
        LocalFree(security);
    }
    result
}

pub(crate) fn verify_server(pipe: &impl AsRawHandle) -> io::Result<()> {
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let mut pid = 0;
    // SAFETY: pipe owns a valid named pipe handle and pid is writable.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: only query access is requested, with no inherited handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let owner = token_sid(process);
    // SAFETY: process was returned by OpenProcess and is no longer used.
    unsafe {
        CloseHandle(process);
    }
    if owner? != sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the pipe belongs to another account",
        ));
    }
    Ok(())
}

pub(crate) fn random(bytes: &mut [u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    // SAFETY: bytes is writable for len bytes; this flag uses the system RNG.
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "system random generator failed: {status}"
        )));
    }
    Ok(())
}

pub(crate) fn hostname() -> io::Result<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsHostname, GetComputerNameExW,
    };
    let mut buffer = [0u16; 256];
    let mut len = buffer.len() as u32;
    // SAFETY: buffer has len UTF-16 code units and len is writable.
    if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(String::from_utf16_lossy(&buffer[..len as usize]))
}

#[cfg(feature = "desktop")]
struct ConsoleModes {
    input: u32,
    output: u32,
}

#[cfg(feature = "desktop")]
static CONSOLE_MODES: Mutex<Option<ConsoleModes>> = Mutex::new(None);

#[cfg(feature = "desktop")]
pub(crate) fn enable_terminal() -> io::Result<()> {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    let mut saved = crate::lock::held(&CONSOLE_MODES);
    // SAFETY: standard handles are borrowed from the process and both mode
    // buffers are writable. Restore input if enabling output fails.
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut modes = ConsoleModes {
            input: 0,
            output: 0,
        };
        if GetConsoleMode(input, &mut modes.input) == 0
            || GetConsoleMode(output, &mut modes.output) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let raw = (modes.input & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if SetConsoleMode(input, raw) == 0 {
            return Err(io::Error::last_os_error());
        }
        if SetConsoleMode(
            output,
            modes.output | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT,
        ) == 0
        {
            let error = io::Error::last_os_error();
            SetConsoleMode(input, modes.input);
            return Err(error);
        }
        *saved = Some(modes);
    }
    Ok(())
}

#[cfg(feature = "desktop")]
pub(crate) fn restore_terminal() {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    if let Some(modes) = crate::lock::held(&CONSOLE_MODES).take() {
        // SAFETY: the standard handles remain borrowed. Cleanup is best effort
        // because this also runs while unwinding after a console closes.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), modes.input);
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), modes.output);
        }
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn flush_input() {
    use windows_sys::Win32::System::Console::{
        FlushConsoleInputBuffer, GetStdHandle, STD_INPUT_HANDLE,
    };
    // SAFETY: the standard input handle is borrowed; a redirected input may fail.
    unsafe {
        FlushConsoleInputBuffer(GetStdHandle(STD_INPUT_HANDLE));
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn input_ready() -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    // SAFETY: wait only observes the borrowed standard input handle.
    unsafe { WaitForSingleObject(GetStdHandle(STD_INPUT_HANDLE), 30) == WAIT_OBJECT_0 }
}
