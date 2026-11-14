#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
//! The only module with `unsafe` code (spec, decision 5). It holds these things, each unsafe
//! block with its safety argument and a test:
//! - the process table: `sysctl` on macOS (`macos.rs`), `/proc` on Linux (`linux.rs`, no
//!   `unsafe`); never `ps`;
//! - closing the run-lock descriptor a command inherited (`close_inherited_fd`), after the
//!   device-and-inode check `launch._close_if_same` makes, and the thread count it relies on;
//! - the steps a child takes before `exec`: `setsid` (`detach_session`) and keeping one
//!   descriptor open across `exec` (`pass_fd_to_child`);
//! - the dispatch runner's start: SIGHUP ignored (`ignore_sighup`, ruling S2-R37), and every
//!   inherited descriptor kept from its children (`keep_inherited_fds_from_children`, S2-R38).
//!
//! The safe layer over the process table is `crate::procs`.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// The variable a launcher sets to the run-lock descriptor it passes `claude` (R41a).
pub(crate) const LOCK_FD_VAR: &str = "LEADHAND_LOCK_FD";

/// Whether `close_inherited_fd` has run in this process.
static CLOSE_INHERITED_DONE: AtomicBool = AtomicBool::new(false);

/// Drops the run-lock descriptor a supervisor's Bash command inherited, as
/// `launch.close_inherited_lock` does (R41a). `main` calls it first; it acts at most once per
/// process, and every later call does nothing.
/// - `LEADHAND_LOCK_FD` is removed from the environment, so children do not try again: but
///   only when the process has one thread (counted at run time), because removing a variable
///   is unsound while another thread may read the environment. With more threads, or a count
///   that cannot be read, the variable stays and only the close below happens.
/// - With `same_file_as` given (the run's `run.lock`), the descriptor the variable names is
///   closed only when it is open and is the same file (device and inode) as that path: never
///   any other descriptor, never 0–2, and never an unlock (closing one reference leaves the
///   supervisor's lock in place). A value that is not an integer is ignored.
pub(crate) fn close_inherited_fd(same_file_as: Option<&Path>) {
    if CLOSE_INHERITED_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(raw) = std::env::var_os(LOCK_FD_VAR) else {
        return;
    };
    if platform::thread_count() == Some(1) {
        // SAFETY: `remove_var` is unsound only while another thread may read or write the
        // environment. The process has exactly one thread, this one, counted just now; no
        // other can start before the call returns, since only this thread could start it.
        unsafe { std::env::remove_var(LOCK_FD_VAR) };
    }
    let (Some(path), Some(fd)) = (same_file_as, descriptor(&raw)) else {
        return;
    };
    // SAFETY: `fd` is > 2 (never -1) and is borrowed for this one `fstat` call. This function
    // runs at most once, and its one caller is the start of `main`, before anything in the
    // process opens a descriptor: no object owns `fd`, so the borrow aliases no owner and
    // nothing closes it during the call. If it is not open, `fstat` fails with EBADF and
    // nothing else happens.
    let held = rustix::fs::fstat(unsafe { BorrowedFd::borrow_raw(fd) });
    let (Ok(held), Ok(want)) = (held, rustix::fs::stat(path)) else {
        return;
    };
    if (held.st_dev, held.st_ino) == (want.st_dev, want.st_ino) {
        // SAFETY: `fstat` just showed `fd` open, and it is the inherited descriptor, which
        // no object in this process owns (as above): taking ownership once and dropping it
        // closes it exactly once, and nothing uses the number afterwards.
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
/// (`subprocess.Popen(pass_fds=…)`).
///
/// The descriptor is borrowed here, so it is open when the step is registered, and its
/// identity (device and inode) is recorded. In the child the step first checks that the same
/// number is still that file; if the caller closed it before spawning (the number then names
/// nothing, or a descriptor opened since, such as std's own exec-error pipe), the spawn fails
/// with EBADF instead of clearing the flag on the wrong descriptor.
pub fn pass_fd_to_child(cmd: &mut Command, fd: BorrowedFd<'_>) {
    let raw = fd.as_raw_fd();
    let identity = rustix::fs::fstat(fd).ok().map(|st| (st.st_dev, st.st_ino));
    let keep_open = move || -> io::Result<()> {
        // SAFETY: in the forked child, which runs no other code, `raw` (a descriptor number
        // that was open when the step was registered, so never -1) is borrowed for these
        // `fstat` and `fcntl` calls only; nothing can close it meanwhile. If it is no longer
        // open, they fail with EBADF.
        let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
        let now = rustix::fs::fstat(borrowed)?;
        if identity != Some((now.st_dev, now.st_ino)) {
            return Err(Errno::BADF.into());
        }
        let flags = fcntl_getfd(borrowed)?;
        fcntl_setfd(borrowed, flags.difference(FdFlags::CLOEXEC))?;
        Ok(())
    };
    // SAFETY: the closure runs in the forked child before `exec`, where only
    // async-signal-safe work is allowed. It makes one `fstat(2)` and two `fcntl(2)` system
    // calls on one descriptor, allocates nothing, takes no lock, and builds any error from
    // the raw errno.
    unsafe { cmd.pre_exec(keep_open) };
}

/// Sets SIGHUP to ignored, as the Python's runner does (`signal.signal(SIGHUP, SIG_IGN)`).
/// An ignored signal stays ignored across `exec`, so Codex and every tool it starts inherit
/// it, as they do under the Python.
pub fn ignore_sighup() {
    // SAFETY: `signal(2)` with `SIG_IGN` installs no handler, so no code of ours ever runs on
    // delivery: it only sets SIGHUP's disposition, for the whole process, which is what is
    // meant. Nothing else in leadhand handles SIGHUP. The result is ignored: it fails only for
    // a signal number that is not valid, and SIGHUP is.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

/// Marks every descriptor above 2 close-on-exec, so that none this process inherited without
/// the flag reaches a child it starts: Python's `Popen(close_fds=True)` keeps them out of
/// Codex. The descriptors are listed from `/dev/fd` (macOS) or `/proc/self/fd` (Linux); the
/// process keeps them open itself.
pub fn keep_inherited_fds_from_children() -> io::Result<()> {
    let listing = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let fds: Vec<RawFd> = std::fs::read_dir(listing)?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|fd| *fd > 2)
        .collect();
    for fd in fds {
        // SAFETY: `fcntl(2)` with `F_GETFD` and `F_SETFD` on a descriptor number only reads and
        // sets that descriptor's close-on-exec flag, and touches no memory. The number need not
        // still be open: the listing's own directory descriptor is closed by now, and a closed
        // number fails with EBADF, which is skipped. Setting the flag on a descriptor this
        // process opened itself changes nothing, since Rust opens every file close-on-exec.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::process::ExitStatusExt;
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
    fn open_in_child(fd: BorrowedFd, pass: bool) -> bool {
        let mut cmd = Command::new("test");
        cmd.arg("-e")
            .arg(format!("/dev/fd/{}", fd.as_raw_fd()))
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
        assert!(open_in_child(passed.as_fd(), true));
        assert!(
            !open_in_child(kept.as_fd(), false),
            "close-on-exec by default"
        );
        // The flag is cleared in the child only.
        assert!(
            rustix::io::fcntl_getfd(&passed)
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );

        // The number is checked to be the same file in the child: when it names another
        // file by then, the spawn fails instead of clearing the flag on the wrong descriptor.
        let mut swapped: OwnedFd = File::create(dir.path().join("swapped")).unwrap().into();
        let mut cmd = Command::new("true");
        pass_fd_to_child(&mut cmd, swapped.as_fd());
        rustix::io::dup2(&kept, &mut swapped).unwrap();
        fcntl_setfd(&swapped, FdFlags::CLOEXEC).unwrap();
        assert!(cmd.status().is_err(), "another file under the number");
        // Closed before the spawn: the number names nothing, or something opened since.
        let gone = File::create(dir.path().join("gone")).unwrap();
        let mut cmd = Command::new("true");
        pass_fd_to_child(&mut cmd, gone.as_fd());
        drop(gone);
        assert!(cmd.status().is_err(), "a closed descriptor");
    }

    const CLOSE_CHILD: &str = "LEADHAND_SYS_TEST_CLOSE_CHILD";
    const CLOSE_LOCK: &str = "LEADHAND_SYS_TEST_LOCK";
    const CLOSE_AGAIN: &str = "LEADHAND_SYS_TEST_AGAIN";

    /// Not a test of its own: the child process `run_close_child` starts (this test binary,
    /// running this test alone). It makes one `close_inherited_fd` call as `main` makes it
    /// (then a second one, with another lock path, if asked), and reports whether
    /// `LEADHAND_LOCK_FD` is still set and which listed descriptors are open. Does nothing in
    /// an ordinary run.
    ///
    /// libtest runs every test on a thread of its own, so this process always has more than
    /// one thread, and `close_inherited_fd` always keeps the variable here. A parked thread is
    /// started as well, so that stays true however libtest runs it. Only a single-threaded
    /// process (`main`) removes the variable.
    #[test]
    fn close_inherited_fd_child() {
        let Some(fds) = std::env::var_os(CLOSE_CHILD) else {
            return;
        };
        std::thread::spawn(|| {
            loop {
                std::thread::park();
            }
        });
        assert!(platform::thread_count().unwrap() > 1);
        let lock = std::env::var_os(CLOSE_LOCK).map(PathBuf::from);
        close_inherited_fd(lock.as_deref());
        if let Some(again) = std::env::var_os(CLOSE_AGAIN) {
            close_inherited_fd(Some(Path::new(&again)));
        }
        let var = std::env::var_os(LOCK_FD_VAR).is_some();
        let open: Vec<String> = fds
            .to_str()
            .unwrap()
            .split(',')
            .map(|fd| format!("{fd}={}", Path::new(&format!("/dev/fd/{fd}")).exists()))
            .collect();
        println!("report var={var} {}", open.join(" "));
    }

    /// One run of `close_inherited_fd_child`.
    #[derive(Default)]
    struct CloseCase<'a> {
        /// `LEADHAND_LOCK_FD`'s value; unset when `None`.
        value: Option<&'a str>,
        /// The lock path `main` would pass.
        lock: Option<&'a Path>,
        /// Descriptors the child inherits and reports on; 0-2 are reported, not passed.
        fds: &'a [BorrowedFd<'a>],
        /// Call again, with this lock path.
        again: Option<&'a Path>,
        /// Another variable set in the child.
        extra: Option<(&'a str, &'a str)>,
    }

    /// Runs `close_inherited_fd_child` for `case`; returns its report.
    fn run_close_child(case: CloseCase) -> String {
        let listed: Vec<String> = case
            .fds
            .iter()
            .map(|fd| fd.as_raw_fd().to_string())
            .collect();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "sys::tests::close_inherited_fd_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CLOSE_CHILD, listed.join(","))
        .env_remove(LOCK_FD_VAR)
        .env_remove(CLOSE_LOCK)
        .env_remove(CLOSE_AGAIN)
        .stdin(Stdio::null());
        if let Some(value) = case.value {
            cmd.env(LOCK_FD_VAR, value);
        }
        if let Some(path) = case.lock {
            cmd.env(CLOSE_LOCK, path);
        }
        if let Some(path) = case.again {
            cmd.env(CLOSE_AGAIN, path);
        }
        if let Some((name, value)) = case.extra {
            cmd.env(name, value);
        }
        for fd in case.fds {
            if fd.as_raw_fd() > 2 {
                pass_fd_to_child(&mut cmd, fd.as_fd());
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

    /// `run_close_child` with `LEADHAND_LOCK_FD=value`, the lock path `lock` and the
    /// descriptors `fds`, nothing else.
    fn close_in_child(value: &str, lock: Option<&Path>, fds: &[BorrowedFd]) -> String {
        run_close_child(CloseCase {
            value: Some(value),
            lock,
            fds,
            ..CloseCase::default()
        })
    }

    #[test]
    fn close_inherited_fd_closes_only_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("run.lock");
        let lock = File::create(&lock_path).unwrap();
        let other = File::create(dir.path().join("other")).unwrap();
        let (l, o) = (lock.as_raw_fd(), other.as_raw_fd());
        let fds = [lock.as_fd(), other.as_fd()];

        // Another file under the variable stays open.
        assert_eq!(
            close_in_child(&o.to_string(), Some(&lock_path), &fds),
            format!("var=true {l}=true {o}=true")
        );
        // The same file (device and inode) is closed, and nothing else.
        assert_eq!(
            close_in_child(&l.to_string(), Some(&lock_path), &fds),
            format!("var=true {l}=false {o}=true")
        );
        // A hard link to run.lock is the same file.
        let link = dir.path().join("link.lock");
        std::fs::hard_link(&lock_path, &link).unwrap();
        assert_eq!(
            close_in_child(&l.to_string(), Some(&link), &fds),
            format!("var=true {l}=false {o}=true")
        );
        // Without a run directory, nothing is closed.
        assert_eq!(
            close_in_child(&l.to_string(), None, &fds),
            format!("var=true {l}=true {o}=true")
        );
        // A missing run.lock closes nothing.
        assert_eq!(
            close_in_child(&l.to_string(), Some(&dir.path().join("missing")), &fds),
            format!("var=true {l}=true {o}=true")
        );
        // Not a number, or a standard descriptor: ignored.
        for value in ["", "x", "2", "-1", "99999999999"] {
            assert_eq!(
                close_in_child(
                    value,
                    Some(&lock_path),
                    &[std::io::stderr().as_fd(), lock.as_fd(), other.as_fd()]
                ),
                format!("var=true 2=true {l}=true {o}=true"),
                "value {value:?}"
            );
        }
        // Surrounding whitespace is allowed, as Python's int() allows it.
        assert_eq!(
            close_in_child(&format!(" {l}\n"), Some(&lock_path), &fds),
            format!("var=true {l}=false {o}=true")
        );
    }

    #[test]
    fn close_inherited_fd_acts_at_most_once() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("run.lock");
        let lock = File::create(&lock_path).unwrap();
        let other_path = dir.path().join("other");
        let other = File::create(&other_path).unwrap();
        let (l, o) = (lock.as_raw_fd(), other.as_raw_fd());
        // The variable stays (more than one thread), so a second call could still act on it:
        // the first call (another file) closes nothing, and the second (run.lock) does
        // nothing at all.
        let report = run_close_child(CloseCase {
            value: Some(&l.to_string()),
            lock: Some(&other_path),
            fds: &[lock.as_fd(), other.as_fd()],
            again: Some(&lock_path),
            extra: None,
        });
        assert_eq!(report, format!("var=true {l}=true {o}=true"));
    }

    #[test]
    fn close_inherited_fd_with_threads_still_closes_but_keeps_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("run.lock");
        let lock = File::create(&lock_path).unwrap();
        let other = File::create(dir.path().join("other")).unwrap();
        let (l, o) = (lock.as_raw_fd(), other.as_raw_fd());
        let report = run_close_child(CloseCase {
            value: Some(&l.to_string()),
            lock: Some(&lock_path),
            fds: &[lock.as_fd(), other.as_fd()],
            ..CloseCase::default()
        });
        assert_eq!(report, format!("var=true {l}=false {o}=true"));
    }

    #[test]
    fn close_inherited_fd_reads_only_its_own_variable() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("run.lock");
        let lock = File::create(&lock_path).unwrap();
        let l = lock.as_raw_fd();
        // Python's in-place checkout claim variable, naming run.lock: not Rust's to close.
        let report = run_close_child(CloseCase {
            value: None,
            lock: Some(&lock_path),
            fds: &[lock.as_fd()],
            extra: Some(("LEADHAND_CHECKOUT_FD", &l.to_string())),
            ..CloseCase::default()
        });
        assert_eq!(report, format!("var=false {l}=true"));
    }

    #[test]
    fn env_has_is_none_for_a_missing_process() {
        assert_eq!(env_has(0, b"A=1"), None);
        assert_eq!(env_has((1 << 22) + 12345, b"A=1"), None);
    }

    /// The check `start_child` makes, when its variable names one: `sighup` or `descriptors`.
    const START_CHILD: &str = "LEADHAND_TEST_SYS_START";

    /// The runner's start steps change the whole process (a signal disposition, every
    /// descriptor's flag), so they are checked in this test binary run again as a child. It
    /// prints `verdict: …` and exits.
    #[test]
    fn start_child() {
        let Some(what) = std::env::var_os(START_CHILD) else {
            return;
        };
        let verdict = match what.to_str() {
            Some("sighup") => {
                // A child that sends itself SIGHUP, before and after.
                let hup = || {
                    Command::new("sh")
                        .args(["-c", "kill -HUP $$; echo survived"])
                        .stdin(Stdio::null())
                        .output()
                        .unwrap()
                };
                let before = hup();
                ignore_sighup();
                let after = hup();
                format!(
                    "{:?} {:?} {}",
                    before.status.signal(),
                    after.status.code(),
                    String::from_utf8_lossy(&after.stdout).trim()
                )
            }
            Some("descriptors") => {
                // A descriptor this process has without close-on-exec, as one it inherited.
                let path =
                    std::env::temp_dir().join(format!("leadhand-start-{}", getpid().as_raw_pid()));
                let file = File::create(&path).unwrap();
                let fd = rustix::io::fcntl_dupfd_cloexec(&file, 60).unwrap();
                fcntl_setfd(&fd, FdFlags::empty()).unwrap();
                let before = open_in_child(fd.as_fd(), false);
                keep_inherited_fds_from_children().unwrap();
                let after = open_in_child(fd.as_fd(), false);
                let flagged = fcntl_getfd(&fd).unwrap().contains(FdFlags::CLOEXEC);
                let _ = std::fs::remove_file(&path);
                format!("{before} {after} {flagged}")
            }
            _ => "unknown".into(),
        };
        println!("verdict: {verdict}");
        std::process::exit(0);
    }

    /// `start_child`'s verdict on `what`.
    fn start(what: &str) -> String {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "sys::tests::start_child",
                "--exact",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(START_CHILD, what)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        // libtest prints the test's name first, on the same line.
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find_map(|l| l.split_once("verdict: ").map(|(_, v)| v))
            .unwrap_or_else(|| panic!("no verdict: {text}{}", String::from_utf8_lossy(&out.stderr)))
            .to_string()
    }

    #[test]
    fn ignore_sighup_is_inherited_across_exec() {
        // Before: the child dies of SIGHUP (here SIGHUP has its default action). After: it
        // inherits SIGHUP ignored, survives its own, and goes on.
        assert_eq!(
            start("sighup"),
            format!("Some({}) Some(0) survived", libc::SIGHUP)
        );
    }

    #[test]
    fn inherited_descriptors_are_kept_from_children() {
        // Before: a child inherits the descriptor. After: it does not, and it is marked
        // close-on-exec, still open here.
        assert_eq!(start("descriptors"), "true false true");
    }
}
