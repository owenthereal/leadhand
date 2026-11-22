//! The Python contract suite's `contract_recovery`'s pieces that more than one file uses: the
//! module helpers `relaunch`, `resume`, `codex_log`, `codex_starts`, `codex_alive` and
//! `card_states`, the fixed commit date, and `StableSupervisor`, the crash matrix's world, which
//! `tests/rust_cli.rs` drives too.
//!
//! **`StableSupervisor`** (Python's FakeSupervisor subclass) is a `FakeSupervisor` made by
//! `stable_supervisor`, with its two overrides in the world's hooks (`stable_hooks`:
//! `work_label_fn` for `work_label`, `take_fn` for `_take`) and its methods as functions on
//! the world: `label`, `operator`, `drive`, `wait_for_orphans`, `final_state` and
//! `assert_invariants!` (`check_invariants`). A drive child that attaches the world
//! (`FakeSupervisor::attach_drive`) sets the hooks again with `stable_hooks`: they are
//! closures, which the world's state does not carry.
//!
//! `final_state` is a JSON object, so the crash matrix can hand the reference run's to its
//! workers, compared with `assert_json_eq!` (Python's dict equality). Its card keys are the
//! cards as JSON text (`1`, or `"final"`), as Python's dict keys are the card values.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use super::engine::{CODEX_CARD, NOTE, RUN, relaunch_env, requested, status_lines};
use super::git::run_git;
use super::json::{self, Json};
use super::supervisor::{
    CardOpts, CliOpts, FakeSupervisor, Options, Step, card, card_with, run_dir, text, truthy,
};
use super::{Out, index, read_jsonl, started_by, strs};

/// The commit date every Git call of the crash matrix's worlds gets.
pub const FIXED_DATE: &str = "2026-01-01T00:00:00+00:00";

/// The variables that fix the commit dates (`mock.patch.dict(os.environ, ...)` with
/// `patch_env`).
pub const DATES: [(&str, &str); 2] = [
    ("GIT_AUTHOR_DATE", FIXED_DATE),
    ("GIT_COMMITTER_DATE", FIXED_DATE),
];

/// `StableSupervisor.CARDS`.
pub const STABLE_CARDS: [i64; 3] = [1, 2, 3];

// --------------------------------------------------------------------------
// contract_recovery's module helpers

/// `run` on the run directory, with the fake `claude` as Claude (`relaunch`): its result,
/// unchecked.
#[track_caller]
pub fn relaunch(w: &FakeSupervisor) -> Out {
    let rd = run_dir(w);
    w.cli_with(
        &[OsStr::new("run"), rd.as_os_str()],
        CliOpts {
            env: relaunch_env(w),
            ..CliOpts::default()
        },
    )
}

/// What a relaunch does: a new supervisor session "supervisor-2", then `run`, which records
/// `resumed` (`resume(w)`).
#[track_caller]
pub fn resume(w: &mut FakeSupervisor) {
    resume_as(w, "supervisor-2");
}

/// `resume(w, session)`.
#[track_caller]
pub fn resume_as(w: &mut FakeSupervisor, session: &str) {
    w.session(session);
    relaunch(w).ok();
}

/// The fake Codex's log file.
fn codex_log_path(w: &FakeSupervisor) -> PathBuf {
    PathBuf::from(w.env.get("FAKE_CODEX_LOG").expect("FAKE_CODEX_LOG"))
}

/// The fake Codex's log lines, or those of one `event` (`codex_log(w, event)`); none while
/// the log does not exist.
#[track_caller]
pub fn codex_log(w: &FakeSupervisor, event: Option<&str>) -> Vec<Json> {
    let log = codex_log_path(w);
    let lines = if log.exists() {
        read_jsonl(&log)
    } else {
        Vec::new()
    };
    lines
        .into_iter()
        .filter(|r| event.is_none_or(|e| r.get("event").is_some_and(|v| v == e)))
        .collect()
}

/// Codex processes started per dispatch, from the fake Codex's log (`codex_starts`, a
/// Counter): each `start` line's dispatch is the folder of its `-o` file.
#[track_caller]
pub fn codex_starts(w: &FakeSupervisor) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for rec in codex_log(w, Some("start")) {
        let argv = strs(&rec["argv"]);
        let o = &argv[index(&argv, "-o") + 1];
        let dispatch = Path::new(o)
            .parent()
            .and_then(Path::file_name)
            .unwrap_or_else(|| panic!("an -o file with no folder: {o}"))
            .to_string_lossy()
            .into_owned();
        *out.entry(dispatch).or_insert(0) += 1;
    }
    out
}

/// Whether the Codex a `start` log line names is still that process (`codex_alive`).
#[track_caller]
pub fn codex_alive(rec: &Json) -> bool {
    let pid = rec["pid"]
        .as_i64()
        .and_then(|p| i32::try_from(p).ok())
        .unwrap_or_else(|| panic!("no pid: {rec}"));
    let when = rec["time"]
        .as_f64()
        .unwrap_or_else(|| panic!("no time: {rec}"));
    started_by(pid, when).is_some()
}

/// Each card's state, from `status RUN_DIR`'s card lines (`Card N: <title> · <state> · ...`)
/// (`card_states`); panics naming the status when a card has no line.
#[track_caller]
pub fn card_states(w: &FakeSupervisor, numbers: &[i64]) -> BTreeMap<i64, String> {
    let lines = status_lines(w);
    numbers
        .iter()
        .map(|&n| {
            let prefix = format!("Card {n}: Card number {n} · ");
            let line = lines
                .iter()
                .find_map(|ln| ln.strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("no line for card {n} in status:\n{}", lines.join("\n")));
            (n, line.split(" · ").next().unwrap().to_string())
        })
        .collect()
}

// --------------------------------------------------------------------------
// StableSupervisor

/// `StableSupervisor(test)`: three cards and the end-of-run gates. Card 1 asks a product
/// question and is parked; once card 2 is accepted the operator unparks it, and it is
/// integrated onto the moved run tip. Card 2 runs on Codex and has one fix round. Card 3
/// ends SPENT and is escalated.
///
/// A worker's work is keyed to its card, attempt and step (not its dispatch ID), and commit
/// dates are fixed (the caller's `patch_env(&DATES)`), so a step re-run under a new dispatch
/// ID makes the same commit and every run of the deck accepts the same SHAs.
#[track_caller]
pub fn stable_supervisor() -> FakeSupervisor {
    let mut w = FakeSupervisor::with_options(
        &[
            card(1),
            card_with(
                2,
                CardOpts {
                    route: Some(CODEX_CARD),
                    ..CardOpts::default()
                },
            ),
            card(3),
        ],
        Options {
            config: "preset = \"claude-only\"\n",
            installed: &["claude", "codex"],
            gates: true,
            ..Options::default()
        },
    );
    w.parks
        .insert(1, "product decision: storage format".to_string());
    let launch_log = w.tmp.join("launches.txt");
    std::fs::write(&launch_log, "")
        .unwrap_or_else(|e| panic!("writing {}: {e}", launch_log.display()));
    w.launch_log = Some(launch_log);
    stable_hooks(&mut w);
    w
}

/// StableSupervisor's overrides, as the world's hooks: `work_label` is `label`, and `_take`
/// scripts each card by its label (not the world's step queues).
pub fn stable_hooks(w: &mut FakeSupervisor) {
    w.work_label_fn = Some(Box::new(|w, a| label(w, text(&a["dispatch_id"]))));
    w.take_fn = Some(Box::new(|w, a| {
        let status = |s: &str| Step {
            status: Some(s.to_string()),
            ..Step::default()
        };
        match label(w, text(&a["dispatch_id"])).as_str() {
            "c1-a1-implement" => status("QUESTION"),
            "c3-a1-implement" => status("SPENT"),
            "c2-a1-review" => Step {
                verdict: Some("defects".to_string()),
                ..Step::default()
            },
            // production: a re-review follows
            "c2-a1-fix" => Step {
                files: Some(vec![(
                    "src/card2/fix.py".to_string(),
                    "FIXED = 'c2-a1-fix'\n".to_string(),
                )]),
                ..Step::default()
            },
            _ => Step::default(),
        }
    }));
}

/// Python's `str()` of a JSON value in an f-string: an int's digits, a string's text, `None`.
fn py_fmt(v: &Json) -> String {
    super::py_str(v)
}

/// A dispatch's work label: its card, attempt and step, never its dispatch ID
/// (`StableSupervisor.label`). A continue takes its continued dispatch's label; an implement
/// or answer after the card's unpark is `c<n>-a<attempt>-unparked`.
#[track_caller]
pub fn label(w: &FakeSupervisor, dispatch_id: &str) -> String {
    let e = requested(w, dispatch_id);
    let kind = text(&e["dispatch_kind"]).to_string();
    let n = &e["card"];
    if kind == "continue"
        && let Some(c) = e.get("continues").filter(|c| truthy(c))
    {
        return label(w, text(c));
    }
    if n.is_null() {
        let wave = e.get("wave").cloned().unwrap_or(Json::Null);
        return format!("run-w{}-{kind}", py_fmt(&wave));
    }
    let seq = e["seq"].as_f64().unwrap_or_else(|| panic!("no seq: {e}"));
    let unparked = w
        .events("unparked")
        .iter()
        .any(|u| json::py_eq(&u["card"], n) && u["seq"].as_f64().is_some_and(|s| s < seq));
    let attempt = py_fmt(&e["attempt"]);
    if (kind == "implement" || kind == "answer") && unparked {
        return format!("c{}-a{attempt}-unparked", py_fmt(n));
    }
    format!("c{}-a{attempt}-{kind}", py_fmt(n))
}

/// Unparks card 1 once card 2 is accepted (once) (`operator`): `unpark` through the world's
/// engine call, in the drive's environment, so a crash point it passes is armed too.
#[track_caller]
pub fn operator(w: &FakeSupervisor) {
    let events = w.events(None);
    let parked = events
        .iter()
        .any(|e| e["kind"] == "parked" && e["card"] == 1);
    let accepted = events
        .iter()
        .any(|e| e["kind"] == "accepted" && e["card"] == 2);
    if parked && accepted && !events.iter().any(|e| e["kind"] == "unparked") {
        let rd = run_dir(w);
        w.engine_cli(&[
            OsStr::new("unpark"),
            rd.as_os_str(),
            OsStr::new("--card"),
            OsStr::new("1"),
            OsStr::new("--note"),
            OsStr::new(NOTE),
        ])
        .ok();
    }
}

/// `drive_limit(w, 200)`.
#[track_caller]
pub fn drive(w: &mut FakeSupervisor) -> Json {
    drive_limit(w, 200)
}

/// `StableSupervisor.drive(limit)`: the operator, then `next`, until `done` (returned); a
/// `wait` sleeps 0.1 s, anything else is acted on. Panics when the run did not finish.
#[track_caller]
pub fn drive_limit(w: &mut FakeSupervisor, limit: usize) -> Json {
    for _ in 0..limit {
        operator(w);
        let a = w.next();
        if a["action"] == "done" {
            return a;
        }
        if a["action"] == "wait" {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        w.act(&a);
    }
    panic!("the run did not finish");
}

/// A runner killed by the crash may leave Codex running; a real restart comes later. Waits
/// (at most `timeout`) for every Codex the fake's log names to be gone (`wait_for_orphans`).
#[track_caller]
pub fn wait_for_orphans(w: &FakeSupervisor, timeout: Duration) {
    let end = Instant::now() + timeout;
    while codex_log(w, Some("start")).iter().any(codex_alive) && Instant::now() < end {
        thread::sleep(Duration::from_millis(50));
    }
}

/// The run's final state (`final_state`): the last outcome's `[outcome, qualifier]`, every
/// card's state, the accepted SHAs by card, and the run branch's tip and tree.
#[track_caller]
pub fn final_state(w: &FakeSupervisor) -> Json {
    let out = w.events("outcome").pop().expect("no outcome event");
    let cards: Vec<(String, Json)> = card_states(w, &STABLE_CARDS)
        .into_iter()
        .map(|(n, s)| (n.to_string(), Json::from(s)))
        .collect();
    let accepted: Vec<(String, Json)> = w
        .events("accepted")
        .iter()
        .map(|e| (json::dumps(&e["card"]), e["sha"].clone()))
        .collect();
    let obj =
        |pairs: &[(String, Json)]| Json::object(pairs.iter().map(|(k, v)| (k.as_str(), v.clone())));
    Json::object([
        (
            "outcome",
            Json::Arr(vec![out["outcome"].clone(), out["qualifier"].clone()]),
        ),
        ("cards", obj(&cards)),
        ("accepted", obj(&accepted)),
        ("head", Json::from(w.branch_tip(RUN))),
        (
            "tree",
            Json::from(run_git(
                &w.repo,
                &["rev-parse", &format!("refs/heads/{RUN}^{{tree}}")],
            )),
        ),
    ])
}

/// The items a Counter holds more than once (`[d for d, k in counter.items() if k > 1]`).
pub fn more_than_once<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|(_, k)| *k > 1)
        .map(|(d, _)| d)
        .collect()
}

/// The `unreachable commit` lines of `git fsck --unreachable --no-reflogs` in the world's
/// repository.
#[track_caller]
pub fn unreachable_commits(w: &FakeSupervisor) -> Vec<String> {
    run_git(
        &w.repo,
        &["fsck", "--unreachable", "--no-reflogs", "--no-progress"],
    )
    .lines()
    .filter(|ln| ln.starts_with("unreachable commit"))
    .map(str::to_string)
    .collect()
}

/// `StableSupervisor.check_invariants`: no dispatch recorded two workers or was launched
/// twice, no Codex started twice for one dispatch, and no commit was lost.
macro_rules! assert_invariants {
    ($w:expr) => {{
        use $crate::common::recovery::{codex_starts, more_than_once, unreachable_commits};
        let w: &$crate::common::supervisor::FakeSupervisor = &$w;
        let started = w.events("worker_started");
        let started = more_than_once(
            started
                .iter()
                .map(|e| $crate::common::supervisor::text(&e["dispatch_id"]).to_string()),
        );
        assert_eq!(
            started,
            Vec::<String>::new(),
            "a dispatch recorded two workers"
        );
        let log = w.launch_log.as_ref().expect("the world's launch log");
        let launched = more_than_once(
            $crate::common::read_text(log)
                .split_whitespace()
                .map(str::to_string),
        );
        assert_eq!(
            launched,
            Vec::<String>::new(),
            "a dispatch was launched twice"
        );
        let codex: Vec<String> = codex_starts(w)
            .into_iter()
            .filter(|(_, k)| *k > 1)
            .map(|(d, _)| d)
            .collect();
        assert_eq!(codex, Vec::<String>::new(), "Codex started twice");
        assert_eq!(
            unreachable_commits(w),
            Vec::<String>::new(),
            "a commit was lost"
        );
    }};
}
