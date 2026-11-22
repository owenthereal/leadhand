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
//! - the dispatch runner's start: SIGHUP set to `SIG_IGN` with `signal(2)` (`ignore_sighup`,
//!   ruling S2-R37), and every inherited descriptor kept from its children
//!   (`keep_inherited_fds_from_children`, S2-R38);
//! - the launcher's SIGINT and SIGTERM forwarding handler (`forward_signals`, ruling S2-R40):
//!   installed and restored with `sigaction(2)`, so the previous dispositions come back as the
//!   Python restores its handlers; it sends with rustix's `kill_process` and keeps `errno` for
//!   the code it interrupts;
//! - the local date a new run directory is named for (`local_date`, `localtime_r(3)`).
//!
//! The safe layer over the process table is `crate::procs`.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use libc::c_int;
use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::process::{Pid, Signal};

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

/// One process's record as the platform gives it (a `/proc/<pid>/stat` line on Linux, a
/// `KERN_PROC_PID` buffer on macOS), parsed as `proc_info` parses it: for the reference
/// samples of `procs::tests::start_time_parsers_match_reference_samples`.
#[cfg(test)]
pub(crate) fn parse_record(pid: i32, raw: &[u8]) -> Option<ProcInfo> {
    platform::parse_record(pid, raw)
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

unsafe extern "C" {
    /// POSIX `tzset(3)`, in the C library on macOS and Linux; the `libc` crate does not
    /// declare it for every target.
    fn tzset();
}

/// The local calendar date at `secs` seconds after the epoch, as `(year, month, day)`
/// (Python's `date.fromtimestamp`, so `date.today()` at the current time): `localtime_r(3)`
/// after `tzset(3)`, so `TZ` and the system's zone are read as the C library reads them.
/// `None` when the C library cannot convert the time.
pub fn local_date(secs: i64) -> Option<(i32, u32, u32)> {
    let t = libc::time_t::try_from(secs).ok()?;
    // SAFETY: `tzset` takes no arguments and only (re)reads `TZ` into the C library's zone
    // state. Reading the environment races only with a concurrent `setenv`/`unsetenv`, and
    // leadhand changes its own environment only in `close_inherited_fd`, at the start of
    // `main` with one thread.
    unsafe { tzset() };
    // SAFETY: `tm` is a zeroed, valid `struct tm` owned by this frame, and `localtime_r`
    // writes only into it (the reentrant form: no shared static buffer). `t` is a live local
    // read through a shared pointer. On failure it returns null and `tm` is not used.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        tm
    };
    let month = u32::try_from(tm.tm_mon + 1).ok()?;
    let day = u32::try_from(tm.tm_mday).ok()?;
    Some((tm.tm_year.checked_add(1900)?, month, day))
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

// --------------------------------------------------------------------------
// The launcher's signal forwarding

/// Where a forwarded signal goes (`launch._supervise`'s `forward`): 0 before the child is
/// named (the signal is kept as pending), its pid while it runs, -1 once it has exited (the
/// signal is dropped: once reaped, the pid may be another process's).
static FORWARD_TO: AtomicI32 = AtomicI32::new(0);
/// The signals that arrived before the child was named, in order (at most `MAX_PENDING`).
const MAX_PENDING: usize = 8;
static PENDING: [AtomicI32; MAX_PENDING] = [const { AtomicI32::new(0) }; MAX_PENDING];
static PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);
/// How many signals the handler has handled, whatever it did with them; and how many of them
/// it sent the child.
static HANDLED: AtomicUsize = AtomicUsize::new(0);
static FORWARDED: AtomicUsize = AtomicUsize::new(0);
/// Whether a `Forwarding` is installed: there is one handler per process.
static FORWARDING: AtomicBool = AtomicBool::new(false);

/// The handler `forward_signals` installs. It runs in signal context, so it only reads and
/// writes atomics and makes one `kill(2)` call, all async-signal-safe; `errno` is kept for
/// the code it interrupted (rustix calls libc's `kill` on macOS, which may set it).
extern "C" fn forward(sig: c_int) {
    let saved = errno::get();
    match FORWARD_TO.load(Ordering::SeqCst) {
        0 => {
            let at = PENDING_COUNT.fetch_add(1, Ordering::SeqCst);
            if let Some(slot) = PENDING.get(at) {
                slot.store(sig, Ordering::SeqCst);
            }
        }
        pid if pid > 0 => {
            // `pid` is the child, which is not reaped until `child_exited` has set `FORWARD_TO`
            // to -1 (the caller waits with `WNOWAIT` first), so it cannot be another process's
            // pid. `kill(2)` is async-signal-safe, and the conversions allocate nothing. A
            // failure (the child is a zombie) is ignored, as the Python ignores
            // ProcessLookupError.
            if let (Some(pid), Some(sig)) = (Pid::from_raw(pid), Signal::from_named_raw(sig)) {
                let _ = rustix::process::kill_process(pid, sig);
            }
            FORWARDED.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    }
    HANDLED.fetch_add(1, Ordering::SeqCst);
    errno::set(saved);
}

/// `errno`, read and restored around the handler's `kill`.
mod errno {
    #[cfg(target_os = "linux")]
    use libc::__errno_location as location;
    #[cfg(target_os = "macos")]
    use libc::__error as location;

    pub(super) fn get() -> libc::c_int {
        // SAFETY: `location()` returns this thread's own `errno` slot, valid for the life of
        // the thread; reading an `int` from it is what reading `errno` means.
        unsafe { *location() }
    }

    pub(super) fn set(value: libc::c_int) {
        // SAFETY: as in `get`: the pointer is this thread's own `errno`, writable.
        unsafe { *location() = value };
    }
}

/// SIGINT and SIGTERM (or whichever `signals`) forwarded to the launcher's child, as
/// `launch._supervise` forwards them: kept as pending until `forward_to` names the child,
/// then sent to it, and dropped once `child_exited` says it has exited. Dropping this
/// restores the dispositions it replaced, as the Python restores its previous handlers.
pub struct Forwarding {
    previous: Vec<(Signal, libc::sigaction)>,
}

/// Installs the forwarding handler for `signals`. An error when one is installed already
/// (there is one handler per process) or `sigaction(2)` fails; nothing stays installed then.
pub fn forward_signals(signals: &[Signal]) -> io::Result<Forwarding> {
    if FORWARDING.swap(true, Ordering::SeqCst) {
        return Err(io::Error::other("signal forwarding is installed already"));
    }
    FORWARD_TO.store(0, Ordering::SeqCst);
    PENDING_COUNT.store(0, Ordering::SeqCst);
    for slot in &PENDING {
        slot.store(0, Ordering::SeqCst);
    }
    let mut installed = Forwarding {
        previous: Vec::new(),
    };
    for &sig in signals {
        // SAFETY: an all-zero `sigaction` is a valid value of the plain C struct (no handler,
        // an empty mask, no flags); the fields that matter are set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = forward as extern "C" fn(c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: as above, for the struct that receives the previous action.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `action.sa_mask` is a valid `sigset_t` owned here; `sigemptyset` only
        // writes it. Then `sigaction(2)` reads `action` and writes `old`, both live locals.
        // The handler it installs (`forward`) is async-signal-safe (see there).
        let failed = unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig.as_raw(), &action, &mut old) != 0
        };
        if failed {
            let err = io::Error::last_os_error();
            drop(installed);
            return Err(err);
        }
        installed.previous.push((sig, old));
    }
    Ok(installed)
}

impl Forwarding {
    /// The signals that arrived before the child was named, in order.
    pub fn pending(&self) -> Vec<Signal> {
        let count = PENDING_COUNT.load(Ordering::SeqCst).min(MAX_PENDING);
        PENDING[..count]
            .iter()
            .filter_map(|slot| Signal::from_named_raw(slot.load(Ordering::SeqCst)))
            .collect()
    }

    /// Names the child: every signal from now on is sent to `pid`. Returns the signals that
    /// arrived before, for the caller to send it (`for signum in pending: forward(signum)`).
    pub fn forward_to(&self, pid: i32) -> Vec<Signal> {
        FORWARD_TO.store(pid, Ordering::SeqCst);
        self.pending()
    }

    /// The child has exited and is about to be reaped: signals from now on are dropped
    /// (`if child.returncode is not None: return`).
    pub fn child_exited(&self) {
        FORWARD_TO.store(-1, Ordering::SeqCst);
    }

    /// How many signals the handler has handled so far, and how many of them it sent the
    /// child (the signals `forward_to` returns are the caller's to send, and not counted).
    pub fn counts(&self) -> (usize, usize) {
        (
            HANDLED.load(Ordering::SeqCst),
            FORWARDED.load(Ordering::SeqCst),
        )
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        for (sig, old) in self.previous.drain(..).rev() {
            // SAFETY: `old` is the action `sigaction(2)` returned for `sig`, restored as it
            // was; the pointer for the replaced action is null, which the call allows.
            unsafe { libc::sigaction(sig.as_raw(), &old, std::ptr::null_mut()) };
        }
        FORWARD_TO.store(0, Ordering::SeqCst);
        FORWARDING.store(false, Ordering::SeqCst);
    }
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
            Some("forwarding") => forwarding_verdict(),
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

    /// The launcher's forwarding, in a child test process (a signal to the whole process):
    /// whether a second install is refused, the signals pending before a child is named and
    /// those `forward_to` returns, how the named child ended, whether a child named after
    /// `child_exited` was spared, the handler's counts, and whether SIGTERM's default
    /// disposition came back.
    fn forwarding_verdict() -> String {
        let fwd = forward_signals(&[Signal::INT, Signal::TERM]).unwrap();
        let twice = forward_signals(&[Signal::TERM]).is_err();
        let raise = |sig: Signal| {
            let (before, _) = fwd.counts();
            rustix::process::kill_process(getpid(), sig).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while fwd.counts().0 == before && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let raw = |sigs: Vec<Signal>| sigs.iter().map(|s| s.as_raw()).collect::<Vec<_>>();
        // Before a child: kept, and the process lives on.
        raise(Signal::INT);
        let pending = raw(fwd.pending());
        let mut kid = sleeper(false);
        let to_send = raw(fwd.forward_to(kid.pid()));
        raise(Signal::TERM);
        let ended = kid.0.wait().unwrap().signal();
        // A child that has exited (as far as the handler knows) gets nothing.
        let mut spared = sleeper(false);
        fwd.forward_to(spared.pid());
        fwd.child_exited();
        raise(Signal::TERM);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let alive = spared.0.try_wait().unwrap().is_none();
        let (handled, forwarded) = fwd.counts();
        drop(fwd);
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct; `sigaction(2)`
        // with a null new action only writes the current one into `now`, a live local.
        let restored = unsafe {
            let mut now: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut now);
            now.sa_sigaction == libc::SIG_DFL
        };
        format!(
            "{twice} {pending:?} {to_send:?} {ended:?} {alive} {handled} {forwarded} {restored}"
        )
    }

    #[test]
    fn signals_are_forwarded_to_the_child_only_while_it_runs() {
        assert_eq!(
            start("forwarding"),
            format!(
                "true [{int}] [{int}] Some({term}) true 3 1 true",
                int = libc::SIGINT,
                term = libc::SIGTERM
            )
        );
    }

    #[test]
    fn local_date_is_pythons_local_date() {
        // The current time, and times around year and day ends, in this process's zone and in
        // two fixed ones, as Python's date.fromtimestamp gives them.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let times = [
            now,
            0,
            951_782_400,
            1_767_225_599,
            1_767_225_600,
            1_794_268_800,
        ];
        let code = "import sys, time\nfrom datetime import date\n\
                    print(' '.join(date.fromtimestamp(int(t)).isoformat() for t in sys.argv[1:]))";
        let args: Vec<String> = times.iter().map(i64::to_string).collect();
        let theirs = Command::new("python3")
            .args(["-B", "-c", code])
            .args(&args)
            .output()
            .unwrap();
        assert!(theirs.status.success());
        let ours: Vec<String> = times
            .iter()
            .map(|t| {
                let (y, m, d) = local_date(*t).unwrap();
                format!("{y:04}-{m:02}-{d:02}")
            })
            .collect();
        assert_eq!(
            ours.join(" "),
            String::from_utf8(theirs.stdout).unwrap().trim_end()
        );
        // In a child with TZ set, so this process's own zone is left alone: Tokyo is a day
        // ahead of UTC at 20:00 UTC, Honolulu a day behind at 05:00 UTC.
        for (tz, t, want) in [
            ("Asia/Tokyo", 1_767_211_200, "2026-01-01"),
            ("Pacific/Honolulu", 1_767_243_600, "2025-12-31"),
            ("UTC", 1_767_225_599, "2025-12-31"),
        ] {
            let out = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sys::tests::local_date_in_child", "--nocapture"])
                .env("TZ", tz)
                .env("LEADHAND_LOCAL_DATE_AT", t.to_string())
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                text.contains(&format!("local date: {want}\n")),
                "{tz}: {text}"
            );
        }
    }

    /// Prints the local date at `$LEADHAND_LOCAL_DATE_AT`; run by the test above in a child.
    #[test]
    fn local_date_in_child() {
        if let Some(t) = std::env::var_os("LEADHAND_LOCAL_DATE_AT") {
            let (y, m, d) = local_date(t.to_str().unwrap().parse().unwrap()).unwrap();
            println!("local date: {y:04}-{m:02}-{d:02}");
        }
    }

    #[test]
    fn inherited_descriptors_are_kept_from_children() {
        // Before: a child inherits the descriptor. After: it does not, and it is marked
        // close-on-exec, still open here.
        assert_eq!(start("descriptors"), "true false true");
    }
}
