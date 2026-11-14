//! Card branches and acceptance (`gitops`: `card_branch_name`, `cut_card_branch`,
//! `fast_forward`, `changed_paths`, `is_ancestor`, `is_test_or_doc_path`), and the reads of the
//! run's workspace the card loop makes before it moves anything (`engine._current_branch`,
//! `_operation`, `_ref_sha`, `_ref_snapshot`; `gitops.detect_violation`).
//!
//! Runs happen only in worktrees (deliberate change 1), so nothing here protects ignored files:
//! the Python's `protect_ignored` is always false for a worktree run. Branch and ref names are
//! text as the Python holds them (`PyStr`), given to Git as their OS bytes.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::workspace::{head, ref_exists, worktrees};
use super::{Git, split_nul, unhide_index_entries};
use crate::error::{Error, Result};
use crate::pytext::py_splitlines_bytes;
use crate::wire::{PyMap, PyStr};

/// Between a run branch and a card number in a card branch's name (`gitops.CARD_INFIX`).
pub const CARD_INFIX: &str = "--card-";

/// The Git operations a workspace can be left in, by the file that marks each
/// (`engine.OPERATIONS`).
pub const OPERATIONS: [(&str, &str); 6] = [
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    ("sequencer", "sequencer"),
];

fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}

/// Git's output without its trailing newlines (`gitops.git`'s `rstrip("\n")`).
fn trim_newlines(out: &[u8]) -> &[u8] {
    let end = out.iter().rposition(|b| *b != b'\n').map_or(0, |i| i + 1);
    &out[..end]
}

/// Text Git printed, as the Python decodes it (`surrogateescape`).
fn text(bytes: &[u8]) -> PyStr {
    PyStr::from_os(OsStr::from_bytes(bytes))
}

/// `prefix` then `name`'s OS bytes: a ref or argument built from carried text.
fn joined(prefix: &str, name: &PyStr) -> Result<OsString> {
    let mut out = OsString::from(prefix);
    out.push(name.to_os()?);
    Ok(out)
}

/// `refs/heads/<branch>`.
pub fn branch_ref(branch: &PyStr) -> Result<OsString> {
    joined("refs/heads/", branch)
}

/// A command whose exit status answers yes (0) or no (1); any other exit is an error
/// (`gitops._test`).
fn yes_or_no(git: &Git, argv: &[&OsStr]) -> Result<bool> {
    let out = git.output(argv)?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git.failure(argv, &out)),
    }
}

/// `<run branch>--card-<card>` (`gitops.card_branch_name`).
pub fn card_branch_name(run_branch: &PyStr, card: &str) -> PyStr {
    let mut out = run_branch.clone();
    out.push_str(CARD_INFIX);
    out.push_str(card);
    out
}

/// Refuses a name Git does not take as a branch, or one that would read as an option
/// (`gitops._check_branch_name`).
fn check_branch_name(git: &Git, name: &PyStr) -> Result<()> {
    let refused = || Error::msg(format!("not a valid branch name: {}", name.repr()));
    let Ok(os_name) = name.to_os() else {
        return Err(refused());
    };
    if name.starts_with("-")
        || !git
            .output(&[os("check-ref-format"), os("--branch"), &os_name])?
            .status
            .success()
    {
        return Err(refused());
    }
    Ok(())
}

/// The branch checked out in `git`'s directory, as `gitops._current_branch` reads it: `None`
/// when HEAD is detached or not on a `refs/heads/` branch.
pub fn checked_out_branch(git: &Git) -> Result<Option<PyStr>> {
    let argv = [os("symbolic-ref"), os("--quiet"), os("HEAD")];
    let out = git.output(&argv)?;
    match out.status.code() {
        Some(0) => {}
        Some(1) => return Ok(None),
        _ => return Err(git.failure(&argv, &out)),
    }
    let name = out.stdout.trim_ascii();
    Ok(name.strip_prefix(b"refs/heads/").map(text))
}

/// The branch the workspace is on, as the engine reads it (`engine._current_branch`:
/// `symbolic-ref --quiet --short HEAD`); `None` when HEAD is detached, or Git fails.
pub fn current_branch(git: &Git) -> Option<PyStr> {
    let out = git
        .run(&[os("symbolic-ref"), os("--quiet"), os("--short"), os("HEAD")])
        .ok()?;
    Some(text(trim_newlines(&out)))
}

/// Checks out the card branch, creating it at the run tip when missing; the branch and its
/// base, which for a branch that already exists is its merge base with the run branch
/// (`gitops.cut_card_branch` in a worktree run).
///
/// `recorded` says whether the run's events record cutting this branch: an existing branch
/// they do not record is not the run's and is refused (R54'), unless it is exactly at the run
/// tip (a cut whose record a crash lost: it holds nothing of its own).
pub fn cut_card_branch(
    git: &Git,
    run_branch: &PyStr,
    card: &str,
    recorded: bool,
) -> Result<(PyStr, String)> {
    let branch = card_branch_name(run_branch, card);
    check_branch_name(git, &branch)?;
    let card_ref = branch_ref(&branch)?;
    let run_ref = branch_ref(run_branch)?;
    let base = if ref_exists(git, &card_ref)? {
        if !recorded && head(git, &card_ref)? != head(git, &run_ref)? {
            return Err(Error::msg(format!(
                "the card branch {} already exists but this run did not cut it; rename or delete \
                 it, or start a new run",
                branch.shown()
            )));
        }
        merge_base(git, &run_ref, &card_ref)?
    } else {
        let base = head(git, &run_ref)?;
        // Empty old value: create only.
        git.run(&[
            os("update-ref"),
            os("-m"),
            os("leadhand: cut card branch"),
            &card_ref,
            os(&base),
            os(""),
        ])?;
        base
    };
    if checked_out_branch(git)?.as_ref() != Some(&branch) {
        git.run(&[os("checkout"), os("--quiet"), &branch.to_os()?, os("--")])?;
    }
    Ok((branch, base))
}

/// `git merge-base a b`.
pub fn merge_base(git: &Git, a: &OsStr, b: &OsStr) -> Result<String> {
    let out = git.run(&[os("merge-base"), a, b])?;
    Ok(String::from_utf8_lossy(trim_newlines(&out)).into_owned())
}

/// Whether commit `ancestor` is in the history of `descendant`, or is it
/// (`gitops.is_ancestor`).
pub fn is_ancestor(git: &Git, ancestor: &OsStr, descendant: &OsStr) -> Result<bool> {
    yes_or_no(
        git,
        &[os("merge-base"), os("--is-ancestor"), ancestor, descendant],
    )
}

/// Moves the run branch forward to `to_sha` and returns the run tip (`gitops.fast_forward`).
/// Idempotent: a run branch that already contains `to_sha` is left where it is. An error when
/// it is not a fast-forward, or when the run branch is checked out in another worktree.
pub fn fast_forward(git: &Git, run_branch: &PyStr, to_sha: &str) -> Result<String> {
    let run_ref = branch_ref(run_branch)?;
    let target = head(git, os(to_sha))?;
    let current = head(git, &run_ref)?;
    if current == target || is_ancestor(git, os(&target), os(&current))? {
        return Ok(current);
    }
    if !is_ancestor(git, os(&current), os(&target))? {
        return Err(Error::msg(format!(
            "not a fast-forward: {} is at {current}, which {target} does not contain",
            run_branch.shown()
        )));
    }
    if checked_out_branch(git)?.as_ref() == Some(run_branch) {
        git.run(&[os("merge"), os("--quiet"), os("--ff-only"), os(&target)])?;
        return head(git, &run_ref);
    }
    for w in worktrees(git)? {
        if w.get(&b"branch"[..]).map(Vec::as_slice) == Some(run_ref.as_bytes()) {
            let held = w.get(&b"worktree"[..]).cloned().unwrap_or_default();
            return Err(Error::msg(format!(
                "{} is checked out in {}; cannot move it from {}",
                run_branch.shown(),
                String::from_utf8_lossy(&held),
                git.cwd().display()
            )));
        }
    }
    git.run(&[
        os("update-ref"),
        os("-m"),
        os("leadhand: fast-forward"),
        &run_ref,
        os(&target),
        os(&current),
    ])?;
    Ok(target)
}

/// The paths that differ between commits `a` and `b`, renames as a delete and an add
/// (`gitops.changed_paths`).
pub fn changed_paths(git: &Git, a: &str, b: &str) -> Result<Vec<PyStr>> {
    let out = git.run(&[
        os("diff"),
        os("--name-only"),
        os("-z"),
        os("--no-renames"),
        os("--no-ext-diff"),
        os(a),
        os(b),
        os("--"),
    ])?;
    Ok(split_nul(trim_newlines(&out))
        .iter()
        .map(|p| text(p))
        .collect())
}

/// The patch between two commits, as a test-only fix's diff check shows it
/// (`git diff --no-ext-diff --no-color a b --`, trailing newlines dropped).
pub fn diff_text(git: &Git, a: &str, b: &str) -> Result<Vec<u8>> {
    let out = git.run(&[
        os("diff"),
        os("--no-ext-diff"),
        os("--no-color"),
        os(a),
        os(b),
        os("--"),
    ])?;
    Ok(trim_newlines(&out).to_vec())
}

/// The commit a ref names, or `None` when it names none or Git fails (`engine._ref_sha`).
pub fn ref_sha(git: &Git, name: &OsStr) -> Option<String> {
    let mut spec = name.to_os_string();
    spec.push("^{commit}");
    let out = git
        .run(&[os("rev-parse"), os("--verify"), os("--quiet"), &spec])
        .ok()?;
    let sha = String::from_utf8_lossy(trim_newlines(&out)).into_owned();
    (!sha.is_empty()).then_some(sha)
}

/// The Git operation left in progress in the workspace (rebase, merge, …), if any
/// (`engine._operation`).
pub fn operation(git: &Git) -> Result<Option<&'static str>> {
    let mut argv = vec![os("rev-parse"), os("--path-format=absolute")];
    for (name, _) in OPERATIONS {
        argv.extend([os("--git-path"), os(name)]);
    }
    let out = git.run(&argv)?;
    let found = py_splitlines_bytes(trim_newlines(&out));
    for ((_, op), path) in OPERATIONS.iter().zip(found) {
        if Path::new(OsStr::from_bytes(path)).exists() {
            return Ok(Some(op));
        }
    }
    Ok(None)
}

/// ref → object for the run branch, its card branches and the run's `refs/leadhand/<run_id>/`
/// (`engine._ref_snapshot`, R14').
pub fn ref_snapshot(git: &Git, run_branch: &PyStr, run_id: &str) -> Result<PyMap<String>> {
    let out = git.run(&[
        os("for-each-ref"),
        os("--format=%(objectname) %(refname)"),
        &branch_ref(run_branch)?,
        &joined("refs/heads/", &{
            let mut cards = run_branch.clone();
            cards.push_str("--card-*");
            cards
        })?,
        os(&format!("refs/leadhand/{run_id}/")),
    ])?;
    let mut snap = PyMap::default();
    for line in py_splitlines_bytes(trim_newlines(&out)) {
        let (sha, name) = match line.iter().position(|b| *b == b' ') {
            Some(at) => (&line[..at], &line[at + 1..]),
            None => (line, &b""[..]),
        };
        if !name.is_empty() {
            *snap.entry(&text(name), String::new) = String::from_utf8_lossy(sha).into_owned();
        }
    }
    Ok(snap)
}

/// How the workspace differs from a branch tip it must be on (`gitops.Violation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// HEAD is not `expected_head` on `expected_branch`.
    pub head_moved: bool,
    /// The index differs from HEAD.
    pub index_dirty: bool,
    /// Tracked files whose working copy differs from the index.
    pub tracked_changes: Vec<PyStr>,
    /// Entries found assume-unchanged or skip-worktree (now cleared).
    pub hidden: Vec<PyStr>,
}

/// `None` when the workspace is on `expected_branch` at `expected_head` with a clean index and
/// clean tracked files (`gitops.detect_violation`). Untracked files (build output) are not a
/// violation. An entry marked assume-unchanged or skip-worktree is: the mark is cleared first,
/// so an edit behind it counts as the tracked change it is.
pub fn detect_violation(
    git: &Git,
    expected_head: &str,
    expected_branch: &PyStr,
) -> Result<Option<Violation>> {
    let hidden = unhide_index_entries(git)?;
    let head_moved = checked_out_branch(git)?.as_ref() != Some(expected_branch)
        || head(git, os("HEAD"))? != expected_head;
    let index_dirty = !yes_or_no(
        git,
        &[
            os("diff"),
            os("--cached"),
            os("--quiet"),
            os("--no-ext-diff"),
            os("HEAD"),
            os("--"),
        ],
    )?;
    let out = git.run(&[
        os("diff"),
        os("--name-only"),
        os("-z"),
        os("--no-renames"),
        os("--no-ext-diff"),
        os("--"),
    ])?;
    let tracked_changes: Vec<PyStr> = split_nul(trim_newlines(&out))
        .iter()
        .map(|p| text(p))
        .collect();
    if head_moved || index_dirty || !tracked_changes.is_empty() || !hidden.is_empty() {
        return Ok(Some(Violation {
            head_moved,
            index_dirty,
            tracked_changes,
            hidden,
        }));
    }
    Ok(None)
}

/// The test and doc folders that count at the top level only.
const TEST_DOC_DIRS: [&str; 4] = ["tests", "test", "spec", "docs"];
/// Test file names, anywhere (`fnmatchcase`).
const TEST_NAMES: [&str; 4] = ["test_*", "*_test.*", "*.test.*", "*.spec.*"];
/// Doc file suffixes, matched lowercased.
const DOC_SUFFIXES: [&str; 3] = [".md", ".rst", ".txt"];
/// Dependency and build inputs: production anywhere, matched lowercased.
const PRODUCTION_NAMES: [&str; 3] = ["requirements*.txt", "constraints*.txt", "cmakelists.txt"];

/// Whether a changed path is a test or a doc, so a fix touching only such paths gets the
/// supervisor's diff check instead of a re-review (`gitops.is_test_or_doc_path`, ruling R13):
/// under a top-level `tests/`, `test/`, `spec/` or `docs/`; or named `test_*`, `*_test.*`,
/// `*.test.*` or `*.spec.*`; or a `*.md`, `*.rst` or `*.txt` file. Dependency and build
/// inputs (`requirements*.txt`, `constraints*.txt`, `CMakeLists.txt`) are production anywhere.
/// Narrow on purpose: a wrong "test or doc" verdict skips re-review.
pub fn is_test_or_doc_path(path: &PyStr) -> bool {
    // Only ASCII patterns are matched, so a surrogate escape (shown as U+FFFD, one character
    // either way) matches as it does in the Python.
    let path = path.lossy();
    let parts = posix_parts(&path);
    let Some(name) = parts.last() else {
        return false;
    };
    let lower = name.to_lowercase();
    if PRODUCTION_NAMES.iter().any(|p| fnmatch(&lower, p)) {
        return false;
    }
    if parts.len() > 1 && TEST_DOC_DIRS.contains(&parts[0]) {
        return true;
    }
    DOC_SUFFIXES.iter().any(|s| lower.ends_with(s)) || TEST_NAMES.iter().any(|p| fnmatch(name, p))
}

/// `PurePosixPath(path).parts`: the components, empty ones and `.` dropped, a leading `/` (or
/// `//`, which POSIX keeps apart) as a part of its own.
fn posix_parts(path: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let rest = if let Some(rest) = path.strip_prefix("//").filter(|r| !r.starts_with('/')) {
        parts.push("//");
        rest
    } else if path.starts_with('/') {
        parts.push("/");
        path.trim_start_matches('/')
    } else {
        path
    };
    parts.extend(rest.split('/').filter(|p| !p.is_empty() && *p != "."));
    parts
}

/// `fnmatchcase(name, pattern)` for a pattern whose only wildcard is `*`.
fn fnmatch(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut n, mut p) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if p < pattern.len() && pattern[p] == name[n] {
            n += 1;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::git::fixture::Fixture;
    use crate::git::{prepare_worktree, repo_identity};

    const RUN: &str = "leadhand/run-1";

    /// A repository and its run worktree on `RUN` at the base (`GitopsTests.workspace`).
    fn workspace(f: &Fixture) -> (PathBuf, PathBuf, String) {
        let repo = f.make_repo("my repo");
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let id = repo_identity(&repo).unwrap();
        let ws = prepare_worktree(&id, RUN, &base, "run-1", &f.root().join("run dir")).unwrap();
        (repo, ws, base)
    }

    /// Cuts card `n` and commits `files` on it (`GitopsTests.card`): its branch, base and tip.
    fn card(f: &Fixture, ws: &Path, n: u32, files: &[(&str, &str)]) -> (PyStr, String, String) {
        let git = Git::at(ws).unwrap();
        let (branch, base) = cut_card_branch(&git, &RUN.into(), &n.to_string(), true).unwrap();
        let tip = f.commit(ws, files, &format!("card {n}"));
        (branch, base, tip)
    }

    // CATALOG: test_gitops.py::GitopsTests::test_is_ancestor
    #[test]
    fn is_ancestor_follows_history() {
        let f = Fixture::new();
        let (_, ws, x) = workspace(&f);
        let (_, _, tip_a) = card(&f, &ws, 1, &[("a.txt", "a\n")]);
        let git = Git::at(&ws).unwrap();
        assert!(is_ancestor(&git, os(&x), os(&tip_a)).unwrap());
        assert!(is_ancestor(&git, os(&tip_a), os(&tip_a)).unwrap());
        assert!(!is_ancestor(&git, os(&tip_a), os(&x)).unwrap());
    }

    #[test]
    fn card_branches_are_cut_at_the_run_tip_and_fast_forwarded() {
        let f = Fixture::new();
        let (repo, ws, base) = workspace(&f);
        let git = Git::at(&ws).unwrap();
        let (branch, cut_base, tip) = card(&f, &ws, 1, &[("a.txt", "a\n")]);
        assert_eq!(
            (branch.lossy().into_owned(), cut_base.as_str()),
            ("leadhand/run-1--card-1".to_string(), base.as_str())
        );
        assert_eq!(
            f.git(&ws, &["symbolic-ref", "--short", "HEAD"]),
            branch.lossy()
        );
        assert_eq!(
            changed_paths(&git, &base, &tip).unwrap(),
            [PyStr::from("a.txt")]
        );
        assert_eq!(fast_forward(&git, &RUN.into(), &tip).unwrap(), tip);
        assert_eq!(f.git(&repo, &["rev-parse", RUN]), tip);
        // Idempotent; an older target leaves the run tip where it is.
        assert_eq!(fast_forward(&git, &RUN.into(), &tip).unwrap(), tip);
        assert_eq!(fast_forward(&git, &RUN.into(), &base).unwrap(), tip);
        // A recut reuses the branch, its base its merge base with the run branch.
        let again = cut_card_branch(&git, &RUN.into(), "1", true).unwrap();
        assert_eq!(again, (branch, tip.clone()));
        // A card branch the run did not record is refused unless it is at the run tip.
        f.git(&repo, &["branch", "leadhand/run-1--card-2", &tip]);
        let (two, two_base) = cut_card_branch(&git, &RUN.into(), "2", false).unwrap();
        assert_eq!(
            (two.lossy().into_owned(), two_base),
            ("leadhand/run-1--card-2".into(), tip.clone())
        );
        f.git(&repo, &["branch", "leadhand/run-1--card-3", &base]);
        let e = cut_card_branch(&git, &RUN.into(), "3", false).unwrap_err();
        assert!(
            e.message
                .contains("already exists but this run did not cut it"),
            "{e}"
        );
        // Not a fast-forward: a commit beside the run tip.
        f.git(&ws, &["checkout", "--quiet", "--detach", &base]);
        let diverged = f.commit(&ws, &[("f.txt", "f\n")], "diverged");
        let e = fast_forward(&git, &RUN.into(), &diverged).unwrap_err();
        assert!(
            e.message
                .starts_with("not a fast-forward: leadhand/run-1 is at "),
            "{e}"
        );
        assert_eq!(f.git(&repo, &["rev-parse", RUN]), tip);
    }

    #[test]
    fn workspace_reads_name_the_branch_operation_and_refs() {
        let f = Fixture::new();
        let (_, ws, base) = workspace(&f);
        let git = Git::at(&ws).unwrap();
        assert_eq!(current_branch(&git), Some(PyStr::from(RUN)));
        assert_eq!(operation(&git).unwrap(), None);
        assert_eq!(
            ref_sha(&git, os(&format!("refs/heads/{RUN}"))),
            Some(base.clone())
        );
        assert_eq!(ref_sha(&git, os("refs/heads/missing")), None);
        let (_, _, tip) = card(&f, &ws, 1, &[("a.txt", "a\n")]);
        f.git(&ws, &["update-ref", "refs/leadhand/abc123/keep", &base]);
        f.git(&ws, &["update-ref", "refs/leadhand/other/keep", &base]);
        let snap = ref_snapshot(&git, &RUN.into(), "abc123").unwrap();
        let want = [
            ("refs/heads/leadhand/run-1", &base),
            ("refs/heads/leadhand/run-1--card-1", &tip),
            ("refs/leadhand/abc123/keep", &base),
        ];
        assert_eq!(
            snap.0,
            want.map(|(k, v)| (PyStr::from(k), v.clone())).to_vec()
        );
        assert_eq!(
            detect_violation(&git, &tip, &card_branch_name(&RUN.into(), "1")).unwrap(),
            None
        );
        f.write(&ws, &[("a.txt", "changed\n")]);
        let v = detect_violation(&git, &tip, &card_branch_name(&RUN.into(), "1"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (v.head_moved, v.index_dirty, v.tracked_changes),
            (false, false, vec![PyStr::from("a.txt")])
        );
        f.git(&ws, &["checkout", "--quiet", "--", "a.txt"]);
        let v = detect_violation(&git, &base, &card_branch_name(&RUN.into(), "1")).unwrap();
        assert!(v.is_some_and(|v| v.head_moved));
        // A merge left in progress is an operation.
        std::fs::write(
            ws.join(f.git(&ws, &["rev-parse", "--git-path", "MERGE_HEAD"])),
            format!("{tip}\n"),
        )
        .unwrap();
        assert_eq!(operation(&git).unwrap(), Some("merge"));
    }

    #[test]
    fn test_or_doc_paths_match_python() {
        let paths = [
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
            "NOTES.MD",
            "src/Test_x.py",
            "./tests//a.py",
            "/tests/a.py",
            "tests/",
            "",
            "src/caf\u{e9}.TXT",
            "src/x.spec",
            "a.test.",
        ];
        let code = "import json, sys\nfrom leadhand.gitops import is_test_or_doc_path\n\
                    print(json.dumps([is_test_or_doc_path(p) for p in sys.argv[1:]]))";
        let args: Vec<&OsStr> = paths.iter().map(OsStr::new).collect();
        let theirs: Vec<bool> =
            serde_json::from_slice(&crate::wire::tests::python(code, &args)).unwrap();
        let ours: Vec<bool> = paths
            .iter()
            .map(|p| is_test_or_doc_path(&PyStr::from(*p)))
            .collect();
        assert_eq!(ours, theirs);
        // A surrogate escape in a name is one character, as in the Python.
        let odd = PyStr::from_os(OsStr::from_bytes(b"tests/a\xff.py"));
        assert!(is_test_or_doc_path(&odd));
        assert!(!is_test_or_doc_path(&PyStr::from_os(OsStr::from_bytes(
            b"src/\xff.py"
        ))));
    }
}
