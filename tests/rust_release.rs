//! The branch release (deliberate change 2, spec "The branch release"). Rust-only.
//!
//! The Rust form of the Python contract suite's `contract_rust_release`. When a run ends complete,
//! incomplete or failed, leadhand detaches the run worktree at the run head, between the
//! `outcome` event and the report: `branch_release_started {worktree, branch, sha}`, the
//! detach, then `branch_released` (with `skipped` and a reason when there was nothing to
//! detach). The report's Result section then says what Git shows afterwards, not what
//! leadhand did: the branch is free only when `git worktree list --porcelain` shows no
//! worktree, prunable ones included, with the run branch checked out; otherwise it names the
//! holder and the command that frees it. The release never prunes worktree records. Blocked
//! runs keep the branch checked out: they resume there.
//!
//! What the release did is judged from Git: the worktree's HEAD, the worktree list, and
//! whether the operator can `git switch` to the branch in their own checkout. The events are
//! asserted beside that. `test_release_never_runs_worktree_prune` logs every Git command line
//! the binary runs through a `git` wrapper first on PATH (a shell script here; the Python's
//! was a Python script logging JSON lines).

#[macro_use]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::engine::{RUN, base_wt, gated_with, path_before, seq, to_last_step, write_script};
use common::git::{run_git, run_git_unchecked};
use common::json::{self, Json};
use common::rundir::append_event;
use common::supervisor::{FakeSupervisor, Step, card, run_dir, text, truthy};
use common::{lines, one, path_str as s, read_text, which};

const FREE: &str =
    "- Branch `leadhand/demo` is free: check it out anywhere with `git switch leadhand/demo`";

/// git, logging each command line to $GIT_ARGV_LOG first: its argument count, then each
/// argument, every field ended by a NUL (an argument can hold anything else).
const GIT_LOG: &str = r#"#!/bin/sh
printf '%s\0' "$#" "$@" >> "$GIT_ARGV_LOG"
exec "$GIT_ARGV_REAL" "$@"
"#;

/// The kinds of the events named, in log order.
#[track_caller]
fn kinds(w: &FakeSupervisor, names: &[&str]) -> Vec<String> {
    w.events(None)
        .iter()
        .map(|e| text(&e["kind"]).to_string())
        .filter(|k| names.contains(&k.as_str()))
        .collect()
}

/// The `seq` of the one event of `kind`.
#[track_caller]
fn seq_of(w: &FakeSupervisor, kind: &str) -> i64 {
    seq(&one(w.events(kind)))
}

/// The lines of the report's Result section.
#[track_caller]
fn result_lines(w: &FakeSupervisor) -> Vec<String> {
    let report = read_text(&w.paths().report);
    let section = report
        .split("## Result")
        .nth(1)
        .unwrap_or_else(|| panic!("no Result section in the report:\n{report}"));
    let section = section.split("## Evidence").next().unwrap();
    lines(section.trim())
}

/// Each registered worktree's checked-out branch ref (`git worktree list --porcelain`).
#[track_caller]
fn listed_branches(w: &FakeSupervisor) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut path: Option<String> = None;
    for line in run_git(&w.repo, &["worktree", "list", "--porcelain"]).lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let (Some(branch), Some(p)) = (line.strip_prefix("branch "), &path) {
            out.insert(p.clone(), branch.to_string());
        }
    }
    out
}

#[track_caller]
fn detached(path: &Path) -> bool {
    run_git_unchecked(path, &["symbolic-ref", "--quiet", "HEAD"]).is_empty()
}

/// Whether the operator's own checkout can switch to the run branch (then back to main).
#[track_caller]
fn can_switch(w: &FakeSupervisor) -> bool {
    run_git_unchecked(&w.repo, &["switch", "--quiet", RUN]);
    if run_git(&w.repo, &["symbolic-ref", "--short", "HEAD"]) != RUN {
        return false;
    }
    run_git(&w.repo, &["switch", "--quiet", "main"]);
    true
}

/// A `git` first on PATH that logs its argv; returns the log.
#[track_caller]
fn git_wrapper(w: &mut FakeSupervisor) -> PathBuf {
    let folder = w.tmp.join("git wrapper");
    fs::create_dir(&folder).unwrap();
    write_script(&folder.join("git"), GIT_LOG);
    let log = w.tmp.join("git argv.log");
    let path = path_before(w, &folder);
    let real = which("git").expect("git on PATH");
    w.env
        .set("PATH", path)
        .set("GIT_ARGV_REAL", real)
        .set("GIT_ARGV_LOG", &log);
    log
}

/// The command lines the wrapper logged (none when there is no log).
#[track_caller]
fn logged(log: &Path) -> Vec<Vec<String>> {
    if !log.exists() {
        return Vec::new();
    }
    let data = fs::read(log).unwrap_or_else(|e| panic!("reading {}: {e}", log.display()));
    // every record ends with a NUL: one cut off inside its last argument is no record, as a
    // cut-off JSON line fails json.loads
    assert!(
        data.is_empty() || data.ends_with(&[0]),
        "a cut-off record at the end of {}",
        log.display()
    );
    let mut fields = data
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    let mut argvs = Vec::new();
    while let Some(count) = fields.next() {
        if count.is_empty() {
            break; // after the last NUL
        }
        let n: usize = count
            .parse()
            .unwrap_or_else(|_| panic!("not an argument count: {count:?}"));
        let argv: Vec<String> = (&mut fields).take(n).collect();
        assert_eq!(argv.len(), n, "a cut-off record in {}", log.display());
        argvs.push(argv);
    }
    argvs
}

/// Whether `words` appear in `argv` one after another.
fn runs(argv: &[String], words: &[&str]) -> bool {
    (0..argv.len()).any(|i| argv[i..].len() >= words.len() && argv[i..i + words.len()] == *words)
}

mod branch_release {
    use super::*;

    #[test]
    fn test_complete_run_detaches_worktree_and_reports_branch_free() {
        if common::rust_only() {
            return;
        }
        let mut w = gated_with(&[card(1)], "");
        w.start();
        let ws = w.workspace();
        let done = w.run().pop().expect("an action");
        assert_eq!(done["outcome"], "complete");
        let head = w.branch_tip(RUN);

        // Recorded between the outcome and the report.
        assert_eq!(
            kinds(
                &w,
                &[
                    "outcome",
                    "branch_release_started",
                    "branch_released",
                    "report_written"
                ]
            ),
            [
                "outcome",
                "branch_release_started",
                "branch_released",
                "report_written"
            ]
        );
        let started = one(w.events("branch_release_started"));
        assert_json_eq!(
            common::pick(&started, &["worktree", "branch", "sha"]),
            Json::Arr(vec![s(&ws).into(), RUN.into(), head.as_str().into()])
        );
        let released = one(w.events("branch_released"));
        assert!(!released.get("skipped").is_some_and(truthy), "{released}");

        // Judged from Git: the worktree is detached at the run head, no worktree holds the
        // branch, and the operator's own checkout can switch to it.
        assert!(detached(&ws));
        assert_eq!(run_git(&ws, &["rev-parse", "HEAD"]), head);
        let run_ref = format!("refs/heads/{RUN}");
        assert!(!listed_branches(&w).values().any(|b| *b == run_ref));
        assert!(can_switch(&w));

        let lines = result_lines(&w);
        let line = lines.iter().find(|ln| ln.starts_with(FREE));
        assert!(line.is_some(), "{lines:?}");
        assert!(line.unwrap().contains(s(&ws)), "{lines:?}");

        // Repeating next changes nothing.
        assert_eq!(w.next()["outcome"], "complete");
        assert_eq!(w.events("branch_release_started").len(), 1);
        assert_eq!(w.events("branch_released").len(), 1);
    }

    #[test]
    fn test_deleted_worktree_folder_reports_prune_then_switch() {
        // The run's worktree folder is deleted by hand: Git keeps its record (prunable),
        // which still has the run branch checked out. The run fails (its branch cannot be
        // read there); the release has nothing to detach, and the report gives the fix.
        if common::rust_only() {
            return;
        }
        let mut w = gated_with(&[card(1)], "");
        w.start();
        let ws = w.workspace();
        fs::remove_dir_all(&ws).unwrap();
        let done = w.next();
        assert_json_eq!(
            common::pick(&done, &["action", "outcome"]),
            common::jv(r#"["done", "failed"]"#)
        );
        let released = one(w.events("branch_released"));
        assert!(released.get("skipped").is_some_and(truthy), "{released}");
        assert!(released.get("reason").is_some_and(truthy), "{released}");

        // Never pruned: Git still records the worktree with the branch checked out.
        let run_ref = format!("refs/heads/{RUN}");
        assert_eq!(
            listed_branches(&w).get(s(&ws)),
            Some(&run_ref),
            "{:?}",
            listed_branches(&w)
        );
        let lines = result_lines(&w);
        assert!(!lines.iter().any(|ln| ln.starts_with(FREE)), "{lines:?}");
        let branch_lines: Vec<&String> = lines
            .iter()
            .filter(|ln| ln.starts_with(&format!("- Branch `{RUN}`")))
            .collect();
        let [line] = <[&String; 1]>::try_from(branch_lines)
            .unwrap_or_else(|l| panic!("expected one branch line: {l:?}"));
        assert!(line.contains(s(&ws)), "{line}");
        let prune = line.find("`git worktree prune`");
        let switch = line.find(&format!("`git switch {RUN}`"));
        assert!(
            matches!((prune, switch), (Some(p), Some(q)) if p < q),
            "{line}"
        );

        // The fix works as the report says.
        assert!(!can_switch(&w));
        run_git(&w.repo, &["worktree", "prune"]);
        assert!(can_switch(&w));
    }

    #[test]
    fn test_blocked_run_keeps_branch_checked_out() {
        if common::rust_only() {
            return;
        }
        let mut w = gated_with(&[card(1)], "[run]\nevidence = \"block\"\n");
        w.start();
        let gap = || Step {
            result: Some("fail".into()),
            ..Step::default()
        };
        w.script(None, "evidence", [gap(), gap(), gap()]);
        let done = w.run().pop().expect("an action");
        assert_json_eq!(
            common::pick(&done, &["outcome", "qualifier"]),
            common::jv(r#"["blocked", "decision needed"]"#)
        );
        let ws = w.workspace();
        assert_empty!(kinds(&w, &["branch_release_started", "branch_released"]));
        assert_eq!(run_git(&ws, &["symbolic-ref", "--short", "HEAD"]), RUN);
        let run_ref = format!("refs/heads/{RUN}");
        assert_eq!(listed_branches(&w).get(s(&ws)), Some(&run_ref));
        let lines = result_lines(&w);
        assert!(
            !lines
                .iter()
                .any(|ln| ln.starts_with(&format!("- Branch `{RUN}`"))),
            "{lines:?}"
        );
    }

    #[test]
    fn test_release_never_runs_worktree_prune() {
        if common::rust_only() {
            return;
        }
        let mut w = gated_with(&[card(1)], "");
        w.start();
        to_last_step(&mut w); // the next `next` ends the run
        let log = git_wrapper(&mut w);
        assert_eq!(w.next()["outcome"], "complete");
        let argvs = logged(&log);
        // The wrapper saw the binary's Git calls: the release's detach among them.
        assert!(
            argvs
                .iter()
                .any(|a| a.iter().any(|x| x == "checkout") && a.iter().any(|x| x == "--detach")),
            "{argvs:?}"
        );
        assert!(
            argvs.iter().any(|a| runs(a, &["worktree", "list"])),
            "{argvs:?}"
        );
        let prunes: Vec<&Vec<String>> = argvs
            .iter()
            .filter(|a| runs(a, &["worktree", "prune"]))
            .collect();
        assert_empty!(prunes);
        assert!(detached(&w.workspace()));
        // Nor does writing the report again.
        fs::remove_file(&log).unwrap();
        w.cli(&os_args!["report", run_dir(&w)]).ok();
        let argvs = logged(&log);
        assert!(
            argvs.iter().any(|a| runs(a, &["worktree", "list"])),
            "{argvs:?}"
        );
        let prunes: Vec<&Vec<String>> = argvs
            .iter()
            .filter(|a| runs(a, &["worktree", "prune"]))
            .collect();
        assert_empty!(prunes);
        // The evidence base worktree, which the evidence gate prepared, is still registered.
        let list = run_git(&w.repo, &["worktree", "list", "--porcelain"]);
        assert!(list.contains(s(&base_wt(&w))), "{list}");
    }

    #[test]
    fn test_crash_between_outcome_and_report_finishes_release_first() {
        // The state a crash between the outcome and the report leaves, rebuilt as stage 1's
        // interrupted-start test rebuilds one: the run is driven to its end, its log cut back
        // to the `outcome`, `report.md` removed, then the release's intent appended in its
        // exact shape, with the worktree still on the run branch (the crash came before the
        // detach) or already detached (after it). The release has no named crash points
        // (S2-R48).
        if common::rust_only() {
            return;
        }
        let all = [
            "outcome",
            "branch_release_started",
            "branch_released",
            "report_written",
        ];
        for detached_before in [false, true] {
            let mut w = gated_with(&[card(1)], "");
            w.start();
            assert_eq!(
                w.run().pop().expect("an action")["outcome"],
                "complete",
                "detached_before={detached_before}"
            );
            let (ws, head) = (w.workspace(), w.branch_tip(RUN));
            let events = w.paths().events;
            let text_ = read_text(&events);
            let lines: Vec<&str> = text_.split_inclusive('\n').collect();
            let at: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, ln)| json::parse(ln.as_bytes())["kind"] == "outcome")
                .map(|(i, _)| i)
                .collect();
            let [at] = <[usize; 1]>::try_from(at)
                .unwrap_or_else(|a| panic!("expected one outcome line: {a:?}"));
            fs::write(&events, lines[..=at].concat()).unwrap();
            fs::remove_file(w.paths().report).unwrap();
            if !detached_before {
                run_git(&ws, &["switch", "--quiet", RUN]);
            }
            append_event(
                &run_dir(&w),
                "branch_release_started",
                [
                    ("worktree", Json::from(s(&ws))),
                    ("branch", Json::from(RUN)),
                    ("sha", Json::from(head.as_str())),
                ],
            );
            assert_eq!(
                kinds(&w, &all),
                ["outcome", "branch_release_started"],
                "detached_before={detached_before}"
            );
            assert_eq!(
                detached(&ws),
                detached_before,
                "detached_before={detached_before}"
            );

            let done = w.next();
            assert_json_eq!(
                common::pick(&done, &["action", "outcome"]),
                common::jv(r#"["done", "complete"]"#),
                "detached_before={detached_before}"
            );
            // The release is finished first, once, then the report is written.
            assert_eq!(kinds(&w, &all), all, "detached_before={detached_before}");
            assert!(
                seq_of(&w, "branch_released") < seq_of(&w, "report_written"),
                "detached_before={detached_before}"
            );
            let released = one(w.events("branch_released"));
            assert!(!released.get("skipped").is_some_and(truthy), "{released}");
            assert!(detached(&ws), "detached_before={detached_before}");
            assert_eq!(
                run_git(&ws, &["rev-parse", "HEAD"]),
                head,
                "detached_before={detached_before}"
            );
            let lines = result_lines(&w);
            assert!(
                lines.iter().any(|ln| ln.starts_with(FREE)),
                "detached_before={detached_before}: {lines:?}"
            );
        }
    }
}
