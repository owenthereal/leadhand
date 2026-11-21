//! A fake `codex exec --json` for leadhand's tests: the Rust form of `tests/fakes/codex`,
//! writing the same log lines byte for byte for the same inputs. Never touches the network.
//!
//! Logs one JSON line per run to $FAKE_CODEX_LOG: its argv, cwd, environment, the
//! `-c shell_environment_policy.set.<NAME>=<value>` overrides (decoded), the prompt read
//! from stdin, its pid and process group. Later lines in the same log record the tool child
//! it starts ("child") and how it ended ("exit"). Every line starts with "time" (the wall
//! clock, as Python's `time.time()`) and "pid".
//!
//! Then it replays $FAKE_CODEX_SCENARIO, a JSON-lines file. A line with "type" is an event
//! printed to stdout; "{thread}" inside it is replaced by the thread ID (the resumed one,
//! else $FAKE_CODEX_THREAD, else a fixed ID). A line with "action" does something:
//!
//!   {"action": "sleep", "seconds": N}
//!   {"action": "stderr", "text": "..."}
//!   {"action": "last_message", "text": "..."}       writes the -o file
//!   {"action": "write_file", "path": "rel", "text": "..."}
//!   {"action": "commit", "message": "..."}          git add -A && git commit (if anything changed)
//!   {"action": "append_loop", "path": "rel", "interval": 0.2}   background writer
//!   {"action": "tool_child", "seconds": N}          a `sleep` child in its own group
//!   {"action": "escaped_writer", "path": "rel", "interval": 0.05}
//!         a double-forked writer in a new session (not a descendant of Codex once
//!         started), appending to `path` for up to 60 s; its pid is logged ("escaped")
//!   {"action": "rollout", "fixture": "name.jsonl"}  copies a fixture into
//!         $CODEX_HOME/sessions/2026/01/01/rollout-2026-01-01T00-00-00-<thread>.jsonl
//!         (skipped when $FAKE_CODEX_NO_ROLLOUT=1)
//!   {"action": "exit", "code": N}
//!
//! Environment knobs: $FAKE_CODEX_DELAY (seconds to sleep before the scenario),
//! $FAKE_CODEX_IGNORE_SIGINT=1 (SIGINT does nothing), $FAKE_CODEX_ORPHAN_CHILD=1 (on SIGINT,
//! exit without stopping the tool child; otherwise it is stopped first, as the real Codex
//! does).
//!
//! The escaped writer's double fork is two re-executions of this binary (no `fork`, no
//! `pre_exec`): the first starts the second and exits at once; the second calls `setsid`
//! and writes.

#[allow(dead_code)]
#[path = "../common/json.rs"]
mod json;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use json::Json;
use rustix::process::{Pid, Signal, getpgrp, kill_process_group, setsid};
use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const DEFAULT_THREAD: &str = "0f0f0f0f-0000-4000-8000-00000000c0de";
/// The first argument of the escaped writer's two re-executions.
const ESCAPED_WRITER: &str = "--leadhand-fake-codex-escaped-writer";
const ESCAPED_SESSION: &str = "--leadhand-fake-codex-escaped-session";

/// Held while a log line is written, and by the SIGINT handler until it exits, so no line
/// is cut short and none follows the exit line.
static LOG: Mutex<()> = Mutex::new(());
/// The tool children, which a SIGINT stops.
static CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends the process as Python ends on an uncaught exception: the message on stderr, exit 1.
fn die(message: impl std::fmt::Display) -> ! {
    eprintln!("fake codex: {message}");
    process::exit(1);
}

fn env_is(name: &str, value: &str) -> bool {
    env::var_os(name).is_some_and(|v| v == value)
}

fn env_set(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|v| !v.is_empty())
}

fn os_json(s: &OsStr) -> Json {
    Json::Str(json::from_os(s.as_bytes()))
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

// --------------------------------------------------------------------------
// The log

fn line(fields: Vec<(&str, Json)>) -> String {
    let mut pairs = vec![
        ("time", Json::Num(json::json_float(now()))),
        ("pid", Json::from(process::id() as i64)),
    ];
    pairs.extend(fields);
    json::dumps(&Json::object(pairs)) + "\n"
}

fn append(path: &Path, text: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text)
}

fn write_log(fields: Vec<(&str, Json)>) {
    if let Some(path) = env_set("FAKE_CODEX_LOG") {
        let text = line(fields);
        if let Err(e) = append(Path::new(&path), text.as_bytes()) {
            die(format_args!("writing {}: {e}", path.to_string_lossy()));
        }
    }
}

fn log(fields: Vec<(&str, Json)>) {
    let _held = lock(&LOG);
    write_log(fields);
}

// --------------------------------------------------------------------------
// Python's conversions of a decoded JSON value

/// The text of a JSON string, as Python prints it to a stream (a lone surrogate as `\udcff`,
/// the backslashreplace stderr uses).
fn str_text(s: &[u8]) -> String {
    let cps = json::code_points(s).unwrap_or_default();
    cps.into_iter()
        .map(|cp| char::from_u32(cp).map_or_else(|| format!("\\u{cp:04x}"), String::from))
        .collect()
}

/// Python's `str()` of a decoded JSON value.
fn py_str(v: &Json) -> String {
    match v {
        Json::Null => "None".to_string(),
        Json::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Json::Num(t)
            if t.trim_start_matches('-')
                .bytes()
                .all(|b| b.is_ascii_digit()) =>
        {
            json::dumps(v)
        }
        Json::Num(t) => t.parse().map_or_else(|_| t.clone(), json::float_repr),
        Json::Str(s) => str_text(s),
        Json::Arr(_) | Json::Obj(_) => py_repr(v),
    }
}

/// Python's `repr()` of a decoded JSON value.
fn py_repr(v: &Json) -> String {
    match v {
        Json::Str(s) => {
            let text = str_text(s);
            let quote = if text.contains('\'') && !text.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::from(quote);
            for c in text.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                        out.push_str(&format!("\\x{:02x}", c as u32))
                    }
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Json::Arr(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Json::Obj(members) => format!(
            "{{{}}}",
            json::dict(members)
                .into_iter()
                .map(|(k, v)| format!("{}: {}", py_repr(&Json::Str(k.to_vec())), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => py_str(other),
    }
}

/// Python's `float()` of a string.
fn float_of_text(text: &str) -> Option<f64> {
    text.trim().replace('_', "").parse().ok()
}

/// Python's `float()` of a decoded JSON value.
fn py_float(v: &Json, what: &str) -> f64 {
    let f = match v {
        Json::Num(t) => t.parse().ok(),
        Json::Str(s) => std::str::from_utf8(s).ok().and_then(float_of_text),
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    };
    f.unwrap_or_else(|| {
        die(format_args!(
            "{what}: could not convert {} to float",
            py_repr(v)
        ))
    })
}

/// Python's `int()` of a decoded JSON value.
fn py_int(v: &Json, what: &str) -> i64 {
    let n = match v {
        Json::Num(t) => t
            .parse::<i64>()
            .ok()
            .or_else(|| t.parse::<f64>().ok().map(|f| f.trunc() as i64)),
        Json::Str(s) => std::str::from_utf8(s)
            .ok()
            .and_then(|t| t.trim().replace('_', "").parse().ok()),
        Json::Bool(b) => Some(i64::from(*b)),
        _ => None,
    };
    n.unwrap_or_else(|| {
        die(format_args!(
            "{what}: invalid literal for int(): {}",
            py_repr(v)
        ))
    })
}

/// A decoded JSON string as OS bytes (surrogateescape), for a path or a file's text.
fn os_of(v: &Json, what: &str) -> OsString {
    v.as_bytes().and_then(json::to_os).unwrap_or_else(|| {
        die(format_args!(
            "{what}: not a string of a path: {}",
            py_repr(v)
        ))
    })
}

/// A decoded JSON string as UTF-8 text, for a file written as UTF-8.
fn utf8_of<'v>(v: &'v Json, what: &str) -> &'v str {
    v.as_str()
        .unwrap_or_else(|| die(format_args!("{what}: not UTF-8 text: {}", py_repr(v))))
}

/// `step[key]`, as Python's dict lookup: dies with KeyError when there is none.
fn field<'s>(step: &'s Json, key: &str) -> &'s Json {
    step.get(key)
        .unwrap_or_else(|| die(format_args!("KeyError: {key:?}")))
}

fn sleep_secs(secs: f64) {
    if !secs.is_finite() || secs < 0.0 {
        die(format_args!(
            "sleep length must be non-negative and finite, not {secs}"
        ));
    }
    thread::sleep(Duration::from_secs_f64(secs));
}

// --------------------------------------------------------------------------
// Arguments, prompt, scenario

#[derive(Default)]
struct Opts {
    policy_set: Vec<(Vec<u8>, Json)>,
    chdir: Option<OsString>,
    output: Option<OsString>,
    resume: Option<OsString>,
}

fn parse(argv: &[OsString]) -> Opts {
    const PREFIX: &str = "shell_environment_policy.set.";
    let mut out = Opts::default();
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_bytes();
        let has_value = i + 1 < argv.len();
        if has_value && [&b"-m"[..], b"-C", b"-s", b"-o", b"--add-dir", b"-c"].contains(&a) {
            let v = &argv[i + 1];
            match a {
                b"-c" => {
                    let text = v.to_string_lossy();
                    let (key, raw) = text.split_once('=').unwrap_or((&text, ""));
                    if let Some(name) = key.strip_prefix(PREFIX) {
                        let value = toml_value(raw);
                        let name = json::from_os(name.as_bytes());
                        match out.policy_set.iter_mut().find(|(k, _)| *k == name) {
                            Some(slot) => slot.1 = value,
                            None => out.policy_set.push((name, value)),
                        }
                    }
                }
                b"-C" => out.chdir = Some(v.clone()),
                b"-o" => out.output = Some(v.clone()),
                _ => {}
            }
            i += 2;
            continue;
        }
        if a == b"resume" && has_value {
            out.resume = Some(argv[i + 1].clone());
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// `tomllib.loads(f"v = {raw}")["v"]`, as JSON.
fn toml_value(raw: &str) -> Json {
    let table: toml::Table = toml::from_str(&format!("v = {raw}"))
        .unwrap_or_else(|e| die(format_args!("TOMLDecodeError: {e}")));
    match table.get("v") {
        Some(v) => toml_json(v),
        None => die("KeyError: 'v'"),
    }
}

fn toml_json(v: &toml::Value) -> Json {
    match v {
        toml::Value::String(s) => Json::from(s.as_str()),
        toml::Value::Integer(n) => Json::from(*n),
        toml::Value::Float(f) => Json::Num(json::json_float(*f)),
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Datetime(_) => {
            die("TypeError: Object of type datetime is not JSON serializable")
        }
        toml::Value::Array(items) => Json::Arr(items.iter().map(toml_json).collect()),
        toml::Value::Table(t) => Json::object(t.iter().map(|(k, v)| (k.as_str(), toml_json(v)))),
    }
}

/// stdin, as Python's `sys.stdin.read()` gives it: no newline translation (Python's standard
/// streams keep `\r`), bytes that are not UTF-8 surrogate-escaped.
fn read_prompt() -> Vec<u8> {
    let mut raw = Vec::new();
    if let Err(e) = io::stdin().read_to_end(&mut raw) {
        die(format_args!("reading stdin: {e}"));
    }
    json::from_os(&raw)
}

/// The environment, as Python's `os.environ` holds it (a repeated name keeps its first value).
fn env_json() -> Json {
    let mut members: Vec<(Vec<u8>, Json)> = Vec::new();
    for (k, v) in env::vars_os() {
        let key = json::from_os(k.as_bytes());
        if !members.iter().any(|(seen, _)| *seen == key) {
            members.push((key, os_json(&v)));
        }
    }
    Json::Obj(members)
}

/// Python's `str.splitlines()`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if boundary {
            lines.push(&text[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|&(_, n)| n == '\n') {
                chars.next();
                start += 1;
            }
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Python's `str.isspace()` for every character (an empty line counts).
fn is_blank(line: &str) -> bool {
    line.chars()
        .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Python's `Path(p).resolve()`: absolute, every symlink in the existing part resolved.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => absolute,
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

// --------------------------------------------------------------------------
// SIGINT

fn stop_children() {
    for child in lock(&CHILDREN).iter_mut() {
        if let Ok(None) = child.try_wait() {
            let _ = kill_process_group(Pid::from_child(child), Signal::TERM);
        }
    }
}

fn on_sigint() {
    if env_is("FAKE_CODEX_IGNORE_SIGINT", "1") {
        log(vec![("event", "sigint_ignored".into())]);
        return;
    }
    if !env_is("FAKE_CODEX_ORPHAN_CHILD", "1") {
        stop_children();
    }
    let _held = lock(&LOG);
    write_log(vec![("event", "exit".into()), ("how", "sigint".into())]);
    process::exit(1);
}

fn handle_sigint() {
    let mut signals =
        Signals::new([SIGINT]).unwrap_or_else(|e| die(format_args!("SIGINT handler: {e}")));
    thread::spawn(move || {
        for _ in signals.forever() {
            on_sigint();
        }
    });
}

// --------------------------------------------------------------------------
// The scenario's actions

fn print_event(step: &Json) {
    let text = json::dumps(step) + "\n";
    let mut out = io::stdout().lock();
    if out
        .write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        process::exit(1); // Python's BrokenPipeError
    }
}

fn commit(message: &Json) {
    let git = |args: &[&OsStr]| {
        let mut cmd = Command::new("git");
        cmd.args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Fake Codex",
            "-c",
            "user.email=fake@example.invalid",
        ]);
        cmd.args(args);
        cmd.status()
            .unwrap_or_else(|e| die(format_args!("git: {e}")))
    };
    let checked = |args: &[&OsStr]| {
        let status = git(args);
        if !status.success() {
            die(format_args!(
                "CalledProcessError: git {} returned {status}",
                args[0].to_string_lossy()
            ));
        }
    };
    checked(&[OsStr::new("add"), OsStr::new("-A")]);
    if !git(&[
        OsStr::new("diff"),
        OsStr::new("--cached"),
        OsStr::new("--quiet"),
    ])
    .success()
    {
        let message = os_of(message, "commit message");
        checked(&[
            OsStr::new("commit"),
            OsStr::new("--quiet"),
            OsStr::new("-m"),
            &message,
        ]);
    }
}

fn append_loop(path: PathBuf, interval: f64) {
    thread::spawn(move || {
        let mut n: u64 = 0;
        loop {
            if let Err(e) = append(&path, format!("line {n}\n").as_bytes()) {
                eprintln!("fake codex: append_loop: {e}");
                return;
            }
            n += 1;
            if !interval.is_finite() || interval < 0.0 {
                eprintln!("fake codex: append_loop: bad interval {interval}");
                return;
            }
            thread::sleep(Duration::from_secs_f64(interval));
        }
    });
}

fn tool_child(seconds: &Json) {
    let mut children = lock(&CHILDREN);
    let child = Command::new("sleep")
        .arg(py_str(seconds))
        .process_group(0)
        .stdin(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(format_args!("starting sleep: {e}")));
    let pid = child.id();
    children.push(child);
    drop(children);
    log(vec![
        ("event", "child".into()),
        ("child_pid", Json::from(i64::from(pid))),
    ]);
}

fn escaped_writer(path: &Json, interval: &Json) {
    let target = resolve(Path::new(&os_of(path, "escaped_writer path")));
    let pidfile = with_suffix(&target, ".pid");
    let exe = env::current_exe().unwrap_or_else(|e| die(format_args!("finding this binary: {e}")));
    let status = Command::new(exe)
        .arg(ESCAPED_WRITER)
        .arg(&target)
        .arg(py_str(interval))
        .stdin(Stdio::null())
        .status()
        .unwrap_or_else(|e| die(format_args!("starting the escaped writer: {e}")));
    if !status.success() {
        die(format_args!(
            "the escaped writer's first stage exited {status}"
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pidfile.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let text = fs::read_to_string(&pidfile)
        .unwrap_or_else(|e| die(format_args!("reading {}: {e}", pidfile.display())));
    let pid: i64 = text
        .trim()
        .parse()
        .unwrap_or_else(|e| die(format_args!("{}: {e}", pidfile.display())));
    log(vec![
        ("event", "escaped".into()),
        ("escaped_pid", Json::from(pid)),
    ]);
}

fn rollout(fixture: &Json, thread_id: &OsStr) {
    let Some(home) = env_set("CODEX_HOME") else {
        return;
    };
    if env_is("FAKE_CODEX_NO_ROLLOUT", "1") {
        return;
    }
    let day = Path::new(&home).join("sessions/2026/01/01");
    fs::create_dir_all(&day).unwrap_or_else(|e| die(format_args!("making {}: {e}", day.display())));
    let source = Path::new(FIXTURES).join(os_of(fixture, "rollout fixture"));
    let mut name = OsString::from("rollout-2026-01-01T00-00-00-");
    name.push(thread_id);
    name.push(".jsonl");
    // shutil.copyfile: the bytes, not the mode.
    let data = fs::read(&source)
        .unwrap_or_else(|e| die(format_args!("reading {}: {e}", source.display())));
    let dest = day.join(name);
    fs::write(&dest, data).unwrap_or_else(|e| die(format_args!("writing {}: {e}", dest.display())));
}

fn write_text(path: &Path, text: &str) {
    fs::write(path, text).unwrap_or_else(|e| die(format_args!("writing {}: {e}", path.display())));
}

// --------------------------------------------------------------------------

fn run(argv: Vec<OsString>) -> i32 {
    handle_sigint();
    let opts = parse(&argv);
    let prompt = read_prompt();
    let cwd = env::current_dir().unwrap_or_else(|e| die(format_args!("getcwd: {e}")));
    log(vec![
        ("event", "start".into()),
        ("argv", Json::Arr(argv.iter().map(|a| os_json(a)).collect())),
        ("cwd", os_json(cwd.as_os_str())),
        ("env", env_json()),
        ("policy_set", Json::Obj(opts.policy_set.clone())),
        ("prompt", Json::Str(prompt)),
        ("pgid", Json::from(getpgrp().as_raw_pid())),
    ]);
    if let Some(dir) = &opts.chdir {
        env::set_current_dir(dir)
            .unwrap_or_else(|e| die(format_args!("chdir {}: {e}", dir.to_string_lossy())));
    }
    let thread_id: OsString = opts
        .resume
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| env_set("FAKE_CODEX_THREAD"))
        .unwrap_or_else(|| DEFAULT_THREAD.into());
    let thread_text = thread_id.to_string_lossy().into_owned();
    if let Some(delay) = env_set("FAKE_CODEX_DELAY") {
        let text = delay.to_string_lossy();
        sleep_secs(
            float_of_text(&text).unwrap_or_else(|| die(format_args!("FAKE_CODEX_DELAY={text}"))),
        );
    }
    let scenario = match env_set("FAKE_CODEX_SCENARIO") {
        Some(path) => fs::read_to_string(&path)
            .unwrap_or_else(|e| die(format_args!("reading {}: {e}", path.to_string_lossy()))),
        None => String::new(),
    };
    for line in split_lines(&scenario) {
        if is_blank(line) {
            continue;
        }
        let text = line.replace("{thread}", &thread_text);
        let step = json::try_parse(text.as_bytes())
            .unwrap_or_else(|e| die(format_args!("a scenario line is not JSON ({e}): {text}")));
        if !matches!(step, Json::Obj(_)) {
            die(format_args!(
                "AttributeError: a scenario line is not an object: {text}"
            ));
        }
        let action = step.get("action").filter(|a| !a.is_null());
        let Some(action) = action else {
            print_event(&step);
            continue;
        };
        match action.as_str().unwrap_or("") {
            "sleep" => sleep_secs(py_float(field(&step, "seconds"), "seconds")),
            "stderr" => {
                let text = py_str(field(&step, "text")) + "\n";
                let mut err = io::stderr().lock();
                let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
            }
            "last_message" => {
                if let Some(out) = &opts.output {
                    write_text(Path::new(out), utf8_of(field(&step, "text"), "text"));
                }
            }
            "write_file" => {
                let path = PathBuf::from(os_of(field(&step, "path"), "path"));
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs::create_dir_all(parent)
                        .unwrap_or_else(|e| die(format_args!("making {}: {e}", parent.display())));
                }
                write_text(&path, utf8_of(field(&step, "text"), "text"));
            }
            "commit" => commit(field(&step, "message")),
            "append_loop" => append_loop(
                PathBuf::from(os_of(field(&step, "path"), "path")),
                py_float(field(&step, "interval"), "interval"),
            ),
            "tool_child" => tool_child(field(&step, "seconds")),
            "escaped_writer" => escaped_writer(field(&step, "path"), field(&step, "interval")),
            "rollout" => rollout(field(&step, "fixture"), &thread_id),
            "exit" => {
                let code = field(&step, "code");
                log(vec![
                    ("event", "exit".into()),
                    ("how", "scenario".into()),
                    ("code", code.clone()),
                ]);
                return py_int(code, "code") as i32;
            }
            _ => die(format_args!("unknown action {}", py_repr(action))),
        }
    }
    log(vec![
        ("event", "exit".into()),
        ("how", "end".into()),
        ("code", Json::from(0)),
    ]);
    0
}

/// The escaped writer's first stage: start the second and exit at once, so the writer is
/// no descendant of Codex.
fn escaped_first_stage(args: &[OsString]) -> ! {
    let exe = env::current_exe().unwrap_or_else(|e| die(format_args!("finding this binary: {e}")));
    if let Err(e) = Command::new(exe).arg(ESCAPED_SESSION).args(args).spawn() {
        die(format_args!("starting the escaped writer: {e}"));
    }
    process::exit(0);
}

/// The escaped writer itself, in a new session: "started", its pid in `<path>.pid`, then
/// "x" every `interval` seconds for 60 s.
fn escaped_session(args: &[OsString]) -> ! {
    let _ = setsid();
    let (Some(path), Some(interval)) = (args.first(), args.get(1)) else {
        die("escaped writer: usage")
    };
    let path = PathBuf::from(path);
    let interval = float_of_text(&interval.to_string_lossy())
        .unwrap_or_else(|| die(format_args!("escaped writer: bad interval {interval:?}")));
    let fail = |e: io::Error| -> ! { die(format_args!("escaped writer: {e}")) };
    append(&path, b"started\n").unwrap_or_else(|e| fail(e));
    let tmp = with_suffix(&path, ".tmp");
    fs::write(&tmp, process::id().to_string()).unwrap_or_else(|e| fail(e));
    fs::rename(&tmp, with_suffix(&path, ".pid")).unwrap_or_else(|e| fail(e));
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        append(&path, b"x\n").unwrap_or_else(|e| fail(e));
        sleep_secs(interval);
    }
    process::exit(0);
}

fn main() {
    let argv: Vec<OsString> = env::args_os().skip(1).collect();
    match argv.first().map(|a| a.as_bytes()) {
        Some(a) if a == ESCAPED_WRITER.as_bytes() => escaped_first_stage(&argv[1..]),
        Some(a) if a == ESCAPED_SESSION.as_bytes() => escaped_session(&argv[1..]),
        _ => process::exit(run(argv)),
    }
}
