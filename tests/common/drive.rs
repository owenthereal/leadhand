//! Forked drives without `fork`: the test binary re-executed as a child that runs only the
//! calling test, in a process group of its own (ruling S3-R23).
//!
//! Python's contract suite drives a world in an `os.fork()` child that is its own process
//! group (`contract_smoke.CrashDrive`, `contract_recovery`'s crash runs). The Rust form starts
//! `current_exe() --exact <test> --nocapture` with `process_group(0)`, selected by
//! LEADHAND_TEST_DRIVE, which is set on the child's `Command` only (the test process's own
//! environment is never changed, S3-R19). The same test code runs again in the child; it
//! sees `selected()`, picks up what the parent wrote to the selector's `payload` file, runs
//! the drive (`run_child`) and ends the process. So the child dies at once when its group
//! is killed, every process it starts is in its group whatever starts it, nothing it changes
//! reaches the parent, and its exit status is a real one.
//!
//! The selector variable starts with LEADHAND_, so `Env` drops it: the binary never sees it.

use std::fs::{self, File};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};

use super::json::{self, Json};

/// The variable that makes a re-executed test binary a drive child: its selector, as JSON.
pub const SELECT_VAR: &str = "LEADHAND_TEST_DRIVE";

/// Which drive a child runs: the test (its libtest name), the drive's label within it, and a
/// file the parent wrote for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    pub test: String,
    pub label: String,
    pub payload: PathBuf,
}

impl Selector {
    fn to_json(&self) -> String {
        json::dumps(&Json::object([
            ("test", Json::from(self.test.as_str())),
            ("label", Json::from(self.label.as_str())),
            (
                "payload",
                Json::Str(json::from_os(self.payload.as_os_str().as_encoded_bytes())),
            ),
        ]))
    }

    fn from_json(text: &[u8]) -> Selector {
        let v = json::parse(text);
        let s = |k: &str| {
            v[k].as_str()
                .unwrap_or_else(|| panic!("{SELECT_VAR}: no {k}"))
                .to_string()
        };
        Selector {
            test: s("test"),
            label: s("label"),
            payload: PathBuf::from(
                v["payload"]
                    .as_os()
                    .unwrap_or_else(|| panic!("{SELECT_VAR}: no payload")),
            ),
        }
    }
}

/// In a drive child, the selector it was started with; None in an ordinary test run.
pub fn selected() -> Option<Selector> {
    let value = std::env::var_os(SELECT_VAR).filter(|v| !v.is_empty())?;
    Some(Selector::from_json(&value.into_vec()))
}

/// In a drive child for the drive `label`, its selector; None otherwise (an ordinary run,
/// or a child started for another drive of a test that has several).
pub fn selected_for(label: &str) -> Option<Selector> {
    selected().filter(|s| s.label == label)
}

/// The running test's name as libtest names it (`module::test_fn`): its thread's name.
#[track_caller]
pub fn this_test() -> String {
    match thread::current().name() {
        Some(name) if name != "main" => name.to_string(),
        _ => panic!("not on a libtest test thread: name the test for the drive child"),
    }
}

/// A drive child that is running: the test binary again, in a process group of its own.
pub struct DriveChild {
    child: Child,
    output: PathBuf,
    test: String,
}

/// How a drive child ended.
#[derive(Clone, Debug)]
pub struct Ended {
    /// Its exit status as `Out.code` gives one: the exit code, or minus the signal.
    pub code: i32,
    /// True when it ran past its deadline and its group was killed.
    pub hung: bool,
    /// Everything it printed (stdout and stderr, interleaved).
    pub output: String,
}

/// Starts the test binary as a drive child for `sel`: only `sel.test` runs (`--exact`), in a
/// process group of its own, with SELECT_VAR set on its `Command` only and its output
/// written to `output`.
#[track_caller]
pub fn start(sel: &Selector, output: &Path) -> DriveChild {
    let exe = std::env::current_exe().expect("the test binary's path");
    let out = File::create(output).unwrap_or_else(|e| panic!("creating {}: {e}", output.display()));
    let err = out.try_clone().expect("duplicating the output file");
    let child = Command::new(exe)
        .args(["--exact", &sel.test, "--nocapture", "--test-threads", "1"])
        .env(SELECT_VAR, sel.to_json())
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("starting the drive child for {}: {e}", sel.test));
    DriveChild {
        child,
        output: output.to_path_buf(),
        test: sel.test.clone(),
    }
}

impl DriveChild {
    /// The child's pid, which is also its process group's id.
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// SIGKILL to the child's whole group (while the child is unreaped, its group id cannot
    /// have been reused).
    pub fn kill_group(&self) {
        if let Some(group) = Pid::from_raw(self.pid()) {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }

    /// Waits for the child to end, at most `deadline`: past it the child's group is killed
    /// and `hung` set. Panics when the child ran no test (a test name `--exact` misses).
    #[track_caller]
    pub fn wait(mut self, deadline: Duration) -> Ended {
        let end = Instant::now() + deadline;
        let mut hung = false;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("polling the drive child") {
                break status;
            }
            if Instant::now() >= end {
                hung = true;
                self.kill_group();
                break self.child.wait().expect("waiting for the drive child");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let code = status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0));
        let output =
            String::from_utf8_lossy(&fs::read(&self.output).unwrap_or_default()).into_owned();
        if !output.contains("running 1 test") {
            panic!(
                "the drive child ran no test named {:?} (exit code {code}):\n{output}",
                self.test
            );
        }
        Ended { code, hung, output }
    }
}

/// In a drive child: runs `f` and ends the process, never returning to the rest of the test.
/// A return exits 0; a panic writes its message to `error` and exits 1 (Python's child
/// writing its traceback). `f` may end the process itself (a crash point's exit 97).
pub fn run_child(error: &Path, f: impl FnOnce()) -> ! {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(()) => std::process::exit(0),
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "(a panic without a message)".to_string());
            let _ = fs::write(error, message);
            std::process::exit(1);
        }
    }
}
