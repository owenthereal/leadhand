//! A scripted world for engine scenarios, driving the binary at LEADHAND_BIN only through its
//! command line: the Rust form of `tests/contract/supervisor.py`.
//!
//! `FakeSupervisor` does what the run skill's supervisor does, with fake workers instead of
//! real ones. Every engine step is a CLI call: `start`, `next`, and `record started|result|
//! ruling|park|diff-check` with the flags the run skill uses (a result through the
//! dispatch's `reply_file`).
//!
//! - implement-type kinds (implement, fix, answer, continue, final_fix) write files in the
//!   workspace, commit them on the checked-out branch (a card branch, or the final fix
//!   branch) and write a report whose first line is the scripted `Status:`;
//! - review kinds (review, rereview, final_review, security) write `Verdict: ...` with
//!   findings;
//! - verify kinds (verify, evidence) write `Result: ...`;
//! - integrate rebases the card branch onto the run tip, resolving conflicts in the card's
//!   favour, and reports like an implementer (a step's `rebase` turns that off for
//!   integrate, or on for another implementer kind such as answer).
//!
//! A step's `tamper(world, ws, action)` runs inside the worker before its report is written.
//! A Claude worker's step may give `transcript`: `Some(Some(text))` is the text its
//! transcript gets in place of the one usage record a worker appends by default (`None`:
//! the key is absent); `Some(None)` (Python's `None`): the worker writes no transcript.
//!
//! Steps are scripted per (card, kind): run-level dispatches have card `None`, and
//! `(None, "fix_review")` is the review of a fix wave. By default the run is the card loop
//! only (LEADHAND_TEST_CARD_LOOP_ONLY); `gates: true` runs the real end of run. `started`
//! holds card dispatches, `run_started` run-level ones.
//!
//! Claude routes: the world records `started` with a fake agent ID, does the work, appends a
//! usage record to that agent's fake transcript, writes the reply to the dispatch's reply
//! file and records the result. Codex routes run the dispatch action's own command line (the
//! `dispatch` line the proxy would run) with the fake `codex` (`Env::fakes`) on PATH.
//!
//! A step's `crash` of "before_commit" or "after_commit" makes the worker die at that point.
//! The same two points are the harness's own crash points (`crash_point`), which follow the
//! CLI's LEADHAND_CRASH_* protocol, read from `env`: with LEADHAND_CRASH_AT naming one, the
//! drive dies there with exit 97, as a crash of the session a Claude worker runs in would.
//! Only a forked drive may die so; outside one the point panics instead.
//!
//! **Forked drives.** Python drives a world in an `os.fork()` child that is its own process
//! group. Without `unsafe` there is no fork, so `fork` runs the drive in this process with
//! fork's semantics: the drive gets a process group of its own (a placeholder leader, its
//! pgid handed to the drive as Python's `os.getpid()`), every process the world starts
//! during the drive joins that group, and the world's state is restored when the drive ends,
//! as a forked child's changes never reach its parent. The drive dies, as a SIGKILLed child
//! would, once its group is killed (a crash point's LEADHAND_CRASH_KILL_PGID): the world
//! checks before each process it starts, after each one ends, and before each file it
//! writes, and unwinds the drive there. `Forked` gives the drive's exit status as `Out.code`
//! gives one (`-9` killed, `97` a harness crash point, `1` a panic, `0` a normal end).
//!
//! **Subclasses.** Python tests subclass FakeSupervisor to override `launch`, `_cli`,
//! `work_label` and `_take`; the Rust hooks for those are `before_launch`, `cli_cwd`,
//! `work_label_fn` and `take_fn`.
//!
//! `env` is the world's sealed environment (`Env`, plus the world's own settings); nothing
//! changes the test process's environment. `advance(seconds)` moves the CLI's clock: the
//! total is passed as LEADHAND_TEST_CLOCK_OFFSET on every call. Where FakeWorld returned an
//! engine object, this returns what the CLI wrote: parsed JSON lines (`charges`, `attempts`)
//! or an `Out` (`cli`).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, flock, fsync};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use super::git::{self, IDENTITY};
use super::json::{self, Json};
use super::rundir::{RunPaths, read_records, run_paths};
use super::{
    Env, Group, Out, Spawn, TIMEOUT, TempRoot, bin, read_jsonl, run_argv_spawn, same_process,
    shlex_split, start_time, started_by,
};

pub const IMPLEMENTER_KINDS: [&str; 5] = ["implement", "fix", "answer", "continue", "final_fix"];
pub const REVIEW_KINDS: [&str; 4] = ["review", "rereview", "final_review", "security"];
pub const PROJECT: &str = "-fake-project";
pub const MAIN_SESSION: &str = "fake-main-session";
/// gitfixture's identity already turns signing off.
pub const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

// --------------------------------------------------------------------------
// Cards

/// `card`'s keyword arguments.
#[derive(Clone, Debug)]
pub struct CardOpts<'a> {
    /// None (or empty): `Card number <n>`.
    pub title: Option<&'a str>,
    pub risk: &'a str,
    pub hard: bool,
    pub depends: &'a [i64],
    pub role: &'a str,
    pub route: Option<&'a str>,
    pub allowed: &'a str,
}

impl Default for CardOpts<'_> {
    fn default() -> Self {
        CardOpts {
            title: None,
            risk: "low",
            hard: false,
            depends: &[],
            role: "implementer",
            route: None,
            allowed: "src/, tests/",
        }
    }
}

/// Card `number` with the default options.
pub fn card(number: i64) -> String {
    card_with(number, CardOpts::default())
}

/// Card `number` as a deck writes it.
pub fn card_with(number: i64, o: CardOpts) -> String {
    let title = match o.title {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => format!("Card number {number}"),
    };
    let depends: Vec<String> = o.depends.iter().map(i64::to_string).collect();
    let depends = if depends.is_empty() {
        "none".to_string()
    } else {
        depends.join(", ")
    };
    let mut lines = vec![
        format!("## Card {number}: {title}"),
        format!("- Role: {}", o.role),
        format!(
            "- Hard: {}",
            if o.hard {
                "yes — touches the core"
            } else {
                "no"
            }
        ),
        format!("- Risk: {}", o.risk),
        format!("- Depends on: {depends}"),
        format!("- Allowed paths: {}", o.allowed),
        "- Done when: the card's file exists and the tests pass".to_string(),
        "- Stop if: the design has to change".to_string(),
    ];
    if let Some(route) = o.route.filter(|r| !r.is_empty()) {
        lines.push(format!("- Route: {route}"));
    }
    lines.join("\n") + "\n"
}

// --------------------------------------------------------------------------
// Steps

/// A step's tamper: runs inside the worker, before its report is written, with the world,
/// the workspace and the dispatch action.
pub type Tamper = Box<dyn Fn(&FakeSupervisor, &Path, &Json)>;
/// `FakeSupervisor::before_launch`: runs with the world and the dispatch action.
pub type LaunchHook = Box<dyn Fn(&FakeSupervisor, &Json)>;
/// `FakeSupervisor::work_label_fn`: the label for the dispatch action.
pub type LabelHook = Box<dyn Fn(&FakeSupervisor, &Json) -> String>;
/// `FakeSupervisor::take_fn`: the step for the dispatch action.
pub type TakeHook = Box<dyn Fn(&FakeSupervisor, &Json) -> Step>;

/// One scripted worker step: Python's step dict, a field per key (None: the key is absent).
#[derive(Default)]
pub struct Step {
    /// `Status:` of an implementer-type report (default READY); set, any kind reports so.
    pub status: Option<String>,
    pub blocked_by: Option<String>,
    /// A review's verdict (default clean).
    pub verdict: Option<String>,
    /// A review's findings; None: none when clean, else one high finding.
    pub findings: Option<Vec<String>>,
    /// A verify kind's `Result:` (default pass).
    pub result: Option<String>,
    /// Rebase onto the run tip first (default: only for integrate).
    pub rebase: Option<bool>,
    /// (path, text) to write and commit (None or empty: one default file).
    pub files: Option<Vec<(String, String)>>,
    /// "before_commit" or "after_commit": the worker dies there.
    pub crash: Option<String>,
    pub tamper: Option<Tamper>,
    /// Write the report (default true).
    pub report: Option<bool>,
    /// None: the default usage record; Some(None): no transcript; Some(Some(t)): `t`.
    pub transcript: Option<Option<String>>,
    /// The reply (None or empty: the report's first line and its path).
    pub reply: Option<String>,
    /// A Codex worker sleeps this long first.
    pub sleep: Option<f64>,
    /// A Codex implementer commits its files (default true; false leaves it to the runner).
    pub commit: Option<bool>,
}

// --------------------------------------------------------------------------
// A background process (Python's subprocess.Popen, for proxies)

/// A process started in the background with piped output: the parts of Python's
/// `subprocess.Popen` the tests use. Clones share the process.
#[derive(Clone)]
pub struct Proc {
    pid: u32,
    state: Arc<Mutex<ProcState>>,
}

struct ProcState {
    child: Child,
    code: Option<i32>,
    outputs: mpsc::Receiver<(u8, Vec<u8>)>,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
}

fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(0))
}

impl Proc {
    /// Wraps a started child, reading its piped stdout and stderr in the background.
    pub fn new(mut child: Child) -> Proc {
        let (tx, rx) = mpsc::channel();
        let read = |n: u8, pipe: Option<Box<dyn Read + Send>>| match pipe {
            Some(mut pipe) => {
                let tx = tx.clone();
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = pipe.read_to_end(&mut buf);
                    let _ = tx.send((n, buf));
                });
                None
            }
            None => Some(Vec::new()),
        };
        let stdout = read(
            1,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        let stderr = read(
            2,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        );
        Proc {
            pid: child.id(),
            state: Arc::new(Mutex::new(ProcState {
                child,
                code: None,
                outputs: rx,
                stdout,
                stderr,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ProcState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn pid(&self) -> i32 {
        self.pid as i32
    }

    /// The exit code once it has exited (minus the signal when one ended it); None while it
    /// runs (`Popen.poll`).
    pub fn poll(&self) -> Option<i32> {
        let mut s = self.lock();
        if s.code.is_none()
            && let Ok(Some(status)) = s.child.try_wait()
        {
            s.code = Some(code_of(status));
        }
        s.code
    }

    /// The exit code seen so far (`Popen.returncode`).
    pub fn returncode(&self) -> Option<i32> {
        self.lock().code
    }

    /// SIGKILL to the process (`Popen.kill`); nothing once it has been reaped.
    pub fn kill(&self) {
        let mut s = self.lock();
        if s.code.is_none() {
            let _ = s.child.kill();
        }
    }

    /// Waits for it to exit; its exit code.
    pub fn wait(&self) -> i32 {
        let mut s = self.lock();
        if s.code.is_none() {
            let status = s.child.wait().expect("waiting for a child");
            s.code = Some(code_of(status));
        }
        s.code.unwrap()
    }

    /// Both outputs to their end and the exit, within `timeout` (`Popen.communicate`); None
    /// when it ran out first (TimeoutExpired), the outputs still being read.
    pub fn communicate(&self, timeout: Duration) -> Option<(Vec<u8>, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        while s.stdout.is_none() || s.stderr.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match s.outputs.recv_timeout(left) {
                Ok((1, buf)) => s.stdout = Some(buf),
                Ok((_, buf)) => s.stderr = Some(buf),
                Err(_) => return None,
            }
        }
        let mut pause = Duration::from_millis(1);
        while s.code.is_none() {
            if let Ok(Some(status)) = s.child.try_wait() {
                s.code = Some(code_of(status));
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            thread::sleep(pause.min(deadline - now));
            pause = (pause * 2).min(Duration::from_millis(20));
        }
        Some((s.stdout.clone().unwrap(), s.stderr.clone().unwrap()))
    }
}

// --------------------------------------------------------------------------
// The world

/// `FakeSupervisor`'s keyword arguments.
#[derive(Clone, Debug)]
pub struct Options<'a> {
    pub config: &'a str,
    pub installed: &'a [&'a str],
    pub title: &'a str,
    pub gates: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            config: "",
            installed: &["claude"],
            title: "Demo deck",
            gates: false,
        }
    }
}

/// `cli`'s keyword arguments: a crash point to arm, stdin, and variables over the world's.
#[derive(Clone, Debug, Default)]
pub struct CliOpts<'a> {
    pub crash: Option<&'a str>,
    pub stdin: Option<&'a str>,
    pub env: Vec<(OsString, OsString)>,
}

/// How a forked drive (`FakeSupervisor::fork`) ended.
#[derive(Clone, Debug)]
pub struct Forked {
    /// Its exit status as `Out.code` gives one: `-9` when its group was killed, `97` at a
    /// harness crash point, `1` when it panicked, `0` when it returned.
    pub code: i32,
    /// The panic's message when it panicked (Python's traceback in the drive's error file).
    pub error: Option<String>,
    /// True when it ran past its deadline and its group was killed.
    pub hung: bool,
}

/// The drive's process group: its placeholder leader's pid and start time.
#[derive(Clone, Debug)]
struct Drive {
    pgid: i32,
    start: String,
}

/// The unwinding payloads that end a drive as a process exit would.
struct DriveExit(i32);
struct DriveKilled;

/// The world's state a forked drive may change, put back when it ends.
struct Saved {
    env: Env,
    offset: f64,
    run_dir: Option<PathBuf>,
    steps: HashMap<(Option<i64>, String), Vec<Rc<Step>>>,
    rulings: HashMap<i64, Vec<String>>,
    parks: HashMap<i64, String>,
    diff_checks: HashMap<i64, Vec<(bool, String)>>,
    started: Vec<Json>,
    run_started: Vec<Json>,
    proxies: Vec<Proc>,
    launch_log: Option<PathBuf>,
    popen_log: Option<PathBuf>,
    cli_cwd: Option<PathBuf>,
    ids: u64,
    messages: u64,
    drive: Option<Drive>,
}

pub struct FakeSupervisor {
    pub tmp: PathBuf,
    pub env: Env,
    /// `<tmp>/my repo`.
    pub repo: PathBuf,
    pub docs: PathBuf,
    pub claude_home: PathBuf,
    pub codex_home: PathBuf,
    pub config_file: PathBuf,
    pub offset: f64,
    pub deck_path: PathBuf,
    pub base: String,
    pub run_dir: Option<PathBuf>,
    pub steps: HashMap<(Option<i64>, String), Vec<Rc<Step>>>,
    pub rulings: HashMap<i64, Vec<String>>,
    pub parks: HashMap<i64, String>,
    pub diff_checks: HashMap<i64, Vec<(bool, String)>>,
    pub started: Vec<Json>,
    pub run_started: Vec<Json>,
    pub proxies: Vec<Proc>,
    pub launch_log: Option<PathBuf>,
    /// When set, every process the world starts appends `{"pid", "start", "argv"}` here:
    /// the Rust form of patching `subprocess.Popen` with a logging subclass.
    pub popen_log: Option<PathBuf>,
    /// The cwd of the engine steps (None: `tmp`); Python's CwdSupervisor `cwd`.
    pub cli_cwd: Option<PathBuf>,
    /// Runs first in `launch` (a subclass's `launch` override before `super().launch`).
    pub before_launch: Option<LaunchHook>,
    /// Replaces `work_label` (a subclass's override).
    pub work_label_fn: Option<LabelHook>,
    /// Replaces `take`'s scripted queues (a subclass's `_take` override).
    pub take_fn: Option<TakeHook>,
    ids: u64,
    messages: u64,
    drive: Option<Drive>,
    _root: TempRoot,
}

/// Python's `str.splitlines`.
fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let sep = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if sep {
            lines.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, n)| *n == '\n') {
                chars.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Python's truth value of a JSON value.
fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Num(_) => v.as_f64().is_some_and(|f| f != 0.0),
        Json::Str(s) => !s.is_empty(),
        Json::Arr(items) => !items.is_empty(),
        Json::Obj(members) => !members.is_empty(),
    }
}

#[track_caller]
fn text(v: &Json) -> &str {
    v.as_str()
        .unwrap_or_else(|| panic!("not a UTF-8 string: {v}"))
}

#[track_caller]
fn os_path(v: &Json) -> PathBuf {
    PathBuf::from(
        v.as_os()
            .unwrap_or_else(|| panic!("not a path string: {v}")),
    )
}

fn os_strs(v: &[OsString]) -> Vec<&OsStr> {
    v.iter().map(OsString::as_os_str).collect()
}

/// A duration in seconds as Python's `{:g}` prints it: six significant digits, trailing
/// zeros dropped (`180`, `0.5`, `1e+06`).
fn seconds_g(d: Duration) -> String {
    let s = d.as_secs_f64();
    let trim = |t: String| {
        if t.contains('.') {
            t.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            t
        }
    };
    let sci = format!("{s:.5e}");
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if (-4..6).contains(&exp) {
        trim(format!("{s:.*}", (5 - exp).max(0) as usize))
    } else {
        format!(
            "{}e{}{:02}",
            trim(mantissa.to_string()),
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    }
}

#[track_caller]
fn append(path: &Path, data: &[u8]) {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut f| f.write_all(data))
        .unwrap_or_else(|e| panic!("appending to {}: {e}", path.display()));
}

#[track_caller]
fn make_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("making {}: {e}", parent.display()));
    }
}

impl FakeSupervisor {
    /// A world with `cards`, Claude installed, the card loop only.
    #[track_caller]
    pub fn new(cards: &[String]) -> FakeSupervisor {
        FakeSupervisor::with_options(cards, Options::default())
    }

    #[track_caller]
    pub fn with_options(cards: &[String], o: Options) -> FakeSupervisor {
        let root = TempRoot::new("lh engine ");
        let tmp = root.path().to_path_buf();
        let mut env = Env::with_options(&tmp, o.installed, o.config);
        env.set("FAKE_CODEX_LOG", tmp.join("codex log.jsonl"));
        if !o.gates {
            env.set("LEADHAND_TEST_CARD_LOOP_ONLY", "1");
        }
        let repo = git::make_repo(&tmp);
        let path_of = |key: &str| PathBuf::from(env.get(key).unwrap());
        let (claude_home, codex_home, config_file) = (
            path_of("CLAUDE_CONFIG_DIR"),
            path_of("CODEX_HOME"),
            path_of("LEADHAND_CONFIG"),
        );
        let deck_path = tmp.join("decks dir").join("demo deck.md");
        fs::create_dir(deck_path.parent().unwrap()).unwrap();
        let base = git::run_git(&repo, &["rev-parse", "HEAD"]);
        let w = FakeSupervisor {
            docs: tmp.join("docs dir"),
            tmp,
            env,
            repo,
            claude_home,
            codex_home,
            config_file,
            offset: 0.0,
            deck_path,
            base,
            run_dir: None,
            steps: HashMap::new(),
            rulings: HashMap::new(),
            parks: HashMap::new(),
            diff_checks: HashMap::new(),
            started: Vec::new(),
            run_started: Vec::new(),
            proxies: Vec::new(),
            launch_log: None,
            popen_log: None,
            cli_cwd: None,
            before_launch: None,
            work_label_fn: None,
            take_fn: None,
            ids: 1,
            messages: 1,
            drive: None,
            _root: root,
        };
        w.write_deck_with(cards, o.title, "leadhand/demo");
        w
    }

    // -- setup ----------------------------------------------------------------

    /// The deck file, titled "Demo deck", on branch leadhand/demo.
    #[track_caller]
    pub fn write_deck(&self, cards: &[String]) {
        self.write_deck_with(cards, "Demo deck", "leadhand/demo");
    }

    #[track_caller]
    pub fn write_deck_with(&self, cards: &[String], title: &str, branch: &str) {
        let text = format!(
            "# Deck: {title}\nSource: docs/plan.md\nBase: main @ {}\nBranch: {branch}\nBuild: true\nTest: true\n\n{}",
            self.base,
            cards.join("\n")
        );
        fs::write(&self.deck_path, text)
            .unwrap_or_else(|e| panic!("writing {}: {e}", self.deck_path.display()));
    }

    /// `start` the deck; the run directory is the one it prints (`run:`, or `existing run:`).
    #[track_caller]
    pub fn start(&mut self) -> PathBuf {
        self.start_with(false)
    }

    /// `start --new` (`start(new=True)`).
    #[track_caller]
    pub fn start_new(&mut self) -> PathBuf {
        self.start_with(true)
    }

    #[track_caller]
    fn start_with(&mut self, new: bool) -> PathBuf {
        let mut args: Vec<OsString> = vec![
            "start".into(),
            self.deck_path.clone().into(),
            "--repo".into(),
            self.repo.clone().into(),
        ];
        if new {
            args.push("--new".into());
        }
        let r = self.engine_cli(&os_strs(&args)).ok();
        let stdout = r.stdout_text();
        for line in splitlines(&stdout) {
            for prefix in ["run: ", "existing run: "] {
                if let Some(dir) = line.strip_prefix(prefix) {
                    let dir = PathBuf::from(dir);
                    self.run_dir = Some(dir.clone());
                    return dir;
                }
            }
        }
        panic!("leadhand start printed no `run: <dir>` line\n{r}");
    }

    /// Appends `steps` to the queue for (`card_number`, `kind`); `None` for run level.
    pub fn script(
        &mut self,
        card_number: impl Into<Option<i64>>,
        kind: &str,
        steps: impl IntoIterator<Item = Step>,
    ) {
        self.steps
            .entry((card_number.into(), kind.to_string()))
            .or_default()
            .extend(steps.into_iter().map(Rc::new));
    }

    pub fn advance(&mut self, seconds: f64) {
        self.offset += seconds;
    }

    /// `session_with(session_id, 1)`.
    #[track_caller]
    pub fn session(&mut self, session_id: &str) -> PathBuf {
        self.session_with(session_id, 1)
    }

    /// Points CLAUDE_CODE_SESSION_ID at a supervisor transcript and appends `messages`
    /// messages to it.
    #[track_caller]
    pub fn session_with(&mut self, session_id: &str, messages: u32) -> PathBuf {
        self.env.set("CLAUDE_CODE_SESSION_ID", session_id);
        let path = self
            .claude_home
            .join("projects")
            .join(PROJECT)
            .join(format!("{session_id}.jsonl"));
        for _ in 0..messages {
            self.append_message(&path, "claude-opus-4");
        }
        path
    }

    // -- reading --------------------------------------------------------------

    #[track_caller]
    pub fn paths(&self) -> RunPaths {
        match &self.run_dir {
            Some(dir) => run_paths(dir),
            None => panic!("no run yet: call start() first"),
        }
    }

    #[track_caller]
    pub fn meta(&self) -> Json {
        super::read_json(&self.paths().run_json)
    }

    #[track_caller]
    pub fn workspace(&self) -> PathBuf {
        os_path(&self.meta()["workspace"])
    }

    /// The run's events, or only those of `kind` (`events(kind=None)`).
    #[track_caller]
    pub fn events<'k>(&self, kind: impl Into<Option<&'k str>>) -> Vec<Json> {
        let kind = kind.into();
        read_records(&self.paths().events)
            .into_iter()
            .filter(|e| kind.is_none_or(|k| e["kind"] == k))
            .collect()
    }

    #[track_caller]
    pub fn charges(&self) -> Vec<Json> {
        read_records(&self.paths().charges)
    }

    /// The engine's attempt records (`recorded_by: engine`).
    #[track_caller]
    pub fn attempts(&self) -> Vec<Json> {
        read_records(&self.paths().attempts)
            .into_iter()
            .filter(|a| a.get("recorded_by").is_some_and(|v| v == "engine"))
            .collect()
    }

    pub fn dispatch_ids(&self) -> Vec<String> {
        self.started
            .iter()
            .map(|a| text(&a["dispatch_id"]).to_string())
            .collect()
    }

    pub fn run_dispatch_ids(&self) -> Vec<String> {
        self.run_started
            .iter()
            .map(|a| text(&a["dispatch_id"]).to_string())
            .collect()
    }

    #[track_caller]
    pub fn branch_tip(&self, branch: &str) -> String {
        self.git(&self.repo, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    // -- driving --------------------------------------------------------------

    #[track_caller]
    pub fn next(&self) -> Json {
        let run_dir = self.run_dir_arg();
        self.engine_cli(&[OsStr::new("next"), &run_dir]).ok().json()
    }

    /// `run_limit(80)`.
    #[track_caller]
    pub fn run(&mut self) -> Vec<Json> {
        self.run_limit(80)
    }

    /// `next` and `act` until `done`; every action. Panics after `limit` actions.
    #[track_caller]
    pub fn run_limit(&mut self, limit: usize) -> Vec<Json> {
        let mut actions = Vec::new();
        for _ in 0..limit {
            let a = self.next();
            actions.push(a.clone());
            if a["action"] == "done" {
                return actions;
            }
            self.act(&a);
        }
        let tail = &actions[actions.len().saturating_sub(3)..];
        panic!("the run did not finish in {limit} actions: {tail:?}");
    }

    #[track_caller]
    pub fn act(&mut self, a: &Json) {
        match a["action"].as_str() {
            Some("dispatch") => {
                let worker = self.launch(a);
                if let Some(reply) = self.perform(a, &worker) {
                    let reply_file = os_path(&a["reply_file"]);
                    self.check_alive();
                    make_parent(&reply_file);
                    fs::write(&reply_file, reply)
                        .unwrap_or_else(|e| panic!("writing {}: {e}", reply_file.display()));
                    let did = text(&a["dispatch_id"]);
                    self.record(
                        "result",
                        &[
                            OsStr::new("--dispatch"),
                            OsStr::new(did),
                            OsStr::new("--worker"),
                            OsStr::new(&worker),
                            OsStr::new("--reply-file"),
                            reply_file.as_os_str(),
                        ],
                    );
                }
            }
            Some("answer") => {
                let n = a["card"]
                    .as_i64()
                    .unwrap_or_else(|| panic!("an answer with no card: {a}"));
                let card = n.to_string();
                if let Some(reason) = self.parks.get(&n).cloned() {
                    self.record(
                        "park",
                        &[
                            OsStr::new("--card"),
                            OsStr::new(&card),
                            OsStr::new("--reason"),
                            OsStr::new(&reason),
                        ],
                    );
                } else {
                    let queue = self
                        .rulings
                        .get_mut(&n)
                        .unwrap_or_else(|| panic!("no ruling scripted for card {n}: {a}"));
                    if queue.is_empty() {
                        panic!("no ruling left for card {n}: {a}");
                    }
                    let ruling = queue.remove(0);
                    self.record(
                        "ruling",
                        &[
                            OsStr::new("--card"),
                            OsStr::new(&card),
                            OsStr::new("--text"),
                            OsStr::new(&ruling),
                        ],
                    );
                }
            }
            Some("check_diff") => {
                let n = a["card"].as_i64();
                let (ok, note) = match n.and_then(|n| self.diff_checks.get_mut(&n)) {
                    Some(queue) if !queue.is_empty() => queue.remove(0),
                    _ => (true, "test-only fix looks right".to_string()),
                };
                let card = match n {
                    Some(n) => n.to_string(),
                    None => json::dumps(&a["card"]),
                };
                self.record(
                    "diff-check",
                    &[
                        OsStr::new("--card"),
                        OsStr::new(&card),
                        OsStr::new("--ok"),
                        OsStr::new(if ok { "yes" } else { "no" }),
                        OsStr::new("--note"),
                        OsStr::new(&note),
                    ],
                );
            }
            _ => panic!("unexpected action in a synchronous world: {a}"),
        }
    }

    /// Starts the dispatch's worker: records `started` with its agent ID.
    #[track_caller]
    pub fn launch(&mut self, a: &Json) -> String {
        if let Some(hook) = &self.before_launch {
            hook(self, a);
        }
        let resume = &a["resume"];
        let worker = if truthy(resume) {
            text(&resume["worker_id"]).to_string()
        } else {
            format!("agent{:04}", self.next_id())
        };
        let did = text(&a["dispatch_id"]).to_string();
        self.record(
            "started",
            &[
                OsStr::new("--dispatch"),
                OsStr::new(&did),
                OsStr::new("--worker"),
                OsStr::new(&worker),
            ],
        );
        if a["card"].is_null() {
            self.run_started.push(a.clone());
        } else {
            self.started.push(a.clone());
        }
        if let Some(log) = &self.launch_log {
            self.check_alive();
            append(log, format!("{did}\n").as_bytes());
        }
        worker
    }

    /// Runs the worker to its end and returns its reply (None if it died).
    #[track_caller]
    pub fn perform(&mut self, a: &Json, worker: &str) -> Option<String> {
        if a["agent"] == "leadhand:codex" {
            let step = self.take(a);
            let reply = self.run_proxy(a, &step);
            let transcript = self.subagent_transcript(worker);
            self.append_message(&transcript, "claude-haiku-4");
            return Some(reply);
        }
        self.claude_worker(a, worker)
    }

    // -- workers --------------------------------------------------------------

    /// The next scripted step for the dispatch (`_take`); an empty step when none is left.
    pub fn take(&mut self, a: &Json) -> Rc<Step> {
        if let Some(f) = &self.take_fn {
            return Rc::new(f(self, a));
        }
        let fix_review =
            a["card"].is_null() && a["kind"] == "final_review" && a.get("wave").is_some_and(truthy);
        let kind = if fix_review {
            "fix_review"
        } else {
            text(&a["kind"])
        };
        match self.steps.get_mut(&(a["card"].as_i64(), kind.to_string())) {
            Some(queue) if !queue.is_empty() => queue.remove(0),
            _ => Rc::new(Step::default()),
        }
    }

    pub fn report_text(&self, a: &Json, step: &Step) -> String {
        let kind = text(&a["kind"]);
        if step.status.is_some() || IMPLEMENTER_KINDS.contains(&kind) || kind == "integrate" {
            let status = step.status.as_deref().unwrap_or("READY");
            let mut out = format!("Status: {status}\n");
            if let Some(by) = step.blocked_by.as_deref().filter(|b| !b.is_empty()) {
                out += &format!("Blocked by: {by}\n");
            }
            out += &format!(
                "## Summary\nFake work for {}.\n## Commits\n## Tests\nfake: pass\n",
                text(&a["dispatch_id"])
            );
            if status == "QUESTION" {
                out += "## Question\nWhich storage format should widgets use?\nOptions:\n\
                        - A: JSON\n- B: SQLite\nRecommendation: A, because it is simpler.\n";
            }
            return out + "## Concerns\nNone\n";
        }
        if REVIEW_KINDS.contains(&kind) {
            let verdict = step.verdict.as_deref().unwrap_or("clean");
            let findings = match &step.findings {
                Some(f) => f.clone(),
                None if verdict == "clean" => Vec::new(),
                None => vec!["[high] src/widget.py:1 — wrong — fix it".to_string()],
            };
            let body: Vec<String> = findings
                .iter()
                .enumerate()
                .map(|(i, f)| format!("{}. {f}", i + 1))
                .collect();
            let body = if body.is_empty() {
                "None".to_string()
            } else {
                body.join("\n")
            };
            return format!("Verdict: {verdict}\n## Findings\n{body}\n## Notes\nRead the diff.\n");
        }
        format!(
            "Result: {}\n## Checks\nfake\n",
            step.result.as_deref().unwrap_or("pass")
        )
    }

    /// The files an implementer step writes: its own, or one default file.
    fn step_files(&self, a: &Json, step: &Step) -> Vec<(String, String)> {
        match &step.files {
            Some(files) if !files.is_empty() => files.clone(),
            _ => vec![(self.default_file(a), format!("{}\n", self.work_label(a)))],
        }
    }

    #[track_caller]
    pub fn claude_worker(&mut self, a: &Json, worker: &str) -> Option<String> {
        let step = self.take(a);
        let ws = self.workspace();
        let kind = text(&a["kind"]).to_string();
        if step.rebase.unwrap_or(kind == "integrate") {
            let branch = text(&self.meta()["branch"]).to_string();
            let run_tip = self.git(&ws, &["rev-parse", &format!("refs/heads/{branch}")]);
            let mut args: Vec<&str> = NO_HOOKS.to_vec();
            args.extend([
                "-c",
                "core.editor=true",
                "rebase",
                "--quiet",
                "-X",
                "theirs",
            ]);
            args.push(&run_tip);
            self.git(&ws, &args);
        }
        if IMPLEMENTER_KINDS.contains(&kind.as_str()) {
            let files = self.step_files(a, &step);
            let pairs: Vec<(&str, &str)> = files
                .iter()
                .map(|(p, t)| (p.as_str(), t.as_str()))
                .collect();
            self.check_alive();
            git::write(&ws, &pairs);
            self.crash_point("before_commit");
            if step.crash.as_deref() == Some("before_commit") {
                return None;
            }
            let mut add = vec!["add", "--"];
            add.extend(pairs.iter().map(|(p, _)| *p));
            self.git(&ws, &add);
            if !self
                .git(&ws, &["diff", "--cached", "--name-only"])
                .is_empty()
            {
                let message = format!("work for {}", self.work_label(a));
                let mut args: Vec<&str> = NO_HOOKS.to_vec();
                args.extend(["commit", "--quiet", "-m", &message]);
                self.git(&ws, &args);
            }
            self.crash_point("after_commit");
            if step.crash.as_deref() == Some("after_commit") {
                return None;
            }
        }
        if let Some(tamper) = &step.tamper {
            tamper(self, &ws, a);
        }
        // Path(a["report"]), as pathlib spells it.
        let report: PathBuf = os_path(&a["report"]).components().collect();
        let text = self.report_text(a, &step);
        if step.report.unwrap_or(true) {
            self.check_alive();
            make_parent(&report);
            fs::write(&report, &text)
                .unwrap_or_else(|e| panic!("writing {}: {e}", report.display()));
        }
        match &step.transcript {
            None => {
                let path = self.subagent_transcript(worker);
                self.append_message(&path, "claude-sonnet-4");
            }
            Some(None) => {}
            Some(Some(t)) => {
                let path = self.subagent_transcript(worker);
                self.check_alive();
                make_parent(&path);
                append(&path, t.as_bytes());
            }
        }
        Some(match step.reply.as_deref() {
            Some(r) if !r.is_empty() => r.to_string(),
            _ => format!(
                "{}\nReport: {}",
                splitlines(&text)[0],
                report.to_string_lossy()
            ),
        })
    }

    /// The label a worker's work is named by: the dispatch ID (or `work_label_fn`'s).
    pub fn work_label(&self, a: &Json) -> String {
        match &self.work_label_fn {
            Some(f) => f(self, a),
            None => text(&a["dispatch_id"]).to_string(),
        }
    }

    pub fn default_file(&self, a: &Json) -> String {
        let folder = if a["card"].is_null() {
            "final".to_string()
        } else {
            format!("card{}", json::dumps(&a["card"]))
        };
        format!("src/{folder}/{}.txt", self.work_label(a))
    }

    pub fn subagent_transcript(&self, worker: &str) -> PathBuf {
        self.claude_home
            .join("projects")
            .join(PROJECT)
            .join(MAIN_SESSION)
            .join("subagents")
            .join(format!("agent-{worker}.jsonl"))
    }

    /// Appends one assistant message with a usage record to the transcript `path`
    /// (`_append_message`).
    #[track_caller]
    pub fn append_message(&mut self, path: &Path, model: &str) {
        let id = format!("msg_{:05}", self.messages);
        self.messages += 1;
        let usage = Json::object([
            ("input_tokens", Json::from(100)),
            ("output_tokens", Json::from(20)),
            ("cache_read_input_tokens", Json::from(0)),
            ("cache_creation_input_tokens", Json::from(0)),
        ]);
        let rec = Json::object([
            ("type", Json::from("assistant")),
            (
                "message",
                Json::object([
                    ("id", Json::from(id)),
                    ("model", Json::from(model)),
                    ("usage", usage),
                ]),
            ),
        ]);
        self.check_alive();
        make_parent(path);
        append(path, format!("{}\n", json::dumps(&rec)).as_bytes());
    }

    /// The fake Codex's scenario for the dispatch: the worker's work, report and reply.
    #[track_caller]
    pub fn codex_scenario(&self, a: &Json, step: &Step) -> PathBuf {
        let mut lines = vec![
            Json::object([
                ("type", Json::from("thread.started")),
                ("thread_id", Json::from("{thread}")),
            ]),
            Json::object([("type", Json::from("turn.started"))]),
        ];
        if let Some(seconds) = step.sleep.filter(|s| *s != 0.0) {
            lines.push(Json::object([
                ("action", Json::from("sleep")),
                ("seconds", Json::from(seconds)),
            ]));
        }
        if IMPLEMENTER_KINDS.contains(&text(&a["kind"])) {
            for (p, t) in self.step_files(a, step) {
                lines.push(Json::object([
                    ("action", Json::from("write_file")),
                    ("path", Json::from(p)),
                    ("text", Json::from(t)),
                ]));
            }
            if step.commit.unwrap_or(true) {
                lines.push(Json::object([
                    ("action", Json::from("commit")),
                    (
                        "message",
                        Json::from(format!("work for {}", self.work_label(a))),
                    ),
                ]));
            }
        }
        let report_text = self.report_text(a, step);
        let reply = match step.reply.as_deref() {
            Some(r) if !r.is_empty() => r.to_string(),
            _ => format!(
                "{}\nReport: {}",
                splitlines(&report_text)[0],
                text(&a["report"])
            ),
        };
        if step.report.unwrap_or(true) {
            lines.push(Json::object([
                ("action", Json::from("write_file")),
                ("path", a["report"].clone()),
                ("text", Json::from(report_text)),
            ]));
        }
        let usage = Json::object([
            ("input_tokens", Json::from(1000)),
            ("cached_input_tokens", Json::from(100)),
            ("cache_write_input_tokens", Json::from(0)),
            ("output_tokens", Json::from(40)),
            ("reasoning_output_tokens", Json::from(5)),
        ]);
        lines.extend([
            Json::object([
                ("type", Json::from("item.completed")),
                (
                    "item",
                    Json::object([
                        ("id", Json::from("item_9")),
                        ("type", Json::from("agent_message")),
                        ("text", Json::from(reply.as_str())),
                    ]),
                ),
            ]),
            Json::object([
                ("action", Json::from("last_message")),
                ("text", Json::from(reply.as_str())),
            ]),
            Json::object([("type", Json::from("turn.completed")), ("usage", usage)]),
        ]);
        let path = self
            .tmp
            .join(format!("scenario {}.jsonl", text(&a["dispatch_id"])));
        let body: String = lines.iter().map(|l| json::dumps(l) + "\n").collect();
        self.check_alive();
        fs::write(&path, body).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        path
    }

    /// The dispatch action's own command line, as is, and its environment: the fake
    /// `codex` on PATH, playing `codex_scenario`.
    #[track_caller]
    pub fn proxy_argv_env(&mut self, a: &Json, step: &Step) -> (Vec<String>, Env) {
        let argv = shlex_split(text(&a["message"]))
            .unwrap_or_else(|| panic!("the dispatch line is not shell words: {}", a["message"]));
        let scenario = self.codex_scenario(a, step);
        let thread = format!("0f0f0f0f-0000-4000-8000-{:012}", self.next_id());
        let mut env = self.clock_env();
        env.set("PATH", self.env.path_with_fakes())
            .set("FAKE_CODEX_SCENARIO", scenario)
            .set("FAKE_CODEX_THREAD", thread);
        (argv, env)
    }

    /// `proxy_with(a, step, None, 180 s, 20)`.
    #[track_caller]
    pub fn proxy(&mut self, a: &Json, step: &Step) -> Out {
        self.proxy_with(a, step, None, Duration::from_secs(180), 20)
    }

    /// What the leadhand:codex proxy does: run the command, again while it is still running
    /// (exit 75), from `cwd` (default the world's tmp). Bounded: all the runs together get
    /// `timeout`, and at most `attempts` runs; past either, a run still exiting 75 fails the
    /// test with the last result.
    #[track_caller]
    pub fn proxy_with(
        &mut self,
        a: &Json,
        step: &Step,
        cwd: Option<&Path>,
        timeout: Duration,
        attempts: u32,
    ) -> Out {
        let (argv, env) = self.proxy_argv_env(a, step);
        let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
        let cwd = cwd.unwrap_or(&self.tmp).to_path_buf();
        let deadline = Instant::now() + timeout;
        let mut last = None;
        for _ in 0..attempts {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let r = self.exec(&argv, &env, &cwd, None, left);
            if r.code != 75 {
                return r;
            }
            last = Some(r);
        }
        panic!(
            "the dispatch line was still running (exit 75) after {attempts} runs or {} s\n{}",
            seconds_g(timeout),
            last.map_or("(no run)".to_string(), |r| r.to_string())
        );
    }

    /// `proxy`, returning what the proxy hands back: the command's stdout and stderr.
    #[track_caller]
    pub fn run_proxy(&mut self, a: &Json, step: &Step) -> String {
        self.run_proxy_with(a, step, None)
    }

    /// `run_proxy` from `cwd`.
    #[track_caller]
    pub fn run_proxy_with(&mut self, a: &Json, step: &Step, cwd: Option<&Path>) -> String {
        let r = self.proxy_with(a, step, cwd, Duration::from_secs(180), 20);
        let mut both = r.stdout;
        both.extend_from_slice(&r.stderr);
        String::from_utf8_lossy(&both).into_owned()
    }

    /// The dispatch line in the background, from the world's tmp, stdout and stderr piped;
    /// kept in `proxies` for the world's cleanup.
    #[track_caller]
    pub fn start_proxy(&mut self, a: &Json, step: &Step) -> Proc {
        let (argv, env) = self.proxy_argv_env(a, step);
        self.check_alive();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&self.tmp)
            .env_clear()
            .envs(env.vars())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = &self.drive {
            cmd.process_group(d.pgid);
        }
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.check_alive();
                panic!("could not start {argv:?}: {e}");
            }
        };
        self.log_popen(child.id(), &argv);
        let p = Proc::new(child);
        self.proxies.push(p.clone());
        p
    }

    /// The harness's crash point `name`, on the CLI's protocol, read from `env`: log every
    /// point passed to LEADHAND_CRASH_LOG; when LEADHAND_CRASH_AT names this one, count it in
    /// LEADHAND_CRASH_COUNTER under flock, past LEADHAND_CRASH_SKIP passes SIGKILL
    /// LEADHAND_CRASH_KILL_PGID's group (0: the drive's own) and exit 97 at once. Only a
    /// forked drive (`fork`) exits; outside one this panics.
    #[track_caller]
    pub fn crash_point(&self, name: &str) {
        let env = &self.env;
        let set = |key: &str| env.get(key).filter(|v| !v.is_empty());
        if env.get("LEADHAND_TESTING") != Some(OsStr::new("1")) {
            return;
        }
        if let Some(log) = set("LEADHAND_CRASH_LOG") {
            OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o644)
                .open(log)
                .and_then(|mut f| f.write_all(format!("{name}\n").as_bytes()))
                .unwrap_or_else(|e| panic!("writing the crash log {log:?}: {e}"));
        }
        if env.get("LEADHAND_CRASH_AT") != Some(OsStr::new(name)) {
            return;
        }
        if let Some(counter) = set("LEADHAND_CRASH_COUNTER") {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o644)
                .open(counter)
                .unwrap_or_else(|e| panic!("opening the crash counter {counter:?}: {e}"));
            flock(&f, FlockOperation::LockExclusive)
                .unwrap_or_else(|e| panic!("locking the crash counter {counter:?}: {e}"));
            let mut buf = [0u8; 64];
            let n = f
                .read_at(&mut buf, 0)
                .unwrap_or_else(|e| panic!("reading the crash counter {counter:?}: {e}"));
            let seen_text = std::str::from_utf8(&buf[..n])
                .unwrap_or_else(|e| panic!("the crash counter {counter:?} is not UTF-8: {e}"))
                .trim();
            let seen: i64 = if seen_text.is_empty() {
                1
            } else {
                seen_text.parse::<i64>().unwrap_or_else(|e| {
                    panic!("the crash counter {counter:?} holds {seen_text:?}: {e}")
                }) + 1
            };
            f.set_len(0)
                .and_then(|()| f.write_at(format!("{seen}\n").as_bytes(), 0).map(|_| ()))
                .unwrap_or_else(|e| panic!("writing the crash counter {counter:?}: {e}"));
            fsync(&f).unwrap_or_else(|e| panic!("syncing the crash counter {counter:?}: {e}"));
            drop(f); // closing it releases the lock
            let skip: i64 =
                match set("LEADHAND_CRASH_SKIP") {
                    None => 0,
                    Some(v) => v.to_string_lossy().trim().parse().unwrap_or_else(|e| {
                        panic!("LEADHAND_CRASH_SKIP={v:?} is not an integer: {e}")
                    }),
                };
            if seen <= skip {
                return;
            }
        }
        let Some(drive) = &self.drive else {
            panic!(
                "crash point {name} reached in the test process itself; \
                 drive the world in a forked child to crash there"
            );
        };
        if let Some(group) = set("LEADHAND_CRASH_KILL_PGID") {
            let pgid = match group.to_string_lossy().trim().parse::<i32>() {
                Ok(0) => Some(drive.pgid),
                Ok(g) => Some(g),
                Err(_) => None,
            };
            if let Some(group) = pgid.and_then(Pid::from_raw).filter(|p| p.as_raw_pid() > 0) {
                let _ = kill_process_group(group, Signal::KILL);
            }
        }
        self.check_alive(); // the drive's own group was killed: it died with it
        panic::resume_unwind(Box::new(DriveExit(97)));
    }

    // -- forked drives --------------------------------------------------------

    /// Python's `os.fork()` drive: runs `f` with the world and the drive's process group id
    /// (Python's `os.getpid()` in the child) as a forked child that is its own group would
    /// run, and puts the world's state back afterwards, as a forked child's changes never
    /// reach its parent. Every process the world starts meanwhile joins that group, so a
    /// LEADHAND_CRASH_KILL_PGID kill takes them down with the drive, and the drive dies with
    /// its group (see the module docs). Past `deadline` the group is killed and `hung` set.
    pub fn fork(&mut self, deadline: Duration, f: impl FnOnce(&mut FakeSupervisor, i32)) -> Forked {
        let saved = self.save();
        let mut leader = Command::new("sleep")
            .arg("86400")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap_or_else(|e| panic!("starting the drive's process group leader: {e}"));
        let pgid = leader.id() as i32;
        let start = start_time(pgid).expect("the drive's group leader runs");
        self.drive = Some(Drive { pgid, start });
        let hung = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel::<()>();
        let watchdog = {
            let hung = Arc::clone(&hung);
            thread::spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(deadline) {
                    hung.store(true, Ordering::SeqCst);
                    if let Some(group) = Pid::from_raw(pgid) {
                        let _ = kill_process_group(group, Signal::KILL);
                    }
                }
            })
        };
        let result = panic::catch_unwind(AssertUnwindSafe(|| f(self, pgid)));
        drop(stop);
        let _ = watchdog.join();
        // The leader is unreaped until here, so its pid (the group's id) was never reused.
        let killed = matches!(leader.try_wait(), Ok(Some(_)));
        let (code, error) = match result {
            Ok(()) => (0, None),
            Err(payload) => {
                if let Some(DriveExit(code)) = payload.downcast_ref::<DriveExit>() {
                    (*code, None)
                } else if payload.is::<DriveKilled>() || killed {
                    // Killed with its group: whatever it did after that never happened.
                    (-9, None)
                } else {
                    let message = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "(a panic without a message)".to_string());
                    (1, Some(message))
                }
            }
        };
        // The drive exits: its leader goes; the rest of its group stays, as a child's would.
        let _ = leader.kill();
        let _ = leader.wait();
        self.restore(saved);
        Forked {
            code,
            error,
            hung: hung.load(Ordering::SeqCst),
        }
    }

    /// True while in a forked drive.
    pub fn in_drive(&self) -> bool {
        self.drive.is_some()
    }

    /// In a forked drive whose group has been killed, ends the drive there (it died).
    fn check_alive(&self) {
        if let Some(d) = &self.drive
            && !same_process(d.pgid, Some(&d.start))
        {
            panic::resume_unwind(Box::new(DriveKilled));
        }
    }

    fn save(&self) -> Saved {
        Saved {
            env: self.env.clone(),
            offset: self.offset,
            run_dir: self.run_dir.clone(),
            steps: self.steps.clone(),
            rulings: self.rulings.clone(),
            parks: self.parks.clone(),
            diff_checks: self.diff_checks.clone(),
            started: self.started.clone(),
            run_started: self.run_started.clone(),
            proxies: self.proxies.clone(),
            launch_log: self.launch_log.clone(),
            popen_log: self.popen_log.clone(),
            cli_cwd: self.cli_cwd.clone(),
            ids: self.ids,
            messages: self.messages,
            drive: self.drive.clone(),
        }
    }

    fn restore(&mut self, s: Saved) {
        self.env = s.env;
        self.offset = s.offset;
        self.run_dir = s.run_dir;
        self.steps = s.steps;
        self.rulings = s.rulings;
        self.parks = s.parks;
        self.diff_checks = s.diff_checks;
        self.started = s.started;
        self.run_started = s.run_started;
        self.proxies = s.proxies;
        self.launch_log = s.launch_log;
        self.popen_log = s.popen_log;
        self.cli_cwd = s.cli_cwd;
        self.ids = s.ids;
        self.messages = s.messages;
        self.drive = s.drive;
    }

    fn next_id(&mut self) -> u64 {
        let n = self.ids;
        self.ids += 1;
        n
    }

    /// Appends `{"pid", "start", "argv"}` to `popen_log` when it is set.
    fn log_popen(&self, pid: u32, argv: &[String]) {
        if let Some(log) = &self.popen_log {
            let pid = pid as i32;
            let line = Json::object([
                ("pid", Json::from(pid)),
                ("start", start_time(pid).map_or(Json::Null, Json::from)),
                ("argv", Json::from(argv)),
            ]);
            append(log, format!("{}\n", json::dumps(&line)).as_bytes());
        }
    }

    /// Runs a command for the world: in the drive's group during a drive, else as
    /// `run_argv` would; logged to `popen_log`.
    #[track_caller]
    fn exec(
        &self,
        argv: &[&OsStr],
        env: &Env,
        cwd: &Path,
        stdin: Option<&str>,
        timeout: Duration,
    ) -> Out {
        self.check_alive();
        let words: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let log = |pid: u32| self.log_popen(pid, &words);
        let group = match &self.drive {
            Some(d) => Group::Join(d.pgid),
            None => Spawn::default_for(env).group,
        };
        let spawn = Spawn {
            group,
            on_spawn: Some(&log),
        };
        let out = panic::catch_unwind(AssertUnwindSafe(|| {
            run_argv_spawn(argv, env, cwd, stdin, timeout, spawn)
        }));
        self.check_alive();
        out.unwrap_or_else(|p| panic::resume_unwind(p))
    }

    /// `git` for the world (gitfixture's run_git): in the drive's group during a drive.
    #[track_caller]
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        self.check_alive();
        let mut words = vec!["git".to_string()];
        words.extend(IDENTITY.iter().chain(args).map(|s| s.to_string()));
        let log = |pid: u32| self.log_popen(pid, &words);
        let group = match &self.drive {
            Some(d) => Group::Join(d.pgid),
            None => Group::Own,
        };
        let spawn = Spawn {
            group,
            on_spawn: Some(&log),
        };
        let out = panic::catch_unwind(AssertUnwindSafe(|| git::run_git_spawn(cwd, args, spawn)));
        self.check_alive();
        out.unwrap_or_else(|p| panic::resume_unwind(p))
    }

    // -- the CLI --------------------------------------------------------------

    /// The world's environment with the clock offset (`_env`).
    pub fn clock_env(&self) -> Env {
        self.env
            .with("LEADHAND_TEST_CLOCK_OFFSET", json::float_repr(self.offset))
    }

    /// The run directory as an argument (`None` before `start`, as Python's `str(None)`).
    fn run_dir_arg(&self) -> OsString {
        match &self.run_dir {
            Some(dir) => dir.clone().into_os_string(),
            None => "None".into(),
        }
    }

    /// An engine step: the binary with `args`, in the world's environment, from `cli_cwd`
    /// (default `tmp`) (`_cli`).
    #[track_caller]
    pub fn engine_cli(&self, args: &[&OsStr]) -> Out {
        let bin = bin();
        let mut argv = vec![bin.as_os_str()];
        argv.extend_from_slice(args);
        let cwd = self.cli_cwd.as_deref().unwrap_or(&self.tmp);
        self.exec(&argv, &self.clock_env(), cwd, None, TIMEOUT)
    }

    /// `record <run> <kind> <flags>`; its JSON (`_record`).
    #[track_caller]
    pub fn record(&self, kind: &str, flags: &[&OsStr]) -> Json {
        let run_dir = self.run_dir_arg();
        let mut args = vec![OsStr::new("record"), &run_dir, OsStr::new(kind)];
        args.extend_from_slice(flags);
        self.engine_cli(&args).ok().json()
    }

    /// The binary with `args` from `tmp`, in the world's environment with no crash point.
    #[track_caller]
    pub fn cli(&self, args: &[&OsStr]) -> Out {
        self.cli_with(args, CliOpts::default())
    }

    /// `cli` with `o.env` over the world's environment, then the crash point `o.crash`
    /// armed (none when None), and `o.stdin`.
    #[track_caller]
    pub fn cli_with(&self, args: &[&OsStr], o: CliOpts) -> Out {
        let mut full = self.clock_env();
        for (k, v) in &o.env {
            full.set(k, v);
        }
        full.remove("LEADHAND_CRASH_AT");
        if let Some(crash) = o.crash.filter(|c| !c.is_empty()) {
            full.set("LEADHAND_CRASH_AT", crash);
        }
        let bin = bin();
        let mut argv = vec![bin.as_os_str()];
        argv.extend_from_slice(args);
        self.exec(&argv, &full, &self.tmp, o.stdin, TIMEOUT)
    }

    // -- cleanup --------------------------------------------------------------

    /// Kills the proxies this world started, then every process a worker folder records
    /// that is still the process it names (`recorded_processes`): runners first, so none
    /// starts another Codex, then the rest. Then the fake Codex processes, tool children and
    /// escaped writers its log names (a runner may not have recorded them yet), each only if
    /// it started no later than the log line naming it. Waits (briefly) for them all to be
    /// gone. Dropping the world does this too.
    pub fn kill_leftovers(&mut self) {
        for p in &self.proxies {
            if p.poll().is_none() {
                p.kill();
            }
            let _ = p.communicate(Duration::from_secs(10));
        }
        if let Some(run_dir) = &self.run_dir {
            let log = PathBuf::from(self.env.get("FAKE_CODEX_LOG").expect("FAKE_CODEX_LOG"));
            kill_run_leftovers(run_dir, &log);
        }
    }
}

impl Drop for FakeSupervisor {
    fn drop(&mut self) {
        let result = panic::catch_unwind(AssertUnwindSafe(|| self.kill_leftovers()));
        if let Err(payload) = result
            && !thread::panicking()
        {
            panic::resume_unwind(payload);
        }
    }
}

/// True when `rec` is a `{pid, start}` record of a process that is still alive with that
/// start time (Python's `same_process(rec.get("pid"), rec.get("start"))`).
pub fn recorded_alive(rec: &Json) -> bool {
    let pid = rec
        .get("pid")
        .and_then(Json::as_i64)
        .and_then(|p| i32::try_from(p).ok());
    let start = rec.get("start").and_then(Json::as_str);
    match pid {
        Some(pid) if pid != 0 => same_process(pid, start),
        _ => false,
    }
}

/// `kill_leftovers` without a world: every live process `run_dir`'s worker folders record
/// (runners first), then the Codex processes, tool children and escaped writers `codex_log`
/// names. For a run whose world is gone, such as a crash-matrix worker killed at its
/// deadline.
pub fn kill_run_leftovers(run_dir: &Path, codex_log: &Path) {
    let mut killed: Vec<(i32, String)> = Vec::new();
    let mut kill = |pid: i32, start: String| {
        if let Some(p) = Pid::from_raw(pid).filter(|p| p.as_raw_pid() > 0)
            && kill_process(p, Signal::KILL).is_ok()
        {
            killed.push((pid, start));
        }
    };
    for runners_only in [true, false] {
        for rec in recorded_processes(run_dir, runners_only) {
            if recorded_alive(&rec) {
                let pid = rec["pid"].as_i64().unwrap() as i32;
                kill(pid, text(&rec["start"]).to_string());
            }
        }
    }
    let lines = if codex_log.exists() {
        read_jsonl(codex_log)
    } else {
        Vec::new()
    };
    for line in lines {
        let pick = |key: &str| line.get(key).filter(|v| truthy(v)).cloned();
        let mut pid = pick("child_pid")
            .or_else(|| pick("escaped_pid"))
            .unwrap_or(Json::Null);
        if line.get("event").is_some_and(|e| e == "start") {
            pid = line["pid"].clone();
        }
        if !truthy(&pid) {
            continue;
        }
        let pid = pid
            .as_i64()
            .and_then(|p| i32::try_from(p).ok())
            .unwrap_or_else(|| panic!("a pid that is not one: {line}"));
        let when = line["time"]
            .as_f64()
            .unwrap_or_else(|| panic!("a log line with no time: {line}"));
        if let Some(start) = started_by(pid, when) {
            kill(pid, start);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while killed.iter().any(|(p, s)| same_process(*p, Some(s))) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

/// The `{pid, start}` records in the run's worker folders and their superseded generations:
/// the runner (runner.json, or spawn.json before it), and unless `runners_only` Codex, its
/// recorded descendants (procs.json) and what a result says was stopped (result.json). A
/// record is a process only while `recorded_alive` holds.
pub fn recorded_processes(run_dir: &Path, runners_only: bool) -> Vec<Json> {
    fn read(path: &Path) -> Json {
        match fs::read(path).ok().map(|b| json::try_parse(&b)) {
            Some(Ok(data @ Json::Obj(_))) => data,
            _ => Json::object([]),
        }
    }
    fn entries(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    }
    let workers = entries(&run_dir.join("workers"));
    let superseded: Vec<PathBuf> = workers
        .iter()
        .flat_map(|w| entries(&w.join("superseded")))
        .collect();
    let mut out: Vec<Json> = Vec::new();
    for folder in workers.iter().chain(&superseded) {
        let runner = read(&folder.join("runner.json"));
        out.extend(runner.get("runner").cloned());
        out.push(read(&folder.join("spawn.json")));
        if !runners_only {
            out.extend(runner.get("codex").cloned());
            let list = |file: &str, key: &str| {
                read(&folder.join(file))
                    .get(key)
                    .and_then(Json::as_array)
                    .map(<[Json]>::to_vec)
                    .unwrap_or_default()
            };
            out.extend(list("procs.json", "descendants"));
            out.extend(list("result.json", "stopped"));
        }
    }
    out.into_iter()
        .filter(|r| matches!(r, Json::Obj(_)) && r.get("pid").is_some_and(truthy))
        .collect()
}
