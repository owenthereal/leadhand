//! The run worktree, recognised by identity before leadhand runs Git there (ruling S2-R54;
//! spec, bug classes 1 and 4). Rust-only.
//!
//! The Rust form of the Python contract suite's `contract_rust_worktree_identity`. The run worktree
//! `<repo>/.worktrees/<slug>` lies inside the operator's checkout. Once its `.git` file is gone,
//! Git run there finds the operator's repository, so every write meant for the workspace would
//! land in the operator's checkout. Bug class 1: leadhand never checks out, resets, cleans,
//! stashes or stages in the operator's working tree. Bug class 4: the run worktree is
//! recognised by identity, so a folder is acted on as the run's worktree only while Git's top
//! there is the folder itself and the run's repository lists it as a linked worktree.
//!
//! Otherwise the command refuses before it writes anything: exit 1, `error:` naming the
//! workspace and the top Git found, and the one command that fixes it alone on the last line.
//! While the repository still lists the worktree, that is `git -C <repo> worktree repair`,
//! which restores the `.git` file. The scenario is the final review's probe. The Python
//! reference acted on the operator's checkout there, so every test here was Rust-only
//! (`rust_only`).

#[macro_use]
mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use common::git::run_git;
use common::json::Json;
use common::launch_world::World;
use common::supervisor::{FakeSupervisor, card, run_dir, text};
use common::{
    TIMEOUT, lines, os_vec, read_text, realpath, run_argv, shlex_join, shlex_split, write_text,
};

/// What the operator sees of their checkout: branch, HEAD, index (with its flags), status and
/// the tracked file they edited.
#[derive(Debug, PartialEq)]
struct Checkout {
    branch: String,
    head: String,
    index: String,
    flags: String,
    status: String,
    file: String,
}

#[track_caller]
fn checkout(repo: &Path, tracked: &str) -> Checkout {
    Checkout {
        branch: run_git(repo, &["symbolic-ref", "HEAD"]),
        head: run_git(repo, &["rev-parse", "HEAD"]),
        index: run_git(repo, &["ls-files", "--stage"]),
        flags: run_git(repo, &["ls-files", "-v"]),
        status: run_git(repo, &["status", "--porcelain", "--untracked-files=all"]),
        file: read_text(&repo.join(tracked)),
    }
}

fn refusal_head(ws: &Path, repo: &Path) -> String {
    format!(
        "error: the run worktree {} is not a worktree top (Git's top there is {}); leadhand \
         will not run Git there. ",
        ws.display(),
        realpath(repo).display()
    )
}

/// The repair line: `git -C <repo, resolved> worktree repair`, as `shlex.join` quotes it.
fn repair_line(repo: &Path) -> String {
    let repo = realpath(repo);
    shlex_join(&os_vec(&[
        "git",
        "-C",
        repo.to_str().unwrap(),
        "worktree",
        "repair",
    ]))
}

/// The run's worktree with its `.git` file gone, and an uncommitted edit of the operator's to a
/// tracked file in their own checkout: the workspace, that file, and the checkout as it is now.
#[track_caller]
fn broken(w: &FakeSupervisor) -> (PathBuf, String, Checkout) {
    let ws = w.workspace();
    let tracked = lines(&run_git(&w.repo, &["ls-files"]))
        .into_iter()
        .next()
        .expect("a tracked file");
    write_text(&w.repo.join(&tracked), "the operator's uncommitted edit\n");
    fs::remove_file(ws.join(".git")).unwrap_or_else(|e| panic!("removing the .git file: {e}"));
    let before = checkout(&w.repo, &tracked);
    (ws, tracked, before)
}

mod missing_git_file {
    use super::*;

    #[test]
    fn test_next_refuses_and_leaves_the_operators_checkout_untouched() {
        if common::rust_only() {
            return;
        }
        // Bug classes 1 and 4: the review's probe. Before the fix, `next` switched the
        // operator's checkout to the card branch and moved the edit onto a leftovers ref.
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        w.start();
        let run_id = text(&w.meta()["run_id"]).to_string();
        let (ws, tracked, before) = broken(&w);
        let events = w.events(None);

        let r = w.cli(&os_args!["next", run_dir(&w)]);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
        let lines = lines(&r.stderr_text());
        assert_eq!(lines.len(), 2, "{r}");
        assert_eq!(
            lines[0],
            refusal_head(&ws, &w.repo) + "Restore its .git file with:",
            "{r}"
        );
        assert_eq!(lines[1], repair_line(&w.repo), "{r}");

        // Nothing was written: the operator's checkout, their edit included, is as it was, no
        // leftovers ref holds it, and no event was appended (no `violation` among them).
        assert_eq!(checkout(&w.repo, &tracked), before);
        assert_eq!(
            run_git(
                &w.repo,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    &format!("refs/leadhand/{run_id}/")
                ]
            ),
            ""
        );
        assert_json_eq!(Json::Arr(w.events(None)), Json::Arr(events));
        assert_empty!(w.events("violation"));

        // The fix line is true: once it has run, the worktree is the run's again and `next`
        // carries on there, still leaving the operator's checkout alone.
        let argv = shlex_split(&lines[1]).expect("a shell line");
        let argv: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
        let fix = run_argv(&argv, &w.env, &w.tmp, None, TIMEOUT);
        if fix.code != 0 {
            panic!("the repair line failed (check=True)\n{fix}");
        }
        assert_eq!(
            run_git(&ws, &["rev-parse", "--show-toplevel"]),
            realpath(&ws).to_str().unwrap()
        );
        assert_eq!(w.next()["action"], "dispatch");
        assert_eq!(checkout(&w.repo, &tracked), before);
    }

    #[test]
    fn test_next_names_a_new_run_once_the_worktree_record_is_pruned() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let (ws, tracked, _) = broken(&w);
        run_git(&w.repo, &["worktree", "prune"]);
        let before = checkout(&w.repo, &tracked);
        let events = w.events(None);

        let r = w.cli(&os_args!["next", run_dir(&w)]);
        assert_eq!(r.code, 1, "{r}");
        let lines = lines(&r.stderr_text());
        assert_eq!(
            lines[0],
            refusal_head(&ws, &w.repo) + "Start a new run of the deck with:",
            "{r}"
        );
        // Alone on the last line: the command that starts a new run of the same deck.
        let argv = shlex_split(&lines[1]).expect("a shell line");
        let meta = w.meta();
        let want = [
            "start",
            text(&meta["deck_source"]),
            "--repo",
            text(&meta["repo"]),
        ];
        assert_eq!(argv[1..], want, "{r}");
        assert_eq!(checkout(&w.repo, &tracked), before);
        assert_json_eq!(Json::Arr(w.events(None)), Json::Arr(events));
    }
}

mod run_refuses {
    use super::*;

    #[test]
    fn test_run_refuses_before_the_supervisor_starts() {
        if common::rust_only() {
            return;
        }
        // `run` would start the supervisor in the folder, where every command it runs acts.
        let w = World::new();
        let (ws, tracked, before) = broken(&w.sup);
        let events = w.sup.events(None);

        let r = w.run(&[]);
        assert_eq!(r.code, 1, "{r}");
        let lines = lines(&r.stderr_text());
        assert_eq!(
            lines[0],
            refusal_head(&ws, &w.repo) + "Restore its .git file with:",
            "{r}"
        );
        assert_eq!(
            lines.last().expect("a last line"),
            &repair_line(&w.repo),
            "{r}"
        );
        assert!(!w.log.exists(), "the supervisor started");
        assert_json_eq!(Json::Arr(w.sup.events(None)), Json::Arr(events));
        assert_eq!(checkout(&w.repo, &tracked), before);
    }
}
