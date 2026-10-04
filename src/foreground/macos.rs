//! Native reads for the process holding a macOS session's terminal.

use std::mem::{MaybeUninit, size_of};
use std::path::Path;
use std::{ptr, str};

use libc::{proc_bsdinfo, proc_vnodepathinfo};

use super::{Foreground, in_front};

pub fn of(leader: u32) -> Foreground {
    let front = in_front(leader, foreground_group(leader));
    Foreground {
        cwd: cwd_of(front),
        argv: argv_of(front),
    }
}

fn foreground_group(pid: u32) -> Option<i32> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = MaybeUninit::<proc_bsdinfo>::uninit();
    // Safe: libproc receives a writable buffer of exactly the requested size.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<proc_bsdinfo>() as i32,
        )
    };
    if read as usize != size_of::<proc_bsdinfo>() {
        return None;
    }
    // Safe: libproc filled the complete C struct, which contains integer fields.
    Some(unsafe { info.assume_init() }.e_tpgid as i32)
}

fn cwd_of(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok()?;
    let mut info = MaybeUninit::<proc_vnodepathinfo>::uninit();
    // Safe: the buffer matches the flavor's C struct and is writable in full.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<proc_vnodepathinfo>() as i32,
        )
    };
    if read as usize != size_of::<proc_vnodepathinfo>() {
        return None;
    }
    // Safe: the complete struct was filled. Search within the fixed array so
    // even a path without a terminator cannot be read past its buffer.
    let info = unsafe { info.assume_init() };
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|b| *b as u8)
        .collect();
    let end = bytes.iter().position(|b| *b == 0)?;
    let path = str::from_utf8(&bytes[..end]).ok()?;
    let path = Path::new(path);
    path.is_absolute().then_some(())?;
    path.is_dir()
        .then(|| path.to_str().map(str::to_owned))
        .flatten()
}

fn argv_of(pid: u32) -> Vec<String> {
    let Ok(pid) = i32::try_from(pid) else {
        return Vec::new();
    };
    let mut limit = 0i32;
    let mut size = size_of::<i32>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // Safe: both MIB and output are live buffers of the sizes passed to sysctl.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            ptr::from_mut(&mut limit).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if status != 0 || limit <= 0 {
        return Vec::new();
    }
    let mut bytes = vec![0u8; limit as usize];
    let mut size = bytes.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // Safe: sysctl writes at most `size` bytes into the allocated buffer.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size > bytes.len() {
        return Vec::new();
    }
    args_from(&bytes[..size])
}

fn args_from(bytes: &[u8]) -> Vec<String> {
    let Some(count) = bytes
        .get(..4)
        .and_then(|raw| raw.try_into().ok())
        .map(i32::from_ne_bytes)
    else {
        return Vec::new();
    };
    if count <= 0 {
        return Vec::new();
    }
    let Some(rest) = bytes.get(4..) else {
        return Vec::new();
    };
    // The executable path is followed by padding, then exactly argc strings.
    // Stop at argc so the environment cannot become part of the command.
    let Some(end) = rest.iter().position(|b| *b == 0) else {
        return Vec::new();
    };
    let rest = &rest[end..];
    let start = rest.iter().position(|b| *b != 0).unwrap_or(rest.len());
    let mut rest = &rest[start..];
    let mut args = Vec::new();
    for _ in 0..count {
        let Some(end) = rest.iter().position(|b| *b == 0) else {
            return Vec::new();
        };
        let Ok(arg) = str::from_utf8(&rest[..end]) else {
            return Vec::new();
        };
        args.push(arg.to_owned());
        rest = &rest[end + 1..];
    }
    args
}

#[cfg(test)]
mod tests {
    use std::{env, process};

    use super::*;

    #[test]
    fn reads_own_directory_and_arguments() {
        let pid = process::id();
        assert_eq!(
            cwd_of(pid),
            env::current_dir()
                .ok()
                .and_then(|p| p.into_os_string().into_string().ok())
        );
        assert!(!argv_of(pid).is_empty());
        assert_eq!(of(u32::MAX), Foreground::default());
    }

    #[test]
    fn arguments_keep_empty_words_and_exclude_environment() {
        let mut bytes = 3i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/tool\0\0tool\0\0last\0SECRET=value\0");
        assert_eq!(args_from(&bytes), ["tool", "", "last"]);
        assert!(args_from(&bytes[..bytes.len() - 20]).is_empty());
    }
}
