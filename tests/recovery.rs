//! Recovery after a relaunch, pause and cancel, and the crash matrix: every crash point at
//! every occurrence, a restart, and the same final state.
//!
//! The Rust form of `tests/contract/contract_recovery.py` (converted from tests/test_recovery.py
//! and tests/test_engine_recovery.py). `FakeSupervisor` plays the run skill. Where a Python
//! test appended `resumed` itself (its `resume`), the run is relaunched with `run` (the fake
//! `claude` as Claude, which exits at once), which records it. Where it called an engine
//! function, this calls the command that does it (`record`, `unpark`, `start`, `timer`, `run`);
//! an EngineError is exit code 1 with `error: ...` on stderr (`assert_fails!`). A crash point
//! is LEADHAND_CRASH_AT on the command, which then exits 97.
//!
//! Recovery is judged from Git, the filesystem and the fakes' logs, beside the binary's events:
//! whether a Codex process or its tool child is gone comes from the fake Codex's log (its pid
//! and the time of the line naming it); a runner is identified by the record in runner.json
//! once this test has seen that pid alive with that start time. Branches, commits and trees
//! are read from Git. Waits poll a condition with a bound; a check that something does not
//! happen watches for it over a stated window.
//!
//! The branch release (deliberate change 2, ruling S1-R15): a run that ends complete,
//! incomplete or failed may record more events after `outcome` and detaches its worktree, so
//! these tests check the events they care about, never a complete list, and branch tips, never
//! which branch the worktree has checked out.
//!
//! **The crash matrix** (`every_crash_point`, ruling S3-R23) keeps the Python's shape with the
//! drive helper (`common::drive`): the test starts one worker per (crash point, occurrence), at
//! most `parallel()` at a time, each a re-executed test binary in a process group of its own
//! with a deadline. A worker forks its crash child (`FakeSupervisor::fork_noting`), again its
//! own process group, which drives the world with LEADHAND_CRASH_AT, _SKIP, _COUNTER and
//! _KILL_PGID until the crash point kills the group; the worker then restarts the run and
//! checks the final state. The test branches on `drive::selected()` first, so each child goes
//! straight to its own part. `StableSupervisor`, the matrix's world, is `common::recovery`'s
//! (`tests/rust_cli.rs` drives it too).
//!
//! **Wall clock** (ruling S3-R19). The tests whose assertions rest on the binary's stop ladder
//! (a Codex runner or Codex stopped, with the 5 s acknowledgement and the stale-heartbeat second
//! look) and the crash matrix, which loads the machine with its parallel workers, hold
//! `common::serial()` for their whole run, each naming its group in a comment.
//!
//! RunnerHelpers' Python started its test sleepers and the runner-lock holder with the Python
//! interpreter: here the sleepers are `sleep`, each in a process group of its own (Python's
//! `start_new_session`), and the test holds runner.lock's flock itself (`hold_lock`), as the
//! holder process did.

#[macro_use]
mod common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use common::dispatch_world::{WAIT, alive, logged_alive, wait_for, wait_for_with, when};
use common::drive;
use common::engine::{
    CARD1, CODEX_CARD, RUN, codex_world, gated, recorded_with, result, run_line, seq, status_lines,
    with_status, with_verdict,
};
use common::git::{commit, run_git, write};
use common::json::{self, Json};
use common::recovery::{
    DATES, codex_alive, codex_log, codex_starts, drive as stable_drive, final_state, relaunch,
    resume, resume_as, stable_hooks, stable_supervisor, wait_for_orphans,
};
use common::rundir::{append_event, outcome};
use common::supervisor::{
    CardOpts, CliOpts, FakeSupervisor, NO_HOOKS, Options, Proc, Step, card, card_with,
    kill_run_leftovers, os_path, run_dir, text, truthy, wait_timeout,
};
use common::{
    Out, TIMEOUT, TempRoot, field, hold_lock, holds, json_in, jv, lines, lockable, one, patch_env,
    pick, py_str, read_json, read_jsonl, read_text, same_process, set_mtime, start_time, strs,
    walk, write_text,
};

const SPENT: &str = "Status: SPENT (time limit)";

/// A JSON list of the values given (each a `Json`, or what `Json::from` takes).
macro_rules! ja {
    ($($x:expr),* $(,)?) => {
        Json::Arr(vec![$(Json::from($x)),*])
    };
}

/// A refusal: exit 1, `error: ...` on stderr, naming each needle (`fails`).
macro_rules! assert_fails {
    ($r:expr $(, $needle:expr)* $(,)?) => {{
        let r: Out = $r;
        assert_eq!(r.code, 1, "{r}");
        let stderr = r.stderr_text();
        assert!(stderr.starts_with("error: "), "{r}");
        $(
            let needle: &str = &$needle;
            assert!(stderr.contains(needle), "{needle:?} not in stderr\n{r}");
        )*
    }};
}

/// runner.json once it names Codex; its runner is checked here to be that live process, so a
/// later `alive(...)` of it is judged against this test's own sighting (`running`). Gives the
/// record.
macro_rules! assert_running {
    ($w:expr, $a:expr, $p:expr) => {{
        let (w, a, p): (&FakeSupervisor, &Json, &Proc) = (&$w, &$a, &$p);
        let did = text(&a["dispatch_id"]).to_string();
        let rec = wait_for_with("Codex to start", WAIT, Some(p), || {
            let r = runner_record(w, &did);
            r.get("codex").is_some_and(truthy).then_some(r)
        });
        assert!(alive(&rec["runner"]), "{rec}");
        rec
    }};
}

// --------------------------------------------------------------------------
// Helpers

/// `card(1, route=CODEX_CARD)`.
fn codex_card() -> String {
    card_with(
        1,
        CardOpts {
            route: Some(CODEX_CARD),
            ..CardOpts::default()
        },
    )
}

/// `card(1)` .. `card(n)` (`card_list(n)`).
fn card_list(n: i64) -> Vec<String> {
    (1..=n).map(card).collect()
}

/// A Codex card 1 world with the supervisor session "supervisor-1", started.
#[track_caller]
fn started_codex_world() -> FakeSupervisor {
    let mut w = codex_world(&[codex_card()], Options::default());
    w.session("supervisor-1");
    w.start();
    w
}

/// A world with `cards` and the supervisor session "supervisor-1", started.
#[track_caller]
fn session_world(cards: &[String]) -> FakeSupervisor {
    let mut w = FakeSupervisor::new(cards);
    w.session("supervisor-1");
    w.start();
    w
}

/// The worker folder of the dispatch `did`.
fn wd(w: &FakeSupervisor, did: &str) -> PathBuf {
    run_dir(w).join("workers").join(did)
}

/// runner.json of the dispatch, or `{}` while it is missing or not JSON (`runner_record`).
fn runner_record(w: &FakeSupervisor, did: &str) -> Json {
    match fs::read(wd(w, did).join("runner.json"))
        .ok()
        .map(|b| json::try_parse(&b))
    {
        Some(Ok(v)) => v,
        _ => Json::object([]),
    }
}

/// `{"type": "thread.started", "thread_id": "{thread}"}` and `{"type": "turn.started"}`.
fn thread_and_turn() -> Vec<Json> {
    vec![
        jv(r#"{"type": "thread.started", "thread_id": "{thread}"}"#),
        jv(r#"{"type": "turn.started"}"#),
    ]
}

/// A fake Codex scenario file `<tmp>/scenario <name>.jsonl` with `lines` (`scenario_file`).
#[track_caller]
fn scenario_file(w: &FakeSupervisor, name: &str, lines: &[Json]) -> PathBuf {
    let path = w.tmp.join(format!("scenario {name}.jsonl"));
    let body: String = lines.iter().map(|x| json::dumps(x) + "\n").collect();
    write_text(&path, body);
    path
}

/// Runs dispatch action `a`'s own `dispatch` line in the background, the fake Codex playing
/// `lines`: the proxy, which the test kills when the supervisor session dies (`start_codex`).
#[track_caller]
fn start_codex(w: &mut FakeSupervisor, a: &Json, lines: &[Json]) -> Proc {
    let (argv, mut env) = w.proxy_argv_env(a, &Step::default());
    env.set(
        "FAKE_CODEX_SCENARIO",
        scenario_file(w, text(&a["dispatch_id"]), lines),
    );
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&w.tmp)
        .env_clear()
        .envs(env.vars())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("could not start {argv:?}: {e}"));
    let p = Proc::new(child);
    w.proxies.push(p.clone());
    p
}

/// The supervisor session died, and the proxy with it (the runner is detached)
/// (`kill_proxy`): a kill, then its end within 30 s.
#[track_caller]
fn kill_proxy(p: &Proc) {
    p.kill();
    p.communicate(Duration::from_secs(30))
        .expect("the killed proxy was still running (or its pipes open) 30 s later");
}

/// The fake Codex's first `start` line, once it is there (`first_codex`).
#[track_caller]
fn first_codex(w: &FakeSupervisor) -> Json {
    wait_for("the fake Codex to start", || {
        codex_log(w, Some("start")).into_iter().next()
    })
}

/// Signals the process `rec` names while it is still that process; a process gone in between
/// is no error, any other failure is (`signal_if_alive`, whose `os.kill` raises).
fn try_signal_if_alive(rec: &Json, sig: Signal) -> Result<(), String> {
    if !alive(rec) {
        return Ok(());
    }
    let pid = rec["pid"]
        .as_i64()
        .and_then(|p| i32::try_from(p).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| format!("not a pid: {rec}"))?;
    match kill_process(pid, sig) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(format!("signalling {rec}: {e}")),
    }
}

#[track_caller]
fn signal_if_alive(rec: &Json, sig: Signal) {
    try_signal_if_alive(rec, sig).unwrap_or_else(|e| panic!("{e}"));
}

/// Sends SIGCONT to a stopped runner when dropped (Python's `addCleanup(signal_if_alive,
/// runner, SIGCONT)`); declared after the world, it runs before the world's cleanup.
struct ContinueOnDrop(Json);

impl Drop for ContinueOnDrop {
    fn drop(&mut self) {
        if let Err(e) = try_signal_if_alive(&self.0, Signal::CONT)
            && !thread::panicking()
        {
            panic!("{e}");
        }
    }
}

#[track_caller]
fn git_log_body(repo: &Path, rev: &str) -> String {
    run_git(repo, &["log", "-1", "--format=%B", rev])
}

/// The first `n` characters (Python's `s[:n]`).
fn prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The `card` of each event, as a JSON list (`[e["card"] for e in events]`).
fn cards_of(events: &[Json]) -> Json {
    field(events, "card")
}

/// `next RUN_DIR` with the crash point `point` armed.
#[track_caller]
fn next_crashing(w: &FakeSupervisor, point: &str) -> Out {
    w.cli_with(
        &os_args!["next", run_dir(w)],
        CliOpts {
            crash: Some(point),
            ..CliOpts::default()
        },
    )
}

/// The size of the file at `path` (`path.stat().st_size`, which raises when it is missing).
#[track_caller]
fn size_of(path: &Path) -> u64 {
    fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .len()
}

/// The events of `kind` that have a `found` member.
#[track_caller]
fn with_found(w: &FakeSupervisor) -> Vec<Json> {
    w.events("reconciled")
        .into_iter()
        .filter(|e| e.get("found").is_some())
        .collect()
}

/// The `reconciled` events of `stage`.
#[track_caller]
fn reconciled_at(w: &FakeSupervisor, stage: &str) -> Vec<Json> {
    w.events("reconciled")
        .into_iter()
        .filter(|e| e.get("stage").is_some_and(|s| s == stage))
        .collect()
}

/// `{did: 1}` as `codex_starts` gives it.
fn started_once(did: &str) -> BTreeMap<String, usize> {
    BTreeMap::from([(did.to_string(), 1)])
}

/// A step that crashes the worker at `point` (`{"crash": point}`).
fn crashing(point: &str) -> Step {
    Step {
        crash: Some(point.to_string()),
        ..Step::default()
    }
}

/// `{"status": "BLOCKED", "blocked_by": by}`.
fn blocked(by: &str) -> Step {
    Step {
        status: Some("BLOCKED".into()),
        blocked_by: Some(by.into()),
        ..Step::default()
    }
}

// --------------------------------------------------------------------------
// test_recovery.py

mod reconcile {
    use super::*;

    #[test]
    fn test_restart_after_truncated_final_event_then_append() {
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        w.start();
        let a = w.next();
        w.act(&a);
        // a crash in the middle of an append
        OpenOptions::new()
            .append(true)
            .open(&w.paths().events)
            .and_then(|mut f| {
                f.write_all(br#"{"seq": 99, "ts": "2026-01-01T00:00:00Z", "kind": "accep"#)
            })
            .expect("appending to events.jsonl");
        assert!(!status_lines(&w)[0].is_empty()); // reading never fails
        resume(&mut w);
        let raw = fs::read(&w.paths().events).unwrap();
        let split: Vec<&[u8]> = raw.split(|b| *b == b'\n').collect();
        assert_eq!(split.last().copied(), Some(&b""[..]));
        let parsed: Vec<Json> = split[..split.len() - 1]
            .iter()
            .map(|line| json::parse(line))
            .collect();
        let seqs: Vec<i64> = parsed.iter().map(seq).collect();
        assert_eq!(seqs, (1..=parsed.len() as i64).collect::<Vec<_>>());
        let last = parsed.last().expect("no event");
        assert_eq!(last["kind"], "resumed");
        let done = w.run().pop().unwrap();
        assert_eq!(done["action"], "done");
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1, 2]"));
        let rec = one(w.events("reconciled"));
        assert_json_eq!(rec["resumed_seq"], last["seq"]);
    }

    #[test]
    fn test_appends_repair_a_torn_last_line_in_any_record_file() {
        // A crash in the middle of an append leaves a partial last line: in charges.jsonl
        // after one whole record, and as the whole of attempts.jsonl. The next append drops it
        // and stays line-aligned (test_state.py).
        let mut w = session_world(&[card(1)]);
        w.next(); // charges the supervisor's session
        let paths = w.paths();
        let whole = fs::read(&paths.charges).unwrap();
        assert!(
            whole.ends_with(b"\n") && whole.iter().filter(|b| **b == b'\n').count() == 1,
            "{whole:?}"
        );
        assert!(!paths.attempts.exists());
        OpenOptions::new()
            .append(true)
            .open(&paths.charges)
            .and_then(|mut f| f.write_all(br#"{"n": 2"#))
            .expect("appending to charges.jsonl");
        write_text(&paths.attempts, br#"{"partial": "attem"#);
        let a = w.next();
        w.act(&a);
        let mut records: BTreeMap<String, Vec<Json>> = BTreeMap::new();
        for path in [&paths.charges, &paths.attempts] {
            let raw = fs::read(path).unwrap();
            assert!(raw.ends_with(b"\n"), "{raw:?}");
            let split: Vec<&[u8]> = raw.split(|b| *b == b'\n').collect();
            records.insert(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                split[..split.len() - 1]
                    .iter()
                    .map(|l| json::parse(l))
                    .collect(),
            );
        }
        assert!(fs::read(&paths.charges).unwrap().starts_with(&whole));
        let charges = &records["charges.jsonl"];
        assert!(charges.len() > 1);
        let keys: Vec<String> = charges.iter().flat_map(|c| c.keys()).collect();
        assert!(!keys.iter().any(|k| k == "n"), "{keys:?}");
        assert_json_eq!(
            field(&records["attempts.jsonl"], "dispatch"),
            ja![a["dispatch_id"].clone()]
        );
    }

    #[test]
    fn test_report_without_recorded_result_is_recorded() {
        let mut w = session_world(&[card(1)]);
        let a = w.next();
        let worker = w.launch(&a);
        w.perform(&a, &worker); // report and commits written; the session died
        resume(&mut w);
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a1-review-1"]"#)
        );
        let res = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["dispatch_id", "worker_id", "status", "source"]),
            ja!["c1-a1-implement-1", worker.as_str(), "READY", "report"]
        );
        assert!(!res.get("interrupted").is_some_and(truthy));
        let ids = field(&w.charges(), "charge_id");
        assert!(
            json_in(&format!("c1-a1-implement-1:claude:{worker}"), &ids),
            "{ids}"
        );
        let rec = one(w.events("reconciled"));
        let last_request = w.events("dispatch_requested").pop().unwrap();
        assert!(seq(&rec) < seq(&last_request));
        assert_eq!(
            status_lines(&w)[0],
            "Resumed at card 1; card 1's report was found and recorded"
        );
        let implements = w
            .events("dispatch_requested")
            .iter()
            .filter(|e| e["dispatch_kind"] == "implement")
            .count();
        assert_eq!(implements, 1);
    }

    #[test]
    fn test_commits_without_report_continue_not_discard() {
        let mut w = session_world(&[card(1)]);
        w.script(1, "implement", [crashing("after_commit")]);
        let a = w.next();
        let worker = w.launch(&a);
        assert!(w.perform(&a, &worker).is_none());
        let tip = w.branch_tip(CARD1);
        assert_ne!(tip, w.base);
        resume(&mut w);
        let nxt = w.next();
        assert_json_eq!(
            pick(
                &nxt,
                &["action", "kind", "dispatch_id", "attempt", "resume"]
            ),
            jv(r#"["dispatch", "continue", "c1-a1-continue-1", 1, null]"#)
        );
        assert_eq!(w.branch_tip(CARD1), tip); // nothing discarded
        let brief = read_text(&os_path(&nxt["brief"]));
        assert!(
            brief.contains("continue from the branch as it stands"),
            "{brief}"
        );
        assert!(brief.contains("c1-a1-implement-1"), "{brief}");
        let res = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["dispatch_id", "interrupted", "tip"]),
            ja!["c1-a1-implement-1", true, tip.as_str()]
        );
        assert_eq!(
            status_lines(&w)[0],
            "Resumed at card 1; card 1's commits were found and recorded"
        );
        w.act(&nxt);
        w.run();
        let acc = one(w.events("accepted"));
        run_git(
            &w.repo,
            &["merge-base", "--is-ancestor", &tip, text(&acc["sha"])],
        );
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c1-a1-continue-1", "c1-a1-review-1"]
        );
    }

    #[test]
    fn test_nothing_on_the_branch_requests_the_same_attempt_again() {
        // Covers P9: a Claude worker without a recorded result counts as interrupted.
        let mut w = session_world(&[card(1)]);
        let a = w.next();
        w.launch(&a); // the worker died before doing anything
        resume(&mut w);
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id", "attempt", "resume"]),
            jv(r#"["implement", "c1-a1-implement-2", 1, null]"#)
        );
        let first = status_lines(&w)[0].clone();
        assert!(
            first.contains("card 1's implement worker was interrupted"),
            "{first}"
        );
        w.act(&nxt);
        w.run();
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1]"));
    }

    #[test]
    fn test_fix_round_resumes_with_a_fresh_worker_from_the_findings() {
        let mut w = session_world(&[card(1)]);
        w.script(1, "review", [with_verdict("defects")]);
        for _ in 0..2 {
            let a = w.next();
            w.act(&a); // implement, then review with defects
        }
        let fix = w.next();
        assert_json_eq!(
            ja![fix["kind"].clone(), !fix["resume"].is_null()],
            jv(r#"["fix", true]"#)
        );
        resume(&mut w); // the fix was requested, never started
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id", "resume"]),
            jv(r#"["fix", "c1-a1-fix-2", null]"#)
        );
        let rnd = one(w.events("fix_round"));
        let brief = read_text(&os_path(&nxt["brief"]));
        assert!(brief.contains(text(&rnd["findings"])), "{brief}");
        w.act(&nxt);
        w.run();
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1]"));
    }

    #[test]
    fn test_acceptance_started_completes_after_restart() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        for _ in 0..2 {
            let a = w.next();
            w.act(&a); // implement, review (clean)
        }
        let r = next_crashing(&w, "before_fast_forward");
        assert_eq!(r.code, 97, "{r}");
        assert_eq!(w.branch_tip(RUN), w.base);
        resume(&mut w);
        assert_eq!(w.next()["action"], "done");
        let acc = one(w.events("accepted"));
        assert_eq!(acc["sha"], w.branch_tip(RUN).as_str());
        let found = w.events("reconciled")[0]["found"].clone();
        assert!(
            json_in("card 1's acceptance was completed", &found),
            "{found}"
        );
    }

    #[test]
    fn test_unmatched_state_fails_with_reason() {
        let mut w = session_world(&[card(1), card(2)]);
        for _ in 0..2 {
            let a = w.next();
            w.act(&a); // card 1 implemented and reviewed
        }
        let a = w.next(); // card 1 accepted; card 2 dispatched
        let acc = one(w.events("accepted"));
        assert_eq!(a["card"], 2);
        w.launch(&a);
        // meanwhile someone moved the run branch back behind card 1
        run_git(
            &w.repo,
            &["update-ref", &format!("refs/heads/{RUN}"), &w.base],
        );
        resume(&mut w);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome"]),
            jv(r#"["done", "failed"]"#)
        );
        let reason = format!(
            "state could not be reconciled: accepted work missing from the run branch: card 1 at {}",
            prefix(text(&acc["sha"]), 12)
        );
        assert!(json_in(&reason, &done["reasons"]), "{}", done["reasons"]);
        assert_eq!(w.events("dispatch_requested").len(), 3); // nothing new was started
        assert!(read_text(&w.paths().report).contains(&reason));
    }

    #[test]
    fn test_missing_card_branch_fails_with_reason() {
        let mut w = session_world(&[card(1)]);
        let a = w.next();
        w.launch(&a);
        let ws = w.workspace();
        run_git(&ws, &["checkout", "--quiet", "--detach"]);
        run_git(&ws, &["branch", "-D", CARD1]);
        resume(&mut w);
        let done = w.next();
        assert_eq!(done["outcome"], "failed");
        let reasons = strs(&done["reasons"]);
        let missing = format!("card 1's branch {CARD1} is missing");
        assert!(reasons.iter().any(|r| r.contains(&missing)), "{reasons:?}");
    }

    #[test]
    fn test_usage_after_interrupted_resume_equals_session_totals() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.session_with("supervisor-1", 2);
        w.start();
        w.script(1, "implement", [crashing("before_commit")]);
        let a = w.next();
        let worker = w.launch(&a);
        assert!(w.perform(&a, &worker).is_none());
        for _ in 0..3 {
            // the interrupted worker's own turns
            let transcript = w.subagent_transcript(&worker);
            w.append_message(&transcript, "claude-sonnet-4");
        }
        w.session_with("supervisor-1", 2); // supervisor turns after its last charge
        resume(&mut w);
        w.run();
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1]"));
        let messages: i64 = walk(&w.claude_home.join("projects"))
            .iter()
            .filter(|p| p.extension() == Some(OsStr::new("jsonl")))
            .map(|p| read_text(p).lines().count() as i64)
            .sum();
        let charges = w.charges();
        assert!(
            charges.iter().all(|c| truthy(&c["usage"]["known"])),
            "{charges:?}"
        );
        let total = |key: &str| -> i64 {
            charges
                .iter()
                .map(|c| c["usage"][key].as_i64().expect("an integer"))
                .sum()
        };
        assert_eq!(
            (total("input"), total("output")),
            (100 * messages, 20 * messages)
        );
        // charged once each: a second next charges nothing more
        w.next();
        assert_eq!(w.charges().len(), charges.len());
    }
}

mod codex {
    use super::*;

    #[test]
    fn test_live_codex_runner_is_waited_not_restarted() {
        let mut w = started_codex_world();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        w.launch(&a);
        let scenario = w.codex_scenario(&a, &Step::default());
        let mut lines: Vec<Json> = read_text(&scenario)
            .lines()
            .map(|x| json::parse(x.as_bytes()))
            .collect();
        lines.insert(2, jv(r#"{"action": "sleep", "seconds": 3}"#));
        let p = start_codex(&mut w, &a, &lines);
        assert_running!(w, a, p);
        kill_proxy(&p);
        resume(&mut w);
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "dispatch_id"]),
            ja!["wait", did.as_str()]
        );
        assert_eq!(w.events("dispatch_requested").len(), 1);
        let rec = one(w.events("reconciled"));
        assert!(
            json_in(
                "card 1's Codex worker is still running; waiting for it",
                &rec["found"]
            ),
            "{rec}"
        );
        // no completion notice comes for it: the timer reports the result once the runner is
        // done
        let r = w
            .cli(&os_args!["timer", run_dir(&w), "--dispatch", &did])
            .ok();
        assert_eq!(r.stdout_text().trim(), "result recorded");
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a1-review-1"]"#)
        );
        let res = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["dispatch_id", "status"]),
            ja![did.as_str(), "READY"]
        );
        assert_eq!(codex_starts(&w), started_once(&did));
    }

    #[test]
    fn test_hung_runner_with_stale_heartbeat_is_cancelled_before_redispatch() {
        let _serial = common::serial(); // group: the stop ladder (the stale heartbeat's 5 s second look)
        let mut w = started_codex_world();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        w.launch(&a);
        let mut lines = thread_and_turn();
        lines.extend([
            jv(r#"{"action": "write_file", "path": "src/card1/partial.txt", "text": "partial\n"}"#),
            jv(r#"{"action": "sleep", "seconds": 120}"#),
        ]);
        let p = start_codex(&mut w, &a, &lines);
        let ws = w.workspace();
        wait_for_with("the partial file", WAIT, Some(&p), || {
            when(ws.join("src/card1/partial.txt").exists())
        });
        let rec = assert_running!(w, a, p);
        let codex = first_codex(&w);
        kill_proxy(&p);
        let runner = rec["runner"].clone();
        signal_if_alive(&runner, Signal::STOP); // hung: alive, heartbeat frozen
        let _cont = ContinueOnDrop(runner.clone());
        let beat = wd(&w, &did).join("heartbeat");
        set_mtime(&beat, SystemTime::now() - Duration::from_secs(600));
        resume(&mut w);
        let nxt = w.next(); // the real 5 s acknowledgement and recheck
        assert!(!alive(&runner));
        assert!(!codex_alive(&codex));
        assert!(!alive(&rec["codex"]));
        let result_json = read_json(&wd(&w, &did).join("result.json"));
        assert_eq!(result_json["exit_reason"], "cancelled");
        // the runner committed the partial work on the card branch; a worker continues from it
        run_git(
            &w.repo,
            &["cat-file", "-e", &format!("{CARD1}:src/card1/partial.txt")],
        );
        assert_json_eq!(
            pick(&nxt, &["action", "kind", "dispatch_id"]),
            jv(r#"["dispatch", "continue", "c1-a1-continue-1"]"#)
        );
        let resume_flag = format!("--resume {}", py_str(&result_json["session_id"]));
        assert!(
            text(&nxt["message"]).contains(&resume_flag),
            "{}",
            nxt["message"]
        );
        let old_result = w
            .events("result_recorded")
            .into_iter()
            .find(|e| e["dispatch_id"] == did.as_str())
            .expect("no result_recorded for the hung dispatch");
        let cont = w
            .events("dispatch_requested")
            .into_iter()
            .find(|e| e["dispatch_id"] == "c1-a1-continue-1")
            .expect("no dispatch_requested for c1-a1-continue-1");
        assert!(seq(&old_result) < seq(&cont));
        assert!(truthy(&old_result["interrupted"]));
        assert_eq!(codex_starts(&w), started_once(&did));
    }

    #[test]
    fn test_dead_runner_without_result_is_finalized_and_dispatched_again() {
        let mut w = started_codex_world();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        w.launch(&a);
        let (argv, mut env) = w.proxy_argv_env(&a, &Step::default());
        env.set("LEADHAND_CRASH_AT", "before_worker_launch"); // every runner it starts dies there
        let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
        let r = common::run_argv(&argv, &env, &w.tmp, None, Duration::from_secs(120));
        assert_ne!(r.code, 0, "{r}");
        assert!(!wd(&w, &did).join("result.json").exists());
        resume(&mut w);
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id"]),
            jv(r#"["implement", "c1-a1-implement-2"]"#)
        );
        assert!(wd(&w, &did).join("result.json").exists());
        assert_eq!(codex_starts(&w), BTreeMap::new());
    }
}

mod pause_cancel {
    use super::*;

    #[test]
    fn test_pause_ends_blocked_paused_with_handoff() {
        let mut w = session_world(&[card(1), card(2)]);
        let a = w.next();
        let worker = w.launch(&a);
        let r = w.cli(&os_args!["pause", run_dir(&w)]).ok();
        assert!(r.stdout_text().contains("pause requested"), "{r}");
        assert!(w.paths().pause_flag.exists());
        let wait = w.next();
        assert_json_eq!(
            pick(&wait, &["action", "dispatch_id"]),
            ja!["wait", a["dispatch_id"].clone()]
        );
        let reply = w.perform(&a, &worker).expect("the worker's reply");
        result(&w, &a, &worker, &reply, &[]);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "qualifier"]),
            jv(r#"["done", "blocked", "paused"]"#)
        );
        assert_eq!(w.events("dispatch_requested").len(), 1); // nothing new started
        assert_eq!(w.events("paused").len(), 1);
        assert!(!w.paths().pause_flag.exists());
        let handoff = read_text(&w.paths().handoff);
        assert!(handoff.contains("Outcome: blocked (paused)"), "{handoff}");
        assert!(handoff.contains("leadhand run"), "{handoff}");
        // a relaunch continues where it stopped
        resume(&mut w);
        w.run();
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1, 2]"));
    }

    #[test]
    fn test_cancel_ends_incomplete_cancelled_and_keeps_card_branches() {
        let mut w = session_world(&[card(1), card(2)]);
        let a = w.next();
        let worker = w.launch(&a);
        let ws = w.workspace();
        write(&ws, &[("src/card1/partial.txt", "partial\n")]); // the worker's partial work so far
        run_git(&ws, &["add", "--", "src/card1/partial.txt"]);
        let mut args: Vec<&str> = NO_HOOKS.to_vec();
        args.extend(["commit", "--quiet", "-m", "partial"]);
        run_git(&ws, &args);
        let partial = w.branch_tip(CARD1);
        let r = w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert!(r.stdout_text().contains("cancel requested"), "{r}");
        assert!(w.paths().cancel_flag.exists());
        let stop = w.next();
        assert_json_eq!(
            pick(
                &stop,
                &["action", "dispatch_id", "worker_id", "record_result"]
            ),
            ja![
                "stop_worker",
                a["dispatch_id"].clone(),
                worker.as_str(),
                true
            ]
        );
        assert_empty!(w.events("outcome")); // not before the worker is stopped
        result(&w, &a, &worker, SPENT, &[OsStr::new("--stopped")]);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "qualifier"]),
            jv(r#"["done", "incomplete", "cancelled"]"#)
        );
        let out = one(w.events("outcome"));
        let cancelled = one(w.events("cancelled"));
        let res = one(w.events("result_recorded"));
        assert!(seq(&cancelled) < seq(&res));
        assert!(seq(&res) < seq(&out));
        assert_eq!(w.branch_tip(CARD1), partial); // partial work stays on the card branch
        assert_eq!(w.branch_tip(RUN), w.base);
        assert_eq!(w.events("dispatch_requested").len(), 1);
        assert!(!w.paths().cancel_flag.exists());
        // a relaunch cannot undo it
        assert_fails!(relaunch(&w), "this run has ended: incomplete (cancelled)");
        assert_empty!(w.events("resumed"));
    }

    #[test]
    fn test_cancel_works_after_the_source_deck_was_edited() {
        let mut w = session_world(&[card(1), card(2)]);
        let a = w.next();
        let worker = w.launch(&a);
        OpenOptions::new()
            .append(true)
            .open(&w.deck_path)
            .and_then(|mut f| f.write_all(b"\nContext: edited after the start.\n"))
            .expect("appending to the deck");
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        let stop = w.next();
        assert_json_eq!(
            pick(&stop, &["action", "dispatch_id", "worker_id"]),
            ja!["stop_worker", a["dispatch_id"].clone(), worker.as_str()]
        );
        result(&w, &a, &worker, SPENT, &[OsStr::new("--stopped")]);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "qualifier"]),
            jv(r#"["done", "incomplete", "cancelled"]"#)
        );
        assert_eq!(w.events("dispatch_requested").len(), 1);
    }

    #[test]
    fn test_pause_works_after_the_source_deck_was_deleted_but_nothing_new_starts() {
        let mut w = session_world(&[card(1), card(2)]);
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("the worker's reply");
        fs::remove_file(&w.deck_path).unwrap();
        // without a pause, the changed deck still blocks the next step: the result is
        // recorded, then `next` refuses
        result(&w, &a, &worker, &reply, &[]);
        assert_fails!(
            w.cli(&os_args!["next", run_dir(&w)]),
            "cannot read the deck"
        );
        assert_eq!(w.events("dispatch_requested").len(), 1);
        w.cli(&os_args!["pause", run_dir(&w)]).ok();
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "qualifier"]),
            jv(r#"["done", "blocked", "paused"]"#)
        );
        assert_eq!(w.events("dispatch_requested").len(), 1); // nothing new started
    }

    #[test]
    fn test_cancel_survives_a_relaunch_before_it_finished() {
        let mut w = session_world(&[card(1), card(2)]);
        let a = w.next();
        w.launch(&a);
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert_eq!(w.next()["action"], "stop_worker"); // the session died before stopping it
        resume(&mut w);
        let done = w.next(); // the dead session's worker needs no stop
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["incomplete", "cancelled"]"#)
        );
        assert_eq!(w.events("dispatch_requested").len(), 1);
    }

    #[test]
    fn test_cancel_confirms_detached_codex_stopped_before_outcome() {
        let _serial = common::serial(); // group: the stop ladder (a detached Codex stopped)
        let mut w = started_codex_world();
        let a = w.next();
        w.launch(&a);
        let mut lines = thread_and_turn();
        lines.extend([
            jv(r#"{"action": "write_file", "path": "src/card1/started.txt", "text": "started\n"}"#),
            jv(r#"{"action": "append_loop", "path": "src/card1/loop.txt", "interval": 0.05}"#),
            jv(r#"{"action": "tool_child", "seconds": 120}"#),
            jv(r#"{"action": "sleep", "seconds": 120}"#),
        ]);
        let p = start_codex(&mut w, &a, &lines);
        let lp = w.workspace().join("src/card1/loop.txt");
        wait_for_with("the worker writing", WAIT, Some(&p), || {
            when(lp.exists() && size_of(&lp) > 0)
        });
        wait_for_with("the tool child", WAIT, Some(&p), || {
            when(!codex_log(&w, Some("child")).is_empty())
        });
        let rec = assert_running!(w, a, p);
        let codex = first_codex(&w);
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["incomplete", "cancelled"]"#)
        );
        let children = codex_log(&w, Some("child"));
        assert!(!children.is_empty());
        assert!(!alive(&rec["runner"]), "{rec}");
        assert!(!codex_alive(&codex), "{codex}");
        assert!(!alive(&rec["codex"]), "{rec}");
        for child in &children {
            assert!(!logged_alive(child), "{child}");
        }
        let size = size_of(&lp);
        holds(
            || size_of(&lp) == size,
            0.5,
            "no workspace write after the outcome",
        );
        let out = one(w.events("outcome"));
        let res = one(w.events("result_recorded"));
        assert!(seq(&res) < seq(&out));
        assert!(truthy(&res["stopped"]));
        p.communicate(Duration::from_secs(60))
            .expect("the proxy still running 60 s after the cancel");
    }

    #[test]
    fn test_crash_after_cancel_request_finishes_the_cancel_on_restart() {
        let _serial = common::serial(); // group: the stop ladder (a detached Codex stopped)
        let mut w = started_codex_world();
        let a = w.next();
        w.launch(&a);
        let mut lines = thread_and_turn();
        lines.push(jv(r#"{"action": "sleep", "seconds": 120}"#));
        let p = start_codex(&mut w, &a, &lines);
        let rec = assert_running!(w, a, p);
        let codex = first_codex(&w);
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        let r = next_crashing(&w, "after_cancel_request");
        assert_eq!(r.code, 97, "{r}");
        assert_empty!(w.events("outcome"));
        kill_proxy(&p);
        resume(&mut w);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["incomplete", "cancelled"]"#)
        );
        assert!(!alive(&rec["runner"]), "{rec}");
        assert!(!codex_alive(&codex), "{codex}");
        assert!(!alive(&rec["codex"]), "{rec}");
        let res = one(w.events("result_recorded"));
        assert!(seq(&res) < seq(&w.events("outcome")[0]));
    }

    #[test]
    fn test_relaunch_refuses_a_finished_run() {
        let mut w = gated(&[card(1)]);
        w.start();
        assert_eq!(w.run().pop().unwrap()["outcome"], "complete");
        assert_fails!(relaunch(&w), "this run has ended: complete");
        assert_empty!(w.events("resumed"));
        assert_fails!(
            w.cli(&os_args!["cancel", run_dir(&w)]),
            "this run has ended: complete"
        );
        assert!(!w.paths().cancel_flag.exists());
    }
}

/// A Codex card dispatch whose runner was SIGKILLed while Codex (and a tool child) live on;
/// the proxy died with the session (`orphaned_codex`). Gives the world, the action,
/// runner.json, the fake's `start` line for Codex and its `child` lines.
#[track_caller]
fn orphaned_codex(lines: &[Json]) -> (FakeSupervisor, Json, Json, Json, Vec<Json>) {
    let mut w = started_codex_world();
    let a = w.next();
    w.launch(&a);
    let p = start_codex(&mut w, &a, lines);
    wait_for_with("the tool child", WAIT, Some(&p), || {
        when(!codex_log(&w, Some("child")).is_empty())
    });
    let rec = assert_running!(w, a, p);
    let codex = first_codex(&w);
    signal_if_alive(&rec["runner"], Signal::KILL);
    kill_proxy(&p);
    wait_for("the runner to die", || when(!alive(&rec["runner"])));
    let children = codex_log(&w, Some("child"));
    (w, a, rec, codex, children)
}

fn orphan_lines() -> Vec<Json> {
    let mut lines = thread_and_turn();
    lines.extend([
        jv(r#"{"action": "write_file", "path": "src/card1/p.txt", "text": "p\n"}"#),
        jv(r#"{"action": "tool_child", "seconds": 120}"#),
        jv(r#"{"action": "sleep", "seconds": 120}"#),
    ]);
    lines
}

mod fix_round1 {
    //! Review fix round 1 of Task 21.
    use super::*;

    #[test]
    fn test_cancel_after_an_interrupted_acceptance_reconciles_first() {
        let mut w = session_world(&[card(1), card(2)]);
        for _ in 0..2 {
            let a = w.next();
            w.act(&a); // card 1 implemented and reviewed (clean)
        }
        let r = next_crashing(&w, "after_fast_forward");
        assert_eq!(r.code, 97, "{r}");
        assert_ne!(w.branch_tip(RUN), w.base);
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        resume(&mut w);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["incomplete", "cancelled"]"#)
        );
        let acc = one(w.events("accepted"));
        assert_json_eq!(pick(&acc, &["card", "sha"]), ja![1, w.branch_tip(RUN)]);
        let reasons = strs(&done["reasons"]);
        assert!(!reasons.iter().any(|r| r.contains("card 1")), "{reasons:?}");
        let rec = one(with_found(&w));
        assert!(
            json_in("card 1's acceptance was completed", &rec["found"]),
            "{rec}"
        );
        assert!(seq(&rec) < seq(&w.events("cancelled")[0]));
    }

    #[test]
    fn test_cancel_commits_a_stopped_claude_workers_partial_work() {
        let mut w = session_world(&[card(1)]);
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        let worker = w.launch(&a);
        let ws = w.workspace();
        write(&ws, &[("src/card1/partial.txt", "partial\n")]); // uncommitted when it is stopped
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert_eq!(w.next()["action"], "stop_worker");
        let event = result(&w, &a, &worker, SPENT, &[OsStr::new("--stopped")]);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["incomplete", "cancelled"]"#)
        );
        let tip = w.branch_tip(CARD1);
        assert_eq!(event["tip"], tip.as_str());
        run_git(
            &w.repo,
            &["cat-file", "-e", &format!("{CARD1}:src/card1/partial.txt")],
        );
        let body = git_log_body(&w.repo, CARD1);
        let trailer = format!("Leadhand-Dispatch: {did}");
        assert!(lines(&body).contains(&trailer), "{body}");
        assert!(body.contains("partial work (stopped)"), "{body}");
        assert_eq!(run_git(&ws, &["status", "--porcelain"]), "");
        let intent = reconciled_at(&w, "partial_work");
        assert_json_eq!(field(&intent, "dispatch_id"), ja![did.as_str()]);
        assert!(seq(&intent[0]) < seq(&event));
    }

    #[test]
    fn test_cancel_commits_partial_work_of_a_worker_that_died_with_its_session() {
        let mut w = session_world(&[card(1)]);
        w.script(1, "implement", [crashing("before_commit")]);
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        let worker = w.launch(&a);
        assert!(w.perform(&a, &worker).is_none()); // its files are written, not committed
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        resume(&mut w);
        let done = w.next(); // no stop needed: it died with the session
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["incomplete", "cancelled"]"#)
        );
        run_git(
            &w.repo,
            &["cat-file", "-e", &format!("{CARD1}:src/card1/{did}.txt")],
        );
        let body = git_log_body(&w.repo, CARD1);
        assert!(
            lines(&body).contains(&format!("Leadhand-Dispatch: {did}")),
            "{body}"
        );
        let res = one(w.events("result_recorded"));
        let tip = w.branch_tip(CARD1);
        assert_json_eq!(
            pick(&res, &["tip", "partial_commit"]),
            ja![tip.as_str(), tip.as_str()]
        );
    }

    #[test]
    fn test_stopped_check_worker_changes_are_not_committed() {
        let mut w = session_world(&[card(1)]);
        let a = w.next();
        w.act(&a); // implement
        let a = w.next();
        assert_eq!(a["kind"], "review");
        let worker = w.launch(&a);
        let tip = w.branch_tip(CARD1);
        write(
            &w.workspace(),
            &[("src/card1/reviewer.txt", "not mine to write\n")],
        );
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert_eq!(w.next()["action"], "stop_worker");
        result(&w, &a, &worker, SPENT, &[OsStr::new("--stopped")]);
        w.next();
        assert_eq!(w.branch_tip(CARD1), tip);
    }

    #[test]
    fn test_failed_reconciliation_stops_live_codex_before_the_outcome() {
        let _serial = common::serial(); // group: the stop ladder (a detached Codex stopped)
        let mut w = started_codex_world();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        w.launch(&a);
        let mut lines = thread_and_turn();
        lines.extend([
            jv(r#"{"action": "write_file", "path": "src/card1/started.txt", "text": "started\n"}"#),
            jv(r#"{"action": "append_loop", "path": "src/card1/loop.txt", "interval": 0.05}"#),
            jv(r#"{"action": "sleep", "seconds": 120}"#),
        ]);
        let p = start_codex(&mut w, &a, &lines);
        let lp = w.workspace().join("src/card1/loop.txt");
        wait_for_with("the worker writing", WAIT, Some(&p), || {
            when(lp.exists() && size_of(&lp) > 0)
        });
        let rec = assert_running!(w, a, p);
        let codex = first_codex(&w);
        kill_proxy(&p);
        // unmatched: the run branch is gone
        run_git(&w.repo, &["update-ref", "-d", &format!("refs/heads/{RUN}")]);
        resume(&mut w);
        let done = w.next();
        assert_eq!(done["outcome"], "failed");
        let reason = format!("state could not be reconciled: the run branch {RUN} is missing");
        assert!(json_in(&reason, &done["reasons"]), "{}", done["reasons"]);
        assert!(!alive(&rec["runner"]), "{rec}");
        assert!(!codex_alive(&codex), "{codex}");
        assert!(!alive(&rec["codex"]), "{rec}");
        assert!(wd(&w, &did).join("result.json").exists());
        let size = size_of(&lp);
        holds(
            || size_of(&lp) == size,
            0.5,
            "no workspace write after the outcome",
        );
    }

    #[test]
    fn test_orphaned_codex_is_cancelled_before_anything_else() {
        let _serial = common::serial(); // group: the stop ladder (an orphaned Codex stopped)
        let (mut w, a, rec, codex, children) = orphaned_codex(&orphan_lines());
        let did = text(&a["dispatch_id"]).to_string();
        assert!(codex_alive(&codex));
        assert!(alive(&rec["codex"]));
        assert!(
            !children.is_empty() && logged_alive(&children[0]),
            "{children:?}"
        );
        resume(&mut w);
        let nxt = w.next();
        assert!(!codex_alive(&codex));
        assert!(!alive(&rec["codex"]));
        assert!(!children.iter().any(logged_alive), "{children:?}");
        let intent = one(reconciled_at(&w, "cancel"));
        assert_json_eq!(
            pick(&intent, &["dispatch_id", "runner"]),
            ja![did.as_str(), "orphaned"]
        );
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id"]),
            jv(r#"["continue", "c1-a1-continue-1"]"#)
        );
        // the runner's cancel commit
        run_git(
            &w.repo,
            &["cat-file", "-e", &format!("{CARD1}:src/card1/p.txt")],
        );
        assert_eq!(codex_starts(&w), started_once(&did));
    }

    #[test]
    fn test_crash_in_reconciliations_cancel_window_is_repeat_safe() {
        let _serial = common::serial(); // group: the stop ladder (an orphaned Codex stopped)
        let (mut w, _a, rec, codex, children) = orphaned_codex(&orphan_lines());
        resume(&mut w);
        let r = next_crashing(&w, "after_cancel_request");
        assert_eq!(r.code, 97, "{r}");
        let intents = reconciled_at(&w, "cancel");
        assert_eq!(intents.len(), 1);
        assert_empty!(with_found(&w));
        assert_empty!(w.events("result_recorded"));
        resume_as(&mut w, "supervisor-3");
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id"]),
            jv(r#"["continue", "c1-a1-continue-1"]"#)
        );
        assert!(!codex_alive(&codex));
        assert!(!alive(&rec["codex"]));
        assert!(!children.iter().any(logged_alive), "{children:?}");
        let res = one(w.events("result_recorded"));
        assert!(truthy(&res["interrupted"])); // our own cancel, not a stop at the deadline
    }

    #[test]
    fn test_unrecorded_codex_start_past_its_deadline_is_not_charged_or_stopped() {
        let _serial = common::serial(); // group: the stop ladder (an unrecorded Codex stopped at its limit)
        let mut lines = thread_and_turn();
        lines.extend([
            jv(r#"{"action": "tool_child", "seconds": 120}"#),
            jv(r#"{"action": "sleep", "seconds": 120}"#),
        ]);
        let mut w = started_codex_world();
        let a = w.next(); // the proxy ran, but `record started` never happened
        let did = text(&a["dispatch_id"]).to_string();
        let p = start_codex(&mut w, &a, &lines);
        assert_running!(w, a, p);
        kill_proxy(&p);
        resume(&mut w);
        let wait = w.next();
        assert_json_eq!(
            pick(&wait, &["action", "worker_id"]),
            ja!["wait", format!("{did}:unrecorded")]
        );
        w.advance(70.0 * 60.0);
        let nxt = w.next();
        assert_eq!(nxt["action"], "dispatch"); // no stop_worker for the made-up worker ID
        assert_eq!(nxt["dispatch_id"], "c1-a2-implement-1"); // stopped at its limit: SPENT, escalated
        let res = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["status", "stopped", "charge_ids"]),
            jv(r#"["SPENT", true, []]"#)
        );
        let ids = strs(&field(&w.charges(), "charge_id"));
        assert!(!ids.iter().any(|c| c.contains(":unrecorded")), "{ids:?}");
    }

    /// A gated card 1 run whose `kind` check worker (a review, or a gate) dies without a
    /// report; after a relaunch the run completes (`interrupted_check`). Gives the
    /// interrupted action, the first action after the relaunch, and the world.
    macro_rules! assert_interrupted_check {
        ($kind:expr) => {{
            let kind: &str = $kind;
            let mut w = gated(&[card(1)]);
            w.session("supervisor-1");
            w.start();
            let mut a = Json::Null;
            for _ in 0..50 {
                a = w.next();
                if a["action"] == "dispatch"
                    && (a.get("gate").is_some_and(|g| g == kind) || a["kind"] == kind)
                {
                    break;
                }
                w.act(&a);
            }
            w.launch(&a); // the worker dies without a report
            resume(&mut w);
            let nxt = w.next();
            let done = w.run().pop().unwrap();
            assert_eq!(done["outcome"], "complete");
            (a, nxt, w)
        }};
    }

    #[test]
    fn test_interrupted_review_without_report_runs_again() {
        let (a, nxt, w) = assert_interrupted_check!("review");
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id", "resume"]),
            jv(r#"["review", "c1-a1-review-2", null]"#)
        );
        let res = w
            .events("result_recorded")
            .into_iter()
            .find(|e| e["dispatch_id"] == a["dispatch_id"])
            .expect("no result_recorded for the interrupted review");
        assert!(truthy(&res["interrupted"]));
    }

    #[test]
    fn test_interrupted_gate_without_report_runs_again() {
        let (_a, nxt, w) = assert_interrupted_check!("final_review");
        assert_json_eq!(
            pick(&nxt, &["kind", "dispatch_id", "gate"]),
            jv(r#"["final_review", "run-final_review-2", "final_review"]"#)
        );
        let of_gate = |kind: &str| -> Json {
            let events: Vec<Json> = w
                .events(kind)
                .into_iter()
                .filter(|e| e["gate"] == "final_review")
                .collect();
            field(&events, "dispatch_id")
        };
        assert_json_eq!(
            of_gate("gate_started"),
            jv(r#"["run-final_review-1", "run-final_review-2"]"#)
        );
        assert_json_eq!(of_gate("gate_result"), jv(r#"["run-final_review-2"]"#));
    }

    /// A gated card 1 run whose final fix (after a final review with defects) is interrupted,
    /// crashing at `crash` (or dying before doing anything); after a relaunch it runs again
    /// and the run completes (`interrupted_final_fix`). Gives the world and the interrupted
    /// action.
    macro_rules! assert_interrupted_final_fix {
        ($crash:expr) => {{
            let crash: Option<&str> = $crash;
            let mut w = gated(&[card(1)]);
            w.script(None, "final_review", [with_verdict("defects")]);
            w.script(
                None,
                "final_fix",
                [crash.map_or_else(Step::default, crashing)],
            );
            w.session("supervisor-1");
            w.start();
            let mut a = Json::Null;
            for _ in 0..50 {
                a = w.next();
                if a["action"] == "dispatch" && a["kind"] == "final_fix" {
                    break;
                }
                w.act(&a);
            }
            let worker = w.launch(&a);
            if crash.is_some() {
                assert!(w.perform(&a, &worker).is_none());
            }
            resume(&mut w);
            let nxt = w.next();
            assert_json_eq!(
                pick(&nxt, &["kind", "dispatch_id", "wave"]),
                jv(r#"["final_fix", "run-w1-final_fix-2", 1]"#)
            );
            let brief = read_text(&os_path(&nxt["brief"]));
            assert!(brief.contains("## The interrupted step"), "{brief}");
            let done = w.run().pop().unwrap();
            assert_eq!(done["outcome"], "complete");
            (w, a)
        }};
    }

    #[test]
    fn test_interrupted_final_fix_before_its_commit_runs_again() {
        let (w, _a) = assert_interrupted_final_fix!(None);
        let finals = w
            .events("accepted")
            .iter()
            .filter(|e| e["card"] == "final")
            .count();
        assert_eq!(finals, 1);
    }

    #[test]
    fn test_interrupted_final_fix_after_its_commit_keeps_it() {
        let (w, a) = assert_interrupted_final_fix!(Some("after_commit"));
        let found = field(&with_found(&w), "found");
        assert!(
            json_in("the run's commits were found and recorded", &found[0]),
            "{found}"
        );
        // on the run branch
        run_git(
            &w.repo,
            &[
                "cat-file",
                "-e",
                &format!("refs/heads/{RUN}:src/final/{}.txt", text(&a["dispatch_id"])),
            ],
        );
    }
}

mod fix_round2 {
    //! Review fix round 2 of Task 21: a reconciliation mismatch outranks a pending cancel.
    use super::*;

    /// Card 1's branch rewritten after its worker's commit, with a cancel pending when
    /// `cancel`: the relaunched run fails with the rewrite as its reason, and starts nothing
    /// new (`rewritten`). Gives the world.
    macro_rules! assert_rewritten {
        ($cancel:expr) => {{
            let cancel: bool = $cancel;
            let mut w = session_world(&[card(1)]);
            w.script(1, "implement", [crashing("after_commit")]);
            let a = w.next();
            let worker = w.launch(&a);
            assert!(w.perform(&a, &worker).is_none());
            let ws = w.workspace();
            let tree = format!("{}^{{tree}}", w.base);
            let orphan = run_git(&ws, &["commit-tree", "-m", "rewritten", &tree]);
            // the card branch no longer holds where it started
            run_git(&ws, &["reset", "--hard", &orphan]);
            if cancel {
                w.cli(&os_args!["cancel", run_dir(&w)]).ok();
            }
            resume(&mut w);
            let done = w.next();
            assert_json_eq!(
                pick(&done, &["action", "outcome", "qualifier"]),
                jv(r#"["done", "failed", ""]"#)
            );
            let reasons = strs(&done["reasons"]);
            assert!(
                reasons.iter().any(|r| r
                    .starts_with("state could not be reconciled: card 1's branch")
                    && r.contains("was rewritten")),
                "{reasons:?}"
            );
            let rec = one(with_found(&w));
            let found = strs(&rec["found"]);
            assert!(
                found.iter().any(|f| f.contains("was rewritten")),
                "{found:?}"
            );
            assert_eq!(w.events("outcome").len(), 1);
            assert_eq!(w.events("dispatch_requested").len(), 1); // nothing new was started
            w
        }};
    }

    #[test]
    fn test_rewritten_branch_fails_the_run() {
        assert_rewritten!(false);
    }

    #[test]
    fn test_rewritten_branch_fails_the_run_even_with_a_cancel_pending() {
        assert_rewritten!(true);
    }

    #[test]
    fn test_cancel_with_the_run_branch_gone_fails_with_every_accepted_card_missing() {
        let mut w = session_world(&[card(1), card(2)]);
        for _ in 0..2 {
            let a = w.next();
            w.act(&a);
        }
        let a = w.next(); // card 1 accepted; card 2 dispatched
        let acc = one(w.events("accepted"));
        let worker = w.launch(&a);
        run_git(&w.repo, &["update-ref", "-d", &format!("refs/heads/{RUN}")]);
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert_eq!(w.next()["action"], "stop_worker");
        result(&w, &a, &worker, SPENT, &[OsStr::new("--stopped")]);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["failed", ""]"#)
        );
        let reason = format!(
            "state could not be reconciled: accepted work missing from the run branch: card 1 at {}",
            prefix(text(&acc["sha"]), 12)
        );
        assert!(json_in(&reason, &done["reasons"]), "{}", done["reasons"]);
        assert!(w.events("outcome")[0]["head"].is_null());
    }

    #[test]
    fn test_run_branch_deleted_after_acceptance_ends_failed_on_relaunch() {
        // With the real end of run (gates on), `done` always carries an outcome.
        let mut w = gated(&[card(1)]);
        w.session("supervisor-1");
        w.start();
        for _ in 0..20 {
            if !w.events("accepted").is_empty() {
                break;
            }
            let a = w.next();
            w.act(&a);
        }
        run_git(&w.repo, &["update-ref", "-d", &format!("refs/heads/{RUN}")]);
        // the live session cannot go on: an error, not a guess
        assert_fails!(w.cli(&os_args!["next", run_dir(&w)]));
        resume(&mut w);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome"]),
            jv(r#"["done", "failed"]"#)
        );
        let reason = format!("state could not be reconciled: the run branch {RUN} is missing");
        assert!(json_in(&reason, &done["reasons"]), "{}", done["reasons"]);
        assert_eq!(w.next()["outcome"], "failed");
    }
}

mod runner_helpers {
    //! What reconciliation does with the processes a finished Codex dispatch's dead runner left
    //! (`dispatch.stop_strays`), against real processes: the worker folder names a test sleeper
    //! as Codex (runner.json) and another as its descendant (procs.json), with a runner that is
    //! gone.
    use super::*;

    /// A `sleep 120` in a process group of its own (Python's sleeper with
    /// `start_new_session`); its group is killed and it is reaped (within 10 s) when dropped
    /// (`reap`).
    struct Sleeper(Proc);

    impl Drop for Sleeper {
        fn drop(&mut self) {
            if self.0.poll().is_none()
                && let Some(group) = Pid::from_raw(self.0.pid())
            {
                let _ = kill_process_group(group, Signal::KILL);
            }
            if wait_timeout(&self.0, Duration::from_secs(10)).is_none() && !thread::panicking() {
                panic!("the sleeper {} outlived its kill by 10 s", self.0.pid());
            }
        }
    }

    /// A sleeper and its `{pid, pgid, start}` record (`sleeper`).
    #[track_caller]
    fn sleeper() -> (Sleeper, Json) {
        let child = Command::new("sleep")
            .arg("120")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting a sleeper");
        let p = Sleeper(Proc::new(child));
        let pid = p.0.pid();
        let start = wait_for("the sleeper's process record", || start_time(pid));
        let rec = Json::object([
            ("pid", Json::from(pid)),
            ("pgid", Json::from(pid)),
            ("start", Json::from(start)),
        ]);
        (p, rec)
    }

    use std::os::unix::process::CommandExt;

    #[test]
    fn test_stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone() {
        // Covers P14.
        let _serial = common::serial(); // group: the stop ladder (strays stopped)
        let mut w = started_codex_world();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        w.act(&a); // Codex ran to its end; its result is recorded
        let res = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["dispatch_id", "status"]),
            ja![did.as_str(), "READY"]
        );
        let wd = wd(&w, &did);
        let (tool, tool_rec) = sleeper();
        let (codex, codex_rec) = sleeper();
        let runner_json = Json::object([
            ("runner", jv(r#"{"pid": 1, "pgid": 1, "start": "gone"}"#)),
            ("codex", codex_rec),
        ]);
        write_text(&wd.join("runner.json"), json::dumps(&runner_json));
        let procs = Json::object([("descendants", Json::Arr(vec![tool_rec]))]);
        write_text(&wd.join("procs.json"), json::dumps(&procs));
        // A live runner holds runner.lock, as the holder process did.
        let lock = wd.join("runner.lock");
        let runner = hold_lock(&lock);
        assert!(!lockable(&lock), "runner.lock is not held");
        resume(&mut w);
        w.next(); // a live runner stops its own
        assert!(codex.0.poll().is_none());
        assert!(tool.0.poll().is_none());
        drop(runner); // the runner is gone
        resume_as(&mut w, "supervisor-3");
        w.next(); // the runner is gone: what it left is stopped
        assert!(
            wait_timeout(&codex.0, Duration::from_secs(10)).is_some(),
            "the stray Codex was still running 10 s on"
        );
        assert!(
            wait_timeout(&tool.0, Duration::from_secs(10)).is_some(),
            "the stray tool child was still running 10 s on"
        );
        resume_as(&mut w, "supervisor-4");
        w.next(); // nothing left: a no-op
    }
}

// --------------------------------------------------------------------------
// Every crash window

mod every_crash_point {
    //! Crash at every occurrence of every crash point, restart, and reach the uninterrupted
    //! run's final state. "The end" is the run's outcome with its report (the world runs the
    //! end-of-run gates, so `before_gate_record` and the gate recovery are covered too). The
    //! crash runs are independent, so they run in workers, a few at a time.
    //!
    //! The contract's crash points are the core plan's CRASH_POINTS and the harness's own
    //! `before_commit` and `after_commit`. The reference run must pass every one of them
    //! except `after_cancel_request` (a cancel's: covered by the cancel tests). The matrix
    //! crashes at every point the reference run's LEADHAND_CRASH_LOG names, as often as it
    //! names it, so a point a binary adds is crash-tested too. "The same final state" is the
    //! reference run's, by the same binary: the outcome, every card's state, the accepted
    //! SHAs, and the run branch's tip and tree.
    //!
    //! Three processes take part, each its own process group (the drive helper's
    //! `process_group(0)`): the test; a worker per crash run (a drive child of the test,
    //! chain length 1), which makes a fresh world and forks its crash child
    //! (`fork_noting`); and the crash child (chain length 2), which attaches the worker's
    //! world and drives it until the crash point kills its group. The test branches on
    //! `drive::selected()` first, so each child goes straight to its own part.
    use super::*;

    const POINTS: [&str; 13] = [
        "before_worker_launch",
        "after_worker_launch",
        "before_rebase",
        "after_rebase",
        "before_fast_forward",
        "after_fast_forward",
        "before_result_record",
        "after_result_files",
        "after_charge_before_attempt",
        "after_cancel_request",
        "before_gate_record",
        "before_commit",
        "after_commit",
    ];
    const NOT_ON_THE_PATH: [&str; 1] = ["after_cancel_request"];
    /// A worker's deadline (`RUN_TIMEOUT`).
    const RUN_TIMEOUT: Duration = Duration::from_secs(300);
    /// A crash child's deadline (crash_child's 180 s).
    const CRASH_DEADLINE: Duration = Duration::from_secs(180);

    /// `PARALLEL`: the workers run at once, `max(1, min(4, cpu_count))`.
    fn parallel() -> usize {
        thread::available_parallelism().map_or(1, |n| n.get().clamp(1, 4))
    }

    /// A crash point's name as a label part: what the drive helper's slug keeps
    /// (alphanumerics), the rest `-`.
    fn slug(point: &str) -> String {
        point
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    }

    /// A worker's drive label.
    fn worker_label(point: &str, k: usize) -> String {
        format!("worker-{}-{k}", slug(point))
    }

    /// A worker's crash child's drive label.
    fn crash_label(point: &str, k: usize) -> String {
        format!("crash-{}-{k}", slug(point))
    }

    /// `stable_world`: a StableSupervisor with the supervisor session "supervisor-1",
    /// started. The caller fixes the commit dates first (`patch_env(&DATES)`, held for the
    /// world's life): its environment, and every Git call here, get the dates.
    #[track_caller]
    fn stable_world() -> FakeSupervisor {
        let mut w = stable_supervisor();
        w.session("supervisor-1");
        w.start();
        w
    }

    /// Appends one JSON line to `path`.
    #[track_caller]
    fn note(path: &Path, rec: &Json) {
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .and_then(|mut f| f.write_all(format!("{}\n", json::dumps(rec)).as_bytes()))
            .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    }

    fn os_json(p: &Path) -> Json {
        Json::Str(json::from_os(p.as_os_str().as_encoded_bytes()))
    }

    /// The crash child's drive: the crash point's group is its own (LEADHAND_CRASH_KILL_PGID,
    /// set here as Python's child sets it to its own pid), then the world's drive, which the
    /// armed crash point ends.
    fn crash_drive(w: &mut FakeSupervisor, pid: i32) {
        w.env.set("LEADHAND_CRASH_KILL_PGID", pid.to_string());
        stable_drive(w);
    }

    /// Cleans up after a worker killed at its deadline, or dead without a result
    /// (`kill_world`): the crash child's group, every process its run records, the fake
    /// Codex's tool children, then its temporary root. The group is killed only while its
    /// leader is still the crash child recorded: it was usually reaped long before, and its
    /// pid may since name an unrelated group.
    fn kill_world(info: &Path) {
        let records = if info.exists() {
            read_jsonl(info)
        } else {
            Vec::new()
        };
        let mut merged: BTreeMap<String, Json> = BTreeMap::new();
        for rec in records {
            for key in rec.keys() {
                merged.insert(key.clone(), rec[key.as_str()].clone());
            }
        }
        if let (Some(pgid), Some(start)) = (
            merged
                .get("drive_pgid")
                .and_then(Json::as_i64)
                .and_then(|p| i32::try_from(p).ok()),
            merged.get("drive_start").and_then(Json::as_str),
        ) && same_process(pgid, Some(start))
            && let Some(group) = Pid::from_raw(pgid)
        {
            let _ = kill_process_group(group, Signal::KILL);
        }
        if let (Some(run_dir), Some(log)) = (
            merged.get("run_dir").and_then(Json::as_os),
            merged.get("codex_log").and_then(Json::as_os),
        ) {
            kill_run_leftovers(Path::new(&run_dir), Path::new(&log));
        }
        if let Some(tmp) = merged.get("tmp").and_then(Json::as_os) {
            let _ = fs::remove_dir_all(tmp);
        }
    }

    /// One crash run, in a worker (`crash_run`): crash at the (k+1)-th pass of `point` in a
    /// crash child, leave the mismatch, relaunch without the crash, run to the end, compare
    /// with `expected`. `info` gets the world's paths and the crash child's group, for the
    /// cleanup of a worker killed at its deadline (`kill_world`).
    macro_rules! assert_crash_run {
        ($point:expr, $k:expr, $expected:expr, $info:expr) => {{
            let (point, k, expected, info): (&str, usize, &Json, &Path) =
                ($point, $k, $expected, $info);
            let _dates = patch_env(&DATES);
            let mut w = stable_world();
            note(
                info,
                &Json::object([
                    ("tmp", os_json(&w.tmp)),
                    ("run_dir", os_json(&run_dir(&w))),
                    (
                        "codex_log",
                        Json::Str(json::from_os(
                            w.env.get("FAKE_CODEX_LOG").unwrap().as_encoded_bytes(),
                        )),
                    ),
                ]),
            );
            let counter = w.tmp.join("crash counter");
            // The crash child's arming (Python's child-only `w.env.update`): it reaches the
            // child through the world's state, and is taken off the worker's world after.
            w.env
                .set("LEADHAND_CRASH_AT", point)
                .set("LEADHAND_CRASH_SKIP", k.to_string())
                .set("LEADHAND_CRASH_COUNTER", &counter);
            let forked = w.fork_noting(
                &crash_label(point, k),
                CRASH_DEADLINE,
                |pid| {
                    note(
                        info,
                        &Json::object([
                            ("drive_pgid", Json::from(pid)),
                            (
                                "drive_start",
                                start_time(pid).map_or(Json::Null, Json::from),
                            ),
                        ]),
                    )
                },
                crash_drive,
            );
            w.env
                .remove("LEADHAND_CRASH_AT")
                .remove("LEADHAND_CRASH_SKIP")
                .remove("LEADHAND_CRASH_COUNTER");
            if forked.hung {
                panic!("{point} #{}: the crashed run hung", k + 1);
            }
            if let Some(error) = &forked.error {
                panic!("{point} #{}: {error}", k + 1);
            }
            assert!(
                forked.code < 0 || forked.code == 97,
                "{point} #{} did not crash (status {})\n{}",
                k + 1,
                forked.code,
                forked.output
            );
            assert_eq!(read_text(&counter).trim(), (k + 1).to_string());
            wait_for_orphans(&w, Duration::from_secs(30));
            resume(&mut w);
            stable_drive(&mut w);
            assert_json_eq!(final_state(&w), *expected);
            assert_invariants!(w);
            assert!(!w.events("reconciled").is_empty());
            assert!(w.paths().report.is_file());
        }};
    }

    /// How one worker ended: "ok", what it wrote when it failed, or why there is nothing.
    fn run_worker(test: &str, results: &Path, point: &str, k: usize, expected: &Json) -> String {
        let label = worker_label(point, k);
        let out = results.join(format!("{label}.txt"));
        let info = results.join(format!("{label}.info"));
        let payload = results.join(format!("{label}.json"));
        let output = results.join(format!("{label} output.txt"));
        let args = Json::object([
            ("point", Json::from(point)),
            ("k", Json::from(k as i64)),
            ("expected", expected.clone()),
            ("out", os_json(&out)),
            ("info", os_json(&info)),
        ]);
        write_text(&payload, json::dumps(&args));
        let ended = drive::start(test, &label, &payload, &output).wait(RUN_TIMEOUT);
        if ended.hung {
            kill_world(&info);
            return format!("timed out after {} s", RUN_TIMEOUT.as_secs());
        }
        match fs::read_to_string(&out) {
            Ok(text) => {
                if text != "ok" {
                    kill_world(&info);
                }
                text
            }
            Err(_) => {
                kill_world(&info);
                format!(
                    "the worker died without a result (exit code {})\n{}",
                    ended.code, ended.output
                )
            }
        }
    }

    /// Sets the flag when the thread holding it unwinds, so the other worker threads take no
    /// new crash run (Python's `finally` stopping what still runs after an error).
    struct AbortOnPanic<'a>(&'a AtomicBool);

    impl Drop for AbortOnPanic<'_> {
        fn drop(&mut self) {
            if thread::panicking() {
                self.0.store(true, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn test_restart_from_every_crash_point_reaches_same_final_state() {
        // Covers bug class 7 (crash windows).
        if let Some(sel) = drive::selected() {
            if sel.chain.len() == 2 {
                // The crash child: the worker's world, its hooks again, and the drive.
                let _dates = patch_env(&DATES);
                let mut w = FakeSupervisor::attach_drive(&sel.label)
                    .expect("the crash child's world from its worker");
                stable_hooks(&mut w);
                w.fork(&sel.label, CRASH_DEADLINE, crash_drive);
                unreachable!("fork ends the crash child");
            }
            // A worker: its one crash run; it writes "ok" to `out`, or the failure.
            let args = read_json(&sel.payload);
            let point = text(&args["point"]).to_string();
            let k = args["k"].as_u64().expect("an occurrence") as usize;
            let expected = args["expected"].clone();
            let out = os_path(&args["out"]);
            let info = os_path(&args["info"]);
            drive::run_child(&out, || {
                assert_crash_run!(&point, k, &expected, &info);
                write_text(&out, "ok");
            });
        }

        let _serial = common::serial(); // group: the stop ladder, beside the matrix's load
        let began = Instant::now();
        let _dates = patch_env(&DATES);
        let mut reference = stable_world();
        let log = reference.tmp.join("crash points.txt");
        reference.env.set("LEADHAND_CRASH_LOG", &log);
        stable_drive(&mut reference);
        reference.env.remove("LEADHAND_CRASH_LOG");
        let expected = final_state(&reference);
        assert_json_eq!(expected["outcome"], jv(r#"["complete", ""]"#));
        let states: BTreeSet<String> = expected["cards"]
            .as_object()
            .expect("the cards' states")
            .iter()
            .map(|(_, s)| text(s).to_string())
            .collect();
        assert_eq!(states, BTreeSet::from(["accepted".to_string()]));
        let mut accepted: Vec<i64> = expected["accepted"]
            .keys()
            .iter()
            .map(|k| json::parse(k.as_bytes()).as_i64().expect("an integer card"))
            .collect();
        accepted.sort();
        assert_eq!(accepted, [1, 2, 3]);
        assert!(
            !reference.events("integration_started").is_empty()
                && !reference.events("fix_round").is_empty()
        );
        assert!(
            reference
                .dispatch_ids()
                .contains(&"c2-a1-rereview-1".to_string())
        );
        assert!(
            reference
                .started
                .iter()
                .any(|d| d["attempt"] == 2 && d["card"] == 3)
        );
        assert!(reference.paths().report.is_file());
        assert_invariants!(reference);
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for p in read_text(&log).split_whitespace() {
            *seen.entry(p.to_string()).or_insert(0) += 1;
        }
        let count = |p: &str| seen.get(p).copied().unwrap_or(0);
        // Every crash point is on this run's path except a cancel's (covered by the cancel
        // tests).
        let missing: Vec<&str> = POINTS
            .iter()
            .copied()
            .filter(|p| !NOT_ON_THE_PATH.contains(p) && count(p) == 0)
            .collect();
        assert_eq!(missing, Vec::<&str>::new(), "{seen:?}");

        let mut order: Vec<String> = POINTS
            .iter()
            .filter(|p| count(p) > 0)
            .map(|p| p.to_string())
            .collect();
        order.extend(
            seen.keys()
                .filter(|p| !POINTS.contains(&p.as_str()))
                .cloned(),
        ); // sorted
        let todo: Vec<(String, usize)> = order
            .iter()
            .flat_map(|p| (0..count(p)).map(move |k| (p.clone(), k)))
            .collect();
        let labels: BTreeSet<String> = todo.iter().map(|(p, k)| worker_label(p, *k)).collect();
        assert!(
            labels.len() == todo.len(),
            "two crash runs share a drive label after the slug: {order:?}"
        );

        let results_dir = TempRoot::new("lh crash results ");
        let test = drive::this_test();
        let queue = Mutex::new(todo.iter().cloned().collect::<VecDeque<_>>());
        let results: Mutex<BTreeMap<(String, usize), String>> = Mutex::new(BTreeMap::new());
        let abort = AtomicBool::new(false);
        let workers = parallel();
        let matrix_began = Instant::now();
        thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    let _abort = AbortOnPanic(&abort);
                    while !abort.load(Ordering::SeqCst) {
                        let Some((point, k)) = queue.lock().unwrap().pop_front() else {
                            break;
                        };
                        let text = run_worker(&test, &results_dir, &point, k, &expected);
                        results.lock().unwrap().insert((point, k), text);
                    }
                });
            }
        });
        let results = results.into_inner().unwrap();
        println!(
            "crash matrix: {} crash runs ({} points) on {workers} workers in {:.1} s; \
             the test with its reference run in {:.1} s",
            todo.len(),
            order.len(),
            matrix_began.elapsed().as_secs_f64(),
            began.elapsed().as_secs_f64()
        );
        let failed: Vec<String> = todo
            .iter()
            .filter_map(|(point, k)| {
                let text = results
                    .get(&(point.clone(), *k))
                    .map_or("(no result: the matrix stopped)", String::as_str);
                (text != "ok").then(|| format!("{point} #{}:\n{text}", k + 1))
            })
            .collect();
        assert!(failed.is_empty(), "{}", failed.join("\n\n"));
        assert_eq!(results.len(), seen.values().sum::<usize>());
    }
}

// --------------------------------------------------------------------------
// test_engine_recovery.py

mod resume_after_blocked {
    use super::*;

    #[test]
    fn test_blocked_provider_capacity_resumes_with_a_new_dispatch() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        w.script(1, "implement", [blocked("usage limit")]);
        let done = w.run().pop().unwrap();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "provider capacity"]"#)
        );
        relaunch(&w).ok(); // capacity is back
        let again = w.next();
        assert_json_eq!(
            pick(&again, &["action", "dispatch_id", "attempt"]),
            jv(r#"["dispatch", "c1-a1-implement-2", 1]"#)
        );
        assert_json_eq!(again["route"], w.started[0]["route"]);
        assert_eq!(w.events("outcome").len(), 1);
        w.act(&again);
        assert_eq!(w.run().pop().unwrap()["action"], "done");
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1]"));
    }

    #[test]
    fn test_blocked_environment_resumes_with_a_fresh_retry_budget() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        w.script(
            1,
            "implement",
            [blocked("environment"), blocked("environment")],
        );
        let done = w.run().pop().unwrap();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "environment"]"#)
        );
        relaunch(&w).ok(); // the tool was reinstalled
        w.script(
            1,
            "implement",
            [blocked("environment"), with_status("READY")],
        );
        w.run();
        let first4: Vec<String> = w.dispatch_ids().into_iter().take(4).collect();
        assert_eq!(
            first4,
            [
                "c1-a1-implement-1",
                "c1-a1-implement-2",
                "c1-a1-implement-3",
                "c1-a1-implement-4"
            ]
        );
        assert_eq!(w.events("outcome").len(), 1);
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1]"));
    }

    #[test]
    fn test_relaunch_without_an_outcome_keeps_the_retry_budget() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        w.script(
            1,
            "implement",
            [blocked("environment"), blocked("environment")],
        );
        let a = w.next();
        w.act(&a);
        let retry = w.next();
        assert_eq!(retry["dispatch_id"], "c1-a1-implement-2");
        relaunch(&w).ok(); // the launcher relaunched the supervisor
        w.act(&retry);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "qualifier"]),
            jv(r#"["done", "blocked", "environment"]"#)
        );
    }

    #[test]
    fn test_park_after_the_run_ended_raises() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        w.script(1, "implement", [blocked("usage limit")]);
        w.run();
        assert_fails!(w.cli(&os_args![
            "record",
            run_dir(&w),
            "park",
            "--card",
            "1",
            "--reason",
            "too late"
        ]));
        assert_empty!(w.events("parked"));
    }
}

/// Makes branch `name` in the world's repository with someone else's commit, back on main
/// after (`_unrelated_branch`); its tip.
#[track_caller]
fn unrelated_branch(w: &FakeSupervisor, name: &str) -> String {
    run_git(&w.repo, &["checkout", "--quiet", "-b", name]);
    let theirs = commit(
        &w.repo,
        &[("src/unrelated.txt", "not this run's\n")],
        "someone else's card",
    );
    run_git(&w.repo, &["checkout", "--quiet", "main"]);
    theirs
}

/// `start DECK --repo REPO` plus `extra`, which must succeed.
#[track_caller]
fn start_again(w: &FakeSupervisor, extra: &[&str]) -> Out {
    let mut args = vec![
        OsStr::new("start"),
        w.deck_path.as_os_str(),
        OsStr::new("--repo"),
        w.repo.as_os_str(),
    ];
    args.extend(extra.iter().map(OsStr::new));
    w.cli(&args).ok()
}

mod run_collisions {
    use super::*;

    #[test]
    fn test_start_returns_a_blocked_run_as_existing() {
        // Covers R25.
        let mut w = FakeSupervisor::new(&card_list(1));
        let first = w.start();
        w.script(1, "implement", [blocked("usage limit")]);
        w.run();
        let r = start_again(&w, &[]);
        assert_eq!(run_line(&r), first);
        assert!(
            r.stdout_text()
                .contains(&format!("existing run: {}", first.display())),
            "{r}"
        );
    }

    #[test]
    fn test_new_run_suffixes_a_branch_owned_by_another_run() {
        // Covers R26 (as refined later: another run's branch is suffixed, never reused).
        let mut w = FakeSupervisor::new(&card_list(1));
        let first = w.start();
        let first_meta = read_json(&first.join("run.json"));
        assert_json_eq!(
            pick(&first_meta, &["branch", "branch_requested"]),
            jv(r#"["leadhand/demo", "leadhand/demo"]"#)
        );
        let r = start_again(&w, &["--new"]);
        let second = run_line(&r);
        assert_ne!(second, first);
        let meta = read_json(&second.join("run.json"));
        assert_json_eq!(
            pick(&meta, &["branch", "branch_requested"]),
            jv(r#"["leadhand/demo-2", "leadhand/demo"]"#)
        );
        assert!(
            r.stdout_text().contains("run branch: leadhand/demo-2"),
            "{r}"
        );
        assert_ne!(meta["workspace"], first_meta["workspace"]);
        let third = run_line(&start_again(&w, &["--new"]));
        assert_eq!(
            read_json(&third.join("run.json"))["branch"],
            "leadhand/demo-3"
        );

        // the suffixed run works on its own branches and leaves the first run's alone
        w.run_dir = Some(second);
        let before = w.branch_tip("leadhand/demo");
        w.run();
        assert_eq!(w.branch_tip("leadhand/demo"), before);
        let accepted = one(w.events("accepted"));
        assert_eq!(accepted["sha"], w.branch_tip("leadhand/demo-2").as_str());
        assert_eq!(w.events("card_cut")[0]["branch"], "leadhand/demo-2--card-1");
    }

    #[test]
    fn test_start_after_a_finished_run_suffixes_its_branch() {
        let mut w = FakeSupervisor::new(&card_list(1));
        let first = w.start();
        w.run();
        append_event(&run_dir(&w), "outcome", outcome("complete", "", &[]));
        let finished_tip = w.branch_tip("leadhand/demo");
        let r = start_again(&w, &[]);
        let stdout = r.stdout_text();
        assert!(stdout.contains("run branch: leadhand/demo-2"), "{r}");
        assert!(!stdout.contains("existing run"), "{r}");
        assert!(
            stdout.contains(&format!("run: {}-2\n", first.display())),
            "{r}"
        );
        assert_eq!(w.branch_tip("leadhand/demo"), finished_tip);
        assert_eq!(w.branch_tip("leadhand/demo-2"), w.base);
    }

    #[test]
    fn test_interrupted_start_retry_uses_a_free_branch() {
        // A start that crashed just before run.json: what a real start leaves at that point
        // (its run branch, worktree, deck and settings), without run.json and the event log.
        let mut w = FakeSupervisor::new(&card_list(1));
        let crashed = w.start();
        for name in ["run.json", "events.jsonl", "state.lock"] {
            match fs::remove_file(crashed.join(name)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    panic!("removing {name}: {e}")
                }
                _ => {}
            }
        }
        w.run_dir = None;
        assert_eq!(
            run_git(&w.repo, &["rev-parse", "refs/heads/leadhand/demo"]),
            w.base
        );
        let retried = w.start();
        assert_ne!(retried, crashed);
        let meta = w.meta();
        assert_json_eq!(
            pick(&meta, &["branch", "branch_requested"]),
            jv(r#"["leadhand/demo-2", "leadhand/demo"]"#)
        );
        assert!(os_path(&meta["workspace"]).is_dir());
        w.run();
        assert_json_eq!(cards_of(&w.events("accepted")), jv("[1]"));
    }

    #[test]
    fn test_unrecorded_existing_branch_is_not_reused() {
        let mut w = FakeSupervisor::new(&card_list(1));
        run_git(&w.repo, &["checkout", "--quiet", "-b", "leadhand/demo"]);
        let theirs = commit(
            &w.repo,
            &[("theirs.txt", "not a run\n")],
            "someone else's work",
        );
        run_git(&w.repo, &["checkout", "--quiet", "main"]);
        w.start();
        assert_eq!(w.meta()["branch"], "leadhand/demo-2");
        assert_eq!(w.branch_tip("leadhand/demo-2"), w.base);
        w.run();
        assert_eq!(w.branch_tip("leadhand/demo"), theirs);
    }

    #[test]
    fn test_existing_card_branch_makes_its_run_branch_name_taken() {
        // R54: leadhand/demo itself is free, but leadhand/demo--card-1 is not this run's.
        let mut w = gated(&card_list(1));
        let theirs = unrelated_branch(&w, "leadhand/demo--card-1");
        w.start();
        assert_eq!(w.meta()["branch"], "leadhand/demo-2");
        let done = w.run().pop().unwrap();
        let reasons = done["reasons"].clone();
        assert_eq!(done["outcome"], "complete", "{reasons}");
        assert_eq!(w.events("card_cut")[0]["branch"], "leadhand/demo-2--card-1");
        let landed = lines(&run_git(
            &w.repo,
            &["diff", "--name-only", &w.base, "refs/heads/leadhand/demo-2"],
        ));
        assert!(
            !landed.contains(&"src/unrelated.txt".to_string()),
            "{landed:?}"
        );
        assert_eq!(w.branch_tip("leadhand/demo--card-1"), theirs);
        assert!(run_git(&w.repo, &["branch", "--list", "leadhand/demo"]).is_empty());
    }

    #[test]
    fn test_existing_final_branch_makes_its_run_branch_name_taken() {
        let mut w = FakeSupervisor::new(&card_list(1));
        unrelated_branch(&w, "leadhand/demo--card-final");
        w.start();
        assert_eq!(w.meta()["branch"], "leadhand/demo-2");
    }

    #[test]
    fn test_card_branch_the_run_did_not_cut_is_refused() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        assert_eq!(w.meta()["branch"], "leadhand/demo");
        // Someone makes the card branch after the run started, before the run cuts it.
        let theirs = unrelated_branch(&w, "leadhand/demo--card-1");
        assert_fails!(
            w.cli(&os_args!["next", run_dir(&w)]),
            "leadhand/demo--card-1"
        );
        assert_empty!(w.events("card_cut"));
        assert_eq!(w.branch_tip("leadhand/demo--card-1"), theirs);
        assert_eq!(w.branch_tip("leadhand/demo"), w.base);
    }
}

mod codex_stops {
    use super::*;

    #[test]
    fn test_hung_proxy_over_a_finished_runner_keeps_its_status() {
        let mut w = codex_world(&card_list(1), Options::default());
        w.start();
        let a = w.next();
        w.launch(&a);
        // the runner finishes; the proxy never reports
        w.run_proxy(
            &a,
            &Step {
                report: Some(false),
                ..Step::default()
            },
        );
        w.advance(66.0 * 60.0);
        let stop = w.next();
        assert_json_eq!(
            pick(&stop, &["action", "vendor", "record_result"]),
            jv(r#"["stop_worker", "codex", false]"#)
        );
        let res = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["status", "stopped"]),
            jv(r#"["READY", false]"#)
        );
    }

    #[test]
    fn test_codex_stop_before_the_proxy_ran_prewrites_the_request() {
        let mut w = codex_world(&card_list(1), Options::default());
        w.start();
        let a = w.next();
        w.launch(&a);
        w.advance(66.0 * 60.0);
        let stop = w.next();
        assert_eq!(stop["action"], "stop_worker");
        let wd = wd(&w, text(&a["dispatch_id"]));
        assert!(wd.join("request.json").is_file());
        let saved = read_json(&wd.join("result.json"));
        assert_eq!(saved["exit_reason"], "cancelled");
        let event = one(w.events("result_recorded"));
        assert_eq!(event["status"], "SPENT");
        let out = w.run_proxy(&a, &Step::default()); // a late proxy finds the cancelled result
        assert!(out.contains("session:"), "{out}");
        assert_empty!(codex_log(&w, Some("start")));
        assert_json_eq!(read_json(&wd.join("result.json")), saved);
    }
}

mod untested {
    use super::*;

    #[test]
    fn test_reviewer_spent_twice_parks_the_card() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        w.script(1, "review", [with_status("SPENT"), with_status("SPENT")]);
        w.run();
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"]
        );
        let parked = one(w.events("parked"));
        assert_eq!(
            parked["reason"],
            "review ended SPENT twice without a verdict"
        );
    }

    #[test]
    fn test_pinned_vendor_not_installed_parks_the_card() {
        let mut w = FakeSupervisor::new(&[codex_card(), card(2)]);
        w.start();
        w.run();
        let parked = one(w.events("parked"));
        assert_json_eq!(
            pick(&parked, &["card", "reason"]),
            jv(r#"[1, "pinned vendor not installed"]"#)
        );
        assert_eq!(w.dispatch_ids(), ["c2-a1-implement-1", "c2-a1-review-1"]);
    }

    /// Card 1's implementer crashes at `point` and is stopped at its time limit: SPENT, and
    /// escalated to attempt 2 (`worker_crash`). Gives the world and the result event.
    macro_rules! assert_worker_crash {
        ($point:expr) => {{
            let mut w = FakeSupervisor::new(&card_list(1));
            w.start();
            w.script(1, "implement", [crashing($point)]);
            let a = w.next();
            let worker = w.launch(&a);
            assert!(w.perform(&a, &worker).is_none());
            assert_eq!(w.next()["action"], "wait");
            w.advance(61.0 * 60.0);
            let stop = w.next();
            assert_json_eq!(
                pick(&stop, &["action", "record_result"]),
                jv(r#"["stop_worker", true]"#)
            );
            let event = result(&w, &a, &worker, "(stopped)", &[OsStr::new("--stopped")]);
            assert_eq!(event["status"], "SPENT");
            let nxt = w.next();
            assert_json_eq!(
                pick(&nxt, &["dispatch_id", "resume"]),
                jv(r#"["c1-a2-implement-1", null]"#)
            );
            (w, event)
        }};
    }

    #[test]
    fn test_worker_crash_before_commit_escalates() {
        let (w, event) = assert_worker_crash!("before_commit");
        assert_eq!(event["tip"], w.base.as_str());
        assert_eq!(w.branch_tip(CARD1), w.base);
    }

    #[test]
    fn test_worker_crash_after_commit_keeps_the_commit() {
        let (w, event) = assert_worker_crash!("after_commit");
        assert_ne!(event["tip"], w.base.as_str());
        assert_eq!(event["tip"], w.branch_tip(CARD1).as_str());
    }

    #[test]
    fn test_crash_before_result_record_writes_nothing() {
        let mut w = FakeSupervisor::new(&card_list(1));
        w.start();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("the worker's reply");
        let r = w.cli_with(
            &os_args![
                "record",
                run_dir(&w),
                "result",
                "--dispatch",
                &did,
                "--worker",
                &worker,
                "--reply",
                "-"
            ],
            CliOpts {
                crash: Some("before_result_record"),
                stdin: Some(reply.as_str()),
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 97, "{r}");
        assert!(!run_dir(&w).join(format!("cards/1/{did}")).exists());
        assert_empty!(w.charges());
        assert_empty!(w.events("result_recorded"));
        let event = recorded_with(
            &w,
            "result",
            &os_args!["--dispatch", &did, "--worker", &worker, "--reply", "-"],
            &reply,
        );
        assert_eq!(event["status"], "READY");
        assert_eq!(w.charges().len(), 1);
    }
}

// --------------------------------------------------------------------------
// test_state.py: the crash protocol's gate

mod crash_protocol {
    use super::*;

    #[test]
    fn test_a_crash_point_exits_97_only_when_testing_and_named() {
        // Covers limit L4 (test hooks need LEADHAND_TESTING=1).
        // `record result` passes before_result_record. It exits 97 there, recording nothing,
        // only under LEADHAND_TESTING=1 with LEADHAND_CRASH_AT naming that point.
        let cases: [(&[(&str, &str)], i32); 5] = [
            (
                &[
                    ("LEADHAND_TESTING", "1"),
                    ("LEADHAND_CRASH_AT", "before_result_record"),
                ],
                97,
            ),
            (&[("LEADHAND_CRASH_AT", "before_result_record")], 0),
            (
                &[
                    ("LEADHAND_TESTING", "0"),
                    ("LEADHAND_CRASH_AT", "before_result_record"),
                ],
                0,
            ),
            (&[("LEADHAND_TESTING", "1")], 0),
            (
                &[
                    ("LEADHAND_TESTING", "1"),
                    ("LEADHAND_CRASH_AT", "after_rebase"),
                ],
                0,
            ),
        ];
        for (overrides, code) in cases {
            let mut w = FakeSupervisor::new(&[card(1)]);
            w.start();
            let a = w.next();
            let did = text(&a["dispatch_id"]).to_string();
            let worker = w.launch(&a);
            let reply = w.perform(&a, &worker).expect("the worker's reply");
            let mut env = w
                .env
                .without("LEADHAND_TESTING")
                .without("LEADHAND_CRASH_AT");
            for (k, v) in overrides {
                env.set(k, v);
            }
            let r = common::cli_with(
                &os_args![
                    "record",
                    run_dir(&w),
                    "result",
                    "--dispatch",
                    &did,
                    "--worker",
                    &worker,
                    "--reply",
                    "-"
                ],
                &env,
                &w.tmp,
                Some(reply.as_str()),
                TIMEOUT,
            );
            assert_eq!(r.code, code, "env={overrides:?}: {r}");
            let recorded = field(&w.events("result_recorded"), "dispatch_id");
            let want = if code == 97 { ja![] } else { ja![did.as_str()] };
            assert_json_eq!(recorded, want, "env={overrides:?}");
            if code == 0 {
                assert_eq!(r.json()["status"], "READY", "env={overrides:?}");
            }
        }
    }
}
