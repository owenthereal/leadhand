//! The end of a run: the commit-bound gates (final review, a fix wave's implementer and
//! review, evidence, security, verify), the outcome, the report and the `<docs_dir>/runs.md`
//! line, as `start`, `next`, `record`, `unpark`, `run`, `pause`, `cancel`, `timer` and `check`
//! show them.
//!
//! The Rust form of the Python contract suite's `contract_finish` (converted from
//! Python's `test_engine_finish` and `test_final_review_minors`). `FakeSupervisor` with
//! `gates: true` plays the run skill through the real end of run. A refusal is exit code 1
//! with `error: ...` on stderr. A run is relaunched with `run`, which records `resumed`. A
//! crash inside a Git call is injected by making that call fail (a held ref lock); a crash
//! right after `git worktree add` is set up as the state it leaves (the
//! `base_worktree_added` event and the worktree, without its marker).
//!
//! Gate outcomes are judged from Git and the filesystem: the run branch's tip and tree, the
//! refs under refs/leadhand/<run id>/, the evidence base worktree at
//! `<repo>/.worktrees/<workspace name>--base` with its `leadhand-run` marker (R60), and the
//! runs.md file. The binary's events are asserted beside that, never as the only evidence.
//!
//! The branch release (deliberate change 2, ruling S1-R15): a run that ends complete,
//! incomplete or failed may record more events after `outcome`, detach its worktree and add
//! a branch line to the report. So these tests check the event kinds and order they care
//! about, never a complete list, and the report's specific lines.
//!
//! The Python's `git` wrapper for the slow commit was a Python script; here it is a shell
//! script that keeps the times as file times (`SLOW_GIT`). `base_wt`, `marker`, `gate_names`,
//! `drive_to_run`, `drive_until_refused` and `to_last_step`, which later files import, are in
//! `common::engine`.

#[macro_use]
mod common;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use common::engine::{
    CODEX_CARD, RUN, base_wt, codex_world, drive_to_run, drive_until_refused, files, gate_names,
    gated, gated_with, last, marker, path_before, relaunch_env, run_id, seq, tampering,
    to_last_step, with_status, with_verdict, write_script,
};
use common::git::{self, commit, run_git, run_git_unchecked};
use common::json::Json;
use common::rundir::append_event;
use common::supervisor::{
    CardOpts, CliOpts, FakeSupervisor, Options, Proc, Step, card, card_with, os_path, run_dir,
    text, truthy, wait_timeout,
};
use common::{
    Out, TIMEOUT, bin, field, hold_lock, is_bin_command, json_in, jv, lines, one, path_str as s,
    pick, read_text, resolve, run_argv, shlex_quote, strs, walk, which,
};

const FINAL: &str = "leadhand/demo--card-final";

// --------------------------------------------------------------------------
// Helpers

/// `card(n, risk=risk)`.
fn risky(n: i64, risk: &str) -> String {
    card_with(
        n,
        CardOpts {
            risk,
            ..CardOpts::default()
        },
    )
}

/// A gate result: `(gate, examined_sha, passed)`.
fn gate(name: &str, sha: &str, passed: bool) -> Json {
    Json::Arr(vec![name.into(), sha.into(), passed.into()])
}

/// `(gate, examined_sha, passed)` for every gate result, in order.
#[track_caller]
fn gates(w: &FakeSupervisor) -> Vec<Json> {
    w.events("gate_result")
        .iter()
        .map(|e| pick(e, &["gate", "examined_sha", "passed"]))
        .collect()
}

/// The last outcome event (`w.events("outcome")[-1]`).
#[track_caller]
fn outcome(w: &FakeSupervisor) -> Json {
    w.events("outcome").pop().expect("an outcome event")
}

/// The last action of a run (`w.run()[-1]`).
#[track_caller]
fn finish_run(w: &mut FakeSupervisor) -> Json {
    w.run().pop().expect("an action")
}

/// The lines of `<docs>/runs.md` that name the world's run.
#[track_caller]
fn runs_lines(w: &FakeSupervisor) -> Vec<String> {
    let path = w.docs.join("runs.md");
    if !path.is_file() {
        return Vec::new();
    }
    let rd = run_dir(w);
    let rd = s(&rd);
    read_text(&path)
        .lines()
        .filter(|ln| ln.contains(rd))
        .map(str::to_string)
        .collect()
}

#[track_caller]
fn on_run_branch(w: &FakeSupervisor, path: &str) -> bool {
    run_git(
        &w.repo,
        &["ls-tree", "-r", "--name-only", &format!("refs/heads/{RUN}")],
    )
    .lines()
    .any(|p| p == path)
}

/// The `found` violations of checks (not an implementer's leftovers).
#[track_caller]
fn found_violations(w: &FakeSupervisor) -> Vec<Json> {
    w.events("violation")
        .into_iter()
        .filter(|e| {
            e.get("stage").is_some_and(|s| s == "found")
                && e.get("by").filter(|b| truthy(b)).map_or("check", text) != "implementer"
        })
        .collect()
}

/// The dispatch IDs of the run-level dispatches that contain `part`.
fn run_ids_with(w: &FakeSupervisor, part: &str) -> Vec<String> {
    w.run_dispatch_ids()
        .into_iter()
        .filter(|i| i.contains(part))
        .collect()
}

/// The accepted events of `card` (an integer, or "final").
#[track_caller]
fn accepted_of(w: &FakeSupervisor, card: &Json) -> Vec<Json> {
    w.events("accepted")
        .into_iter()
        .filter(|e| &e["card"] == card)
        .collect()
}

/// The integer cards of the accepted events, sorted.
#[track_caller]
fn sorted_cards(events: &[Json]) -> Vec<i64> {
    let mut cards: Vec<i64> = events
        .iter()
        .map(|e| {
            e["card"]
                .as_i64()
                .unwrap_or_else(|| panic!("not an integer card: {e}"))
        })
        .collect();
    cards.sort();
    cards
}

/// `run RUN_DIR` with the fake `claude` on PATH, which must succeed.
#[track_caller]
fn relaunch(w: &FakeSupervisor) {
    w.cli_with(
        &os_args!["run", run_dir(w)],
        CliOpts {
            env: relaunch_env(w),
            ..CliOpts::default()
        },
    )
    .ok();
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

/// The launched worker `worker` does dispatch `a`'s work; its result is recorded through
/// the reply file.
#[track_caller]
fn finish(w: &mut FakeSupervisor, a: &Json, worker: &str) {
    let reply = w.perform(a, worker).expect("the worker's reply");
    let reply_file = os_path(&a["reply_file"]);
    fs::create_dir_all(reply_file.parent().unwrap()).unwrap();
    fs::write(&reply_file, reply).unwrap();
    w.cli(&os_args![
        "record",
        run_dir(w),
        "result",
        "--dispatch",
        text(&a["dispatch_id"]),
        "--worker",
        worker,
        "--reply-file",
        reply_file
    ])
    .ok();
}

/// True when `rest` occurs in `text` right after one shell word naming the binary under test
/// (compared by file, S1-R11).
fn names_bin_before(text: &str, rest: &str) -> bool {
    let mut start = 0;
    while let Some(off) = text[start..].find(rest) {
        let j = start + off;
        // range(j - 1, max(-1, j - 4096), -1): up to 4095 code points back
        let found = text[..j]
            .char_indices()
            .rev()
            .take(4095)
            .any(|(i, _)| is_bin_command(&text[i..j], "", ""));
        if found {
            return true;
        }
        start = j + text[j..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// `^\d{4}-\d{2}-\d{2}$`.
fn is_date(cell: &str) -> bool {
    let b = cell.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// `^(\$\d+\.\d\d|unknown)$`.
fn is_cost(cell: &str) -> bool {
    if cell == "unknown" {
        return true;
    }
    let Some(amount) = cell.strip_prefix('$') else {
        return false;
    };
    let Some((dollars, cents)) = amount.split_once('.') else {
        return false;
    };
    !dollars.is_empty()
        && dollars.bytes().all(|c| c.is_ascii_digit())
        && cents.len() == 2
        && cents.bytes().all(|c| c.is_ascii_digit())
}

/// A runs.md line's cells: split at each `|` not after a backslash, less the empty first and
/// last (`re.split(r"(?<!\\)\|", line.strip())[1:-1]`, each stripped).
fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut prev = None;
    for c in line.chars() {
        if c == '|' && prev != Some('\\') {
            parts.push(std::mem::take(&mut part));
        } else {
            part.push(c);
        }
        prev = Some(c);
    }
    parts.push(part);
    let inner = if parts.len() >= 2 {
        &parts[1..parts.len() - 1]
    } else {
        &[][..]
    };
    inner.iter().map(|c| c.trim().to_string()).collect()
}

/// A file's modification time.
#[track_caller]
fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("reading the time of {}: {e}", path.display()))
}

// --------------------------------------------------------------------------

mod complete {
    use super::*;

    #[test]
    fn test_complete_run_writes_outcome_and_report() {
        let mut w = gated(&[
            card(1),
            card_with(
                2,
                CardOpts {
                    depends: &[1],
                    ..CardOpts::default()
                },
            ),
        ]);
        w.start();
        let done = finish_run(&mut w);

        let head = w.branch_tip(RUN);
        for n in [1, 2] {
            assert!(
                on_run_branch(&w, &format!("src/card{n}/c{n}-a1-implement-1.txt")),
                "{n}"
            );
        }
        assert_json_eq!(
            Json::Arr(gates(&w)),
            Json::Arr(vec![
                gate("final_review", &head, true),
                gate("evidence", &head, true),
                gate("verify", &head, true)
            ])
        );
        assert_eq!(
            w.run_dispatch_ids(),
            ["run-final_review-1", "run-evidence-1", "run-verify-1"]
        );
        assert_json_eq!(
            field(&w.run_started, "role"),
            jv(r#"["final_reviewer", "verifier", "verifier"]"#)
        );
        assert!(w.run_started.iter().all(|a| a["card"].is_null()));

        // the briefs name the run branch, the base and the accepted cards
        let review_brief = read_text(&os_path(&w.run_started[0]["brief"]));
        assert!(review_brief.contains(&format!("{}..{head}", w.base)));
        let evidence_brief = read_text(&os_path(&w.run_started[1]["brief"]));
        let bw = base_wt(&w);
        let rd = run_dir(&w);
        let mut name = rd.file_name().unwrap().to_os_string();
        name.push("--base");
        assert_eq!(resolve(&bw), resolve(&w.repo.join(".worktrees").join(name)));
        assert!(evidence_brief.contains(s(&bw)));
        // the evidence base worktree, read from Git: detached at the base, marked as this
        // run's (R60)
        assert_eq!(run_git(&bw, &["rev-parse", "HEAD"]), w.base);
        assert_eq!(
            run_git_unchecked(&bw, &["symbolic-ref", "--quiet", "HEAD"]),
            ""
        );
        assert_eq!(read_text(&marker(&bw)).trim(), run_id(&w));
        for a in &w.run_started {
            assert!(read_text(&os_path(&a["brief"])).contains(&format!("Branch: {RUN}")));
        }
        // every gate ran on the run branch at the run head, guarded like a check
        for e in w.events("dispatch_requested") {
            if e.get("gate").is_some_and(truthy) {
                assert_json_eq!(
                    pick(&e, &["expected_branch", "expected_head"]),
                    Json::Arr(vec![RUN.into(), head.as_str().into()])
                );
            }
        }
        assert_json_eq!(
            field(&w.events("gate_started"), "gate"),
            jv(r#"["final_review", "evidence", "verify"]"#)
        );

        let out = one(w.events("outcome"));
        assert_json_eq!(
            pick(&out, &["outcome", "qualifier", "next_step"]),
            jv(r#"["complete", "", "none"]"#)
        );
        let written = one(w.events("report_written"));
        assert!(seq(&written) > seq(&out));
        let report = w.paths().report;
        assert!(report.is_file());
        let report_lines = lines(&read_text(&report));
        assert!(report_lines.iter().any(|l| l == "Outcome: complete"));
        let passed = format!("- final_review: passed on commit {}", &head[..12]);
        assert!(report_lines.contains(&passed), "{report_lines:?}");
        assert!(!w.paths().handoff.exists());
        assert_json_eq!(
            pick(&done, &["action", "outcome", "report"]),
            Json::Arr(vec!["done".into(), "complete".into(), s(&report).into()])
        );
        assert_eq!(runs_lines(&w).len(), 1);

        // repeating next changes nothing
        assert_eq!(w.next()["outcome"], "complete");
        assert_eq!(w.events("outcome").len(), 1);
        assert_eq!(w.events("report_written").len(), 1);
        assert_eq!(w.branch_tip(RUN), head);
    }

    #[test]
    fn test_runs_md_line_appended_once() {
        let mut w = FakeSupervisor::with_options(
            &[card(1), card(2)],
            Options {
                title: "Widgets | phase 1",
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        w.run();
        w.next();
        w.next();
        let [line] = <[String; 1]>::try_from(runs_lines(&w))
            .unwrap_or_else(|l| panic!("expected one runs.md line: {l:?}"));
        let cells = cells(&line);
        assert_eq!(cells.len(), 6, "{line}"); // the title's pipe is escaped
        assert!(is_date(&cells[0]), "{:?}", cells[0]);
        assert_eq!(cells[1], "Widgets \\| phase 1");
        assert_eq!(cells[2], "complete");
        assert_eq!(cells[3], "2/2 cards");
        assert!(is_cost(&cells[4]), "{:?}", cells[4]);
        assert_eq!(cells[5], s(&run_dir(&w)));
    }
}

mod shared_run_index {
    use super::*;

    #[test]
    fn test_concurrent_runs_never_lose_a_runs_md_line() {
        // Two runs of two repositories share one docs_dir and end at once. runs.md is
        // read-modified-replaced under an exclusive flock on <docs_dir>/runs.md.lock; the test
        // holds that lock while both runs end, so both wait for it and then read the same
        // file.
        let mut w1 = gated(&[card(1)]);
        let mut w2 = gated(&[card(1)]);
        fs::write(&w2.config_file, format!("docs_dir = \"{}\"\n", s(&w1.docs))).unwrap();
        w2.docs = w1.docs.clone();
        for w in [&mut w1, &mut w2] {
            w.start();
            to_last_step(w);
        }
        assert_ne!(w1.run_dir, w2.run_dir);

        let lock = w1.docs.join("runs.md.lock");
        // Each run's `next` as its world's `cli` runs it: the world's environment with no
        // crash point, from its temp root.
        let calls: Vec<(Vec<OsString>, common::Env, PathBuf)> = [&w1, &w2]
            .iter()
            .map(|w| {
                let mut env = w.clock_env();
                env.remove("LEADHAND_CRASH_AT");
                let argv = vec![
                    bin().into_os_string(),
                    "next".into(),
                    run_dir(w).into_os_string(),
                ];
                (argv, env, w.tmp.clone())
            })
            .collect();
        // The scope joins both threads before it returns, and a failure inside it drops the
        // lock first (Python's `finally`).
        let results = thread::scope(|scope| {
            let held = hold_lock(&lock);
            let threads: Vec<_> = calls
                .iter()
                .map(|(argv, env, cwd)| {
                    scope.spawn(move || {
                        let argv: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
                        run_argv(&argv, env, cwd, None, TIMEOUT)
                    })
                })
                .collect();
            let deadline = Instant::now() + Duration::from_secs(60);
            while ![&w1, &w2].iter().all(|w| !w.events("outcome").is_empty()) {
                assert!(
                    Instant::now() < deadline,
                    "the runs did not reach their outcome in 60 s"
                );
                thread::sleep(Duration::from_millis(50));
            }
            assert!(
                threads.iter().all(|t| !t.is_finished()),
                "a run ended while the runs.md lock was held"
            );
            assert_empty!([runs_lines(&w1), runs_lines(&w2)].concat());
            drop(held);
            threads.into_iter().map(|t| t.join()).collect::<Vec<_>>()
        });
        for r in results {
            assert!(
                r.is_ok(),
                "a run's `next` failed: {:?}",
                r.as_ref().err().and_then(|e| e.downcast_ref::<String>())
            );
            let r = r.unwrap();
            assert_eq!(r.ok().json()["outcome"], "complete");
        }
        let text = read_text(&w1.docs.join("runs.md"));
        for w in [&w1, &w2] {
            let tail = format!("| {} |", s(&run_dir(w)));
            assert_eq!(
                text.lines().filter(|ln| ln.ends_with(&tail)).count(),
                1,
                "{text}"
            );
        }
    }
}

mod relative_docs_dir {
    use super::*;

    #[test]
    fn test_relative_docs_dir_resolves_against_the_repository_from_any_cwd() {
        // `start --repo` from another directory and the wrap-up run from the workspace must
        // use one docs dir: the repository-relative one.
        let mut w = gated(&[card(1)]);
        w.cli_cwd = Some(w.tmp.clone()); // CwdSupervisor: the engine steps run in `cli_cwd`
        fs::write(&w.config_file, "docs_dir = \"../notes/{repo}\"\n").unwrap();
        w.docs = resolve(
            &w.repo
                .parent()
                .unwrap()
                .join("notes")
                .join(w.repo.file_name().unwrap()),
        );
        let elsewhere = w.tmp.join("a/b");
        fs::create_dir_all(&elsewhere).unwrap();
        w.cli_cwd = Some(elsewhere);
        w.start();
        let rd = run_dir(&w);
        assert_eq!(resolve(rd.parent().unwrap().parent().unwrap()), w.docs);
        w.cli_cwd = Some(w.workspace());
        w.run();
        let found: Vec<PathBuf> = walk(&w.tmp)
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "runs.md"))
            .collect();
        let mut names: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
        names.sort();
        assert_eq!(runs_lines(&w).len(), 1, "{names:?}");
        let resolved: Vec<PathBuf> = found.iter().map(|p| resolve(p)).collect();
        assert_eq!(resolved, [w.docs.join("runs.md")]);
    }
}

mod fix_waves {
    use super::*;

    #[test]
    fn test_final_review_defects_get_a_fix_wave_then_all_gates_rerun() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        w.run();

        assert_eq!(
            w.run_dispatch_ids(),
            [
                "run-final_review-1",
                "run-w1-final_fix-1",
                "run-w1-final_review-1",
                "run-final_review-2",
                "run-evidence-1",
                "run-verify-1"
            ]
        );
        let fix = &w.run_started[1];
        assert_json_eq!(
            pick(fix, &["kind", "role", "wave"]),
            jv(r#"["final_fix", "implementer", 1]"#)
        );
        let fix_brief = read_text(&os_path(&fix["brief"]));
        assert!(fix_brief.contains("\"final_review\" failed"), "{fix_brief}");
        assert!(fix_brief.contains(FINAL), "{fix_brief}");
        let accepted_fix = one(accepted_of(&w, &Json::from("final")));
        let head = w.branch_tip(RUN);
        assert_eq!(accepted_fix["sha"], head.as_str());
        let old = text(&w.events("gate_result")[0]["examined_sha"]).to_string();
        assert_ne!(old, head);
        run_git(&w.repo, &["merge-base", "--is-ancestor", &old, &head]);
        assert_json_eq!(
            Json::Arr(gates(&w)),
            Json::Arr(vec![
                gate("final_review", &old, false),
                gate("final_review", &head, true),
                gate("evidence", &head, true),
                gate("verify", &head, true)
            ])
        );
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{RUN}:src/final/run-w1-final_fix-1.txt")]
            ),
            "run-w1-final_fix-1"
        );
        assert_eq!(outcome(&w)["outcome"], "complete");
        assert!(runs_lines(&w)[0].contains("| complete |"));
    }

    #[test]
    fn test_a_fix_waves_acceptance_intent_names_its_wave() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        w.run();
        let intent = one(w
            .events("accept_started")
            .into_iter()
            .filter(|e| e["card"] == "final")
            .collect());
        let acc = one(accepted_of(&w, &Json::from("final")));
        // The record the Python reference writes for a fix wave's acceptance.
        let got = Json::Arr(
            ["card", "wave", "target", "dispatch_id"]
                .iter()
                .map(|k| intent.get(k).cloned().unwrap_or(Json::Null))
                .collect(),
        );
        assert_json_eq!(
            got,
            Json::Arr(vec![
                "final".into(),
                Json::from(1_i64),
                acc["sha"].clone(),
                "run-w1-final_review-1".into()
            ]),
            "the fix wave's accept_started: {intent}"
        );
    }

    #[test]
    fn test_verify_fix_after_security_pass_reruns_security_and_evidence() {
        let mut w = gated(&[risky(1, "medium")]);
        w.start();
        w.script(
            None,
            "verify",
            [Step {
                result: Some("fail".into()),
                ..Step::default()
            }],
        );
        w.run();

        let first = text(&w.events("gate_result")[0]["examined_sha"]).to_string();
        let head = w.branch_tip(RUN);
        assert_ne!(first, head);
        assert_json_eq!(
            Json::Arr(gates(&w)),
            Json::Arr(vec![
                gate("final_review", &first, true),
                gate("evidence", &first, true),
                gate("security", &first, true),
                gate("verify", &first, false),
                gate("final_review", &head, true),
                gate("evidence", &head, true),
                gate("security", &head, true),
                gate("verify", &head, true)
            ])
        );
        let changed = lines(&run_git(&w.repo, &["diff", "--name-only", &first, &head]));
        assert_eq!(changed, ["src/final/run-w1-final_fix-1.txt"]);
        assert_eq!(outcome(&w)["outcome"], "complete");
    }

    #[test]
    fn test_fix_waves_are_capped() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(
            None,
            "final_review",
            (0..4).map(|_| with_verdict("defects")),
        );
        w.run();

        let fixes = run_ids_with(&w, "final_fix");
        assert_eq!(fixes, ["run-w1-final_fix-1", "run-w2-final_fix-1"]); // two waves at most
        let head = w.branch_tip(RUN);
        for wave in [1, 2] {
            assert!(
                on_run_branch(&w, &format!("src/final/run-w{wave}-final_fix-1.txt")),
                "{wave}"
            );
        }
        assert_eq!(
            gate_names(&w),
            [
                "final_review",
                "final_review",
                "final_review",
                "evidence",
                "verify"
            ]
        );
        assert_json_eq!(
            Json::Arr(last(&gates(&w), 3).to_vec()),
            Json::Arr(vec![
                gate("final_review", &head, false),
                gate("evidence", &head, true),
                gate("verify", &head, true)
            ])
        );
        let out = outcome(&w);
        assert_eq!(out["outcome"], "incomplete");
        assert!(
            json_in("unresolved review findings", &out["reasons"]),
            "{}",
            out["reasons"]
        );
        assert!(runs_lines(&w)[0].contains("| incomplete |"));
    }

    #[test]
    fn test_rejected_fix_is_not_accepted_and_spends_its_wave() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        w.script(
            None,
            "fix_review",
            [with_verdict("defects"), with_verdict("clean")],
        );
        w.run();
        assert_eq!(
            run_ids_with(&w, "final_fix"),
            ["run-w1-final_fix-1", "run-w2-final_fix-1"]
        );
        let accepted_fix = one(accepted_of(&w, &Json::from("final")));
        assert_eq!(accepted_fix["wave"], 2);
        // wave 1's commits never reached the run branch; they are kept on a ref
        let run_files = lines(&run_git(&w.repo, &["ls-tree", "-r", "--name-only", RUN]));
        assert!(
            !run_files
                .iter()
                .any(|f| f == "src/final/run-w1-final_fix-1.txt")
        );
        assert!(
            run_files
                .iter()
                .any(|f| f == "src/final/run-w2-final_fix-1.txt")
        );
        let kept = lines(&run_git(
            &w.repo,
            &["for-each-ref", "--format=%(refname)", "refs/leadhand/"],
        ));
        assert!(kept.iter().any(|r| r.ends_with("/final/1")), "{kept:?}");
        let wave1: Vec<&String> = kept.iter().filter(|r| r.ends_with("/final/1")).collect();
        let [wave1] = <[&String; 1]>::try_from(wave1)
            .unwrap_or_else(|r| panic!("expected one wave-1 ref: {r:?}"));
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{wave1}:src/final/run-w1-final_fix-1.txt")]
            ),
            "run-w1-final_fix-1"
        );
        assert_eq!(outcome(&w)["outcome"], "complete");
    }
}

mod evidence {
    use super::*;

    /// Every evidence gate finds a gap; the run's last action.
    #[track_caller]
    fn evidence_fails(config: &str) -> (FakeSupervisor, Json) {
        let mut w = gated_with(&[card(1)], config);
        w.start();
        let gap = || Step {
            result: Some("fail".into()),
            ..Step::default()
        };
        w.script(None, "evidence", [gap(), gap(), gap()]);
        let done = finish_run(&mut w);
        (w, done)
    }

    #[test]
    fn test_evidence_block_gap_ends_blocked() {
        let (w, done) = evidence_fails("[run]\nevidence = \"block\"\n");
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "decision needed"]"#)
        );
        assert_eq!(
            gate_names(&w).iter().filter(|g| *g == "evidence").count(),
            3
        );
        assert!(!gate_names(&w).iter().any(|g| g == "verify"));
        let handoff = read_text(&w.paths().handoff);
        assert!(handoff.contains("decision needed"), "{handoff}");
        assert!(handoff.contains("evidence"), "{handoff}");
        assert!(handoff.contains("leadhand run"), "{handoff}");
        assert_eq!(w.events("report_written").len(), 1);
        let line = &runs_lines(&w)[0];
        let cell = line.split('|').nth(3).expect("a fourth cell");
        assert_eq!(cell.trim(), "blocked (decision needed)");
        // a blocked run keeps its branch checked out: it resumes there
        assert_eq!(
            run_git(&w.workspace(), &["symbolic-ref", "--short", "HEAD"]),
            RUN
        );
    }

    #[test]
    fn test_evidence_record_gap_makes_incomplete_with_hold() {
        let (w, done) = evidence_fails("");
        assert_eq!(done["outcome"], "incomplete");
        assert!(
            strs(&done["reasons"])
                .iter()
                .any(|r| r.starts_with("hold: evidence")),
            "{}",
            done["reasons"]
        );
        let head = w.branch_tip(RUN);
        assert_json_eq!(
            Json::Arr(last(&gates(&w), 2).to_vec()),
            Json::Arr(vec![
                gate("evidence", &head, false),
                gate("verify", &head, true)
            ])
        );
        assert!(!w.paths().handoff.exists());
        assert!(runs_lines(&w)[0].contains("| incomplete |"));
    }

    #[test]
    fn test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head() {
        let (mut w, _done) = evidence_fails("[run]\nevidence = \"block\"\n");
        let before = w.events("gate_result").len();
        let head = w.branch_tip(RUN);
        relaunch(&w); // the operator decided the gap is fine to re-check
        assert!(!w.events("resumed").is_empty());
        w.script(
            None,
            "evidence",
            [Step {
                result: Some("pass".into()),
                ..Step::default()
            }],
        );
        let done = finish_run(&mut w);
        let later = field(&w.events("gate_result")[before..], "gate");
        assert_json_eq!(later, jv(r#"["evidence", "verify"]"#)); // final_review stayed current
        assert_eq!(done["outcome"], "complete");
        assert_eq!(w.branch_tip(RUN), head);
        assert!(!w.paths().handoff.exists()); // the blocked run's handoff is gone
        assert_eq!(runs_lines(&w).len(), 1);
        assert!(runs_lines(&w)[0].contains("| complete |"));
    }

    #[test]
    fn test_handoff_lists_the_outcomes_reasons() {
        let (w, done) = evidence_fails("[run]\nevidence = \"block\"\n");
        let reasons = strs(&done["reasons"]);
        assert!(
            reasons.iter().any(|r| r == "blocked: decision needed"),
            "{reasons:?}"
        );
        let handoff = lines(&read_text(&w.paths().handoff));
        let at = |heading: &str| {
            handoff
                .iter()
                .position(|l| l == heading)
                .unwrap_or_else(|| panic!("no {heading:?} in {handoff:#?}"))
        };
        // "## Why", a blank line, the reasons, a blank line, "## What to do".
        let why = &handoff[at("## Why") + 2..at("## What to do") - 1];
        let listed: Vec<String> = reasons.iter().map(|r| format!("- {r}")).collect();
        assert_eq!(why, listed, "the handoff's reasons are not the outcome's");
    }

    #[test]
    fn test_a_resumed_runs_line_is_replaced_in_place() {
        let (mut w, _done) = evidence_fails("[run]\nevidence = \"block\"\n");
        // A later run sharing the docs folder writes its own line after this run's.
        let other =
            "| 2026-01-01 | Another deck | complete | 1/1 cards | $0.00 | /elsewhere/another-run |";
        let index = w.docs.join("runs.md");
        fs::write(&index, format!("{}{other}\n", read_text(&index))).unwrap();
        relaunch(&w);
        w.script(
            None,
            "evidence",
            [Step {
                result: Some("pass".into()),
                ..Step::default()
            }],
        );
        assert_eq!(finish_run(&mut w)["outcome"], "complete");
        let all = lines(&read_text(&index));
        let cell = format!("| {} |", s(&run_dir(&w)));
        let ours: Vec<usize> = (0..all.len())
            .filter(|i| all[*i].ends_with(&cell))
            .collect();
        let [ours] = <[usize; 1]>::try_from(ours)
            .unwrap_or_else(|o| panic!("expected one line of this run: {o:?} in {all:#?}"));
        assert!(all[ours].contains("| complete |"), "{}", all[ours]);
        let theirs = all
            .iter()
            .position(|l| l == other)
            .unwrap_or_else(|| panic!("the other run's line is gone: {all:#?}"));
        assert!(
            ours < theirs,
            "this run's line moved below the later run's instead of being replaced in place: {all:#?}"
        );
    }
}

mod security {
    use super::*;

    /// A gated run of `deck_card` whose implementer writes `files` (its default file when
    /// empty); the security dispatches.
    #[track_caller]
    fn security_ran(deck_card: String, written: &[(&str, &str)]) -> (FakeSupervisor, Vec<Json>) {
        let mut w = gated(&[deck_card]);
        w.start();
        if !written.is_empty() {
            w.script(1, "implement", [files(written)]);
        }
        w.run();
        let ran = w
            .run_started
            .iter()
            .filter(|a| a["kind"] == "security")
            .cloned()
            .collect();
        (w, ran)
    }

    #[test]
    fn test_security_gate_only_for_medium_or_high_or_sensitive_paths() {
        let (w, ran) = security_ran(card(1), &[]);
        assert_empty!(ran);
        assert_eq!(outcome(&w)["outcome"], "complete");

        for risk in ["medium", "high"] {
            let (w, ran) = security_ran(risky(1, risk), &[]);
            let sec = one(ran);
            assert_eq!(sec["role"], "reviewer_hard", "risk={risk}");
            assert_eq!(
                gate_names(&w),
                ["final_review", "evidence", "security", "verify"],
                "risk={risk}"
            );
        }

        for path in [
            ".github/workflows/ci.yml",
            "scripts/install.sh",
            "db/migrations/0001_init.sql",
            "src/auth/session.py",
            "terraform/main.tf",
        ] {
            let (w, ran) = security_ran(card(1), &[(path, "x\n")]);
            assert_eq!(ran.len(), 1, "{path}");
            assert!(on_run_branch(&w, path), "{path}");
        }

        // paths that are not sensitive
        for path in ["src/widget.py", "docs/authoring.md"] {
            let (w, ran) = security_ran(card(1), &[(path, "x\n")]);
            assert!(ran.is_empty(), "{path}: {ran:?}");
            assert!(!gate_names(&w).iter().any(|g| g == "security"), "{path}");
            assert!(on_run_branch(&w, path), "{path}");
        }
    }
}

mod verification {
    use super::*;

    #[test]
    fn test_verification_failure_after_last_wave_is_failed() {
        let mut w = gated(&[card(1)]);
        w.start();
        let fail = || Step {
            result: Some("fail".into()),
            ..Step::default()
        };
        w.script(None, "verify", [fail(), fail(), fail()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "failed");
        assert_eq!(
            run_ids_with(&w, "final_fix"),
            ["run-w1-final_fix-1", "run-w2-final_fix-1"]
        );
        assert_eq!(gate_names(&w).iter().filter(|g| *g == "verify").count(), 3);
        assert!(
            json_in("verification of the final commit failed", &done["reasons"]),
            "{}",
            done["reasons"]
        );
        let report = w.paths().report;
        assert!(report.is_file());
        assert!(
            lines(&read_text(&report))
                .iter()
                .any(|l| l == "Outcome: failed")
        );
        assert!(!w.paths().handoff.exists());
        assert!(runs_lines(&w)[0].contains("| failed |"));
    }
}

mod unpark {
    use super::*;

    #[test]
    fn test_unparked_card_waits_for_an_open_gate_then_gates_rerun() {
        let mut w = gated(&[card(1), card(2)]);
        w.session("supervisor-1");
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks
            .insert(1, "product decision: storage format".to_string());
        let gate_action = drive_to_run(&mut w, |_| true);
        assert_eq!(gate_action["dispatch_id"], "run-final_review-1");
        let worker = w.launch(&gate_action);
        w.cli(&os_args![
            "unpark",
            run_dir(&w),
            "--card",
            "1",
            "--note",
            "Use JSON."
        ])
        .ok();
        let waiting = w.next();
        assert_json_eq!(
            pick(&waiting, &["action", "dispatch_id"]),
            jv(r#"["wait", "run-final_review-1"]"#)
        );
        finish(&mut w, &gate_action, &worker);
        w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
        let done = finish_run(&mut w);
        // Python builds its message eagerly: a done action without `reasons` raises
        let (reasons, accepted) = (&done["reasons"], w.events("accepted"));
        assert_eq!(done["outcome"], "complete", "{reasons} {accepted:?}");
        let head = w.branch_tip(RUN);
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[2, 1]"));
        for n in [1, 2] {
            assert!(
                on_run_branch(&w, &format!("src/card{n}/c{n}-a1-implement-1.txt")),
                "{n}"
            );
        }
        assert_json_eq!(
            Json::Arr(last(&gates(&w), 3).to_vec()),
            Json::Arr(vec![
                gate("final_review", &head, true),
                gate("evidence", &head, true),
                gate("verify", &head, true)
            ])
        );
    }
}

mod guards {
    use super::*;

    /// The worker puts the run branch back at the run's base.
    fn rewind(world: &FakeSupervisor, ws: &Path, _a: &Json) {
        run_git(ws, &["branch", "-f", RUN, &world.base]);
    }

    #[test]
    fn test_final_fix_moving_the_run_branch_is_voided_and_restored() {
        let mut w = gated(&[card(1), card(2)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        w.script(None, "final_fix", [tampering(rewind), Step::default()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        let v = one(found_violations(&w));
        assert_json_eq!(
            pick(&v, &["dispatch_id", "dispatch_kind"]),
            jv(r#"["run-w1-final_fix-1", "final_fix"]"#)
        );
        let ids = w.run_dispatch_ids();
        assert!(ids.iter().any(|i| i == "run-w1-final_fix-2"), "{ids:?}");
        let head = w.branch_tip(RUN);
        for e in w.events("accepted") {
            run_git(
                &w.repo,
                &["merge-base", "--is-ancestor", text(&e["sha"]), &head],
            );
        }
        let cards: Vec<Json> = w
            .events("accepted")
            .into_iter()
            .filter(|e| e["card"] != "final")
            .collect();
        assert_eq!(sorted_cards(&cards), [1, 2]);
        for n in [1, 2] {
            assert!(
                on_run_branch(&w, &format!("src/card{n}/c{n}-a1-implement-1.txt")),
                "{n}"
            );
        }
    }

    #[test]
    fn test_accepted_work_missing_from_the_run_branch_fails_the_run() {
        // Card 1 is accepted; while card 2 waits on the supervisor and no worker is out, the
        // run branch is moved back to the base and the run is relaunched. Reconciling after
        // the relaunch finds card 1's accepted work missing from the run branch: the run ends
        // failed.
        let mut w = gated(&[
            card(1),
            card_with(
                2,
                CardOpts {
                    depends: &[1],
                    ..CardOpts::default()
                },
            ),
        ]);
        w.start();
        w.script(2, "implement", [with_status("QUESTION")]);
        let mut a = Json::Null;
        for _ in 0..40 {
            a = w.next();
            if a["action"] == "answer" {
                break;
            }
            w.act(&a);
        }
        assert_json_eq!(pick(&a, &["action", "card"]), jv(r#"["answer", 2]"#));
        let accepted = one(w.events("accepted"));
        let sha = text(&accepted["sha"]).to_string();
        assert_eq!(w.branch_tip(RUN), sha);
        run_git(
            &w.repo,
            &["update-ref", &format!("refs/heads/{RUN}"), &w.base],
        );
        relaunch(&w);
        assert!(!w.events("resumed").is_empty());
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "failed");
        let at = format!("card 1 at {}", &sha[..12]);
        assert!(
            strs(&done["reasons"]).iter().any(|r| r
                .contains("accepted work missing from the run branch")
                && r.contains(&at)),
            "{}",
            done["reasons"]
        );
        assert_eq!(w.branch_tip(RUN), w.base);
        assert!(runs_lines(&w)[0].contains("| failed |"));
    }

    #[test]
    fn test_evidence_base_worktree_is_reset_and_guarded() {
        let tamper = |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
            let bw = base_wt(world);
            git::write(&bw, &[("tests/leftover.txt", "x\n")]);
            commit(
                &bw,
                &[("src/moved.txt", "moved\n")],
                "verifier moved the base",
            );
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "evidence", [tampering(tamper), Step::default()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        let v = one(found_violations(&w));
        assert_json_eq!(
            pick(&v, &["dispatch_id", "dispatch_kind"]),
            jv(r#"["run-evidence-1", "evidence"]"#)
        );
        let rerun = w
            .run_started
            .iter()
            .find(|a| a["dispatch_id"] == "run-evidence-2")
            .expect("the evidence re-run");
        assert!(rerun["resume"].is_null(), "{rerun}");
        let bw = base_wt(&w);
        assert_eq!(run_git(&bw, &["rev-parse", "HEAD"]), w.base);
        assert_eq!(run_git(&bw, &["status", "--porcelain", "--ignored"]), "");
        assert_eq!(read_text(&marker(&bw)).trim(), run_id(&w));
        let evidence: Vec<Json> = w
            .events("gate_result")
            .into_iter()
            .filter(|e| e["gate"] == "evidence")
            .collect();
        assert_json_eq!(field(&evidence, "dispatch_id"), jv(r#"["run-evidence-2"]"#));
    }

    /// The `found` record of an evidence base violation carries the voided request's guard
    /// (`expected_head`, `expected_branch`) and `head_moved`, as `docs/cli-contract.md` lists
    /// them.
    #[test]
    fn test_evidence_base_violation_records_the_requests_guard() {
        let tamper = |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
            let bw = base_wt(world);
            commit(
                &bw,
                &[("src/moved.txt", "moved\n")],
                "verifier moved the base",
            );
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "evidence", [tampering(tamper), Step::default()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete", "{}", done["reasons"]);
        let v = one(found_violations(&w));
        let req = one(w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["dispatch_id"] == v["dispatch_id"])
            .collect());
        assert_eq!(req["dispatch_id"], "run-evidence-1");
        for key in ["expected_head", "expected_branch"] {
            let guard = req
                .get(key)
                .unwrap_or_else(|| panic!("the request has no {key}: {req}"));
            assert_eq!(v.get(key), Some(guard), "{key}: {v}");
        }
        assert_eq!(
            v.get("head_moved").and_then(Json::as_bool),
            Some(true),
            "{v}"
        );
    }

    #[test]
    fn test_evidence_base_edit_hidden_behind_an_index_flag_is_violation() {
        let tamper = |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
            let bw = base_wt(world);
            run_git(&bw, &["update-index", "--assume-unchanged", "README.md"]);
            git::write(&bw, &[("README.md", "edited in the base, hidden\n")]);
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "evidence", [tampering(tamper), Step::default()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        let v = one(found_violations(&w));
        assert_json_eq!(
            pick(&v, &["dispatch_id", "dispatch_kind"]),
            jv(r#"["run-evidence-1", "evidence"]"#)
        );
        let bw = base_wt(&w);
        assert_eq!(
            run_git(&bw, &["ls-files", "-v", "--", "README.md"]),
            "H README.md"
        );
        assert_eq!(read_text(&bw.join("README.md")), "hello\n");
    }

    #[test]
    fn test_gate_violation_reruns_with_a_fresh_worker() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "changed by the final reviewer\n")]);
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [tampering(tamper), Step::default()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        let v = one(found_violations(&w));
        assert_json_eq!(
            pick(&v, &["dispatch_id", "card"]),
            jv(r#"["run-final_review-1", null]"#)
        );
        assert_eq!(
            w.run_dispatch_ids()[..2],
            ["run-final_review-1", "run-final_review-2"]
        );
        assert!(w.run_started[1]["resume"].is_null());
        let reviews: Vec<Json> = w
            .events("gate_result")
            .into_iter()
            .filter(|e| e["gate"] == "final_review")
            .collect();
        assert_json_eq!(
            field(&reviews, "dispatch_id"),
            jv(r#"["run-final_review-2"]"#)
        );
        assert_eq!(read_text(&w.workspace().join("README.md")), "hello\n");
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }

    #[test]
    fn test_gate_violation_cap_ends_blocked() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "changed by the verifier\n")]);
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "verify", (0..4).map(|_| tampering(tamper)));
        let done = finish_run(&mut w);
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "decision needed"]"#)
        );
        assert_eq!(found_violations(&w).len(), 3); // the cap of two voided re-runs, plus one
        assert_eq!(
            run_ids_with(&w, "verify"),
            ["run-verify-1", "run-verify-2", "run-verify-3"]
        );
        assert!(
            strs(&done["reasons"])
                .iter()
                .any(|r| r.contains("verify workers change tracked files: README.md"))
        );
        assert!(w.paths().handoff.is_file());
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }
}

mod run_worker_failures {
    use super::*;

    #[test]
    fn test_gate_without_a_result_twice_ends_blocked_environment() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "verify", [with_status("READY"), with_status("READY")]);
        let done = finish_run(&mut w);
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "environment"]"#)
        );
        assert_eq!(run_ids_with(&w, "verify"), ["run-verify-1", "run-verify-2"]);
        assert!(w.paths().handoff.is_file());
    }

    #[test]
    fn test_gate_usage_limit_ends_blocked_provider_capacity() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(
            None,
            "evidence",
            [Step {
                blocked_by: Some("usage limit".into()),
                ..with_status("BLOCKED")
            }],
        );
        let done = finish_run(&mut w);
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "provider capacity"]"#)
        );
        relaunch(&w); // capacity is back
        assert_eq!(finish_run(&mut w)["outcome"], "complete");
        assert_eq!(
            run_ids_with(&w, "evidence"),
            ["run-evidence-1", "run-evidence-2"]
        );
    }
}

mod crashes {
    use super::*;

    #[test]
    fn test_crash_before_gate_record_records_once() {
        let mut w = gated(&[card(1)]);
        w.start();
        let a = drive_to_run(&mut w, |a| a["gate"] == "final_review");
        w.act(&a);
        let r = next_crashing(&w, "before_gate_record");
        assert_eq!(r.code, 97, "{r}");
        assert_empty!(w.events("gate_result"));
        assert_eq!(finish_run(&mut w)["outcome"], "complete");
        assert_json_eq!(
            field(&w.events("gate_result"), "dispatch_id"),
            jv(r#"["run-final_review-1", "run-evidence-1", "run-verify-1"]"#)
        );
    }

    /// A fix wave's review passes, then `next` crashes at `point` while accepting the fix:
    /// the world, the run tip before, and the accept intent.
    #[track_caller]
    fn crash_final_accept(point: &str) -> (FakeSupervisor, String, Json) {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        let a = drive_to_run(&mut w, |a| {
            a["kind"] == "final_review" && truthy(&a["wave"])
        });
        w.act(&a);
        let before = w.branch_tip(RUN);
        let r = next_crashing(&w, point);
        assert_eq!(r.code, 97, "{r}");
        let intent = one(w
            .events("accept_started")
            .into_iter()
            .filter(|e| e["card"] == "final")
            .collect());
        assert_empty!(accepted_of(&w, &Json::from("final")));
        (w, before, intent)
    }

    #[test]
    fn test_crash_before_final_fast_forward_reconciles() {
        let (mut w, before, intent) = crash_final_accept("before_fast_forward");
        assert_eq!(w.branch_tip(RUN), before);
        assert_eq!(finish_run(&mut w)["outcome"], "complete");
        let acc = one(accepted_of(&w, &Json::from("final")));
        assert_json_eq!(acc["sha"], intent["target"]);
        let starts = w
            .events("accept_started")
            .into_iter()
            .filter(|e| e["card"] == "final")
            .count();
        assert_eq!(starts, 1);
        assert_eq!(w.branch_tip(RUN), text(&intent["target"]));
        assert_eq!(w.branch_tip(FINAL), text(&intent["target"]));
    }

    #[test]
    fn test_crash_after_final_fast_forward_reconciles() {
        let (mut w, _before, intent) = crash_final_accept("after_fast_forward");
        assert_eq!(w.branch_tip(RUN), text(&intent["target"]));
        let reflog_args = [
            "reflog",
            "show",
            "--format=%H",
            &format!("refs/heads/{RUN}"),
        ];
        let reflog = run_git(&w.repo, &reflog_args);
        assert_eq!(finish_run(&mut w)["outcome"], "complete");
        let acc = one(accepted_of(&w, &Json::from("final")));
        assert_json_eq!(acc["sha"], intent["target"]);
        assert_eq!(run_git(&w.repo, &reflog_args), reflog);
    }
}

mod fix_round2 {
    use super::*;

    /// Covers R37.
    #[test]
    fn test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash() {
        let mut w = gated(&[card(1), card(2)]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks.insert(1, "product decision".to_string());
        w.script(None, "final_review", [with_verdict("defects")]);
        w.script(None, "final_fix", [with_status("QUESTION")]);
        let fix = drive_to_run(&mut w, |a| a["kind"] == "final_fix");
        let worker = w.launch(&fix);
        w.cli(&os_args![
            "unpark",
            run_dir(&w),
            "--card",
            "1",
            "--note",
            "Use JSON."
        ])
        .ok();
        w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
        finish(&mut w, &fix, &worker);
        for _ in 0..60 {
            let a = w.next();
            w.act(&a);
            if a["action"] == "dispatch" && a["card"] == 1 && a["kind"] == "verify" {
                break;
            }
        }
        let r = next_crashing(&w, "after_fast_forward");
        assert_eq!(r.code, 97, "{r}");
        let done = finish_run(&mut w);
        let reasons = &done["reasons"]; // Python's message, built eagerly: a KeyError when missing
        assert_eq!(done["outcome"], "complete", "{reasons}");
        assert_empty!(found_violations(&w));
        assert_eq!(sorted_cards(&w.events("accepted")), [1, 2]);
        let head = w.branch_tip(RUN);
        for e in w.events("accepted") {
            run_git(
                &w.repo,
                &["merge-base", "--is-ancestor", text(&e["sha"]), &head],
            );
        }
        for n in [1, 2] {
            assert!(
                on_run_branch(&w, &format!("src/card{n}/c{n}-a1-implement-1.txt")),
                "{n}"
            );
        }
        let spent = one(w
            .events("verdict_recorded")
            .into_iter()
            .filter(|e| e.get("dispatch_id") == Some(&fix["dispatch_id"]))
            .collect());
        assert_eq!(spent["verdict"], "spent");
    }

    #[test]
    fn test_crash_between_base_violation_found_and_restored_recovers() {
        // The Python test crashed the engine's `git update-ref` of the kept base HEAD; here
        // that Git call fails, as a held ref lock makes it, and `next` stops there.
        let mv = |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
            commit(&base_wt(world), &[("src/moved.txt", "m\n")], "moved base");
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "evidence", [tampering(mv), Step::default()]);
        let keep = format!("refs/leadhand/{}/violations/1-base", run_id(&w));
        let common_dir = run_git(
            &w.repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let lock = PathBuf::from(common_dir).join(format!("{keep}.lock"));
        let parent = lock.parent().unwrap();
        if parent.exists() {
            // Python's mkdir(parents=True) without exist_ok
            panic!("{} already exists", parent.display());
        }
        fs::create_dir_all(parent).unwrap();
        fs::write(&lock, "").unwrap();
        let r = drive_until_refused(&mut w);
        assert_ne!(r.code, 0, "{r}");
        assert_json_eq!(field(&found_violations(&w), "stage"), jv(r#"["found"]"#));
        assert_eq!(
            run_git(&w.repo, &["for-each-ref", "--format=%(refname)", &keep]),
            ""
        );
        fs::remove_file(&lock).unwrap();
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        let stages = field(&w.events("violation"), "stage");
        assert_json_eq!(stages, jv(r#"["found", "restored"]"#));
        assert_eq!(run_git(&base_wt(&w), &["rev-parse", "HEAD"]), w.base);
        let kept = lines(&run_git(
            &w.repo,
            &["for-each-ref", "--format=%(refname)", "refs/leadhand/"],
        ));
        assert!(kept.iter().any(|k| k.ends_with("-base")), "{kept:?}");
        assert_eq!(
            run_git(&w.repo, &["log", "-1", "--format=%s", &keep]),
            "moved base"
        );
    }

    #[test]
    fn test_evidence_rerun_after_a_worker_failure_gets_a_reset_base() {
        let seen: Rc<RefCell<Option<BTreeSet<String>>>> = Rc::new(RefCell::new(None));

        let leave = |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
            git::write(
                &base_wt(world),
                &[("tests/leftover_test.py", "x\n"), ("build/cache.o", "y\n")],
            );
        };
        let look = {
            let seen = Rc::clone(&seen);
            move |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
                let names = walk(&base_wt(world))
                    .into_iter()
                    .filter(|p| p.is_file() && !p.components().any(|c| c.as_os_str() == ".git"))
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                *seen.borrow_mut() = Some(names);
            }
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(
            None,
            "evidence",
            [
                Step {
                    status: Some("BLOCKED".into()),
                    ..tampering(leave)
                },
                tampering(look),
            ],
        );
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        assert_eq!(
            run_ids_with(&w, "evidence"),
            ["run-evidence-1", "run-evidence-2"]
        );
        let files = seen
            .borrow()
            .clone()
            .expect("the second evidence worker looked");
        assert!(files.contains("README.md"), "{files:?}");
        assert!(!files.contains("leftover_test.py"), "{files:?}");
        assert!(!files.contains("cache.o"), "{files:?}");
    }

    /// Covers R36.
    #[test]
    fn test_voided_final_fix_reruns_from_the_run_head() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        let rewind = |world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["branch", "-f", RUN, &world.base]);
        };
        w.script(None, "final_fix", [tampering(rewind), Step::default()]);
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete");
        let v = one(found_violations(&w));
        let run_files = lines(&run_git(&w.repo, &["ls-tree", "-r", "--name-only", RUN]));
        // the voided worker's commit
        assert!(
            !run_files
                .iter()
                .any(|f| f == "src/final/run-w1-final_fix-1.txt")
        );
        assert!(
            run_files
                .iter()
                .any(|f| f == "src/final/run-w1-final_fix-2.txt")
        );
        let review = w
            .run_started
            .iter()
            .find(|a| a["kind"] == "final_review" && truthy(&a["wave"]))
            .expect("a fix wave's review");
        let req = w
            .events("dispatch_requested")
            .into_iter()
            .find(|e| e["dispatch_id"] == review["dispatch_id"])
            .expect("its dispatch_requested");
        let range = text(&req["extra"]["diff_range"]);
        let parts: Vec<&str> = range.split("..").collect();
        let [base, tip] =
            <[&str; 2]>::try_from(parts).unwrap_or_else(|p| panic!("not a base..tip range: {p:?}"));
        assert_eq!(
            lines(&run_git(&w.repo, &["diff", "--name-only", base, tip])),
            ["src/final/run-w1-final_fix-2.txt"]
        );
        let keep = format!("refs/leadhand/{}/violations/{}-final", run_id(&w), v["n"]);
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{keep}:src/final/run-w1-final_fix-1.txt")]
            ),
            "run-w1-final_fix-1"
        );
    }

    /// R36, twice: each voided final fix's commits are kept under its own violation's number,
    /// so the second keep ref never takes the first one's place.
    #[test]
    fn test_each_voided_final_fix_is_kept_under_its_own_violation_number() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(None, "final_review", [with_verdict("defects")]);
        let rewind = |world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["branch", "-f", RUN, &world.base]);
        };
        w.script(
            None,
            "final_fix",
            [tampering(rewind), tampering(rewind), Step::default()],
        );
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "complete", "{}", done["reasons"]);
        let found = found_violations(&w);
        assert_json_eq!(
            field(&found, "dispatch_id"),
            jv(r#"["run-w1-final_fix-1", "run-w1-final_fix-2"]"#)
        );
        // Worker k's own file, read from the keep ref of the violation that voided it.
        let kept: Vec<String> = found
            .iter()
            .zip(1..)
            .map(|(v, k)| {
                let keep = format!("refs/leadhand/{}/violations/{}-final", run_id(&w), v["n"]);
                let path = format!("{keep}:src/final/run-w1-final_fix-{k}.txt");
                run_git_unchecked(&w.repo, &["show", &path])
            })
            .collect();
        assert_eq!(kept, ["run-w1-final_fix-1", "run-w1-final_fix-2"]);
    }
}

mod nothing_accepted {
    use super::*;

    #[test]
    fn test_run_with_every_card_parked_runs_no_gates() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks.insert(1, "product decision".to_string());
        let done = finish_run(&mut w);
        assert_empty!(w.run_started);
        assert_empty!(w.events("gate_started"));
        assert_json_eq!(
            pick(&done, &["outcome", "qualifier"]),
            jv(r#"["blocked", "decision needed"]"#)
        );
        assert!(w.paths().report.is_file());
        assert!(!base_wt(&w).exists());
        assert_eq!(w.branch_tip(RUN), w.base);
    }
}

/// The evidence base worktree is checked out, hard-reset and cleaned before every evidence
/// request: only ever one this run recorded adding, never an unrelated worktree that happens
/// to sit at its path. A run refuses one with exit code 1 and `error: <path> is a worktree
/// this run did not add ...`.
mod base_worktree_ownership {
    use super::*;

    /// The operator's own worktree on a branch of its own, with untracked notes in it.
    #[track_caller]
    fn foreign_worktree(w: &FakeSupervisor, path: &Path) -> PathBuf {
        run_git(
            &w.repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "operator-notes",
                s(path),
                &w.base,
            ],
        );
        git::write(
            path,
            &[("valuable.txt", "the operator's untracked notes\n")],
        );
        path.to_path_buf()
    }

    /// The operator's worktree at `path` is as it was.
    macro_rules! assert_untouched {
        ($path:expr) => {{
            let path: &Path = &$path;
            assert_eq!(
                read_text(&path.join("valuable.txt")),
                "the operator's untracked notes\n"
            );
            assert_eq!(
                run_git(path, &["symbolic-ref", "--short", "HEAD"]),
                "operator-notes"
            );
        }};
    }

    /// `r` is the refusal of the worktree at `path`.
    macro_rules! assert_refused {
        ($r:expr, $path:expr) => {{
            let r: Out = $r;
            let path: &Path = &$path;
            assert_eq!(r.code, 1, "{r}");
            let want = format!(
                "error: {} is a worktree this run did not add",
                path.display()
            );
            assert!(r.stderr_text().contains(&want), "{r}");
        }};
    }

    /// The state a crash right after the run's `git worktree add` leaves: the add recorded,
    /// the worktree detached at the base, without the run's marker.
    #[track_caller]
    fn crash_window(w: &FakeSupervisor) -> PathBuf {
        let path = base_wt(w);
        append_event(
            &run_dir(w),
            "base_worktree_added",
            [("path", Json::from(s(&path)))],
        );
        run_git(
            &w.repo,
            &["worktree", "add", "--quiet", "--detach", s(&path), &w.base],
        );
        assert!(!marker(&path).exists());
        path
    }

    #[test]
    fn test_worktree_run_refuses_a_foreign_worktree_at_its_base_path() {
        let mut w = gated(&[card(1)]);
        w.start();
        let path = foreign_worktree(&w, &base_wt(&w));
        assert_refused!(drive_until_refused(&mut w), path);
        assert_untouched!(path);
        assert_eq!(w.events("gate_result")[0]["gate"], "final_review");
        assert!(!gate_names(&w).iter().any(|g| g == "evidence"));
    }

    #[test]
    fn test_a_foreign_worktree_after_a_failed_add_is_refused() {
        // The intent was recorded, then the add failed or crashed, and a worktree appeared
        // there.
        let mut w = gated(&[card(1)]);
        w.start();
        let path = base_wt(&w);
        append_event(
            &run_dir(&w),
            "base_worktree_added",
            [("path", Json::from(s(&path)))],
        );
        foreign_worktree(&w, &path);
        assert_refused!(drive_until_refused(&mut w), path);
        assert_untouched!(path);
    }

    #[test]
    fn test_a_foreign_worktree_replacing_the_runs_base_worktree_is_refused() {
        // While the first evidence worker runs, the run's base worktree is removed and an
        // unrelated one added at its path; the worker ends without a report, so the evidence
        // gate is requested again.
        let replace = |world: &FakeSupervisor, _ws: &Path, _a: &Json| {
            let path = base_wt(world);
            assert_eq!(
                read_text(&marker(&path)).trim(),
                text(&world.meta()["run_id"])
            );
            run_git(&world.repo, &["worktree", "remove", "--force", s(&path)]);
            foreign_worktree(world, &path);
        };
        let mut w = gated(&[card(1)]);
        w.start();
        w.script(
            None,
            "evidence",
            [Step {
                status: Some("BLOCKED".into()),
                ..tampering(replace)
            }],
        );
        let path = base_wt(&w);
        assert_refused!(drive_until_refused(&mut w), path);
        assert_eq!(run_ids_with(&w, "evidence"), ["run-evidence-1"]);
        assert_untouched!(path);
    }

    #[test]
    fn test_a_base_worktree_left_by_a_crash_is_refused_once_it_holds_work() {
        let mut w = gated(&[card(1)]);
        w.start();
        let path = crash_window(&w);
        git::write(
            &path,
            &[("valuable.txt", "written there while the run was down\n")],
        );
        let r = drive_until_refused(&mut w);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        assert_eq!(
            read_text(&path.join("valuable.txt")),
            "written there while the run was down\n"
        );
    }

    #[test]
    fn test_a_crash_window_worktree_hiding_a_tracked_edit_is_refused() {
        // `status` does not show an edit behind assume-unchanged (or a skip-worktree file),
        // but the reset would overwrite it (or leave the evidence base off its commit).
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            let mut w = gated(&[card(1)]);
            w.start();
            let path = crash_window(&w);
            git::write(&path, &[("README.md", "an edit someone hid from status\n")]);
            run_git(&path, &["update-index", flag, "README.md"]);
            assert_eq!(
                run_git(
                    &path,
                    &[
                        "status",
                        "--porcelain",
                        "--ignored",
                        "--untracked-files=all"
                    ]
                ),
                "",
                "flag={flag}"
            );
            let r = drive_until_refused(&mut w);
            assert_eq!(r.code, 1, "flag={flag}: {r}");
            assert!(r.stderr_text().starts_with("error: "), "flag={flag}: {r}");
            assert_eq!(
                read_text(&path.join("README.md")),
                "an edit someone hid from status\n",
                "flag={flag}"
            );
        }
    }

    #[test]
    fn test_a_base_worktree_added_before_a_crash_is_still_the_runs() {
        let mut w = gated(&[card(1)]);
        w.start();
        let path = crash_window(&w);
        assert_eq!(finish_run(&mut w)["outcome"], "complete");
        assert_eq!(run_git(&path, &["rev-parse", "HEAD"]), w.base);
        assert_eq!(read_text(&marker(&path)).trim(), run_id(&w));
        let passed: Vec<Json> = w
            .events("gate_result")
            .into_iter()
            .filter(|e| truthy(&e["passed"]))
            .collect();
        assert_json_eq!(
            field(&passed, "gate"),
            jv(r#"["final_review", "evidence", "verify"]"#)
        );
    }

    #[test]
    fn test_a_clean_worktree_at_the_base_path_outside_the_crash_window_is_refused() {
        // Detached at the base and holding nothing, but with no add recorded: not the run's.
        let mut w = gated(&[card(1)]);
        w.start();
        let path = base_wt(&w);
        run_git(
            &w.repo,
            &["worktree", "add", "--quiet", "--detach", s(&path), &w.base],
        );
        assert!(!marker(&path).exists());
        assert_refused!(drive_until_refused(&mut w), path);
        assert!(!marker(&path).exists(), "the run claimed the worktree");
        assert_empty!(w.events("base_worktree_ready"));
    }
}

/// R63: sparse checkouts are not supported in this version; so every assume-unchanged or
/// skip-worktree entry seen after a worker is the worker's, a sparse checkout it turned on
/// itself included.
mod sparse_checkout {
    use super::*;

    #[test]
    fn test_a_worktree_run_in_a_sparse_checkout_is_refused_at_start() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.base = commit(
            &w.repo,
            &[("far/away.txt", "outside the sparse patterns\n")],
            "far",
        );
        w.write_deck(&[card(1)]);
        run_git(
            &w.repo,
            &["sparse-checkout", "set", "--no-cone", "/README.md", "/src/"],
        );
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        assert!(
            r.stderr_text()
                .contains("sparse checkouts are not supported"),
            "{r}"
        );
        assert!(!w.docs.join("runs").exists());
        assert_eq!(
            run_git(&w.repo, &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
    }

    #[test]
    fn test_a_sparse_checkout_a_reviewer_turns_on_is_a_violation() {
        // Turning sparse checkout on hides README.md (skip-worktree, file removed): the gates
        // would check a tree without it. Leadhand unhides it, counts it, and turns sparse off.
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["sparse-checkout", "set", "--no-cone", "/src/"]);
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "review", [tampering(tamper), Step::default()]);
        w.run();
        let found: Vec<Json> = w
            .events("violation")
            .into_iter()
            .filter(|e| e.get("stage").is_some_and(|s| s == "found"))
            .collect();
        let ids = Json::Arr(
            found
                .iter()
                .map(|e| e.get("dispatch_id").cloned().unwrap_or(Json::Null))
                .collect(),
        );
        assert_json_eq!(ids, jv(r#"["c1-a1-review-1"]"#));
        assert!(json_in("README.md", &found[0]["hidden"]), "{}", found[0]);
        assert!(json_in("README.md", &found[0]["paths"]), "{}", found[0]);
        let ws = w.workspace();
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
        assert!(
            !lines(&run_git(&ws, &["ls-files", "-v"]))
                .iter()
                .any(|l| l == "S README.md")
        );
        assert_ne!(
            run_git_unchecked(&ws, &["config", "--bool", "--get", "core.sparseCheckout"]),
            "true"
        );
    }
}

// --------------------------------------------------------------------------
// Python's `test_final_review_minors`

/// `timer RUN_DIR --dispatch ID` running in the background; the world's cleanup kills it
/// (`TimerProc`).
#[track_caller]
fn timer_proc(w: &mut FakeSupervisor, dispatch_id: &str) -> Proc {
    let child = Command::new(bin())
        .args([
            OsStr::new("timer"),
            run_dir(w).as_os_str(),
            OsStr::new("--dispatch"),
            OsStr::new(dispatch_id),
        ])
        .current_dir(&w.tmp)
        .env_clear()
        .envs(w.clock_env().vars())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("starting {}: {e}", bin().display()));
    let p = Proc::new(child);
    w.proxies.push(p.clone());
    p
}

mod timer {
    use super::*;

    #[test]
    fn test_timer_returns_promptly_when_the_run_is_paused_or_cancelled() {
        // `timer` looks for the flags between its polls (every 10 s at most), not only at its
        // start.
        for request in ["pause", "cancel"] {
            let mut w = FakeSupervisor::new(&[card(1)]);
            w.start();
            let a = w.next();
            w.launch(&a);
            let timer = timer_proc(&mut w, text(&a["dispatch_id"]));
            // still waiting: no result, no flag, no deadline
            assert!(
                wait_timeout(&timer, Duration::from_secs_f64(1.5)).is_none(),
                "request={request}: the timer returned at once"
            );
            w.cli(&os_args![request, run_dir(&w)]).ok();
            let (out, err) = timer
                .communicate(Duration::from_secs(30))
                .unwrap_or_else(|| {
                    panic!("request={request}: the timer was still running 30 s on")
                });
            assert_eq!(
                timer.returncode(),
                Some(0),
                "request={request}: {}",
                String::from_utf8_lossy(&err)
            );
            assert_eq!(
                lines(&String::from_utf8_lossy(&out)),
                ["pause or cancel requested"],
                "request={request}"
            );
        }
    }
}

mod pinned_integrate {
    use super::*;

    #[test]
    fn test_integrate_after_a_codex_pinned_card_runs_on_claude() {
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
            Options::default(),
        );
        w.session("supervisor-1");
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                status: Some("QUESTION".into()),
                ..files(&[("src/shared.txt", "card one\n")])
            }],
        );
        w.script(2, "implement", [files(&[("src/shared.txt", "card two\n")])]);
        w.parks
            .insert(1, "decision needed: storage format".to_string());
        w.run();
        w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
        w.cli(&os_args![
            "unpark",
            run_dir(&w),
            "--card",
            "1",
            "--note",
            "Use JSON."
        ])
        .ok();
        w.run();
        let reasons = field(&w.events("parked"), "reason");
        assert!(
            !json_in("integration needs a Claude worker", &reasons),
            "{reasons}"
        );
        assert!(
            !json_in("pinned vendor not installed", &reasons),
            "{reasons}"
        );
        let integrate: Vec<Json> = w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["dispatch_kind"] == "integrate")
            .collect();
        assert!(!integrate.is_empty());
        assert_eq!(integrate[0]["vendor"], "claude");
        assert!(
            w.started
                .iter()
                .any(|a| a["kind"] == "integrate" && a["agent"] != "leadhand:codex")
        );
        assert_eq!(sorted_cards(&w.events("accepted")), [1, 2]);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card one"
        );
    }
}

mod operator_commands {
    use super::*;

    #[test]
    fn test_pause_cancel_and_handoff_print_the_quoted_absolute_cli() {
        // The commands name the binary (compared by file, S1-R11) and the quoted run
        // directory.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let rd = run_dir(&w);
        let root = shlex_quote(s(&rd));
        assert!(s(&rd).contains(' '));
        let run = format!(" run {root}");
        let paused = w.cli(&os_args!["pause", rd]).ok().stdout_text();
        assert!(names_bin_before(&paused, &run), "{paused}");
        let cancelled = w.cli(&os_args!["cancel", rd]).ok().stdout_text();
        assert!(names_bin_before(&cancelled, &run), "{cancelled}");
        w.script(
            1,
            "implement",
            [Step {
                blocked_by: Some("usage limit".into()),
                ..with_status("BLOCKED")
            }],
        );
        fs::remove_file(w.paths().cancel_flag).unwrap();
        let done = finish_run(&mut w);
        assert_eq!(done["outcome"], "blocked");
        let text = read_text(&w.paths().handoff);
        assert!(names_bin_before(&text, &run), "{text}");
        assert!(
            names_bin_before(&text, &format!(" launch-line {root}")),
            "{text}"
        );
        assert!(
            names_bin_before(&text, &format!(" unpark {root}")),
            "{text}"
        );
    }

    #[test]
    fn test_relaunch_refusal_names_the_quoted_start_command() {
        let mut w = gated(&[card(1)]);
        w.start();
        w.run();
        let r = w.cli_with(
            &os_args!["run", run_dir(&w)],
            CliOpts {
                env: relaunch_env(&w),
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 1, "{r}");
        let start = format!(" start {}", shlex_quote(s(&resolve(&w.deck_path))));
        assert!(names_bin_before(&r.stderr_text(), &start), "{r}");
    }
}

mod settings {
    use super::*;

    #[test]
    fn test_notify_and_capacity_are_warned_as_not_supported() {
        let w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: "[notify]\ncommand = \"say done\"\n[capacity]\ncodex_pause_percent = 80\n",
                ..Options::default()
            },
        );
        let r = w.cli(&os_args!["check", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 0, "{r}");
        assert!(
            (r.stdout_text() + &r.stderr_text()).contains("notify.command"),
            "{r}"
        );
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 0, "{r}"); // warned, not refused
        let err = r.stderr_text();
        assert!(
            err.contains("not supported in this version: notify.command = say done"),
            "{r}"
        );
        assert!(
            err.contains("not supported in this version: capacity.codex_pause_percent = 80"),
            "{r}"
        );
    }

    #[test]
    fn test_defaults_are_not_warned() {
        let w = FakeSupervisor::new(&[card(1)]);
        let r = w.cli(&os_args!["check", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 0, "{r}");
        assert!(
            !(r.stdout_text() + &r.stderr_text()).contains("not supported"),
            "{r}"
        );
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 0, "{r}");
        assert!(r.stdout_text().contains("run: "), "{r}");
        assert!(
            !(r.stdout_text() + &r.stderr_text()).contains("not supported"),
            "{r}"
        );
    }
}

mod start {
    use super::*;

    #[test]
    fn test_start_skips_an_old_run_whose_event_log_is_unreadable() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        let old = w.start();
        OpenOptions::new()
            .append(true)
            .open(old.join("events.jsonl"))
            .and_then(|mut f| f.write_all(b"{not json\n"))
            .unwrap();
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 0, "{r}");
        assert!(r.stderr_text().contains("warning"), "{r}");
        assert!(r.stderr_text().contains(s(&old)), "{r}");
        assert!(r.stdout_text().contains("run:"), "{r}");
    }
}

mod copy_files {
    use super::*;

    #[test]
    fn test_a_symlinked_report_is_not_followed() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        let secret = w.tmp.join("secret.txt");
        fs::write(&secret, "Status: READY\nnot the worker's\n").unwrap();
        let report = os_path(&a["report"]);
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&secret, &report).unwrap();
        let reply = os_path(&a["reply_file"]);
        fs::write(&reply, "no status here").unwrap();
        let did = text(&a["dispatch_id"]);
        let ev = w
            .cli(&os_args![
                "record",
                run_dir(&w),
                "result",
                "--dispatch",
                did,
                "--worker",
                worker,
                "--reply-file",
                reply
            ])
            .ok()
            .json();
        let folder = run_dir(&w).join("cards/1").join(did);
        assert!(!folder.join("report.md").exists());
        assert!(!json_in("report", &ev["files"]), "{ev}");
        let skipped = ev
            .get("files_skipped")
            .filter(|v| truthy(v))
            .map(strs)
            .unwrap_or_default();
        assert!(
            skipped
                .iter()
                .any(|n| n.contains("symlink") || n.contains("regular file")),
            "{ev}"
        );
        assert_eq!(ev["status"], "BLOCKED");
        assert_eq!(read_text(&secret), "Status: READY\nnot the worker's\n");
    }
}

/// git, except that every `git commit` takes 2.5 s (in this scenario only the runner
/// commits: the fake Codex leaves its change uncommitted); after the wait, the worker
/// folders' heartbeats are looked at. Each commit's record is a folder, named on its own line
/// of $SLOW_GIT_LOG once the wait is over: its file `began` is made before the wait, and
/// `beats/<worker>` gets the times of `<worker>/heartbeat` after it (`touch -r`), so the test
/// compares the two as file times (the Python logged `time.time()` and each heartbeat's
/// `st_mtime`).
const SLOW_GIT: &str = r#"#!/bin/sh
for arg in "$@"; do
    if [ "$arg" = commit ]; then
        entry=$(mktemp -d "$SLOW_GIT_LOG.XXXXXX") || exit 70
        : > "$entry/began"
        sleep 2.5
        mkdir "$entry/beats"
        for beat in "$SLOW_GIT_WORKERS"/*/heartbeat; do
            [ -e "$beat" ] || continue
            worker=${beat%/heartbeat}
            touch -r "$beat" "$entry/beats/${worker##*/}"
        done
        printf '%s\n' "$entry" >> "$SLOW_GIT_LOG"
        break
    fi
done
exec "$SLOW_GIT_REAL" "$@"
"#;

mod runner_finalize {
    use super::*;

    /// One card routed to Codex, whose fake Codex leaves its change uncommitted.
    #[track_caller]
    fn uncommitted_codex_world() -> FakeSupervisor {
        let mut w = codex_world(
            &[card_with(
                1,
                CardOpts {
                    route: Some(CODEX_CARD),
                    ..CardOpts::default()
                },
            )],
            Options::default(),
        );
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                commit: Some(false),
                ..Step::default()
            }],
        );
        w
    }

    /// The result_recorded event of the dispatch action `a`.
    #[track_caller]
    fn result_of(w: &FakeSupervisor, a: &Json) -> Json {
        one(w
            .events("result_recorded")
            .into_iter()
            .filter(|e| e["dispatch_id"] == a["dispatch_id"])
            .collect())
    }

    #[test]
    fn test_heartbeat_is_touched_while_finalizing() {
        // A Codex worker leaves its change uncommitted; the runner's own commit of it is slow.
        let mut w = uncommitted_codex_world();
        let shim = w.tmp.join("slow git");
        fs::create_dir(&shim).unwrap();
        write_script(&shim.join("git"), SLOW_GIT);
        let log = w.tmp.join("slow git.log");
        let path = path_before(&w, &shim);
        let real = which("git").expect("git on PATH");
        let workers = run_dir(&w).join("workers");
        w.env
            .set("PATH", path)
            .set("SLOW_GIT_REAL", real)
            .set("SLOW_GIT_LOG", &log)
            .set("SLOW_GIT_WORKERS", workers);
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["kind", "agent"]),
            jv(r#"["implement", "leadhand:codex"]"#)
        );
        w.act(&a);
        let res = result_of(&w, &a);
        assert!(res.get("committed").is_some_and(truthy), "{res}");
        let commits = lines(&read_text(&log));
        assert!(!commits.is_empty(), "no `git commit` ran");
        // the run's last commit: the runner's, of Codex's change
        let slow = PathBuf::from(commits.last().unwrap());
        let did = text(&a["dispatch_id"]);
        let beat = slow.join("beats").join(did);
        assert!(beat.exists(), "no heartbeat of {did} in {}", slow.display());
        // touched while the commit ran
        assert!(mtime(&beat) > mtime(&slow.join("began")));
    }

    #[test]
    fn test_non_utf8_git_output_does_not_kill_the_commit() {
        // The card branch's HEAD is a commit whose message `git log` prints as bytes that are
        // not UTF-8 (e9 and ff, under an encoding iconv does not know, so Git cannot re-encode
        // it). With a commit intent already in the worker folder (planted, as the Python test
        // did), the runner reads that message back with `git log -1 --format=%B HEAD` before
        // it commits what the Codex worker left uncommitted: it must still commit.
        let mut w = uncommitted_codex_world();
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["kind", "agent"]),
            jv(r#"["implement", "leadhand:codex"]"#)
        );
        let ws = w.workspace();
        let branch = format!("{RUN}--card-1");
        assert_eq!(run_git(&ws, &["symbolic-ref", "--short", "HEAD"]), branch);
        let msg = w.tmp.join("non-utf8 message");
        fs::write(&msg, b"caf\xe9 \xff\n").unwrap();
        run_git(
            &ws,
            &[
                "-c",
                "i18n.commitEncoding=x-not-an-encoding",
                "commit",
                "--quiet",
                "--allow-empty",
                "-F",
                s(&msg),
            ],
        );
        let odd = run_git(&ws, &["rev-parse", "HEAD"]);
        // subprocess.run(..., env=w.env, check=True)
        let shown = run_argv(
            &os_args!["git", "log", "-1", "--format=%B", odd],
            &w.env,
            &ws,
            None,
            TIMEOUT,
        );
        if shown.code != 0 {
            panic!("git log failed\n{shown}");
        }
        // what the runner reads back is not UTF-8
        assert!(
            shown
                .stdout
                .windows(b"caf\xe9 \xff".len())
                .any(|win| win == b"caf\xe9 \xff"),
            "{shown}"
        );
        let wd = run_dir(&w).join("workers").join(text(&a["dispatch_id"]));
        fs::create_dir_all(&wd).unwrap();
        let intent = format!("{{\"head_before\": \"{}\"}}", "0".repeat(40));
        fs::write(wd.join("commit.json"), intent).unwrap();
        w.act(&a);
        let res = result_of(&w, &a);
        assert!(res.get("committed").is_some_and(truthy), "{res}");
        // judged from Git: the runner's commit is on the card branch, on top of the odd one,
        // with the work the Codex worker left uncommitted
        let committed = text(&res["committed"]);
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("refs/heads/{branch}")]),
            committed
        );
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{committed}~1")]),
            odd
        );
        let did = text(&a["dispatch_id"]);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{branch}:src/card1/{did}.txt")]),
            did
        );
    }
}
