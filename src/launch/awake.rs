//! Keep-awake beside the supervisor (`launch._keep_awake_argv`, R27): `caffeinate -i -w
//! <pid>` on macOS; on Linux with `systemd-inhibit` on PATH, `systemd-inhibit --what=idle
//! --why="leadhand run" tail --pid=<pid> -f /dev/null`; nothing elsewhere. It never wraps the
//! supervisor, which stays the launcher's direct child, and it never holds `run.lock`: every
//! file Rust opens is close-on-exec, and only `claude`'s pre-exec step clears the flag on the
//! lock. Its stdin, stdout and stderr are `/dev/null`, so it never holds a caller's pipes.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

/// How long the keep-awake process gets to exit after SIGTERM, then after SIGKILL.
pub const KEEP_AWAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The platforms keep-awake knows (`sys.platform` as `run` reads it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Other,
}

/// This build's platform.
pub const PLATFORM: Platform = if cfg!(target_os = "macos") {
    Platform::MacOs
} else if cfg!(target_os = "linux") {
    Platform::Linux
} else {
    Platform::Other
};

/// What runs beside the supervisor `pid` and ends with it, with `which` as `shutil.which`;
/// empty when nothing does.
pub fn keep_awake_argv(
    platform: Platform,
    pid: i32,
    which: &dyn Fn(&str) -> Option<PathBuf>,
) -> Vec<OsString> {
    match platform {
        Platform::MacOs => match which("caffeinate") {
            Some(exe) => vec![exe.into(), "-i".into(), "-w".into(), pid.to_string().into()],
            None => Vec::new(),
        },
        Platform::Linux if which("systemd-inhibit").is_some() => [
            "systemd-inhibit",
            "--what=idle",
            "--why=leadhand run",
            "tail",
            &format!("--pid={pid}"),
            "-f",
            "/dev/null",
        ]
        .map(OsString::from)
        .to_vec(),
        _ => Vec::new(),
    }
}

/// Starts keep-awake beside the supervisor `pid`; `None` when there is none, or it cannot be
/// started.
pub fn start(pid: i32) -> Option<Child> {
    let argv = keep_awake_argv(PLATFORM, pid, &crate::pytext::which);
    let (program, args) = argv.split_first()?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Stops keep-awake (`_stop_keep_awake`): SIGTERM when it is still running, up to
/// `KEEP_AWAKE_TIMEOUT` for it to exit, then SIGKILL.
pub fn stop(child: Option<Child>) {
    let Some(mut child) = child else {
        return;
    };
    if matches!(child.try_wait(), Ok(None)) {
        // Not reaped yet, so the pid is still the child's.
        let _ = kill_process(Pid::from_child(&child), Signal::TERM);
    }
    let deadline = Instant::now() + KEEP_AWAKE_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str) -> Option<PathBuf> {
        Some(PathBuf::from(format!("/usr/bin/{name}")))
    }

    fn words(argv: Vec<OsString>) -> Vec<String> {
        argv.into_iter().map(|w| w.into_string().unwrap()).collect()
    }

    #[test]
    fn keep_awake_prefix_per_platform() {
        assert_eq!(
            words(keep_awake_argv(Platform::MacOs, 42, &found)),
            ["/usr/bin/caffeinate", "-i", "-w", "42"]
        );
        assert_eq!(
            words(keep_awake_argv(Platform::Linux, 42, &found)),
            [
                "systemd-inhibit",
                "--what=idle",
                "--why=leadhand run",
                "tail",
                "--pid=42",
                "-f",
                "/dev/null"
            ]
        );
        // Not on PATH, or another platform: nothing.
        for platform in [Platform::MacOs, Platform::Linux] {
            assert!(keep_awake_argv(platform, 42, &|_| None).is_empty());
        }
        assert!(keep_awake_argv(Platform::Other, 42, &found).is_empty());
        // Each platform looks only for its own tool.
        let only =
            |want: &'static str| move |name: &str| (name == want).then(|| found(name)).flatten();
        assert!(keep_awake_argv(Platform::MacOs, 1, &only("systemd-inhibit")).is_empty());
        assert!(keep_awake_argv(Platform::Linux, 1, &only("caffeinate")).is_empty());
    }

    #[test]
    fn stop_ends_a_running_keep_awake_and_tolerates_one_that_exited() {
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = Instant::now();
        stop(Some(child));
        assert!(started.elapsed() < KEEP_AWAKE_TIMEOUT, "stopped by SIGTERM");
        assert!(crate::procs::identify(i32::try_from(pid).unwrap()).is_none());
        let mut done = Command::new("true").spawn().unwrap();
        done.wait().unwrap();
        stop(Some(done));
        stop(None);
    }
}
