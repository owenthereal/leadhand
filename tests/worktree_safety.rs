//! Safety behaviours of a worktree run, and the operator-checkout invariant (ruling S1-R16).
//!
//! The Rust form of the Python contract suite's `contract_worktree_safety` (written for stage 1
//! from Python's `test_engine_in_place`, the in-place cases of `test_gitops` and the rulings they
//! rest on: core run Codex round 10, R30, R54', R60, R63, R64; docs/compatibility.md, Security).
//! In-place runs are gone (deliberate change 1); what they tested that also holds for a run in its
//! own worktree is a worktree scenario here. SavedState and Workspace, at the end, convert the rest
//! of Python's `test_gitops`'s GitopsTests that a run shows from outside: a violation's saved
//! commit and sibling refs, the run's worktree and the excludes, and leadhand's own commits made
//! unsigned.
//!
//! `FakeSupervisor` plays the run skill; a worker misbehaves through its step's `tamper`, which
//! runs in the harness against the run's worktree. Every safety outcome is judged from Git and
//! the filesystem directly: refs and the trees they hold, `ls-files -v` tags (assume-unchanged
//! is a lowercase tag, skip-worktree `S`), `core.sparseCheckout`, the files under a folder a
//! worker pointed elsewhere, and the operator's own checkout. The binary's events are asserted
//! beside that, never as the only evidence.

#[macro_use]
mod common;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use common::engine::{
    BY_RISK, CARD1, CODEX_CARD, NOTE, RUN, base_wt, codex_world, commit_on, drive_until_refused,
    files, found, found_by, gate_names, gated, marker, parked_world, relaunch_env, requested,
    run_id, seq, tampering, unpark,
};
use common::git::{
    IDENTITY, commit, current_branch, make_repo_named, run_git, run_git_unchecked, write,
};
use common::json::{self, Json};
use common::supervisor::{
    CardOpts, CliOpts, FakeSupervisor, Options, Step, card, card_with, os_path, run_dir, text,
    truthy,
};
use common::{
    Env, Out, field, jv, lines, one, os, path_str, pick, process_env, read_json, read_text,
    realpath, resolve, run_argv, set_mtime, strs, symlink, write_text,
};

const CARD1_FILE: &str = "src/card1/c1-a1-implement-1.txt";
const SPARSE_REFUSAL: &str = "sparse checkouts are not supported";

/// Not UTF-8.
const ODD_NAME: &[u8] = b"tests/notes-\xff.txt";

/// `git ARGV` in `cwd` with the test process's own environment (Python's
/// `subprocess.run([...], env=dict(os.environ), check=True, timeout=60)`); panics when it
/// fails.
#[track_caller]
fn git_checked(world_env: &Env, cwd: &Path, args: &[&OsStr]) -> Out {
    let env = Env::from_parts(process_env(), world_env.fakes().to_path_buf());
    let mut argv = vec![OsStr::new("git")];
    argv.extend_from_slice(args);
    let r = run_argv(&argv, &env, cwd, None, Duration::from_secs(60));
    if r.code != 0 {
        panic!("git failed (check=True)\n{r}");
    }
    r
}

/// A worker's tamper: commit ODD_NAME through Git's index alone (the file system here may
/// refuse such a name), so the file is in the commit but not in the worktree.
fn odd_name(world: &FakeSupervisor, ws: &Path, _a: &Json) {
    let notes = world.tmp.join("notes blob.txt");
    write_text(&notes, "notes\n");
    let out = git_checked(&world.env, ws, &os_args!["hash-object", "-w", notes]);
    let blob = out.stdout.trim_ascii();
    let mut cacheinfo = b"100644,".to_vec();
    cacheinfo.extend_from_slice(blob);
    cacheinfo.push(b',');
    cacheinfo.extend_from_slice(ODD_NAME);
    git_checked(
        &world.env,
        ws,
        &os_args!["update-index", "--add", "--cacheinfo", os(&cacheinfo)],
    );
    let mut args: Vec<&OsStr> = IDENTITY.iter().map(OsStr::new).collect();
    args.extend(["commit", "--quiet", "-m", "odd name"].map(OsStr::new));
    git_checked(&world.env, ws, &args);
}

/// The paths of the tree at `rev`.
#[track_caller]
fn tree_paths(repo: &Path, rev: &str) -> Vec<String> {
    lines(&run_git(repo, &["ls-tree", "-r", "--name-only", rev]))
}

/// `git ls-files -v` lines: tag, space, path.
#[track_caller]
fn tags(cwd: &Path) -> Vec<String> {
    lines(&run_git(cwd, &["ls-files", "-v"]))
}

/// The `ls-files -v` lines whose tag is not `H` (a flagged or unusual entry).
#[track_caller]
fn flagged(cwd: &Path) -> Vec<String> {
    tags(cwd)
        .into_iter()
        .filter(|t| !t.starts_with("H "))
        .collect()
}

/// A NUL-separated Git listing as bytes (names that are not UTF-8 kept as they are).
#[track_caller]
fn run_git_bytes(world_env: &Env, cwd: &Path, args: &[&str]) -> Vec<Vec<u8>> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    git_checked(world_env, cwd, &args)
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// core.sparseCheckout as that checkout sees it ("" when unset).
#[track_caller]
fn sparse_setting(cwd: &Path) -> String {
    run_git_unchecked(cwd, &["config", "--bool", "--get", "core.sparseCheckout"])
}

/// The last action of a run (`w.run()[-1]`).
#[track_caller]
fn run_to_end(w: &mut FakeSupervisor) -> Json {
    w.run().pop().expect("an action")
}

/// A review step with `verdict` and `findings`.
fn review(verdict: &str, findings: &[&str]) -> Step {
    Step {
        verdict: Some(verdict.into()),
        findings: Some(findings.iter().map(|f| f.to_string()).collect()),
        ..Step::default()
    }
}

/// The ref a violation saved, as text.
#[track_caller]
fn ref_of(v: &Json) -> String {
    text(&v["ref"]).to_string()
}

// --------------------------------------------------------------------------
// A reviewer hiding an edit behind an index flag (Codex round 10)

/// An index entry a reviewer marks assume-unchanged or skip-worktree hides its edit from
/// `status` and `diff`. Leadhand clears the mark before the check, so the edit is a violation:
/// preserved on refs/leadhand/<run id>/violations/<n>, the worktree restored, and the review
/// run again by a fresh worker. The operator's own checkout (another index) is never touched.
mod hidden_index_entries {
    use super::*;

    #[track_caller]
    fn reviewed(
        tamper: impl Fn(&FakeSupervisor, &Path, &Json) + 'static,
    ) -> (FakeSupervisor, Json, String) {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "review", [tampering(tamper), Step::default()]);
        let done = run_to_end(&mut w);
        assert_eq!(done["action"], "done");
        let v = one(found(&w));
        assert_json_eq!(
            pick(&v, &["dispatch_id", "dispatch_kind"]),
            jv(r#"["c1-a1-review-1", "review"]"#)
        );
        assert_eq!(
            v["ref"],
            format!("refs/leadhand/{}/violations/1", run_id(&w)).as_str()
        );
        let tip = text(&requested(&w, "c1-a1-review-1")["expected_head"]).to_string();
        // The edit is kept, on top of the card tip the reviewer was given.
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{}^1", ref_of(&v))]),
            tip
        );
        // Run again by a fresh worker; only the second review's verdict counts.
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"]
        );
        assert!(requested(&w, "c1-a1-review-2")["resume_worker"].is_null());
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-2"]"#)
        );
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
        assert_eq!(w.branch_tip(RUN), tip);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
        // The worktree is back on the card tip, every entry visible again and clean.
        let ws = w.workspace();
        assert_eq!(current_branch(&ws).as_deref(), Some(CARD1));
        assert_eq!(run_git(&ws, &["rev-parse", "HEAD"]), tip);
        assert_empty!(flagged(&ws));
        assert_eq!(
            run_git(&ws, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
        // The operator's checkout keeps its own index and files.
        assert_eq!(read_text(&w.repo.join("README.md")), "hello\n");
        assert_eq!(tags(&w.repo), ["H README.md"]);
        assert_eq!(current_branch(&w.repo).as_deref(), Some("main"));
        (w, v, tip)
    }

    #[test]
    fn test_reviewer_assume_unchanged_edit_is_a_violation() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["update-index", "--assume-unchanged", "README.md"]);
            write(ws, &[("README.md", "edited by the reviewer, hidden\n")]);
            assert_eq!(run_git(ws, &["status", "--porcelain"]), ""); // hidden indeed
        };
        let (w, v, _) = reviewed(tamper);
        assert_json_eq!(v["hidden"], jv(r#"["README.md"]"#));
        assert_json_eq!(v["paths"], jv(r#"["README.md"]"#));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{}:README.md", ref_of(&v))]),
            "edited by the reviewer, hidden"
        );
        assert_eq!(read_text(&w.workspace().join("README.md")), "hello\n");
    }

    #[test]
    fn test_reviewer_skip_worktree_edit_is_a_violation() {
        // One entry edited, another deleted, both behind skip-worktree.
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(
                ws,
                &["update-index", "--skip-worktree", "README.md", CARD1_FILE],
            );
            write(ws, &[("README.md", "edited by the reviewer, hidden\n")]);
            fs::remove_file(ws.join(CARD1_FILE)).expect("removing the card's file");
            assert_eq!(run_git(ws, &["status", "--porcelain"]), ""); // hidden indeed
        };
        let (w, v, tip) = reviewed(tamper);
        let mut hidden = strs(&v["hidden"]);
        hidden.sort();
        assert_eq!(hidden, ["README.md", CARD1_FILE]);
        let mut paths = strs(&v["paths"]);
        paths.sort();
        assert_eq!(paths, ["README.md", CARD1_FILE]);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{}:README.md", ref_of(&v))]),
            "edited by the reviewer, hidden"
        );
        assert!(!tree_paths(&w.repo, &ref_of(&v)).contains(&CARD1_FILE.to_string()));
        let ws = w.workspace();
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
        assert_eq!(read_text(&ws.join(CARD1_FILE)), "c1-a1-implement-1\n");
        assert!(tree_paths(&w.repo, &tip).contains(&CARD1_FILE.to_string()));
    }
}

// --------------------------------------------------------------------------
// Sparse checkouts (R63, R64)

mod sparse_checkouts {
    use super::*;

    #[test]
    fn test_sparse_checkout_refused_by_check_start_and_run() {
        // R63: a worktree run's worktrees would inherit a sparse checkout, so `check`, `start`
        // and every launch refuse a repository whose checkout is sparse, with short advice.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.base = commit(
            &w.repo,
            &[("far/away.txt", "outside the sparse patterns\n")],
            "far",
        );
        w.write_deck(&[card(1)]);
        let sparse = ["sparse-checkout", "set", "--no-cone", "/README.md", "/src/"];
        run_git(&w.repo, &sparse);
        assert!(!w.repo.join("far/away.txt").exists());

        let r = w.cli(&os_args!["check", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        let refusals: Vec<String> = lines(&r.stdout_text())
            .into_iter()
            .filter(|ln| ln.contains(SPARSE_REFUSAL))
            .collect();
        assert_eq!(refusals.len(), 1, "{r}");
        assert!(
            refusals[0].starts_with("error: not supported in this version: "),
            "{r}"
        );

        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        let stderr = r.stderr_text();
        assert!(stderr.starts_with("error: "), "{r}");
        assert!(stderr.contains(SPARSE_REFUSAL));
        assert!(!stderr.contains("--no-skip-worktree"));
        assert!(stderr.chars().count() < 600, "{r}");
        assert!(!w.docs.join("runs").exists());
        assert_eq!(
            run_git(&w.repo, &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
        assert_eq!(run_git(&w.repo, &["branch", "--list", "leadhand/*"]), "");

        // Not sparse: the same deck starts.
        run_git(&w.repo, &["sparse-checkout", "disable"]);
        w.start();
        // Sparse again before the launch: `run` refuses it, and no supervisor starts.
        run_git(&w.repo, &sparse);
        let claude_log = w.tmp.join("fake claude.log");
        let mut env = relaunch_env(&w);
        env.push(("FAKE_CLAUDE_LOG".into(), claude_log.clone().into()));
        env.push(("FAKE_CLAUDE_SLEEP".into(), "0".into()));
        let r = w.cli_with(
            &os_args!["run", run_dir(&w)],
            CliOpts {
                env,
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().contains(SPARSE_REFUSAL));
        assert!(!claude_log.exists(), "a supervisor was started");
        assert_empty!(w.events("resumed"));
    }

    #[test]
    fn test_worker_enabled_sparse_checkout_is_disabled_again() {
        // R64: an implementer turns sparse checkout on in the run's worktree, which hides
        // README.md (skip-worktree, file removed). Before the review, leadhand clears the mark,
        // turns sparse checkout off again in that worktree, and keeps the deletion as the
        // implementer's leftovers; the operator's checkout never becomes sparse.
        let sparse_on = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["sparse-checkout", "set", "--no-cone", "/src/"]);
            assert!(!ws.join("README.md").exists());
            assert!(tags(ws).contains(&"S README.md".to_string()));
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [tampering(sparse_on)]);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        let v = one(found_by(&w, "implementer"));
        assert!(strs(&v["hidden"]).contains(&"README.md".to_string()), "{v}");
        assert!(strs(&v["paths"]).contains(&"README.md".to_string()), "{v}");
        assert!(!tree_paths(&w.repo, &ref_of(&v)).contains(&"README.md".to_string()));
        let ws = w.workspace();
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
        assert_empty!(flagged(&ws));
        assert_ne!(sparse_setting(&ws), "true");
        assert_ne!(sparse_setting(&w.repo), "true");
        assert_eq!(read_text(&w.repo.join("README.md")), "hello\n");
        assert_eq!(tags(&w.repo), ["H README.md"]);
        // Leftovers void nothing: the review ran once and the card was accepted with README.md.
        assert_empty!(found(&w));
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"]);
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }
}

// --------------------------------------------------------------------------
// A symlink planted at a later dispatch's scratch folder (L2)

mod scratch_folders {
    use super::*;

    #[test]
    fn test_symlinked_scratch_folder_refused_without_writing() {
        // The implementer plants a symlink where the review's scratch folder goes. `next`
        // refuses with the path and writes nothing through it, as long as it is there; once
        // the operator removes it, the review's brief is written in the worktree and the run
        // goes on.
        let mut w = FakeSupervisor::new(&[card(1)]);
        let outside = w.tmp.join("outside");
        fs::create_dir(&outside).unwrap();
        let kept = [
            ("brief.md", "the operator's brief\n"),
            ("report.md", "keep me\n"),
            ("reply.txt", "keep me too\n"),
        ];
        for (name, text) in kept {
            write_text(&outside.join(name), text);
        }
        let planted: Rc<RefCell<Option<PathBuf>>> = Rc::default();
        let (target, record) = (outside.clone(), planted.clone());
        let plant = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            let link = ws.join(".leadhand/1/c1-a1-review-1");
            symlink(&target, &link);
            *record.borrow_mut() = Some(link);
        };

        w.start();
        w.script(1, "implement", [tampering(plant)]);
        let a = w.next();
        w.act(&a);
        let link = planted.borrow().clone().expect("the planted link"); // planted["link"]
        for _ in 0..2 {
            let r = w.cli(&os_args!["next", run_dir(&w)]);
            assert_eq!(r.code, 1, "{r}");
            assert!(r.stderr_text().starts_with("error: "), "{r}");
            assert!(r.stderr_text().contains(".leadhand/1/c1-a1-review-1"));
        }
        let mut names: Vec<OsString> = fs::read_dir(&outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        let mut want: Vec<OsString> = kept.iter().map(|(n, _)| OsString::from(n)).collect();
        want.sort();
        assert_eq!(names, want);
        for (name, text) in kept {
            assert_eq!(read_text(&outside.join(name)), text, "{name}");
        }
        let reviews: Vec<Json> = w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["dispatch_kind"] == "review")
            .collect();
        assert_empty!(reviews);

        fs::remove_file(&link).expect("removing the link"); // the operator removes it
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a1-review-1"]"#)
        );
        let brief = os_path(&a["brief"]);
        assert_eq!(brief, link.join("brief.md"));
        assert!(!link.is_symlink());
        assert!(brief.is_file());
        assert_eq!(
            realpath(&brief),
            realpath(&w.workspace()).join(".leadhand/1/c1-a1-review-1/brief.md")
        );
        w.act(&a);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
        for (name, text) in kept {
            assert_eq!(read_text(&outside.join(name)), text, "{name}");
        }
    }
}

// --------------------------------------------------------------------------
// Implementer leftovers before a check (R30)

mod leftovers {
    use super::*;

    #[test]
    fn test_leftovers_preserved_on_ref_before_review() {
        // The implementer commits, then leaves a staged edit, a different unstaged edit of the
        // same file, and an edit hidden behind assume-unchanged. Before the review they are
        // kept on refs/leadhand/<run id>/leftovers/1 (a stash-style commit: the work tree's
        // files, the index as its second parent), the worktree is put back on the card tip,
        // and the review is not voided.
        let leave = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[("README.md", "staged by the implementer\n")]);
            run_git(ws, &["add", "--", "README.md"]);
            write(ws, &[("README.md", "unstaged by the implementer\n")]);
            run_git(ws, &["update-index", "--assume-unchanged", CARD1_FILE]);
            write(ws, &[(CARD1_FILE, "hidden by the implementer\n")]);
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [tampering(leave)]);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        let v = one(found_by(&w, "implementer"));
        let saved = format!("refs/leadhand/{}/leftovers/1", run_id(&w));
        assert_eq!(v["ref"], saved.as_str());
        assert_eq!(v["why"], "before review c1-a1-review-1");
        let mut paths = strs(&v["paths"]);
        paths.sort();
        assert_eq!(paths, ["README.md", CARD1_FILE]);
        assert!(truthy(&v["index_dirty"]), "{v}");
        assert_json_eq!(v["hidden"], Json::from(&[CARD1_FILE][..]));
        assert!(seq(&v) < seq(&requested(&w, "c1-a1-review-1")));
        let restored = one(w
            .events("violation")
            .into_iter()
            .filter(|e| {
                e.get("by").is_some_and(|b| b == "implementer")
                    && e.get("stage").is_some_and(|s| s == "restored")
            })
            .collect());
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &saved]),
            text(&restored["saved"])
        );

        let tip = text(&requested(&w, "c1-a1-review-1")["expected_head"]).to_string();
        assert_eq!(run_git(&w.repo, &["rev-parse", &format!("{saved}^1")]), tip);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{saved}:README.md")]),
            "unstaged by the implementer"
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{saved}^2:README.md")]),
            "staged by the implementer"
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{saved}:{CARD1_FILE}")]),
            "hidden by the implementer"
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{tip}:{CARD1_FILE}")]),
            "c1-a1-implement-1"
        );

        let ws = w.workspace();
        assert_eq!(run_git(&ws, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            run_git(&ws, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
        assert_empty!(flagged(&ws));
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
        assert_eq!(read_text(&ws.join(CARD1_FILE)), "c1-a1-implement-1\n");
        assert_empty!(found(&w));
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"]);
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-1"]"#)
        );
        assert_eq!(w.branch_tip(RUN), tip);
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:README.md")]),
            "hello"
        );
    }
}

// --------------------------------------------------------------------------
// The evidence base worktree's identity (R60)

/// What `BaseWorktreeIdentity` compares of a worktree: HEAD, branch, index, status, files.
#[derive(Debug, PartialEq)]
struct WorktreeState {
    head: String,
    branch: String,
    index: String,
    status: String,
    files: Vec<(&'static str, String)>,
}

mod base_worktree_identity {
    use super::*;

    #[test]
    fn test_foreign_worktree_at_base_path_is_never_reset() {
        // Covers bug class 4 (ownership by identity) and R60.
        // A worktree this run did not add sits where the evidence base worktree goes, on the
        // operator's branch with a commit of its own and work of every kind. The evidence gate
        // is refused, again on every `next`, and nothing in that worktree changes.
        let mut w = gated(&[card(1)]);
        w.start();
        let path = base_wt(&w);
        run_git(
            &w.repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "operator-notes",
                path_str(&path),
                &w.base,
            ],
        );
        commit(
            &path,
            &[(".gitignore", "*.log\n"), ("notes.md", "committed notes\n")],
            "the operator's commit",
        );
        write(&path, &[("staged.txt", "staged\n")]);
        run_git(&path, &["add", "--", "staged.txt"]);
        write(
            &path,
            &[
                ("README.md", "an unstaged edit\n"),
                ("untracked.txt", "untracked\n"),
                ("build.log", "ignored\n"),
            ],
        );

        let state = || WorktreeState {
            head: run_git(&path, &["rev-parse", "HEAD"]),
            branch: run_git(&path, &["symbolic-ref", "--short", "HEAD"]),
            index: run_git(&path, &["ls-files", "-s", "-v"]),
            status: run_git(
                &path,
                &[
                    "status",
                    "--porcelain",
                    "--ignored",
                    "--untracked-files=all",
                ],
            ),
            files: [
                "README.md",
                "notes.md",
                "staged.txt",
                "untracked.txt",
                "build.log",
            ]
            .map(|p| (p, read_text(&path.join(p))))
            .to_vec(),
        };

        let before = state();
        assert!(lines(&before.status).contains(&"!! build.log".to_string()));
        for attempt in 0..2 {
            let r = if attempt == 0 {
                drive_until_refused(&mut w)
            } else {
                w.cli(&os_args!["next", run_dir(&w)])
            };
            assert_eq!(r.code, 1, "{r}");
            let want = format!(
                "error: {} is a worktree this run did not add",
                path.display()
            );
            assert!(r.stderr_text().contains(&want), "{r}");
        }
        assert_eq!(state(), before);
        assert!(!marker(&path).exists());
        assert!(!gate_names(&w).iter().any(|g| g == "evidence"));
        let evidence: Vec<Json> = w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["dispatch_kind"] == "evidence")
            .collect();
        assert_empty!(evidence);
    }
}

// --------------------------------------------------------------------------
// A card branch the run did not record (R54')

mod unrecorded_card_branch {
    use super::*;

    #[test]
    fn test_unrecorded_card_branch_adopted_only_at_run_tip() {
        // A branch named like card 1's exists before the run cuts it. Holding a commit of its
        // own, it is not the run's: `next` refuses and leaves it alone. Exactly at the run tip
        // (a cut whose record a crash lost holds nothing of its own) it is taken up.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let theirs = commit_on(&w.repo, &w.base);
        run_git(&w.repo, &["branch", CARD1, &theirs]);
        for _ in 0..2 {
            let r = w.cli(&os_args!["next", run_dir(&w)]);
            assert_eq!(r.code, 1, "{r}");
            assert!(r.stderr_text().contains(&format!(
                "error: the card branch {CARD1} already exists but this run did not cut it"
            )));
            assert_eq!(
                run_git(&w.repo, &["rev-parse", &format!("refs/heads/{CARD1}")]),
                theirs
            );
            assert_eq!(current_branch(&w.workspace()).as_deref(), Some(RUN));
            assert_empty!(w.events("card_cut"));
        }

        run_git(&w.repo, &["branch", "-f", CARD1, RUN]);
        let a = w.next();
        assert_json_eq!(
            pick(&a, &["action", "dispatch_id"]),
            jv(r#"["dispatch", "c1-a1-implement-1"]"#)
        );
        let cut = one(w.events("card_cut"));
        assert_json_eq!(
            pick(&cut, &["card", "branch", "base"]),
            Json::Arr(vec![1.into(), CARD1.into(), w.base.as_str().into()])
        );
        assert_eq!(current_branch(&w.workspace()).as_deref(), Some(CARD1));
        w.act(&a);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        let accepted = one(w.events("accepted"));
        let sha = text(&accepted["sha"]).to_string();
        assert_eq!(w.branch_tip(RUN), sha);
        assert_eq!(run_git(&w.repo, &["rev-parse", &format!("{sha}^")]), w.base);
        assert_ne!(run_git(&w.repo, &["merge-base", &theirs, RUN]), theirs);
    }
}

// --------------------------------------------------------------------------
// In-place behaviour that holds in a worktree run too

mod worktree_rewrites {
    use super::*;

    #[test]
    fn test_an_ignored_file_a_worker_left_does_not_block_a_card_cut() {
        // Covers R61.
        // In a worktree run an ignored file in the run's worktree is a worker's or a build's,
        // never the operator's, so cutting a card branch may overwrite it (an in-place run
        // refused to). Card 1 tracks local.env (ignored by .gitignore) and is accepted; card 2
        // stops tracking it, leaves its own version behind and is parked; card 3 is cut at the
        // run tip, which tracks it again.
        let track = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[("local.env", "card one's settings\n")]);
            run_git(ws, &["add", "--force", "--", "local.env"]);
            run_git(ws, &["commit", "--quiet", "-m", "track local.env"]);
        };
        let untrack = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["rm", "--quiet", "--cached", "--", "local.env"]);
            run_git(ws, &["commit", "--quiet", "-m", "stop tracking local.env"]);
            write(ws, &[("local.env", "a worker's leftover\n")]);
            assert!(
                lines(&run_git(ws, &["status", "--porcelain", "--ignored"]))
                    .contains(&"!! local.env".to_string())
            );
        };
        let seen: Rc<RefCell<HashMap<&'static str, String>>> = Rc::default();
        let record = seen.clone();
        let look = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            record
                .borrow_mut()
                .insert("local.env", read_text(&ws.join("local.env")));
        };

        let cards = [card(1), card(2), card(3)];
        let mut w = FakeSupervisor::new(&cards);
        w.base = commit(
            &w.repo,
            &[(".gitignore", "local.env\n")],
            "ignore local.env",
        );
        w.write_deck(&cards);
        w.start();
        w.script(1, "implement", [tampering(track)]);
        w.script(
            2,
            "implement",
            [Step {
                status: Some("QUESTION".into()),
                ..tampering(untrack)
            }],
        );
        w.script(3, "implement", [tampering(look)]);
        w.parks
            .insert(2, "decision needed: settings format".to_string());
        let done = run_to_end(&mut w);
        assert_eq!(done["action"], "done");
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1, 3]"));
        let accepted: HashMap<i64, Json> = w
            .events("accepted")
            .into_iter()
            .map(|e| (e["card"].as_i64().unwrap(), e["sha"].clone()))
            .collect();
        let cut: HashMap<i64, Json> = w
            .events("card_cut")
            .into_iter()
            .map(|e| (e["card"].as_i64().unwrap(), e))
            .collect();
        assert_json_eq!(
            cut.get(&3).expect("card 3's cut")["base"],
            accepted.get(&1).expect("card 1's accept")
        );
        let seen_env = seen
            .borrow()
            .get("local.env")
            .cloned()
            .expect("card 3's worker looked at local.env");
        assert_eq!(seen_env, "card one's settings\n");
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{RUN}:local.env")]),
            "card one's settings"
        );
        assert_empty!(found_by(&w, "implementer"));
    }

    #[test]
    fn test_accept_moves_a_run_branch_checked_out_in_the_worktree() {
        // A low-risk card under by-risk review is accepted straight from its implementer's
        // result, with no workspace check between. An implementer that commits on its card
        // branch and then leaves the run's worktree on the run branch has the acceptance
        // fast-forward a branch checked out there: the branch moves, and the worktree with it,
        // clean and holding the card's work.
        let to_run_branch = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["checkout", "--quiet", RUN]);
        };
        let mut w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: BY_RISK,
                ..Options::default()
            },
        );
        w.start();
        w.script(1, "implement", [tampering(to_run_branch)]);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1"]);
        let accepted = one(w.events("accepted"));
        let tip = text(&accepted["sha"]).to_string();
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("refs/heads/{CARD1}")]),
            tip
        );
        assert_eq!(w.branch_tip(RUN), tip);
        let ws = w.workspace();
        assert_eq!(current_branch(&ws).as_deref(), Some(RUN));
        assert_eq!(run_git(&ws, &["rev-parse", "HEAD"]), tip);
        assert_eq!(run_git(&ws, &["status", "--porcelain"]), "");
        assert_eq!(read_text(&ws.join(CARD1_FILE)), "c1-a1-implement-1\n");
    }

    #[test]
    fn test_check_diff_patch_keeps_the_standard_prefixes_whatever_the_repo_diff_settings() {
        // Covers bug class 3 (user Git config).
        // The operator's diff settings in the repository's configuration, which the run's
        // worktree shares, never change the patch a test-only fix's diff check reads.
        let mut w = FakeSupervisor::new(&[card(1)]);
        for (key, value) in [
            ("diff.noprefix", "true"),
            ("diff.mnemonicPrefix", "true"),
            ("diff.srcPrefix", "old/"),
            ("diff.dstPrefix", "new/"),
            ("diff.relative", "true"),
        ] {
            run_git(&w.repo, &["config", key, value]);
        }
        w.start();
        w.script(
            1,
            "implement",
            [files(&[
                ("src/card1/widget.py", "W = 1\n"),
                ("tests/test_widget.py", "assert True\n"),
            ])],
        );
        w.script(
            1,
            "review",
            [review(
                "defects",
                &["[medium] tests/test_widget.py — weak — assert more"],
            )],
        );
        w.script(
            1,
            "fix",
            [files(&[("tests/test_widget.py", "assert more\n")])],
        );
        let mut a = Json::Null;
        for _ in 0..30 {
            a = w.next();
            if a["action"] == "check_diff" {
                break;
            }
            w.act(&a);
        }
        assert_json_eq!(
            pick(&a, &["action", "dispatch_id"]),
            jv(r#"["check_diff", "c1-a1-fix-1"]"#)
        );
        let patch = read_text(&os_path(&a["diff"]));
        assert!(
            patch.contains("diff --git a/tests/test_widget.py b/tests/test_widget.py\n"),
            "{patch}"
        );
        assert!(
            patch.contains("--- a/tests/test_widget.py\n+++ b/tests/test_widget.py\n"),
            "{patch}"
        );
        assert!(patch.contains("+assert more\n"), "{patch}");
    }

    #[test]
    fn test_json_output_escapes_undecodable_names() {
        // Covers bug class 2 (path frames).
        // Spec contract item 13: an undecodable byte 0xXX of a name Git lists appears in the
        // JSON the CLI prints as \udcXX, in ASCII JSON. The fix commits a test file whose name
        // is not UTF-8 (odd_name), so the diff check's `paths` carries it. The diff check's
        // note, non-ASCII text, goes the other way: `record` prints it, and writes it to
        // events.jsonl, as ASCII JSON escapes that parse back to the same text.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(
            1,
            "review",
            [review(
                "defects",
                &["[medium] tests/test_widget.py — weak — assert more"],
            )],
        );
        w.script(
            1,
            "fix",
            [Step {
                tamper: Some(Box::new(odd_name)),
                ..files(&[("tests/test_widget.py", "assert more\n")])
            }],
        );
        let mut last: Option<(Out, Json)> = None;
        for _ in 0..30 {
            let r = w.cli(&os_args!["next", run_dir(&w)]).ok();
            let a = r.json();
            let hit = a["action"] == "check_diff";
            last = Some((r, a.clone()));
            if hit {
                break;
            }
            w.act(&a);
        }
        let (r, a) = last.expect("a next");
        assert_json_eq!(
            pick(&a, &["action", "dispatch_id"]),
            jv(r#"["check_diff", "c1-a1-fix-1"]"#)
        );
        assert!(r.stdout.is_ascii(), "{r}");
        assert!(
            r.stdout_text().contains(r#""tests/notes-\udcff.txt""#),
            "{r}"
        );
        let mut paths: Vec<Vec<u8>> = a["paths"]
            .as_array()
            .unwrap_or_else(|| panic!("no paths list: {a}"))
            .iter()
            .map(|p| p.as_bytes().expect("a string path").to_vec())
            .collect();
        paths.sort();
        let mut want = vec![json::from_os(ODD_NAME), b"tests/test_widget.py".to_vec()];
        want.sort();
        assert_eq!(paths, want);
        assert!(
            run_git_bytes(
                &w.env,
                &w.repo,
                &[
                    "ls-tree",
                    "-r",
                    "--name-only",
                    "-z",
                    &format!("refs/heads/{CARD1}")
                ]
            )
            .contains(&ODD_NAME.to_vec())
        );

        let note = "café — the test-only fix looks right";
        let r = w
            .cli(&os_args![
                "record",
                run_dir(&w),
                "diff-check",
                "--card",
                "1",
                "--ok",
                "yes",
                "--note",
                note
            ])
            .ok();
        assert!(r.stdout.is_ascii(), "{r}");
        assert_json_eq!(
            pick(&r.json(), &["kind", "note"]),
            Json::Arr(vec!["diff_checked".into(), note.into()]),
            "{r}"
        );
        let raw = read_text(&run_dir(&w).join("events.jsonl"));
        let matching: Vec<&str> = raw
            .lines()
            .filter(|line| line.contains(r#""kind":"diff_checked""#))
            .collect();
        let [line] = matching[..] else {
            panic!("expected one diff_checked line, got {matching:?}")
        };
        assert!(line.is_ascii(), "{line}");
        assert_eq!(jv(line)["note"], note);
    }

    #[test]
    fn test_events_jsonl_keeps_undecodable_names_as_escapes() {
        // Spec contract item 13, on disk: an implementer that commits a name that is not UTF-8
        // (odd_name) leaves the run's worktree with that file missing, so the check before the
        // review finds a violation. Its `paths` is written to events.jsonl as \udcff in an
        // ASCII line, and a later reader (`status`) reads it back. Whether the worktree can
        // then be restored depends on the file system (APFS refuses the name; ext4 accepts it
        // and a second violation line, stage restored, follows), so that `next`'s outcome is
        // not asserted and the found line is selected by its stage.
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "implement", [tampering(odd_name)]);
        let a = w.next();
        w.act(&a);
        w.cli(&os_args!["next", run_dir(&w)]); // the check before the review
        let v = one(found_by(&w, "implementer"));
        assert_json_eq!(
            v["paths"],
            Json::Arr(vec![Json::Str(json::from_os(ODD_NAME))])
        );
        assert_eq!(v["why"], "before review c1-a1-review-1");
        assert!(
            run_git_bytes(
                &w.env,
                &w.repo,
                &[
                    "ls-tree",
                    "-r",
                    "--name-only",
                    "-z",
                    &format!("refs/heads/{CARD1}")
                ]
            )
            .contains(&ODD_NAME.to_vec())
        );
        run_git(&w.repo, &["rev-parse", "--verify", "--quiet", &ref_of(&v)]);
        let raw = read_text(&run_dir(&w).join("events.jsonl"));
        let matching: Vec<&str> = raw
            .lines()
            .filter(|line| {
                line.contains(r#""kind":"violation""#)
                    && jv(line).get("stage").is_some_and(|s| s == "found")
            })
            .collect();
        let [line] = matching[..] else {
            panic!("expected one found violation line, got {matching:?}")
        };
        assert!(line.is_ascii(), "{line}");
        assert!(
            line.contains(r#""paths":["tests/notes-\udcff.txt"]"#),
            "{line}"
        );
        let st = w.cli(&os_args!["status", run_dir(&w)]);
        assert_eq!(st.code, 0, "{st}");
        let vref = ref_of(&v);
        let want = format!(
            "Violation by a implementer worker (dispatch unknown): its changes were preserved \
             at {vref} — next: inspect {vref}"
        );
        assert!(lines(&st.stdout_text()).contains(&want), "{st}");
    }
}

// --------------------------------------------------------------------------
// The operator's checkout (ruling S1-R16)

/// What `OperatorCheckout` compares of the operator's checkout.
#[derive(Debug, PartialEq)]
struct CheckoutState {
    head: (String, String),
    index: String,
    status: Vec<String>,
    files: Vec<(&'static str, Vec<u8>)>,
}

mod operator_checkout {
    use super::*;

    #[test]
    fn test_operator_checkout_untouched_by_a_complete_run() {
        // Covers bug class 1 (the operator's own checkout).
        // A gated three-card run (Codex implementers, the second leaving its work for the
        // runner to stage and commit) never touches the operator's checkout: its HEAD, its
        // index (tags included: an assume-unchanged entry stays one), `status --porcelain
        // --ignored` and the content of its tracked files are the same before and after, and
        // none of the operator's files reach the run branch.
        let cards = || {
            [
                card(1),
                card_with(
                    2,
                    CardOpts {
                        route: Some(CODEX_CARD),
                        ..CardOpts::default()
                    },
                ),
                card(3),
            ]
        };
        let mut w = codex_world(
            &cards(),
            Options {
                gates: true,
                ..Options::default()
            },
        );
        let repo = w.repo.clone();
        w.base = commit(
            &repo,
            &[
                (".gitignore", "*.log\n"),
                ("notes/plan.md", "the plan\n"),
                ("config/settings.toml", "level = 1\n"),
            ],
            "operator files",
        );
        w.write_deck(&cards());
        write(&repo, &[("notes/plan.md", "the plan, staged edit\n")]);
        run_git(&repo, &["add", "--", "notes/plan.md"]);
        write(
            &repo,
            &[
                ("README.md", "an unsaved edit\n"),
                ("my-notes.txt", "operator notes\n"),
                ("scratch/todo list.txt", "later\n"),
                ("build.log", "the operator's build log\n"),
            ],
        );
        run_git(
            &repo,
            &["update-index", "--assume-unchanged", "config/settings.toml"],
        );
        write(
            &repo,
            &[("config/settings.toml", "level = 2  # local, hidden\n")],
        );
        let tracked = [
            "README.md",
            "notes/plan.md",
            "config/settings.toml",
            ".gitignore",
        ];

        let state = || CheckoutState {
            head: (
                run_git(&repo, &["symbolic-ref", "HEAD"]),
                run_git(&repo, &["rev-parse", "HEAD"]),
            ),
            index: run_git(&repo, &["ls-files", "-s", "-v"]),
            status: lines(&run_git(&repo, &["status", "--porcelain", "--ignored"])),
            files: tracked
                .map(|p| {
                    let path = repo.join(p);
                    let data = fs::read(&path)
                        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
                    (p, data)
                })
                .to_vec(),
        };

        let before = state();
        assert!(
            lines(&run_git(&repo, &["ls-files", "-v"]))
                .contains(&"h config/settings.toml".to_string())
        );
        w.start();
        // Every run records the untracked files its workspace holds at start (R48'), a
        // worktree run too: a fresh worktree holds none, and the operator's own untracked files
        // are in their checkout, not in it.
        assert_json_eq!(
            read_json(&run_dir(&w).join("untracked-at-start.json")),
            jv(r#"{"paths": []}"#)
        );
        w.script(
            2,
            "implement",
            [Step {
                commit: Some(false),
                ..Step::default()
            }],
        );
        let done = run_to_end(&mut w);
        assert_eq!(done["outcome"], "complete", "{done}");
        let cards_accepted: Vec<Json> = w
            .events("accepted")
            .into_iter()
            .filter(|e| e["card"] != "final")
            .collect();
        assert_json_eq!(field(&cards_accepted, "card"), jv("[1, 2, 3]"));
        let mut after = state();
        // S1-R16: by design (spec contract item 7) the run's worktree and its evidence base
        // worktree live in <repo>/.worktrees/, which start excludes through info/exclude. That
        // one ignored entry is new; it is the only line taken out, and only from the listing
        // after the run.
        assert!(
            after.status.contains(&"!! .worktrees/".to_string()),
            "{:?}",
            after.status
        );
        after.status.retain(|ln| ln != "!! .worktrees/");
        assert_eq!(after, before);
        let landed = lines(&run_git(
            &repo,
            &["diff", "--name-only", &w.base, &format!("refs/heads/{RUN}")],
        ));
        for path in [
            "my-notes.txt",
            "scratch/todo list.txt",
            "build.log",
            "README.md",
            "notes/plan.md",
            "config/settings.toml",
        ] {
            assert!(!landed.contains(&path.to_string()), "{path}: {landed:?}");
        }
        assert!(landed.contains(&"src/card2/c2-a1-implement-1.txt".to_string()));
    }
}

// --------------------------------------------------------------------------
// A violation's saved state: what Python's `test_gitops` checks of preserve_and_restore

/// The absolute path of `name` in the worktree's Git folders (`rev-parse --git-path`).
#[track_caller]
fn git_path(ws: &Path, name: &str) -> PathBuf {
    PathBuf::from(run_git(
        ws,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    ))
}

#[track_caller]
fn ref_exists(repo: &Path, name: &str) -> bool {
    !run_git_unchecked(repo, &["rev-parse", "--verify", "--quiet", name]).is_empty()
}

/// Every commit made with the repository's own settings tries to sign, and signing fails.
#[track_caller]
fn sign_nothing_or_fail(repo: &Path) {
    run_git(repo, &["config", "commit.gpgsign", "true"]);
    run_git(repo, &["config", "gpg.program", "false"]);
}

/// Branch `name` off main in the operator's checkout, one commit per entry of `files`, then
/// back on main; returns the commits.
#[track_caller]
fn side_branch(repo: &Path, name: &str, files: &[&[(&str, &str)]]) -> Vec<String> {
    run_git(repo, &["checkout", "--quiet", "-b", name]);
    let shas = files
        .iter()
        .enumerate()
        .map(|(i, f)| commit(repo, f, &format!("{name} {}", i + 1)))
        .collect();
    run_git(repo, &["checkout", "--quiet", "main"]);
    shas
}

/// The worktree is back on card 1's branch at the expected head, clean, with no operation in
/// progress and no stash; gives the workspace.
macro_rules! assert_back_on_the_card {
    ($w:expr, $v:expr) => {{
        let w: &FakeSupervisor = &$w;
        let v: &Json = &$v;
        let ws = w.workspace();
        assert_eq!(current_branch(&ws).as_deref(), Some(CARD1));
        assert_eq!(
            run_git(&ws, &["rev-parse", "HEAD"]),
            text(&v["expected_head"])
        );
        assert_json_eq!(w.events("accepted")[0]["sha"], v["expected_head"]);
        assert_eq!(
            run_git(&ws, &["status", "--porcelain", "--untracked-files=no"]),
            ""
        );
        for name in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ] {
            assert!(!git_path(&ws, name).exists(), "{name}");
        }
        assert_eq!(run_git(&ws, &["stash", "list"]), ""); // refs/stash untouched
        ws
    }};
}

/// A reviewer leaves card 1's worktree in some state: a moved or deleted branch, a Git
/// operation stopped on a conflict, an autostash, staged work. The violation saves it under
/// refs/leadhand/<run id>/violations/<n> (a stash-style commit: first parent the HEAD it found,
/// second the index) with sibling refs `<ref>-branch` (the card branch's moved tip, when HEAD
/// was elsewhere) and `<ref>-autostash` (an operation's autostash), which the `restored` event
/// lists as `siblings`; then the worktree is back on the card branch at the expected head,
/// clean, with no operation in progress, and the review runs again. Judged from Git.
mod saved_state {
    use super::*;

    /// Card 1's implementer writes `files` (or its default file); the first review tampers.
    /// `before(world)` runs before `start`. Returns the world, the `found` and `restored`
    /// events.
    #[track_caller]
    fn reviewed(
        tamper: impl Fn(&FakeSupervisor, &Path, &Json) + 'static,
        implemented: &[(&str, &str)],
        before: Option<&dyn Fn(&mut FakeSupervisor)>,
    ) -> (FakeSupervisor, Json, Json) {
        let mut w = FakeSupervisor::new(&[card(1)]);
        if let Some(before) = before {
            before(&mut w);
        }
        w.start();
        if !implemented.is_empty() {
            w.script(1, "implement", [files(implemented)]);
        }
        w.script(1, "review", [tampering(tamper), Step::default()]);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        let v = one(found(&w));
        assert_json_eq!(
            pick(&v, &["dispatch_id", "ref"]),
            Json::Arr(vec![
                "c1-a1-review-1".into(),
                format!("refs/leadhand/{}/violations/1", run_id(&w)).into()
            ])
        );
        let done = one(w
            .events("violation")
            .into_iter()
            .filter(|e| e.get("stage").is_some_and(|s| s == "restored"))
            .collect());
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &ref_of(&v)]),
            text(&done["saved"])
        );
        assert_eq!(
            w.dispatch_ids(),
            ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"]
        );
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
        (w, v, done)
    }

    /// `[sibling]` as JSON.
    fn siblings(names: &[String]) -> Json {
        Json::from(names)
    }

    #[test]
    fn test_a_moved_card_tip_is_kept_when_head_is_detached() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            let h = run_git(ws, &["rev-parse", "HEAD"]);
            commit(
                ws,
                &[("src/stray.txt", "stray\n")],
                "reviewer commit on the card branch",
            );
            run_git(ws, &["checkout", "--quiet", "--detach", &h]);
        };
        let (w, v, done) = reviewed(tamper, &[], None);
        let stray = text(&v["refs"][&*format!("refs/heads/{CARD1}")]["new"]).to_string();
        assert!(truthy(&v["head_moved"]), "{v}");
        assert_json_eq!(done["saved"], v["expected_head"]);
        assert_json_eq!(
            done["siblings"],
            siblings(&[format!("{}-branch", ref_of(&v))])
        );
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{}-branch", ref_of(&v))]),
            stray
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{stray}:src/stray.txt")]),
            "stray"
        );
        assert_back_on_the_card!(w, v);
    }

    #[test]
    fn test_a_moved_card_tip_is_kept_when_another_branch_is_checked_out() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            commit(
                ws,
                &[("src/stray.txt", "stray\n")],
                "reviewer commit on the card branch",
            );
            run_git(ws, &["checkout", "--quiet", RUN]);
        };
        let (w, v, done) = reviewed(tamper, &[], None);
        let stray = text(&v["refs"][&*format!("refs/heads/{CARD1}")]["new"]).to_string();
        assert_eq!(done["saved"], w.base.as_str()); // the run branch's head
        assert_json_eq!(
            done["siblings"],
            siblings(&[format!("{}-branch", ref_of(&v))])
        );
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{}-branch", ref_of(&v))]),
            stray
        );
        assert_back_on_the_card!(w, v);
    }

    #[test]
    fn test_a_deleted_card_branch_is_recreated() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["checkout", "--quiet", RUN]);
            run_git(ws, &["branch", "--quiet", "-D", CARD1]);
        };
        let (w, v, done) = reviewed(tamper, &[], None);
        assert!(
            v["refs"][&*format!("refs/heads/{CARD1}")]["new"].is_null(),
            "{v}"
        );
        assert_json_eq!(done["siblings"], jv("[]"));
        assert!(!ref_exists(&w.repo, &format!("{}-branch", ref_of(&v))));
        assert_back_on_the_card!(w, v);
    }

    #[test]
    fn test_a_card_tip_already_in_the_saved_commit_is_not_kept_twice() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            commit(ws, &[("README.md", "committed\n")], "reviewer commit");
            write(ws, &[("README.md", "dirty\n")]);
        };
        let (w, v, done) = reviewed(tamper, &[], None);
        let moved = text(&v["refs"][&*format!("refs/heads/{CARD1}")]["new"]).to_string();
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{}^1", ref_of(&v))]),
            moved
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{}:README.md", ref_of(&v))]),
            "dirty"
        );
        assert_json_eq!(done["siblings"], jv("[]"));
        assert!(!ref_exists(&w.repo, &format!("{}-branch", ref_of(&v))));
        let ws = assert_back_on_the_card!(w, v);
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
    }

    #[test]
    fn test_a_conflicted_merge_is_saved_and_cleared_without_signing() {
        // The saved commit of an unmerged index is leadhand's own commit: never signed.
        let side: RefCell<Option<String>> = RefCell::new(None);
        let make_side = |w: &mut FakeSupervisor| {
            let shas = side_branch(&w.repo, "side", &[&[("README.md", "side\n")]]);
            let [sha] = <[String; 1]>::try_from(shas)
                .unwrap_or_else(|s| panic!("expected one commit, got {s:?}"));
            *side.borrow_mut() = Some(sha);
            sign_nothing_or_fail(&w.repo);
        };
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git_unchecked(ws, &["merge", "--no-edit", "side"]);
            assert!(!run_git(ws, &["ls-files", "-u"]).is_empty()); // an unmerged index
        };
        let (w, v, _done) = reviewed(tamper, &[("README.md", "card\n")], Some(&make_side));
        let side = side.borrow().clone().expect("the side commit");
        let saved = ref_of(&v);
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{saved}^1")]),
            text(&v["expected_head"])
        );
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{saved}^2")]),
            side
        );
        assert!(run_git(&w.repo, &["show", &format!("{saved}:README.md")]).contains("<<<<<<<"));
        let ws = assert_back_on_the_card!(w, v);
        assert_eq!(read_text(&ws.join("README.md")), "card\n");
    }

    #[test]
    fn test_a_conflicted_rebase_or_cherry_pick_is_saved_and_cleared() {
        for operation in ["rebase", "cherry-pick"] {
            let picks: Rc<RefCell<Option<[String; 2]>>> = Rc::default();
            let make_other = |w: &mut FakeSupervisor| {
                let shas = side_branch(
                    &w.repo,
                    "other",
                    &[&[("README.md", "other\n")], &[("src/g.txt", "g\n")]],
                );
                let two = <[String; 2]>::try_from(shas)
                    .unwrap_or_else(|s| panic!("expected two commits, got {s:?}"));
                *picks.borrow_mut() = Some(two);
            };
            let theirs = picks.clone();
            let tamper = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
                if operation == "rebase" {
                    run_git_unchecked(ws, &["rebase", "other"]);
                    assert!(
                        git_path(ws, "rebase-merge").exists()
                            || git_path(ws, "rebase-apply").exists(),
                        "operation={operation}"
                    );
                } else {
                    let [one, two] = theirs.borrow().clone().expect("the picks");
                    run_git_unchecked(ws, &["cherry-pick", &one, &two]);
                    assert!(git_path(ws, "sequencer").exists(), "operation={operation}");
                }
            };
            let (w, v, _done) = reviewed(tamper, &[("README.md", "card\n")], Some(&make_other));
            assert!(
                run_git(&w.repo, &["show", &format!("{}:README.md", ref_of(&v))])
                    .contains("<<<<<<<"),
                "operation={operation}"
            );
            let ws = assert_back_on_the_card!(w, v);
            assert_eq!(
                read_text(&ws.join("README.md")),
                "card\n",
                "operation={operation}"
            );
        }
    }

    #[test]
    fn test_an_operations_autostash_is_kept_beside_the_saved_commit() {
        for operation in ["merge", "rebase"] {
            let make_side = |w: &mut FakeSupervisor| {
                side_branch(&w.repo, "side", &[&[("README.md", "side\n")]]);
            };
            let tamper = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
                write(ws, &[(CARD1_FILE, "dirty\n")]); // the reviewer's edit, autostashed
                if operation == "merge" {
                    run_git_unchecked(ws, &["merge", "--autostash", "--no-edit", "side"]);
                    assert!(ref_exists(ws, "MERGE_AUTOSTASH"), "operation={operation}");
                } else {
                    run_git_unchecked(ws, &["rebase", "--autostash", "side"]);
                    assert!(
                        git_path(ws, "rebase-merge/autostash").exists()
                            || git_path(ws, "rebase-apply/autostash").exists(),
                        "operation={operation}"
                    );
                }
            };
            let (w, v, done) = reviewed(
                tamper,
                &[("README.md", "card\n"), (CARD1_FILE, "card file\n")],
                Some(&make_side),
            );
            assert!(
                run_git(&w.repo, &["show", &format!("{}:README.md", ref_of(&v))])
                    .contains("<<<<<<<"),
                "operation={operation}"
            );
            assert_json_eq!(
                done["siblings"],
                siblings(&[format!("{}-autostash", ref_of(&v))]),
                "operation={operation}"
            );
            assert_eq!(
                run_git(
                    &w.repo,
                    &["show", &format!("{}-autostash:{CARD1_FILE}", ref_of(&v))]
                ),
                "dirty",
                "operation={operation}"
            );
            let ws = assert_back_on_the_card!(w, v);
            assert!(!ref_exists(&ws, "MERGE_AUTOSTASH"), "operation={operation}");
            assert_eq!(
                read_text(&ws.join(CARD1_FILE)),
                "card file\n",
                "operation={operation}"
            );
        }
    }

    #[test]
    fn test_a_staged_change_whose_working_copy_was_put_back_is_kept() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[("README.md", "staged\n")]);
            run_git(ws, &["add", "--", "README.md"]);
            write(ws, &[("README.md", "hello\n")]);
        };
        let (w, v, _done) = reviewed(tamper, &[], None);
        assert!(truthy(&v["index_dirty"]), "{v}");
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{}^2:README.md", ref_of(&v))]),
            "staged"
        );
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{}:README.md", ref_of(&v))]),
            "hello"
        );
        assert_back_on_the_card!(w, v);
    }

    #[test]
    fn test_restore_returns_to_the_card_from_another_branch_and_keeps_untracked_files() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            run_git(ws, &["checkout", "--quiet", "-b", "elsewhere"]);
            commit(ws, &[("src/g.txt", "g\n")], "elsewhere");
            write(
                ws,
                &[("README.md", "staged\n"), ("scratch.log", "keep me\n")],
            );
            run_git(ws, &["add", "--", "README.md"]);
        };
        let (w, v, _done) = reviewed(tamper, &[], None);
        let moved = run_git(&w.repo, &["rev-parse", "refs/heads/elsewhere"]);
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{}^1", ref_of(&v))]),
            moved
        );
        let ws = assert_back_on_the_card!(w, v);
        assert_eq!(run_git(&ws, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(read_text(&ws.join("scratch.log")), "keep me\n");
        assert!(!ws.join("src/g.txt").exists());
    }

    #[test]
    fn test_a_tracked_edit_and_a_deletion_are_both_listed() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[(CARD1_FILE, "edited\n")]);
            fs::remove_file(ws.join("README.md")).expect("removing README.md");
        };
        let (w, v, _done) = reviewed(tamper, &[], None);
        assert_json_eq!(
            pick(&v, &["head_moved", "index_dirty", "paths"]),
            Json::Arr(vec![
                false.into(),
                false.into(),
                Json::from(&["README.md", CARD1_FILE][..])
            ])
        );
        let ws = assert_back_on_the_card!(w, v);
        assert_eq!(read_text(&ws.join("README.md")), "hello\n");
    }

    #[test]
    fn test_paths_with_spaces_and_non_ascii_names() {
        let (spaced, accented) = ("src/my dir/a file.txt", "src/café notes.md");
        let tamper = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[(spaced, "edited\n")]);
        };
        let (w, v, _done) = reviewed(tamper, &[(spaced, "a\n"), (accented, "n\n")], None);
        assert_json_eq!(v["paths"], Json::from(&[spaced][..]));
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{}:{spaced}", ref_of(&v))]),
            "edited"
        );
        let ws = assert_back_on_the_card!(w, v);
        assert_eq!(read_text(&ws.join(spaced)), "a\n");
        let tip = text(&w.events("accepted")[0]["sha"]).to_string();
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{tip}:{accented}")]),
            "n"
        );
    }

    #[test]
    fn test_a_nested_repository_at_a_gitlink_is_not_a_violation() {
        // Leadhand's Git calls ignore submodules (diff.ignoreSubmodules=all,
        // Python's `test_git_safety`): a reviewer who checks out a nested repository at a gitlink
        // path, at another commit than the gitlink records, changes nothing leadhand checks.
        // Plain Git sees the gitlink as a tracked change, so a binary that drops the setting
        // finds a violation here.
        let mut w = FakeSupervisor::new(&[card(1)]);
        let nested = make_repo_named(&w.tmp, "nested repo", "main");
        let recorded = run_git(&nested, &["rev-parse", "HEAD"]);
        run_git(
            &w.repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{recorded},src/sub"),
            ],
        );
        run_git(&w.repo, &["commit", "--quiet", "-m", "a submodule pointer"]);
        w.base = run_git(&w.repo, &["rev-parse", "HEAD"]);
        w.write_deck(&[card(1)]);
        let moved = commit(&nested, &[("README.md", "moved\n")], "moved");
        let seen: Rc<RefCell<HashMap<&'static str, String>>> = Rc::default();
        let record = seen.clone();
        let populate = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            let gitdir = format!(
                "gitdir: {}\n",
                run_git(&nested, &["rev-parse", "--absolute-git-dir"])
            );
            write(
                ws,
                &[("src/sub/.git", &gitdir), ("src/sub/README.md", "moved\n")],
            );
            let mut seen = record.borrow_mut();
            seen.insert("sub", run_git(&ws.join("src/sub"), &["rev-parse", "HEAD"]));
            seen.insert("plain", run_git(ws, &["diff", "--name-only", "HEAD"]));
        };

        w.start();
        w.script(1, "review", [tampering(populate)]);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        // The trap is armed: the nested repository is at another commit, and plain Git sees it.
        let seen = seen.borrow().clone();
        assert_eq!(seen.get("sub").expect("seen[\"sub\"]"), &moved);
        assert_ne!(moved, recorded);
        assert_eq!(seen.get("plain").expect("seen[\"plain\"]"), "src/sub");
        assert_empty!(w.events("violation"));
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"]);
        let run_id = run_id(&w);
        assert_eq!(
            run_git(
                &w.repo,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/leadhand/{run_id}/violations/")
                ]
            ),
            ""
        );
        let accepted = one(w.events("accepted"));
        assert_eq!(
            run_git(
                &w.repo,
                &["rev-parse", &format!("{}:src/sub", text(&accepted["sha"]))]
            ),
            recorded
        );
    }

    #[test]
    fn test_a_stat_only_touch_and_untracked_output_are_not_a_violation() {
        let tamper = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[("build/out.o", "bin\n"), ("new file.txt", "x\n")]);
            set_mtime(&ws.join(CARD1_FILE), SystemTime::now());
        };
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.script(1, "review", [tampering(tamper)]);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        assert_empty!(w.events("violation"));
        assert_eq!(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"]);
    }
}

// --------------------------------------------------------------------------
// The run's worktree and the excludes `start` adds (Python's `test_gitops`)

mod workspace {
    use super::*;

    #[test]
    fn test_start_adds_each_exclude_once_and_the_run_worktree_on_the_run_branch() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        let first = w.start();
        let second = w.start_new();
        for run in [&first, &second] {
            let meta = read_json(&run.join("run.json"));
            let ws = os_path(&meta["workspace"]);
            assert_eq!(
                ws,
                resolve(&w.repo)
                    .join(".worktrees")
                    .join(run.file_name().unwrap()),
                "{}",
                run.display()
            );
            assert_eq!(current_branch(&ws).as_deref(), meta["branch"].as_str());
            assert_eq!(run_git(&ws, &["rev-parse", "HEAD"]), w.base);
        }
        let exclude = lines(&read_text(&w.repo.join(".git/info/exclude")));
        let count = |line: &str| exclude.iter().filter(|l| *l == line).count();
        assert_eq!((count(".worktrees/"), count(".leadhand/")), (1, 1));
        write(&w.repo, &[(".leadhand/state.json", "{}\n")]);
        assert_eq!(run_git(&w.repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn test_the_integration_rebase_is_never_signed() {
        // Card 1 is parked while card 2 is accepted; once unparked it is rebased onto the run
        // tip by leadhand itself, in a repository where every signing attempt fails.
        let mut w = parked_world();
        sign_nothing_or_fail(&w.repo);
        unpark(&w, 1, NOTE);
        assert_eq!(run_to_end(&mut w)["action"], "done");
        let rebased = one(w.events("rebased"));
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[2, 1]"));
        assert_eq!(w.branch_tip(RUN), text(&w.events("accepted")[1]["sha"]));
        assert_eq!(
            run_git(
                &w.repo,
                &["log", "-1", "--format=%G?", text(&rebased["tip"])]
            ),
            "N"
        );
    }
}
