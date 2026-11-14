//! macOS: the process table from `sysctl` `kern.proc`, read as `procs.py` reads it through
//! ctypes (Claude Code's sandbox refuses `ps` but allows this).
//!
//! - `KERN_PROC_PID` / `KERN_PROC_ALL` give `struct kinfo_proc` records; the start time is
//!   `kp_proc.p_starttime` as `"<sec>.<usec:06>"`.
//! - `KERN_PROCARGS2` gives a process's initial argv and environment.
//!
//! The one `unsafe` block is the `sysctl(3)` call; everything it returns is parsed from bytes.

use std::io;
use std::ptr;

use libc::{CTL_KERN, KERN_ARGMAX, KERN_PROC, KERN_PROC_ALL, KERN_PROC_PID, KERN_PROCARGS2, c_int};

use super::ProcInfo;

// `struct kinfo_proc` (<sys/sysctl.h>: `struct extern_proc kp_proc` from <sys/proc.h>, then
// `struct eproc kp_eproc`). The size and offsets are `procs.py`'s, and match `offsetof` and
// `sizeof` compiled against the macOS SDK headers (arm64; x86_64 has the same LP64 layout).
/// `sizeof(struct kinfo_proc)` on 64-bit Darwin.
const KINFO_SIZE: usize = 648;
/// `kp_proc.p_un.__p_starttime`: `struct timeval { int64 tv_sec; int32 tv_usec }`.
const OFF_START: usize = 0;
/// `kp_proc.p_stat` (`char`).
const OFF_STAT: usize = 36;
/// `kp_proc.p_pid` (`pid_t`).
const OFF_PID: usize = 40;
/// `kp_eproc.e_pcred.p_ruid` (`uid_t`): the real user.
const OFF_RUID: usize = 392;
/// `kp_eproc.e_ppid` (`pid_t`).
const OFF_PPID: usize = 560;
/// `kp_eproc.e_pgid` (`pid_t`).
const OFF_PGID: usize = 564;
/// `p_stat` of a zombie (<sys/proc.h>).
const SZOMB: i8 = 5;
/// Fetches of the whole table, when it grows between the size query and the fetch (ENOMEM).
const ATTEMPTS: usize = 5;
/// `KERN_PROCARGS2`'s buffer when `kern.argmax` cannot be read.
const DEFAULT_ARGMAX: usize = 1 << 20;

/// `sysctl(mib)` into `buf`, or with `buf` `None` only the size the value needs. Returns the
/// number of bytes (written, or needed).
fn sysctl(mib: &mut [c_int], buf: Option<&mut [u8]>) -> io::Result<usize> {
    let (oldp, mut len) = match buf {
        Some(b) => (b.as_mut_ptr().cast::<libc::c_void>(), b.len()),
        None => (ptr::null_mut(), 0),
    };
    let namelen = libc::c_uint::try_from(mib.len())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `mib` points at `namelen` initialised ints, borrowed for the call. `oldp` is
    // either null (the size query, `len` 0) or the start of `buf`, which is `len` writable
    // bytes borrowed mutably for the call; the kernel writes at most `*oldlenp` = `len` bytes
    // there and stores the byte count through `&mut len`, a valid `size_t`. `newp` is null
    // with `newlen` 0: nothing is set. Nothing is retained after the call returns.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            namelen,
            oldp,
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(len)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `procs._sysctl`: the value of `mib`, `size` bytes at most; with `size` `None`, the size is
/// asked first, with room to spare, and the fetch retried while the table grows (ENOMEM).
/// `None` on failure.
fn fetch(mib: &mut [c_int], size: Option<usize>) -> Option<Vec<u8>> {
    for _ in 0..ATTEMPTS {
        let want = match size {
            Some(n) => n,
            None => {
                let n = sysctl(mib, None).ok()?;
                n + n / 8 + KINFO_SIZE * 16
            }
        };
        let mut buf = vec![0u8; want];
        match sysctl(mib, Some(&mut buf)) {
            Ok(n) => {
                buf.truncate(n);
                return Some(buf);
            }
            Err(e) if size.is_none() && e.raw_os_error() == Some(libc::ENOMEM) => {}
            Err(_) => return None,
        }
    }
    None
}

fn i32_at(raw: &[u8], off: usize) -> i32 {
    i32::from_ne_bytes(raw[off..off + 4].try_into().unwrap())
}

/// One `kinfo_proc` record, at `raw[off..off + KINFO_SIZE]`.
fn kinfo(raw: &[u8], off: usize) -> ProcInfo {
    let sec = i64::from_ne_bytes(
        raw[off + OFF_START..off + OFF_START + 8]
            .try_into()
            .unwrap(),
    );
    let usec = i32_at(raw, off + OFF_START + 8);
    let stat = i8::from_ne_bytes([raw[off + OFF_STAT]]);
    ProcInfo {
        pid: i32_at(raw, off + OFF_PID),
        ppid: i32_at(raw, off + OFF_PPID),
        pgid: i32_at(raw, off + OFF_PGID),
        start: format!("{sec}.{usec:06}"),
        zombie: stat == SZOMB,
        uid: u32::from_ne_bytes(raw[off + OFF_RUID..off + OFF_RUID + 4].try_into().unwrap()),
    }
}

/// `procs._darwin_pid`: `None` when the pid does not exist or `sysctl` fails (the Python then
/// tries `ps`, which Rust never runs).
pub(super) fn proc_info(pid: i32) -> Option<ProcInfo> {
    let raw = fetch(
        &mut [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid],
        Some(KINFO_SIZE),
    )?;
    if raw.len() < KINFO_SIZE {
        return None;
    }
    let info = kinfo(&raw, 0);
    (info.pid == pid).then_some(info)
}

/// `procs._darwin_all`.
pub(super) fn all_procs() -> Option<Vec<ProcInfo>> {
    let raw = fetch(&mut [CTL_KERN, KERN_PROC, KERN_PROC_ALL], None)?;
    let (records, _) = raw.as_chunks::<KINFO_SIZE>();
    Some(records.iter().map(|r| kinfo(r, 0)).collect())
}

/// `procs._darwin_argmax`: `kern.argmax`, the size of `KERN_PROCARGS2`'s value at most.
fn argmax() -> usize {
    fetch(&mut [CTL_KERN, KERN_ARGMAX], Some(4))
        .filter(|raw| raw.len() == 4)
        .and_then(|raw| usize::try_from(i32_at(&raw, 0)).ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_ARGMAX)
}

/// Whether the process's initial environment holds `entry`; `None` when it cannot be read.
pub(super) fn env_has(pid: i32, entry: &[u8]) -> Option<bool> {
    let raw = fetch(&mut [CTL_KERN, KERN_PROCARGS2, pid], Some(argmax()))?;
    Some(environment(&raw)?.contains(&entry))
}

/// `procs._darwin_environ`: the environment in a `KERN_PROCARGS2` value: `int argc`, the
/// exec path, NUL padding, the `argc` arguments, then the environment up to an empty string.
fn environment(raw: &[u8]) -> Option<Vec<&[u8]>> {
    if raw.len() < 4 {
        return None;
    }
    let argc = usize::try_from(i32_at(raw, 0)).ok()?;
    let mut end = 4 + raw[4..].iter().position(|&b| b == 0)?;
    while end < raw.len() && raw[end] == 0 {
        end += 1;
    }
    Some(
        raw[end..]
            .split(|&b| b == 0)
            .skip(argc)
            .take_while(|s| !s.is_empty())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procargs(argc: i32, rest: &[u8]) -> Vec<u8> {
        let mut raw = argc.to_ne_bytes().to_vec();
        raw.extend_from_slice(rest);
        raw
    }

    #[test]
    fn procargs_environment_follows_the_arguments() {
        let raw = procargs(
            2,
            b"/usr/bin/python3\0\0\0\0python3\0A=1-argv\0A=1\0B=2\0\0\0junk\0",
        );
        assert_eq!(environment(&raw), Some(vec![&b"A=1"[..], b"B=2"]));
        // No environment at all.
        assert_eq!(environment(&procargs(1, b"/bin/x\0\0x\0")), Some(vec![]));
        assert_eq!(environment(&procargs(0, b"/bin/x\0\0")), Some(vec![]));
    }

    #[test]
    fn procargs_that_cannot_be_parsed() {
        assert_eq!(environment(b"\x01\0\0"), None, "shorter than argc");
        assert_eq!(
            environment(&procargs(1, b"/bin/x")),
            None,
            "no NUL after the exec path"
        );
        assert_eq!(
            environment(&procargs(-1, b"/bin/x\0\0")),
            None,
            "a negative argc"
        );
    }

    #[test]
    fn kinfo_reads_procs_py_offsets() {
        let mut raw = vec![0u8; KINFO_SIZE];
        raw[OFF_START..OFF_START + 8].copy_from_slice(&1_700_000_000i64.to_ne_bytes());
        raw[OFF_START + 8..OFF_START + 12].copy_from_slice(&42i32.to_ne_bytes());
        raw[OFF_STAT] = 5;
        raw[OFF_PID..OFF_PID + 4].copy_from_slice(&123i32.to_ne_bytes());
        raw[OFF_RUID..OFF_RUID + 4].copy_from_slice(&501u32.to_ne_bytes());
        raw[OFF_PPID..OFF_PPID + 4].copy_from_slice(&1i32.to_ne_bytes());
        raw[OFF_PGID..OFF_PGID + 4].copy_from_slice(&120i32.to_ne_bytes());
        let info = kinfo(&raw, 0);
        assert_eq!(
            info,
            ProcInfo {
                pid: 123,
                ppid: 1,
                pgid: 120,
                start: "1700000000.000042".into(),
                zombie: true,
                uid: 501
            }
        );
    }

    #[test]
    fn argmax_is_the_kernel_value() {
        assert!(argmax() >= 4096);
    }
}
