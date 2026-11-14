#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
//! The only module with `unsafe` code (spec, decision 5). It holds three things, each unsafe
//! block with its safety argument and a test:
//! - the process table: `sysctl` on macOS (`macos.rs`), `/proc` on Linux (`linux.rs`, no
//!   `unsafe`); never `ps`;
//! - closing the run-lock descriptor a command inherited (`close_inherited_fd`), after the
//!   device-and-inode check `launch._close_if_same` makes;
//! - the steps a child takes before `exec`: `setsid` (`detach_session`) and keeping one
//!   descriptor open across `exec` (`pass_fd_to_child`).
//!
//! The safe layer over the process table is `crate::procs`.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

/// One process-table entry (`procs.ProcInfo`, plus the owner `procs.tagged_processes`
/// filters on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// An opaque start time, byte-identical to `procs.py`'s: `"<sec>.<usec:06>"` on macOS,
    /// `"ticks:<field 22 of /proc/<pid>/stat>"` on Linux. Only ever compared for equality.
    pub start: String,
    /// Exited and not yet reaped: it cannot run or write anything, so it counts as gone.
    pub zombie: bool,
    /// The real user on macOS (`kp_eproc.e_pcred.p_ruid`); the owner of `/proc/<pid>` on
    /// Linux.
    pub uid: u32,
}

/// The process's entry, or `None` if there is none (`procs.process_info`). A zombie is
/// returned, marked.
pub fn proc_info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        None
    } else {
        platform::proc_info(pid)
    }
}

/// The whole process table, zombies included; `None` when it cannot be read.
pub fn all_procs() -> Option<Vec<ProcInfo>> {
    platform::all_procs()
}

/// Whether the process's initial environment holds `entry` (`NAME=value`, matched whole);
/// `None` when the environment cannot be read (a process that is gone, or another user's).
pub fn env_has(pid: i32, entry: &[u8]) -> Option<bool> {
    if pid <= 0 {
        None
    } else {
        platform::env_has(pid, entry)
    }
}

/// Drops the descriptor a supervisor's Bash command inherited, as `launch.close_inherited_lock`
/// does for `LEADHAND_LOCK_FD` (R41a). The variable `var` is removed from the environment
/// either way, so children do not try again. Then, with `same_file_as` given (the run's
/// `run.lock`), the descriptor `var` named is closed only when it is open and is the same file
/// (device and inode) as that path: never any other descriptor, never 0–2, and never an unlock
/// (closing one reference leaves the supervisor's lock in place). A value that is not an
/// integer is ignored.
///
/// Call it first thing in `main`, while the process has one thread and has opened nothing:
/// both the environment change and taking the descriptor rely on that.
pub fn close_inherited_fd(var: &str, same_file_as: Option<&Path>) {
    let Some(raw) = std::env::var_os(var) else {
        return;
    };
    // SAFETY: `remove_var` is unsound only while another thread reads or writes the
    // environment. The documented caller is `main`, before anything else runs: one thread,
    // and no code that touches the environment behind std's lock. (The unit test calls it
    // in a child process running that one test.)
    unsafe { std::env::remove_var(var) };
    let (Some(path), Some(fd)) = (same_file_as, descriptor(&raw)) else {
        return;
    };
    // SAFETY: `fd` is > 2 (never -1) and is borrowed for this one `fstat` call. No object in
    // this process owns it (nothing has been opened yet, see above), so the borrow aliases no
    // owner, and with one thread nothing can close it during the call. If it is not open,
    // `fstat` fails with EBADF and nothing else happens.
    let held = rustix::fs::fstat(unsafe { BorrowedFd::borrow_raw(fd) });
    let (Ok(held), Ok(want)) = (held, rustix::fs::stat(path)) else {
        return;
    };
    if (held.st_dev, held.st_ino) == (want.st_dev, want.st_ino) {
        // SAFETY: `fstat` just showed `fd` open, and it is the inherited descriptor, which
        // no object in this process owns: taking ownership once and dropping it closes it
        // exactly once, and nothing uses the number afterwards.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
}

/// The descriptor number in `raw`, as `launch._close_if_same` reads it (`int()`, so
/// surrounding whitespace is allowed); `None` for anything else, or 0–2.
fn descriptor(raw: &OsStr) -> Option<RawFd> {
    raw.to_str()?
        .trim()
        .parse::<RawFd>()
        .ok()
        .filter(|&fd| fd > 2)
}

/// Starts the child in a new session (`setsid` before `exec`, as Python's
/// `start_new_session=True`): its own session and process group, no controlling terminal.
pub fn detach_session(cmd: &mut Command) {
    let setsid = || -> io::Result<()> {
        rustix::process::setsid()?;
        Ok(())
    };
    // SAFETY: the closure runs in the forked child before `exec`, where only
    // async-signal-safe work is allowed. It makes one `setsid(2)` system call, allocates
    // nothing, takes no lock, and builds any error from the raw errno.
    unsafe { cmd.pre_exec(setsid) };
}

/// Keeps descriptor `fd` open across the child's `exec` by clearing `FD_CLOEXEC` in the child
/// only: Rust opens every file close-on-exec, so this one child, and no other, inherits it
/// (`subprocess.Popen(pass_fds=…)`). A descriptor that is not open fails the spawn (EBADF).
pub fn pass_fd_to_child(cmd: &mut Command, fd: RawFd) {
    let keep_open = move || -> io::Result<()> {
        if fd < 0 {
            return Err(Errno::BADF.into());
        }
        // SAFETY: in the forked child, which runs no other code, `fd` (not -1) is borrowed
        // for two `fcntl` calls. If the parent did not have it open, they fail with EBADF.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let flags = fcntl_getfd(borrowed)?;
        fcntl_setfd(borrowed, flags.difference(FdFlags::CLOEXEC))?;
        Ok(())
    };
    // SAFETY: the closure runs in the forked child before `exec`, where only
    // async-signal-safe work is allowed. It makes two `fcntl(2)` system calls on one
    // descriptor, allocates nothing, takes no lock, and builds any error from the raw errno.
    unsafe { cmd.pre_exec(keep_open) };
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::process::{Child, Stdio};

    use rustix::io::FdFlags;
    use rustix::process::{Pid, getpgrp, getpid, getsid};

    use super::*;

    /// A child process this test started: killed and reaped when the test ends.
    pub(crate) struct Kid(pub Child);

    impl Kid {
        pub(crate) fn pid(&self) -> i32 {
            i32::try_from(self.0.id()).unwrap()
        }
    }

    impl Drop for Kid {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// `sleep 30`, in a new session when `detach`.
    pub(crate) fn sleeper(detach: bool) -> Kid {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdin(Stdio::null());
        if detach {
            detach_session(&mut cmd);
        }
        Kid(cmd.spawn().expect("spawn sleep"))
    }

    #[test]
    fn detached_child_has_new_session() {
        let kid = sleeper(true);
        let pid = kid.pid();
        let info = proc_info(pid).expect("our own child is in the table");
        assert_eq!((info.ppid, info.pgid), (getpid().as_raw_pid(), pid));
        let sid = getsid(Pid::from_raw(pid)).unwrap();
        assert_eq!(sid.as_raw_pid(), pid, "the child leads a new session");
        assert_ne!(getsid(None).unwrap(), sid);

        let plain = sleeper(false);
        let info = proc_info(plain.pid()).unwrap();
        assert_eq!(
            info.pgid,
            getpgrp().as_raw_pid(),
            "without it, the child stays in our group"
        );
        assert_eq!(
            getsid(Pid::from_raw(plain.pid())).unwrap(),
            getsid(None).unwrap()
        );
    }

    /// Whether `fd` is open in a `test -e /dev/fd/<fd>` child, passed to it or not.
    fn open_in_child(fd: RawFd, pass: bool) -> bool {
        let mut cmd = Command::new("test");
        cmd.arg("-e")
            .arg(format!("/dev/fd/{fd}"))
            .stdin(Stdio::null());
        if pass {
            pass_fd_to_child(&mut cmd, fd);
        }
        cmd.status().expect("run test").success()
    }

    #[test]
    fn pass_fd_to_child_keeps_only_that_descriptor_open() {
        let dir = tempfile::tempdir().unwrap();
        let passed = File::create(dir.path().join("passed")).unwrap();
        let kept = File::create(dir.path().join("kept")).unwrap();
        assert!(open_in_child(passed.as_raw_fd(), true));
        assert!(
            !open_in_child(kept.as_raw_fd(), false),
            "close-on-exec by default"
        );
        // The flag is cleared in the child only.
        assert!(
            rustix::io::fcntl_getfd(&passed)
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );

        // A descriptor that is not open fails the spawn instead of being ignored.
        let mut cmd = Command::new("true");
        pass_fd_to_child(&mut cmd, -1);
        assert!(cmd.status().is_err());
    }

    const CLOSE_CHILD: &str = "LEADHAND_SYS_TEST_CLOSE_CHILD";
    const CLOSE_LOCK: &str = "LEADHAND_SYS_TEST_LOCK";

    /// Not a test of its own: the child process `close_in_child` starts (this test binary,
    /// running this test alone). One `close_inherited_fd` call, as `main` makes it, then a
    /// report of the variable and of each listed descriptor. Does nothing in an ordinary run.
    #[test]
    fn close_inherited_fd_child() {
        let Some(fds) = std::env::var_os(CLOSE_CHILD) else {
            return;
        };
        let lock = std::env::var_os(CLOSE_LOCK).map(PathBuf::from);
        close_inherited_fd("LEADHAND_LOCK_FD", lock.as_deref());
        let var = std::env::var_os("LEADHAND_LOCK_FD").is_some();
        let open: Vec<String> = fds
            .to_str()
            .unwrap()
            .split(',')
            .map(|fd| format!("{fd}={}", Path::new(&format!("/dev/fd/{fd}")).exists()))
            .collect();
        println!("report var={var} {}", open.join(" "));
    }

    /// Runs `close_inherited_fd_child` with `LEADHAND_LOCK_FD=value`, the lock path `lock`
    /// and the descriptors `fds` inherited; returns its report.
    fn close_in_child(value: &str, lock: Option<&Path>, fds: &[RawFd]) -> String {
        let listed: Vec<String> = fds.iter().map(RawFd::to_string).collect();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "sys::tests::close_inherited_fd_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CLOSE_CHILD, listed.join(","))
        .env("LEADHAND_LOCK_FD", value)
        .stdin(Stdio::null());
        match lock {
            Some(path) => cmd.env(CLOSE_LOCK, path),
            None => cmd.env_remove(CLOSE_LOCK),
        };
        for &fd in fds {
            if fd > 2 {
                pass_fd_to_child(&mut cmd, fd);
            }
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            out.status.success(),
            "child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // libtest prints the report after the test's name, on the same line.
        let (_, report) = stdout
            .split_once("report ")
            .unwrap_or_else(|| panic!("no report in {stdout:?}"));
        report.lines().next().unwrap().to_owned()
    }

    #[test]
    fn close_inherited_fd_closes_only_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("run.lock");
        let lock = File::create(&lock_path).unwrap();
        let other = File::create(dir.path().join("other")).unwrap();
        let (l, o) = (lock.as_raw_fd(), other.as_raw_fd());

        // Another file under the variable stays open.
        assert_eq!(
            close_in_child(&o.to_string(), Some(&lock_path), &[l, o]),
            format!("var=false {l}=true {o}=true")
        );
        // The same file (device and inode) is closed, and nothing else.
        assert_eq!(
            close_in_child(&l.to_string(), Some(&lock_path), &[l, o]),
            format!("var=false {l}=false {o}=true")
        );
        // A hard link to run.lock is the same file.
        let link = dir.path().join("link.lock");
        std::fs::hard_link(&lock_path, &link).unwrap();
        assert_eq!(
            close_in_child(&l.to_string(), Some(&link), &[l, o]),
            format!("var=false {l}=false {o}=true")
        );
        // Without a run directory, nothing is closed; the variable goes either way.
        assert_eq!(
            close_in_child(&l.to_string(), None, &[l, o]),
            format!("var=false {l}=true {o}=true")
        );
        // A missing run.lock closes nothing.
        assert_eq!(
            close_in_child(&l.to_string(), Some(&dir.path().join("missing")), &[l, o]),
            format!("var=false {l}=true {o}=true")
        );
        // Not a number, or a standard descriptor: ignored.
        for value in ["", "x", "2", "-1", "99999999999"] {
            assert_eq!(
                close_in_child(value, Some(&lock_path), &[2, l, o]),
                format!("var=false 2=true {l}=true {o}=true"),
                "value {value:?}"
            );
        }
        // Surrounding whitespace is allowed, as Python's int() allows it.
        assert_eq!(
            close_in_child(&format!(" {l}\n"), Some(&lock_path), &[l, o]),
            format!("var=false {l}=false {o}=true")
        );
    }

    #[test]
    fn env_has_is_none_for_a_missing_process() {
        assert_eq!(env_has(0, b"A=1"), None);
        assert_eq!(env_has((1 << 22) + 12345, b"A=1"), None);
    }
}
