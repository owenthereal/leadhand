//! The contract suite's harness: it reaches leadhand only as a subprocess at LEADHAND_BIN.
//! The Rust form of the Python contract suite's `harness`.
//!
//! - LEADHAND_BIN: the binary under test (default the testing build, `CARGO_BIN_EXE_leadhand`).
//!   A relative value is resolved against the test process's working directory, the package
//!   root (ruling S3-R5), so `LEADHAND_BIN=target/release/leadhand` works.
//!
//! Tests run in parallel (ruling S3-R19): nothing here changes the test process's own
//! environment or relies on running alone. Each test makes its own `TempRoot`, and `Env`
//! hands each child its environment explicitly. Nothing here uses the crate's library except
//! `leadhand::procs`, to check whether a process is alive.

#![allow(dead_code, unused_macros)]

pub mod dispatch_world;
pub mod drive;
pub mod engine;
pub mod git;
pub mod json;
pub mod launch_world;
pub mod plugin;
#[macro_use]
pub mod recovery;
pub mod rundir;
pub mod rust_src;
pub mod supervisor;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::ops::Deref;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::{Pid, Signal, kill_process_group};

use json::Json;

/// `[&OsStr; N]` from values that are each `AsRef<OsStr>` (`&str`, `String`, `&Path`,
/// `PathBuf`, `OsString`), for `cli` and `run_argv`: `cli(&os_args!["next", run_dir], ...)`.
macro_rules! os_args {
    ($($arg:expr),* $(,)?) => {
        [$(::std::convert::AsRef::<::std::ffi::OsStr>::as_ref(&$arg)),*]
    };
}

/// Evaluates `expr`, which must panic (a failed assertion inside the harness), and gives the
/// panic's message; panics itself if `expr` returns. An assertion: a macro, so that the
/// conversion check, which compared assertion counts with the Python tests' until the
/// cut-over, counted it as one.
macro_rules! assert_panics {
    ($e:expr) => {
        $crate::common::panic_message(|| {
            let _ = $e;
        })
        .unwrap_or_else(|| panic!("expected a panic from `{}`", stringify!($e)))
    };
}

/// `assert_eq!` for parsed JSON with Python's equality (`json::py_eq`): objects compare as
/// dicts (duplicate keys collapsed, last value winning, in any order) and numbers by value
/// (`1 == 1.0`, `1.5 == 1.50`). Use it wherever a Python test compares parsed JSON with
/// `assertEqual`; the derived `==` on `Json` is stricter. Each side is a `Json` or `&Json`;
/// an optional message follows, as for `assert_eq!`.
macro_rules! assert_json_eq {
    ($left:expr, $right:expr $(,)?) => {
        assert_json_eq!($left, $right, "")
    };
    ($left:expr, $right:expr, $($arg:tt)+) => {{
        let left: &$crate::common::json::Json = &$left;
        let right: &$crate::common::json::Json = &$right;
        if !$crate::common::json::py_eq(left, right) {
            panic!(
                "assertion `left == right` failed (Python equality of JSON) {}\n  left: {left}\n right: {right}",
                format_args!($($arg)+)
            );
        }
    }};
}

/// `assertEqual(items, [])` for a list (a `Vec` or slice): it must be empty, and the message
/// shows what it holds.
macro_rules! assert_empty {
    ($items:expr $(,)?) => {{
        let items = &$items;
        assert!(items.is_empty(), "expected none, got {items:?}");
    }};
}

/// The message of the panic `f` raises, or None when it returns.
pub fn panic_message(f: impl FnOnce()) -> Option<String> {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).err()?;
    Some(
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "(a panic without a message)".to_string()),
    )
}

/// The default timeout of a command: 120 s.
pub const TIMEOUT: Duration = Duration::from_secs(120);

const FAKE_CODEX: &str = env!(
    "CARGO_BIN_EXE_leadhand-fake-codex",
    "the integration tests need --features testing"
);
const FAKE_CLAUDE: &str = env!(
    "CARGO_BIN_EXE_leadhand-fake-claude",
    "the integration tests need --features testing"
);

// --------------------------------------------------------------------------
// The binary

/// The binary under test: LEADHAND_BIN, made absolute against the working directory, else
/// the testing build.
pub fn bin() -> PathBuf {
    match std::env::var_os("LEADHAND_BIN").filter(|v| !v.is_empty()) {
        Some(value) => std::path::absolute(&value)
            .unwrap_or_else(|e| panic!("LEADHAND_BIN={}: {e}", value.to_string_lossy())),
        None => PathBuf::from(env!("CARGO_BIN_EXE_leadhand")),
    }
}

// --------------------------------------------------------------------------
// Running the binary

/// What a command did. Its Display names the argv, the exit code and both outputs.
#[derive(Clone, Debug)]
pub struct Out {
    pub argv: Vec<OsString>,
    /// The exit code; minus the signal number when a signal ended it.
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl fmt::Display for Out {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = |b: &[u8]| {
            if b.is_empty() {
                "(empty)".to_string()
            } else {
                String::from_utf8_lossy(b).into_owned()
            }
        };
        write!(
            f,
            "argv: {}\nexit code: {}\nstdout:\n{}\nstderr:\n{}",
            shlex_join(&self.argv),
            self.code,
            text(&self.stdout),
            text(&self.stderr)
        )
    }
}

impl Out {
    /// `self` when the command exited 0; else panics naming the argv, code and outputs.
    #[track_caller]
    pub fn ok(self) -> Out {
        if self.code != 0 {
            panic!(
                "leadhand failed: exit code {}, expected 0\n{self}",
                self.code
            );
        }
        self
    }

    /// stdout as JSON; panics naming the argv, code and outputs when it is not JSON.
    #[track_caller]
    pub fn json(&self) -> Json {
        json::try_parse(&self.stdout)
            .unwrap_or_else(|e| panic!("leadhand's stdout is not JSON ({e})\n{self}"))
    }

    /// stdout as text (invalid UTF-8 replaced).
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr as text (invalid UTF-8 replaced).
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Runs the binary with `args`, from `cwd`, with no stdin and the 120 s timeout. See
/// `run_argv`.
#[track_caller]
pub fn cli(args: &[&OsStr], env: &Env, cwd: &Path) -> Out {
    cli_with(args, env, cwd, None, TIMEOUT)
}

/// `cli` with `stdin` written to the binary's standard input.
#[track_caller]
pub fn cli_stdin(args: &[&OsStr], env: &Env, cwd: &Path, stdin: &str) -> Out {
    cli_with(args, env, cwd, Some(stdin), TIMEOUT)
}

/// `cli` with an optional stdin and a timeout of its own.
#[track_caller]
pub fn cli_with(
    args: &[&OsStr],
    env: &Env,
    cwd: &Path,
    stdin: Option<&str>,
    timeout: Duration,
) -> Out {
    let bin = bin();
    let mut argv = vec![bin.as_os_str()];
    argv.extend_from_slice(args);
    run_argv(&argv, env, cwd, stdin, timeout)
}

/// Runs a full command line (such as a dispatch line the binary printed) as `cli` runs the
/// binary: in its own process group, so a run past `timeout` kills the children that hold
/// the pipes too, then panics naming the command. In a crash drive (`env` names
/// LEADHAND_CRASH_KILL_PGID) the command stays in the caller's group instead, so a crash
/// point's group kill takes it down with the drive, and a timeout kills only the command.
#[track_caller]
pub fn run_argv(
    argv: &[&OsStr],
    env: &Env,
    cwd: &Path,
    stdin: Option<&str>,
    timeout: Duration,
) -> Out {
    run_argv_spawn(argv, env, cwd, stdin, timeout, Spawn::default_for(env))
}

/// The process group a command starts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// A group of its own (`process_group(0)`): a timeout kills the whole group.
    Own,
    /// The caller's group (in a drive child, the drive's): a timeout kills only the command.
    Inherit,
}

/// How `run_argv_spawn` starts a command: its process group, and a hook called with the
/// pid of the process once it has started (a fake supervisor's log of what it started).
#[derive(Clone, Copy)]
pub struct Spawn<'a> {
    pub group: Group,
    pub on_spawn: Option<&'a dyn Fn(u32)>,
}

impl Spawn<'_> {
    /// `run_argv`'s choice for `env`: a group of its own, or the caller's in a crash drive
    /// (`env` names LEADHAND_CRASH_KILL_PGID).
    pub fn default_for(env: &Env) -> Spawn<'static> {
        let group = if env.get("LEADHAND_CRASH_KILL_PGID").is_some() {
            Group::Inherit
        } else {
            Group::Own
        };
        Spawn {
            group,
            on_spawn: None,
        }
    }
}

/// `run_argv` with the process group and spawn hook `spawn` gives. A run past `timeout`
/// kills the command's group when it has one of its own (`Group::Own`), else only the
/// command, then panics naming the command.
#[track_caller]
pub fn run_argv_spawn(
    argv: &[&OsStr],
    env: &Env,
    cwd: &Path,
    stdin: Option<&str>,
    timeout: Duration,
    spawn: Spawn,
) -> Out {
    match run_process(
        argv,
        env.vars(),
        cwd,
        stdin.map(str::as_bytes),
        timeout,
        spawn,
    ) {
        Ok(out) => out,
        Err(out) => {
            let killed = if spawn.group == Group::Own {
                "its process group was"
            } else {
                "it was"
            };
            panic!(
                "leadhand timed out after {} s; {killed} killed\n{out}",
                seconds_g(timeout)
            );
        }
    }
}

/// A duration in seconds as Python's `{:g}` prints it: `1`, `120`, `0.5`.
fn seconds_g(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        d.as_secs().to_string()
    } else {
        d.as_secs_f64().to_string()
    }
}

/// Runs `argv` with exactly the environment `vars`, collecting both outputs. Err when it ran
/// past `timeout`: it (or its own group) was killed, and the output is whatever came before
/// the pipes closed, or a note when a process outside the group held them for another 5 s.
/// Panics when it cannot be started (`try_run_process` returns that error instead).
fn run_process<K, V>(
    argv: &[&OsStr],
    vars: impl IntoIterator<Item = (K, V)>,
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
    spawn: Spawn,
) -> Result<Out, Out>
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    try_run_process(argv, vars, cwd, stdin, timeout, spawn).unwrap_or_else(|e| {
        let argv: Vec<OsString> = argv.iter().map(|a| a.to_os_string()).collect();
        panic!(
            "could not start {} in {}: {e}",
            shlex_join(&argv),
            cwd.display()
        )
    })
}

/// `run_process`, with the error when `argv` cannot be started (Python's `OSError` from
/// `subprocess.run`) instead of a panic: for callers that record it, as the live drivers do.
pub fn try_run_process<K, V>(
    argv: &[&OsStr],
    vars: impl IntoIterator<Item = (K, V)>,
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
    spawn: Spawn,
) -> std::io::Result<Result<Out, Out>>
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    Ok(
        match run_process_until(argv, vars, cwd, stdin, timeout, spawn, &|| false)? {
            Finished::Done(out) => Ok(out),
            Finished::TimedOut(out) | Finished::Stopped(out) => Err(out),
        },
    )
}

/// How `run_process_until` ended.
#[derive(Debug)]
pub enum Finished {
    /// The command exited.
    Done(Out),
    /// It ran past its timeout and was killed (its group, with `Group::Own`).
    TimedOut(Out),
    /// `stop` turned true while it ran (an operator's Ctrl-C), and it was killed the same way.
    Stopped(Out),
}

/// `try_run_process`, also killing the command as soon as `stop` returns true (checked
/// every 100 ms). The kill is by identity: the child is not reaped before it, so neither its
/// pid nor its group's id can have been reused.
pub fn run_process_until<K, V>(
    argv: &[&OsStr],
    vars: impl IntoIterator<Item = (K, V)>,
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
    spawn: Spawn,
    stop: &dyn Fn() -> bool,
) -> std::io::Result<Finished>
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let argv_owned: Vec<OsString> = argv.iter().map(|a| a.to_os_string()).collect();
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(vars)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let own_group = spawn.group == Group::Own;
    match spawn.group {
        Group::Own => {
            cmd.process_group(0);
        }
        Group::Inherit => {}
    }
    let mut child = cmd.spawn()?;
    if let Some(hook) = spawn.on_spawn {
        hook(child.id());
    }
    let (tx, rx) = mpsc::channel::<(u8, Vec<u8>)>();
    let read = |n: u8, mut pipe: Box<dyn Read + Send>, tx: mpsc::Sender<(u8, Vec<u8>)>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send((n, buf));
        });
    };
    read(1, Box::new(child.stdout.take().unwrap()), tx.clone());
    read(2, Box::new(child.stderr.take().unwrap()), tx);
    if let (Some(mut pipe), Some(data)) = (child.stdin.take(), stdin) {
        let data = data.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&data); // dropped here: the command sees EOF
        });
    }
    let deadline = Instant::now() + timeout;
    let mut outputs: [Option<Vec<u8>>; 2] = [None, None];
    let stopped = std::cell::Cell::new(false);
    let collect = |until: Instant, outputs: &mut [Option<Vec<u8>>; 2], watch: bool| {
        while outputs.iter().any(Option::is_none) {
            if watch && stop() {
                stopped.set(true);
                return false;
            }
            let left = until.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok((n, buf)) => outputs[usize::from(n - 1)] = Some(buf),
                Err(mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {}
                Err(_) => return false,
            }
        }
        true
    };
    let finished = collect(deadline, &mut outputs, true) && wait_until(&mut child, deadline);
    if finished {
        let status = child.wait().expect("waiting for a finished child");
        let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
        return Ok(Finished::Done(Out {
            argv: argv_owned,
            code: code_of(status),
            stdout,
            stderr,
        }));
    }
    let pid = Pid::from_child(&child);
    if own_group {
        let _ = kill_process_group(pid, Signal::KILL);
    } else {
        let _ = child.kill();
    }
    let (stdout, stderr) = if collect(Instant::now() + Duration::from_secs(5), &mut outputs, false)
    {
        let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
        (stdout, stderr)
    } else {
        // A descendant left the group and holds the pipes.
        let _ = child.kill();
        let note = "(not read: a process outside the binary's group held the pipes)";
        (Vec::new(), note.as_bytes().to_vec())
    };
    let status = child.wait().expect("waiting for a killed child");
    let out = Out {
        argv: argv_owned,
        code: code_of(status),
        stdout,
        stderr,
    };
    Ok(if stopped.get() {
        Finished::Stopped(out)
    } else {
        Finished::TimedOut(out)
    })
}

/// The child's exit status once it has exited, polling until `deadline`; None past it.
fn wait_until(child: &mut Child, deadline: Instant) -> bool {
    let mut pause = Duration::from_millis(1);
    loop {
        if child.try_wait().expect("polling a child").is_some() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

/// An exit status as a code: the exit code, or minus the signal number when a signal ended
/// the process (Python's `returncode`).
pub fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(0))
}

// --------------------------------------------------------------------------
// The test's view of the process environment

thread_local! {
    static PATCHED: std::cell::RefCell<Vec<(OsString, OsString)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Variables laid over the process environment for the calling test's thread until the
/// guard drops: the Rust form of `mock.patch.dict(os.environ, vars)`. The process
/// environment itself is never changed (ruling S3-R19); `Env::new`/`with_options` and the
/// Git helpers read it through `process_env`, so an `Env` made, or a fixture Git command
/// run, while the guard lives sees `vars` too.
#[must_use = "the variables are unset when the guard drops"]
pub struct PatchedEnv {
    previous: Vec<(OsString, OsString)>,
}

pub fn patch_env(vars: &[(&str, &str)]) -> PatchedEnv {
    PATCHED.with(|p| {
        let mut p = p.borrow_mut();
        let previous = p.clone();
        p.extend(vars.iter().map(|(k, v)| (k.into(), v.into())));
        PatchedEnv { previous }
    })
}

impl Drop for PatchedEnv {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        PATCHED.with(|p| *p.borrow_mut() = previous);
    }
}

/// The process environment with this thread's `patch_env` variables over it (a later entry
/// of the same name wins).
pub fn process_env() -> Vec<(OsString, OsString)> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    PATCHED.with(|p| vars.extend(p.borrow().iter().cloned()));
    vars
}

// --------------------------------------------------------------------------
// Temporary roots and the sealed environment

/// A new directory in the system temp dir, its name containing a space; removed on drop.
pub struct TempRoot {
    dir: tempfile::TempDir,
}

impl TempRoot {
    pub fn new(prefix: &str) -> TempRoot {
        let prefix = if prefix.contains(' ') {
            prefix.to_string()
        } else {
            format!("{prefix} ")
        };
        let dir = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .unwrap_or_else(|e| panic!("making a temp root {prefix:?}: {e}"));
        TempRoot { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path()
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

/// The suite's own settings, passed through to the binary.
const SUITE_VARS: [&str; 1] = ["LEADHAND_BIN"];
/// Inherited variables that would steer the binary or the fakes: other LEADHAND_* names are
/// test hooks or set by leadhand itself.
const INHERITED_PREFIXES: [&str; 3] = ["LEADHAND_", "FAKE_CODEX_", "FAKE_CLAUDE_"];
const INHERITED_VARS: [&str; 1] = ["CLAUDE_CODE_SESSION_ID"];

fn inherited(name: &OsStr) -> bool {
    let name = name.as_bytes();
    if SUITE_VARS.iter().any(|v| v.as_bytes() == name) {
        return false;
    }
    INHERITED_VARS.iter().any(|v| v.as_bytes() == name)
        || INHERITED_PREFIXES
            .iter()
            .any(|p| name.starts_with(p.as_bytes()))
}

/// The environment for a command, handed to it whole (the test process's own is never
/// changed): the test process's environment, with Git sealed off from the operator's
/// configuration (`git::HERMETIC_ENV`, GIT_CEILING_DIRECTORIES at the temp root's parent),
/// inherited hooks dropped, and leadhand's configuration, Claude and Codex homes under the
/// temp root, as `harness.hermetic_env` builds it.
///
/// It also holds `fakes()`, a folder in the temp root with `codex` and `claude`, symlinks to
/// the fake binaries: the Rust form of `tests/fakes`. Like `tests/fakes` it is not on PATH
/// unless a test puts it there (`path_with_fakes`).
#[derive(Clone, Debug)]
pub struct Env {
    vars: BTreeMap<OsString, OsString>,
    fakes: PathBuf,
}

impl Env {
    /// The environment for a test whose temp root is `tmp`, with Claude installed.
    pub fn new(tmp: &Path) -> Env {
        Env::with_options(tmp, &["claude"], "")
    }

    /// `new` with the installed vendors and text appended to the user config.
    pub fn with_options(tmp: &Path, installed: &[&str], config_text: &str) -> Env {
        Env::from_base(process_env(), tmp, installed, config_text)
    }

    /// `with_options` over `base` in place of the test process's environment (a later entry
    /// replaces an earlier one of the same name).
    pub fn from_base(
        base: impl IntoIterator<Item = (OsString, OsString)>,
        tmp: &Path,
        installed: &[&str],
        config_text: &str,
    ) -> Env {
        let root = git::add_root(tmp);
        let mut vars: BTreeMap<OsString, OsString> = base
            .into_iter()
            .filter(|(k, _)| !git::LEAKY_VARS.iter().any(|v| OsStr::new(v) == k) && !inherited(k))
            .collect();
        for (k, v) in git::HERMETIC_ENV {
            vars.insert(k.into(), v.into());
        }
        vars.insert(
            "GIT_CEILING_DIRECTORIES".into(),
            root.parent().unwrap().into(),
        );
        let config = tmp.join("user config.toml");
        let mut text = b"docs_dir = \"".to_vec();
        text.extend_from_slice(tmp.join("docs dir").as_os_str().as_bytes());
        text.extend_from_slice(b"\"\n");
        text.extend_from_slice(config_text.as_bytes());
        fs::write(&config, text).unwrap_or_else(|e| panic!("writing {}: {e}", config.display()));
        let set: [(&str, OsString); 5] = [
            ("LEADHAND_TESTING", "1".into()),
            ("LEADHAND_CONFIG", config.into()),
            ("CLAUDE_CONFIG_DIR", tmp.join("claude home").into()),
            ("CODEX_HOME", tmp.join("codex home").into()),
            ("LEADHAND_INSTALLED_VENDORS", installed.join(",").into()),
        ];
        for (k, v) in set {
            vars.insert(k.into(), v);
        }
        let fakes = tmp.join("leadhand fakes");
        fs::create_dir_all(&fakes).unwrap_or_else(|e| panic!("making {}: {e}", fakes.display()));
        for (name, target) in [("codex", FAKE_CODEX), ("claude", FAKE_CLAUDE)] {
            let link = fakes.join(name);
            if fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(target, &link)
                    .unwrap_or_else(|e| panic!("linking {}: {e}", link.display()));
            }
        }
        Env { vars, fakes }
    }

    pub fn get(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.vars.get(key.as_ref()).map(OsString::as_os_str)
    }

    /// The value of `key` as text; panics when it is unset or not UTF-8.
    #[track_caller]
    pub fn get_str(&self, key: &str) -> &str {
        self.get(key)
            .and_then(OsStr::to_str)
            .unwrap_or_else(|| panic!("{key} is unset or not UTF-8 in the test environment"))
    }

    pub fn set(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Env {
        self.vars.insert(key.as_ref().into(), value.as_ref().into());
        self
    }

    pub fn remove(&mut self, key: impl AsRef<OsStr>) -> &mut Env {
        self.vars.remove(key.as_ref());
        self
    }

    /// A copy with `key` set to `value` (Python's `dict(env, KEY=value)`).
    pub fn with(&self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Env {
        let mut env = self.clone();
        env.set(key, value);
        env
    }

    /// A copy without `key`.
    pub fn without(&self, key: impl AsRef<OsStr>) -> Env {
        let mut env = self.clone();
        env.remove(key);
        env
    }

    pub fn vars(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }

    /// An environment of exactly `vars`, with `fakes` as its fakes folder: one an `Env` gave
    /// before (`vars`, `fakes`), rebuilt elsewhere, such as in a drive child.
    pub fn from_parts(vars: impl IntoIterator<Item = (OsString, OsString)>, fakes: PathBuf) -> Env {
        Env {
            vars: vars.into_iter().collect(),
            fakes,
        }
    }

    /// The folder holding `codex` and `claude`, the fake binaries (the Rust `tests/fakes`).
    pub fn fakes(&self) -> &Path {
        &self.fakes
    }

    /// The fakes folder, then this environment's PATH: a PATH on which `codex` and `claude`
    /// are the fakes.
    pub fn path_with_fakes(&self) -> OsString {
        let mut path = self.fakes.clone().into_os_string();
        if let Some(rest) = self.get("PATH").filter(|p| !p.is_empty()) {
            path.push(":");
            path.push(rest);
        }
        path
    }
}

// --------------------------------------------------------------------------
// The binary's own path (ruling S1-R11)
//
// Where the binary prints or passes a path for itself (an unpark hint, the launch line, a
// dispatch line's argv[0], LEADHAND_CLI), parity is the file it names, not its spelling.

/// `path` with every symlink resolved; a path that does not exist is made absolute instead
/// (`os.path.realpath`).
pub fn realpath(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| std::path::absolute(path).unwrap_or_default())
}

/// `path` with every symlink in its existing part resolved (Python's non-strict `resolve`).
pub fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// True when `path_text` names the same file as `file` once both are resolved.
pub fn names_file(file: &Path, path_text: impl AsRef<OsStr>) -> bool {
    !path_text.as_ref().is_empty() && realpath(Path::new(path_text.as_ref())) == realpath(file)
}

/// True when `path_text` names the binary under test (`names_file` of `bin()`).
pub fn names_bin(path_text: impl AsRef<OsStr>) -> bool {
    names_file(&bin(), path_text)
}

/// The middle of `text` when it is exactly `before`, a middle, then `after`.
fn middle<'t>(text: &'t str, before: &str, after: &str) -> Option<&'t str> {
    let rest = text.strip_prefix(before)?;
    rest.strip_suffix(after)
}

/// True when `text` is exactly `before`, then one shell word naming `file` (`names_file`),
/// then exactly `after`.
pub fn is_file_command(file: &Path, text: &str, before: &str, after: &str) -> bool {
    let Some(word) = middle(text, before, after) else {
        return false;
    };
    if word.is_empty() || word != word.trim() {
        return false;
    }
    match shlex_split(word) {
        Some(words) if words.len() == 1 => names_file(file, &words[0]),
        _ => false,
    }
}

/// `is_file_command` for the binary under test.
pub fn is_bin_command(text: &str, before: &str, after: &str) -> bool {
    is_file_command(&bin(), text, before, after)
}

/// True when `text` is exactly `before`, then a path naming `file` (`names_file`), then
/// exactly `after`, the path taken verbatim (not a shell word): for text that is not a
/// command line, such as the launch's first message or its sandbox exclusion.
pub fn names_file_between(file: &Path, text: &str, before: &str, after: &str) -> bool {
    middle(text, before, after).is_some_and(|path| !path.is_empty() && names_file(file, path))
}

/// `names_file_between` for the binary under test.
pub fn names_bin_between(text: &str, before: &str, after: &str) -> bool {
    names_file_between(&bin(), text, before, after)
}

// --------------------------------------------------------------------------
// Shell words, as Python's shlex

/// Python's `shlex.quote`.
pub fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Python's `shlex.join`, over OS strings (invalid UTF-8 replaced).
pub fn shlex_join(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| shlex_quote(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Python's `shlex.split` (POSIX mode, no comments); None where it raises ValueError.
pub fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if let Some(w) = word.take() {
                    words.push(w);
                }
            }
            '\\' => word.get_or_insert_with(String::new).push(chars.next()?),
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => w.push(ch),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            ch @ ('"' | '\\') => w.push(ch),
                            ch => {
                                w.push('\\');
                                w.push(ch);
                            }
                        },
                        ch => w.push(ch),
                    }
                }
            }
            ch => word.get_or_insert_with(String::new).push(ch),
        }
    }
    words.extend(word);
    Some(words)
}

// --------------------------------------------------------------------------
// Reading what the binary wrote

/// Every complete line of a JSON-lines file; a trailing partial line (a write in progress)
/// is skipped.
#[track_caller]
pub fn read_jsonl(path: &Path) -> Vec<Json> {
    let data = fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    lines.pop();
    lines
        .into_iter()
        .filter(|l| !l.trim_ascii().is_empty())
        .map(json::parse)
        .collect()
}

#[track_caller]
pub fn read_json(path: &Path) -> Json {
    json::parse(&fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display())))
}

/// Checks that `cond` stays true for a window of `seconds` (something must not happen);
/// panics naming `what` as soon as it is false (`contract_dispatch.holds`).
#[track_caller]
pub fn holds(mut cond: impl FnMut() -> bool, seconds: f64, what: &str) {
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        if !cond() {
            panic!("{what}: no longer true within the {seconds} s window");
        }
        if Instant::now() > end {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The members `keys` of `v`, as a list (Python's tuple of `v[k]`s).
#[track_caller]
pub fn pick(v: &Json, keys: &[&str]) -> Json {
    Json::Arr(keys.iter().map(|k| v[*k].clone()).collect())
}

/// Each item's member `key`, as a list (`[e[key] for e in items]`).
#[track_caller]
pub fn field(items: &[Json], key: &str) -> Json {
    Json::Arr(items.iter().map(|e| e[key].clone()).collect())
}

/// Python's `needle in hay` for a JSON list (an item equal to it), object (a key) or string
/// (a substring).
#[track_caller]
pub fn json_in(needle: &str, hay: &Json) -> bool {
    match hay {
        Json::Arr(items) => items.iter().any(|x| x == needle),
        Json::Obj(_) => hay.get(needle).is_some(),
        Json::Str(_) => supervisor::text(hay).contains(needle),
        other => panic!("`in` on a value that is no container: {other}"),
    }
}

/// `shutil.which(name)` on the test process's PATH: the first file of that name with an
/// execute bit set.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let path = process_env()
        .into_iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v)?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| executable(p))
}

/// Every file and folder under `root`, recursively, without following symlinks to folders
/// (`Path.rglob("*")`).
#[track_caller]
pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = fs::read_dir(root).unwrap_or_else(|e| panic!("reading {}: {e}", root.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("reading {}: {e}", root.display()));
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        found.push(path.clone());
        if is_dir {
            found.extend(walk(&path));
        }
    }
    found
}

/// Python's `list.index`.
#[track_caller]
pub fn index(items: &[String], item: &str) -> usize {
    items
        .iter()
        .position(|a| a == item)
        .unwrap_or_else(|| panic!("{item:?} is not in {items:?}"))
}

/// `text`'s lines, as owned strings (`str.splitlines()` of text with `\n` line ends).
pub fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// A UTF-8 file's text (`Path.read_text`).
#[track_caller]
pub fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// A JSON list of strings, as strings.
#[track_caller]
pub fn strs(v: &Json) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|s| {
            s.as_str()
                .unwrap_or_else(|| panic!("not a string: {s}"))
                .to_string()
        })
        .collect()
}

/// `items` as owned strings (a Python list literal of strings).
pub fn str_vec(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

/// `items` as OS strings, for `shlex_join`.
pub fn os_vec(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// A path the tests built from UTF-8 parts, as `&str` (Python's `str(path)`).
#[track_caller]
pub fn path_str(p: &Path) -> &str {
    p.to_str()
        .unwrap_or_else(|| panic!("not UTF-8: {}", p.display()))
}

/// The one line a successful command printed, without its newline; panics unless it
/// printed exactly one line.
#[track_caller]
pub fn one_line(r: Out) -> String {
    let r = r.ok();
    let out = r.stdout_text();
    assert!(out.ends_with('\n') && out.matches('\n').count() == 1, "{r}");
    out[..out.len() - 1].to_string()
}

/// `path` opened read-write (created, mode 0644, never truncated), for flock.
#[track_caller]
pub fn open_lock_file(path: &Path) -> fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)
        .unwrap_or_else(|e| panic!("opening {}: {e}", path.display()))
}

/// Takes an exclusive flock on `path`, as another leadhand process would; dropping the file
/// (closing the descriptor) releases it.
#[track_caller]
pub fn hold_lock(path: &Path) -> fs::File {
    let f = open_lock_file(path);
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)
        .unwrap_or_else(|e| panic!("locking {}: {e}", path.display()));
    f
}

/// Whether an exclusive flock on `path` can be taken now (it is taken and dropped again).
#[track_caller]
pub fn lockable(path: &Path) -> bool {
    let f = open_lock_file(path);
    match rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => true,
        Err(rustix::io::Errno::WOULDBLOCK) => false,
        Err(e) => panic!("locking {}: {e}", path.display()),
    }
}

/// The lock that serializes a test file's wall-clock-sensitive group (ruling S3-R19): a
/// test whose assertions rest on the binary's wall-clock budgets (the stop ladder's 5 s
/// steps, the stale-heartbeat second look, the foreground wait) holds it for its whole run,
/// so no two such tests in one file load the machine together. Each test file is its own
/// binary, so the lock is per file. A test that failed holding it does not poison it.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The top-level keys of status.json, and of `status --json`'s output.
pub const STATUS_KEYS: [&str; 10] = [
    "run_id",
    "deck",
    "state",
    "headline",
    "activity",
    "progress",
    "attention",
    "cards",
    "usage",
    "updated_at",
];

/// A JSON object's distinct keys, as Python's `set(d)`.
pub fn key_set(v: &Json) -> BTreeSet<String> {
    v.keys().into_iter().collect()
}

/// `items` as a set of strings, as a Python set literal.
pub fn set_of(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Sets `path`'s access and modification times to `when` (`os.utime(path, (t, t))`;
/// `SystemTime::now()` for `os.utime(path)`).
#[track_caller]
pub fn set_mtime(path: &Path, when: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_times(fs::FileTimes::new().set_accessed(when).set_modified(when)))
        .unwrap_or_else(|e| panic!("setting the times of {}: {e}", path.display()));
}

/// Writes `text` to the file `path`, whose folder must exist (`Path.write_text`).
#[track_caller]
pub fn write_text(path: &Path, text: impl AsRef<[u8]>) {
    fs::write(path, text).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// Makes `link` a symlink to `target` (`link.symlink_to(target)`); panics when `link`
/// exists.
#[track_caller]
pub fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link)
        .unwrap_or_else(|e| panic!("linking {} to {}: {e}", link.display(), target.display()));
}

/// Makes the folder `path` and its missing parents; panics when `path` exists
/// (`Path.mkdir(parents=True)` without `exist_ok`).
#[track_caller]
pub fn mkdir_new(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("making {}: {e}", parent.display()));
    }
    fs::create_dir(path).unwrap_or_else(|e| panic!("making {}: {e}", path.display()));
}

/// `path` relative to `start`, both absolute and normalised, without resolving symlinks
/// (`os.path.relpath`). Unlike `os.path.relpath`, it does not collapse `..` components, so
/// neither path may hold one.
pub fn relpath(path: &Path, start: &Path) -> PathBuf {
    let (p, s): (Vec<_>, Vec<_>) = (path.components().collect(), start.components().collect());
    let common = p.iter().zip(&s).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..s.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// The local date, `YYYY-MM-DD` (Python's `date.today().isoformat()`).
#[track_caller]
pub fn today() -> String {
    let out = Command::new("date")
        .arg("+%F")
        .stdin(Stdio::null())
        .output()
        .expect("running date");
    assert!(out.status.success(), "date +%F failed: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// The date `days` after the `YYYY-MM-DD` date `date` (`date + timedelta(days=days)`), in
/// the proleptic Gregorian calendar.
#[track_caller]
pub fn add_days(date: &str, days: i64) -> String {
    let parts: Vec<i64> = date
        .split('-')
        .map(|p| p.parse().unwrap_or_else(|_| panic!("not a date: {date:?}")))
        .collect();
    let [y, m, d] = parts[..] else {
        panic!("not a date: {date:?}")
    };
    // Days from civil and back (H. Hinnant's algorithms), counted from 0000-03-01.
    let (y0, mp) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y0.div_euclid(400);
    let yoe = y0 - era * 400;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let z = era * 146097 + doe + days;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Python's `str()` of a JSON value read back, as an f-string prints it: `None`, an int's
/// digits, a string's text (anything else as JSON).
#[track_caller]
pub fn py_str(v: &Json) -> String {
    match v {
        Json::Null => "None".to_string(),
        Json::Num(n) => n.clone(),
        Json::Str(_) => v
            .as_str()
            .unwrap_or_else(|| panic!("not a UTF-8 string: {v}"))
            .to_string(),
        other => other.to_string(),
    }
}

/// A JSON literal, parsed by the harness decoder.
#[track_caller]
pub fn jv(text: &str) -> Json {
    json::parse(text.as_bytes())
}

/// The one item of `items` (Python's `[x] = items`); panics unless there is exactly one.
#[track_caller]
pub fn one(items: Vec<Json>) -> Json {
    let [x] = n_of(items);
    x
}

/// Exactly `N` items (Python's `a, b = items`); panics unless there are exactly `N`.
#[track_caller]
pub fn n_of<const N: usize>(items: Vec<Json>) -> [Json; N] {
    <[Json; N]>::try_from(items)
        .unwrap_or_else(|items| panic!("expected {N} items, got {}: {items:?}", items.len()))
}

/// `bytes` as an OS string (for paths in files the binary wrote).
pub fn os(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

// --------------------------------------------------------------------------
// Process identity, through `leadhand::procs` (liveness only)

/// The process's start time; None if it is gone or a zombie.
pub fn start_time(pid: i32) -> Option<String> {
    if pid <= 0 {
        None
    } else {
        leadhand::procs::identify(pid).map(|id| id.start)
    }
}

/// True when `pid` is alive (not a zombie) and still has start time `start`.
pub fn same_process(pid: i32, start: Option<&str>) -> bool {
    leadhand::procs::same_process(Some(pid), start)
}

/// A start time as seconds since the epoch (on Linux to the second of the boot time).
fn start_wall(start: &str) -> Option<f64> {
    match start.strip_prefix("ticks:") {
        Some(ticks) => {
            let stat = fs::read_to_string("/proc/stat").ok()?;
            let btime: f64 = stat
                .lines()
                .find_map(|line| line.strip_prefix("btime "))?
                .trim()
                .parse()
                .ok()?;
            Some(btime + ticks.parse::<f64>().ok()? / clock_ticks()?)
        }
        None => start.parse().ok(),
    }
}

/// The kernel's clock ticks per second (`os.sysconf("SC_CLK_TCK")`), from `getconf`.
fn clock_ticks() -> Option<f64> {
    static TICKS: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *TICKS.get_or_init(|| {
        let out = Command::new("getconf")
            .arg("CLK_TCK")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    })
}

/// The start time of `pid` when it is alive and started no later than `when` (seconds since
/// the epoch), so it is the process a record written at `when` named, not a later one reusing
/// the pid; else None (`harness.started_by`).
pub fn started_by(pid: i32, when: f64) -> Option<String> {
    let start = start_time(pid)?;
    (start_wall(&start)? <= when).then_some(start)
}
