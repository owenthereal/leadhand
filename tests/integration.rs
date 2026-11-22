//! Integration and violations: how a card's work is rebased onto the run tip and
//! fast-forwarded, what a conflict does, unparking, and how a worker that changes the
//! workspace or moves a ref it must leave alone is caught, preserved and re-run, as `start`,
//! `next`, `record` and `unpark` show them.
//!
//! The Rust form of the Python contract suite's `contract_integration` (converted from
//! Python's `test_engine_integration`). `FakeSupervisor` plays the run skill; workers misbehave
//! through a step's `tamper`, which runs in the harness against the workspace. A refusal is
//! exit code 1 with `error: ...` on stderr.
//!
//! Integration outcomes and violations are judged from Git itself: branch tips, ancestry,
//! trees, the refs under refs/leadhand/<run id>/ and their contents, trailers, and the
//! workspace's checkout. The binary's events are asserted beside that, never as the only
//! evidence.
//!
//! `NOTE`, `parked_world` and `unpark`, which later files import, are in `common::engine`.

#[macro_use]
mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::engine::{
    CARD1, NOTE, RUN, codex_world, files, found, found_by, last, parked_world, parked_world_with,
    requested, run_id, seq, tampering, unpark, with_status, with_verdict,
};
use common::git::{self, commit, current_branch, git_ok, run_git};
use common::json::Json;
use common::rundir::append_event;
use common::supervisor::{
    CliOpts, FakeSupervisor, Options, Step, card, os_path, run_dir, text, truthy,
};
use common::{field, index, jv, n_of, one, pick, read_text, shlex_split};

// --------------------------------------------------------------------------
// Helpers

/// The run branch, read from Git, is at card `n`'s accepted commit; gives that commit
/// (`EngineCase.assert_run_tip_is_accepted`).
macro_rules! assert_run_tip_is_accepted {
    ($w:expr, $n:expr) => {{
        let w: &FakeSupervisor = &$w;
        let sha = accepted_sha(w, $n);
        assert_eq!(w.branch_tip(RUN), sha);
        sha
    }};
}

/// Card `n`'s sha in `{e["card"]: e["sha"] for e in accepted}`: the last accepted event for
/// it; panics (Python's KeyError) when there is none.
#[track_caller]
fn accepted_sha(w: &FakeSupervisor, n: i64) -> String {
    let accepted = w.events("accepted");
    let e = accepted
        .iter()
        .rev()
        .find(|e| e["card"] == n)
        .unwrap_or_else(|| panic!("no accepted event for card {n}: {accepted:?}"));
    text(&e["sha"]).to_string()
}

/// Acts on every action until the dispatch of `kind` for the card; returns it unacted.
#[track_caller]
fn drive_to(w: &mut FakeSupervisor, kind: &str, card_number: i64) -> Json {
    for _ in 0..80 {
        let a = w.next();
        if a["action"] == "dispatch" && a["kind"] == kind && a["card"] == card_number {
            return a;
        }
        w.act(&a);
    }
    panic!("no {kind} dispatch for card {card_number}");
}

/// The worker that started `dispatch_id`.
#[track_caller]
fn worker_of(w: &FakeSupervisor, dispatch_id: &str) -> Json {
    w.events("worker_started")
        .into_iter()
        .find(|e| e["dispatch_id"] == dispatch_id)
        .map(|e| e["worker_id"].clone())
        .unwrap_or_else(|| panic!("no worker_started for {dispatch_id}"))
}

/// The violations of `stage` (`[e for e in violation if e.get("stage") == stage]`).
#[track_caller]
fn staged(w: &FakeSupervisor, stage: &str) -> Vec<Json> {
    w.events("violation")
        .into_iter()
        .filter(|e| e.get("stage").is_some_and(|s| s == stage))
        .collect()
}

/// The refs under refs/leadhand/<run id>/<kind>/, as Git lists them.
#[track_caller]
fn leadhand_refs(w: &FakeSupervisor, kind: &str) -> Vec<String> {
    let prefix = format!("refs/leadhand/{}/{kind}/", text(&w.meta()["run_id"]));
    run_git(&w.repo, &["for-each-ref", "--format=%(refname)", &prefix])
        .lines()
        .map(str::to_string)
        .collect()
}

#[track_caller]
fn on_branch(w: &FakeSupervisor, branch: &str, path: &str) -> bool {
    git_ok(
        &w.repo,
        &["cat-file", "-e", &format!("refs/heads/{branch}:{path}")],
    )
}

/// The cards of the `accepted` events, in order.
#[track_caller]
fn accepted_cards(w: &FakeSupervisor) -> Json {
    field(&w.events("accepted"), "card")
}

/// The text of the file a JSON string names (`Path(v).read_text()`).
#[track_caller]
fn brief(v: &Json) -> String {
    read_text(&os_path(v))
}

/// A list of `items`, as Python's tuple.
fn tuple(items: Vec<Json>) -> Json {
    Json::Arr(items)
}

/// The worker rewrites README.md, naming its dispatch.
fn rewrite_readme(_w: &FakeSupervisor, ws: &Path, a: &Json) {
    let line = format!("rewritten by {}\n", text(&a["dispatch_id"]));
    git::write(ws, &[("README.md", &line)]);
}

/// The worker moves the run branch to its own HEAD.
fn push_run(_w: &FakeSupervisor, ws: &Path, _a: &Json) {
    run_git(ws, &["update-ref", &format!("refs/heads/{RUN}"), "HEAD"]);
}

const SHARED_ONE: [(&str, &str); 1] = [("src/shared.txt", "card one\n")];
const SHARED_TWO: [(&str, &str); 1] = [("src/shared.txt", "card two\n")];

// --------------------------------------------------------------------------

mod integration {
    use super::*;

    #[test]
    fn test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded() {
        let mut w = parked_world();
        assert_json_eq!(accepted_cards(&w), jv("[2]"));
        let run_tip = w.branch_tip(RUN);
        let r = unpark(&w, 1, NOTE);
        assert!(r.stdout_text().contains("card 1 unparked"), "{r}");
        let event = one(w.events("unparked"));
        assert_json_eq!(
            pick(&event, &["kind", "card", "note"]),
            tuple(vec!["unparked".into(), 1.into(), NOTE.into()])
        );
        let actions = w.run();
        assert_eq!(actions.last().expect("an action")["action"], "done");

        assert_eq!(
            w.dispatch_ids()[3..],
            [
                "c1-a1-answer-1",
                "c1-a1-review-1",
                "c1-a1-rereview-1",
                "c1-a1-verify-1"
            ]
        );
        let [answer, _review, rereview, verify] = n_of(w.started[3..].to_vec());
        // the parked worker is resumed with the operator's note as the answer
        assert_json_eq!(
            answer["resume"],
            Json::object([("worker_id", worker_of(&w, "c1-a1-implement-1"))])
        );
        assert!(brief(&answer["brief"]).contains(&format!("> {NOTE}")));

        let intent = one(w.events("integration_started"));
        let rebased = one(w.events("rebased"));
        assert_empty!(w.events("conflict"));
        assert_eq!(intent["onto"], run_tip.as_str());
        assert!(seq(&requested(&w, "c1-a1-review-1")) < seq(&intent));
        assert!(seq(&intent) < seq(&rebased));
        assert!(seq(&rebased) < seq(&requested(&w, "c1-a1-rereview-1")));
        assert!(seq(&requested(&w, "c1-a1-rereview-1")) < seq(&requested(&w, "c1-a1-verify-1")));
        let accept = one(w
            .events("accept_started")
            .into_iter()
            .filter(|e| e["card"] == 1)
            .collect());
        assert!(seq(&requested(&w, "c1-a1-verify-1")) < seq(&accept));

        // the rereview covers the integrated diff; the verifier runs the deck's checks on the
        // card branch
        let tip = text(&rebased["tip"]);
        assert!(brief(&rereview["brief"]).contains(&format!("{run_tip}..{tip}")));
        assert!(rereview["resume"].is_null());
        assert_json_eq!(
            pick(&verify, &["role", "agent"]),
            jv(r#"["verifier", "leadhand:verifier"]"#)
        );
        let vbrief = brief(&verify["brief"]);
        assert!(vbrief.contains("- Test: true"), "{vbrief}");
        assert!(vbrief.contains(CARD1), "{vbrief}");

        assert_eq!(accepted_sha(&w, 1), tip);
        assert_eq!(w.branch_tip(RUN), tip);
        assert!(git_ok(
            &w.repo,
            &["merge-base", "--is-ancestor", &run_tip, tip]
        ));
        for path in [
            "src/card1/c1-a1-implement-1.txt",
            "src/card1/c1-a1-answer-1.txt",
            "src/card2/c2-a1-implement-1.txt",
        ] {
            run_git(&w.repo, &["cat-file", "-e", &format!("{RUN}:{path}")]);
        }
        assert_empty!(w.events("violation"));
        assert_empty!(leadhand_refs(&w, "violations"));
    }

    #[test]
    fn test_integration_conflict_requests_integrate_attempt() {
        let mut w = parked_world_with(&SHARED_ONE, &SHARED_TWO, &["claude"]);
        let run_tip = w.branch_tip(RUN);
        unpark(&w, 1, NOTE);
        let integrate = drive_to(&mut w, "integrate", 1);

        let intent = one(w.events("integration_started"));
        let conflict = one(w.events("conflict"));
        assert_json_eq!(conflict["paths"], jv(r#"["src/shared.txt"]"#));
        assert_empty!(w.events("rebased"));
        // the engine's own rebase was aborted: the card branch is as it was, and checked out
        assert_eq!(w.branch_tip(CARD1), text(&intent["tip"]));
        let ws = w.workspace();
        assert_eq!(current_branch(&ws).as_deref(), Some(CARD1));
        assert!(run_git(&ws, &["status", "--porcelain", "--untracked-files=no"]).is_empty());
        assert!(!git_ok(
            &ws,
            &["merge-base", "--is-ancestor", &run_tip, CARD1]
        ));
        // a new attempt on the route of the card's last implementer attempt, not escalated
        let answer = requested(&w, "c1-a1-answer-1");
        assert_json_eq!(
            pick(&integrate, &["dispatch_id", "attempt", "role"]),
            tuple(vec![
                "c1-a2-integrate-1".into(),
                2.into(),
                answer["role"].clone()
            ])
        );
        assert_json_eq!(
            integrate["route"],
            Json::object([
                ("vendor", answer["vendor"].clone()),
                ("model", answer["model"].clone()),
                ("effort", answer["effort"].clone()),
            ])
        );
        assert!(integrate["resume"].is_null());
        let text_ = brief(&integrate["brief"]);
        assert!(text_.contains(&run_tip), "{text_}");
        assert!(text_.contains("src/shared.txt"), "{text_}");

        w.act(&integrate);
        w.run();
        assert_eq!(
            last(&w.dispatch_ids(), 3),
            ["c1-a2-integrate-1", "c1-a2-rereview-1", "c1-a2-verify-1"]
        );
        let rebased = one(w.events("rebased"));
        assert_eq!(rebased["dispatch_id"], "c1-a2-integrate-1");
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        let tip = text(&rebased["tip"]);
        assert_eq!(w.branch_tip(RUN), tip);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card one"
        );
        assert!(git_ok(
            &w.repo,
            &["merge-base", "--is-ancestor", &run_tip, tip]
        ));
    }

    /// Drives the unparked card 1 to its review, then crashes `next` at `point`.
    #[track_caller]
    fn crash_at_rebase(point: &str) -> (FakeSupervisor, Json) {
        let mut w = parked_world();
        unpark(&w, 1, NOTE);
        let review = drive_to(&mut w, "review", 1);
        w.act(&review);
        let r = w.cli_with(
            &os_args!["next", run_dir(&w)],
            CliOpts {
                crash: Some(point),
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 97, "{r}");
        let intent = one(w.events("integration_started"));
        assert_empty!(w.events("rebased"));
        (w, intent)
    }

    #[test]
    fn test_crash_after_rebase_before_event_reconciles() {
        let (mut w, intent) = crash_at_rebase("after_rebase");
        let tip = w.branch_tip(CARD1);
        assert_ne!(tip, text(&intent["tip"]));
        assert!(git_ok(
            &w.repo,
            &["merge-base", "--is-ancestor", text(&intent["onto"]), &tip]
        ));
        w.run();
        // a card branch that already contains the run tip counts as rebased: no second rebase
        let rebased = one(w.events("rebased"));
        assert_eq!(rebased["tip"], tip.as_str());
        assert_eq!(w.events("integration_started").len(), 1);
        assert_empty!(w.events("conflict"));
        assert_eq!(accepted_sha(&w, 1), tip);
        assert_eq!(w.branch_tip(RUN), tip);
    }

    #[test]
    fn test_crash_before_rebase_rebases_on_restart() {
        let (mut w, intent) = crash_at_rebase("before_rebase");
        assert_eq!(w.branch_tip(CARD1), text(&intent["tip"]));
        w.run();
        let rebased = one(w.events("rebased"));
        assert!(git_ok(
            &w.repo,
            &[
                "merge-base",
                "--is-ancestor",
                text(&intent["onto"]),
                text(&rebased["tip"])
            ]
        ));
        assert_eq!(w.branch_tip(RUN), text(&rebased["tip"]));
    }

    #[test]
    fn test_failed_verification_escalates() {
        let mut w = parked_world();
        unpark(&w, 1, NOTE);
        w.script(
            1,
            "verify",
            [Step {
                result: Some("fail".into()),
                ..Step::default()
            }],
        );
        w.run();
        let ids = w.dispatch_ids();
        assert!(ids.iter().any(|i| i == "c1-a1-verify-1"), "{ids:?}");
        assert!(ids.iter().any(|i| i == "c1-a2-implement-1"), "{ids:?}");
        // the escalated attempt's tip is verified again before it is accepted
        assert_eq!(ids.last().expect("a dispatch"), "c1-a2-verify-1");
        let verify = requested(&w, "c1-a2-verify-1");
        assert_eq!(verify["extra"]["tip"], accepted_sha(&w, 1).as_str());
        assert_run_tip_is_accepted!(w, 1);
        assert!(on_branch(&w, RUN, "src/card1/c1-a2-implement-1.txt"));
    }
}

mod unpark {
    use super::*;

    #[test]
    fn test_unpark_without_a_known_worker_starts_a_fresh_attempt() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let first = w.next();
        assert_eq!(first["dispatch_id"], "c1-a1-implement-1");
        w.cli(&os_args![
            "record",
            run_dir(&w),
            "park",
            "--card",
            "1",
            "--reason",
            "operator wants to look first"
        ])
        .ok();
        unpark(&w, 1, NOTE);
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["action", "kind", "dispatch_id", "resume"]),
            jv(r#"["dispatch", "implement", "c1-a1-implement-2", null]"#)
        );
        assert!(brief(&a["brief"]).contains(NOTE));
        w.act(&a);
        w.run();
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        assert_run_tip_is_accepted!(w, 1);
        assert!(on_branch(&w, RUN, "src/card1/c1-a1-implement-2.txt"));
    }

    /// Covers limit M6 (run forgery): `unpark` has no caller check; any process that can run
    /// the CLI can unpark a card of the run.
    #[test]
    fn test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.next();
        let rd = run_dir(&w);
        let r = w.cli(&os_args!["unpark", rd, "--card", "1", "--note", NOTE]);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        assert!(r.stderr_text().contains("not parked"), "{r}");
        assert_empty!(w.events("unparked"));
        w.cli(&os_args![
            "record", rd, "park", "--card", "1", "--reason", "hold"
        ])
        .ok();
        let r = w.cli(&os_args!["unpark", rd, "--card", "1", "--note", NOTE]);
        assert_eq!(r.code, 0, "{r}");
        let event = one(w.events("unparked"));
        assert_json_eq!(
            pick(&event, &["card", "note"]),
            tuple(vec![1.into(), NOTE.into()])
        );
        for note in [NOTE, "  "] {
            // no longer parked; an empty note
            let r = w.cli(&os_args!["unpark", rd, "--card", "1", "--note", note]);
            assert_eq!(r.code, 1, "{r}");
            assert!(r.stderr_text().starts_with("error: "), "{r}");
        }
        assert_eq!(w.events("unparked").len(), 1);
    }
}

mod violations {
    use super::*;

    /// One card, whose first `kind` dispatch takes `first` and the next a plain step.
    #[track_caller]
    fn world(kind: &str, first: Step) -> FakeSupervisor {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, kind, [first, Step::default()]);
        w
    }

    /// The voided dispatch was re-run on the same route by a fresh worker, and the workspace
    /// is back on the card branch, clean.
    #[track_caller]
    fn check_rerun(w: &FakeSupervisor, kind: &str, voided: &str, rerun: &str) {
        let ids = w.dispatch_ids();
        assert!(ids.iter().any(|i| i == voided), "{ids:?}");
        assert!(ids.iter().any(|i| i == rerun), "{ids:?}");
        let (old, new) = (requested(w, voided), requested(w, rerun));
        assert_json_eq!(
            pick(
                &new,
                &["dispatch_kind", "vendor", "model", "effort", "role"]
            ),
            tuple(vec![
                kind.into(),
                old["vendor"].clone(),
                old["model"].clone(),
                old["effort"].clone(),
                old["role"].clone()
            ])
        );
        assert!(new["resume_worker"].is_null(), "{new}");
        let ws = w.workspace();
        assert_eq!(current_branch(&ws).as_deref(), Some(CARD1));
        assert_eq!(
            run_git(&ws, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
    }

    #[test]
    fn test_reviewer_tracked_change_voids_review_and_reruns() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "changed by the reviewer\n")]);
        };
        let mut w = world(
            "review",
            Step {
                verdict: Some("defects".into()),
                ..tampering(tamper)
            },
        );
        w.run();
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"]
        );
        let v = one(found(&w));
        let run_id = run_id(&w);
        let violation_ref = format!("refs/leadhand/{run_id}/violations/1");
        assert_json_eq!(
            pick(&v, &["dispatch_id", "paths", "ref"]),
            tuple(vec![
                "c1-a1-review-1".into(),
                jv(r#"["README.md"]"#),
                violation_ref.as_str().into()
            ])
        );
        assert_eq!(leadhand_refs(&w, "violations"), [violation_ref.as_str()]);
        // the voided defects verdict never counted: no fix round, the re-run's verdict decided
        assert_empty!(w.events("fix_round"));
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-2"]"#)
        );
        check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{}:README.md", text(&v["ref"]))]
            ),
            "changed by the reviewer"
        );
        assert_eq!(read_text(&w.workspace().join("README.md")), "hello\n");
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        assert_run_tip_is_accepted!(w, 1);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }

    #[test]
    fn test_reviewer_moving_head_is_violation() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            commit(
                ws,
                &[("src/reviewer.txt", "by the reviewer\n")],
                "reviewer commit",
            );
        };
        let mut w = world("review", tampering(tamper));
        w.run();
        let v = one(found(&w));
        assert!(truthy(&v["head_moved"]), "{v}");
        check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
        let impl_tip = w
            .events("result_recorded")
            .into_iter()
            .find(|e| e["dispatch_id"] == "c1-a1-implement-1")
            .map(|e| text(&e["tip"]).to_string())
            .expect("the implementer's result");
        assert_eq!(
            requested(&w, "c1-a1-review-1")["expected_head"],
            impl_tip.as_str()
        );
        assert_eq!(w.events("accepted")[0]["sha"], impl_tip.as_str());
        assert_eq!(w.branch_tip(RUN), impl_tip);
        assert!(!on_branch(&w, RUN, "src/reviewer.txt"));
        // the reviewer's commit is preserved, not lost
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{}:src/reviewer.txt", text(&v["ref"]))]
            ),
            "by the reviewer"
        );
        let card_ref = format!("refs/heads/{CARD1}");
        let moved = &v["refs"][card_ref.as_str()];
        assert_eq!(moved["old"], impl_tip.as_str());
        let keep = text(&v["preserved"][card_ref.as_str()]);
        assert_eq!(run_git(&w.repo, &["rev-parse", keep]), text(&moved["new"]));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{keep}:src/reviewer.txt")]),
            "by the reviewer"
        );
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{keep}^")]),
            impl_tip
        );
    }

    #[test]
    fn test_reviewer_hiding_an_edit_behind_an_index_flag_is_violation() {
        // Codex 4554880076: assume-unchanged or skip-worktree hides an edit from status and
        // diff.
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            let tamper = move |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
                run_git(ws, &["update-index", flag, "README.md"]);
                git::write(ws, &[("README.md", "edited by the reviewer, hidden\n")]);
            };
            let mut w = world("review", tampering(tamper));
            w.run();
            let v = one(found(&w));
            assert_eq!(v["dispatch_id"], "c1-a1-review-1", "flag={flag}");
            assert!(
                common::json_in("README.md", &v["paths"]),
                "flag={flag}: {v}"
            );
            assert_json_eq!(v["hidden"], jv(r#"["README.md"]"#), "flag={flag}");
            check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
            let ws = w.workspace();
            assert_eq!(
                run_git(&ws, &["ls-files", "-v", "--", "README.md"]),
                "H README.md",
                "flag={flag}"
            );
            assert_eq!(read_text(&ws.join("README.md")), "hello\n", "flag={flag}");
            assert_eq!(
                run_git(
                    &w.repo,
                    &["show", &format!("{}:README.md", text(&v["ref"]))]
                ),
                "edited by the reviewer, hidden",
                "flag={flag}"
            );
        }
    }

    #[test]
    fn test_a_partial_commit_takes_an_edit_hidden_behind_an_index_flag() {
        // The Python test called engine._commit_partial; its outside effect is the partial
        // commit a cancel makes of a stopped Claude implementer's work (R42).
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.session("supervisor-1");
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        let ws = w.workspace();
        run_git(&ws, &["update-index", "--assume-unchanged", "README.md"]);
        git::write(&ws, &[("README.md", "a stopped worker's hidden edit\n")]);
        let rd = run_dir(&w);
        w.cli(&os_args!["cancel", rd]).ok();
        assert_eq!(w.next()["action"], "stop_worker");
        let did = text(&a["dispatch_id"]);
        let event = w
            .cli(&os_args![
                "record",
                rd,
                "result",
                "--dispatch",
                did,
                "--worker",
                worker,
                "--reply",
                "Status: SPENT (time limit)",
                "--stopped"
            ])
            .ok()
            .json();
        assert!(event.get("partial_commit").is_some(), "{event}");
        assert_eq!(
            run_git(&ws, &["show", "HEAD:README.md"]),
            "a stopped worker's hidden edit"
        );
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("refs/heads/{CARD1}")]),
            text(&event["partial_commit"])
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{CARD1}:README.md")]),
            "a stopped worker's hidden edit"
        );
        let trailer = run_git(
            &w.repo,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Leadhand-Dispatch,valueonly)",
                CARD1,
            ],
        );
        assert_eq!(trailer, did);
    }

    #[test]
    fn test_verifier_staged_change_is_violation() {
        let mut w = parked_world();
        unpark(&w, 1, NOTE);
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(
                ws,
                &[(
                    "src/card1/c1-a1-implement-1.txt",
                    "staged by the verifier\n",
                )],
            );
            run_git(ws, &["add", "--", "src/card1/c1-a1-implement-1.txt"]);
        };
        w.script(1, "verify", [tampering(tamper), Step::default()]);
        w.run();
        let v = one(found(&w));
        assert_eq!(v["dispatch_id"], "c1-a1-verify-1");
        assert!(truthy(&v["index_dirty"]), "{v}");
        assert!(!truthy(&v["head_moved"]), "{v}");
        check_rerun(&w, "verify", "c1-a1-verify-1", "c1-a1-verify-2");
        assert_json_eq!(
            requested(&w, "c1-a1-verify-2")["extra"],
            requested(&w, "c1-a1-verify-1")["extra"]
        );
        assert_eq!(
            w.dispatch_ids().last().expect("a dispatch"),
            "c1-a1-verify-2"
        );
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{RUN}:src/card1/c1-a1-implement-1.txt")]
            ),
            "c1-a1-implement-1"
        );
        assert_eq!(
            run_git(
                &w.repo,
                &[
                    "show",
                    &format!("{}^2:src/card1/c1-a1-implement-1.txt", text(&v["ref"]))
                ]
            ),
            "staged by the verifier"
        );
    }

    #[test]
    fn test_violation_preserves_index_and_worktree_on_ref() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "staged\n")]);
            run_git(ws, &["add", "--", "README.md"]);
            git::write(
                ws,
                &[
                    ("README.md", "unstaged\n"),
                    ("build/out.txt", "untracked output\n"),
                ],
            );
        };
        let mut w = world("review", tampering(tamper));
        w.run();
        let v = one(found(&w));
        assert!(truthy(&v["index_dirty"]), "{v}");
        assert_json_eq!(v["paths"], jv(r#"["README.md"]"#));
        let done = one(staged(&w, "restored"));
        assert_json_eq!(done["ref"], v["ref"]);
        let vref = text(&v["ref"]);
        assert_eq!(run_git(&w.repo, &["rev-parse", vref]), text(&done["saved"]));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{vref}:README.md")]),
            "unstaged"
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{vref}^2:README.md")]),
            "staged"
        );
        let ws = w.workspace();
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
        assert_eq!(read_text(&ws.join("build/out.txt")), "untracked output\n");
        check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
    }

    #[test]
    fn test_untracked_build_output_is_not_a_violation() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("build/out.txt", "output\n")]);
        };
        let mut w = world("review", tampering(tamper));
        w.run();
        assert_empty!(w.events("violation"));
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"]);
        assert_empty!(leadhand_refs(&w, "violations"));
        assert_eq!(read_text(&w.workspace().join("build/out.txt")), "output\n");
        assert!(!on_branch(&w, RUN, "build/out.txt"));
    }

    /// Covers R14.
    #[test]
    fn test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            let back = current_branch(ws).expect("a checked-out branch");
            run_git(ws, &["checkout", "--quiet", RUN]);
            commit(
                ws,
                &[("src/sneaky.txt", "on the run branch\n")],
                "reviewer commit on the run branch",
            );
            run_git(ws, &["checkout", "--quiet", &back]);
        };
        let mut w = world("review", tampering(tamper));
        let base = w.base.clone();
        w.run();
        let run_ref = format!("refs/heads/{RUN}");
        let req = requested(&w, "c1-a1-review-1");
        assert_eq!(req["refs"][run_ref.as_str()], base.as_str());
        let v = one(found(&w));
        assert!(!truthy(&v["head_moved"]), "{v}");
        assert!(v["ref"].is_null(), "{v}");
        let moved = &v["refs"][run_ref.as_str()];
        assert_eq!(moved["old"], base.as_str());
        let run_id = run_id(&w);
        let keep = text(&v["preserved"][run_ref.as_str()]);
        assert_eq!(
            keep,
            format!("refs/leadhand/{run_id}/violations/1-heads-leadhand-demo")
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{keep}:src/sneaky.txt")]),
            "on the run branch"
        );
        assert_eq!(leadhand_refs(&w, "violations"), [keep]);
        check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
        // the run branch was put back before acceptance, so it holds only the card's work
        let accepted = text(&w.events("accepted")[0]["sha"]).to_string();
        assert_eq!(w.branch_tip(RUN), accepted);
        assert!(!git_ok(
            &w.repo,
            &["cat-file", "-e", &format!("{RUN}:src/sneaky.txt")]
        ));
        assert_eq!(run_git(&w.repo, &["rev-parse", &format!("{RUN}^")]), base);
    }

    #[test]
    fn test_reviewer_creating_and_deleting_refs_is_violation() {
        // R14': under refs/leadhand/ only this run's namespace is watched.
        let tamper = |world: &FakeSupervisor, ws: &Path, _a: &Json| {
            let run_id = text(&world.meta()["run_id"]).to_string();
            run_git(ws, &["branch", "leadhand/demo--card-7"]);
            run_git(
                ws,
                &[
                    "update-ref",
                    &format!("refs/leadhand/{run_id}/elsewhere/note"),
                    "HEAD",
                ],
            );
            run_git(ws, &["update-ref", "refs/leadhand/other-run/note", "HEAD"]);
            run_git(ws, &["update-ref", "-d", &format!("refs/heads/{RUN}")]);
        };
        let mut w = world("review", tampering(tamper));
        w.run();
        let run_id = run_id(&w);
        let note = format!("refs/leadhand/{run_id}/elsewhere/note");
        let run_ref = format!("refs/heads/{RUN}");
        let v = one(found(&w));
        let refs: BTreeSet<String> = v["refs"].keys().into_iter().collect();
        let want: BTreeSet<String> = [
            "refs/heads/leadhand/demo--card-7".to_string(),
            note.clone(),
            run_ref.clone(),
        ]
        .into();
        assert_eq!(refs, want);
        assert!(v["refs"][run_ref.as_str()]["new"].is_null(), "{v}");
        assert!(v["refs"][note.as_str()]["old"].is_null(), "{v}");
        for r in ["refs/heads/leadhand/demo--card-7", note.as_str()] {
            assert!(
                !git_ok(&w.repo, &["show-ref", "--verify", "--quiet", r]),
                "{r}"
            );
        }
        // another run's ref is not this run's business: left alone
        assert!(git_ok(
            &w.repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/leadhand/other-run/note"
            ]
        ));
        check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        // the deleted run branch was put back, and the card fast-forwarded onto it
        assert_run_tip_is_accepted!(w, 1);
        assert_eq!(run_git(&w.repo, &["rev-parse", &format!("{RUN}^")]), w.base);
    }

    #[test]
    fn test_violation_interrupted_before_restore_still_voids_on_restart() {
        // The Python test made engine.preserve_and_restore raise. Here a ref under the
        // violation ref's name (made before the run dispatches anything, so no worker moved
        // it) blocks Git from creating that ref: the restore fails part way, after `found` is
        // recorded.
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "changed by the reviewer\n")]);
        };
        let mut w = world(
            "review",
            Step {
                verdict: Some("defects".into()),
                ..tampering(tamper)
            },
        );
        let run_id = run_id(&w);
        let vref = format!("refs/leadhand/{run_id}/violations/1");
        let blocker = format!("{vref}/blocker");
        run_git(&w.repo, &["update-ref", &blocker, &w.base]);
        let review = drive_to(&mut w, "review", 1);
        w.act(&review);
        let r = w.cli(&os_args!["next", run_dir(&w)]);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        let v = one(found(&w));
        assert_eq!(v["ref"], vref.as_str());
        assert_empty!(staged(&w, "restored"));
        // nothing was restored: the reviewer's edit is still in the workspace, and no ref
        // saved it
        assert_eq!(
            read_text(&w.workspace().join("README.md")),
            "changed by the reviewer\n"
        );
        assert!(!git_ok(
            &w.repo,
            &["show-ref", "--verify", "--quiet", &vref]
        ));
        run_git(&w.repo, &["update-ref", "-d", &blocker]);
        w.run();
        assert_eq!(found(&w).len(), 1);
        let done = one(staged(&w, "restored"));
        assert_eq!(done["ref"], vref.as_str());
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{}:README.md", text(&done["ref"]))]
            ),
            "changed by the reviewer"
        );
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-2"]"#)
        );
        check_rerun(&w, "review", "c1-a1-review-1", "c1-a1-review-2");
        assert_eq!(read_text(&w.workspace().join("README.md")), "hello\n");
    }

    #[test]
    fn test_voided_reviews_do_not_spend_the_retry_budget() {
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "changed\n")]);
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "review",
            [
                Step {
                    status: Some("SPENT".into()),
                    ..tampering(tamper)
                },
                Step {
                    status: Some("SPENT".into()),
                    ..tampering(tamper)
                },
                with_status("SPENT"),
                Step::default(),
            ],
        );
        w.run();
        assert_eq!(found(&w).len(), 2);
        assert_empty!(w.events("parked"));
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        let run_id = run_id(&w);
        assert_eq!(
            leadhand_refs(&w, "violations"),
            [
                format!("refs/leadhand/{run_id}/violations/1"),
                format!("refs/leadhand/{run_id}/violations/2")
            ]
        );
        for r in leadhand_refs(&w, "violations") {
            assert_eq!(
                run_git(&w.repo, &["show", &format!("{r}:README.md")]),
                "changed"
            );
        }
        assert_run_tip_is_accepted!(w, 1);
        assert!(on_branch(&w, RUN, "src/card1/c1-a1-implement-1.txt"));
    }
}

mod one_card_in_flight {
    use super::*;

    /// Covers R31 (one card in flight).
    #[test]
    fn test_unpark_while_another_card_is_in_flight_waits() {
        // Review repro: unparking card 1 while card 2's review is requested must not
        // interleave.
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks.insert(1, "product decision".to_string());
        let review = drive_to(&mut w, "review", 2);
        w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
        unpark(&w, 1, "go");
        assert_json_eq!(w.next()["dispatch_id"], review["dispatch_id"]);
        w.run();
        let ids = w.dispatch_ids();
        let accepted = w.events("accepted");
        assert_json_eq!(field(&accepted, "card"), jv("[2, 1]"));
        // nothing for card 1 was dispatched until card 2 was accepted
        let accept2 = seq(&accepted[0]);
        let later: Vec<Json> = w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["card"] == 1 && e["dispatch_id"] != "c1-a1-implement-1")
            .collect();
        assert!(!later.is_empty());
        let later_ids = field(&later, "dispatch_id"); // Python's message, built eagerly
        assert!(later.iter().all(|e| seq(e) > accept2), "{later_ids}");
        let not_implementer: Vec<Json> = w
            .events("violation")
            .into_iter()
            .filter(|e| e.get("by").is_none_or(|b| b != "implementer"))
            .collect();
        assert_empty!(not_implementer);
        assert_empty!(leadhand_refs(&w, "violations"));
        let tip = w.branch_tip(RUN);
        assert_eq!(accepted[1]["sha"], tip.as_str());
        for path in [
            "src/card1/c1-a1-implement-1.txt",
            "src/card2/c2-a1-implement-1.txt",
        ] {
            run_git(&w.repo, &["cat-file", "-e", &format!("{RUN}:{path}")]);
        }
        assert!(ids.iter().any(|i| i == "c1-a1-verify-1"), "{ids:?}");
    }

    #[test]
    fn test_engine_ref_moves_after_dispatch_are_not_violations() {
        let engine_moves = |world: &FakeSupervisor, ws: &Path, _a: &Json| {
            // what the engine records while the review runs: card 2 accepted on the run branch
            let tree = run_git(ws, &["rev-parse", &format!("{}^{{tree}}", world.base)]);
            let other = run_git(
                ws,
                &["commit-tree", "-p", &world.base, "-m", "card 2", &tree],
            );
            run_git(
                ws,
                &[
                    "update-ref",
                    &format!("refs/heads/{RUN}"),
                    &other,
                    &world.base,
                ],
            );
            append_event(
                &run_dir(world),
                "accepted",
                [("card", Json::from(2)), ("sha", Json::from(other))],
            );
        };
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        w.start();
        w.script(1, "review", [tampering(engine_moves)]);
        w.run();
        assert_empty!(w.events("violation"));
        assert_eq!(
            w.dispatch_ids()[..2],
            ["c1-a1-implement-1", "c1-a1-review-1"]
        );
        assert!(!w.dispatch_ids().iter().any(|i| i == "c1-a1-review-2"));
        let cards: BTreeSet<i64> = w
            .events("accepted")
            .iter()
            .map(|e| e["card"].as_i64().expect("an integer card"))
            .collect();
        assert_eq!(cards, BTreeSet::from([1, 2]));
        assert_empty!(leadhand_refs(&w, "violations"));
    }
}

mod fix_round1 {
    use super::*;

    /// Card 1 parks on a question, card 2 is accepted, both writing src/shared.txt; card 1 is
    /// unparked.
    #[track_caller]
    fn conflicted() -> FakeSupervisor {
        let w = parked_world_with(&SHARED_ONE, &SHARED_TWO, &["claude"]);
        unpark(&w, 1, NOTE);
        w
    }

    /// Covers R31 (the violation cap).
    #[test]
    fn test_repeated_check_violations_park_the_card() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "review", (0..5).map(|_| tampering(rewrite_readme)));
        w.run();
        assert_eq!(found(&w).len(), 3);
        let reviews: Vec<String> = w
            .dispatch_ids()
            .into_iter()
            .filter(|i| i.contains("review"))
            .collect();
        assert_eq!(
            reviews,
            ["c1-a1-review-1", "c1-a1-review-2", "c1-a1-review-3"]
        );
        let parked = one(w.events("parked"));
        assert_eq!(parked["reason"], "checks change tracked files: README.md");
        assert_empty!(w.events("accepted"));
        assert_eq!(read_text(&w.workspace().join("README.md")), "hello\n");
        let run_id = run_id(&w);
        let refs = leadhand_refs(&w, "violations");
        let want: Vec<String> = (1..=3)
            .map(|n| format!("refs/leadhand/{run_id}/violations/{n}"))
            .collect();
        assert_eq!(refs, want);
        for (n, r) in refs.iter().enumerate() {
            assert_eq!(
                run_git(&w.repo, &["show", &format!("{r}:README.md")]),
                format!("rewritten by c1-a1-review-{}", n + 1)
            );
        }
        assert_eq!(w.branch_tip(RUN), w.base);
    }

    #[test]
    fn test_unpark_after_relaunch_starts_fresh_attempt_with_note() {
        let mut w = parked_world();
        w.session("supervisor-2"); // relaunched: the parked worker died with the old session
        unpark(&w, 1, NOTE);
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["kind", "dispatch_id", "resume"]),
            jv(r#"["implement", "c1-a1-implement-2", null]"#)
        );
        assert!(brief(&a["brief"]).contains(NOTE));
        let started = w
            .events("worker_started")
            .into_iter()
            .find(|e| e["dispatch_id"] == "c1-a1-implement-1")
            .expect("the first implementer's worker_started");
        assert_eq!(started["session"], "supervisor-1");
        w.act(&a);
        w.run();
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_run_tip_is_accepted!(w, 1);
        for path in [
            "src/card1/c1-a1-implement-2.txt",
            "src/card2/c2-a1-implement-1.txt",
        ] {
            assert!(on_branch(&w, RUN, path), "{path}");
        }
    }

    #[test]
    fn test_operation_left_in_progress_is_blocked_and_preserved() {
        let leave_merge = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            let back = current_branch(ws).expect("a checked-out branch");
            run_git(ws, &["checkout", "--quiet", "-b", "side"]);
            commit(ws, &[("README.md", "side\n")], "side change");
            run_git(ws, &["checkout", "--quiet", &back]);
            run_git(ws, &["merge", "--quiet", "--no-ff", "--no-commit", "side"]);
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [tampering(leave_merge), Step::default()]);
        w.run();
        assert_eq!(
            w.dispatch_ids()[..2],
            ["c1-a1-implement-1", "c1-a1-implement-2"]
        );
        let v = one(found_by(&w, "implementer"));
        let run_id = run_id(&w);
        assert_json_eq!(
            pick(&v, &["operation", "after_dispatch", "ref"]),
            tuple(vec![
                "merge".into(),
                "c1-a1-implement-1".into(),
                format!("refs/leadhand/{run_id}/leftovers/1").into()
            ])
        );
        let vref = text(&v["ref"]);
        assert_eq!(leadhand_refs(&w, "leftovers"), [vref]);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{vref}:README.md")]),
            "side"
        );
        let ws = w.workspace();
        assert!(!git_ok(
            &ws,
            &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]
        ));
        assert_eq!(current_branch(&ws).as_deref(), Some(CARD1));
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        assert_run_tip_is_accepted!(w, 1);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }

    #[test]
    fn test_implementer_leftovers_are_preserved_before_review_without_voiding() {
        let leave_dirty = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "uncommitted by the implementer\n")]);
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [tampering(leave_dirty)]);
        w.run();
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"]);
        let v = one(found_by(&w, "implementer"));
        assert_json_eq!(v["paths"], jv(r#"["README.md"]"#));
        assert!(v["operation"].is_null(), "{v}");
        assert_empty!(found(&w));
        assert_empty!(leadhand_refs(&w, "violations"));
        let vref = text(&v["ref"]);
        assert_eq!(leadhand_refs(&w, "leftovers"), [vref]);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{vref}:README.md")]),
            "uncommitted by the implementer"
        );
        assert!(seq(&v) < seq(&requested(&w, "c1-a1-review-1")));
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-1"]"#)
        );
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        assert_run_tip_is_accepted!(w, 1);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }

    #[test]
    fn test_integration_keeps_the_pre_integration_tip() {
        let mut w = conflicted();
        w.run();
        let intent = one(w.events("integration_started"));
        let run_id = run_id(&w);
        let keep = text(&intent["keep"]);
        assert_eq!(keep, format!("refs/leadhand/{run_id}/integration/1-1"));
        assert_eq!(run_git(&w.repo, &["rev-parse", keep]), text(&intent["tip"]));
        assert_eq!(leadhand_refs(&w, "integration"), [keep]);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{keep}:src/shared.txt")]),
            "card one"
        );
        assert!(!git_ok(
            &w.repo,
            &["merge-base", "--is-ancestor", text(&intent["onto"]), keep]
        ));
    }

    #[test]
    fn test_integrate_moving_run_branch_is_violation() {
        let mut w = conflicted();
        let onto = w.branch_tip(RUN);
        w.script(1, "integrate", [tampering(push_run), Step::default()]);
        w.run();
        let v = one(found(&w));
        assert_eq!(v["dispatch_id"], "c1-a2-integrate-1");
        let run_ref = format!("refs/heads/{RUN}");
        assert_eq!(v["refs"][run_ref.as_str()]["old"], onto.as_str());
        let keep = text(&v["preserved"][run_ref.as_str()]);
        assert_eq!(
            run_git(&w.repo, &["rev-parse", keep]),
            text(&v["refs"][run_ref.as_str()]["new"])
        );
        let ids = w.dispatch_ids();
        assert!(ids.iter().any(|i| i == "c1-a2-integrate-2"), "{ids:?}");
        let rebased = one(w.events("rebased"));
        assert_eq!(rebased["dispatch_id"], "c1-a2-integrate-2");
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_eq!(assert_run_tip_is_accepted!(w, 1), text(&rebased["tip"]));
        assert!(git_ok(
            &w.repo,
            &["merge-base", "--is-ancestor", &onto, RUN]
        ));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card one"
        );
    }

    #[test]
    fn test_integrate_dropping_card_work_escalates() {
        let reset_to_onto = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(
                ws,
                &["reset", "--quiet", "--hard", &format!("refs/heads/{RUN}")],
            );
        };
        let mut w = conflicted();
        w.script(
            1,
            "integrate",
            [Step {
                rebase: Some(false),
                ..tampering(reset_to_onto)
            }],
        );
        w.run();
        assert_empty!(w.events("rebased"));
        let ids = w.dispatch_ids();
        assert!(ids.iter().any(|i| i == "c1-a3-implement-1"), "{ids:?}");
        let text_ = brief(&requested(&w, "c1-a3-implement-1")["brief"]);
        let intent = one(w.events("integration_started"));
        assert!(text_.contains(text(&intent["keep"])), "{text_}");
        assert_eq!(
            run_git(&w.repo, &["rev-parse", text(&intent["keep"])]),
            text(&intent["tip"])
        );
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_run_tip_is_accepted!(w, 1);
        assert!(on_branch(&w, RUN, "src/card1/c1-a3-implement-1.txt"));
    }

    #[test]
    fn test_integrate_question_answer_goes_through_integration() {
        let mut w = conflicted();
        w.script(
            1,
            "integrate",
            [Step {
                rebase: Some(false),
                ..with_status("QUESTION")
            }],
        );
        // the unpark answer, then the integrate answer
        w.script(
            1,
            "answer",
            [
                Step::default(),
                Step {
                    rebase: Some(true),
                    ..Step::default()
                },
            ],
        );
        w.rulings
            .insert(1, vec!["Keep card one's version.".to_string()]);
        w.run();
        let ids = w.dispatch_ids();
        assert!(ids.iter().any(|i| i == "c1-a2-answer-1"), "{ids:?}");
        let rebased = one(w.events("rebased"));
        assert_eq!(rebased["dispatch_id"], "c1-a2-answer-1");
        assert_eq!(ids[index(&ids, "c1-a2-answer-1") + 1], "c1-a2-rereview-1");
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card one"
        );
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_eq!(assert_run_tip_is_accepted!(w, 1), text(&rebased["tip"]));
    }
}

mod fix_round2 {
    use super::*;

    #[test]
    fn test_voided_answer_to_integrate_keeps_its_resume_chain() {
        // Review scratch test: an answer resuming an integrate worker moves the run branch.
        let mut w = parked_world_with(&SHARED_ONE, &SHARED_TWO, &["claude"]);
        unpark(&w, 1, NOTE);
        let onto = w.branch_tip(RUN);
        w.script(
            1,
            "integrate",
            [Step {
                rebase: Some(false),
                ..with_status("QUESTION")
            }],
        );
        w.script(
            1,
            "answer",
            [
                Step::default(),
                Step {
                    rebase: Some(true),
                    ..tampering(push_run)
                },
                Step {
                    rebase: Some(true),
                    ..Step::default()
                },
            ],
        );
        w.rulings
            .insert(1, vec!["Keep card one's version.".to_string()]);
        w.run();
        let v = one(found(&w));
        assert_eq!(v["dispatch_id"], "c1-a2-answer-1");
        let run_ref = format!("refs/heads/{RUN}");
        assert_eq!(v["refs"][run_ref.as_str()]["old"], onto.as_str());
        assert_eq!(
            run_git(
                &w.repo,
                &["rev-parse", text(&v["preserved"][run_ref.as_str()])]
            ),
            text(&v["refs"][run_ref.as_str()]["new"])
        );
        let rerun = requested(&w, "c1-a2-answer-2");
        assert_eq!(rerun["resumes"], "c1-a2-integrate-1");
        assert_json_eq!(rerun["resume_worker"], worker_of(&w, "c1-a2-integrate-1"));
        assert_json_eq!(
            rerun["refs"],
            Json::object([(run_ref.as_str(), Json::from(onto.as_str()))])
        );
        let rebased = one(w.events("rebased"));
        assert_eq!(rebased["dispatch_id"], "c1-a2-answer-2");
        let ids = w.dispatch_ids();
        assert_eq!(ids[index(&ids, "c1-a2-answer-2") + 1], "c1-a2-rereview-1");
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card one"
        );
        assert_eq!(assert_run_tip_is_accepted!(w, 1), text(&rebased["tip"]));
    }

    /// Covers R32.
    #[test]
    fn test_unpark_after_violation_cap_reruns_the_check() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.session("supervisor-1");
        w.start();
        w.script(
            1,
            "review",
            (0..3)
                .map(|_| tampering(rewrite_readme))
                .chain([Step::default()]),
        );
        w.run();
        let parked = one(w.events("parked"));
        assert!(
            text(&parked["reason"]).starts_with("checks change tracked files"),
            "{parked}"
        );
        unpark(&w, 1, "The test command no longer rewrites README.md.");
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["action", "kind", "dispatch_id", "resume"]),
            jv(r#"["dispatch", "review", "c1-a1-review-4", null]"#)
        );
        let old = requested(&w, "c1-a1-review-3");
        assert_json_eq!(
            a["route"],
            Json::object([
                ("vendor", old["vendor"].clone()),
                ("model", old["model"].clone()),
                ("effort", old["effort"].clone()),
            ])
        );
        assert_json_eq!(a["role"], old["role"]);
        assert!(brief(&a["brief"]).contains("no longer rewrites README.md"));
        w.act(&a);
        w.run();
        let workers: Vec<String> = w
            .dispatch_ids()
            .into_iter()
            .filter(|i| i.contains("implement") || i.contains("answer"))
            .collect();
        assert_eq!(workers, ["c1-a1-implement-1"]);
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-4"]"#)
        );
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        let impl_tip = w
            .events("result_recorded")
            .into_iter()
            .find(|e| e["dispatch_id"] == "c1-a1-implement-1")
            .map(|e| text(&e["tip"]).to_string())
            .expect("the implementer's result");
        assert_eq!(assert_run_tip_is_accepted!(w, 1), impl_tip);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }

    #[test]
    fn test_voided_rereview_is_rerun_by_a_fresh_worker() {
        // R33: a voided check never resumes the reviewer whose result was voided.
        let tamper = |_w: &FakeSupervisor, ws: &Path, _a: &Json| {
            git::write(ws, &[("README.md", "changed by the reviewer\n")]);
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "review", [with_verdict("defects")]);
        // production: a rereview
        w.script(1, "fix", [files(&[("src/widget.py", "fixed = True\n")])]);
        w.script(1, "rereview", [tampering(tamper), Step::default()]);
        w.run();
        let first = requested(&w, "c1-a1-rereview-1");
        assert_eq!(first["resumes"], "c1-a1-review-1");
        assert_json_eq!(first["resume_worker"], worker_of(&w, "c1-a1-review-1"));
        let v = one(found(&w));
        assert_eq!(v["dispatch_id"], "c1-a1-rereview-1");
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{}:README.md", text(&v["ref"]))]
            ),
            "changed by the reviewer"
        );
        let rerun = requested(&w, "c1-a1-rereview-2");
        assert!(rerun["resumes"].is_null(), "{rerun}");
        assert!(rerun["resume_worker"].is_null(), "{rerun}");
        let started = &w.started[index(&w.dispatch_ids(), "c1-a1-rereview-2")];
        assert!(started["resume"].is_null(), "{started}");
        let route = ["vendor", "model", "effort", "role"];
        assert_json_eq!(pick(&rerun, &route), pick(&first, &route));
        assert_json_eq!(rerun["extra"], first["extra"]);
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-1", "c1-a1-rereview-2"]"#)
        );
        assert_json_eq!(accepted_cards(&w), jv("[1]"));
        assert_run_tip_is_accepted!(w, 1);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/widget.py")]),
            "fixed = True"
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }
}

/// Ruling R39: integrating rewrites the card branch, and Codex's sandbox refuses Git writes.
mod codex_integrate {
    use super::*;

    #[test]
    fn test_integrate_after_codex_implementer_routes_to_claude() {
        let mut w = parked_world_with(&SHARED_ONE, &SHARED_TWO, &["claude", "codex"]);
        let implement = requested(&w, "c1-a1-implement-1");
        assert_eq!(implement["vendor"], "codex");
        let message = text(&w.started[0]["message"]);
        let argv = shlex_split(message).unwrap_or_else(|| panic!("not shell words: {message}"));
        assert_eq!(argv[index(&argv, "--kind") + 1], "implement");
        assert_eq!(argv[index(&argv, "--branch") + 1], CARD1);
        unpark(&w, 1, NOTE);
        let integrate = drive_to(&mut w, "integrate", 1);
        assert_eq!(integrate["route"]["vendor"], "claude");
        assert_eq!(integrate["agent"], "leadhand:implementer");
        assert_json_eq!(integrate["role"], implement["role"]);
        w.act(&integrate);
        w.run();
        let rebased = one(w.events("rebased"));
        assert_eq!(rebased["dispatch_id"], "c1-a2-integrate-1");
        assert_json_eq!(accepted_cards(&w), jv("[2, 1]"));
        assert_eq!(assert_run_tip_is_accepted!(w, 1), text(&rebased["tip"]));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card one"
        );
    }

    #[test]
    fn test_failed_runner_commit_is_recorded_blocked_environment() {
        let mut w = codex_world(&[card(1)], Options::default());
        for name in [
            "EMAIL",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            w.env.remove(name);
        }
        run_git(&w.repo, &["config", "--unset", "user.name"]);
        run_git(&w.repo, &["config", "--unset", "user.email"]);
        run_git(&w.repo, &["config", "user.useConfigOnly", "true"]);
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                commit: Some(false),
                ..Step::default()
            }],
        );
        let a = w.next();
        assert_eq!(a["agent"], "leadhand:codex");
        w.act(&a);
        let recorded = one(w.events("result_recorded"));
        assert_json_eq!(
            pick(&recorded, &["status", "blocked_by"]),
            jv(r#"["BLOCKED", "environment"]"#)
        );
        assert!(recorded["committed"].is_null(), "{recorded}");
        assert!(truthy(&recorded["commit_error"]), "{recorded}");
        let status = run_git(
            &w.workspace(),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert!(status.contains("src/card1/"), "{status}");
        // nothing was committed: the card branch is still at the run's base
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("refs/heads/{CARD1}")]),
            w.base
        );
    }

    #[test]
    fn test_integrate_without_claude_parks_the_card() {
        let mut w = parked_world_with(&SHARED_ONE, &SHARED_TWO, &["codex"]);
        unpark(&w, 1, NOTE);
        w.run();
        let parked = w
            .events("parked")
            .into_iter()
            .rfind(|e| e["card"] == 1)
            .expect("a park of card 1");
        assert_eq!(parked["reason"], "integration needs a Claude worker");
        assert!(!w.dispatch_ids().iter().any(|i| i == "c1-a2-integrate-1"));
        // only card 2 reached the run branch
        assert_json_eq!(accepted_cards(&w), jv("[2]"));
        assert_run_tip_is_accepted!(w, 2);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:src/shared.txt")]),
            "card two"
        );
    }
}
