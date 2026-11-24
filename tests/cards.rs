//! The card loop: cards, risk, decisions, reply files, the Codex result path, run branches and
//! untracked files, as `start`, `next`, `record`, `timer`, `unpark`, `cancel`, `status`,
//! `report`, `launch-line` and `dispatch` show them.
//!
//! The Rust form of the Python contract suite's `contract_cards` (converted from the Python
//! unit tests `test_engine_cards`, `test_engine_by_risk`, `test_engine_decisions`,
//! `test_engine_reply_file`, `test_engine_codex_result`, `test_engine_codex_e2e`,
//! `test_engine_run_branch` and `test_engine_untracked`, with the clock offset of
//! `test_test_hooks`). The happy path is `smoke::smoke::test_two_cards_accepted_in_order`.
//!
//! `FakeSupervisor` plays the run skill. A refusal is exit code 1 with `error: ...` on stderr
//! (`assert_fails!`). Codex routes run the dispatch action's own `dispatch` line with the fake
//! `codex` as Codex (LEADHAND_TEST_CODEX_BIN). Whether a Codex process is alive or gone is
//! judged from the fake's own log (its pid and the time of its `start` line), never from what
//! the binary recorded alone; branches, commits and trees are read from Git.
//!
//! The module helpers other files share (`codex_world`, `record`, `recorded`, `result`,
//! `run_line`, `relaunch_env`, `status_lines`, `commit_on`) are in `common::engine`.

#[macro_use]
mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::dispatch_world::{WAIT, wait_for, wait_for_with};
use common::engine::{RUN, files, record, recorded, result, with_status, with_verdict};
use common::git::{self, run_git, run_git_unchecked};
use common::json::Json;
use common::supervisor::{FakeSupervisor, NO_HOOKS, Proc, Step, os_path, run_dir, text, truthy};
use common::{
    Out, field, json_in, jv, lines, path_str as s, pick, read_json, read_jsonl, read_text,
    started_by, today,
};

/// A refusal: exit `code` (default 1), and for 1 `error: ...` on stderr (2 is the usage
/// error's). Gives the `Out`.
macro_rules! assert_fails {
    ($r:expr) => {
        assert_fails!($r, 1)
    };
    ($r:expr, $code:expr) => {{
        let r: Out = $r;
        let code: i32 = $code;
        assert_eq!(r.code, code, "{r}");
        if code == 1 {
            assert!(r.stderr_text().starts_with("error: "), "{r}");
        }
        r
    }};
}

// --------------------------------------------------------------------------
// Helpers

fn without_deadline(action: &Json) -> Json {
    Json::Obj(
        action
            .as_object()
            .unwrap_or_else(|| panic!("not an object: {action}"))
            .iter()
            .filter(|(k, _)| k != b"deadline")
            .cloned()
            .collect(),
    )
}

/// Act until the next action is a dispatch of `kind`; return it unacted.
#[track_caller]
fn drive_to(w: &mut FakeSupervisor, kind: &str) -> Json {
    for _ in 0..30 {
        let a = w.next();
        if a["action"] == "dispatch" && a["kind"] == kind {
            return a;
        }
        w.act(&a);
    }
    panic!("no {kind} dispatch");
}

fn codex_log(w: &FakeSupervisor) -> PathBuf {
    PathBuf::from(w.env.get("FAKE_CODEX_LOG").expect("FAKE_CODEX_LOG"))
}

/// The fake Codex's `start` log line (its pid and the time it was written), once it is there.
#[track_caller]
fn codex_start(w: &FakeSupervisor) -> Json {
    let log = codex_log(w);
    wait_for("the fake Codex to start", || {
        let records = if log.exists() {
            read_jsonl(&log)
        } else {
            Vec::new()
        };
        records
            .into_iter()
            .find(|r| r.get("event").is_some_and(|e| e == "start"))
    })
}

/// The `start` lines in the fake Codex's log (the log must exist).
#[track_caller]
fn codex_starts(w: &FakeSupervisor) -> usize {
    read_jsonl(&codex_log(w))
        .iter()
        .filter(|r| r.get("event").is_some_and(|e| e == "start"))
        .count()
}

/// Whether the Codex a `start` log line names is still that process.
#[track_caller]
fn codex_alive(rec: &Json) -> bool {
    let pid = rec["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("no pid: {rec}"));
    let when = rec["time"]
        .as_f64()
        .unwrap_or_else(|| panic!("no time: {rec}"));
    started_by(pid as i32, when).is_some()
}

/// runner.json's Codex record, once the runner has written it.
#[track_caller]
fn wait_runner_codex(w: &FakeSupervisor, did: &str, proxy: &Proc) -> Json {
    let runner = run_dir(w).join("workers").join(did).join("runner.json");
    wait_for_with(
        &format!("runner.json to name Codex for {did}"),
        WAIT,
        Some(proxy),
        || {
            let data = if runner.exists() {
                read_json(&runner)
            } else {
                Json::object([])
            };
            data.get("codex").filter(|c| truthy(c)).cloned()
        },
    )
}

/// The proxy's end, within `timeout` (Python's `communicate(timeout=...)`, which raises
/// TimeoutExpired, failing the test, when the proxy is still running).
#[track_caller]
fn finish_proxy(proxy: &Proc, timeout: Duration) {
    if proxy.communicate(timeout).is_none() {
        panic!("the proxy was still running {} s on", timeout.as_secs_f64());
    }
}

/// A sleeping Codex step.
fn sleeping(seconds: f64) -> Step {
    Step {
        sleep: Some(seconds),
        ..Step::default()
    }
}

/// The worker also commits on the run branch, then returns to its card branch.
#[track_caller]
fn stray(ws: &Path) {
    let current = run_git(ws, &["symbolic-ref", "--short", "HEAD"]);
    run_git(ws, &["checkout", "--quiet", RUN]);
    git::write(ws, &[("src/stray.txt", "unreviewed\n")]);
    run_git(ws, &["add", "--", "src/stray.txt"]);
    let mut args: Vec<&str> = NO_HOOKS.to_vec();
    args.extend(["commit", "--quiet", "-m", "stray commit on the run branch"]);
    run_git(ws, &args);
    run_git(ws, &["checkout", "--quiet", &current]);
}

#[track_caller]
fn run_log(w: &FakeSupervisor) -> Vec<String> {
    run_git(
        &w.repo,
        &[
            "log",
            "--format=%s",
            &format!("{}..refs/heads/{RUN}", w.base),
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

/// `^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z$`.
fn is_utc_ts(s: &str) -> bool {
    let b = s.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    if b.len() < 20 || !s.ends_with('Z') {
        return false;
    }
    let head = digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19);
    let tail = &s[19..s.len() - 1];
    head && (tail.is_empty()
        || tail
            .strip_prefix('.')
            .is_some_and(|f| !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit())))
}

// --------------------------------------------------------------------------
// Python's `test_engine_cards`

mod happy_path {
    use super::*;
    use crate::common::supervisor::card;

    #[test]
    fn test_requested_dispatch_is_repeated_until_started() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a1 = w.next();
        let a2 = w.next();
        assert_eq!(a1["action"], "dispatch", "{a1}");
        assert_json_eq!(without_deadline(&a1), without_deadline(&a2));
        assert_eq!(w.events("dispatch_requested").len(), 1);
        recorded(
            &w,
            "started",
            &os_args![
                "--dispatch",
                text(&a1["dispatch_id"]),
                "--worker",
                "agent-x"
            ],
        );
        let a3 = w.next();
        assert_eq!(a3["action"], "wait", "{a3}");
        assert_json_eq!(
            pick(&a3, &["dispatch_id", "worker_id"]),
            Json::from(vec![a1["dispatch_id"].clone(), "agent-x".into()])
        );
        assert!(truthy(&a3["reason"]), "{a3}");
        assert_eq!(w.events("dispatch_requested").len(), 1);
    }

    #[test]
    fn test_started_dispatch_returns_wait_then_stop_worker_after_deadline() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        w.advance(59.0 * 60.0);
        let wait = w.next();
        assert_eq!(wait["action"], "wait", "{wait}");
        w.advance(2.0 * 60.0);
        let stop = w.next();
        assert_json_eq!(
            stop,
            Json::object([
                ("action", "stop_worker".into()),
                ("dispatch_id", a["dispatch_id"].clone()),
                ("worker_id", worker.as_str().into()),
                ("vendor", "claude".into()),
                ("record_result", true.into()),
            ])
        );
        assert_json_eq!(w.next(), stop);
        let transcript = w.subagent_transcript(&worker);
        w.append_message(&transcript, "claude-sonnet-4");
        let event = result(
            &w,
            &a,
            &worker,
            "(the worker was stopped)",
            &os_args!["--stopped"],
        );
        assert_eq!(event["status"], "SPENT", "{event}");
        let charge = common::one(w.charges());
        assert!(truthy(&charge["possibly_incomplete"]), "{charge}");
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "dispatch_id", "resume"]),
            jv(r#"["dispatch", "c1-a2-implement-1", null]"#)
        );
    }

    #[test]
    fn test_duplicate_result_is_harmless() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        let e1 = result(&w, &a, &worker, &reply, &[]);
        let e2 = result(&w, &a, &worker, &reply, &[]);
        let e3 = result(&w, &a, &worker, "Status: SPENT\nsomething else", &[]);
        assert_json_eq!(e1, e2);
        assert_json_eq!(e1, e3);
        assert_eq!(w.events("result_recorded").len(), 1);
        assert_eq!(w.charges().len(), 1);
        assert_eq!(w.attempts().len(), 1);
        assert_eq!(w.next()["dispatch_id"], "c1-a1-review-1");
    }
}

/// A review step whose report file is exactly `text` (the worker writes it itself).
fn custom_report(text: &str) -> Step {
    let text = text.to_string();
    Step {
        report: Some(false),
        tamper: Some(Box::new(
            move |_w: &FakeSupervisor, _ws: &Path, a: &Json| {
                let report = os_path(&a["report"]);
                fs::create_dir_all(report.parent().unwrap()).unwrap();
                fs::write(&report, &text).unwrap();
            },
        )),
        ..Step::default()
    }
}

mod loop_ {
    use std::collections::BTreeMap;

    use super::*;
    use crate::common::engine::BY_RISK;
    use crate::common::supervisor::{CardOpts, Options, card, card_with};

    #[test]
    fn test_repeated_answers_and_reviews_get_distinct_ids() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.script(1, "answer", [with_status("QUESTION"), with_status("READY")]);
        w.script(
            1,
            "review",
            [
                Step {
                    blocked_by: Some("environment".into()),
                    ..with_status("BLOCKED")
                },
                with_verdict("clean"),
            ],
        );
        w.rulings
            .insert(1, common::str_vec(&["Use JSON.", "Keep the old name."]));
        w.run();
        assert_eq!(
            w.dispatch_ids(),
            [
                "c1-a1-implement-1",
                "c1-a1-answer-1",
                "c1-a1-answer-2",
                "c1-a1-review-1",
                "c1-a1-review-2"
            ]
        );
        let distinct: std::collections::BTreeSet<String> = w.dispatch_ids().into_iter().collect();
        assert_eq!(distinct.len(), 5);
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
    }

    #[test]
    fn test_by_risk_review_skips_low_risk_card() {
        let mut w = FakeSupervisor::with_options(
            &[
                card(1),
                card_with(
                    2,
                    CardOpts {
                        risk: "medium",
                        ..CardOpts::default()
                    },
                ),
                card_with(
                    3,
                    CardOpts {
                        hard: true,
                        ..CardOpts::default()
                    },
                ),
            ],
            Options {
                config: BY_RISK,
                ..Options::default()
            },
        );
        w.start();
        w.run();
        assert_eq!(
            w.dispatch_ids(),
            [
                "c1-a1-implement-1",
                "c2-a1-implement-1",
                "c2-a1-review-1",
                "c3-a1-implement-1",
                "c3-a1-review-1"
            ]
        );
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1, 2, 3]"));
        let (hard_impl, hard_review) = (&w.started[3], &w.started[4]);
        assert_eq!(hard_impl["route"]["model"], "opus", "{hard_impl}");
        assert_eq!(hard_review["route"]["model"], "opus", "{hard_review}");
    }

    #[test]
    fn test_defects_get_one_fix_round_then_rereview_on_production_change() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "review",
            [Step {
                findings: Some(common::str_vec(&[
                    "[high] src/widget.py:3 — off by one — fix it",
                ])),
                ..with_verdict("defects")
            }],
        );
        w.script(1, "fix", [files(&[("src/widget.py", "fixed\n")])]);
        w.script(1, "rereview", [with_verdict("clean")]);
        w.run();
        assert_eq!(
            w.dispatch_ids(),
            [
                "c1-a1-implement-1",
                "c1-a1-review-1",
                "c1-a1-fix-1",
                "c1-a1-rereview-1"
            ]
        );
        let [_impl, _review, fix, rereview] = common::n_of(w.started.clone());
        let started = w.events("worker_started");
        let impl_worker = started[0]["worker_id"].clone();
        let review_worker = started[1]["worker_id"].clone();
        assert_json_eq!(fix["resume"], Json::object([("worker_id", impl_worker)]));
        assert_json_eq!(
            rereview["resume"],
            Json::object([("worker_id", review_worker)])
        );
        let fix_brief = read_text(&os_path(&fix["brief"]));
        let report = run_dir(&w).join("cards/1/c1-a1-review-1/report.md");
        assert!(fix_brief.contains(s(&report)), "{fix_brief}");
        assert_eq!(w.events("fix_round").len(), 1);
        let accepted = common::one(w.events("accepted"));
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{}:src/widget.py", text(&accepted["sha"]))]
            ),
            "fixed"
        );
    }

    #[test]
    fn test_clean_verdict_with_medium_finding_counts_as_defects() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "review",
            [Step {
                findings: Some(common::str_vec(&["[medium] src/a.py:1 — leaks — close it"])),
                ..with_verdict("clean")
            }],
        );
        w.script(1, "fix", [files(&[("src/a.py", "closed\n")])]);
        w.run();
        assert_eq!(
            w.dispatch_ids()[..3],
            ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1"]
        );
        let verdicts = w.events("verdict_recorded");
        assert_eq!(verdicts[0]["verdict"], "defects", "{}", verdicts[0]);
    }

    /// Runs one independent card per report, its reviewer writing that report as is; each
    /// card's first review result (result_recorded), by card.
    #[track_caller]
    fn review_verdicts(reports: &[&str]) -> BTreeMap<i64, Json> {
        let cards: Vec<String> = (1..=reports.len() as i64).map(card).collect();
        let mut w = FakeSupervisor::new(&cards);
        w.start();
        for (n, report) in reports.iter().enumerate() {
            w.script(n as i64 + 1, "review", [custom_report(report)]);
        }
        w.run();
        let mut first = BTreeMap::new();
        for e in w.events("result_recorded") {
            if e["dispatch_kind"] == "review" {
                let n = e["card"].as_i64().expect("a card number");
                first.entry(n).or_insert(e);
            }
        }
        let want: Vec<i64> = (1..=reports.len() as i64).collect();
        assert_eq!(first.keys().copied().collect::<Vec<_>>(), want, "{first:?}");
        first
    }

    #[test]
    fn test_clean_rereview_noting_fixed_findings_stays_clean() {
        // Only outstanding findings (numbered or bulleted lines under ## Findings) overrule
        // the verdict.
        let heads = review_verdicts(&["Verdict: clean\n## Findings\nNone\n## Notes\n\
             1. [high] src/widget.py:1 — fixed; regression test passes.\n"]);
        let head = common::one(heads.into_values().collect());
        assert_eq!(head["verdict"], "clean", "{head}");
        assert!(head.get("cross_checked").is_none(), "{head}");
    }

    #[test]
    fn test_clean_verdict_cross_check_reads_only_findings_lines() {
        let heads = review_verdicts(&[
            "Verdict: clean\n## Findings\n2. [High] src/a.py:3 — leaks — close it\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\n  1) [medium] src/a.py:3 — x — y\n",
            "Verdict: clean\n## Findings\n1. [low] src/a.py:3 — nit — rename\n\
             ## Notes\n- [high] earlier round: fixed\n1. [medium] fixed\n",
            "Verdict: clean\n[high] src/a.py:3 outside any section\n## Findings\nNone\n",
        ]);
        let verdicts: Vec<Json> = [1, 2, 3, 4]
            .iter()
            .map(|n| heads[n]["verdict"].clone())
            .collect();
        assert_json_eq!(
            Json::Arr(verdicts),
            jv(r#"["defects", "defects", "clean", "clean"]"#)
        );
    }

    #[test]
    fn test_clean_verdict_cross_check_counts_bullet_findings_lines() {
        let heads = review_verdicts(&[
            "Verdict: clean\n## Findings\n- [high] src/a.py:3 — leaks — close it\n",
            "Verdict: clean\n## Findings\n  * [Medium] src/a.py:3 — x — y\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\n- [low] src/a.py:3 — nit — rename\n\
             ## Notes\n- [high] earlier round: fixed\n* [medium] fixed\n",
        ]);
        let verdicts: Vec<Json> = [1, 2, 3]
            .iter()
            .map(|n| heads[n]["verdict"].clone())
            .collect();
        assert_json_eq!(
            Json::Arr(verdicts),
            jv(r#"["defects", "defects", "clean"]"#)
        );
    }

    #[test]
    fn test_which_paths_make_a_fix_test_only() {
        // Ruling R13 (Python's `test_gitops`'s is_test_or_doc_path): test and doc folders count at
        // the top level only; test files and docs by name anywhere; dependency and build inputs are
        // production. A fix touching only the first kind is a `check_diff`, else a rereview.
        let yes = [
            "tests/test_x.py",
            "test/a.c",
            "spec/a_spec.rb",
            "docs/guide.html",
            "tests/deep/helper.py",
            "test_foo.py",
            "src/foo_test.go",
            "src/foo.test.ts",
            "src/foo.spec.js",
            "README.md",
            "notes/usage.rst",
            "notes/todo.txt",
        ];
        let no = [
            "src/main.py",
            "scripts/build.sh",
            "src/contest.py",
            "src/latest.py",
            "docs",
            "tests",
            "src/tester.py",
            "pkg/tests/helper.py",
            "pkg/spec/x.py",
            "app/docs/views.py",
            "requirements.txt",
            "requirements-dev.txt",
            "tests/requirements.txt",
            "constraints.txt",
            "CMakeLists.txt",
            "src/CMakeLists.txt",
        ];

        let after_the_fix = |paths: &[&str]| -> Json {
            let mut w = FakeSupervisor::new(&[card(1)]);
            w.start();
            w.script(1, "review", [with_verdict("defects")]);
            let fixed: Vec<(&str, &str)> = paths.iter().map(|p| (*p, "fixed\n")).collect();
            w.script(1, "fix", [files(&fixed)]);
            for _ in 0..30 {
                let a = w.next();
                if a["action"] == "check_diff" || a.get("kind").is_some_and(|k| k == "rereview") {
                    assert!(
                        w.dispatch_ids().iter().any(|d| d == "c1-a1-fix-1"),
                        "{:?}",
                        w.dispatch_ids()
                    );
                    return a;
                }
                w.act(&a);
            }
            panic!("no action after the fix");
        };

        let a = after_the_fix(&yes);
        assert_eq!(a["action"], "check_diff", "{a}");
        let mut paths = common::strs(&a["paths"]);
        paths.sort();
        let mut want = common::str_vec(&yes);
        want.sort();
        assert_eq!(paths, want);
        for path in no {
            let a = after_the_fix(&[path]);
            assert_json_eq!(
                pick(&a, &["action", "kind"]),
                jv(r#"["dispatch", "rereview"]"#),
                "path={path}: {a}"
            );
        }
    }

    #[test]
    fn test_test_only_fix_returns_check_diff() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "review",
            [Step {
                findings: Some(common::str_vec(&[
                    "[medium] tests/test_widget.py — weak — assert more",
                ])),
                ..with_verdict("defects")
            }],
        );
        w.script(
            1,
            "fix",
            [files(&[
                ("tests/test_widget.py", "assert more\n"),
                ("docs/notes.md", "note\n"),
            ])],
        );
        let mut a = Json::Null;
        for _ in 0..30 {
            a = w.next();
            if a["action"] == "check_diff" {
                break;
            }
            w.act(&a);
        }
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1"]
        );
        assert_json_eq!(
            pick(&a, &["card", "dispatch_id"]),
            jv(r#"[1, "c1-a1-fix-1"]"#)
        );
        let mut paths = common::strs(&a["paths"]);
        paths.sort();
        assert_eq!(paths, ["docs/notes.md", "tests/test_widget.py"]);
        let diff = os_path(&a["diff"]);
        assert!(
            diff.starts_with(run_dir(&w).join("cards/1")),
            "{}",
            diff.display()
        );
        assert!(read_text(&diff).contains("+assert more"));
        assert_json_eq!(w.next(), a); // repeated until recorded
        recorded(
            &w,
            "diff-check",
            &os_args![
                "--card",
                "1",
                "--ok",
                "yes",
                "--note",
                "only tests and docs"
            ],
        );
        let done = w.run();
        assert_eq!(done.last().unwrap()["action"], "done");
        assert!(!w.dispatch_ids().iter().any(|d| d == "c1-a1-rereview-1"));
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
    }

    #[test]
    fn test_defects_after_fix_round_escalate_with_fresh_worker() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "review",
            [with_verdict("defects"), with_verdict("clean")],
        );
        w.script(1, "fix", [files(&[("src/widget.py", "still wrong\n")])]);
        w.script(1, "rereview", [with_verdict("defects")]);
        w.run();
        assert_eq!(
            w.dispatch_ids(),
            [
                "c1-a1-implement-1",
                "c1-a1-review-1",
                "c1-a1-fix-1",
                "c1-a1-rereview-1",
                "c1-a2-implement-1",
                "c1-a2-review-1"
            ]
        );
        let second = w.started[4].clone();
        assert!(second["resume"].is_null(), "{second}");
        assert_json_eq!(second["attempt"], Json::from(2));
        assert_json_eq!(
            second["route"],
            jv(r#"{"vendor": "claude", "model": "sonnet", "effort": "high"}"#)
        );
        let workers = field(&w.events("worker_started"), "worker_id");
        let workers = workers.as_array().unwrap();
        assert!(!workers[..4].contains(&workers[4]), "{workers:?}");
        let brief = read_text(&os_path(&second["brief"]));
        let cards = run_dir(&w).join("cards/1");
        assert!(
            brief.contains(s(&cards.join("c1-a1-fix-1/report.md"))),
            "{brief}"
        );
        assert!(
            brief.contains(s(&cards.join("c1-a1-rereview-1/report.md"))),
            "{brief}"
        );
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
    }

    #[test]
    fn test_spent_escalates_and_exhausted_ladder_parks() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "implement",
            [
                with_status("SPENT"),
                with_status("SPENT"),
                with_status("SPENT"),
            ],
        );
        let actions = w.run();
        assert_eq!(
            w.dispatch_ids(),
            [
                "c1-a1-implement-1",
                "c1-a2-implement-1",
                "c1-a3-implement-1"
            ]
        );
        let efforts: Vec<Json> = w
            .started
            .iter()
            .map(|a| a["route"]["effort"].clone())
            .collect();
        assert_json_eq!(Json::Arr(efforts), jv(r#"["medium", "high", "high"]"#));
        assert_eq!(w.started[2]["route"]["model"], "opus");
        let parked = common::one(w.events("parked"));
        assert_json_eq!(
            pick(&parked, &["card", "reason"]),
            jv(r#"[1, "ladder exhausted"]"#)
        );
        assert_eq!(actions.last().unwrap()["action"], "done");
        let accepted = w.events("accepted");
        assert!(accepted.is_empty(), "{accepted:?}");
    }

    #[test]
    fn test_usage_limit_ends_blocked_without_switching_vendor() {
        let mut w = FakeSupervisor::with_options(
            &[card(1), card(2)],
            Options {
                installed: &["claude", "codex"],
                config: "[roles.implementer]\nprefer = \"claude\"\n",
                ..Options::default()
            },
        );
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                blocked_by: Some("usage limit".into()),
                ..with_status("BLOCKED")
            }],
        );
        let actions = w.run();
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1"]);
        let done = actions.last().unwrap().clone();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "qualifier"]),
            jv(r#"["done", "blocked", "provider capacity"]"#)
        );
        let outcome = common::one(w.events("outcome"));
        assert_json_eq!(
            pick(&outcome, &["outcome", "qualifier"]),
            jv(r#"["blocked", "provider capacity"]"#)
        );
        assert_json_eq!(w.next(), done);
        assert_eq!(w.events("dispatch_requested").len(), 1);
    }

    #[test]
    fn test_other_blocked_retries_once_then_ends_blocked_environment() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let env_blocked = || Step {
            blocked_by: Some("environment".into()),
            ..with_status("BLOCKED")
        };
        w.script(1, "implement", [env_blocked(), env_blocked()]);
        let actions = w.run();
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-implement-2"]);
        assert_json_eq!(w.started[0]["route"], w.started[1]["route"]);
        assert_json_eq!(w.started[1]["attempt"], Json::from(1));
        let done = actions.last().unwrap();
        assert_json_eq!(
            pick(done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "environment"]"#)
        );
    }

    #[test]
    fn test_question_ruling_resumes_worker() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        result(&w, &a, &worker, &reply, &[]);
        let ask = w.next();
        assert_json_eq!(
            pick(&ask, &["action", "card", "dispatch_id", "worker_id"]),
            Json::from(vec![
                "answer".into(),
                1.into(),
                "c1-a1-implement-1".into(),
                worker.as_str().into()
            ])
        );
        assert!(
            text(&ask["question"]).contains("Which storage format"),
            "{ask}"
        );
        assert!(os_path(&ask["report"]).is_file(), "{ask}");
        assert_json_eq!(w.next(), ask);
        recorded(
            &w,
            "ruling",
            &os_args!["--card", "1", "--text", "Use JSON; it is simpler."],
        );
        assert_fails!(record(
            &w,
            "ruling",
            &os_args!["--card", "1", "--text", "again"]
        ));
        let ans = w.next();
        assert_json_eq!(
            pick(&ans, &["action", "kind", "dispatch_id", "resume"]),
            Json::from(vec![
                "dispatch".into(),
                "answer".into(),
                "c1-a1-answer-1".into(),
                Json::object([("worker_id", worker.as_str().into())])
            ])
        );
        let brief = read_text(&os_path(&ans["brief"]));
        assert!(
            brief.lines().any(|l| l == "> Use JSON; it is simpler."),
            "{brief}"
        );
        let ruling = common::one(w.events("ruling"));
        assert_eq!(ruling["text"], "Use JSON; it is simpler.", "{ruling}");
        w.run();
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
    }

    #[test]
    fn test_parked_card_skips_dependents_and_continues_independent() {
        let mut w = FakeSupervisor::new(&[
            card(1),
            card_with(
                2,
                CardOpts {
                    depends: &[1],
                    ..CardOpts::default()
                },
            ),
            card(3),
        ]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks
            .insert(1, "product decision: storage format".to_string());
        let actions = w.run();
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c3-a1-implement-1", "c3-a1-review-1"]
        );
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[3]"));
        let cuts: BTreeMap<i64, Json> = w
            .events("card_cut")
            .into_iter()
            .map(|e| (e["card"].as_i64().unwrap(), e))
            .collect();
        assert!(!cuts.contains_key(&2), "{cuts:?}");
        assert_eq!(cuts[&3]["base"], w.base.as_str());
        assert_eq!(
            run_git_unchecked(
                &w.repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/leadhand/demo--card-2"
                ]
            ),
            ""
        );
        let done = actions.last().unwrap();
        assert_eq!(done["action"], "done", "{done}");
        let reasons: BTreeMap<i64, Json> = done["cards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["card"].as_i64().unwrap(), c.clone()))
            .collect();
        assert_eq!(reasons[&1]["state"], "parked");
        assert!(text(&reasons[&1]["reason"]).contains("product decision"));
        assert_eq!(reasons[&2]["state"], "pending");
        assert!(text(&reasons[&2]["reason"]).contains("parked card 1"));
        assert_eq!(reasons[&3]["state"], "accepted");
    }
}

mod start {
    use super::*;
    use crate::common::engine::{CODEX_CARD, codex_world, run_line};
    use crate::common::rundir::{append_event, outcome};
    use crate::common::supervisor::{CardOpts, Options, card, card_with};
    use crate::common::{TempRoot, cli, realpath};

    #[test]
    fn test_run_folders_are_dated_and_suffixed() {
        // <docs_dir>/runs/<local date>-<slug>, then the first free -2, -3 (Python's `test_state`).
        let mut w = FakeSupervisor::new(&[card(1)]);
        let before = today();
        let first = w.start();
        let second = w.start_new();
        let third = w.start_new();
        let after = today();
        assert_eq!(first.parent().unwrap(), w.docs.join("runs"));
        let name = first.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            [before, after]
                .iter()
                .any(|d| name == format!("{d}-demo-deck")),
            "{name}"
        );
        let parent = first.parent().unwrap();
        assert_eq!(
            [second.clone(), third.clone()],
            [
                parent.join(format!("{name}-2")),
                parent.join(format!("{name}-3"))
            ]
        );
        for run in [&first, &second, &third] {
            assert!(run.join("run.json").is_file(), "{}", run.display());
        }
    }

    #[test]
    fn test_every_event_line_carries_seq_ts_and_kind() {
        // events.jsonl: one JSON object per line, numbered from 1 with no gap, each with its
        // UTC time and its kind (Python's `test_state`).
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        w.start();
        w.run();
        assert!(fs::read(w.paths().events).unwrap().ends_with(b"\n"));
        let events = read_jsonl(&w.paths().events);
        assert!(events.len() > 10, "{}", events.len());
        assert_json_eq!(
            field(&events, "seq"),
            Json::Arr((1..=events.len() as i64).map(Json::from).collect())
        );
        assert_eq!(events[0]["kind"], "run_started");
        for e in &events {
            assert!(is_utc_ts(text(&e["ts"])), "{e}");
            assert!(matches!(e["kind"], Json::Str(_)), "{e}");
        }
    }

    #[test]
    fn test_a_whole_run_leaves_no_temporary_files() {
        // Every JSON file and report is written through a temporary file in the same folder
        // (`.<name>.<random>`, `.tmp` for JSON) and renamed: none survives in the run
        // directory, its card folders or a Codex worker folder (Python's `test_state`). Read from
        // the filesystem.
        let mut w = codex_world(
            &[
                card_with(
                    1,
                    CardOpts {
                        route: Some(CODEX_CARD),
                        ..CardOpts::default()
                    },
                ),
                card(2),
            ],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        w.session("supervisor-1");
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                commit: Some(false),
                ..Step::default()
            }],
        );
        assert_eq!(w.run().last().unwrap()["action"], "done");
        assert!(w.paths().report.is_file());
        let rd = run_dir(&w);
        let mut workers: Vec<PathBuf> = fs::read_dir(rd.join("workers"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        workers.sort();
        assert!(!workers.is_empty());
        assert!(
            workers.iter().all(|wd| wd.join("result.json").is_file()),
            "{workers:?}"
        );
        let mut left = Vec::new();
        let mut stack = vec![rd.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                if name.ends_with(".tmp") || name.starts_with('.') {
                    left.push(p.strip_prefix(&rd).unwrap().display().to_string());
                }
                if fs::symlink_metadata(&p).unwrap().is_dir() {
                    stack.push(p);
                }
            }
        }
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn test_start_returns_existing_unfinished_run_unless_new() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        let first = w.start();
        let meta = w.meta();
        let run_id = text(&meta["run_id"]);
        assert!(
            run_id.len() == 12
                && run_id
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{run_id}"
        );
        assert_eq!(
            meta["deck_source"].as_os(),
            Some(realpath(&w.deck_path).into_os_string())
        );
        assert_eq!(meta["base_sha"], w.base.as_str());
        assert_eq!(meta["branch"], "leadhand/demo");
        assert_json_eq!(meta["supervisor_sessions"], jv("[]"));
        assert_eq!(meta["slug"], "demo-deck");
        for key in ["repo", "deck_digest", "base_ref", "workspace", "created_at"] {
            assert!(meta.get(key).is_some(), "{key}: {meta}");
        }
        assert!(os_path(&meta["workspace"]).is_dir());
        assert_eq!(
            fs::read(first.join("deck.md")).unwrap(),
            fs::read(&w.deck_path).unwrap()
        );
        assert_json_eq!(field(&w.events(None), "kind"), jv(r#"["run_started"]"#));

        let r = w
            .cli(&os_args!["start", w.deck_path, "--repo", w.repo])
            .ok();
        assert_eq!(run_line(&r), first);
        assert!(
            lines(&r.stdout_text()).contains(&format!("existing run: {}", s(&first))),
            "{r}"
        );

        let r = w
            .cli(&os_args!["start", w.deck_path, "--repo", w.repo, "--new"])
            .ok();
        let fresh = run_line(&r);
        assert!(
            lines(&r.stdout_text()).contains(&format!("run: {}", s(&fresh))),
            "{r}"
        );
        assert_ne!(fresh, first);
        assert!(fresh.join("run.json").is_file());

        for run in [&first, &fresh] {
            append_event(run, "outcome", outcome("incomplete", "", &[]));
        }
        let third = run_line(
            &w.cli(&os_args!["start", w.deck_path, "--repo", w.repo])
                .ok(),
        );
        assert!(third != first && third != fresh, "{}", third.display());

        // the CLI, from an unrelated directory
        let elsewhere = TempRoot::new("lh elsewhere ");
        let r = cli(
            &os_args!["start", w.deck_path, "--repo", w.repo],
            &w.env,
            &elsewhere,
        )
        .ok();
        assert!(
            lines(&r.stdout_text()).contains(&format!("existing run: {}", s(&third))),
            "{r}"
        );
    }

    #[test]
    fn test_start_never_reuses_an_unfinished_run_of_another_clone() {
        // Two clones share the docs_dir and the deck file: start against the second clone
        // must create its own run, not return the first clone's.
        let mut w = FakeSupervisor::new(&[card(1)]);
        let first = w.start();
        let clone = w.tmp.join("second clone");
        run_git(&w.tmp, &["clone", "--quiet", s(&w.repo), s(&clone)]);
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", clone]).ok();
        let second = run_line(&r);
        assert_ne!(second, first, "{r}");
        assert!(!r.stdout_text().contains("existing run"), "{r}");
        let meta = read_json(&second.join("run.json"));
        assert_eq!(
            meta["repo"].as_os(),
            Some(realpath(&clone).into_os_string())
        );
        assert!(realpath(&os_path(&meta["workspace"])).starts_with(realpath(&clone)));
        // each clone still finds its own unfinished run
        assert!(
            w.cli(&os_args!["start", w.deck_path, "--repo", w.repo])
                .ok()
                .stdout_text()
                .contains(&format!("existing run: {}", s(&first)))
        );
        assert!(
            w.cli(&os_args!["start", w.deck_path, "--repo", clone])
                .ok()
                .stdout_text()
                .contains(&format!("existing run: {}", s(&second)))
        );
    }

    #[test]
    fn test_start_never_reuses_another_runs_worktree() {
        // Covers R26 (as refined later: another run's worktree is never reused).
        // docs_dir changed between two same-day runs of the same deck: the second run's
        // directory has the same name, so its default worktree path is the first run's.
        let mut w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        let first = w.start();
        let a = w.next();
        assert_eq!(a["action"], "dispatch", "{a}");
        w.launch(&a); // the first run's worker is active
        let ws1 = w.workspace();
        let branch1 = run_git(&ws1, &["symbolic-ref", "--short", "HEAD"]);
        let head1 = run_git(&ws1, &["rev-parse", "HEAD"]);
        assert_eq!(branch1, "leadhand/demo--card-1");

        fs::write(
            &w.config_file,
            format!("docs_dir = \"{}\"\n", s(&w.tmp.join("other docs"))),
        )
        .unwrap();
        let second = run_line(
            &w.cli(&os_args!["start", w.deck_path, "--repo", w.repo])
                .ok(),
        );
        assert_eq!(second.file_name(), first.file_name());
        let meta = read_json(&second.join("run.json"));
        let ws2 = os_path(&meta["workspace"]);
        assert_ne!(realpath(&ws2), realpath(&ws1));
        let first_name = first.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            ws2.file_name().unwrap().to_str().unwrap(),
            format!("{first_name}-2")
        );
        assert_eq!(
            run_git(&ws2, &["symbolic-ref", "--short", "HEAD"]),
            text(&meta["branch"])
        );
        w.cli(&os_args!["next", second]).ok();
        // the first run's worktree is untouched
        assert_eq!(run_git(&ws1, &["symbolic-ref", "--short", "HEAD"]), branch1);
        assert_eq!(run_git(&ws1, &["rev-parse", "HEAD"]), head1);
        assert_eq!(
            run_git(&ws2, &["symbolic-ref", "--short", "HEAD"]),
            format!("{}--card-1", text(&meta["branch"]))
        );
        // the second run's evidence base worktree is its own too
        w.run_dir = Some(second.clone());
        assert_eq!(w.run().last().unwrap()["outcome"], "complete");
        let ws2_name = ws2.file_name().unwrap().to_str().unwrap();
        assert!(
            ws2.parent()
                .unwrap()
                .join(format!("{ws2_name}--base"))
                .is_dir()
        );
        assert!(
            !ws1.parent()
                .unwrap()
                .join(format!("{first_name}--base"))
                .exists()
        );
        assert_eq!(run_git(&ws1, &["symbolic-ref", "--short", "HEAD"]), branch1);
    }

    #[test]
    fn test_start_refuses_unsupported_settings_and_scout_cards() {
        let w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: "[after_run]\nstep = \"pr\"\n",
                ..Options::default()
            },
        );
        let r = assert_fails!(w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]));
        assert!(
            r.stderr_text()
                .contains("not supported in this version: after_run.step = pr"),
            "{r}"
        );
        assert!(!w.docs.join("runs").exists());

        let sc = FakeSupervisor::new(&[card_with(
            1,
            CardOpts {
                role: "scout",
                ..CardOpts::default()
            },
        )]);
        let r = assert_fails!(sc.cli(&os_args!["start", sc.deck_path, "--repo", sc.repo]));
        assert!(
            r.stderr_text()
                .contains("not supported in this version: scout cards"),
            "{r}"
        );
        assert!(!sc.docs.join("runs").exists());
    }

    #[test]
    fn test_check_and_start_refuse_step_limits_claude_workers_cannot_honour() {
        let w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: "[limits]\nimplementer = { steps = 90, minutes = 60 }\n",
                ..Options::default()
            },
        );
        let msg = "not supported in this version: limits.implementer.steps = 90 \
                   (Claude workers use the agent file's maxTurns = 150)";
        let r = w.cli(&os_args!["check", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        assert!((r.stdout_text() + &r.stderr_text()).contains(msg), "{r}");
        let r = assert_fails!(w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]));
        assert!(r.stderr_text().contains(msg), "{r}");
        assert!(!w.docs.join("runs").exists());
    }

    #[test]
    fn test_deck_changed_since_start_refuses() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let digest = text(&w.meta()["deck_digest"]).to_string();
        let mut deck = read_text(&w.deck_path);
        deck.push_str("\nContext: edited after the start.\n");
        fs::write(&w.deck_path, deck).unwrap();
        let r = assert_fails!(w.cli(&os_args!["next", run_dir(&w)]));
        assert_eq!(
            lines(&r.stderr_text())[0],
            format!("error: deck changed since start (expected {digest})")
        );
        let requested = w.events("dispatch_requested");
        assert!(requested.is_empty(), "{requested:?}");
    }
}

mod records {
    use super::*;
    use crate::common::engine::recorded_with;
    use crate::common::supervisor::card;

    #[test]
    fn test_reply_without_status_blocks_and_keeps_raw() {
        // Covers P7.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let raw = "I changed some things.\nIt went fine, mostly.\n";
        w.script(
            1,
            "implement",
            [Step {
                report: Some(false),
                reply: Some(raw.into()),
                ..Step::default()
            }],
        );
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        let event = result(&w, &a, &worker, &reply, &[]);
        assert_json_eq!(
            pick(&event, &["status", "blocked_by"]),
            jv(r#"["BLOCKED", ""]"#)
        );
        assert!(truthy(&event["raw_kept"]), "{event}");
        let kept = run_dir(&w).join("cards/1/c1-a1-implement-1/reply.md");
        assert_eq!(read_text(&kept), raw);
        let retry = w.next();
        assert_json_eq!(
            pick(&retry, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a1-implement-2"]"#)
        );
    }

    #[test]
    fn test_turn_limit_reply_is_spent() {
        // Covers P7.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                report: Some(false),
                reply: Some(
                    "NOTE: this agent stopped at its 150-turn limit before finishing.\n".into(),
                ),
                ..Step::default()
            }],
        );
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        let event = result(&w, &a, &worker, &reply, &[]);
        assert_eq!(event["status"], "SPENT", "{event}");
    }

    #[test]
    fn test_invalid_record_transition_raises() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        assert_fails!(record(
            &w,
            "result",
            &os_args![
                "--dispatch",
                "c9-a1-implement-1",
                "--worker",
                "x",
                "--reply",
                "Status: READY"
            ]
        ));
        assert_fails!(record(
            &w,
            "started",
            &os_args!["--dispatch", "c1-a1-implement-1", "--worker", "x"]
        ));
        let a = w.next();
        let did = text(&a["dispatch_id"]);
        // no worker started yet
        assert_fails!(record(
            &w,
            "result",
            &os_args![
                "--dispatch",
                did,
                "--worker",
                "x",
                "--reply",
                "Status: READY"
            ]
        ));
        assert_fails!(record(
            &w,
            "ruling",
            &os_args!["--card", "1", "--text", "do it"]
        )); // no question pending
        assert_fails!(record(
            &w,
            "diff-check",
            &os_args!["--card", "1", "--ok", "yes", "--note", ""]
        )); // no diff check pending
        assert_fails!(record(
            &w,
            "park",
            &os_args!["--card", "7", "--reason", "no such card"]
        ));
        assert_fails!(record(&w, "nonsense", &[]), 2); // not a record kind
        recorded(
            &w,
            "started",
            &os_args!["--dispatch", did, "--worker", "w1"],
        );
        recorded(
            &w,
            "started",
            &os_args!["--dispatch", did, "--worker", "w1"],
        ); // repeat: harmless
        assert_fails!(record(
            &w,
            "started",
            &os_args!["--dispatch", did, "--worker", "w2"]
        ));
        assert_fails!(record(
            &w,
            "result",
            &os_args![
                "--dispatch",
                did,
                "--worker",
                "w2",
                "--reply",
                "Status: READY"
            ]
        ));
        assert_eq!(w.events("worker_started").len(), 1);
        let results = w.events("result_recorded");
        assert!(results.is_empty(), "{results:?}");
        recorded(&w, "park", &os_args!["--card", "1", "--reason", "operator"]);
        assert_fails!(record(
            &w,
            "park",
            &os_args!["--card", "1", "--reason", "twice"]
        ));
        assert_eq!(w.events("parked").len(), 1);
    }

    #[test]
    fn test_timer_returns_at_deadline() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        w.launch(&a);
        w.advance(61.0 * 60.0);
        let r = w
            .cli(&os_args![
                "timer",
                run_dir(&w),
                "--dispatch",
                text(&a["dispatch_id"])
            ])
            .ok();
        assert_eq!(lines(&r.stdout_text()), ["deadline passed"]);
    }

    #[test]
    fn test_cli_records_reply_from_stdin() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let did = text(&a["dispatch_id"]);
        let event = recorded(
            &w,
            "started",
            &os_args!["--dispatch", did, "--worker", "agent-cli"],
        );
        assert_eq!(event["worker_id"], "agent-cli", "{event}");
        let reply = w.perform(&a, "agent-cli").expect("a reply");
        let event = recorded_with(
            &w,
            "result",
            &os_args!["--dispatch", did, "--worker", "agent-cli", "--reply", "-"],
            &reply,
        );
        assert_eq!(event["status"], "READY", "{event}");
        assert_eq!(read_text(&os_path(&event["files"]["reply"])), reply);
        assert_eq!(w.next()["dispatch_id"], "c1-a1-review-1");
        let r = w
            .cli(&os_args!["timer", run_dir(&w), "--dispatch", did])
            .ok();
        assert!(r.stdout_text().contains("result recorded"), "{r}");
    }
}

mod codex {
    use super::*;
    use crate::common::engine::{CARD1, codex_world};
    use crate::common::launch_world::now;
    use crate::common::rundir::epoch_seconds;
    use crate::common::supervisor::{Options, card};
    use crate::common::{index, names_bin, shlex_split};

    #[test]
    fn test_codex_route_dispatches_proxy_with_null_agent_model() {
        let mut w = codex_world(&[card(1)], Options::default());
        w.start();
        let before = now();
        let a = w.next();
        assert_eq!(a["agent"], "leadhand:codex", "{a}");
        assert!(a["agent_model"].is_null(), "{a}");
        assert!(a["agent_effort"].is_null(), "{a}");
        assert_json_eq!(
            a["route"],
            jv(r#"{"vendor": "codex", "model": "gpt-6.1-terra", "effort": "high"}"#)
        );
        let argv = shlex_split(text(&a["message"])).expect("shell words");
        assert!(names_bin(&argv[0]), "{argv:?}"); // S1-R11: by file
        assert_eq!(argv[1], "dispatch");
        assert_eq!(argv[index(&argv, "--dispatch-id") + 1], "c1-a1-implement-1");
        assert_eq!(argv[index(&argv, "--cwd") + 1], s(&w.workspace()));
        assert_eq!(argv[index(&argv, "--run") + 1], s(&run_dir(&w)));
        assert!(!argv.iter().any(|x| x == "--resume"), "{argv:?}");
        let deadline = epoch_seconds(text(&a["deadline"]));
        assert!(
            (deadline - before - 65.0 * 60.0).abs() <= 30.0,
            "{deadline} - {before}"
        );

        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        let event = result(&w, &a, &worker, &reply, &[]);
        assert_eq!(event["status"], "READY", "{event}");
        assert_eq!(event["tip"], w.branch_tip(CARD1).as_str(), "{event}");
        assert!(truthy(&event["session_id"]), "{event}");
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{CARD1}:src/card1/c1-a1-implement-1.txt")]
            ),
            "c1-a1-implement-1"
        );
        assert_eq!(codex_starts(&w), 1);
        let mut roles: Vec<String> = w
            .charges()
            .iter()
            .map(|c| text(&c["role"]).to_string())
            .collect();
        roles.sort();
        assert_eq!(roles, ["implementer", "proxy"]);
        let proxy = w
            .charges()
            .into_iter()
            .find(|c| c["role"] == "proxy")
            .expect("a proxy charge");
        assert_eq!(
            proxy["charge_id"],
            format!("{}:claude:{worker}", text(&a["dispatch_id"])).as_str()
        );
        let review = w.next();
        assert_json_eq!(
            Json::from(vec![
                review["kind"].clone(),
                review["agent"].clone(),
                review["route"]["vendor"].clone()
            ]),
            jv(r#"["review", "leadhand:reviewer", "claude"]"#)
        );
        w.act(&review);
        assert_eq!(w.run().last().unwrap()["action"], "done");
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
    }

    #[test]
    fn test_stop_worker_for_codex_cancels_runner_and_records_before_returning() {
        let mut w = codex_world(&[card(1)], Options::default());
        w.start();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        let worker = w.launch(&a);
        let proxy = w.start_proxy(&a, &sleeping(120.0));
        let wd = run_dir(&w).join("workers").join(&did);
        wait_runner_codex(&w, &did, &proxy);
        let codex = codex_start(&w);
        assert!(codex_alive(&codex), "{codex}");
        assert_eq!(w.next()["action"], "wait");
        w.advance(66.0 * 60.0);
        let stop = w.next();
        assert_json_eq!(
            stop,
            Json::object([
                ("action", "stop_worker".into()),
                ("dispatch_id", did.as_str().into()),
                ("worker_id", worker.as_str().into()),
                ("vendor", "codex".into()),
                ("record_result", false.into()),
            ])
        );
        let res = common::one(w.events("result_recorded"));
        assert_json_eq!(pick(&res, &["status", "stopped"]), jv(r#"["SPENT", true]"#));
        assert_eq!(
            read_json(&wd.join("result.json"))["exit_reason"],
            "cancelled"
        );
        assert!(!codex_alive(&codex), "{codex}"); // stopped before `next` returned
        finish_proxy(&proxy, Duration::from_secs(60));
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a2-implement-1"]"#)
        );
    }
}

mod crashes {
    use super::*;
    use crate::common::supervisor::{CliOpts, card};

    #[test]
    fn test_crash_after_fast_forward_before_accepted_reconciles() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let review = drive_to(&mut w, "review");
        w.act(&review);
        let r = w.cli_with(
            &os_args!["next", run_dir(&w)],
            CliOpts {
                crash: Some("after_fast_forward"),
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 97, "{r}");
        let intent = common::one(w.events("accept_started"));
        let accepted = w.events("accepted");
        assert!(accepted.is_empty(), "{accepted:?}");
        assert_eq!(w.branch_tip(RUN), text(&intent["target"]));
        let reflog_args = ["reflog", "show", "--format=%H", "refs/heads/leadhand/demo"];
        let reflog = run_git(&w.repo, &reflog_args);
        let done = w.next();
        assert_eq!(done["action"], "done", "{done}");
        let accepted = common::one(w.events("accepted"));
        assert_json_eq!(accepted["sha"], intent["target"]);
        assert_eq!(w.events("accept_started").len(), 1);
        assert_eq!(w.branch_tip(RUN), text(&intent["target"]));
        assert_eq!(run_git(&w.repo, &reflog_args), reflog);
    }

    /// The worker's result recorded with the crash point `point` armed, which must end the
    /// command with 97: the dispatch, worker and reply.
    #[track_caller]
    fn crash_record(w: &mut FakeSupervisor, point: &str) -> (Json, String, String) {
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        let r = w.cli_with(
            &os_args![
                "record",
                run_dir(w),
                "result",
                "--dispatch",
                text(&a["dispatch_id"]),
                "--worker",
                worker,
                "--reply",
                "-"
            ],
            CliOpts {
                crash: Some(point),
                stdin: Some(&reply),
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 97, "{r}");
        (a, worker, reply)
    }

    #[test]
    fn test_crash_after_result_files_before_event_reconciles() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let (a, worker, reply) = crash_record(&mut w, "after_result_files");
        let folder = run_dir(&w).join("cards/1").join(text(&a["dispatch_id"]));
        for name in ["brief.md", "report.md", "reply.md"] {
            assert!(folder.join(name).is_file(), "{name}");
        }
        let results = w.events("result_recorded");
        assert!(results.is_empty(), "{results:?}");
        let charges = w.charges();
        assert!(charges.is_empty(), "{charges:?}");
        assert_eq!(w.next()["action"], "wait");
        let event = result(&w, &a, &worker, &reply, &[]);
        assert_eq!(event["status"], "READY", "{event}");
        assert_eq!(read_text(&folder.join("reply.md")), reply);
        assert_eq!(w.events("result_recorded").len(), 1);
        assert_eq!(w.charges().len(), 1);
        assert_eq!(w.attempts().len(), 1);
        assert_eq!(w.next()["dispatch_id"], "c1-a1-review-1");
    }

    #[test]
    fn test_crash_after_charge_before_attempt_retry_keeps_one_charge() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let (a, worker, reply) = crash_record(&mut w, "after_charge_before_attempt");
        assert_eq!(w.charges().len(), 1);
        let attempts = w.attempts();
        assert!(attempts.is_empty(), "{attempts:?}");
        let results = w.events("result_recorded");
        assert!(results.is_empty(), "{results:?}");
        let event = result(&w, &a, &worker, &reply, &[]);
        let charge = common::one(w.charges());
        let attempt = common::one(w.attempts());
        let ids = Json::Arr(vec![charge["charge_id"].clone()]);
        assert_json_eq!(attempt["charge_ids"], ids);
        assert_json_eq!(event["charge_ids"], ids);
        assert_eq!(w.events("result_recorded").len(), 1);
    }
}

mod supervisor {
    use super::*;
    use crate::common::supervisor::card;

    #[test]
    fn test_supervisor_usage_charged_once_across_calls() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.session_with("sess-1", 2);
        w.next();
        w.next();
        let sup: Vec<Json> = w
            .charges()
            .into_iter()
            .filter(|c| c["role"] == "supervisor")
            .collect();
        assert_json_eq!(field(&sup, "charge_id"), jv(r#"["supervisor:sess-1:1"]"#));
        assert_json_eq!(sup[0]["usage"]["output"], Json::from(40));
        assert_json_eq!(w.meta()["supervisor_sessions"], jv(r#"["sess-1"]"#));
        w.session_with("sess-1", 1);
        w.next();
        w.next();
        let sup: Vec<Json> = w
            .charges()
            .into_iter()
            .filter(|c| c["role"] == "supervisor")
            .collect();
        assert_json_eq!(
            field(&sup, "charge_id"),
            jv(r#"["supervisor:sess-1:1", "supervisor:sess-1:2"]"#)
        );
        assert_json_eq!(sup[1]["usage"]["output"], Json::from(20));
        assert_json_eq!(w.meta()["supervisor_sessions"], jv(r#"["sess-1"]"#));
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_by_risk`: `card_review = "by-risk"` never accepts a READY that changed
// nothing without a review (final review I8).

mod by_risk {
    use super::*;
    use crate::common::engine::BY_RISK;
    use crate::common::supervisor::{Options, card};

    fn by_risk_world() -> FakeSupervisor {
        FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: BY_RISK,
                ..Options::default()
            },
        )
    }

    #[test]
    fn test_ready_at_the_attempt_start_tip_goes_to_review() {
        let mut w = by_risk_world();
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        // The implementer writes a file but never commits it, and says READY.
        git::write(&w.workspace(), &[("src/uncommitted.txt", "x\n")]);
        result(&w, &a, &worker, "Status: READY\nReport: none", &[]);
        let nxt = w.next();
        let accepted = w.events("accepted");
        assert!(accepted.is_empty(), "{accepted:?}");
        assert_eq!(w.branch_tip(RUN), w.base);
        assert_json_eq!(
            pick(&nxt, &["action", "kind"]),
            jv(r#"["dispatch", "review"]"#)
        );
    }

    #[test]
    fn test_low_risk_ready_with_commits_is_still_accepted_without_review() {
        let mut w = by_risk_world();
        w.start();
        w.run();
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1"]);
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{RUN}:src/card1/c1-a1-implement-1.txt")]
            ),
            "c1-a1-implement-1"
        );
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_decisions`: a run whose every remaining card waits on a decision ends
// blocked (decision needed), and `unpark` continues it (final review I2, ruling R44).

mod decision_needed {
    use super::*;
    use crate::common::engine::{is_unpark_line, relaunch_env, status_lines};
    use crate::common::supervisor::{CardOpts, CliOpts, Options, card, card_with};

    fn gated(cards: &[String]) -> FakeSupervisor {
        FakeSupervisor::with_options(
            cards,
            Options {
                gates: true,
                ..Options::default()
            },
        )
    }

    #[test]
    fn test_question_parked_on_a_decision_ends_blocked_and_unpark_continues() {
        let mut w = gated(&[card(1), card(2)]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        let reason = "decision needed: which storage format";
        w.parks.insert(1, reason.to_string());
        let done = w.run().pop().unwrap();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "decision needed"]"#),
            "{}",
            done["reasons"]
        );
        assert!(
            json_in(&format!("card 1 parked: {reason}"), &done["reasons"]),
            "{}",
            done["reasons"]
        );
        assert!(w.paths().handoff.is_file());
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[2]"));
        let lines = status_lines(&w);
        let rd = run_dir(&w);
        assert!(
            lines
                .iter()
                .any(|line| is_unpark_line(line, 1, &rd, reason)),
            "{lines:?}"
        );
        // The operator answers and relaunches: the run continues and completes.
        w.cli(&os_args![
            "unpark",
            rd,
            "--card",
            "1",
            "--note",
            "Use JSON."
        ])
        .ok();
        w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
        w.cli_with(
            &os_args!["run", rd],
            CliOpts {
                env: relaunch_env(&w),
                ..CliOpts::default()
            },
        )
        .ok();
        let done = w.run().pop().unwrap();
        assert_eq!(done["outcome"], "complete", "{}", done["reasons"]);
        let mut accepted: Vec<i64> = w
            .events("accepted")
            .iter()
            .map(|e| e["card"].as_i64().unwrap())
            .collect();
        accepted.sort();
        assert_eq!(accepted, [1, 2]);
        for n in [1, 2] {
            run_git(
                &w.repo,
                &[
                    "cat-file",
                    "-e",
                    &format!("{RUN}:src/card{n}/c{n}-a1-implement-1.txt"),
                ],
            );
        }
    }

    #[test]
    fn test_card_waiting_on_a_decision_card_still_ends_blocked() {
        let mut w = gated(&[
            card(1),
            card_with(
                2,
                CardOpts {
                    depends: &[1],
                    ..CardOpts::default()
                },
            ),
            card(3),
        ]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks
            .insert(1, "decision needed: which storage format".to_string());
        let done = w.run().pop().unwrap();
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "decision needed"]"#),
            "{}",
            done["reasons"]
        );
    }

    #[test]
    fn test_violation_cap_park_ends_blocked_for_a_decision() {
        let tamper = || Step {
            tamper: Some(Box::new(|_w: &FakeSupervisor, ws: &Path, a: &Json| {
                let line = format!("rewritten by {}\n", text(&a["dispatch_id"]));
                git::write(ws, &[("README.md", &line)]);
            })),
            ..Step::default()
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(1, "review", (0..5).map(|_| tamper()));
        let done = w.run().pop().unwrap();
        let parked = common::one(w.events("parked"));
        assert!(parked.get("cap_dispatch").is_some_and(truthy), "{parked}");
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "decision needed"]"#),
            "{}",
            done["reasons"]
        );
        assert!(w.paths().handoff.is_file());
    }

    #[test]
    fn test_park_that_is_not_a_decision_still_ends_incomplete() {
        let mut w = gated(&[card(1), card(2)]);
        w.start();
        let a = w.next();
        w.act(&a);
        recorded(
            &w,
            "park",
            &os_args!["--card", "1", "--reason", "operator: skip"],
        );
        let done = w.run().pop().unwrap();
        assert_eq!(done["outcome"], "incomplete", "{}", done["reasons"]);
    }

    /// Each parked card is judged by its own latest park: another card's later decision park
    /// does not make the operator's plain park wait on a decision.
    #[test]
    fn test_a_decision_park_after_another_cards_plain_park_still_ends_incomplete() {
        let mut w = gated(&[card(1), card(2)]);
        w.start();
        let a = w.next();
        w.act(&a);
        recorded(
            &w,
            "park",
            &os_args!["--card", "1", "--reason", "operator: skip"],
        );
        w.script(2, "implement", [with_status("QUESTION")]);
        w.parks
            .insert(2, "decision needed: which storage format".to_string());
        let done = w.run().pop().unwrap();
        assert_json_eq!(field(&w.events("parked"), "card"), jv("[1, 2]"));
        assert_eq!(done["outcome"], "incomplete", "{}", done["reasons"]);
        assert_ne!(done["qualifier"], "decision needed", "{}", done["reasons"]);
    }

    #[test]
    fn test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint() {
        let mut w = gated(&[card(1), card(2)]);
        w.start();
        let a = w.next();
        w.act(&a);
        recorded(
            &w,
            "park",
            &os_args!["--card", "1", "--reason", "operator: skip"],
        );
        let done = w.run().pop().unwrap();
        assert_eq!(done["outcome"], "incomplete", "{done}");
        let r = assert_fails!(w.cli(&os_args![
            "unpark",
            run_dir(&w),
            "--card",
            "1",
            "--note",
            "go on"
        ]));
        assert!(r.stderr_text().contains("incomplete"), "{r}");
        let unparked = w.events("unparked");
        assert!(unparked.is_empty(), "{unparked:?}");
        let lines = status_lines(&w);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("Card 1 parked: operator: skip")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("unpark")),
            "{lines:?}"
        );
        w.cli(&os_args!["report", run_dir(&w)]).ok();
        let report = read_text(&w.paths().report);
        assert!(report.contains("Card 1 parked: operator: skip"), "{report}");
        let attention = report
            .split("## Needs your attention")
            .nth(1)
            .expect("a Needs your attention section");
        let attention = attention.split("## Result").next().unwrap();
        assert!(!attention.contains(" unpark "), "{attention}");
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_reply_file`: R38, the supervisor records a worker's reply through a file,
// never a heredoc.

mod reply_file {
    use super::*;
    use crate::common::resolve;
    use crate::common::supervisor::card;

    #[test]
    fn test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let path = os_path(&a["reply_file"]);
        assert!(path.is_absolute(), "{}", path.display());
        let ws = w.workspace();
        assert_eq!(
            path,
            ws.join(".leadhand/1")
                .join(text(&a["dispatch_id"]))
                .join("reply.txt")
        );
        let ws_real = resolve(&ws);
        assert!(
            resolve(&path).ancestors().skip(1).any(|p| p == ws_real),
            "{}",
            path.display()
        );
        fs::write(&path, "Status: READY\n").unwrap();
        assert_eq!(run_git(&ws, &["check-ignore", s(&path)]), s(&path)); // raises unless ignored
        assert!(
            !run_git(&ws, &["status", "--porcelain", "--untracked-files=all"])
                .contains("reply.txt")
        );
    }

    #[test]
    fn test_wait_names_the_same_reply_file() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        w.launch(&a);
        let wait = w.next();
        assert_eq!(wait["action"], "wait", "{wait}");
        assert_json_eq!(wait["reply_file"], a["reply_file"]);
    }

    #[test]
    fn test_cli_records_reply_from_a_file_with_spaces_in_its_path() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        assert!(text(&a["reply_file"]).contains(' '), "{a}");
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        fs::write(os_path(&a["reply_file"]), &reply).unwrap();
        let event = recorded(
            &w,
            "result",
            &os_args![
                "--dispatch",
                text(&a["dispatch_id"]),
                "--worker",
                worker,
                "--reply-file",
                os_path(&a["reply_file"])
            ],
        );
        assert_eq!(event["status"], "READY", "{event}");
        assert_eq!(read_text(&os_path(&event["files"]["reply"])), reply);
    }

    #[test]
    fn test_reply_and_reply_file_are_exclusive() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker).expect("a reply");
        fs::write(os_path(&a["reply_file"]), reply).unwrap();
        let r = record(
            &w,
            "result",
            &os_args![
                "--dispatch",
                text(&a["dispatch_id"]),
                "--worker",
                worker,
                "--reply",
                "Status: READY",
                "--reply-file",
                os_path(&a["reply_file"])
            ],
        );
        assert_eq!(r.code, 2, "{r}"); // a usage error
        let results = w.events("result_recorded");
        assert!(results.is_empty(), "{results:?}");
        assert_eq!(w.next()["action"], "wait");
    }

    #[test]
    fn test_missing_reply_file_falls_back_to_the_report_head_with_a_note() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        w.perform(&a, &worker); // writes the report; the reply file is never written
        let event = recorded(
            &w,
            "result",
            &os_args![
                "--dispatch",
                text(&a["dispatch_id"]),
                "--worker",
                worker,
                "--reply-file",
                os_path(&a["reply_file"])
            ],
        );
        assert_eq!(event["status"], "READY", "{event}");
        let note = read_text(&os_path(&event["files"]["reply"]));
        assert!(note.contains("reply file"), "{note}");
        assert!(note.contains(text(&a["reply_file"])), "{note}");
    }

    /// A stale reply file and report at the dispatch's scratch path.
    fn plant(ws: &Path, did: &str) -> PathBuf {
        let folder = ws.join(".leadhand/1").join(did);
        fs::create_dir_all(&folder).unwrap();
        for name in ["reply.txt", "report.md"] {
            fs::write(folder.join(name), "Status: READY\nstale\n").unwrap();
        }
        folder
    }

    #[test]
    fn test_new_request_removes_a_stale_reply_file() {
        // Dispatch IDs are deterministic, so a reply file or report already at a new
        // request's scratch path is stale: before the first request, and planted by an
        // earlier worker.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let ws = w.workspace();

        let implement = plant(&ws, "c1-a1-implement-1");
        let planted = ws.clone();
        w.script(
            1,
            "implement",
            [Step {
                tamper: Some(Box::new(
                    move |_w: &FakeSupervisor, _ws: &Path, _a: &Json| {
                        plant(&planted, "c1-a1-review-1");
                    },
                )),
                ..Step::default()
            }],
        );
        let a = w.next();
        assert_eq!(os_path(&a["reply_file"]), implement.join("reply.txt"));
        assert_eq!(os_path(&a["report"]), implement.join("report.md"));
        assert!(!implement.join("reply.txt").exists());
        assert!(!implement.join("report.md").exists());
        w.act(&a);
        assert!(ws.join(".leadhand/1/c1-a1-review-1/reply.txt").exists());
        let review = w.next();
        assert_eq!(review["dispatch_id"], "c1-a1-review-1", "{review}");
        let folder = ws.join(".leadhand/1/c1-a1-review-1");
        assert_eq!(os_path(&review["reply_file"]), folder.join("reply.txt"));
        assert!(!folder.join("reply.txt").exists());
        assert!(!folder.join("report.md").exists());
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_codex_result`: a Codex-route dispatch's result comes only from the runner's
// result.json (final review I1). The proxy only runs `dispatch`; whatever text it returns is
// never taken as the worker's result.

mod codex_result_from_runner {
    use super::*;
    use crate::common::engine::codex_world;
    use crate::common::supervisor::{CardOpts, Options, card, card_with};

    #[test]
    fn test_early_still_running_reply_waits_and_starts_no_second_implementer() {
        let mut w = codex_world(&[card(1)], Options::default());
        w.start();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        assert_eq!(a["agent"], "leadhand:codex", "{a}");
        let worker = w.launch(&a);
        let proxy = w.start_proxy(&a, &sleeping(4.0));
        wait_runner_codex(&w, &did, &proxy);
        let codex = codex_start(&w);
        // The proxy gives up early and replies with the dispatcher's "still running" line.
        let out = result(
            &w,
            &a,
            &worker,
            &format!("still running: run the same command again (dispatch {did})"),
            &[],
        );
        assert_eq!(out["action"], "wait", "{out}");
        assert_eq!(out["dispatch_id"], did.as_str(), "{out}");
        let results = w.events("result_recorded");
        assert!(results.is_empty(), "{results:?}");
        assert!(codex_alive(&codex), "{codex}");
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "dispatch_id"]),
            Json::from(vec!["wait".into(), did.as_str().into()])
        );
        // The timer reports the runner's result once result.json is there; `next` records it.
        let r = w
            .cli(&os_args!["timer", run_dir(&w), "--dispatch", did])
            .ok();
        assert_eq!(lines(&r.stdout_text()), ["result recorded"]);
        finish_proxy(&proxy, Duration::from_secs(60));
        let review = w.next();
        let res = common::one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&res, &["dispatch_id", "status"]),
            Json::from(vec![did.as_str().into(), "READY".into()])
        );
        assert_json_eq!(
            pick(&review, &["action", "kind"]),
            jv(r#"["dispatch", "review"]"#)
        );
        let implements: Vec<Json> = w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["dispatch_kind"] == "implement")
            .map(|e| e["dispatch_id"].clone())
            .collect();
        assert_json_eq!(Json::Arr(implements), Json::from(vec![did.as_str().into()]));
        assert_eq!(codex_starts(&w), 1);
    }

    #[test]
    fn test_invented_verdict_without_a_worker_is_not_accepted() {
        let cfg = "[roles.reviewer]\ncodex = \"gpt-6.1-terra medium\"\nprefer = \"codex\"\n";
        let mut w = codex_world(
            &[card_with(
                1,
                CardOpts {
                    route: Some("claude sonnet low"),
                    risk: "medium",
                    ..CardOpts::default()
                },
            )],
            Options {
                config: cfg,
                ..Options::default()
            },
        );
        w.start();
        let a = w.next();
        w.act(&a); // the Claude implementer
        let r = w.next();
        assert_json_eq!(
            pick(&r, &["action", "kind", "agent"]),
            jv(r#"["dispatch", "review", "leadhand:codex"]"#)
        );
        let worker = w.launch(&r);
        // The proxy never ran the command and invents a clean verdict.
        let ev = result(&w, &r, &worker, "Verdict: clean\nReport: none", &[]);
        assert!(ev.get("verdict").is_none_or(|v| v != "clean"), "{ev}");
        let wd = run_dir(&w).join("workers").join(text(&r["dispatch_id"]));
        assert_eq!(
            read_json(&wd.join("result.json"))["exit_reason"],
            "cancelled"
        );
        let nxt = w.next();
        let accepted = w.events("accepted");
        assert!(accepted.is_empty(), "{accepted:?}");
        assert_eq!(w.branch_tip(RUN), w.base);
        let verdicts = w.events("verdict_recorded");
        assert!(verdicts.is_empty(), "{verdicts:?}");
        assert_json_eq!(
            pick(&nxt, &["action", "kind"]),
            jv(r#"["dispatch", "review"]"#)
        );
        assert_ne!(nxt["dispatch_id"], r["dispatch_id"]);
    }

    #[test]
    fn test_skipping_a_codex_card_stops_its_runner() {
        let mut w = codex_world(&[card(1)], Options::default());
        w.start();
        let a = w.next();
        let did = text(&a["dispatch_id"]).to_string();
        let worker = w.launch(&a);
        let proxy = w.start_proxy(&a, &sleeping(120.0));
        let wd = run_dir(&w).join("workers").join(&did);
        wait_runner_codex(&w, &did, &proxy);
        let codex = codex_start(&w);
        assert!(codex_alive(&codex), "{codex}");
        // The run skill's skip path: stop the proxy, record it stopped, park the card.
        let ev = result(
            &w,
            &a,
            &worker,
            "Status: SPENT (time limit)",
            &os_args!["--stopped"],
        );
        assert_eq!(ev["kind"], "result_recorded", "{ev}");
        assert!(truthy(&ev["stopped"]), "{ev}");
        assert!(!codex_alive(&codex), "{codex}");
        assert_eq!(
            read_json(&wd.join("result.json"))["exit_reason"],
            "cancelled"
        );
        recorded(
            &w,
            "park",
            &os_args!["--card", "1", "--reason", "operator: skip"],
        );
        finish_proxy(&proxy, Duration::from_secs(60));
        assert_eq!(w.next()["action"], "done");
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_codex_e2e`: end to end with a Codex worker that never commits, as the real
// one (ruling R39, final review I11, ruling R47): the runner commits its work and the run
// completes.

mod non_committing_codex {
    use std::collections::BTreeMap;

    use super::*;
    use crate::common::engine::{CODEX_CARD, codex_world};
    use crate::common::supervisor::{CardOpts, Options, card_with};

    #[test]
    fn test_two_card_run_with_gates_completes_with_the_runners_commits() {
        let mut w = codex_world(
            &[
                card_with(
                    1,
                    CardOpts {
                        route: Some("claude sonnet low"),
                        ..CardOpts::default()
                    },
                ),
                card_with(
                    2,
                    CardOpts {
                        depends: &[1],
                        route: Some(CODEX_CARD),
                        ..CardOpts::default()
                    },
                ),
            ],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        w.script(
            2,
            "implement",
            [Step {
                commit: Some(false),
                ..Step::default()
            }],
        );
        let done = w.run().pop().unwrap();
        assert_eq!(done["outcome"], "complete", "{}", done["reasons"]);
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1, 2]"));
        let codex = common::one(
            w.events("result_recorded")
                .into_iter()
                .filter(|e| e["dispatch_id"] == "c2-a1-implement-1")
                .collect(),
        );
        assert!(codex.get("committed").is_some_and(truthy), "{codex}");
        let committed = text(&codex["committed"]);
        let head = w.branch_tip(RUN);
        run_git(&w.repo, &["merge-base", "--is-ancestor", committed, &head]);
        let body = run_git(&w.repo, &["log", "-1", "--format=%B", committed]);
        assert!(
            body.lines()
                .any(|l| l == "Leadhand-Dispatch: c2-a1-implement-1"),
            "{body}"
        );
        let mut files: Vec<String> =
            lines(&run_git(&w.repo, &["diff", "--name-only", &w.base, &head]));
        files.sort();
        assert_eq!(
            files,
            [
                "src/card1/c1-a1-implement-1.txt",
                "src/card2/c2-a1-implement-1.txt"
            ]
        );
        let gates: BTreeMap<String, Json> = w
            .events("gate_result")
            .into_iter()
            .map(|e| (text(&e["gate"]).to_string(), e))
            .collect();
        assert!(!gates.is_empty());
        assert!(
            gates
                .values()
                .all(|g| truthy(&g["passed"]) && g["examined_sha"] == head.as_str()),
            "{gates:?}"
        );
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_run_branch`: the run branch moves only by acceptance (final review I4).

mod implementer_guard {
    use super::*;
    use crate::common::supervisor::{Options, card};

    #[test]
    fn test_implementer_commit_on_the_run_branch_is_a_violation() {
        let mut w = FakeSupervisor::with_options(
            &[card(1), card(2)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                tamper: Some(Box::new(|_w: &FakeSupervisor, ws: &Path, _a: &Json| {
                    stray(ws)
                })),
                ..with_status("QUESTION")
            }],
        );
        w.parks
            .insert(1, "decision needed: storage format".to_string());
        w.run();
        let found: Vec<Json> = w
            .events("violation")
            .into_iter()
            .filter(|e| {
                e.get("stage").is_some_and(|s| s == "found")
                    && e.get("by").is_some_and(|b| b == "check")
            })
            .collect();
        assert_json_eq!(field(&found, "dispatch_id"), jv(r#"["c1-a1-implement-1"]"#));
        let run_ref = format!("refs/heads/{RUN}");
        assert!(json_in(&run_ref, &found[0]["refs"]), "{}", found[0]);
        let kept = text(&found[0]["preserved"][run_ref.as_str()]);
        assert_eq!(
            run_git(&w.repo, &["log", "-1", "--format=%s", kept]),
            "stray commit on the run branch"
        );
        assert!(
            !run_log(&w)
                .iter()
                .any(|s| s == "stray commit on the run branch")
        );
        assert!(w.dispatch_ids().iter().any(|d| d == "c1-a1-implement-2"));
        let mut accepted: Vec<i64> = w
            .events("accepted")
            .iter()
            .map(|e| e["card"].as_i64().unwrap())
            .collect();
        accepted.sort();
        assert_eq!(accepted, [1, 2]);
        let done = common::one(w.events("outcome"));
        assert_eq!(done["outcome"], "complete", "{}", done["reasons"]);
    }
}

mod run_head_check {
    use super::*;
    use crate::common::engine::commit_on;
    use crate::common::supervisor::{Options, card};

    #[test]
    fn test_run_branch_moved_between_dispatches_fails_the_run() {
        let mut w = FakeSupervisor::with_options(
            &[card(1), card(2)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        let a = w.next();
        w.act(&a);
        assert_eq!(w.next()["action"], "answer");
        recorded(
            &w,
            "park",
            &os_args!["--card", "1", "--reason", "operator: skip"],
        );
        // Someone moves the run branch while no worker is out.
        let ws = w.workspace();
        let outside = commit_on(&w.repo, &w.base);
        run_git(&ws, &["update-ref", &format!("refs/heads/{RUN}"), &outside]);
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome"]),
            jv(r#"["done", "failed"]"#),
            "{done}"
        );
        let reasons = common::strs(&done["reasons"]);
        assert!(
            reasons.iter().any(|r| r.contains("run branch")),
            "{reasons:?}"
        );
        assert_eq!(w.branch_tip(RUN), outside);
    }

    #[test]
    fn test_outcome_checks_the_run_head_after_a_cancel() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.session("supervisor-1");
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        stray(&w.workspace()); // the worker commits on the run branch, then is cancelled
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert_eq!(w.next()["action"], "stop_worker");
        result(
            &w,
            &a,
            &worker,
            "Status: SPENT (time limit)",
            &os_args!["--stopped"],
        );
        let done = w.next();
        assert_json_eq!(
            pick(&done, &["action", "outcome"]),
            jv(r#"["done", "failed"]"#),
            "{done}"
        );
    }
}

// --------------------------------------------------------------------------
// Python's `test_engine_untracked`: untracked files a worker did not create never land in its
// card's commits (final review I3).

mod engine_side {
    use super::*;
    use crate::common::engine::{CODEX_CARD, codex_world};
    use crate::common::supervisor::{CardOpts, Options, card, card_with};

    #[test]
    fn test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card() {
        let mut w = codex_world(
            &[
                card_with(
                    1,
                    CardOpts {
                        route: Some("claude sonnet low"),
                        ..CardOpts::default()
                    },
                ),
                card_with(
                    2,
                    CardOpts {
                        route: Some(CODEX_CARD),
                        ..CardOpts::default()
                    },
                ),
            ],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                tamper: Some(Box::new(|_w: &FakeSupervisor, ws: &Path, _a: &Json| {
                    git::write(
                        ws,
                        &[(
                            "src/card1/half-done.py",
                            "# card 1's unfinished, uncommitted new file\n",
                        )],
                    );
                })),
                ..with_status("QUESTION")
            }],
        );
        w.parks
            .insert(1, "decision needed: storage format".to_string());
        w.script(
            2,
            "implement",
            [Step {
                commit: Some(false), // like real Codex: the runner commits
                ..Step::default()
            }],
        );
        w.run();
        let run = text(&w.meta()["branch"]).to_string();
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[2]"));
        let landed = lines(&run_git(
            &w.repo,
            &["diff", "--name-only", &w.base, &format!("refs/heads/{run}")],
        ));
        assert_eq!(landed, ["src/card2/c2-a1-implement-1.txt"]);
        // The file is kept on a leftovers ref, and gone from the workspace.
        let by_implementer = |stage: &str| -> Vec<Json> {
            w.events("violation")
                .into_iter()
                .filter(|e| {
                    e.get("by").is_some_and(|b| b == "implementer")
                        && e.get("stage").is_some_and(|s| s == stage)
                })
                .collect()
        };
        let found = by_implementer("found");
        assert!(
            found.iter().any(|e| e
                .get("untracked")
                .filter(|u| truthy(u))
                .is_some_and(|u| json_in("src/card1/half-done.py", u))),
            "{found:?}"
        );
        let restored = by_implementer("restored");
        let kept: Vec<String> = restored
            .iter()
            .filter_map(|e| e.get("saved").filter(|s| truthy(s)))
            .map(|s| text(s).to_string())
            .collect();
        let has_blob = |sha: &str| {
            run_git_unchecked(
                &w.repo,
                &["cat-file", "-t", &format!("{sha}:src/card1/half-done.py")],
            ) == "blob"
        };
        assert!(kept.iter().any(|sha| has_blob(sha)), "{kept:?}");
        let run_id = text(&w.meta()["run_id"]).to_string();
        let leftovers = lines(&run_git(
            &w.repo,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/leadhand/{run_id}/leftovers/"),
            ],
        ));
        assert!(leftovers.iter().any(|sha| has_blob(sha)), "{leftovers:?}");
        assert!(!w.workspace().join("src/card1/half-done.py").exists());
    }

    #[test]
    fn test_cancel_partial_commit_leaves_untracked_files_that_were_there_before() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.session("supervisor-1");
        w.start();
        w.script(
            1,
            "review",
            [Step {
                tamper: Some(Box::new(|_w: &FakeSupervisor, ws: &Path, _a: &Json| {
                    git::write(ws, &[("build/out.txt", "reviewer's build output\n")]);
                })),
                ..with_verdict("defects")
            }],
        );
        let a = w.next();
        w.act(&a); // implement
        let a = w.next();
        w.act(&a); // review: defects, leaves untracked build output
        let fix = w.next();
        assert_eq!(fix["kind"], "fix", "{fix}");
        let worker = w.launch(&fix);
        let ws = w.workspace();
        git::write(&ws, &[("src/card1/fix-partial.txt", "half a fix\n")]);
        w.cli(&os_args!["cancel", run_dir(&w)]).ok();
        assert_eq!(w.next()["action"], "stop_worker");
        let event = result(
            &w,
            &fix,
            &worker,
            "Status: SPENT (time limit)",
            &os_args!["--stopped"],
        );
        assert!(event.get("partial_commit").is_some_and(truthy), "{event}");
        let files = lines(&run_git(
            &w.repo,
            &[
                "show",
                "--name-only",
                "--format=",
                text(&event["partial_commit"]),
            ],
        ));
        assert_eq!(files, ["src/card1/fix-partial.txt"]);
        assert!(ws.join("build/out.txt").exists());
    }
}

mod runner_side {
    use serde_json::json;

    use super::*;
    use crate::common::dispatch_world::{CARD_BRANCH, CARD_BRIEF, Req, World};

    #[test]
    fn test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch() {
        let mut w = World::new();
        w.git(&["checkout", "--quiet", "-b", CARD_BRANCH]);
        git::write(
            &w.ws,
            &[("stray/left by another card.txt", "not this dispatch's\n")],
        );
        w.scratch_with("d1", Some(1), Some(CARD_BRIEF));
        w.scenario_steps(
            &[
                json!({"type": "thread.started", "thread_id": "{thread}"}),
                json!({"type": "turn.started"}),
                json!({"action": "write_file", "path": "README.md", "text": "changed\n"}),
                json!({"action": "write_file", "path": "src/new.py", "text": "NEW = 1\n"}),
                json!({"action": "last_message", "text": "Status: READY\n"}),
                json!({"type": "turn.completed", "usage": {"input_tokens": 1, "cached_input_tokens": 0,
                                                         "output_tokens": 1}}),
            ],
            &[],
        );
        w.dispatch_with(
            "d1",
            &Req {
                kind: Some("implement"),
                branch: Some(CARD_BRANCH),
                ..Req::default()
            },
        )
        .ok();
        let saved = w.result("d1");
        assert_eq!(saved["status"], "READY", "{saved}");
        assert_eq!(
            saved["committed"],
            w.git(&["rev-parse", "HEAD"]).as_str(),
            "{saved}"
        );
        let mut files = lines(&w.git(&[
            "show",
            "--name-only",
            "--format=",
            text(&saved["committed"]),
        ]));
        files.sort();
        assert_eq!(files, ["README.md", "src/new.py"]);
        assert!(w.ws.join("stray/left by another card.txt").exists());
        assert!(w.git(&["status", "--porcelain"]).contains("?? stray/"));
    }
}

// --------------------------------------------------------------------------
// Review focus 5: spaces in every path, every call from an unrelated directory.

mod paths {
    use super::*;
    use crate::common::engine::{codex_world, run_line};
    use crate::common::launch_world::path_without_self;
    use crate::common::supervisor::{Options, card};
    use crate::common::{
        TempRoot, bin, cli, index, is_bin_command, names_bin, realpath, shlex_join, shlex_split,
    };

    #[test]
    fn test_spaces_everywhere_from_unrelated_cwd() {
        let w = FakeSupervisor::new(&[card(1)]);
        let elsewhere = TempRoot::new("lh unrelated cwd ");
        let env = w
            .env
            .with("PATH", path_without_self(w.env.get("PATH").expect("PATH")));
        for path in [&w.repo, &w.docs, &w.deck_path] {
            assert!(s(path).contains(' '), "{}", path.display());
        }
        assert!(!realpath(&elsewhere).starts_with(realpath(&w.tmp)));

        let r = cli(
            &os_args!["start", w.deck_path, "--repo", w.repo],
            &env,
            &elsewhere,
        )
        .ok();
        let run_dir = run_line(&r);
        assert!(
            lines(&r.stdout_text()).contains(&format!("run: {}", s(&run_dir))),
            "{r}"
        );
        assert!(
            realpath(&run_dir).starts_with(realpath(&w.docs)),
            "{}",
            run_dir.display()
        );
        let meta = read_json(&run_dir.join("run.json"));
        assert_eq!(
            meta["repo"].as_os(),
            Some(realpath(&w.repo).into_os_string())
        );
        assert!(text(&meta["workspace"]).contains(' '), "{meta}");

        let a = cli(&os_args!["next", run_dir], &env, &elsewhere)
            .ok()
            .json();
        assert_json_eq!(
            pick(&a, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a1-implement-1"]"#)
        );
        assert!(os_path(&a["brief"]).is_file());
        let scratch = os_path(&meta["workspace"]).join(".leadhand/1/c1-a1-implement-1");
        assert_eq!(os_path(&a["brief"]), scratch.join("brief.md"));

        let deck = s(&run_dir.join("deck.md")).to_string();
        let plugin = elsewhere.join("plugin dir");
        let cases: [(Vec<&OsStr>, Vec<String>); 2] = [
            (
                vec![run_dir.as_os_str()],
                vec!["run".to_string(), deck.clone()],
            ),
            (
                vec![
                    run_dir.as_os_str(),
                    OsStr::new("--plugin-dir"),
                    plugin.as_os_str(),
                ],
                vec![
                    "run".to_string(),
                    deck.clone(),
                    "--plugin-dir".to_string(),
                    s(&plugin).to_string(),
                ],
            ),
        ];
        for (args, tail) in cases {
            let mut argv = vec![OsStr::new("launch-line")];
            argv.extend(args);
            let r = cli(&argv, &env, &elsewhere).ok();
            let stdout = r.stdout_text();
            assert_eq!(stdout.matches('\n').count(), 1, "{r}");
            let words = shlex_split(&stdout).expect("shell words");
            assert!(names_bin(&words[0]), "{words:?}"); // S1-R11: by file
            assert_eq!(words[1..], tail[..], "{r}");
            let tail_os: Vec<std::ffi::OsString> = tail.iter().map(Into::into).collect();
            assert!(
                is_bin_command(
                    &stdout[..stdout.len() - 1],
                    "",
                    &format!(" {}", shlex_join(&tail_os))
                ),
                "{r}"
            );
        }
    }

    #[test]
    fn test_spaced_cli_path_from_unrelated_cwd() {
        // Ruling S1-R14: the CLI's own path gets a space through LEADHAND_TEST_CLI, a symlink
        // to the binary in a directory whose name has a space. The dispatch line and the
        // launch line quote it, and the dispatch line runs to completion through it.
        let mut w = codex_world(&[card(1)], Options::default());
        let elsewhere = TempRoot::new("lh unrelated cwd ");
        let link_root = TempRoot::new("lh cli ");
        let link = link_root.join("lead hand bin/leadhand");
        fs::create_dir(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(bin(), &link).unwrap();
        assert!(names_bin(&link));
        let path = path_without_self(w.env.get("PATH").expect("PATH"));
        w.env.set("LEADHAND_TEST_CLI", &link).set("PATH", path);
        for path in [&w.repo, &w.docs, &w.deck_path, &link] {
            assert!(s(path).contains(' '), "{}", path.display());
        }

        let run_dir = run_line(
            &cli(
                &os_args!["start", w.deck_path, "--repo", w.repo],
                &w.env,
                &elsewhere,
            )
            .ok(),
        );
        w.run_dir = Some(run_dir.clone());
        let a = cli(&os_args!["next", run_dir], &w.env, &elsewhere)
            .ok()
            .json();
        assert_json_eq!(
            pick(&a, &["action", "agent"]),
            jv(r#"["dispatch", "leadhand:codex"]"#),
            "{a}"
        );
        assert_eq!(text(&a["message"]).lines().count(), 1, "{a}");
        let argv = shlex_split(text(&a["message"])).expect("shell words");
        assert_eq!(argv[0], s(&link)); // the spaced path, quoted as one word
        assert!(names_bin(&argv[0]), "{argv:?}"); // S1-R11: by file
        assert_eq!(argv[1], "dispatch");
        assert_eq!(argv[index(&argv, "--run") + 1], s(&run_dir));

        let r = cli(&os_args!["launch-line", run_dir], &w.env, &elsewhere).ok();
        let stdout = r.stdout_text();
        let words = shlex_split(&stdout).expect("shell words");
        assert_eq!(
            words,
            [
                s(&link).to_string(),
                "run".to_string(),
                s(&run_dir.join("deck.md")).to_string()
            ],
            "{r}"
        );
        let rest: Vec<std::ffi::OsString> = words[1..].iter().map(Into::into).collect();
        assert!(
            is_bin_command(
                &stdout[..stdout.len() - 1],
                "",
                &format!(" {}", shlex_join(&rest))
            ),
            "{r}"
        );

        // The printed dispatch line runs, from the unrelated directory, through the spaced
        // path.
        recorded(
            &w,
            "started",
            &os_args![
                "--dispatch",
                text(&a["dispatch_id"]),
                "--worker",
                "agent-proxy"
            ],
        );
        let (line, _) = w.proxy_argv_env(&a, &Step::default());
        assert_eq!(line, argv);
        let out = w.proxy_with(
            &a,
            &Step::default(),
            Some(&elsewhere),
            Duration::from_secs(180),
            20,
        );
        assert_eq!(out.code, 0, "{out}");
        assert_eq!(lines(&out.stdout_text())[0], "Status: READY", "{out}");
        let saved = read_json(
            &run_dir
                .join("workers")
                .join(text(&a["dispatch_id"]))
                .join("result.json"),
        );
        assert_eq!(saved["status"], "READY", "{saved}");
        assert_eq!(
            run_git(
                &w.repo,
                &[
                    "show",
                    "leadhand/demo--card-1:src/card1/c1-a1-implement-1.txt"
                ]
            ),
            "c1-a1-implement-1"
        );
    }
}

// --------------------------------------------------------------------------
// Python's `test_test_hooks`: the clock offset

mod clock_offset_hook {
    use super::*;
    use crate::common::cli;
    use crate::common::launch_world::now;
    use crate::common::rundir::epoch_seconds;
    use crate::common::supervisor::card;

    #[test]
    fn test_clock_offset_is_honoured_only_under_leadhand_testing() {
        // Covers limit L4 (test hooks need LEADHAND_TESTING=1). Observed through the deadline
        // `record started` gives a worker (the CLI's clock plus the dispatch's budget): one
        // fresh run per case, the same card, so the same budget.
        let deadline_lead = |vars: &[(&str, &str)]| -> f64 {
            let mut w = FakeSupervisor::new(&[card(1)]);
            w.start();
            let a = w.next();
            assert_eq!(a["action"], "dispatch", "{a}");
            let before = now();
            let mut env = w.env.clone();
            for (k, v) in vars {
                env.set(k, v);
            }
            let e = cli(
                &os_args![
                    "record",
                    run_dir(&w),
                    "started",
                    "--dispatch",
                    text(&a["dispatch_id"]),
                    "--worker",
                    "agent0001"
                ],
                &env,
                &w.tmp,
            )
            .ok()
            .json();
            assert_eq!(e["kind"], "worker_started", "{e}");
            epoch_seconds(text(&e["deadline"])) - before
        };

        let plain = deadline_lead(&[]);
        let shifted = deadline_lead(&[("LEADHAND_TEST_CLOCK_OFFSET", "3600")]);
        let ignored = deadline_lead(&[
            ("LEADHAND_TEST_CLOCK_OFFSET", "3600"),
            ("LEADHAND_TESTING", "0"),
        ]);
        assert!(
            (shifted - plain - 3600.0).abs() <= 60.0,
            "{shifted} - {plain}"
        );
        assert!((ignored - plain).abs() <= 60.0, "{ignored} vs {plain}");
    }
}

// --------------------------------------------------------------------------
// The card-loop-only test mode (LEADHAND_TEST_CARD_LOOP_ONLY)

mod card_loop_only_hook {
    use super::*;
    use crate::common::supervisor::{Options, card};

    #[test]
    fn test_card_loop_only_mode_ends_the_run_after_its_cards() {
        // Covers R34: with LEADHAND_TEST_CARD_LOOP_ONLY=1 (honoured under LEADHAND_TESTING)
        // the engine stops once the cards are done: `done` with no outcome and no report, and
        // no end-of-run gate. Without it the same deck goes on to the final review.
        let mut lp = FakeSupervisor::new(&[card(1)]);
        assert_eq!(
            lp.env.get("LEADHAND_TEST_CARD_LOOP_ONLY"),
            Some(OsStr::new("1"))
        );
        lp.start();
        let done = lp.run().pop().unwrap();
        assert_json_eq!(
            pick(&done, &["action", "outcome", "report"]),
            jv(r#"["done", null, null]"#),
            "{done}"
        );
        assert_json_eq!(
            done["cards"],
            jv(r#"[{"card": 1, "state": "accepted", "reason": ""}]"#),
            "{done}"
        );
        let outcomes = lp.events("outcome");
        assert!(outcomes.is_empty(), "{outcomes:?}");
        assert!(
            lp.run_dispatch_ids().is_empty(),
            "{:?}",
            lp.run_dispatch_ids()
        );
        let run_level: Vec<Json> = lp
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e.get("card").is_none_or(Json::is_null))
            .collect();
        assert!(run_level.is_empty(), "{run_level:?}");

        let mut gated = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        assert!(gated.env.get("LEADHAND_TEST_CARD_LOOP_ONLY").is_none());
        gated.start();
        let mut a = Json::Null;
        for _ in 0..20 {
            a = gated.next();
            if a["action"] == "dispatch" && a["card"].is_null() {
                break;
            }
            gated.act(&a);
        }
        assert_json_eq!(
            pick(&a, &["action", "kind"]),
            jv(r#"["dispatch", "final_review"]"#),
            "{a}"
        );
    }
}
