//! The contract suite's harness: it reaches leadhand only as a subprocess at LEADHAND_BIN.
//! The Rust form of `tests/contract/harness.py`.
//!
//! - LEADHAND_BIN: the binary under test (default the testing build, `CARGO_BIN_EXE_leadhand`).
//!   A relative value is resolved against the test process's working directory, the package
//!   root (ruling S3-R5), so `LEADHAND_BIN=scripts/leadhand` works.
//! - LEADHAND_IMPL: `rust` (the default) or `python`. Rust-only tests return early under
//!   `python` (`rust_only`).
//!
//! Tests run in parallel (ruling S3-R19): nothing here changes the test process's own
//! environment or relies on running alone. Each test makes its own `TempRoot`, and `Env`
//! hands each child its environment explicitly. Nothing here uses the crate's library except
//! `leadhand::procs`, to check whether a process is alive.

#![allow(dead_code, unused_macros)]

pub mod git;
pub mod json;

use std::collections::BTreeMap;
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
use std::time::{Duration, Instant};

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
/// panic's message; panics itself if `expr` returns. An assertion, so it counts as one in
/// `check_conversion.py`.
macro_rules! assert_panics {
    ($e:expr) => {
        $crate::common::panic_message(|| {
            let _ = $e;
        })
        .unwrap_or_else(|| panic!("expected a panic from `{}`", stringify!($e)))
    };
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
// The binary and the tags

/// The binary under test: LEADHAND_BIN, made absolute against the working directory, else
/// the testing build.
pub fn bin() -> PathBuf {
    match std::env::var_os("LEADHAND_BIN").filter(|v| !v.is_empty()) {
        Some(value) => std::path::absolute(&value)
            .unwrap_or_else(|e| panic!("LEADHAND_BIN={}: {e}", value.to_string_lossy())),
        None => PathBuf::from(env!("CARGO_BIN_EXE_leadhand")),
    }
}

fn impl_of(value: Option<&OsStr>) -> &'static str {
    match value.map(OsStr::as_bytes) {
        None | Some(b"" | b"rust") => "rust",
        Some(b"python") => "python",
        Some(other) => panic!(
            "LEADHAND_IMPL must be rust or python, not {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

/// True when LEADHAND_IMPL is `python`: the binary under test is the Python reference.
pub fn impl_is_python() -> bool {
    impl_of(std::env::var_os("LEADHAND_IMPL").as_deref()) == "python"
}

/// Whether a rust-only test skips under LEADHAND_IMPL=`value` (None: unset).
pub fn rust_only_under(value: Option<&OsStr>) -> bool {
    impl_of(value) == "python"
}

/// The rust-only tag, for new Rust behaviour: true when the test must skip (under
/// LEADHAND_IMPL=python), after printing `skipped: rust-only`. Use it as
/// `if common::rust_only() { return; }`.
pub fn rust_only() -> bool {
    let skip = rust_only_under(std::env::var_os("LEADHAND_IMPL").as_deref());
    if skip {
        println!("skipped: rust-only");
    }
    skip
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
    let own_group = env.get("LEADHAND_CRASH_KILL_PGID").is_none();
    match run_process(
        argv,
        env.vars(),
        cwd,
        stdin.map(str::as_bytes),
        timeout,
        own_group,
    ) {
        Ok(out) => out,
        Err(out) => {
            let killed = if own_group {
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
/// past `timeout`: it (or its group) was killed, and the output is whatever came before the
/// pipes closed, or a note when a process outside the group held them for another 5 s.
fn run_process<K, V>(
    argv: &[&OsStr],
    vars: impl IntoIterator<Item = (K, V)>,
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
    own_group: bool,
) -> Result<Out, Out>
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
    if own_group {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().unwrap_or_else(|e| {
        panic!(
            "could not start {} in {}: {e}",
            shlex_join(&argv_owned),
            cwd.display()
        )
    });
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
    let collect = |until: Instant, outputs: &mut [Option<Vec<u8>>; 2]| {
        while outputs.iter().any(Option::is_none) {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok((n, buf)) => outputs[usize::from(n - 1)] = Some(buf),
                Err(_) => return false,
            }
        }
        true
    };
    let finished = collect(deadline, &mut outputs) && wait_until(&mut child, deadline);
    if finished {
        let status = child.wait().expect("waiting for a finished child");
        let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
        return Ok(Out {
            argv: argv_owned,
            code: code_of(status),
            stdout,
            stderr,
        });
    }
    let pid = Pid::from_child(&child);
    if own_group {
        let _ = kill_process_group(pid, Signal::KILL);
    } else {
        let _ = child.kill();
    }
    let (stdout, stderr) = if collect(Instant::now() + Duration::from_secs(5), &mut outputs) {
        let [stdout, stderr] = outputs.map(Option::unwrap_or_default);
        (stdout, stderr)
    } else {
        // A descendant left the group and holds the pipes.
        let _ = child.kill();
        let note = "(not read: a process outside the binary's group held the pipes)";
        (Vec::new(), note.as_bytes().to_vec())
    };
    let status = child.wait().expect("waiting for a killed child");
    Err(Out {
        argv: argv_owned,
        code: code_of(status),
        stdout,
        stderr,
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

fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(0))
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
const SUITE_VARS: [&str; 5] = [
    "LEADHAND_BIN",
    "LEADHAND_IMPL",
    "LEADHAND_RUST_COMMANDS",
    "LEADHAND_RUST_BIN",
    "LEADHAND_PY_BIN",
];
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
        Env::from_base(std::env::vars_os(), tmp, installed, config_text)
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
        let set: [(&str, OsString); 6] = [
            ("LEADHAND_TESTING", "1".into()),
            ("PYTHONDONTWRITEBYTECODE", "1".into()),
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

/// `path` with every symlink resolved; a path that does not exist is made absolute instead.
fn realpath(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| std::path::absolute(path).unwrap_or_default())
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
