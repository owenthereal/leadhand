//! The live end-to-end run, with the real `claude` and `codex`: the Rust form of the Python
//! end-to-end driver it replaced. One test runs everything unattended:
//!
//! 1. Step 1 (complete run, unless LEADHAND_LIVE_SKIP_PUSH_PROBE): the push probe
//!    (`push_probe.rs`) into the same output folder.
//! 2. The fixture (`fixture.rs`) in a temp folder whose paths contain spaces; the operator's
//!    leadhand user config is replaced by an empty file (LEADHAND_CONFIG).
//! 3. `leadhand check`, `leadhand route` for both cards and `leadhand start`, all run from an
//!    unrelated directory.
//! 4. The supervisor is started through `leadhand run <deck> --plugin-dir <this repo>` from
//!    the unrelated directory, so the run lock, holder.json and the launch environment are
//!    the real ones.
//!    - headless (the default): with the hidden `--print` option (`claude -p --output-format
//!      stream-json --verbose`); the stream is saved with the time each line arrived. The
//!      launcher leads a session of its own (the test binary re-executed, `exec_if_selected`).
//!    - tmux: interactive in a private tmux server; first-run dialogs are answered (the trust
//!      dialog by moving to "Yes, I trust this folder" and checking the highlighted line
//!      before Enter, never a bare Enter on "No, exit").
//!
//!    If the session exits while the run has no outcome, the driver records what was still
//!    running at that moment and relaunches with the exact `leadhand launch-line` command,
//!    in a loop, until the run has an outcome or the budget (45 minutes) ends.
//! 5. While polling (every 5 s) it samples status.json's headline.
//! 6. After the run it collects the run directory, the stream(s), the supervisor and worker
//!    transcripts, the Codex rollouts and Git state, and performs every check plus the open
//!    checks (print-mode waiting, sandbox exclusion of the CLI, the heredoc record command,
//!    Agent model names, the run skill loading through the Skill tool).
//!
//! The checks run on recorded inputs (`Evidence`, `run_checks`): the session's transcript
//! files, the run directory, the sessions and status samples, and Git answers through `Repo`.
//!
//! The CLI path (ruling S3-R14): the driver starts LEADHAND_BIN, but checks the supervisor's
//! Bash calls against the CLI path the launch reported (the first message's "leadhand CLI:"
//! path, else `launch-line`'s first word). When LEADHAND_BIN is not that path (a shim), it
//! adds a check that the launch's path is the resolved platform binary.
//!
//! The interrupted run: the fixture's card 2 gets more work (so its Codex worker runs long
//! enough), and the `Interrupter` poll hook (polling every 0.5 s while a trigger is pending)
//! SIGKILLs card 2's Codex process once it is running, then, when card 2's next attempt runs,
//! tries the exact launch-line relaunch under the live supervisor (it must be refused, exit 3)
//! and SIGKILLs the supervisor (holder.json's pid; every kill is pid and start-time checked).
//! The relaunch loop then relaunches with launch-line, retrying refusals, and polls to the
//! end; the interrupt checks are added. It never runs the push probe.
//!
//! Writes `<out>/e2e-raw.md` (a verdict per check, with `key: value` facts) and
//! `<out>/e2e-artifacts/`. Cleans up every process it started (checked by pid and start time)
//! and removes the temp folder unless LEADHAND_LIVE_KEEP.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use crate::common::json::{self, Json};
use crate::common::launch_world::now;
use crate::common::plugin::is_word;
use crate::common::supervisor::{splitlines, truthy};
use crate::common::{self, py_str};
use crate::fixture::{self, Fixture};
use crate::push_probe::{
    self, Facts, Ran, RunError, SessionFiles, absolute, catch, copy2, copytree, live_env, member,
    operator_interrupted, or_none, parse_ts, py_text, read_json, read_jsonl, real, round_to, short,
    short_text, show, sleep_s, utc_now,
};

const BUDGET_MINUTES: u64 = 45;
const POLL_S: f64 = 5.0;
/// A status sample is kept when the headline changes, or this often.
const STATUS_KEEP_EVERY_S: f64 = 60.0;
/// The session may still write the report after the outcome event.
const AFTER_OUTCOME_GRACE_S: f64 = 180.0;
const STOP_GRACE_S: f64 = 30.0;
/// Consecutive sessions that added no event: stop relaunching.
const NO_PROGRESS_LIMIT: u32 = 3;
const MAX_SESSIONS: usize = 15;
const CMD_TIMEOUT: Duration = Duration::from_secs(120);
/// A quiet gap after a turn's result this long counts as "waited".
const PRINT_WAIT_NOTE_S: f64 = 20.0;
const PANE_SAVE_EVERY_S: f64 = 60.0;
/// The dialogs answered in tmux mode: (the Python pattern, recorded as is; the text it
/// matches).
const DIALOG_PATTERNS: [(&str, &str); 8] = [
    ("trust the files", "trust the files"),
    ("Yes, I trust", "Yes, I trust"),
    ("Yes, proceed", "Yes, proceed"),
    ("Choose the text style", "Choose the text style"),
    ("Press Enter to continue", "Press Enter to continue"),
    ("Enter to confirm", "Enter to confirm"),
    ("Enable auto mode", "Enable auto mode"),
    ("Do you want to proceed\\?", "Do you want to proceed?"),
];
const MENU_WANTED: [&str; 3] = ["Yes", "Proceed", "Continue"];
const MENU_FORBIDDEN: [&str; 2] = ["No", "exit"];
const MENU_MAX_MOVES: usize = 6;
const DIALOG_REPEAT_LIMIT: usize = 3;
/// `leadhand run`: someone else owns the run.
const EXIT_REFUSED: i64 = 3;
const REFUSED_RETRY_S: f64 = 20.0;
/// Security too when the run requires it.
const REQUIRED_GATES: [&str; 3] = ["final_review", "evidence", "verify"];
const CARD_PINS: [(i64, (&str, &str, &str)); 2] = [
    (1, ("claude", "sonnet", "low")),
    (2, ("codex", "gpt-6-luna", "medium")),
];

// The interrupted run.
/// Trigger polling while an interrupt is pending.
const INTERRUPT_POLL_S: f64 = 0.5;
/// Kill card 2's Codex at its first completed item, or this long after it started.
const KILL_AFTER_FIRST_STEP_S: f64 = 15.0;
const PREKILL_RELAUNCH_WAIT_S: f64 = 20.0;
/// How long after the supervisor kill polling stays fast (so a short-lived reconciliation
/// headline is sampled).
const FAST_AFTER_KILL_S: f64 = 300.0;

const CHECK_DOCS: [(&str, &str); 17] = [
    (
        "outcome complete",
        "the last `outcome` event in events.jsonl has outcome `complete`",
    ),
    (
        "run branch holds both cards' commits",
        "each card's `accepted` sha is an ancestor of refs/heads/<run branch> (git merge-base \
         --is-ancestor); also lists base..branch commits and runs the fixture tests in the workspace",
    ),
    (
        "report.md line 3",
        "line 3 of <run>/report.md is exactly `Outcome: complete`, and the run wrote report.md \
         itself (FAIL when only the driver's fallback `leadhand report` produced it)",
    ),
    (
        "every gate examined the run head",
        "final_review, evidence and verify (and security when the run ran it) each have a last \
         `gate_result` that passed with examined_sha equal to `git rev-parse refs/heads/<run \
         branch>`; earlier results on other shas are listed",
    ),
    (
        "claude implementer attempt on sonnet low",
        "attempts.jsonl has card 1, role implementer, vendor claude, model sonnet, effort low",
    ),
    (
        "codex implementer attempt on gpt-6-luna medium",
        "attempts.jsonl has card 2, role implementer, vendor codex, model gpt-6-luna, effort medium",
    ),
    (
        "supervisor and proxy usage recorded",
        "charges.jsonl has role `supervisor` charges; attempts.jsonl (or charges.jsonl) has role \
         `proxy`",
    ),
    (
        "worker_started before result_recorded",
        "every dispatch with a `result_recorded` event has a `worker_started` event with a smaller seq",
    ),
    (
        "workers ran in the workspace",
        "every Claude worker transcript record's cwd, every Codex request.json cwd and rollout \
         session_meta cwd resolve to run.json's workspace (or the evidence base worktree \
         <repo>/.worktrees/<run dir name>--base)",
    ),
    (
        "headline moved through implementing and reviewing",
        "status.json sampled every 5 s; some headline contains `Implementing` and some contains \
         `Reviewing`",
    ),
    (
        "no publishing command ran",
        "no Bash command in the supervisor or worker transcripts and no Codex command_execution \
         matches git+push / gh pr; the fixture's bare origin refs are unchanged",
    ),
    (
        "elapsed time",
        "run_started -> outcome event timestamps; driver wall time; per-session durations",
    ),
    (
        "open check: print mode outlives long workers",
        "per session: exit time vs worker_started/result_recorded; dispatches open at exit; Codex \
         results written after exit; quiet gaps after each stream `result`",
    ),
    (
        "open check: CLI calls ran unsandboxed (exclusion matched)",
        "every Bash call containing the CLI path: its tool_result record's `sandboxed` flag is \
         false, AND at least one non-CLI Bash call (control, e.g. `printenv LEADHAND_RUN_ID`) ran \
         sandboxed; without that control the verdict is INCONCLUSIVE",
    ),
    (
        "open check: heredoc record command matched the exclusion",
        "the `record ... --reply - <<'LEADHAND_REPLY'` Bash calls: `sandboxed` is false (same \
         control as above)",
    ),
    (
        "open check: Agent tool accepted the route model names",
        "every Agent call (subagent_type, model, effort) got a non-error result; the worker \
         transcript's message.model is listed",
    ),
    (
        "open check: leadhand:run loaded through the Skill tool",
        "a Skill tool call naming leadhand:run without error, and the first paragraph of \
         skills/run/SKILL.md appears in the session transcript",
    ),
];

const INTERRUPT_CHECK_DOCS: [(&str, &str); 9] = [
    (
        "interrupt: each card accepted exactly once",
        "events.jsonl has exactly one `accepted` event for card 1 and card 2",
    ),
    (
        "interrupt: no duplicate commits on the run branch",
        "non-merge commits base..run branch: no two share a `git patch-id --stable`, and no two \
         share subject + tree",
    ),
    (
        "interrupt: relaunch refused while the old supervisor lived, succeeded after",
        "the launch-line relaunch tried under the live supervisor exited 3 and added no event; a \
         session started after the SIGKILL was not refused and made progress (refusals after the \
         kill, with lsof lock holders, are listed)",
    ),
    (
        "interrupt: the runner that outlived the supervisor recorded its result, not restarted",
        "card 2's next (Codex) dispatch: result.json present and written after the kill, \
         runner.json generation 1, no superseded/, same runner pid as at the kill, a \
         result_recorded event",
    ),
    (
        "interrupt: the killed Codex attempt is recorded (usage recovered or unknown, possibly_incomplete)",
        "attempts.jsonl has the killed dispatch's Codex attempt with usage_known + usage_source, \
         or usage_known false; its Codex charges carry possibly_incomplete",
    ),
    (
        "interrupt: charged Codex usage per thread equals the rollout total",
        "per Codex thread: the sum of charges.jsonl usage (input, cached, output) equals the \
         rollout's token_usage_record sum over distinct response_id (input excludes cached); a \
         thread whose rollout has no token_usage_record passes when all its charges are \
         known=false and possibly_incomplete",
    ),
    (
        "interrupt: the report's Usage counts unknown attempts",
        "report.md's `Attempts with unknown usage: N` equals the attempts with usage_known false",
    ),
    (
        "interrupt: a reconciliation headline appeared",
        "a status sample headline contains `Resumed at`",
    ),
    (
        "interrupt: timings and estimated cost",
        "interrupter facts (kill times, pids, relaunch attempts), kill -> successful relaunch, \
         the report's cost line, per-session stream cost",
    ),
];

/// The check added when LEADHAND_BIN is not the resolved platform binary (a shim, or the
/// testing build), or the launch reported no CLI path (S3-R14).
const LAUNCH_CLI_DOC: (&str, &str) = (
    "launch CLI path is the resolved platform binary",
    "added when LEADHAND_BIN is not the resolved platform binary or the launch reported no CLI \
     path: the path the supervisor's Bash calls are checked against names the resolved \
     platform binary, <root>/bin/<target>/leadhand when it exists, else \
     <root>/target/release/leadhand",
);

/// The check added when the launch reported no CLI path, so the Bash calls were checked
/// against LEADHAND_BIN instead (S3-R14).
const LAUNCH_CLI_MISSING_DOC: (&str, &str) = (
    "launch reported its CLI path",
    "added only when it fails: neither a first message (\"leadhand CLI: <path>\") in the \
     supervisor transcripts nor a launch-line output named the CLI path the launch used, so \
     the CLI-call checks fell back to LEADHAND_BIN",
);

/// Whether the shim check (`LAUNCH_CLI_DOC`) runs: LEADHAND_BIN (`bin`) does not name the
/// resolved platform binary under `root`, or the launch's CLI path was not found and fell
/// back to LEADHAND_BIN. The dry run's plan and the checks both decide by this (the plan,
/// before any launch, with `fell_back` false).
pub fn adds_launch_cli_check(bin: &Path, root: &Path, fell_back: bool) -> bool {
    fell_back || !common::names_file(&platform_binary(root), bin.as_os_str())
}

/// The first message's text around the CLI path (`src/launch/argv.rs`'s `first_message`).
const FIRST_MESSAGE_CLI: &str = ". leadhand CLI: ";
const FIRST_MESSAGE_END: &str = ". First invoke the Skill tool with leadhand:run, then follow it.";

// --------------------------------------------------------------------------
// Options and the entry point

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Headless,
    Tmux,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Headless => "headless",
            Mode::Tmux => "tmux",
        }
    }
}

/// The Python driver's command-line options, from the environment (see `tests/live.rs`).
#[derive(Clone, Debug)]
pub struct Options {
    pub out: PathBuf,
    pub mode: Mode,
    pub budget_minutes: u64,
    pub skip_push_probe: bool,
    pub keep: bool,
    pub dry_run: bool,
    pub interrupt: bool,
}

fn env_text(name: &str) -> Option<String> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
}

/// A yes/no setting: unset, empty, `0`, `false` or `no` is no; `1`, `true` or `yes` is yes.
fn flag(name: &str) -> bool {
    match env_text(name).as_deref() {
        None | Some("0" | "false" | "no") => false,
        Some("1" | "true" | "yes") => true,
        Some(other) => panic!("{name} must be 1 or 0 (or true/false, yes/no), not {other:?}"),
    }
}

impl Options {
    /// The options for the complete run (`interrupt` false) or the interrupted one. Panics at
    /// once when LEADHAND_LIVE_OUT is unset, or a setting is invalid (the Python's usage
    /// error).
    pub fn from_env(interrupt: bool) -> Options {
        let Some(base) = std::env::var_os("LEADHAND_LIVE_OUT").filter(|v| !v.is_empty()) else {
            panic!(
                "LEADHAND_LIVE_OUT is unset: set it to the folder for the live runs' results \
                 (this run writes under $LEADHAND_LIVE_OUT/{}/)",
                if interrupt { "interrupt" } else { "complete" }
            );
        };
        let out = absolute(Path::new(&base)).join(if interrupt { "interrupt" } else { "complete" });
        let mode = match env_text("LEADHAND_LIVE_MODE").as_deref() {
            None | Some("headless") => Mode::Headless,
            Some("tmux") => Mode::Tmux,
            Some(other) => panic!("LEADHAND_LIVE_MODE must be headless or tmux, not {other:?}"),
        };
        let budget_minutes = match env_text("LEADHAND_LIVE_BUDGET_MINUTES") {
            None => BUDGET_MINUTES,
            Some(text) => match text.parse::<u64>() {
                Ok(n) if n >= 1 => n,
                _ => panic!("LEADHAND_LIVE_BUDGET_MINUTES must be at least 1, not {text:?}"),
            },
        };
        Options {
            out,
            mode,
            budget_minutes,
            skip_push_probe: flag("LEADHAND_LIVE_SKIP_PUSH_PROBE") || interrupt,
            keep: flag("LEADHAND_LIVE_KEEP"),
            dry_run: flag("LEADHAND_LIVE_DRY_RUN"),
            interrupt,
        }
    }
}

/// A live test: `test` is its libtest name, `interrupt` picks the interrupted run. In a dry
/// run it prints the plan and starts nothing; otherwise it refuses inside Claude Code, runs,
/// and fails when a check (or the push probe) failed, after writing everything.
pub fn main(test: &'static str, interrupt: bool) {
    exec_if_selected();
    let opts = Options::from_env(interrupt);
    let mut d = Driver::new(opts, test);
    if d.opts.dry_run {
        d.plan();
        return;
    }
    push_probe::refuse_inside_claude_code();
    let failed = d.run();
    assert!(
        failed.is_empty(),
        "live run: {} failed (see {}):\n{}",
        failed.len(),
        d.raw.display(),
        failed.join("\n")
    );
}

// --------------------------------------------------------------------------
// New sessions, without `fork`: the test binary re-executed (ruling S3-R8)

/// Set on a re-executed test binary that is to become a new session's leader and exec an
/// argv (JSON list of WTF-8 strings). It starts with LEADHAND_, but is removed before the
/// exec, so leadhand never sees it.
const EXEC_VAR: &str = "LEADHAND_LIVE_SESSION_EXEC";
/// Printed by the re-executed child just before the exec: everything before it on stdout is
/// the test harness's own output.
const EXEC_SENTINEL: &[u8] = b"\x01leadhand-live: exec\x01\n";

/// In a child `spawn_new_session` started: calls `setsid` and execs the argv it was given
/// (Python's `start_new_session=True`). Returns at once in an ordinary run.
pub fn exec_if_selected() {
    let Some(spec) = std::env::var_os(EXEC_VAR) else {
        return;
    };
    let v = json::parse(spec.as_bytes());
    let argv: Vec<OsString> = v
        .as_array()
        .unwrap_or_else(|| panic!("{EXEC_VAR}: not a list"))
        .iter()
        .map(|a| a.as_os().unwrap_or_else(|| panic!("{EXEC_VAR}: {a}")))
        .collect();
    if let Err(e) = rustix::process::setsid() {
        eprintln!("leadhand live: setsid failed: {e}");
        std::process::exit(126);
    }
    {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(EXEC_SENTINEL);
        let _ = out.flush();
    }
    let err = Command::new(&argv[0])
        .args(&argv[1..])
        .env_remove(EXEC_VAR)
        .exec();
    eprintln!(
        "leadhand live: could not start {}: {err}",
        common::shlex_join(&argv)
    );
    std::process::exit(127);
}

/// Starts `argv` from `cwd` with exactly `env`, stdin from /dev/null and stderr to `stderr`,
/// as the leader of a new session (the test binary `test` re-executed, which calls `setsid`
/// and execs it). Its stdout is the returned pipe, positioned after the re-executed binary's
/// own output.
pub fn spawn_new_session(
    test: &str,
    argv: &[OsString],
    cwd: &Path,
    env: &[(OsString, OsString)],
    stderr: File,
) -> Result<(Child, BufReader<ChildStdout>), String> {
    let exe = std::env::current_exe().map_err(|e| format!("the test binary's path: {e}"))?;
    let spec = Json::Arr(
        argv.iter()
            .map(|a| Json::Str(json::from_os(a.as_bytes())))
            .collect(),
    );
    let mut child = Command::new(exe)
        .args([
            "--exact",
            test,
            "--include-ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned())
        .env(EXEC_VAR, json::dumps(&spec))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("starting {}: {e}", common::shlex_join(argv)))?;
    let mut out = BufReader::new(child.stdout.take().expect("a piped stdout"));
    // Skip the re-executed binary's own lines, up to the sentinel.
    let mut line = Vec::new();
    loop {
        line.clear();
        match out.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line.ends_with(EXEC_SENTINEL) => break,
            Ok(_) => {}
        }
    }
    Ok((child, out))
}

/// The child's exit code once it has exited within `secs` seconds (polling); None past it.
fn wait_child(child: &mut Child, secs: f64) -> Option<i32> {
    let end = now() + secs;
    loop {
        if let Some(status) = child.try_wait().expect("polling a child") {
            return Some(common::code_of(status));
        }
        if now() >= end {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn pid_of(pid: i32) -> Option<Pid> {
    Pid::from_raw(pid)
}

/// Sends `sig` to `pid`; whether it was sent.
fn send(pid: i32, sig: Signal) -> bool {
    pid_of(pid).is_some_and(|p| kill_process(p, sig).is_ok())
}

/// Sends `sig` to the process group `pgid`; whether it was sent.
fn send_group(pgid: i32, sig: Signal) -> bool {
    pid_of(pgid).is_some_and(|p| kill_process_group(p, sig).is_ok())
}

/// True when `pid` is alive (not a zombie) and still has start time `start`.
fn same(pid: &Json, start: &Json) -> bool {
    match (pid.as_i64(), start.as_str()) {
        (Some(p), Some(s)) if p > 0 => common::same_process(p as i32, Some(s)),
        _ => false,
    }
}

// --------------------------------------------------------------------------
// Small Python semantics

/// `e.get(k)` as a string, when it is one.
fn s<'a>(e: &'a Json, k: &str) -> Option<&'a str> {
    e.get(k).and_then(Json::as_str)
}

fn kind(e: &Json) -> &str {
    s(e, "kind").unwrap_or("")
}

/// `e.get(k) == v` for a number (Python's `==`: `1 == 1.0 == True`).
fn eq_num(v: &Json, n: i64) -> bool {
    json::py_eq(v, &Json::from(n))
}

/// `e.get("seq") or 0`, as a number.
fn num_or0(v: &Json) -> f64 {
    match v {
        Json::Num(t) => t.parse().unwrap_or(0.0),
        Json::Bool(true) => 1.0,
        _ => 0.0,
    }
}

/// `int(v)` (Python's): a number truncated, a bool as 0 or 1, a string of digits parsed;
/// panics on anything else (Python's TypeError or ValueError, which fails the check).
fn py_int(v: &Json) -> i64 {
    match v {
        Json::Num(t) => t
            .parse::<i64>()
            .ok()
            .or_else(|| t.parse::<f64>().ok().map(|f| f.trunc() as i64))
            .unwrap_or_else(|| panic!("int() of {t}")),
        Json::Bool(b) => i64::from(*b),
        Json::Str(_) => {
            let t = py_text(Some(v));
            t.trim()
                .parse()
                .unwrap_or_else(|_| panic!("invalid literal for int(): {t:?}"))
        }
        other => panic!("int() argument must be a number, not {other}"),
    }
}

/// `x or 0` then `int(x)`.
fn int_or0(v: &Json) -> i64 {
    if truthy(v) { py_int(v) } else { 0 }
}

/// A float as Python's `round(x)`: the nearest integer, ties to even.
fn round0(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// A list of strings as JSON.
fn strs(items: &[impl AsRef<str>]) -> Json {
    Json::Arr(items.iter().map(|s| Json::from(s.as_ref())).collect())
}

/// `None` or the value.
fn opt(v: Option<impl Into<Json>>) -> Json {
    v.map_or(Json::Null, Into::into)
}

/// `dict.fromkeys(items)` keys, in order.
fn distinct(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        if !out.contains(i) {
            out.push(i.clone());
        }
    }
    out
}

/// A dict key as `json.dumps` writes it: a string as is, a number's digits, `null`, `true`.
fn key_text(k: &Json) -> String {
    match k {
        Json::Str(_) => py_text(Some(k)),
        Json::Null => "null".to_string(),
        other => json::dumps(other),
    }
}

/// A Python dict keyed by JSON values (first place kept, last value wins).
#[derive(Default)]
struct Dict(Vec<(Json, Json)>);

impl Dict {
    fn set(&mut self, k: Json, v: Json) {
        match self.0.iter_mut().find(|(x, _)| json::py_eq(x, &k)) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k, v)),
        }
    }

    /// `dict.setdefault(k, v)`.
    fn set_default(&mut self, k: Json, v: Json) {
        if self.get(&k).is_none() {
            self.0.push((k, v));
        }
    }

    fn get(&self, k: &Json) -> Option<&Json> {
        self.0
            .iter()
            .find(|(x, _)| json::py_eq(x, k))
            .map(|(_, v)| v)
    }

    fn to_json(&self) -> Json {
        Json::Obj(
            self.0
                .iter()
                .map(|(k, v)| (key_text(k).into_bytes(), v.clone()))
                .collect(),
        )
    }
}

/// Python's `repr` of a JSON value read back (for the log lines that print one).
fn py_repr(v: &Json) -> String {
    match v {
        Json::Null => "None".to_string(),
        Json::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Json::Num(_) => json::dumps(v),
        Json::Str(_) => {
            let t = py_text(Some(v));
            if t.contains('\'') && !t.contains('"') {
                format!("\"{t}\"")
            } else {
                format!("'{}'", t.replace('\\', "\\\\").replace('\'', "\\'"))
            }
        }
        Json::Arr(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Json::Obj(_) => format!(
            "{{{}}}",
            v.keys()
                .iter()
                .map(|k| format!(
                    "{}: {}",
                    py_repr(&Json::from(k.as_str())),
                    py_repr(&v[k.as_str()])
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Every string inside a JSON value, joined by spaces (dict values, not keys).
fn strings(v: &Json) -> String {
    match v {
        Json::Str(_) => py_text(Some(v)),
        Json::Obj(members) => json::dict(members)
            .into_iter()
            .map(|(_, x)| strings(x))
            .collect::<Vec<_>>()
            .join(" "),
        Json::Arr(items) => items.iter().map(strings).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

/// `" ".join(text.split())`.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The offsets where `word` occurs in `text` with `\b` on both sides (ASCII case-folded when
/// `ci`).
fn word_at(text: &str, word: &str, ci: bool) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, _) in text.char_indices() {
        let Some(cand) = text.get(i..i + word.len()) else {
            continue;
        };
        let hit = if ci {
            cand.eq_ignore_ascii_case(word)
        } else {
            cand == word
        };
        if hit && boundary(text, i, word) {
            out.push(i);
        }
    }
    out
}

/// `\b` before offset `i` and after `word` there: a word character on exactly one side of
/// each edge (the text's ends count as non-word).
fn boundary(text: &str, i: usize, word: &str) -> bool {
    let before = text[..i].chars().next_back().is_some_and(is_word);
    let after = text[i + word.len()..].chars().next().is_some_and(is_word);
    let first = word.chars().next().is_some_and(is_word);
    let last = word.chars().next_back().is_some_and(is_word);
    before != first && after != last
}

/// `re.search(r"\b(w1|w2|…)\b", text)`, ASCII case-folded when `ci`.
pub(crate) fn has_word(text: &str, words: &[&str], ci: bool) -> bool {
    words.iter().any(|w| !word_at(text, w, ci).is_empty())
}

/// A publishing command: `\bgit\b[^\n;&|]*\bpush\b|\bgh\s+pr\b|\bgh\s+repo\s+create\b`.
pub(crate) fn publishes(c: &str) -> bool {
    let push = push_probe::PUSH;
    for i in word_at(c, "git", false) {
        let rest = &c[i + 3..];
        let span = &rest[..rest.find(['\n', ';', '&', '|']).unwrap_or(rest.len())];
        for (j, _) in span.match_indices(push) {
            if boundary(c, i + 3 + j, push) {
                return true;
            }
        }
    }
    let ws = |t: &str| t.len() - t.trim_start_matches(char::is_whitespace).len();
    for i in word_at(c, "gh", false) {
        let rest = &c[i + 2..];
        let n = ws(rest);
        if n == 0 {
            continue;
        }
        let rest = &rest[n..];
        let at = c.len() - rest.len();
        if rest.starts_with("pr") && boundary(c, at, "pr") {
            return true;
        }
        if let Some(r) = rest.strip_prefix("repo") {
            let m = ws(r);
            if m > 0 && r[m..].starts_with("create") && boundary(c, c.len() - r.len() + m, "create")
            {
                return true;
            }
        }
    }
    false
}

/// `\\?"action\\?":\s*\\?"done\\?"`: a `leadhand next` result whose action is `done`.
pub(crate) fn engine_done(text: &str) -> bool {
    for (i, _) in text.match_indices("\"action") {
        let r = &text[i + 7..];
        let r = r.strip_prefix('\\').unwrap_or(r);
        let Some(r) = r.strip_prefix("\":") else {
            continue;
        };
        let r = r.trim_start_matches(char::is_whitespace);
        let r = r.strip_prefix('\\').unwrap_or(r);
        let Some(r) = r.strip_prefix("\"done") else {
            continue;
        };
        let r = r.strip_prefix('\\').unwrap_or(r);
        if r.starts_with('"') {
            return true;
        }
    }
    false
}

/// `re.search(prefix + r"\s*(\d+)", text)`: the whole match and the number.
pub(crate) fn number_after(text: &str, prefix: &str) -> Option<(String, i64)> {
    for (i, _) in text.match_indices(prefix) {
        let r = &text[i + prefix.len()..];
        let r2 = r.trim_start_matches(char::is_whitespace);
        let digits: String = r2.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            let end = i + prefix.len() + (r.len() - r2.len()) + digits.len();
            return Some((text[i..end].to_string(), digits.parse().ok()?));
        }
    }
    None
}

/// `re.search(r"agentId:\s*([A-Za-z0-9]+)", text)`'s group.
pub(crate) fn agent_id(text: &str) -> Option<String> {
    for (i, _) in text.match_indices("agentId:") {
        let r = text[i + 8..].trim_start_matches(char::is_whitespace);
        let id: String = r.chars().take_while(char::is_ascii_alphanumeric).collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

/// The last line of an error (the Python's `traceback.format_exc().splitlines()[-1]`).
fn last_line(e: &str) -> String {
    splitlines(e)
        .last()
        .map_or(String::new(), |l| l.to_string())
}

// --------------------------------------------------------------------------
// Process records (runner.json)

/// runner.json's nested process records: {"runner": {pid, pgid, start}, "codex": {...}}.
fn runner_processes(runner_json: &Path) -> Facts {
    let rj = read_json(runner_json);
    let mut out = Facts::default();
    for role in ["runner", "codex"] {
        if let Some(rec) = rj.get(role)
            && rec.as_object().is_some()
            && matches!(rec.get("pid"), Some(Json::Num(_)))
            && rec.get("pid").and_then(Json::as_i64).is_some()
        {
            out.set(role, rec.clone());
        }
    }
    out
}

/// Signals the recorded processes whose pid and start time still match; what was signalled.
fn kill_runner_processes(runner_json: &Path, sig: Signal, roles: &[&str]) -> Vec<(String, i64)> {
    let recs = runner_processes(runner_json);
    let mut done = Vec::new();
    for role in roles {
        let Some(rec) = recs.get(role) else {
            continue;
        };
        if rec.get("start").and_then(Json::as_str).is_none() {
            continue;
        }
        if same(&rec["pid"], &rec["start"]) {
            let pid = rec["pid"].as_i64().unwrap();
            if send(pid as i32, sig) {
                done.push((role.to_string(), pid));
            }
        }
    }
    done
}

fn signalled_json(done: &[(String, i64)]) -> Json {
    Json::Arr(
        done.iter()
            .map(|(r, p)| Json::Arr(vec![Json::from(r.as_str()), Json::from(*p)]))
            .collect(),
    )
}

// --------------------------------------------------------------------------
// Transcripts

pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Json,
}

pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    /// The user record's `sandboxed` flag (true, false, or null when absent).
    pub sandboxed: Json,
}

fn tool_uses(records: &[Json]) -> Vec<ToolUse> {
    let mut out = Vec::new();
    for r in records {
        if s(r, "type") != Some("assistant") {
            continue;
        }
        let msg = member(r, "message");
        let Some(content) = member(msg, "content").as_array() else {
            continue;
        };
        for c in content {
            if c.as_object().is_some() && s(c, "type") == Some("tool_use") {
                let input = member(c, "input");
                out.push(ToolUse {
                    id: py_text(c.get("id")),
                    name: py_text(c.get("name")),
                    input: if truthy(input) {
                        input.clone()
                    } else {
                        Json::Obj(Vec::new())
                    },
                });
            }
        }
    }
    out
}

fn tool_results(records: &[Json]) -> BTreeMap<String, ToolResult> {
    let mut out = BTreeMap::new();
    for r in records {
        if s(r, "type") != Some("user") {
            continue;
        }
        let Some(content) = member(member(r, "message"), "content").as_array() else {
            continue;
        };
        for c in content {
            if c.as_object().is_none() || s(c, "type") != Some("tool_result") {
                continue;
            }
            let body = member(c, "content");
            let text = match body {
                Json::Arr(items) => items
                    .iter()
                    .filter(|x| x.as_object().is_some())
                    .map(|x| match x.get("text") {
                        None => String::new(),
                        Some(t @ Json::Str(_)) => py_text(Some(t)),
                        Some(Json::Null) => "None".to_string(),
                        Some(other) => json::dumps(other),
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                other => py_text(Some(other)),
            };
            out.insert(
                py_text(c.get("tool_use_id")),
                ToolResult {
                    text,
                    is_error: truthy(member(c, "is_error")),
                    sandboxed: member(r, "sandboxed").clone(),
                },
            );
        }
    }
    out
}

/// The supervisor transcripts of `session_ids` and their subagent transcripts.
pub fn transcripts(files: &SessionFiles, session_ids: &[String]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut mains, mut subs) = (Vec::new(), Vec::new());
    for sid in session_ids {
        let Some(p) = files.transcript(sid) else {
            continue;
        };
        let dir = p.parent().unwrap().join(sid).join("subagents");
        let mut found: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(OsStr::to_str)
                            .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".jsonl"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        mains.push(p);
        subs.extend(found);
    }
    (mains, subs)
}

/// The CLI path a first message names: "Run the leadhand deck in <run dir>. leadhand CLI:
/// <path>. First invoke the Skill tool with leadhand:run, then follow it."
pub fn first_message_cli(records: &[Json]) -> Option<String> {
    fn walk(v: &Json) -> Option<String> {
        match v {
            Json::Str(_) => {
                let t = py_text(Some(v));
                let at = t.find(FIRST_MESSAGE_CLI)?;
                let rest = &t[at + FIRST_MESSAGE_CLI.len()..];
                let end = rest.rfind(FIRST_MESSAGE_END)?;
                Some(rest[..end].to_string()).filter(|p| !p.is_empty())
            }
            Json::Arr(items) => items.iter().find_map(walk),
            Json::Obj(members) => members.iter().find_map(|(_, x)| walk(x)),
            _ => None,
        }
    }
    records.iter().find_map(walk)
}

/// The CLI path the launch reported (S3-R14), where it came from, and every path reported:
/// the first message's path in each supervisor transcript of `mains`, then the first word of
/// each `launch-line` output in `launch_lines`. LEADHAND_BIN (`bin`) when none is; the last
/// item says so (the fallback).
pub fn launch_cli(
    mains: &[PathBuf],
    launch_lines: &[String],
    bin: &Path,
) -> (String, String, Json, bool) {
    let mut seen: Vec<(String, String)> = Vec::new();
    for p in mains {
        if let Some(cli) = first_message_cli(&read_jsonl(p)) {
            seen.push((cli, format!("first message, {}", file_name(p))));
        }
    }
    for l in launch_lines {
        if let Some(cli) = common::shlex_split(l).and_then(|w| w.into_iter().next()) {
            seen.push((cli, "launch-line".to_string()));
        }
    }
    let all = Json::Arr(
        seen.iter()
            .map(|(p, from)| strs(&[p.as_str(), from.as_str()]))
            .collect(),
    );
    match seen.into_iter().next() {
        Some((p, from)) => (p, from, all, false),
        None => (
            bin.display().to_string(),
            "not reported by the launch; LEADHAND_BIN used".to_string(),
            all,
            true,
        ),
    }
}

/// The platform part of `bin/<target>/leadhand`, as `bin/leadhand` maps `uname`.
pub fn platform_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

/// The binary `bin/leadhand` runs: `<root>/bin/<target>/leadhand` when it exists, else
/// `<root>/target/release/leadhand`.
pub fn platform_binary(root: &Path) -> PathBuf {
    if let Some(t) = platform_target() {
        let p = root.join(format!("bin/{t}/leadhand"));
        if p.exists() {
            return p;
        }
    }
    root.join("target/release/leadhand")
}

// --------------------------------------------------------------------------
// Sessions

/// What the checks read about one supervisor session.
#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub n: usize,
    pub how: String,
    pub exit_code: Json,
    pub started_at: f64,
    pub started_utc: String,
    pub exited_at: Option<f64>,
    pub exited_utc: Option<String>,
    /// The last event seq when the session started.
    pub seq_at_start: i64,
    /// Headless: per stream line, its arrival time, type and a few fields.
    pub stream_meta: Vec<Json>,
    pub notes: Facts,
    /// Its artefact folder (`sessions/session-<n>`), holding stream.jsonl when headless.
    pub dir: PathBuf,
}

/// What was still running when a session exited, and what happened after.
pub fn analyze_exit(rec: &mut SessionRecord, events: &[Json], run_dir: &Path, now_t: f64) {
    let exit_t = rec.exited_at.unwrap_or(now_t);
    let (mut started, mut results) = (Dict::default(), Dict::default());
    for e in events {
        if kind(e) == "worker_started" {
            started.set(member(e, "dispatch_id").clone(), e.clone());
        } else if kind(e) == "result_recorded" {
            results.set(member(e, "dispatch_id").clone(), e.clone());
        }
    }
    let ts = |e: &Json| parse_ts(e.get("ts")).unwrap_or(0.0);
    let mut open_at_exit = Vec::new();
    for (did, e) in &started.0 {
        let t = ts(e);
        let r = results.get(did);
        if t <= exit_t && r.is_none_or(|r| ts(r) > exit_t) {
            open_at_exit.push(did.clone());
        }
    }
    let mut runners_after = Vec::new();
    for d in sorted_entries(&run_dir.join("workers")) {
        let res = d.join("result.json");
        if let Ok(m) = fs::metadata(&res)
            && mtime_s(&m) > exit_t
        {
            runners_after.push(Json::from(file_name(&d)));
        }
    }
    let outcome = events
        .iter()
        .any(|e| kind(e) == "outcome" && ts(e) <= exit_t);
    // Progress = events the session caused; the launcher's own `resumed` does not count.
    let added = events
        .iter()
        .filter(|e| {
            num_or0(member(e, "seq")) > rec.seq_at_start as f64
                && kind(e) != "resumed"
                && ts(e) <= exit_t + 2.0
        })
        .count();
    rec.notes.update(facts! {
        "events_added" => added,
        "dispatches_open_at_exit" => or_none(open_at_exit.clone()),
        "codex_results_written_after_exit" => or_none(runners_after),
        "run_had_outcome_at_exit" => outcome,
        "exited_while_workers_running" => !open_at_exit.is_empty() && !outcome,
    });
    let meta = &rec.stream_meta;
    if meta.is_empty() {
        return;
    }
    let results_meta: Vec<&Json> = meta
        .iter()
        .filter(|m| s(m, "type") == Some("result"))
        .collect();
    let t_of = |m: &Json| member(m, "t").as_f64().unwrap_or(0.0);
    let mut gaps: Vec<Json> = Vec::new();
    let mut longest = Json::from(0);
    let mut longest_f = f64::NEG_INFINITY;
    for (i, m) in meta[..meta.len() - 1].iter().enumerate() {
        if s(m, "type") == Some("result") {
            let nxt = &meta[i + 1];
            let gap = round_to(t_of(nxt) - t_of(m), 1);
            if gap > longest_f {
                longest_f = gap;
                longest = Json::from(gap);
            }
            gaps.push(obj! {
                "after_result_index" => member(m, "result_index").clone(),
                "gap_s" => gap,
                "next" => format!("{}/{}", py_str(member(nxt, "type")), py_str(member(nxt, "subtype"))),
            });
        }
    }
    let max_index = results_meta
        .iter()
        .map(|m| {
            let v = member(m, "result_index");
            if truthy(v) { v.clone() } else { Json::from(0) }
        })
        .max_by(|a, b| num_or0(a).total_cmp(&num_or0(b)));
    let last_cost = results_meta
        .last()
        .map_or(Json::Null, |m| member(m, "total_cost_usd").clone());
    let mut ids: Vec<String> = meta
        .iter()
        .filter(|m| truthy(member(m, "session_id")))
        .map(|m| py_text(m.get("session_id")))
        .collect();
    ids.sort();
    ids.dedup();
    let last_t = t_of(meta.last().unwrap());
    let gaps = or_none(gaps);
    rec.notes.update(facts! {
        "stream_lines" => meta.len(),
        "result_records" => results_meta.len(),
        "max_result_index" => max_index.unwrap_or(Json::Null),
        "task_notifications" => meta.iter().filter(|m| s(m, "subtype") == Some("task_notification")).count(),
        "quiet_gaps_after_a_result" => gaps,
        "longest_wait_after_a_result_s" => longest,
        "last_result_cost_usd" => last_cost,
        "session_ids" => strs(&ids),
        "last_line_to_exit_s" => round_to(exit_t - last_t, 1),
    });
}

fn mtime_s(m: &fs::Metadata) -> f64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The entries of `dir`, sorted (`sorted(dir.iterdir())`); none when it is not a folder.
fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|e| e.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Stream lines' arrival records, shared with the reader thread.
type Meta = Arc<Mutex<Vec<Json>>>;

enum Kind {
    Headless {
        child: Child,
        meta: Meta,
        done: mpsc::Receiver<()>,
    },
    Tmux(Tmux),
}

struct Tmux {
    sock: String,
    name: String,
    answered: Vec<String>,
    selections: Vec<Json>,
    last_save: f64,
}

struct Session {
    rec: SessionRecord,
    launcher_pid: Option<i32>,
    launcher_start: Option<String>,
    kind: Kind,
}

/// One stream line's arrival record.
fn stream_meta(t: f64, line: &[u8]) -> Json {
    let mut meta = facts! {"t" => round_to(t, 3), "type" => Json::Null};
    match json::try_parse(line) {
        Ok(rec) if rec.as_object().is_some() => {
            meta.set("type", member(&rec, "type").clone());
            meta.set("subtype", member(&rec, "subtype").clone());
            if s(&rec, "type") == Some("result") {
                for k in [
                    "result_index",
                    "is_error",
                    "num_turns",
                    "total_cost_usd",
                    "duration_ms",
                ] {
                    meta.set(k, member(&rec, k).clone());
                }
                meta.set("origin", member(member(&rec, "origin"), "kind").clone());
            }
            if s(&rec, "type") == Some("system") && s(&rec, "subtype") == Some("init") {
                for k in ["session_id", "model", "permissionMode"] {
                    meta.set(k, member(&rec, k).clone());
                }
            }
        }
        Ok(_) => {}
        Err(_) => meta.set("type", "unparsed"),
    }
    meta.to_json()
}

impl Session {
    fn start_headless(
        rec: SessionRecord,
        test: &str,
        argv: &[OsString],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Result<Session, String> {
        let dir = rec.dir.clone();
        fs::create_dir_all(&dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
        let err = File::create(dir.join("launcher.stderr.txt")).map_err(|e| e.to_string())?;
        let mut rec = rec;
        rec.started_at = now();
        rec.started_utc = utc_now();
        let (child, mut out) = spawn_new_session(test, argv, cwd, env, err)?;
        let pid = child.id() as i32;
        let start = common::start_time(pid);
        let meta: Meta = Arc::default();
        let (tx, done) = mpsc::channel();
        let shared = Arc::clone(&meta);
        thread::spawn(move || {
            let open = |name: &str| File::create(dir.join(name)).expect("a stream file");
            let (mut raw, mut times) = (open("stream.jsonl"), open("stream-times.jsonl"));
            let mut line = Vec::new();
            loop {
                line.clear();
                match out.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let t = now();
                let _ = raw.write_all(&line);
                let _ = raw.flush();
                let m = stream_meta(t, &line);
                let _ = writeln!(times, "{}", json::dumps(&m));
                let _ = times.flush();
                shared.lock().unwrap_or_else(|e| e.into_inner()).push(m);
            }
            let _ = tx.send(());
        });
        Ok(Session {
            rec,
            launcher_pid: Some(pid),
            launcher_start: start,
            kind: Kind::Headless { child, meta, done },
        })
    }

    fn start_tmux(
        rec: SessionRecord,
        argv: &[OsString],
        cwd: &Path,
        env: &[(OsString, OsString)],
        sock: &str,
    ) -> Result<Session, String> {
        let dir = rec.dir.clone();
        fs::create_dir_all(&dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
        let name = format!("{sock}-{}", rec.n);
        let config = env
            .iter()
            .rev()
            .find(|(k, _)| k == "LEADHAND_CONFIG")
            .map(|(_, v)| v.clone())
            .ok_or("LEADHAND_CONFIG is not in the environment")?;
        let mut e = OsString::from("LEADHAND_CONFIG=");
        e.push(config);
        let mut tmux_argv: Vec<OsString> = [
            "tmux",
            "-L",
            sock,
            "start-server",
            ";",
            "set-option",
            "-g",
            "remain-on-exit",
            "on",
            ";",
            "new-session",
            "-d",
            "-s",
            &name,
            "-c",
        ]
        .map(OsString::from)
        .to_vec();
        tmux_argv.push(cwd.into());
        tmux_argv.extend(["-x", "200", "-y", "50", "-e"].map(OsString::from));
        tmux_argv.push(e);
        tmux_argv.extend(["env", "-u", "CLAUDECODE"].map(OsString::from));
        tmux_argv.extend(argv.iter().cloned());
        let mut rec = rec;
        rec.started_at = now();
        rec.started_utc = utc_now();
        let r = push_probe::run(&tmux_argv, env, cwd, None, Duration::from_secs(30));
        let code = raising(&r, &tmux_argv);
        rec.notes.set("tmux_exit", code);
        if code != 0 {
            rec.notes.set("tmux_stderr", r.stderr.trim());
        }
        let t = Tmux {
            sock: sock.to_string(),
            name,
            answered: Vec::new(),
            selections: Vec::new(),
            last_save: 0.0,
        };
        let pid = t.fmt("#{pane_pid}");
        let launcher_pid = pid
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| pid.parse::<i32>().ok())
            .flatten();
        let launcher_start = launcher_pid.and_then(common::start_time);
        Ok(Session {
            rec,
            launcher_pid,
            launcher_start,
            kind: Kind::Tmux(t),
        })
    }

    /// Signals the launcher (it forwards SIGINT/SIGTERM to claude), checked by pid + start.
    fn signal(&self, sig: Signal) -> bool {
        match self.launcher_pid {
            Some(pid) if common::same_process(pid, self.launcher_start.as_deref()) => {
                send(pid, sig)
            }
            _ => false,
        }
    }

    fn alive(&mut self) -> bool {
        match &mut self.kind {
            Kind::Headless { child, meta, done } => {
                let status = child.try_wait().expect("polling the launcher");
                if let Some(status) = status
                    && self.rec.exited_at.is_none()
                {
                    self.rec.exited_at = Some(now());
                    self.rec.exited_utc = Some(utc_now());
                    self.rec.exit_code = Json::from(common::code_of(status));
                    let _ = done.recv_timeout(Duration::from_secs(10));
                }
                self.rec.stream_meta = meta.lock().unwrap_or_else(|e| e.into_inner()).clone();
                status.is_none()
            }
            Kind::Tmux(t) => {
                if t.fmt("#{pane_dead}") == "0" {
                    return true;
                }
                if self.rec.exited_at.is_none() {
                    self.rec.exited_at = Some(now());
                    self.rec.exited_utc = Some(utc_now());
                    let status = t.fmt("#{pane_dead_status}");
                    let digits = status.strip_prefix('-').unwrap_or(&status);
                    self.rec.exit_code =
                        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                            Json::from(status.parse::<i64>().unwrap_or(0))
                        } else if status.is_empty() {
                            Json::from("unknown")
                        } else {
                            Json::from(status)
                        };
                    let pane = t.capture();
                    write_file(&self.rec.dir.join("pane-final.txt"), &pane);
                }
                false
            }
        }
    }

    /// Answers tmux mode's first-run dialogs (headless: nothing).
    fn poll(&mut self) {
        if matches!(self.kind, Kind::Headless { .. }) || !self.alive() {
            return;
        }
        let Kind::Tmux(t) = &mut self.kind else {
            return;
        };
        let pane = t.capture();
        let now_t = now();
        let dir = self.rec.dir.clone();
        if now_t - t.last_save >= PANE_SAVE_EVERY_S {
            let secs = (now_t - self.rec.started_at) as i64;
            write_file(&dir.join(format!("pane-{secs:05}s.txt")), &pane);
            t.last_save = now_t;
        }
        for (pat, text) in DIALOG_PATTERNS {
            let seen = t.answered.iter().filter(|a| *a == pat).count();
            if pane.contains(text) && seen < DIALOG_REPEAT_LIMIT {
                t.answered.push(pat.to_string());
                write_file(&dir.join(format!("dialog-{}.txt", t.answered.len())), &pane);
                let (ok, hl, keys) = t.select();
                t.selections.push(obj! {
                    "dialog" => pat,
                    "ok" => ok,
                    "highlighted" => opt(hl),
                    "keys" => strs(&keys),
                    "at_s" => round_to(now_t - self.rec.started_at, 1),
                });
                if !ok {
                    for _ in 0..DIALOG_REPEAT_LIMIT {
                        t.answered.push(pat.to_string());
                    }
                }
                thread::sleep(Duration::from_secs(2));
                break;
            }
        }
        self.rec
            .notes
            .set("dialogs", Json::Arr(t.selections.clone()));
    }

    fn stop(&mut self) -> Result<(), String> {
        match &mut self.kind {
            Kind::Headless { child, .. } => {
                if child.try_wait().expect("polling the launcher").is_none() {
                    self.signal(Signal::TERM);
                    let Kind::Headless { child, .. } = &mut self.kind else {
                        unreachable!()
                    };
                    if wait_child(child, STOP_GRACE_S).is_none() {
                        send_group(child.id() as i32, Signal::KILL);
                        if wait_child(child, 10.0).is_none() {
                            return Err(format!(
                                "TimeoutExpired: the launcher {} outlived SIGKILL by 10 s",
                                child.id()
                            ));
                        }
                    }
                    self.rec.notes.set("stopped_by_driver", true);
                }
                self.alive();
            }
            Kind::Tmux(_) => {
                if self.alive() {
                    let Kind::Tmux(t) = &self.kind else {
                        unreachable!()
                    };
                    write_file(&self.rec.dir.join("pane-at-stop.txt"), &t.capture());
                    self.signal(Signal::TERM);
                    let deadline = now() + STOP_GRACE_S;
                    while now() < deadline && self.alive() {
                        thread::sleep(Duration::from_secs(1));
                    }
                    if self.alive()
                        && let Some(pid) = self.launcher_pid
                        && common::same_process(pid, self.launcher_start.as_deref())
                    {
                        send(pid, Signal::KILL);
                    }
                    self.rec.notes.set("stopped_by_driver", true);
                }
                let Kind::Tmux(t) = &self.kind else {
                    unreachable!()
                };
                t.tmux(&["kill-session", "-t", &t.name]);
                self.alive();
            }
        }
        Ok(())
    }
}

/// A command's exit code; panics (Python's raised `TimeoutExpired` or `OSError`) when it has
/// none.
fn raising(r: &Ran, argv: &[OsString]) -> i32 {
    match &r.code {
        Ok(c) => *c,
        Err(e) => panic!("{}: {e}", common::shlex_join(argv)),
    }
}

fn write_file(path: &Path, text: &str) {
    fs::write(path, text).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// The highlighted menu line: `^\s*❯\s*(.*)$` (multiline) on `pane`, its group.
pub(crate) fn highlighted(pane: &str) -> Option<String> {
    let mut at = 0;
    for line in pane.split_inclusive('\n') {
        let lead = line.len() - line.trim_start_matches(char::is_whitespace).len();
        if line[lead..].starts_with('❯') {
            let rest = &pane[at + lead + '❯'.len_utf8()..];
            let rest = rest.trim_start_matches(char::is_whitespace);
            return Some(rest.split('\n').next().unwrap_or("").to_string());
        }
        at += line.len();
    }
    None
}

impl Tmux {
    fn tmux(&self, args: &[&str]) -> Ran {
        let mut argv: Vec<OsString> = vec!["tmux".into(), "-L".into(), self.sock.clone().into()];
        argv.extend(args.iter().map(OsString::from));
        let r = push_probe::run(
            &argv,
            &common::process_env(),
            Path::new("/"),
            None,
            Duration::from_secs(30),
        );
        raising(&r, &argv);
        r
    }

    fn fmt(&self, fmt: &str) -> String {
        let r = self.tmux(&["display-message", "-p", "-t", &self.name, fmt]);
        if matches!(r.code, Ok(0)) {
            r.stdout.trim().to_string()
        } else {
            String::new()
        }
    }

    fn capture(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", &self.name]).stdout
    }

    fn select(&self) -> (bool, Option<String>, Vec<String>) {
        let (mut pane, mut keys) = (self.capture(), Vec::new());
        for mv in 0..=MENU_MAX_MOVES {
            let m = highlighted(&pane);
            let has_yes = splitlines(&pane)
                .iter()
                .any(|line| has_word(line, &MENU_WANTED, false));
            let Some(group) = m.filter(|_| has_yes) else {
                let m = highlighted(&pane);
                if let Some(g) = &m
                    && has_word(g, &MENU_FORBIDDEN, true)
                {
                    return (false, Some(g.trim().to_string()), keys);
                }
                self.tmux(&["send-keys", "-t", &self.name, "Enter"]);
                keys.push("Enter".to_string());
                return (true, m.map(|g| g.trim().to_string()), keys);
            };
            let line = group.trim().to_string();
            if has_word(&line, &MENU_WANTED, false) && !has_word(&line, &MENU_FORBIDDEN, true) {
                self.tmux(&["send-keys", "-t", &self.name, "Enter"]);
                keys.push("Enter".to_string());
                return (true, Some(line), keys);
            }
            if mv == MENU_MAX_MOVES {
                return (false, Some(line), keys);
            }
            self.tmux(&["send-keys", "-t", &self.name, "Down"]);
            keys.push("Down".to_string());
            thread::sleep(Duration::from_millis(500));
            pane = self.capture();
        }
        (false, None, keys)
    }
}

// --------------------------------------------------------------------------
// The interrupted run's poll hook

/// What the checks read about the interruption.
#[derive(Clone, Debug)]
pub struct InterruptRecord {
    pub phase: String,
    pub facts: Facts,
    pub killed_dispatch: Json,
    pub next_dispatch: Json,
}

impl Default for InterruptRecord {
    fn default() -> InterruptRecord {
        InterruptRecord {
            phase: String::new(),
            facts: Facts::default(),
            killed_dispatch: Json::Null,
            next_dispatch: Json::Null,
        }
    }
}

impl InterruptRecord {
    /// When the supervisor was killed (`facts["supervisor killed"]["t"]`).
    fn kill_t(&self) -> Option<f64> {
        self.facts
            .get("supervisor killed")
            .and_then(|f| f.get("t"))
            .and_then(Json::as_f64)
    }
}

/// Processes holding the run lock open, from `lsof` (pid and command).
fn lock_holders(lock: &Path) -> Json {
    if common::which("lsof").is_none() || !lock.exists() {
        return Json::Arr(Vec::new());
    }
    let argv: Vec<OsString> = vec!["lsof".into(), "-Fpc".into(), "--".into(), lock.into()];
    let r = push_probe::run(
        &argv,
        &common::process_env(),
        Path::new("/"),
        None,
        Duration::from_secs(20),
    );
    if let Err(RunError::Timeout(_)) = r.code {
        return strs(&["lsof timed out"]);
    }
    raising(&r, &argv);
    let (mut out, mut pid) = (Vec::new(), None::<String>);
    for line in splitlines(&r.stdout) {
        if let Some(p) = line.strip_prefix('p') {
            pid = Some(p.to_string());
        } else if let (Some(c), Some(p)) = (line.strip_prefix('c'), &pid)
            && !p.is_empty()
        {
            out.push(format!("{p} {c}"));
        }
    }
    strs(&out)
}

/// The interrupted run's poll hook. Phase 1: when card 2's Codex implementer is running
/// (runner.json names a live codex pid), SIGKILL that codex process at its first completed
/// item (or after KILL_AFTER_FIRST_STEP_S). Phase 2: when card 2's next attempt is running
/// (its worker started; for Codex, its runner's codex is live), try the exact relaunch while
/// the old supervisor lives (it must be refused), then SIGKILL the supervisor (holder.json's
/// pid). The driver's relaunch loop then relaunches with launch-line until the lock is free.
struct Interrupter {
    rec: InterruptRecord,
    codex_live_since: Option<f64>,
}

impl Interrupter {
    fn new() -> Interrupter {
        Interrupter {
            rec: InterruptRecord {
                phase: "wait card 2 codex".to_string(),
                ..InterruptRecord::default()
            },
            codex_live_since: None,
        }
    }

    fn pending(&self) -> bool {
        self.rec.phase.starts_with("wait")
    }

    /// Poll fast while a trigger is pending and for a while after the supervisor kill.
    fn fast(&self) -> bool {
        self.pending()
            || self
                .rec
                .kill_t()
                .is_some_and(|t| now() - t < FAST_AFTER_KILL_S)
    }

    fn note(&mut self, d: &Driver, key: &str, value: impl Into<Json>) {
        let value = value.into();
        d.log(&format!("interrupt: {key}: {}", show(&value)));
        self.rec.facts.set(key, value);
    }

    fn card2_dispatches(events: &[Json]) -> Vec<&Json> {
        events
            .iter()
            .filter(|e| kind(e) == "dispatch_requested" && eq_num(member(e, "card"), 2))
            .collect()
    }

    fn state(events: &[Json], did: &Json) -> (bool, bool) {
        let is = |k: &str| {
            events
                .iter()
                .any(|e| kind(e) == k && json::py_eq(member(e, "dispatch_id"), did))
        };
        (is("worker_started"), is("result_recorded"))
    }

    fn call(&mut self, d: &mut Driver, events: &[Json]) {
        if self.rec.phase == "wait card 2 codex" {
            self.phase1(d, events);
        } else if self.rec.phase == "wait card 2 next attempt" {
            self.phase2(d, events);
        }
    }

    fn worker_dir(d: &Driver, did: &Json) -> PathBuf {
        let did = did
            .as_str()
            .unwrap_or_else(|| panic!("a dispatch_id that is no string: {did}"));
        d.run_dir().join("workers").join(did)
    }

    fn phase1(&mut self, d: &mut Driver, events: &[Json]) {
        let reqs: Vec<&Json> = Self::card2_dispatches(events)
            .into_iter()
            .filter(|e| {
                s(e, "vendor") == Some("codex") && s(e, "dispatch_kind") == Some("implement")
            })
            .collect();
        let Some(first) = reqs.first() else {
            return;
        };
        let did = first["dispatch_id"].clone();
        let wd = Self::worker_dir(d, &did);
        if wd.join("result.json").exists() || Self::state(events, &did).1 {
            self.note(
                d,
                "too fast to interrupt",
                format!(
                    "{} finished before its codex process could be killed",
                    py_str(&did)
                ),
            );
            self.rec.phase = "skipped: too fast to interrupt".to_string();
            return;
        }
        let procs = runner_processes(&wd.join("runner.json"));
        let Some(codex) = procs.get("codex").cloned() else {
            return;
        };
        if !same(member(&codex, "pid"), member(&codex, "start")) {
            return;
        }
        let now_t = now();
        if self.codex_live_since.is_none() {
            self.codex_live_since = Some(now_t);
            self.note(
                d,
                "card 2 codex seen",
                obj! {"dispatch" => did.clone(), "pid" => codex["pid"].clone(), "utc" => utc_now()},
            );
        }
        let steps = read_jsonl(&wd.join("stream.jsonl"))
            .iter()
            .filter(|e| s(e, "type") == Some("item.completed"))
            .count();
        if steps == 0 && now_t - self.codex_live_since.unwrap() < KILL_AFTER_FIRST_STEP_S {
            return;
        }
        let killed = kill_runner_processes(&wd.join("runner.json"), Signal::KILL, &["codex"]);
        self.rec.killed_dispatch = did.clone();
        let runner = runner_processes(&wd.join("runner.json"))
            .get("runner")
            .cloned()
            .unwrap_or(Json::Null);
        self.note(
            d,
            "codex killed",
            obj! {
                "dispatch" => did, "pid" => codex["pid"].clone(), "signalled" => signalled_json(&killed),
                "utc" => utc_now(), "t" => round_to(now_t, 3),
                "completed items before the kill" => steps, "runner" => runner,
            },
        );
        self.rec.phase = "wait card 2 next attempt".to_string();
    }

    fn phase2(&mut self, d: &mut Driver, events: &[Json]) {
        let killed = self.rec.killed_dispatch.clone();
        let killed_seq = events
            .iter()
            .find(|e| {
                kind(e) == "dispatch_requested" && json::py_eq(member(e, "dispatch_id"), &killed)
            })
            .map_or(Json::from(0), |e| e["seq"].clone());
        let nxt: Vec<&Json> = Self::card2_dispatches(events)
            .into_iter()
            .filter(|e| {
                num_or0(&e["seq"]) > num_or0(&killed_seq)
                    && !json::py_eq(member(e, "dispatch_id"), &killed)
            })
            .collect();
        let Some(req) = nxt.first() else {
            return;
        };
        let did = req["dispatch_id"].clone();
        let (started, result) = Self::state(events, &did);
        if result {
            self.note(d, "next attempt finished before the supervisor kill", did);
            self.rec.phase = "skipped: next attempt too fast".to_string();
            return;
        }
        if !started {
            return;
        }
        let mut runner = Json::Null;
        if s(req, "vendor") == Some("codex") {
            let procs = runner_processes(&Self::worker_dir(d, &did).join("runner.json"));
            let Some(codex) = procs.get("codex") else {
                return;
            };
            if !same(member(codex, "pid"), member(codex, "start")) {
                return;
            }
            runner = procs.get("runner").cloned().unwrap_or(Json::Null);
        }
        self.rec.next_dispatch = did.clone();
        self.note(
            d,
            "card 2 next attempt running",
            obj! {
                "dispatch" => did, "vendor" => member(req, "vendor").clone(),
                "kind" => member(req, "dispatch_kind").clone(), "attempt" => member(req, "attempt").clone(),
                "runner at kill" => runner, "utc" => utc_now(),
            },
        );
        self.prekill_relaunch(d, events.len());
        let holder = read_json(&d.run_dir().join("holder.json"));
        let (pid, start) = (
            member(&holder, "supervisor_pid"),
            member(&holder, "supervisor_start"),
        );
        let alive = same(pid, start);
        let done = alive && send(pid.as_i64().unwrap() as i32, Signal::KILL);
        self.note(
            d,
            "supervisor killed",
            obj! {
                "pid" => pid.clone(), "was alive (pid + start match)" => alive, "signalled" => done,
                "utc" => utc_now(), "t" => round_to(now(), 3), "events before the kill" => events.len(),
            },
        );
        self.rec.phase = if done {
            "done"
        } else {
            "failed: supervisor not found"
        }
        .to_string();
    }

    fn prekill_relaunch(&mut self, d: &mut Driver, _events_now: usize) {
        let Some(argv) = d.relaunch_argv() else {
            self.note(d, "relaunch under a live supervisor", "launch-line failed");
            return;
        };
        let before = d.events().len();
        let holders = lock_holders(&d.run_dir().join("run.lock"));
        let out_dir = d.art.join("interrupt");
        let _ = fs::create_dir_all(&out_dir);
        let err_path = out_dir.join("prekill-relaunch.stderr");
        let err = File::create(&err_path).expect("the pre-kill relaunch's stderr file");
        let elsewhere = d.fx().elsewhere.clone();
        let (mut p, mut out) = spawn_new_session(d.test, &argv, &elsewhere, &d.env, err)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut stdout = File::create(out_dir.join("prekill-relaunch.stdout"))
            .expect("the pre-kill relaunch's stdout file");
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            let _ = stdout.write_all(&buf);
        });
        d.commands.borrow_mut().push(CommandRecord {
            argv: common::shlex_join(&argv),
            code: Json::from("(pre-kill relaunch)"),
            cwd: elsewhere.display().to_string(),
        });
        let code = match wait_child(&mut p, PREKILL_RELAUNCH_WAIT_S) {
            Some(c) => Json::from(c),
            None => {
                send(p.id() as i32, Signal::TERM); // forwarded to its claude
                if wait_child(&mut p, STOP_GRACE_S).is_none() {
                    send_group(p.id() as i32, Signal::KILL);
                    if wait_child(&mut p, 10.0).is_none() {
                        panic!("TimeoutExpired: the pre-kill relaunch outlived SIGKILL by 10 s");
                    }
                }
                Json::from("still running: NOT refused")
            }
        };
        let err_text = fs::read(&err_path)
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();
        let added = d.events().len() as i64 - before as i64;
        self.note(
            d,
            "relaunch under a live supervisor",
            obj! {
                "argv" => common::shlex_join(&argv), "exit" => code, "stderr" => short_text(&err_text, 300),
                "events added by the attempt" => added, "lock holders" => holders,
            },
        );
    }
}

// --------------------------------------------------------------------------
// Git answers, live or recorded

/// The Git questions the checks ask about the fixture repository.
pub trait Repo {
    /// `git rev-parse refs/heads/<branch>`, or "" when it fails.
    fn head(&self, branch: &str) -> String;
    /// `git merge-base --is-ancestor <sha> refs/heads/<branch>` exited 0.
    fn is_ancestor(&self, sha: &str, branch: &str) -> bool;
    /// `git log --format=%h %s <base>..refs/heads/<branch>`, or "".
    fn log_since_base(&self, branch: &str) -> String;
    /// `git show refs/heads/<branch>:<path>`, or "".
    fn show_file(&self, branch: &str, path: &str) -> String;
    /// `git for-each-ref` in the bare origin, or "".
    fn origin_refs(&self) -> String;
    /// `git rev-list --no-merges <base>..refs/heads/<branch>`, or "".
    fn commits_since_base(&self, branch: &str) -> String;
    /// `git show <sha> | git patch-id --stable`'s output; panics when it times out.
    fn patch_id(&self, sha: &str) -> String;
    /// `git show -s --format=%s%x00%T <sha>`, or "".
    fn subject_tree(&self, sha: &str) -> String;
    /// The fixture's tests in the workspace `ws`: `exit <code>: <stderr's end>`.
    fn fixture_tests(&self, ws: &Path) -> String;
}

struct LiveRepo<'a> {
    d: &'a Driver,
}

impl Repo for LiveRepo<'_> {
    fn head(&self, branch: &str) -> String {
        self.d
            .git(&["rev-parse", &format!("refs/heads/{branch}")], None)
    }

    fn is_ancestor(&self, sha: &str, branch: &str) -> bool {
        let r = self.d.cmd(
            &self.d.git_argv(
                &[
                    "merge-base",
                    "--is-ancestor",
                    sha,
                    &format!("refs/heads/{branch}"),
                ],
                None,
            ),
            None,
            CMD_TIMEOUT,
            false,
        );
        matches!(r.code, Ok(0))
    }

    fn log_since_base(&self, branch: &str) -> String {
        let range = format!("{}..refs/heads/{branch}", self.d.fx().base_sha);
        self.d.git(&["log", "--format=%h %s", &range], None)
    }

    fn show_file(&self, branch: &str, path: &str) -> String {
        self.d
            .git(&["show", &format!("refs/heads/{branch}:{path}")], None)
    }

    fn origin_refs(&self) -> String {
        self.d.git(&["for-each-ref"], Some(&self.d.fx().origin))
    }

    fn commits_since_base(&self, branch: &str) -> String {
        let range = format!("{}..refs/heads/{branch}", self.d.fx().base_sha);
        self.d.git(&["rev-list", "--no-merges", &range], None)
    }

    fn patch_id(&self, sha: &str) -> String {
        let show = self.d.cmd(
            &self.d.git_argv(&["show", sha], None),
            None,
            CMD_TIMEOUT,
            false,
        );
        let argv: Vec<OsString> = ["git", "patch-id", "--stable"].map(OsString::from).to_vec();
        let r = push_probe::run(
            &argv,
            &self.d.env,
            &self.d.fx().repo,
            Some(show.stdout.as_bytes()),
            Duration::from_secs(60),
        );
        raising(&r, &argv);
        r.stdout
    }

    fn subject_tree(&self, sha: &str) -> String {
        self.d.git(&["show", "-s", "--format=%s%x00%T", sha], None)
    }

    fn fixture_tests(&self, ws: &Path) -> String {
        let argv: Vec<OsString> = fixture::TEST_ARGV.map(OsString::from).to_vec();
        let r = self.d.cmd(&argv, Some(ws), CMD_TIMEOUT, true);
        let err = r.stderr.trim();
        let tail: String = {
            let chars: Vec<char> = err.chars().collect();
            chars[chars.len().saturating_sub(200)..].iter().collect()
        };
        format!("exit {}: {}", show(&code_json(&r)), short_text(&tail, 300))
    }
}

/// A command's exit code as the Python recorded it: the number, or why there is none.
fn code_json(r: &Ran) -> Json {
    match &r.code {
        Ok(c) => Json::from(*c),
        Err(e) => Json::from(e.to_string()),
    }
}

// --------------------------------------------------------------------------
// Checks

#[derive(Clone, Debug)]
pub struct Check {
    pub name: String,
    pub verdict: String,
    pub how: String,
    pub facts: Facts,
    pub excerpt: Vec<String>,
}

/// Everything the checks read: recorded by this driver, or read back from a run's
/// artefacts.
pub struct Evidence<'a> {
    pub run_dir: PathBuf,
    pub files: SessionFiles,
    pub repo: &'a dyn Repo,
    /// LEADHAND_BIN, the binary the driver started.
    pub bin: PathBuf,
    /// The CLI path the launch reported, which the supervisor's Bash calls are checked
    /// against, and where it came from.
    pub cli: String,
    pub cli_source: String,
    /// True when the launch reported no CLI path and `cli` is LEADHAND_BIN.
    pub cli_fell_back: bool,
    /// This checkout (the plugin dir).
    pub root: PathBuf,
    pub mode: Mode,
    pub sessions: Vec<SessionRecord>,
    pub session_ids: Vec<String>,
    pub status_samples: Vec<Json>,
    /// The driver's setup facts.
    pub facts: Facts,
    pub stop_reason: String,
    pub interrupt: Option<InterruptRecord>,
    pub t0: f64,
    pub now: f64,
    pub origin_refs_before: String,
}

struct Ctx<'a> {
    ev: &'a Evidence<'a>,
    events: Vec<Json>,
    meta: Json,
    attempts: Vec<Json>,
    charges: Vec<Json>,
    branch: String,
    head: String,
    main_recs: Vec<(PathBuf, Vec<Json>)>,
    sub_recs: Vec<(PathBuf, Vec<Json>)>,
}

impl Ctx<'_> {
    fn all_recs(&self) -> impl Iterator<Item = &(PathBuf, Vec<Json>)> {
        self.main_recs.iter().chain(&self.sub_recs)
    }

    fn headlines(&self) -> Vec<String> {
        self.ev
            .status_samples
            .iter()
            .map(|s| {
                member(s, "headline")
                    .as_str()
                    .unwrap_or_else(|| panic!("a headline that is no string: {s}"))
                    .to_string()
            })
            .collect()
    }

    fn report_text(&self) -> String {
        let p = self.ev.run_dir.join("report.md");
        if p.exists() {
            common::read_text(&p)
        } else {
            String::new()
        }
    }

    fn intr(&self) -> &InterruptRecord {
        self.ev
            .interrupt
            .as_ref()
            .expect("the interrupted run's record")
    }
}

#[derive(Default)]
struct Checks(Vec<Check>);

impl Checks {
    fn add(&mut self, name: &str, verdict: &str, facts: Facts, excerpt: Vec<String>) {
        let how = CHECK_DOCS
            .iter()
            .chain(&INTERRUPT_CHECK_DOCS)
            .chain([&LAUNCH_CLI_DOC, &LAUNCH_CLI_MISSING_DOC])
            .find(|(n, _)| *n == name)
            .map_or("", |(_, h)| *h);
        self.0.push(Check {
            name: name.to_string(),
            verdict: verdict.to_string(),
            how: how.to_string(),
            facts,
            excerpt,
        });
    }
}

fn pass(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

type Step = fn(&mut Checks, &Ctx);

/// Every check, on `ev`; a check that panics (Python's exception) is a FAIL named after it.
pub fn run_checks(ev: &Evidence) -> Vec<Check> {
    let events = read_jsonl(&ev.run_dir.join("events.jsonl"));
    let meta = read_json(&ev.run_dir.join("run.json"));
    let branch = py_text(meta.get("branch"));
    let (mains, subs) = transcripts(&ev.files, &ev.session_ids);
    let ctx = Ctx {
        ev,
        attempts: read_jsonl(&ev.run_dir.join("attempts.jsonl")),
        charges: read_jsonl(&ev.run_dir.join("charges.jsonl")),
        head: ev.repo.head(&branch),
        main_recs: mains
            .into_iter()
            .map(|p| {
                let r = read_jsonl(&p);
                (p, r)
            })
            .collect(),
        sub_recs: subs
            .into_iter()
            .map(|p| {
                let r = read_jsonl(&p);
                (p, r)
            })
            .collect(),
        events,
        meta,
        branch,
    };
    let mut steps: Vec<(&str, Step)> = vec![
        ("c_outcome", c_outcome),
        ("c_branch", c_branch),
        ("c_report", c_report),
        ("c_gates", c_gates),
        ("c_attempts", c_attempts),
        ("c_usage", c_usage),
        ("c_order", c_order),
        ("c_workspace", c_workspace),
        ("c_headline", c_headline),
        ("c_publish", c_publish),
        ("c_elapsed", c_elapsed),
        ("c_print_wait", c_print_wait),
        ("c_sandbox", c_sandbox),
        ("c_agent_models", c_agent_models),
        ("c_skill", c_skill),
    ];
    if ev.cli_fell_back {
        steps.push(("c_launch_cli_missing", c_launch_cli_missing));
    }
    if adds_launch_cli_check(&ev.bin, &ev.root, ev.cli_fell_back) {
        steps.push(("c_launch_cli", c_launch_cli));
    }
    if ev.interrupt.is_some() {
        steps.extend::<[(&str, Step); 9]>([
            ("i_accepted_once", i_accepted_once),
            ("i_no_duplicates", i_no_duplicates),
            ("i_relaunch", i_relaunch),
            ("i_runner", i_runner),
            ("i_killed_attempt", i_killed_attempt),
            ("i_thread_usage", i_thread_usage),
            ("i_report_unknown", i_report_unknown),
            ("i_headline", i_headline),
            ("i_timings", i_timings),
        ]);
    }
    let mut out = Checks::default();
    for (name, f) in steps {
        if let Err(e) = catch(|| {
            f(&mut out, &ctx);
            Ok(())
        }) {
            let lines = splitlines(&e);
            let tail = lines[lines.len().saturating_sub(8)..]
                .iter()
                .map(|l| l.to_string())
                .collect();
            out.add(name, "FAIL", facts! {"error" => last_line(&e)}, tail);
        }
    }
    out.0
}

fn c_outcome(out: &mut Checks, c: &Ctx) {
    let outs: Vec<&Json> = c.events.iter().filter(|e| kind(e) == "outcome").collect();
    let last = outs.last().copied().unwrap_or(&Json::Null);
    out.add(
        "outcome complete",
        pass(s(last, "outcome") == Some("complete")),
        facts! {
            "outcome events" => or_none(outs.iter().map(|e| Json::Arr(vec![
                member(e, "outcome").clone(), member(e, "qualifier").clone(), member(e, "reasons").clone(),
            ])).collect()),
            "stop reason" => c.ev.stop_reason.as_str(),
        },
        vec![],
    );
}

fn c_branch(out: &mut Checks, c: &Ctx) {
    let mut acc = Dict::default();
    for e in c.events.iter().filter(|e| kind(e) == "accepted") {
        acc.set(member(e, "card").clone(), member(e, "sha").clone());
    }
    let mut facts = facts! {
        "run branch" => c.branch.as_str(),
        "run head" => if c.head.is_empty() { "missing" } else { c.head.as_str() },
        "accepted" => if acc.0.is_empty() { Json::from("none") } else { acc.to_json() },
    };
    let mut ok = !c.head.is_empty();
    for card in [1, 2] {
        let sha = acc.get(&Json::from(card)).cloned().unwrap_or(Json::Null);
        let on = truthy(&sha)
            && c.ev.repo.is_ancestor(
                sha.as_str()
                    .unwrap_or_else(|| panic!("a sha that is no string: {sha}")),
                &c.branch,
            );
        facts.set(format!("card {card} commit on run branch"), on);
        ok = ok && on;
    }
    let log = c.ev.repo.log_since_base(&c.branch);
    facts.set(
        "commits since base",
        or_none(splitlines(&log).iter().map(|l| Json::from(*l)).collect()),
    );
    let src = c.ev.repo.show_file(&c.branch, fixture::SOURCE_FILE);
    facts.set(
        "calc has subtract/multiply",
        Json::Arr(vec![
            Json::from(src.contains("def subtract")),
            Json::from(src.contains("def multiply")),
        ]),
    );
    let ws = py_text(c.ev_meta().get("workspace"));
    if !ws.is_empty() && Path::new(&ws).is_dir() {
        facts.set(
            "fixture tests in the workspace",
            c.ev.repo.fixture_tests(Path::new(&ws)),
        );
    }
    out.add(
        "run branch holds both cards' commits",
        pass(ok),
        facts,
        vec![],
    );
}

impl Ctx<'_> {
    /// run.json, read again (the Python check reads it a second time).
    fn ev_meta(&self) -> Json {
        read_json(&self.ev.run_dir.join("run.json"))
    }
}

fn c_report(out: &mut Checks, c: &Ctx) {
    let report = c.ev.run_dir.join("report.md");
    let text = if report.exists() {
        common::read_text(&report)
    } else {
        String::new()
    };
    let lines: Vec<String> = splitlines(&text).into_iter().map(String::from).collect();
    let line3 = lines.get(2).cloned();
    let by_run = c.ev.facts.get("report.md written by the run") == Some(&Json::Bool(true));
    out.add(
        "report.md line 3",
        pass(line3.as_deref() == Some("Outcome: complete") && by_run),
        facts! {
            "line 3" => line3.unwrap_or_else(|| "missing".to_string()),
            "report exists" => report.exists(),
            "written by the run itself (not the driver's `leadhand report`)" => by_run,
        },
        lines.into_iter().take(10).collect(),
    );
}

fn c_gates(out: &mut Checks, c: &Ctx) {
    let res: Vec<&Json> = c
        .events
        .iter()
        .filter(|e| kind(e) == "gate_result")
        .collect();
    let mut last = Dict::default();
    for e in &res {
        last.set(member(e, "gate").clone(), (*e).clone());
    }
    let sha12 = |e: &Json| {
        py_text(e.get("examined_sha"))
            .chars()
            .take(12)
            .collect::<String>()
    };
    let on_head = |e: &Json| member(e, "examined_sha") == c.head.as_str();
    let mut facts = facts! {
        "run head" => if c.head.is_empty() { "missing" } else { c.head.as_str() },
        "gate results" => or_none(res.iter().map(|e| Json::Arr(vec![
            member(e, "gate").clone(), Json::from(sha12(e)), member(e, "passed").clone(),
        ])).collect()),
    };
    for (g, e) in &last.0 {
        facts.set(
            format!("{} last examined_sha == head", py_str(g)),
            on_head(e),
        );
        facts.set(format!("{} passed", py_str(g)), member(e, "passed").clone());
    }
    let stale: Vec<&Json> = last
        .0
        .iter()
        .filter(|(_, e)| !on_head(e))
        .map(|(g, _)| g)
        .collect();
    let mut required: Vec<&str> = REQUIRED_GATES.to_vec();
    if c.events.iter().any(|e| {
        matches!(kind(e), "gate_started" | "gate_result") && s(e, "gate") == Some("security")
    }) {
        required.push("security");
    }
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|g| last.get(&Json::from(*g)).is_none())
        .collect();
    let failed: Vec<&str> = required
        .iter()
        .copied()
        .filter(|g| {
            last.get(&Json::from(*g))
                .is_some_and(|e| !truthy(member(e, "passed")))
        })
        .collect();
    facts.set("required gates", strs(&required));
    facts.set(
        "required gates missing",
        or_none(missing.iter().map(|g| Json::from(*g)).collect()),
    );
    facts.set(
        "required gates not passed",
        or_none(failed.iter().map(|g| Json::from(*g)).collect()),
    );
    facts.set(
        "earlier results on another sha",
        or_none(
            res.iter()
                .filter(|e| !on_head(e))
                .map(|e| Json::Arr(vec![member(e, "gate").clone(), Json::from(sha12(e))]))
                .collect(),
        ),
    );
    let ok = !c.head.is_empty() && stale.is_empty() && missing.is_empty() && failed.is_empty();
    out.add("every gate examined the run head", pass(ok), facts, vec![]);
}

fn c_attempts(out: &mut Checks, c: &Ctx) {
    let brief = Json::Arr(
        c.attempts
            .iter()
            .map(|a| {
                Json::Arr(
                    [
                        "card",
                        "role",
                        "vendor",
                        "model",
                        "effort",
                        "status",
                        "usage_known",
                    ]
                    .iter()
                    .map(|k| member(a, k).clone())
                    .collect(),
                )
            })
            .collect(),
    );
    for (card, (vendor, model, effort)) in CARD_PINS {
        let mine: Vec<&Json> = c
            .attempts
            .iter()
            .filter(|a| {
                s(a, "role") == Some("implementer")
                    && s(a, "vendor") == Some(vendor)
                    && s(a, "model") == Some(model)
                    && s(a, "effort") == Some(effort)
                    && eq_num(member(a, "card"), card)
            })
            .collect();
        out.add(
            &format!("{vendor} implementer attempt on {model} {effort}"),
            pass(!mine.is_empty()),
            facts! {
                "matching attempts" => mine.len(),
                "usage_known" => Json::Arr(mine.iter().map(|a| member(a, "usage_known").clone()).collect()),
                "all attempts (card, role, vendor, model, effort, status, usage_known)" => brief.clone(),
            },
            vec![],
        );
    }
}

fn c_usage(out: &mut Checks, c: &Ctx) {
    let role = |items: &[Json], r: &str| -> Vec<Json> {
        items
            .iter()
            .filter(|x| s(x, "role") == Some(r))
            .cloned()
            .collect()
    };
    let sup = role(&c.charges, "supervisor");
    let prox_c = role(&c.charges, "proxy");
    let prox_a = role(&c.attempts, "proxy");
    let sup_a = role(&c.attempts, "supervisor");
    let known = |cs: &[Json]| {
        Json::Arr(
            cs.iter()
                .map(|ch| {
                    let usage = member(ch, "usage");
                    Json::from(match usage.get("known") {
                        None => true,
                        Some(k) => truthy(k),
                    })
                })
                .collect(),
        )
    };
    let (mut total, mut any_float) = (0.0f64, false);
    for a in &c.attempts {
        match member(a, "cost_estimate") {
            Json::Num(t) => {
                any_float |= t.parse::<i64>().is_err();
                total += t.parse::<f64>().unwrap_or(0.0);
            }
            Json::Bool(b) => total += f64::from(u8::from(*b)),
            _ => {}
        }
    }
    let cost = if any_float {
        Json::from(round_to(total, 4))
    } else {
        Json::from(total as i64)
    };
    out.add(
        "supervisor and proxy usage recorded",
        pass(!sup.is_empty() && !(prox_a.is_empty() && prox_c.is_empty())),
        facts! {
            "supervisor charges (charges.jsonl)" => sup.len(),
            "supervisor charges known" => known(&sup),
            "supervisor attempts (attempts.jsonl)" => sup_a.len(),
            "proxy attempts (attempts.jsonl)" => prox_a.len(),
            "proxy charges" => prox_c.len(),
            "sum of attempt cost estimates (USD, priced Claude only)" => cost,
            "session cost from stream result (USD)" => Json::Arr(c.ev.sessions.iter()
                .map(|s| s.notes.get("last_result_cost_usd").cloned().unwrap_or(Json::Null)).collect()),
        },
        vec![],
    );
}

fn c_order(out: &mut Checks, c: &Ctx) {
    let (mut req, mut started, mut result) = (Dict::default(), Dict::default(), Dict::default());
    for e in &c.events {
        let did = member(e, "dispatch_id").clone();
        match kind(e) {
            "dispatch_requested" => req.set_default(did, e["seq"].clone()),
            "worker_started" => started.set_default(did, e["seq"].clone()),
            "result_recorded" => result.set_default(did, e["seq"].clone()),
            _ => {}
        }
    }
    let bad: Vec<Json> = result
        .0
        .iter()
        .filter(|(d, s)| started.get(d).is_none_or(|st| num_or0(st) >= num_or0(s)))
        .map(|(d, _)| d.clone())
        .collect();
    let never: Vec<Json> = req
        .0
        .iter()
        .filter(|(d, _)| result.get(d).is_none())
        .map(|(d, _)| d.clone())
        .collect();
    out.add(
        "worker_started before result_recorded",
        pass(!result.0.is_empty() && bad.is_empty()),
        facts! {
            "dispatches" => req.0.len(),
            "with result" => result.0.len(),
            "out of order or unstarted" => or_none(bad),
            "requested but no result" => or_none(never),
        },
        vec![],
    );
}

fn c_workspace(out: &mut Checks, c: &Ctx) {
    let ws = real(&py_text(c.meta.get("workspace")));
    let repo = match py_text(c.meta.get("repo")) {
        r if r.is_empty() => "/nonexistent".to_string(),
        r => r,
    };
    let run_name = file_name(&c.ev.run_dir);
    // Evidence checks run in the base worktree `<repo>/.worktrees/<run dir name>--base`.
    let base_ws = real(&format!("{repo}/.worktrees/{run_name}--base"));
    let allowed = [ws.clone(), base_ws.clone()];
    let mut facts = facts! {
        "workspace" => member(&c.meta, "workspace").clone(),
        "base worktree (evidence)" => base_ws.as_str(),
    };
    let mut ok = true;
    for (path, recs) in &c.sub_recs {
        let mut cwds: Vec<String> = recs
            .iter()
            .filter(|r| truthy(member(r, "cwd")))
            .map(|r| py_text(r.get("cwd")))
            .collect();
        cwds.sort();
        cwds.dedup();
        let mine = !cwds.is_empty() && cwds.iter().all(|cwd| allowed.contains(&real(cwd)));
        facts.set(
            format!("claude {} cwd", file_name(path)),
            if cwds.is_empty() {
                Json::from("none")
            } else {
                strs(&cwds)
            },
        );
        ok = ok && mine;
    }
    for d in sorted_entries(&c.ev.run_dir.join("workers")) {
        let req = read_json(&d.join("request.json"));
        let res = read_json(&d.join("result.json"));
        let sid = py_text(res.get("session_id"));
        let rollout = if sid.is_empty() {
            None
        } else {
            c.ev.files.codex(&sid)
        };
        let mut rcwd = Json::Null;
        if let Some(r) = rollout {
            let first = read_jsonl(&r)
                .into_iter()
                .next()
                .unwrap_or(Json::Obj(Vec::new()));
            rcwd = member(member(&first, "payload"), "cwd").clone();
        }
        let name = file_name(&d);
        facts.set(
            format!("codex {name} request cwd"),
            member(&req, "cwd").clone(),
        );
        facts.set(
            format!("codex {name} rollout cwd"),
            if truthy(&rcwd) {
                rcwd.clone()
            } else {
                Json::from("not found")
            },
        );
        ok = ok
            && allowed.contains(&real(&py_text(req.get("cwd"))))
            && (rcwd.is_null() || allowed.contains(&real(&py_text(Some(&rcwd)))));
    }
    if c.sub_recs.is_empty() {
        ok = false;
        facts.set("claude worker transcripts", "none found");
    }
    out.add("workers ran in the workspace", pass(ok), facts, vec![]);
}

fn c_headline(out: &mut Checks, c: &Ctx) {
    let heads = c.headlines();
    let implementing = heads.iter().any(|h| h.contains("Implementing"));
    let reviewing = heads.iter().any(|h| h.contains("Reviewing"));
    let kinds = Json::Arr(
        c.events
            .iter()
            .filter(|e| kind(e) == "dispatch_requested")
            .map(|e| member(e, "dispatch_kind").clone())
            .collect(),
    );
    let first30: Vec<String> = distinct(&heads).into_iter().take(30).collect();
    out.add(
        "headline moved through implementing and reviewing",
        pass(implementing && reviewing),
        facts! {
            "samples" => heads.len(),
            "distinct headlines" => strs(&first30),
            "dispatch kinds in events (for reference)" => kinds,
        },
        vec![],
    );
}

fn c_publish(out: &mut Checks, c: &Ctx) {
    let (mut hits, mut n) = (Vec::new(), 0usize);
    for (path, recs) in c.all_recs() {
        for u in tool_uses(recs) {
            if u.name == "Bash" {
                n += 1;
                let cmd = py_text(u.input.get("command"));
                if publishes(&cmd) {
                    hits.push(format!("{}: {}", file_name(path), short_text(&cmd, 200)));
                }
            }
        }
    }
    let mut codex_n = 0usize;
    for d in sorted_entries(&c.ev.run_dir.join("workers")) {
        for e in read_jsonl(&d.join("stream.jsonl")) {
            let item = member(&e, "item");
            if s(&e, "type") == Some("item.completed")
                && s(item, "type") == Some("command_execution")
            {
                codex_n += 1;
                if publishes(&py_text(item.get("command"))) {
                    hits.push(format!(
                        "codex {}: {}",
                        file_name(&d),
                        short(member(item, "command"), 200)
                    ));
                }
            }
        }
    }
    let after = c.ev.repo.origin_refs();
    let unchanged = after == c.ev.origin_refs_before;
    out.add(
        "no publishing command ran",
        pass(hits.is_empty() && unchanged && (n > 0 || codex_n > 0)),
        facts! {
            "claude Bash commands scanned" => n,
            "codex commands scanned" => codex_n,
            "publishing commands" => or_none(hits.iter().map(|h| Json::from(h.as_str())).collect()),
            "origin refs unchanged" => unchanged,
            "origin refs after" => strs(&splitlines(&after)),
        },
        vec![],
    );
}

fn c_elapsed(out: &mut Checks, c: &Ctx) {
    let empty = Json::Obj(Vec::new());
    let start = c
        .events
        .iter()
        .find(|e| kind(e) == "run_started")
        .unwrap_or(&empty);
    let end = c
        .events
        .iter()
        .rev()
        .find(|e| kind(e) == "outcome")
        .unwrap_or(&empty);
    let (t1, t2) = (parse_ts(start.get("ts")), parse_ts(end.get("ts")));
    let secs = match (t1, t2) {
        (Some(a), Some(b)) if a != 0.0 && b != 0.0 => Some(round0(b - a)),
        _ => None,
    };
    out.add(
        "elapsed time",
        if secs.is_some() { "OBSERVED" } else { "FAIL" },
        facts! {
            "run_started -> outcome (s)" => opt(secs),
            "run_started" => member(start, "ts").clone(),
            "outcome" => member(end, "ts").clone(),
            "driver wall time (s)" => round0(c.ev.now - c.ev.t0),
            "sessions" => c.ev.sessions.len(),
            "session durations (s)" => Json::Arr(c.ev.sessions.iter()
                .map(|s| Json::from(round0(s.exited_at.unwrap_or(c.ev.now) - s.started_at))).collect()),
        },
        vec![],
    );
}

fn c_print_wait(out: &mut Checks, c: &Ctx) {
    let mut facts = facts! {"mode" => c.ev.mode.as_str()};
    let mut early = Vec::new();
    for s in &c.ev.sessions {
        let mut f = facts! {
            "how" => s.how.as_str(),
            "exit_code" => s.exit_code.clone(),
            "started" => s.started_utc.as_str(),
            "exited" => opt(s.exited_utc.clone()),
        };
        f.update(s.notes.clone());
        facts.set(format!("session {}", s.n), f.to_json());
        if s.notes
            .get("exited_while_workers_running")
            .is_some_and(truthy)
        {
            early.push(Json::from(s.n));
        }
    }
    facts.set(
        "sessions that exited while workers ran (relaunched)",
        or_none(early),
    );
    let waited: Vec<Json> =
        c.ev.sessions
            .iter()
            .map(|s| match s.notes.get("longest_wait_after_a_result_s") {
                Some(v) if truthy(v) => v.clone(),
                _ => Json::from(0),
            })
            .collect();
    let any_wait = waited.iter().any(|w| num_or0(w) >= PRINT_WAIT_NOTE_S);
    facts.set(
        "longest quiet wait after a result (s), per session",
        Json::Arr(waited),
    );
    facts.set(
        format!("print mode waited >= {PRINT_WAIT_NOTE_S:.0}s for a worker"),
        any_wait,
    );
    out.add(
        "open check: print mode outlives long workers",
        "OBSERVED",
        facts,
        vec![],
    );
}

fn c_sandbox(out: &mut Checks, c: &Ctx) {
    let cli = c.ev.cli.as_str();
    let quoted = common::shlex_quote(cli);
    let (mut rows, mut heredoc, mut plain_violations, mut others) =
        (vec![], vec![], vec![], vec![]);
    for (path, recs) in c.all_recs() {
        let results = tool_results(recs);
        for u in tool_uses(recs) {
            if u.name != "Bash" {
                continue;
            }
            let cmd = py_text(u.input.get("command"));
            let r = results.get(&u.id);
            let sandboxed = r.map_or(Json::from("no result"), |r| r.sandboxed.clone());
            let disable = truthy(member(&u.input, "dangerouslyDisableSandbox"));
            if !cmd.contains(cli) {
                others.push((short_text(&cmd, 100), sandboxed, disable));
                continue;
            }
            let row = (
                short_text(&cmd, 140),
                sandboxed,
                disable,
                file_name(path),
                truthy(member(&u.input, "run_in_background")),
            );
            let stripped = cmd.trim_start();
            if !(stripped.starts_with(cli) || stripped.starts_with(&quoted)) {
                plain_violations.push(row.0.clone());
            }
            if cmd.contains("<<") && cmd.contains("record") {
                heredoc.push(row.clone());
            }
            rows.push(row);
        }
    }
    let is_true = |v: &Json| *v == Json::Bool(true);
    let is_bool = |v: &Json| matches!(v, Json::Bool(_));
    let cmds =
        |items: Vec<&String>| or_none(items.into_iter().map(|c| Json::from(c.as_str())).collect());
    let sandboxed: Vec<&String> = rows
        .iter()
        .filter(|r| is_true(&r.1))
        .map(|r| &r.0)
        .collect();
    let unknown: Vec<&String> = rows
        .iter()
        .filter(|r| !is_bool(&r.1))
        .map(|r| &r.0)
        .collect();
    let flagged = rows.iter().filter(|r| is_bool(&r.1)).count();
    let control: Vec<&String> = others
        .iter()
        .filter(|o| is_true(&o.1) && !o.2)
        .map(|o| &o.0)
        .collect();
    let verdict = if !sandboxed.is_empty() {
        "FAIL"
    } else if flagged > 0 && !control.is_empty() {
        "PASS"
    } else {
        "INCONCLUSIVE"
    };
    out.add(
        "open check: CLI calls ran unsandboxed (exclusion matched)",
        verdict,
        facts! {
            "cli calls" => rows.len(),
            "with a sandboxed flag" => flagged,
            "control: non-CLI Bash calls that ran sandboxed" => cmds(control.iter().take(5).copied().collect()),
            "non-CLI Bash calls (command, sandboxed)" => or_none(others.iter().take(10)
                .map(|o| Json::Arr(vec![Json::from(o.0.as_str()), o.1.clone()])).collect()),
            "sandboxed" => cmds(sandboxed),
            "no sandboxed flag (e.g. background calls)" => cmds(unknown),
            "dangerouslyDisableSandbox set" => rows.iter().filter(|r| r.2).count(),
            "calls not starting with the CLI path" => or_none(plain_violations.iter().map(|c| Json::from(c.as_str())).collect()),
            "first calls" => Json::Arr(rows.iter().take(8)
                .map(|r| Json::Arr(vec![Json::from(r.0.as_str()), r.1.clone()])).collect()),
        },
        vec![],
    );
    let verdict = if heredoc.iter().any(|r| is_true(&r.1)) {
        "FAIL"
    } else if !heredoc.is_empty()
        && !control.is_empty()
        && heredoc.iter().all(|r| r.1 == Json::Bool(false))
    {
        "PASS"
    } else {
        "INCONCLUSIVE"
    };
    out.add(
        "open check: heredoc record command matched the exclusion",
        verdict,
        facts! {
            "heredoc record calls" => heredoc.len(),
            "sandboxed flags" => or_none(heredoc.iter().map(|r| r.1.clone()).collect()),
            "examples" => or_none(heredoc.iter().take(3).map(|r| Json::from(r.0.as_str())).collect()),
        },
        vec![],
    );
}

fn c_agent_models(out: &mut Checks, c: &Ctx) {
    let mut sub_models: Vec<(String, Vec<String>)> = Vec::new();
    for (path, recs) in &c.sub_recs {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let aid = stem.strip_prefix("agent-").unwrap_or(&stem).to_string();
        let mut models: Vec<String> = recs
            .iter()
            .filter(|r| {
                s(r, "type") == Some("assistant") && truthy(member(member(r, "message"), "model"))
            })
            .map(|r| py_text(member(r, "message").get("model")))
            .collect();
        models.sort();
        models.dedup();
        match sub_models.iter_mut().find(|(a, _)| *a == aid) {
            Some(slot) => slot.1 = models,
            None => sub_models.push((aid, models)),
        }
    }
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for (_, recs) in &c.main_recs {
        let results = tool_results(recs);
        for u in tool_uses(recs) {
            if u.name != "Agent" && u.name != "Task" {
                continue;
            }
            let r = results.get(&u.id);
            let aid = agent_id(r.map_or("", |r| r.text.as_str()));
            let actual = sub_models
                .iter()
                .find(|(a, _)| *a == aid.clone().unwrap_or_default())
                .map_or(Json::from("transcript not found"), |(_, m)| strs(m));
            let error = r.is_some_and(|r| r.is_error);
            let call = obj! {
                "subagent_type" => member(&u.input, "subagent_type").clone(),
                "model" => member(&u.input, "model").clone(),
                "effort" => member(&u.input, "effort").clone(),
                "error" => error,
                "result" => short_text(r.map_or("no result", |r| r.text.as_str()), 120),
                "agent_id" => opt(aid),
                "actual_models" => actual,
            };
            if error {
                errors.push(call.clone());
            }
            calls.push(call);
        }
    }
    let verdict = if !calls.is_empty() && errors.is_empty() {
        "PASS"
    } else if !calls.is_empty() {
        "FAIL"
    } else {
        "OBSERVED"
    };
    out.add(
        "open check: Agent tool accepted the route model names",
        verdict,
        facts! {"agent calls" => calls.len(), "rejected" => or_none(errors), "calls" => Json::Arr(calls)},
        vec![],
    );
}

fn c_skill(out: &mut Checks, c: &Ctx) {
    let skill = common::read_text(&c.ev.root.join("skills/run/SKILL.md"));
    let body = skill.splitn(3, "---").last().unwrap_or("");
    let para = body
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.starts_with('#'))
        .unwrap_or("");
    let marker: String = squash(para).chars().take(50).collect();
    let (mut calls, mut loaded) = (Vec::new(), false);
    for (_, recs) in &c.main_recs {
        let results = tool_results(recs);
        for u in tool_uses(recs) {
            if u.name != "Skill" {
                continue;
            }
            let r = results.get(&u.id);
            calls.push((
                u.input.clone(),
                r.is_some_and(|r| r.is_error),
                short_text(r.map_or("none", |r| r.text.as_str()), 120),
            ));
        }
        loaded = loaded || recs.iter().any(|r| squash(&strings(r)).contains(&marker));
    }
    let wanted: Vec<_> = calls
        .iter()
        .filter(|c| json::dumps(&c.0).contains("leadhand:run"))
        .collect();
    let ok = !wanted.is_empty() && !wanted.iter().any(|c| c.1) && loaded;
    out.add(
        "open check: leadhand:run loaded through the Skill tool",
        pass(ok),
        facts! {
            "skill calls" => or_none(calls.iter().map(|(i, e, r)| obj!{"input" => i.clone(), "error" => *e, "result" => r.as_str()}).collect()),
            "body marker" => marker.as_str(),
            "body marker found in transcript" => loaded,
        },
        vec![],
    );
}

fn c_launch_cli_missing(out: &mut Checks, c: &Ctx) {
    out.add(
        LAUNCH_CLI_MISSING_DOC.0,
        "FAIL",
        facts! {
            "LEADHAND_BIN (started)" => c.ev.bin.display().to_string(),
            "CLI path the CLI-call checks used" => c.ev.cli.as_str(),
            "launch CLI path from" => c.ev.cli_source.as_str(),
            "supervisor transcripts" => c.main_recs.len(),
        },
        vec![],
    );
}

fn c_launch_cli(out: &mut Checks, c: &Ctx) {
    let platform = platform_binary(&c.ev.root);
    let ok = common::names_file(&platform, &c.ev.cli);
    out.add(
        LAUNCH_CLI_DOC.0,
        pass(ok),
        facts! {
            "LEADHAND_BIN (started)" => c.ev.bin.display().to_string(),
            "launch CLI path (checked)" => c.ev.cli.as_str(),
            "launch CLI path from" => c.ev.cli_source.as_str(),
            "platform target" => opt(platform_target()),
            "resolved platform binary" => platform.display().to_string(),
            "launch CLI path resolved" => common::realpath(Path::new(&c.ev.cli)).display().to_string(),
        },
        vec![],
    );
}

fn i_accepted_once(out: &mut Checks, c: &Ctx) {
    let count = |card: i64| {
        c.events
            .iter()
            .filter(|e| kind(e) == "accepted" && eq_num(member(e, "card"), card))
            .count()
    };
    let (one, two) = (count(1), count(2));
    out.add(
        "interrupt: each card accepted exactly once",
        pass(one == 1 && two == 1),
        facts! {"accepted events per card" => obj!{"1" => one, "2" => two}},
        vec![],
    );
}

fn i_no_duplicates(out: &mut Checks, c: &Ctx) {
    let listed = c.ev.repo.commits_since_base(&c.branch);
    let shas: Vec<&str> = listed.split_whitespace().collect();
    type Groups = Vec<(String, Vec<String>)>;
    let (mut pids, mut keys, mut rows): (Groups, Groups, Vec<Json>) = (vec![], vec![], vec![]);
    let push = |d: &mut Vec<(String, Vec<String>)>, k: String, v: String| match d
        .iter_mut()
        .find(|(x, _)| *x == k)
    {
        Some(slot) => slot.1.push(v),
        None => d.push((k, vec![v])),
    };
    let cut = |t: &str| t.chars().take(12).collect::<String>();
    for sha in &shas {
        let pid =
            c.ev.repo
                .patch_id(sha)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
        let subj_tree = c.ev.repo.subject_tree(sha);
        rows.push(strs(&[
            cut(sha),
            cut(&pid),
            subj_tree.split('\0').next().unwrap_or("").to_string(),
        ]));
        if !pid.is_empty() {
            push(&mut pids, pid, cut(sha));
        }
        push(&mut keys, subj_tree, cut(sha));
    }
    let dups = |d: &[(String, Vec<String>)]| -> Vec<Json> {
        d.iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(_, v)| strs(v))
            .collect()
    };
    let (dup_p, dup_k) = (dups(&pids), dups(&keys));
    out.add(
        "interrupt: no duplicate commits on the run branch",
        pass(!shas.is_empty() && dup_p.is_empty() && dup_k.is_empty()),
        facts! {
            "commits (sha, patch-id, subject)" => or_none(rows),
            "same patch-id" => or_none(dup_p),
            "same subject and tree" => or_none(dup_k),
        },
        vec![],
    );
}

fn i_relaunch(out: &mut Checks, c: &Ctx) {
    let f = &c.intr().facts;
    let pre = f
        .get("relaunch under a live supervisor")
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or(Json::Obj(Vec::new()));
    let kill_t = c.intr().kill_t().filter(|t| *t != 0.0);
    let after: Vec<&SessionRecord> =
        c.ev.sessions
            .iter()
            .filter(|s| kill_t.is_some_and(|t| s.started_at > t))
            .collect();
    let refused_after: Vec<Json> = after
        .iter()
        .filter(|s| eq_num(&s.exit_code, EXIT_REFUSED))
        .map(|s| Json::from(s.n))
        .collect();
    let ok_after: Vec<&&SessionRecord> = after
        .iter()
        .filter(|s| {
            !eq_num(&s.exit_code, EXIT_REFUSED)
                && (s.notes.get("events_added").is_some_and(truthy)
                    || s.notes.get("run_had_outcome_at_exit").is_some_and(truthy)
                    || s.exited_at.is_none())
        })
        .collect();
    let refused = pre.as_object().is_some()
        && eq_num(member(&pre, "exit"), EXIT_REFUSED)
        && eq_num(member(&pre, "events added by the attempt"), 0);
    out.add(
        "interrupt: relaunch refused while the old supervisor lived, succeeded after",
        pass(refused && !ok_after.is_empty()),
        facts! {
            "relaunch under the live supervisor" => if truthy(&pre) { pre.clone() } else { Json::from("not attempted") },
            "sessions after the kill" => or_none(after.iter().map(|s| Json::Arr(vec![
                Json::from(s.n), Json::from(s.how.as_str()), s.exit_code.clone(), Json::from(s.started_utc.as_str()),
            ])).collect()),
            "refused after the kill (lock still held)" => or_none(refused_after),
            "first successful session after the kill" => ok_after.first().map_or(Json::from("none"), |s| Json::from(s.n)),
            "refusals recorded" => c.ev.facts.get("refused launches (exit 3, lock held)").cloned().unwrap_or(Json::from("none")),
        },
        vec![],
    );
}

fn i_runner(out: &mut Checks, c: &Ctx) {
    let rec = c.intr();
    let did = &rec.next_dispatch;
    let nxt = rec
        .facts
        .get("card 2 next attempt running")
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or(Json::Obj(Vec::new()));
    let name =
        "interrupt: the runner that outlived the supervisor recorded its result, not restarted";
    let mut facts = facts! {
        "dispatch" => if truthy(did) { did.clone() } else { Json::from("none") },
        "vendor" => member(&nxt, "vendor").clone(),
    };
    if !truthy(did) || s(&nxt, "vendor") != Some("codex") {
        facts.set(
            "why",
            "no Codex next attempt was running at the supervisor kill",
        );
        out.add(name, "INCONCLUSIVE", facts, vec![]);
        return;
    }
    let wd =
        c.ev.run_dir
            .join("workers")
            .join(did.as_str().expect("a dispatch id"));
    let res = wd.join("result.json");
    let kill_t = rec.kill_t().unwrap_or(0.0);
    let runner_end = runner_processes(&wd.join("runner.json"))
        .get("runner")
        .cloned()
        .unwrap_or(Json::Null);
    let rj = read_json(&wd.join("runner.json"));
    let superseded: Vec<String> = sorted_entries(&wd.join("superseded"))
        .iter()
        .map(|p| file_name(p))
        .collect();
    let recorded = c
        .events
        .iter()
        .any(|e| kind(e) == "result_recorded" && json::py_eq(member(e, "dispatch_id"), did));
    let after_kill = fs::metadata(&res).is_ok_and(|m| mtime_s(&m) > kill_t);
    let runner_at_kill = member(&nxt, "runner at kill").clone();
    facts.update(facts! {
        "runner at the kill" => runner_at_kill.clone(),
        "runner at the end" => runner_end.clone(),
        "generation" => member(&rj, "generation").clone(),
        "superseded" => if superseded.is_empty() { Json::from("none") } else { strs(&superseded) },
        "result.json present" => res.exists(),
        "result.json written after the kill" => after_kill,
        "result_recorded event" => recorded,
        "result exit_reason" => member(&read_json(&res), "exit_reason").clone(),
    });
    let same_runner = json::py_eq(member(&runner_at_kill, "pid"), member(&runner_end, "pid"));
    let ok = res.exists()
        && eq_num(member(&rj, "generation"), 1)
        && superseded.is_empty()
        && same_runner
        && recorded
        && after_kill;
    out.add(name, pass(ok), facts, vec![]);
}

fn i_killed_attempt(out: &mut Checks, c: &Ctx) {
    let did = &c.intr().killed_dispatch;
    let mine: Vec<&Json> = c
        .attempts
        .iter()
        .filter(|a| json::py_eq(member(a, "dispatch"), did) && s(a, "vendor") == Some("codex"))
        .collect();
    let ch: Vec<&Json> = c
        .charges
        .iter()
        .filter(|x| json::py_eq(member(x, "dispatch_id"), did) && s(x, "source") == Some("codex"))
        .collect();
    let pick = |v: &Json, keys: &[&str]| {
        let mut f = Facts::default();
        for k in keys {
            f.set(*k, member(v, k).clone());
        }
        f.to_json()
    };
    let mut facts = facts! {
        "dispatch" => if truthy(did) { did.clone() } else { Json::from("none (no kill)") },
        "attempts" => or_none(mine.iter().map(|a| pick(a, &["status", "exit_reason", "usage_known", "usage_source", "usage", "worker_id", "steps"])).collect()),
        "codex charges" => or_none(ch.iter().map(|x| pick(x, &["charge_id", "possibly_incomplete", "usage"])).collect()),
    };
    let is_false = |v: &Json| *v == Json::Bool(false);
    let known_ok = !mine.is_empty()
        && mine.iter().all(|a| {
            (truthy(member(a, "usage_known")) && truthy(member(a, "usage_source")))
                || is_false(member(a, "usage_known"))
        });
    let flagged = !ch.is_empty() && ch.iter().all(|x| truthy(member(x, "possibly_incomplete")));
    let no_charge_unknown = ch.is_empty()
        && !mine.is_empty()
        && mine.iter().all(|a| is_false(member(a, "usage_known")));
    facts.set("charges marked possibly_incomplete", flagged);
    out.add(
        "interrupt: the killed Codex attempt is recorded (usage recovered or unknown, possibly_incomplete)",
        pass(known_ok && (flagged || no_charge_unknown)),
        facts,
        vec![],
    );
}

/// A Codex rollout's usage, summed over distinct `token_usage_record` response IDs, in
/// leadhand's charge fields (input excludes cached); None without any.
pub fn rollout_total(path: &Path) -> Option<[i64; 4]> {
    let (mut seen, mut tot) = (Vec::<String>::new(), [0i64; 3]);
    for rec in read_jsonl(path) {
        if s(&rec, "type") != Some("token_usage_record") {
            continue;
        }
        let pl = member(&rec, "payload");
        let (rid, u) = (member(pl, "response_id"), member(pl, "usage"));
        let key = json::dumps(rid);
        if !truthy(rid) || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let cached = int_or0(member(u, "cached_input_tokens"));
        tot[0] += (int_or0(member(u, "input_tokens")) - cached).max(0);
        tot[1] += cached;
        tot[2] += int_or0(member(u, "output_tokens"));
    }
    (!seen.is_empty()).then(|| [tot[0], tot[1], tot[2], seen.len() as i64])
}

fn i_thread_usage(out: &mut Checks, c: &Ctx) {
    let mut threads: Vec<(Json, Vec<&Json>)> = Vec::new();
    for x in &c.charges {
        let key = member(x, "key");
        if s(x, "source") == Some("codex") && truthy(key) {
            match threads.iter_mut().find(|(k, _)| json::py_eq(k, key)) {
                Some(slot) => slot.1.push(x),
                None => threads.push((key.clone(), vec![x])),
            }
        }
    }
    let mut facts = Facts::default();
    let mut ok = !threads.is_empty();
    for (thread, cs) in &threads {
        let sum = |k: &str| {
            cs.iter()
                .map(|x| int_or0(member(member(x, "usage"), k)))
                .sum::<i64>()
        };
        let charged = [sum("input"), sum("cached"), sum("output")];
        let charged_json =
            obj! {"input" => charged[0], "cached" => charged[1], "output" => charged[2]};
        let id = thread
            .as_str()
            .unwrap_or_else(|| panic!("a thread key that is no string: {thread}"));
        let path = c.ev.files.codex(id);
        let rollout = path.as_deref().and_then(rollout_total);
        if rollout.is_none() && path.as_deref().is_some_and(Path::exists) {
            // No token_usage_record at all (killed before its first response): unknown, so
            // every charge must be known=false and possibly_incomplete.
            let matched = cs.iter().all(|x| {
                *member(member(x, "usage"), "known") == Json::Bool(false)
                    && truthy(member(x, "possibly_incomplete"))
            });
            facts.set(
                key_text(thread),
                obj! {
                    "charges" => cs.len(), "charged" => charged_json, "rollout (distinct response_id)" => Json::Null,
                    "no token_usage_record: charges unknown and possibly_incomplete" => matched,
                    "equal" => matched,
                },
            );
            ok = ok && matched;
            continue;
        }
        let matched = rollout.is_some_and(|r| r[..3] == charged);
        let rollout_json = rollout.map_or(Json::Null, |r| {
            obj! {"input" => r[0], "cached" => r[1], "output" => r[2], "responses" => r[3]}
        });
        facts.set(
            key_text(thread),
            obj! {
                "charges" => cs.len(), "charged" => charged_json, "rollout (distinct response_id)" => rollout_json,
                "responses" => opt(rollout.map(|r| r[3])), "equal" => matched,
            },
        );
        ok = ok && matched;
    }
    if facts.0.is_empty() {
        facts.set("codex threads", "none");
    }
    out.add(
        "interrupt: charged Codex usage per thread equals the rollout total",
        pass(ok),
        facts,
        vec![],
    );
}

fn i_report_unknown(out: &mut Checks, c: &Ctx) {
    let text = c.report_text();
    let m = number_after(&text, "Attempts with unknown usage:");
    let unknown = c
        .attempts
        .iter()
        .filter(|a| *member(a, "usage_known") == Json::Bool(false))
        .count();
    out.add(
        "interrupt: the report's Usage counts unknown attempts",
        pass(m.as_ref().is_some_and(|(_, n)| *n == unknown as i64)),
        facts! {
            "report line" => m.map_or("missing".to_string(), |(t, _)| t),
            "attempts with usage_known false" => unknown,
        },
        vec![],
    );
}

fn i_headline(out: &mut Checks, c: &Ctx) {
    let heads = c.headlines();
    let resumed: Vec<&String> = heads
        .iter()
        .filter(|h| has_word(h, &["Resumed at"], true))
        .collect();
    let first30: Vec<String> = distinct(&heads).into_iter().take(30).collect();
    out.add(
        "interrupt: a reconciliation headline appeared",
        pass(!resumed.is_empty()),
        facts! {
            "matching headlines" => or_none(resumed.iter().map(|h| Json::from(h.as_str())).collect()),
            "distinct headlines" => strs(&first30),
        },
        vec![],
    );
}

fn i_timings(out: &mut Checks, c: &Ctx) {
    let rec = c.intr();
    let kill_t = rec.kill_t().filter(|t| *t != 0.0);
    let after: Vec<&SessionRecord> = c
        .ev
        .sessions
        .iter()
        .filter(|s| kill_t.is_some_and(|t| s.started_at > t) && !eq_num(&s.exit_code, EXIT_REFUSED))
        .collect();
    let text = c.report_text();
    let cost = text.find("Estimated API-equivalent cost").map(|i| {
        let rest = &text[i..];
        rest[..rest.find('\n').unwrap_or(rest.len())].to_string()
    });
    out.add(
        "interrupt: timings and estimated cost",
        "OBSERVED",
        facts! {
            "interrupter phase" => rec.phase.as_str(),
            "interrupter facts" => rec.facts.to_json(),
            "supervisor kill -> successful relaunch (s)" => opt(after.first().map(|s| round_to(s.started_at - kill_t.unwrap(), 1))),
            "report cost line" => cost.unwrap_or_else(|| "missing".to_string()),
            "session costs from the stream (USD)" => Json::Arr(c.ev.sessions.iter()
                .map(|s| s.notes.get("last_result_cost_usd").cloned().unwrap_or(Json::Null)).collect()),
        },
        vec![],
    );
}

/// The raw results' `## Checks` and `## Summary` sections.
pub fn checks_markdown(checks: &[Check]) -> Vec<String> {
    let mut out = vec!["## Checks".to_string(), String::new()];
    for c in checks {
        out.extend([
            format!("### {} — {}", c.name, c.verdict),
            String::new(),
            format!("How: {}", c.how),
            String::new(),
        ]);
        if !c.excerpt.is_empty() {
            out.push("```".to_string());
            out.extend(c.excerpt.iter().take(10).cloned());
            out.extend(["```".to_string(), String::new()]);
        }
        out.extend(c.facts.lines());
        out.push(String::new());
    }
    out.extend(["## Summary".to_string(), String::new()]);
    out.extend(
        checks
            .iter()
            .map(|c| format!("- {:8} {}", c.verdict, c.name)),
    );
    out.push(String::new());
    out
}

// --------------------------------------------------------------------------
// The driver

/// One command the driver ran: its argv, exit (or a note) and folder.
struct CommandRecord {
    argv: String,
    code: Json,
    cwd: String,
}

pub struct Driver {
    opts: Options,
    test: &'static str,
    out: PathBuf,
    art: PathBuf,
    raw: PathBuf,
    budget_s: u64,
    /// LEADHAND_BIN: the binary started.
    bin: PathBuf,
    /// This checkout: the plugin dir.
    root: PathBuf,
    tmp: Option<PathBuf>,
    fx: Option<Fixture>,
    run_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    commands: RefCell<Vec<CommandRecord>>,
    sessions: Vec<Session>,
    status_samples: Vec<Json>,
    checks: Vec<Check>,
    facts: Facts,
    errors: Vec<String>,
    interrupter: Option<Interrupter>,
    tmux_sock: String,
    t0: f64,
    outcome_seen_at: Option<f64>,
    stop_reason: String,
    origin_refs_before: String,
    files: SessionFiles,
}

impl Driver {
    pub fn new(opts: Options, test: &'static str) -> Driver {
        let out = opts.out.clone();
        Driver {
            art: out.join("e2e-artifacts"),
            raw: out.join("e2e-raw.md"),
            out,
            budget_s: opts.budget_minutes * 60,
            bin: common::bin(),
            root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            tmp: None,
            fx: None,
            run_dir: None,
            env: Vec::new(),
            commands: RefCell::new(Vec::new()),
            sessions: Vec::new(),
            status_samples: Vec::new(),
            checks: Vec::new(),
            facts: Facts::default(),
            errors: Vec::new(),
            interrupter: opts.interrupt.then(Interrupter::new),
            tmux_sock: format!("lhe2e-{}", std::process::id()),
            t0: now(),
            outcome_seen_at: None,
            stop_reason: String::new(),
            origin_refs_before: String::new(),
            files: SessionFiles::operator(),
            test,
            opts,
        }
    }

    fn run_dir(&self) -> &Path {
        self.run_dir
            .as_deref()
            .expect("the run directory (after setup)")
    }

    fn fx(&self) -> &Fixture {
        self.fx.as_ref().expect("the fixture (after setup)")
    }

    fn bin_os(&self) -> OsString {
        self.bin.clone().into_os_string()
    }

    fn log(&self, msg: &str) {
        let line = format!("[{}] {msg}", utc_now());
        say!("{line}");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.art.join("driver.log"))
            .unwrap_or_else(|e| panic!("opening the driver log: {e}"));
        let _ = writeln!(f, "{line}");
    }

    fn cmd_env(&self) -> Vec<(OsString, OsString)> {
        if self.env.is_empty() {
            common::process_env()
        } else {
            self.env.clone()
        }
    }

    /// Runs `argv` from `cwd` (default: the unrelated folder, else the output folder), with
    /// the run's environment and the timeout; records it unless `record` is false.
    fn cmd(&self, argv: &[OsString], cwd: Option<&Path>, timeout: Duration, record: bool) -> Ran {
        let cwd = cwd.map(Path::to_path_buf).unwrap_or_else(|| {
            self.fx
                .as_ref()
                .map_or_else(|| self.out.clone(), |f| f.elsewhere.clone())
        });
        let r = push_probe::run(argv, &self.cmd_env(), &cwd, None, timeout);
        if record {
            let line = common::shlex_join(argv);
            let code = code_json(&r);
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.art.join("commands.log"))
                .unwrap_or_else(|e| panic!("opening the commands log: {e}"));
            let _ = write!(
                f,
                "$ (cwd={}) {line}\n-> exit {}\n{}{}\n",
                cwd.display(),
                show(&code),
                r.stdout,
                r.stderr
            );
            self.commands.borrow_mut().push(CommandRecord {
                argv: line,
                code,
                cwd: cwd.display().to_string(),
            });
        }
        r
    }

    fn git_argv(&self, args: &[&str], repo: Option<&Path>) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["git".into(), "-C".into()];
        argv.push(repo.unwrap_or(&self.fx().repo).into());
        argv.extend(args.iter().map(OsString::from));
        argv
    }

    /// `git -C <repo> <args>`'s stdout, stripped; "" when it fails.
    fn git(&self, args: &[&str], repo: Option<&Path>) -> String {
        let r = self.cmd(&self.git_argv(args, repo), None, CMD_TIMEOUT, false);
        if matches!(r.code, Ok(0)) {
            r.stdout.trim().to_string()
        } else {
            String::new()
        }
    }

    fn events(&self) -> Vec<Json> {
        self.run_dir
            .as_ref()
            .map_or_else(Vec::new, |d| read_jsonl(&d.join("events.jsonl")))
    }

    // ------------------------------------------------------------------ plan

    fn launch_argv_first(&self) -> Vec<OsString> {
        let deck = self.run_dir().join("deck.md");
        let mut argv: Vec<OsString> = vec![
            self.bin_os(),
            "run".into(),
            deck.into(),
            "--plugin-dir".into(),
        ];
        argv.push(self.root.clone().into());
        if self.opts.mode == Mode::Headless {
            argv.push("--print".into());
        }
        argv
    }

    pub fn plan(&self) {
        let t = "<tmp>/lh e2e XXXX";
        let cli = self.bin.display().to_string();
        let join = |argv: &[&str]| common::shlex_join(&common::os_vec(argv));
        say!("Plan (dry run; nothing is executed):");
        if !self.opts.skip_push_probe {
            say!("0. Step 1, Codex push probe (tests/live/push_probe.rs) into the same folder:");
            push_probe::Probe::new(&self.out, false).plan();
        }
        say!("{}", fixture::plan_line(t, self.opts.interrupt));
        say!(
            "   env: LEADHAND_CONFIG='{t}/user config.toml' (empty); every command runs from '{t}/elsewhere'"
        );
        say!(
            "2. versions: claude --version; codex --version{}",
            if self.opts.mode == Mode::Tmux {
                "; tmux -V"
            } else {
                ""
            }
        );
        let (deck, repo) = (
            format!("{t}/plans/fixture deck.md"),
            format!("{t}/fixture repo"),
        );
        for argv in [
            vec![cli.as_str(), "check", &deck, "--repo", &repo],
            vec![
                cli.as_str(),
                "route",
                &deck,
                "1",
                "implementer",
                "--repo",
                &repo,
            ],
            vec![
                cli.as_str(),
                "route",
                &deck,
                "2",
                "implementer",
                "--repo",
                &repo,
            ],
            vec![cli.as_str(), "start", &deck, "--repo", &repo],
        ] {
            say!("   {}", join(&argv));
        }
        let root = self.root.display().to_string();
        let first = vec![
            cli.as_str(),
            "run",
            "<run dir>/deck.md",
            "--plugin-dir",
            root.as_str(),
        ];
        if self.opts.mode == Mode::Headless {
            let mut a = first.clone();
            a.push("--print");
            say!("3. launch (headless): {}", join(&a));
            say!(
                "   -> claude ... --permission-mode auto -p --output-format stream-json --verbose '<first message>'"
            );
        } else {
            say!(
                "3. launch (tmux): tmux -L {s} ... new-session -d -s {s}-1 -c '{t}/elsewhere' -x 200 -y 50 env -u CLAUDECODE {}",
                join(&first),
                s = self.tmux_sock
            );
            say!(
                "   dialogs: trust dialog answered with Down, then Enter once 'Yes, I trust' is highlighted"
            );
        }
        say!(
            "4. poll every {POLL_S:.0}s: status.json headline, events.jsonl; budget {} min",
            self.budget_s / 60
        );
        say!(
            "   session exits without an outcome -> record open dispatches, relaunch with `{cli} launch-line <run dir>`{}",
            if self.opts.mode == Mode::Headless {
                " + --print"
            } else {
                ""
            }
        );
        say!(
            "5. after: {cli} status <run dir>; {cli} report <run dir> (only if report.md is missing)"
        );
        if self.opts.interrupt {
            say!(
                "   interrupted run: fixture card 2 gets more work (fixture::make_fixture slow_card2); triggers poll every {INTERRUPT_POLL_S}s"
            );
            say!(
                "   a. card 2's Codex implementer live (workers/<id>/runner.json codex pid + start match):"
            );
            say!(
                "      SIGKILL that codex pid at its first item.completed or {KILL_AFTER_FIRST_STEP_S:.0}s after; if it already finished: record 'too fast to interrupt'"
            );
            say!(
                "   b. card 2's next attempt running (worker_started; Codex: its runner's codex live):"
            );
            say!(
                "      run `{cli} launch-line <run dir>` (+ --print) while the supervisor lives -> must exit 3;"
            );
            say!(
                "      lsof the run lock; SIGKILL holder.json's supervisor pid (start-time checked)"
            );
            say!(
                "   c. the relaunch loop relaunches via launch-line, retrying refusals (exit 3) every {REFUSED_RETRY_S:.0}s; fast polling continues {FAST_AFTER_KILL_S:.0}s after the kill"
            );
        }
        say!("6. checks:");
        let mut docs: Vec<(&str, &str)> = CHECK_DOCS.to_vec();
        if adds_launch_cli_check(&self.bin, &self.root, false) {
            docs.push(LAUNCH_CLI_DOC);
        }
        if self.opts.interrupt {
            docs.extend(INTERRUPT_CHECK_DOCS);
        }
        for (name, how) in docs {
            say!("   - {name}: {how}");
        }
        say!(
            "   (the CLI calls are checked against the CLI path the launch reports; LEADHAND_BIN is {cli}, the platform binary {}; when the launch reports none, `{}` FAILs and the platform-binary check is added)",
            platform_binary(&self.root).display(),
            LAUNCH_CLI_MISSING_DOC.0
        );
        say!(
            "7. write {} and {}/; stop sessions, runners and tmux; remove the temp dir",
            self.raw.display(),
            self.art.display()
        );
    }

    // ------------------------------------------------------------------ phases

    fn preflight(&mut self) -> bool {
        let mut wanted = vec!["claude", "codex", "git"];
        if self.opts.mode == Mode::Tmux {
            wanted.push("tmux");
        }
        let missing: Vec<&str> = wanted
            .into_iter()
            .filter(|b| common::which(b).is_none())
            .collect();
        self.facts.set(
            "binaries missing",
            or_none(missing.iter().map(|b| Json::from(*b)).collect()),
        );
        let cli = self.bin.display().to_string();
        if cli.chars().any(char::is_whitespace) {
            self.facts.set("cli path has whitespace", cli);
        }
        for (name, argv) in [
            ("claude", ["claude", "--version"]),
            ("codex", ["codex", "--version"]),
            ("tmux", ["tmux", "-V"]),
            ("git", ["git", "--version"]),
        ] {
            if common::which(argv[0]).is_some() {
                let r = self.cmd(
                    &argv.map(OsString::from),
                    Some(&self.out.clone()),
                    CMD_TIMEOUT,
                    true,
                );
                let text = if r.stdout.is_empty() {
                    &r.stderr
                } else {
                    &r.stdout
                };
                self.facts.set(format!("version {name}"), text.trim());
            }
        }
        self.facts.set(
            "platform",
            match std::env::consts::OS {
                "macos" => "darwin",
                other => other,
            },
        );
        let user_cfg = push_probe::home().join(".config/leadhand/config.toml");
        self.facts.set(
            "operator leadhand user config exists (not read)",
            user_cfg.exists(),
        );
        missing.is_empty()
    }

    fn safe_setup(&mut self) -> bool {
        match catch(|| self.setup()) {
            Ok(ok) => ok,
            Err(e) => {
                self.errors.push(format!("setup: {e}"));
                self.stop_reason = "setup failed".to_string();
                false
            }
        }
    }

    fn setup(&mut self) -> Result<bool, String> {
        let tmp = tempfile::Builder::new()
            .prefix("lh e2e ")
            .tempdir()
            .map_err(|e| format!("making the temp dir: {e}"))?
            .keep();
        self.tmp = Some(tmp.clone());
        let fx = fixture::make_fixture(&tmp, self.opts.interrupt)?;
        let mut env = live_env();
        env.retain(|(k, _)| k != "LEADHAND_CONFIG");
        env.push(("LEADHAND_CONFIG".into(), fx.user_config.clone().into()));
        self.env = env;
        write_file(
            &self.art.join("fixture.json"),
            &json::dumps_indent(&fx.to_json(), 2),
        );
        self.fx = Some(fx);
        self.origin_refs_before = self.git(&["for-each-ref"], Some(&self.fx().origin.clone()));
        let (deck, repo): (OsString, OsString) =
            (self.fx().deck.clone().into(), self.fx().repo.clone().into());
        let bin = self.bin_os();
        let argv = |args: &[&OsStr]| -> Vec<OsString> {
            let mut v = vec![bin.clone()];
            v.extend(args.iter().map(|a| a.to_os_string()));
            v
        };
        let r = self.cmd(
            &argv(&[OsStr::new("check"), &deck, OsStr::new("--repo"), &repo]),
            None,
            CMD_TIMEOUT,
            true,
        );
        self.facts.set(
            "leadhand check",
            format!(
                "exit {}: {}",
                show(&code_json(&r)),
                short_text(r.stdout.trim(), 300)
            ),
        );
        for card in ["1", "2"] {
            let r = self.cmd(
                &argv(&[
                    OsStr::new("route"),
                    &deck,
                    OsStr::new(card),
                    OsStr::new("implementer"),
                    OsStr::new("--repo"),
                    &repo,
                ]),
                None,
                CMD_TIMEOUT,
                true,
            );
            let text = if r.stdout.trim().is_empty() {
                r.stderr.trim()
            } else {
                r.stdout.trim()
            };
            self.facts.set(
                format!("route card {card} implementer"),
                short_text(text, 300),
            );
        }
        let r = self.cmd(
            &argv(&[OsStr::new("start"), &deck, OsStr::new("--repo"), &repo]),
            None,
            CMD_TIMEOUT,
            true,
        );
        let run_line = r
            .stdout
            .split('\n')
            .find_map(|l| l.strip_prefix("run: ").filter(|rest| !rest.is_empty()))
            .map(|rest| rest.trim().to_string());
        let text = if r.stdout.trim().is_empty() {
            r.stderr.trim()
        } else {
            r.stdout.trim()
        };
        self.facts.set(
            "leadhand start",
            format!("exit {}: {}", show(&code_json(&r)), short_text(text, 300)),
        );
        let Some(run) = run_line.filter(|_| matches!(r.code, Ok(0))) else {
            self.errors.push("leadhand start failed".to_string());
            return Ok(false);
        };
        let run_dir = PathBuf::from(run);
        let meta = read_json(&run_dir.join("run.json"));
        self.facts.set("run dir", run_dir.display().to_string());
        self.facts
            .set("run branch", member(&meta, "branch").clone());
        self.facts
            .set("workspace", member(&meta, "workspace").clone());
        self.run_dir = Some(run_dir);
        Ok(true)
    }

    fn launch(&mut self, argv: Vec<OsString>, how: &str) -> Result<(), String> {
        let n = self.sessions.len() + 1;
        let seq = self
            .events()
            .iter()
            .map(|e| num_or0(member(e, "seq")) as i64)
            .max()
            .unwrap_or(0);
        let rec = SessionRecord {
            n,
            how: how.to_string(),
            exit_code: Json::Null,
            started_at: 0.0,
            started_utc: String::new(),
            exited_at: None,
            exited_utc: None,
            seq_at_start: seq,
            stream_meta: Vec::new(),
            notes: Facts::default(),
            dir: self.art.join(format!("sessions/session-{n}")),
        };
        let elsewhere = self.fx().elsewhere.clone();
        let s = match self.opts.mode {
            Mode::Headless => {
                Session::start_headless(rec, self.test, &argv, &elsewhere, &self.env)?
            }
            Mode::Tmux => Session::start_tmux(rec, &argv, &elsewhere, &self.env, &self.tmux_sock)?,
        };
        self.commands.borrow_mut().push(CommandRecord {
            argv: common::shlex_join(&argv),
            code: Json::from("(session)"),
            cwd: elsewhere.display().to_string(),
        });
        self.sessions.push(s);
        self.log(&format!(
            "session {n} started ({how}): {}",
            common::shlex_join(&argv)
        ));
        Ok(())
    }

    fn relaunch_argv(&mut self) -> Option<Vec<OsString>> {
        let argv = vec![self.bin_os(), "launch-line".into(), self.run_dir().into()];
        let r = self.cmd(&argv, None, CMD_TIMEOUT, true);
        let line = r.stdout.trim().to_string();
        if !matches!(r.code, Ok(0)) || line.is_empty() {
            self.errors
                .push(format!("launch-line failed: {}", r.stderr.trim()));
            return None;
        }
        let words =
            common::shlex_split(&line).unwrap_or_else(|| panic!("launch-line printed {line:?}"));
        let mut argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        self.facts.push_to("launch-line outputs", line.as_str());
        if !words.iter().any(|w| w == "--plugin-dir") {
            // holder.json records the plugin dir only once a launch got that far.
            self.facts.push_to(
                "launch-line without --plugin-dir (driver added it)",
                line.as_str(),
            );
            argv.push("--plugin-dir".into());
            argv.push(self.root.clone().into());
        }
        if self.opts.mode == Mode::Headless {
            argv.push("--print".into());
        }
        Some(argv)
    }

    fn sample_status(&mut self, force: bool) {
        let st = read_json(&self.run_dir().join("status.json"));
        let Some(headline) = st.get("headline").cloned() else {
            return;
        };
        if headline.is_null() {
            return;
        }
        let now_t = now();
        let last = self.status_samples.last();
        let changed = last.is_none_or(|l| member(l, "headline") != &headline);
        let stale = last
            .is_some_and(|l| now_t - member(l, "t").as_f64().unwrap_or(0.0) >= STATUS_KEEP_EVERY_S);
        if force || changed || stale {
            let sample = obj! {
                "t" => round_to(now_t, 3), "utc" => utc_now(), "elapsed_s" => round_to(now_t - self.t0, 1),
                "headline" => headline.clone(), "activity" => member(&st, "activity").clone(),
            };
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.art.join("status-samples.jsonl"))
                .unwrap_or_else(|e| panic!("opening the status samples: {e}"));
            let _ = writeln!(f, "{}", json::dumps(&sample));
            self.status_samples.push(sample);
            if changed {
                self.log(&format!("status: {}", py_str(&headline)));
            }
        }
    }

    fn session_ids(&self) -> Vec<String> {
        let run = self
            .run_dir
            .as_ref()
            .map(|d| read_json(&d.join("run.json")));
        let mut ids: Vec<String> = run
            .as_ref()
            .and_then(|r| r.get("supervisor_sessions"))
            .and_then(Json::as_array)
            .map(|a| a.iter().map(|x| py_text(Some(x))).collect())
            .unwrap_or_default();
        for s in &self.sessions {
            if let Some(Json::Arr(more)) = s.rec.notes.get("session_ids") {
                for sid in more {
                    let sid = py_text(Some(sid));
                    if !ids.contains(&sid) {
                        ids.push(sid);
                    }
                }
            }
        }
        ids
    }

    fn transcripts(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        transcripts(&self.files, &self.session_ids())
    }

    /// Whether a `leadhand next` result in the session's stream or transcript was `done`.
    fn engine_said_done(&self, session: &SessionRecord) -> bool {
        let mut texts = Vec::new();
        let stream = session.dir.join("stream.jsonl");
        if stream.exists() {
            texts.push(fs::read(&stream).unwrap_or_default());
        }
        for p in self.transcripts().0 {
            texts.push(fs::read(&p).unwrap_or_default());
        }
        texts
            .iter()
            .any(|t| engine_done(&String::from_utf8_lossy(t)))
    }

    fn cur(&mut self) -> &mut Session {
        self.sessions.last_mut().expect("a session")
    }

    fn drive(&mut self) -> Result<(), String> {
        let deadline = self.t0 + self.budget_s as f64;
        let first = self.launch_argv_first();
        self.launch(first, "first launch")?;
        let mut no_progress = 0u32;
        loop {
            if operator_interrupted() {
                return Ok(());
            }
            let now_t = now();
            let events = self.events();
            self.sample_status(false);
            self.cur().poll();
            if let Some(mut i) = self.interrupter.take() {
                if let Err(e) = catch(|| {
                    i.call(self, &events);
                    Ok(())
                }) {
                    self.errors.push(format!("poll hook: {}", last_line(&e)));
                }
                self.interrupter = Some(i);
            }
            let outcome = events.iter().rev().find(|e| kind(e) == "outcome").cloned();
            if let Some(o) = &outcome
                && self.outcome_seen_at.is_none()
            {
                self.outcome_seen_at = Some(now_t);
                let q = member(o, "qualifier");
                let q = if truthy(q) { py_str(q) } else { String::new() };
                self.log(format!("outcome: {} {q}", py_str(member(o, "outcome"))).trim_end());
            }
            let alive = self.cur().alive();
            if let Some(o) = &outcome {
                let reported = events.iter().any(|e| kind(e) == "report_written");
                let seen = self.outcome_seen_at.unwrap();
                if !alive
                    || now_t - seen > AFTER_OUTCOME_GRACE_S
                    || (self.opts.mode == Mode::Tmux && reported && now_t - seen > 20.0)
                {
                    self.stop_reason = format!("outcome {}", py_str(member(o, "outcome")));
                    break;
                }
            } else if !alive {
                let run_dir = self.run_dir().to_path_buf();
                let s = &mut self.cur().rec;
                analyze_exit(s, &events, &run_dir, now());
                let (n, code) = (s.n, s.exit_code.clone());
                let open = s
                    .notes
                    .get("dispatches_open_at_exit")
                    .cloned()
                    .unwrap_or(Json::Null);
                self.log(&format!(
                    "session {n} exited (code {}) without an outcome; open dispatches: {}",
                    py_str(&code),
                    if matches!(open, Json::Str(_)) {
                        py_str(&open)
                    } else {
                        py_repr(&open)
                    }
                ));
                if now_t >= deadline {
                    self.stop_reason = "budget ended".to_string();
                    break;
                }
                if eq_num(&code, EXIT_REFUSED) {
                    // The run lock is still held (an earlier supervisor lives): not a lack of
                    // progress.
                    self.cur().rec.notes.set("refused", true);
                    let utc = opt(self.cur().rec.exited_utc.clone());
                    let holders = lock_holders(&run_dir.join("run.lock"));
                    self.facts.push_to(
                        "refused launches (exit 3, lock held)",
                        obj! {"session" => n, "utc" => utc, "lock holders" => holders},
                    );
                    if !sleep_s(REFUSED_RETRY_S) {
                        return Ok(());
                    }
                    let Some(argv) = self.relaunch_argv() else {
                        self.stop_reason = "launch-line failed".to_string();
                        break;
                    };
                    self.launch(argv, "relaunch after refusal (launch-line)")?;
                    continue;
                }
                let rec = self.cur().rec.clone();
                if self.engine_said_done(&rec) {
                    self.cur()
                        .rec
                        .notes
                        .set("engine_done_without_outcome", true);
                    self.stop_reason =
                        "the engine returned a `done` action but no outcome event was written; not relaunching"
                            .to_string();
                    break;
                }
                no_progress = if rec.notes.get("events_added").is_some_and(truthy) {
                    0
                } else {
                    no_progress + 1
                };
                if no_progress >= NO_PROGRESS_LIMIT {
                    self.stop_reason = format!(
                        "{no_progress} sessions in a row added no event; stopped relaunching"
                    );
                    break;
                }
                if self.sessions.len() >= MAX_SESSIONS {
                    self.stop_reason =
                        format!("{MAX_SESSIONS} sessions started; stopped relaunching");
                    break;
                }
                if !sleep_s(if no_progress == 0 { 5.0 } else { 20.0 }) {
                    return Ok(());
                }
                let Some(argv) = self.relaunch_argv() else {
                    self.stop_reason = "launch-line failed".to_string();
                    break;
                };
                self.launch(argv, "relaunch (launch-line)")?;
                continue;
            }
            if now_t >= deadline {
                self.stop_reason = "budget ended".to_string();
                break;
            }
            let fast = self.interrupter.as_ref().is_some_and(Interrupter::fast);
            if !sleep_s(if fast { INTERRUPT_POLL_S } else { POLL_S }) {
                return Ok(());
            }
        }
        self.log(&format!("stopping: {}", self.stop_reason));
        self.sample_status(true);
        Ok(())
    }

    fn stop_everything(&mut self) {
        let mut errors = Vec::new();
        for s in &mut self.sessions {
            let n = s.rec.n;
            if let Err(e) = catch(|| s.stop()) {
                errors.push(format!("stop session {n}: {}", last_line(&e)));
            }
        }
        self.errors.extend(errors);
        if self.opts.mode == Mode::Tmux {
            let argv: Vec<OsString> = ["tmux", "-L", &self.tmux_sock, "kill-server"]
                .map(OsString::from)
                .to_vec();
            let _ = push_probe::run(
                &argv,
                &common::process_env(),
                Path::new("/"),
                None,
                Duration::from_secs(30),
            );
        }
        let Some(run_dir) = self.run_dir.clone() else {
            return;
        };
        let mut stopped = Vec::new();
        for d in sorted_entries(&run_dir.join("workers")) {
            if d.join("result.json").exists() {
                continue;
            }
            let argv = vec![
                self.bin_os(),
                "stop-dispatch".into(),
                run_dir.clone().into(),
                "--dispatch".into(),
                d.file_name().unwrap().to_os_string(),
            ];
            let r = self.cmd(&argv, None, Duration::from_secs(120), true);
            stopped.push(Json::from(format!(
                "{}: exit {}",
                file_name(&d),
                show(&code_json(&r))
            )));
            for (role, pid) in
                kill_runner_processes(&d.join("runner.json"), Signal::KILL, &["codex", "runner"])
            {
                stopped.push(Json::from(format!(
                    "{}: SIGKILL {role} {pid}",
                    file_name(&d)
                )));
            }
        }
        self.facts
            .set("runners stopped by the driver", or_none(stopped));
    }

    fn finish_run(&mut self) -> Result<(), String> {
        let run_dir = self.run_dir().to_path_buf();
        let r = self.cmd(
            &[self.bin_os(), "status".into(), run_dir.clone().into()],
            None,
            CMD_TIMEOUT,
            true,
        );
        write_file(
            &self.art.join("leadhand-status.txt"),
            &format!("{}{}", r.stdout, r.stderr),
        );
        let report = run_dir.join("report.md");
        self.facts
            .set("report.md written by the run", report.exists());
        if !report.exists() {
            let r = self.cmd(
                &[self.bin_os(), "report".into(), run_dir.into()],
                None,
                CMD_TIMEOUT,
                true,
            );
            let text = if r.stdout.trim().is_empty() {
                r.stderr.trim()
            } else {
                r.stdout.trim()
            };
            self.facts.set(
                "leadhand report (driver)",
                format!("exit {}: {}", show(&code_json(&r)), short_text(text, 300)),
            );
        }
        Ok(())
    }

    fn collect(&mut self) -> Result<(), String> {
        let run_dir = self.run_dir().to_path_buf();
        if run_dir.is_dir() {
            copytree(&run_dir, &self.art.join("run-dir"))?;
        }
        let (mains, subs) = self.transcripts();
        let tdir = self.art.join("transcripts");
        fs::create_dir_all(&tdir).map_err(|e| e.to_string())?;
        for p in &mains {
            copy2(p, &tdir.join(p.file_name().unwrap()))?;
        }
        for p in &subs {
            let sid = p
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .unwrap();
            let d = tdir.join(sid).join("subagents");
            fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            copy2(p, &d.join(p.file_name().unwrap()))?;
        }
        let found: Vec<String> = mains
            .iter()
            .chain(&subs)
            .map(|p| p.display().to_string())
            .collect();
        self.facts.set(
            "transcripts found",
            if found.is_empty() {
                Json::from("none")
            } else {
                strs(&found)
            },
        );
        let cdir = self.art.join("codex-rollouts");
        let mut found = Vec::new();
        for a in read_jsonl(&run_dir.join("attempts.jsonl")) {
            if s(&a, "vendor") == Some("codex")
                && truthy(member(&a, "worker_id"))
                && let Some(p) = self.files.codex(&py_text(a.get("worker_id")))
            {
                fs::create_dir_all(&cdir).map_err(|e| e.to_string())?;
                copy2(&p, &cdir.join(p.file_name().unwrap()))?;
                found.push(p.display().to_string());
            }
        }
        self.facts.set(
            "codex rollouts found",
            if found.is_empty() {
                Json::from("none")
            } else {
                strs(&found)
            },
        );
        let gdir = self.art.join("git");
        fs::create_dir_all(&gdir).map_err(|e| e.to_string())?;
        let branch = py_text(read_json(&run_dir.join("run.json")).get("branch"));
        let range = format!("{}..refs/heads/{branch}", self.fx().base_sha);
        let origin = self.fx().origin.clone();
        for (name, args, repo) in [
            (
                "log-all.txt",
                vec!["log", "--all", "--graph", "--format=%h %d %s"],
                None,
            ),
            ("run-branch.diff", vec!["diff", range.as_str()], None),
            ("refs.txt", vec!["for-each-ref"], None),
            (
                "origin-refs-after.txt",
                vec!["for-each-ref"],
                Some(origin.as_path()),
            ),
        ] {
            write_file(&gdir.join(name), &format!("{}\n", self.git(&args, repo)));
        }
        write_file(
            &gdir.join("origin-refs-before.txt"),
            &format!("{}\n", self.origin_refs_before),
        );
        Ok(())
    }

    /// The CLI path the launch reported, where it came from, and every path reported (S3-R14).
    fn launch_cli(&self) -> (String, String, Json, bool) {
        let lines: Vec<String> = match self.facts.get("launch-line outputs") {
            Some(Json::Arr(lines)) => lines.iter().map(|l| py_text(Some(l))).collect(),
            _ => Vec::new(),
        };
        launch_cli(&self.transcripts().0, &lines, &self.bin)
    }

    fn evidence<'a>(&'a self, repo: &'a dyn Repo) -> Evidence<'a> {
        let (cli, cli_source, _, cli_fell_back) = self.launch_cli();
        Evidence {
            run_dir: self.run_dir().to_path_buf(),
            files: self.files.clone(),
            repo,
            bin: self.bin.clone(),
            cli,
            cli_source,
            cli_fell_back,
            root: self.root.clone(),
            mode: self.opts.mode,
            sessions: self.sessions.iter().map(|s| s.rec.clone()).collect(),
            session_ids: self.session_ids(),
            status_samples: self.status_samples.clone(),
            facts: self.facts.clone(),
            stop_reason: self.stop_reason.clone(),
            interrupt: self.interrupter.as_ref().map(|i| i.rec.clone()),
            t0: self.t0,
            now: now(),
            origin_refs_before: self.origin_refs_before.clone(),
        }
    }

    fn run_checks(&mut self) -> Result<(), String> {
        let (cli, source, all, _) = self.launch_cli();
        self.facts.set(
            "launch CLI path (checked against the supervisor's Bash calls)",
            cli,
        );
        self.facts.set("launch CLI path from", source);
        self.facts.set("launch CLI paths reported", all);
        let checks = {
            let repo = LiveRepo { d: self };
            run_checks(&self.evidence(&repo))
        };
        self.checks = checks;
        Ok(())
    }

    // ------------------------------------------------------------------ output

    fn write_raw(&self, started: &str, push_summary: Option<&[String]>) {
        let tmp = self
            .tmp
            .as_ref()
            .map_or("None".to_string(), |t| t.display().to_string());
        let launch = match (
            self.facts
                .get("launch CLI path (checked against the supervisor's Bash calls)"),
            self.facts.get("launch CLI path from"),
        ) {
            (Some(p), Some(from)) => format!("{} ({})", show(p), show(from)),
            _ => "not determined (no checks ran)".to_string(),
        };
        let mut out = vec![
            "# Task 20: first complete run, end to end — raw results".to_string(),
            String::new(),
            format!("- started: {started}"),
            format!("- finished: {}", utc_now()),
            format!("- mode: {}", self.opts.mode.as_str()),
            format!("- budget: {} min", self.budget_s / 60),
            format!(
                "- stop reason: {}",
                if self.stop_reason.is_empty() {
                    "n/a"
                } else {
                    &self.stop_reason
                }
            ),
            format!(
                "- temp dir: {tmp} ({})",
                if self.opts.keep { "kept" } else { "removed" }
            ),
            format!("- plugin dir: {}", self.root.display()),
            format!("- cli: {}", self.bin.display()),
            format!("- launch cli: {launch}"),
            String::new(),
        ];
        if let Some(summary) = push_summary {
            out.extend(
                [
                    "## Step 1: Codex push probe",
                    "",
                    "See push-probe-raw.md.",
                    "",
                ]
                .map(String::from),
            );
            out.extend(summary.iter().map(|s| format!("- {s}")));
            out.push(String::new());
        }
        out.extend(["## Setup facts".to_string(), String::new()]);
        out.extend(self.facts.lines());
        out.extend([
            String::new(),
            "## Commands".to_string(),
            String::new(),
            "```".to_string(),
        ]);
        for c in self.commands.borrow().iter() {
            out.push(format!(
                "(exit {}) (cwd={}) {}",
                show(&c.code),
                c.cwd,
                c.argv
            ));
        }
        out.extend(["```".to_string(), String::new()]);
        if !self.errors.is_empty() {
            out.extend(["## Driver errors".to_string(), String::new()]);
            out.extend(self.errors.iter().map(|e| format!("- {e}")));
            out.push(String::new());
        }
        out.extend(checks_markdown(&self.checks));
        write_file(&self.raw, &out.join("\n"));
    }

    /// Runs everything; returns what failed (FAIL checks, and the push probe's FAIL or
    /// REFUSED sections).
    pub fn run(&mut self) -> Vec<String> {
        for d in [&self.out, &self.art] {
            fs::create_dir_all(d).unwrap_or_else(|e| panic!("making {}: {e}", d.display()));
        }
        push_probe::watch_operator_interrupt();
        let started = utc_now();
        let mut push_summary: Option<Vec<String>> = None;
        if !self.opts.skip_push_probe {
            self.log("Step 1: Codex push probe");
            let out = self.out.clone();
            push_summary = Some(match catch(|| Ok(push_probe::run_probe(&out))) {
                Ok(summary) => summary,
                Err(e) => vec![format!("FAIL     push probe crashed: {}", last_line(&e))],
            });
        }
        if !operator_interrupted() {
            self.log("Step 2: end-to-end run");
            self.t0 = now();
            let ready = match catch(|| Ok(self.preflight())) {
                Ok(ready) => ready,
                Err(e) => {
                    self.errors.push(format!("preflight: {e}"));
                    self.stop_reason = "preflight failed".to_string();
                    false
                }
            };
            if ready && self.safe_setup() {
                if let Err(e) = catch(|| self.drive()) {
                    self.errors.push(format!("drive: {e}"));
                    if self.stop_reason.is_empty() {
                        self.stop_reason = "driver error".to_string();
                    }
                }
                self.stop_everything();
                if !operator_interrupted() {
                    // Again, now that every later result is known.
                    let events = self.events();
                    let run_dir = self.run_dir().to_path_buf();
                    for s in &mut self.sessions {
                        s.alive();
                        analyze_exit(&mut s.rec, &events, &run_dir, now());
                    }
                    type Phase = fn(&mut Driver) -> Result<(), String>;
                    let phases: [(&str, Phase); 3] = [
                        ("finish_run", Driver::finish_run),
                        ("collect", Driver::collect),
                        ("run_checks", Driver::run_checks),
                    ];
                    for (name, phase) in phases {
                        if let Err(e) = catch(|| phase(self)) {
                            self.errors.push(format!("{name}: {e}"));
                        }
                    }
                }
            }
        }
        if operator_interrupted() {
            self.errors
                .push("interrupted by the operator (Ctrl-C)".to_string());
            if self.stop_reason.is_empty() {
                self.stop_reason = "operator interrupt".to_string();
            }
            self.stop_everything();
        }
        self.write_raw(&started, push_summary.as_deref());
        if let Some(tmp) = &self.tmp
            && !self.opts.keep
        {
            let _ = fs::remove_dir_all(tmp);
        }
        say!("");
        say!("Summary:");
        for s in push_summary.iter().flatten() {
            say!("  push probe: {s}");
        }
        for c in &self.checks {
            say!("  {:8} {}", c.verdict, c.name);
        }
        say!("raw results: {}", self.raw.display());
        let mut failed: Vec<String> = push_summary
            .iter()
            .flatten()
            .filter(|s| s.starts_with("FAIL") || s.starts_with("REFUSED"))
            .map(|s| format!("push probe: {s}"))
            .collect();
        failed.extend(
            self.checks
                .iter()
                .filter(|c| c.verdict == "FAIL")
                .map(|c| format!("{:8} {}", c.verdict, c.name)),
        );
        if self.checks.is_empty() {
            failed.push(format!(
                "no check ran (stop reason: {}; driver errors: {})",
                if self.stop_reason.is_empty() {
                    "n/a"
                } else {
                    &self.stop_reason
                },
                self.errors
                    .iter()
                    .map(|e| last_line(e))
                    .collect::<Vec<_>>()
                    .join(" / ")
            ));
        }
        failed
    }
}
