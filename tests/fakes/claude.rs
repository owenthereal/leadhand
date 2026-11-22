//! A fake `claude` for leadhand's launcher tests: the Rust form of the Python fake it replaced,
//! writing the same log lines byte for byte for the same inputs. Never touches the network.
//!
//! Appends one JSON line to $FAKE_CLAUDE_LOG when it starts: its argv, cwd, pid, ppid and
//! the LEADHAND_* environment variables the launcher sets (null when unset). Every line also
//! has "time", the wall-clock time it was written (as Python's `time.time()`), and each is
//! fsynced. Then it sleeps $FAKE_CLAUDE_SLEEP seconds (default 0) and exits
//! $FAKE_CLAUDE_EXIT (default 0).
//!
//! argv[0] is the path the binary was started by, as the Python script's `sys.argv[0]` is:
//! a bare name (found on PATH) is the PATH entry that holds it.
//!
//! With $FAKE_CLAUDE_SPAWN (a JSON argv list) it first starts that command as a child that
//! inherits every open descriptor (as Claude Code's Bash commands do), does not wait for
//! it, and appends {"pid": ..., "spawned": <child pid>}.
//!
//! On SIGINT or SIGTERM it appends {"pid": ..., "signal": N} and exits 128 + N.

#[allow(dead_code)]
#[path = "../common/json.rs"]
mod json;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use json::Json;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const ENV_NAMES: [&str; 3] = ["LEADHAND_RUN_ID", "LEADHAND_RUN_DIR", "LEADHAND_CLI"];

/// Held while a log line is written, and by the signal handler until it exits, so no line
/// is cut short and none follows the signal's line.
static LOG: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends the process as Python ends on an uncaught exception: the message on stderr, exit 1.
fn die(message: impl std::fmt::Display) -> ! {
    eprintln!("fake claude: {message}");
    process::exit(1);
}

fn os_json(s: &OsStr) -> Json {
    Json::Str(json::from_os(s.as_bytes()))
}

fn pid() -> Json {
    Json::from(i64::from(process::id()))
}

/// Appends `{"time": ..., <fields>}` to $FAKE_CLAUDE_LOG and fsyncs it; the caller holds LOG.
fn write_log(fields: Vec<(&str, Json)>) {
    let Some(path) = env::var_os("FAKE_CLAUDE_LOG").filter(|p| !p.is_empty()) else {
        return;
    };
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let mut pairs = vec![("time", Json::Num(json::json_float(time)))];
    pairs.extend(fields);
    let line = json::dumps(&Json::object(pairs)) + "\n";
    let written = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            f.write_all(line.as_bytes())?;
            f.flush()?;
            rustix::fs::fsync(&f)?;
            Ok(())
        });
    if let Err(e) = written {
        die(format_args!("writing {}: {e}", path.to_string_lossy()));
    }
}

fn log(fields: Vec<(&str, Json)>) {
    let _held = lock();
    write_log(fields);
}

/// The path this binary was started by, as a script's `sys.argv[0]` is (the path the kernel
/// was given): `arg0` when it holds a slash, else the first PATH entry holding an
/// executable `arg0`, joined as Python's `os.path.join` joins.
fn started_as(arg0: &OsStr) -> OsString {
    if arg0.as_bytes().contains(&b'/') {
        return arg0.to_os_string();
    }
    let executable = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(path) = env::var_os("PATH") {
        for dir in path.as_bytes().split(|&b| b == b':') {
            let candidate = if dir.is_empty() {
                PathBuf::from(arg0)
            } else {
                Path::new(OsStr::from_bytes(dir)).join(arg0)
            };
            if executable(&candidate) {
                return candidate.into_os_string();
            }
        }
    }
    env::current_exe().map_or_else(|_| arg0.to_os_string(), PathBuf::into_os_string)
}

/// Python's `float(os.environ.get(name, "0") or 0)` and `int(...)`, as text.
fn number_env(name: &str) -> String {
    let value = env::var_os(name).unwrap_or_default();
    let text = value.to_string_lossy().trim().replace('_', "");
    if text.is_empty() {
        "0".to_string()
    } else {
        text
    }
}

// The Python fake never waits for the child either: it stays a zombie until this exits.
#[allow(clippy::zombie_processes)]
fn spawn(text: &OsStr) {
    let argv: Vec<OsString> = match json::try_parse(text.as_bytes()) {
        Ok(Json::Arr(items)) => items
            .iter()
            .map(|item| item.as_bytes().and_then(json::to_os))
            .collect::<Option<_>>()
            .unwrap_or_else(|| die("FAKE_CLAUDE_SPAWN: not a list of strings")),
        Ok(other) => die(format_args!("FAKE_CLAUDE_SPAWN: not a list: {other}")),
        Err(e) => die(format_args!("FAKE_CLAUDE_SPAWN: not JSON ({e})")),
    };
    let Some((program, args)) = argv.split_first() else {
        die("FAKE_CLAUDE_SPAWN: an empty list")
    };
    // Every open descriptor is inherited: this binary opens nothing without close-on-exec.
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(format_args!("FAKE_CLAUDE_SPAWN: {e}")));
    log(vec![
        ("pid", pid()),
        ("spawned", Json::from(i64::from(child.id()))),
    ]);
    // Never waited for; the Child is dropped without killing it.
}

fn main() {
    let mut signals = Signals::new([SIGINT, SIGTERM])
        .unwrap_or_else(|e| die(format_args!("signal handler: {e}")));
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let _held = lock();
            write_log(vec![("pid", pid()), ("signal", Json::from(signal))]);
            process::exit(128 + signal);
        }
    });

    let args: Vec<OsString> = env::args_os().collect();
    let mut argv = vec![os_json(&started_as(
        args.first().map_or(OsStr::new(""), |a| a.as_os_str()),
    ))];
    argv.extend(args.iter().skip(1).map(|a| os_json(a)));
    let ppid = rustix::process::getppid().map_or(0, |p| p.as_raw_pid());
    let cwd = env::current_dir().unwrap_or_else(|e| die(format_args!("getcwd: {e}")));
    let leadhand_env =
        ENV_NAMES.map(|name| (name, env::var_os(name).map_or(Json::Null, |v| os_json(&v))));
    log(vec![
        ("pid", pid()),
        ("ppid", Json::from(ppid)),
        ("argv", Json::Arr(argv)),
        ("cwd", os_json(cwd.as_os_str())),
        ("env", Json::object(leadhand_env)),
    ]);
    if let Some(text) = env::var_os("FAKE_CLAUDE_SPAWN").filter(|t| !t.is_empty()) {
        spawn(&text);
    }
    let sleep = number_env("FAKE_CLAUDE_SLEEP");
    let secs: f64 = sleep
        .parse()
        .unwrap_or_else(|_| die(format_args!("FAKE_CLAUDE_SLEEP={sleep}")));
    // As the Python's deadline loop: nothing for a negative or NaN length, forever for inf.
    if secs > 0.0 {
        loop {
            thread::sleep(Duration::from_secs_f64(secs.min(86_400.0)));
            if secs.is_finite() {
                break;
            }
        }
    }
    let exit = number_env("FAKE_CLAUDE_EXIT");
    let code: i64 = exit
        .parse()
        .unwrap_or_else(|_| die(format_args!("FAKE_CLAUDE_EXIT={exit}")));
    process::exit(code as i32);
}
