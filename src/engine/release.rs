//! The branch release (new: deliberate change 2, spec "The branch release").
//!
//! A finished run used to keep its branch checked out in its worktree, so `git switch <run
//! branch>` in the operator's own checkout failed. When a run ends `complete`, `incomplete` or
//! `failed` (terminal outcomes, which cannot be relaunched), leadhand detaches the run worktree
//! at the run head, between the `outcome` event and the report:
//!
//! 1. the `outcome` is recorded (`finish::end`);
//! 2. `branch_release_started {worktree, branch, sha}` is recorded;
//! 3. if the worktree is on the run branch at `sha`, `git checkout --detach` runs there;
//! 4. `branch_released` is recorded, with `skipped` and the reason when there was nothing to
//!    detach (the worktree is gone, on another branch, or elsewhere); a worktree already
//!    detached at the run head needs nothing and only the completion is recorded;
//! 5. `git worktree list --porcelain` is read: the branch is free only when no entry,
//!    prunable ones included, has it checked out; otherwise the holder is named with the
//!    command that frees it (for a folder deleted by hand, `git worktree prune`, then
//!    `git switch <branch>`);
//! 6. the report is written (`finish::wrap_up`), its branch line from step 5.
//!
//! The release never prunes worktree records: pruning is repository-wide, and the records it
//! would remove may not be the run's. Blocked runs skip it: they resume on the branch. Steps 2
//! to 4 are a two-phase step, intent and completion appended with `append_event` (the helper
//! `two_phase` appends through), but with no named crash points (ruling S2-R48: S2-R34's points
//! are the Python's, and the release has no Python counterpart; a point after the outcome could
//! not be recovered by `run`, which refuses to reopen a terminal run, as the Python's does).
//! After a crash between the outcome and the report, the next `next` finishes the release from
//! its recorded intent (the detach is idempotent), then writes the report.
//!
//! The only write is the detach, in the run's own worktree, which leaves HEAD at the same
//! commit and the files as they are (bug class 1: never the operator's checkout; bug class 4:
//! the worktree is the one `run.json` names as the run's, on the run's branch at the commit
//! the run recorded). The report's line is what Git shows, not what this did.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::cards::{Log, Run, heads, os, short_sha};
use crate::cli::human::shell_quote;
use crate::error::Result;
use crate::git::Git;
use crate::git::cards::checked_out_branch;
use crate::git::workspace::{WorktreeEntry, head, not_a_run_worktree, worktrees};
use crate::pytext::{path_text, real_path};
use crate::state::events::{self, Outcome};
use crate::state::{Event, append_event};
use crate::wire::PyStr;

/// What Git shows of the run branch after the release (step 5), or that the run skips it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Release {
    /// No worktree has the branch checked out; `worktree` is what Git shows of the run's.
    Free { worktree: RunWorktree },
    /// `holder` still has the branch checked out (`prunable`: Git's record of a worktree whose
    /// folder is gone); `fix` is the commands that free it, in order.
    Held {
        holder: PathBuf,
        prunable: bool,
        fix: Vec<PyStr>,
    },
    /// No worktree has the branch checked out, and it no longer exists.
    Deleted,
    /// The run keeps its branch (a blocked run), for this reason.
    Skipped(PyStr),
}

/// The run's worktree, as `git worktree list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunWorktree {
    /// Detached at this commit.
    Detached(String),
    /// On this branch (another than the run's).
    On(PyStr),
    /// Not a registered worktree.
    Gone,
}

/// The release of the run that ended with `ended` (steps 2 to 5): recorded once per outcome,
/// finished from its intent after a crash, skipped for a blocked run. What Git then shows of
/// the run branch.
pub fn release_branch(run: &Run, log: &Log, ended: &Outcome) -> Result<Release> {
    if ended.outcome.as_deref() == Some("blocked") {
        return Ok(Release::Skipped(
            "a blocked run keeps its branch checked out: it resumes there".into(),
        ));
    }
    release(run, log, ended)?;
    branch_status(&run.repo, &run.ws, &run.branch)
}

/// Steps 2 to 4 of the release, for a terminal outcome: what the wrap-up runs before it
/// writes the report, which reads Git itself (step 5), so a repository Git cannot read
/// degrades the report's line instead of failing the run's end.
pub(super) fn release(run: &Run, log: &Log, ended: &Outcome) -> Result<()> {
    if ended.outcome.as_deref() == Some("blocked") {
        return Ok(());
    }
    let after = |e: &&Event| e.seq() > ended.seq;
    let released = log
        .events
        .iter()
        .filter(after)
        .any(|e| matches!(e, Event::BranchReleased(_)));
    if !released {
        let started = log.events.iter().filter(after).find_map(|e| match e {
            Event::BranchReleaseStarted(s) => Some(s.clone()),
            _ => None,
        });
        let intent = match started {
            // A crash came between the intent and its completion: finish from the intent.
            Some(intent) => intent,
            None => {
                let intent = events::BranchReleaseStarted {
                    worktree: Some(path_text(&run.ws)),
                    branch: Some(run.branch.clone()),
                    sha: ended.head.clone(),
                    ..events::BranchReleaseStarted::default()
                };
                append_event(&run.paths, Event::BranchReleaseStarted(intent.clone()))?;
                intent
            }
        };
        let ws = PathBuf::from(intent.worktree.clone().unwrap_or_default().to_os()?);
        let branch = intent.branch.clone().unwrap_or_default();
        let skipped = detach(&run.repo, &ws, &branch, intent.sha.as_deref());
        append_event(&run.paths, released_event(&intent, skipped.as_ref()))?;
    }
    Ok(())
}

/// `branch_released`, with `skipped` and its reason when there was nothing to detach.
fn released_event(intent: &events::BranchReleaseStarted, skipped: Option<&PyStr>) -> Event {
    Event::BranchReleased(events::BranchReleased {
        worktree: intent.worktree.clone(),
        branch: intent.branch.clone(),
        sha: intent.sha.clone(),
        skipped: skipped.map(|_| Some(true)),
        reason: skipped.map(|why| Some(why.clone())),
        ..events::BranchReleased::default()
    })
}

/// Detaches the worktree `ws` when it is on `branch` at `sha` (step 3); why not, else. Never
/// an error: a worktree Git cannot read, or a detach Git refuses, is a reason to skip, and the
/// report says what Git shows.
///
/// Bug class 1: Git is run in `ws` only once it is a worktree top of its own, listed by the
/// run's repository `repo` and not its main working tree (`git::workspace::not_a_run_worktree`,
/// the check every command makes of the run worktree). A folder whose `.git` is gone would
/// otherwise make Git find the enclosing checkout, the operator's, and detach that.
fn detach(repo: &Path, ws: &Path, branch: &PyStr, sha: Option<&str>) -> Option<PyStr> {
    let why = |text: String| Some(PyStr::from(text));
    if ws.as_os_str().is_empty() {
        return why("run.json names no run worktree".into());
    }
    if !ws.is_dir() {
        return why(format!("the run worktree {} is gone", ws.display()));
    }
    if let Some(found) = not_a_run_worktree(repo, ws) {
        return Some(found.problem);
    }
    let read = |ws: &Path| -> Result<(Git, Option<PyStr>, String)> {
        let git = Git::at(ws)?;
        let on = checked_out_branch(&git)?;
        let at = head(&git, os("HEAD"))?;
        Ok((git, on, at))
    };
    let (git, on, at) = match read(ws) {
        Ok(state) => state,
        Err(e) => return why(format!("git cannot read the run worktree: {}", e.message)),
    };
    let wanted = sha.unwrap_or_default();
    match on {
        Some(b) if b == *branch && at == wanted => {
            match git.run(&[os("checkout"), os("--quiet"), os("--detach")]) {
                Ok(_) => None,
                Err(e) => why(format!("git checkout --detach failed: {}", e.message)),
            }
        }
        Some(b) if b == *branch => why(format!(
            "the run worktree is on {} at {}, not at the run head {}",
            branch.shown(),
            short_sha(&at),
            short_sha(wanted)
        )),
        Some(other) => why(format!("the run worktree is on {}", other.shown())),
        None if at == wanted => None,
        None => why(format!(
            "the run worktree is detached at {}, not at the run head {}",
            short_sha(&at),
            short_sha(wanted)
        )),
    }
}

/// What `git worktree list --porcelain` shows of `branch` (step 5): free when no worktree,
/// prunable ones included, has it checked out; else its holder and the commands that free it.
/// Read in the repository `repo`; nothing is written.
pub fn branch_status(repo: &Path, ws: &Path, branch: &PyStr) -> Result<Release> {
    let git = Git::at(repo)?;
    let target = heads(branch);
    let entries = worktrees(&git)?;
    let field = |e: &WorktreeEntry, key: &[u8]| e.get(key).cloned();
    let mut switch = OsString::from("git switch ");
    switch.push(shell_quote(&branch.to_os()?));
    let switch = PyStr::from_os(&switch);
    if let Some(e) = entries
        .iter()
        .find(|e| field(e, b"branch").as_deref() == Some(target.as_wtf8()))
    {
        let holder = path_of(&field(e, b"worktree").unwrap_or_default());
        let prunable = e.contains_key(&b"prunable"[..]);
        let fix = if prunable {
            vec![PyStr::from("git worktree prune"), switch]
        } else {
            let mut detach = OsString::from("git -C ");
            detach.push(shell_quote(holder.as_os_str()));
            detach.push(" switch --detach");
            vec![PyStr::from_os(&detach), switch]
        };
        return Ok(Release::Held {
            holder,
            prunable,
            fix,
        });
    }
    if !git.ok(&[
        os("rev-parse"),
        os("--verify"),
        os("--quiet"),
        &target.to_os()?,
    ]) {
        // Nothing to switch to: Git would refuse `git switch`.
        return Ok(Release::Deleted);
    }
    // An empty path names no worktree: `real_path("")` would be the current directory's.
    let real = (!ws.as_os_str().is_empty()).then(|| real_path(ws));
    let mine = entries
        .iter()
        .find(|e| field(e, b"worktree").is_some_and(|p| Some(real_path(&path_of(&p))) == real));
    let worktree = match mine {
        None => RunWorktree::Gone,
        Some(e) => match field(e, b"branch") {
            Some(b) => RunWorktree::On(PyStr::from_os(OsStr::from_bytes(
                b.strip_prefix(b"refs/heads/").unwrap_or(&b),
            ))),
            None => RunWorktree::Detached(
                String::from_utf8_lossy(&field(e, b"HEAD").unwrap_or_default()).into_owned(),
            ),
        },
    };
    Ok(Release::Free { worktree })
}

fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// The report's branch line (in its Result section) for `status`, what Git showed of the run
/// branch; `None` when the run skips the release (a blocked run). A status Git could not give
/// degrades to a line that says so: the report never fails on it.
pub fn report_line(branch: &PyStr, ws: &Path, status: &Result<Release>) -> Option<PyStr> {
    let mut line = PyStr::from("- Branch `");
    line.push(branch);
    line.push_str("`");
    match status {
        Ok(Release::Skipped(_)) => return None,
        Ok(Release::Deleted) => {
            line.push_str(" no longer exists: there is nothing to check out");
        }
        Ok(Release::Free { worktree }) => {
            line.push_str(" is free: check it out anywhere with `git switch ");
            line.push(&PyStr::from_os(&shell_quote(&branch.to_os_lossy())));
            if ws.as_os_str().is_empty() {
                line.push_str("` (run.json names no run worktree)");
                return Some(line);
            }
            line.push_str("` (the run worktree ");
            line.push(&path_text(ws));
            match worktree {
                RunWorktree::Detached(sha) => {
                    line.push_str(&format!(" is detached at {})", short_sha(sha)));
                }
                RunWorktree::On(other) => {
                    line.push_str(" is on ");
                    line.push(other);
                    line.push_str(")");
                }
                RunWorktree::Gone => line.push_str(" is no longer a worktree)"),
            }
        }
        Ok(Release::Held {
            holder,
            prunable,
            fix,
        }) => {
            if *prunable {
                line.push_str(" is still checked out in the worktree ");
                line.push(&path_text(holder));
                line.push_str(", whose folder is gone; Git keeps its record until it is pruned");
            } else {
                line.push_str(" is still checked out in the worktree ");
                line.push(&path_text(holder));
            }
            line.push_str(": free it with ");
            for (i, command) in fix.iter().enumerate() {
                if i > 0 {
                    line.push_str(", then ");
                }
                line.push_str("`");
                line.push(command);
                line.push_str("`");
            }
        }
        Err(e) => {
            line.push_str(&format!(
                ": Git could not say whether it is free ({})",
                first_line(&e.message)
            ));
        }
    }
    Some(line)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::state::{append_event, read_events, run_paths};
    use crate::wire::PyMap;

    const BRANCH: &str = "leadhand/demo";
    const DECK: &str = "# Deck: Demo deck\nSource: docs/plan.md\nBase: main @ BASE\n\
        Branch: leadhand/demo\nBuild: true\nTest: true\n\n\
        ## Card 1: Card number 1\n- Role: implementer\n- Hard: no\n- Risk: low\n\
        - Depends on: none\n- Allowed paths: src/, tests/\n\
        - Done when: the card's file exists and the tests pass\n- Stop if: the design has to change\n";

    /// A run whose worktree `<repo>/.worktrees/demo` is on the run branch at the base.
    fn run(f: &Fixture) -> (Run, String) {
        let repo = f.make_repo("repo");
        let ws = repo.join(".worktrees").join("demo");
        f.git(
            &repo,
            &["worktree", "add", "-q", "-b", BRANCH, ws.to_str().unwrap()],
        );
        let sha = f.git(&repo, &["rev-parse", "HEAD"]);
        let root = f.root().join("runs").join("demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("deck.md"), DECK.replace("BASE", &sha)).unwrap();
        let paths = run_paths(&root);
        let user = f.root().join("user.toml");
        std::fs::write(&user, "").unwrap();
        let run = Run {
            deck: crate::deck::parse_deck(&paths.deck).unwrap(),
            config: crate::config::load_config_from(&repo, &user).unwrap(),
            root,
            paths,
            meta: PyMap::default(),
            run_id: Some("abc123abc123".into()),
            repo,
            ws,
            branch: BRANCH.into(),
            base_sha: sha.clone(),
            worktree: Default::default(),
        };
        (run, sha)
    }

    fn outcome(run: &Run, outcome: &str, head: &str) -> Outcome {
        let (e, _) = append_event(
            &run.paths,
            Event::Outcome(Outcome {
                outcome: Some(outcome.into()),
                qualifier: Some(String::new()),
                reasons: Some(Vec::new()),
                next_step: Some("none".into()),
                head: Some(head.into()),
                ..Outcome::default()
            }),
        )
        .unwrap();
        let Event::Outcome(o) = e else { unreachable!() };
        o
    }

    fn kinds(run: &Run) -> Vec<&'static str> {
        read_events(&run.paths)
            .unwrap()
            .into_iter()
            .map(|(e, _)| e.kind())
            .collect()
    }

    #[test]
    fn a_terminal_run_detaches_its_worktree_once_and_its_branch_is_free() {
        let f = Fixture::new();
        let (run, sha) = run(&f);
        let ended = outcome(&run, "complete", &sha);
        let log = Log::read(&run.paths).unwrap();
        let status = release_branch(&run, &log, &ended).unwrap();
        assert_eq!(
            status,
            Release::Free {
                worktree: RunWorktree::Detached(sha.clone())
            }
        );
        assert_eq!(
            kinds(&run),
            ["outcome", "branch_release_started", "branch_released"]
        );
        let events = read_events(&run.paths).unwrap();
        let Event::BranchReleased(done) = &events[2].0 else {
            panic!()
        };
        assert_eq!((done.skipped, done.reason.clone()), (None, None));
        assert_eq!(f.git(&run.ws, &["rev-parse", "HEAD"]), sha);
        assert!(f.git(&run.ws, &["status", "--porcelain"]).is_empty());
        // Done once per outcome.
        let log = Log::read(&run.paths).unwrap();
        release_branch(&run, &log, &ended).unwrap();
        assert_eq!(kinds(&run).len(), 3);
        let line = report_line(&run.branch, &run.ws, &Ok(status)).unwrap();
        assert_eq!(
            line.lossy(),
            format!(
                "- Branch `leadhand/demo` is free: check it out anywhere with `git switch \
                 leadhand/demo` (the run worktree {} is detached at {})",
                run.ws.display(),
                &sha[..12]
            )
        );
    }

    #[test]
    fn a_blocked_run_keeps_its_branch() {
        let f = Fixture::new();
        let (run, sha) = run(&f);
        let ended = outcome(&run, "blocked", &sha);
        let log = Log::read(&run.paths).unwrap();
        let status = release_branch(&run, &log, &ended).unwrap();
        assert!(matches!(status, Release::Skipped(_)));
        assert_eq!(kinds(&run), ["outcome"]);
        assert_eq!(f.git(&run.ws, &["symbolic-ref", "--short", "HEAD"]), BRANCH);
        assert_eq!(report_line(&run.branch, &run.ws, &Ok(status)), None);
    }

    #[test]
    fn a_release_interrupted_after_its_intent_is_finished_from_it() {
        let f = Fixture::new();
        let (run, sha) = run(&f);
        let ended = outcome(&run, "incomplete", &sha);
        append_event(
            &run.paths,
            Event::BranchReleaseStarted(events::BranchReleaseStarted {
                worktree: Some(path_text(&run.ws)),
                branch: Some(run.branch.clone()),
                sha: Some(sha.clone()),
                ..events::BranchReleaseStarted::default()
            }),
        )
        .unwrap();
        let log = Log::read(&run.paths).unwrap();
        release_branch(&run, &log, &ended).unwrap();
        assert_eq!(
            kinds(&run),
            ["outcome", "branch_release_started", "branch_released"]
        );
        assert!(
            !Git::at(&run.ws)
                .unwrap()
                .ok(&[os("symbolic-ref"), os("--quiet"), os("HEAD")])
        );
    }

    #[test]
    fn a_worktree_elsewhere_is_skipped_with_its_reason() {
        let f = Fixture::new();
        let (run, sha) = run(&f);
        f.git(&run.ws, &["switch", "-q", "-c", "other"]);
        assert_eq!(
            detach(&run.repo, &run.ws, &run.branch, Some(&sha)).map(|w| w.lossy().into_owned()),
            Some("the run worktree is on other".to_string())
        );
        f.git(&run.ws, &["switch", "-q", "--detach"]);
        assert_eq!(detach(&run.repo, &run.ws, &run.branch, Some(&sha)), None);
        let gone = f.root().join("gone");
        assert_eq!(
            detach(&run.repo, &gone, &run.branch, Some(&sha)).map(|w| w.lossy().into_owned()),
            Some(format!("the run worktree {} is gone", gone.display()))
        );
    }

    #[test]
    fn a_deleted_worktree_folder_still_holds_the_branch_until_pruned() {
        let f = Fixture::new();
        let (run, _) = run(&f);
        std::fs::remove_dir_all(&run.ws).unwrap();
        let status = branch_status(&run.repo, &run.ws, &run.branch).unwrap();
        let Release::Held {
            holder,
            prunable,
            fix,
        } = &status
        else {
            panic!("{status:?}");
        };
        assert_eq!((holder, *prunable), (&run.ws, true));
        let fix: Vec<String> = fix.iter().map(|c| c.lossy().into_owned()).collect();
        assert_eq!(fix, ["git worktree prune", "git switch leadhand/demo"]);
        let line = report_line(&run.branch, &run.ws, &Ok(status)).unwrap();
        assert!(
            line.lossy()
                .ends_with(": free it with `git worktree prune`, then `git switch leadhand/demo`")
        );
        // Nothing was pruned: Git still has the record.
        assert!(
            f.git(&run.repo, &["worktree", "list", "--porcelain"])
                .contains(&format!("branch refs/heads/{BRANCH}"))
        );
        // A worktree folder still there is freed by detaching it.
        let other = f.root().join("other wt");
        f.git(
            &run.repo,
            &["worktree", "add", "-q", "-b", "x", other.to_str().unwrap()],
        );
        let x = PyStr::from("x");
        let Release::Held { fix, prunable, .. } = branch_status(&run.repo, &run.ws, &x).unwrap()
        else {
            panic!()
        };
        assert!(!prunable);
        assert_eq!(
            fix[0].lossy(),
            format!("git -C '{}' switch --detach", other.display())
        );
    }

    // Bug class 1: a worktree folder whose `.git` is gone makes Git find the enclosing
    // checkout, the operator's. The release must not detach that, even on the run branch at the
    // run head.
    #[test]
    fn a_folder_that_is_no_longer_a_worktree_is_never_detached() {
        let f = Fixture::new();
        let (run, sha) = run(&f);
        std::fs::remove_file(run.ws.join(".git")).unwrap();
        f.git(&run.repo, &["worktree", "prune"]);
        f.git(&run.repo, &["switch", "-q", BRANCH]);
        let ended = outcome(&run, "complete", &sha);
        let log = Log::read(&run.paths).unwrap();
        release(&run, &log, &ended).unwrap();
        let events = read_events(&run.paths).unwrap();
        let Event::BranchReleased(done) = &events.last().unwrap().0 else {
            panic!("{events:?}")
        };
        assert_eq!(done.skipped, Some(Some(true)));
        let reason = done.reason.clone().flatten().unwrap().lossy().into_owned();
        assert!(reason.contains("is not a worktree top"), "{reason}");
        // The operator's checkout is where it was: on the branch, at the same commit.
        assert_eq!(
            f.git(&run.repo, &["symbolic-ref", "--short", "HEAD"]),
            BRANCH
        );
        assert_eq!(f.git(&run.repo, &["rev-parse", "HEAD"]), sha);
        // The main working tree itself is never the run worktree either.
        let why = not_a_run_worktree(&run.repo, &run.repo).unwrap().problem;
        assert!(why.contains("main working tree"), "{why:?}");
    }

    #[test]
    fn a_deleted_branch_is_not_free_to_switch_to() {
        let f = Fixture::new();
        let (run, _) = run(&f);
        f.git(&run.ws, &["switch", "-q", "--detach"]);
        f.git(
            &run.repo,
            &["update-ref", "-d", &format!("refs/heads/{BRANCH}")],
        );
        let status = branch_status(&run.repo, &run.ws, &run.branch).unwrap();
        assert_eq!(status, Release::Deleted);
        let line = report_line(&run.branch, &run.ws, &Ok(status)).unwrap();
        assert_eq!(
            line.lossy(),
            "- Branch `leadhand/demo` no longer exists: there is nothing to check out"
        );
    }

    #[test]
    fn an_empty_worktree_path_names_no_worktree() {
        let f = Fixture::new();
        let (run, sha) = run(&f);
        f.git(&run.ws, &["switch", "-q", "--detach"]);
        let empty = Path::new("");
        let status = branch_status(&run.repo, empty, &run.branch).unwrap();
        assert_eq!(
            status,
            Release::Free {
                worktree: RunWorktree::Gone
            }
        );
        let line = report_line(&run.branch, empty, &Ok(status)).unwrap();
        assert!(
            line.lossy()
                .ends_with("`git switch leadhand/demo` (run.json names no run worktree)"),
            "{}",
            line.lossy()
        );
        assert_eq!(
            detach(&run.repo, empty, &run.branch, Some(&sha)).map(|w| w.lossy().into_owned()),
            Some("run.json names no run worktree".to_string())
        );
    }

    #[test]
    fn a_repository_git_cannot_read_degrades_the_line() {
        let f = Fixture::new();
        let status = branch_status(&f.root().join("no repo"), Path::new("/ws"), &BRANCH.into());
        let line = report_line(&BRANCH.into(), Path::new("/ws"), &status).unwrap();
        assert!(
            line.lossy()
                .starts_with("- Branch `leadhand/demo`: Git could not say whether it is free ("),
            "{}",
            line.lossy()
        );
    }
}
