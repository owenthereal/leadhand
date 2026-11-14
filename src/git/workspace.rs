//! A run's base and its worktree (`gitops`: `default_base`, `ensure_excluded`,
//! `resolves_in_place`, `prepare_workspace` in worktree mode, `_worktrees`).
//!
//! What this writes in the operator's repository (spec, bug class 1) is only: the run branch,
//! created only when missing (`update-ref` with an empty old value, so a branch made meanwhile
//! is never overwritten); the run's worktree under `<top>/.worktrees/`; two lines in
//! `<common dir>/info/exclude`; the run record in that worktree's admin folder; and what a
//! fetch of `origin` updates. Nothing is checked out, reset, cleaned, stashed or staged in the
//! operator's working tree.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{FETCH_TIMEOUT, Git, RepoIdentity, linked_worktree};
use crate::error::{Error, Result};
use crate::pytext::{
    abspath, normpath, py_repr, py_split, py_splitlines, py_splitlines_bytes, py_strip,
    py_strip_bytes, real_path,
};
use crate::runsel::RUN_RECORD;
use crate::store::write_file_atomic_mode;

/// One entry of `git worktree list --porcelain -z`: each field's value by its name
/// (`worktree`, `HEAD`, `branch`, `detached`, …), a later field of the same name winning, as
/// `gitops._worktrees` reads it. A field without a value (`detached`, `bare`) has an empty one.
pub type WorktreeEntry = HashMap<Vec<u8>, Vec<u8>>;

/// The entries of `git worktree list --porcelain -z`; the main worktree comes first.
pub fn worktrees(git: &Git) -> Result<Vec<WorktreeEntry>> {
    let out = git.run(&args(&["worktree", "list", "--porcelain", "-z"]))?;
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    for field in out.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        let (key, value) = match field.iter().position(|b| *b == b' ') {
            Some(at) => (&field[..at], &field[at + 1..]),
            None => (field, &b""[..]),
        };
        if key == b"worktree" {
            entries.push(HashMap::new());
        }
        if let Some(last) = entries.last_mut() {
            last.insert(key.to_vec(), value.to_vec());
        }
    }
    Ok(entries)
}

/// The commit `rev` names (`gitops.head`): `git rev-parse --verify <rev>^{commit}`.
pub fn head(git: &Git, rev: &OsStr) -> Result<String> {
    let mut spec = rev.to_os_string();
    spec.push("^{commit}");
    let out = git.run(&[OsStr::new("rev-parse"), OsStr::new("--verify"), &spec])?;
    let sha = trim_newlines(&out);
    String::from_utf8(sha.to_vec()).map_err(|_| {
        Error::msg(format!(
            "git rev-parse: not a commit name: {}",
            rev.display()
        ))
    })
}

/// Whether the ref exists (`gitops._ref_exists`).
pub fn ref_exists(git: &Git, name: &OsStr) -> Result<bool> {
    yes_or_no(
        git,
        &[
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            name,
        ],
    )
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

/// Refuses a name Git does not take as a branch, or one that would read as an option
/// (`gitops._check_branch_name`).
fn check_branch_name(git: &Git, name: &str) -> Result<()> {
    let argv = args(&["check-ref-format", "--branch", name]);
    if name.starts_with('-') || !git.output(&argv)?.status.success() {
        return Err(Error::msg(format!(
            "not a valid branch name: {}",
            py_repr(name)
        )));
    }
    Ok(())
}

// --------------------------------------------------------------------------
// The base

/// The run's base: `("origin/<default>", sha)` after a fetch when `origin` exists, otherwise
/// `(<local default branch>, sha)`; the name records which (`gitops.default_base`).
pub fn default_base(git: &Git) -> Result<(OsString, String)> {
    let remotes = git.run(&args(&["remote"]))?;
    if py_split(&String::from_utf8_lossy(&remotes)).any(|r| r == "origin") {
        git.with_deadline(FETCH_TIMEOUT)
            .run(&args(&["fetch", "--quiet", "origin"]))?;
        let branch = origin_default_branch(git)?;
        let mut name = OsString::from("origin/");
        name.push(&branch);
        let mut remote = OsString::from("refs/remotes/origin/");
        remote.push(&branch);
        return Ok((name, head(git, &remote)?));
    }
    let branch = local_default_branch(git)?;
    let mut local = OsString::from("refs/heads/");
    local.push(&branch);
    let sha = head(git, &local)?;
    Ok((branch, sha))
}

/// `origin`'s default branch: `refs/remotes/origin/HEAD`, else what the remote says its HEAD
/// is, else `main` or `master`, the first that origin has (`gitops._origin_default_branch`).
fn origin_default_branch(git: &Git) -> Result<OsString> {
    const REMOTE: &[u8] = b"refs/remotes/origin/";
    const SYMREF: &[u8] = b"ref: refs/heads/";
    let out = git.output(&args(&[
        "symbolic-ref",
        "--quiet",
        "refs/remotes/origin/HEAD",
    ]))?;
    let target = py_strip_bytes(&out.stdout);
    if out.status.success()
        && let Some(branch) = target.strip_prefix(REMOTE)
    {
        return Ok(OsStr::from_bytes(branch).to_os_string());
    }
    let listing = git.with_deadline(FETCH_TIMEOUT).run(&args(&[
        "ls-remote",
        "--symref",
        "origin",
        "HEAD",
    ]))?;
    for line in py_splitlines_bytes(&listing) {
        let (target, name) = match line.iter().position(|b| *b == b'\t') {
            Some(at) => (&line[..at], &line[at + 1..]),
            None => (line, &b""[..]),
        };
        if name == b"HEAD"
            && let Some(branch) = target.strip_prefix(SYMREF)
        {
            return Ok(OsStr::from_bytes(branch).to_os_string());
        }
    }
    for candidate in ["main", "master"] {
        let remote = format!("refs/remotes/origin/{candidate}");
        if ref_exists(git, OsStr::new(&remote))? {
            return Ok(candidate.into());
        }
    }
    Err(Error::msg(format!(
        "cannot determine the default branch of origin (in {})",
        git.cwd().display()
    )))
}

/// `init.defaultBranch`, `main`, `master` (the first that exists), then the main worktree's
/// branch, then the only branch (`gitops._local_default_branch`). The main worktree's branch
/// comes late because after a run without a worktree it was the run branch.
fn local_default_branch(git: &Git) -> Result<OsString> {
    const HEADS: &[u8] = b"refs/heads/";
    let listing = git.run(&args(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads/",
    ]))?;
    let branches: Vec<&[u8]> = py_splitlines_bytes(&listing)
        .into_iter()
        .map(|r| r.strip_prefix(HEADS).unwrap_or(r))
        .collect();
    let configured = git.output(&args(&["config", "--get", "init.defaultBranch"]))?;
    for candidate in [py_strip_bytes(&configured.stdout), b"main", b"master"] {
        if !candidate.is_empty() && branches.contains(&candidate) {
            return Ok(OsStr::from_bytes(candidate).to_os_string());
        }
    }
    let worktrees = worktrees(git)?;
    let main = worktrees
        .first()
        .and_then(|w| w.get(&b"branch"[..]))
        .and_then(|r| r.strip_prefix(HEADS));
    if let Some(branch) = main
        && branches.contains(&branch)
    {
        return Ok(OsStr::from_bytes(branch).to_os_string());
    }
    if let [only] = branches[..] {
        return Ok(OsStr::from_bytes(only).to_os_string());
    }
    Err(Error::msg(format!(
        "cannot determine the local default branch (in {})",
        git.cwd().display()
    )))
}

// --------------------------------------------------------------------------
// The worktree

/// Adds `pattern` to `<common dir>/info/exclude` once, replacing the file atomically with its
/// mode kept (`gitops.ensure_excluded`). The file must be UTF-8, as in the Python.
pub fn ensure_excluded(common_dir: &Path, pattern: &str) -> Result<()> {
    let info = common_dir.join("info");
    let path = info.join("exclude");
    std::fs::create_dir_all(&info).map_err(|e| Error::io(&info, e))?;
    let (mut text, mode) = match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|e| Error::msg(format!("{}: not UTF-8: {e}", path.display())))?;
            let meta = std::fs::metadata(&path).map_err(|e| Error::io(&path, e))?;
            (text, meta.permissions().mode() & 0o777)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), 0o644),
        Err(e) => return Err(Error::io(&path, e)),
    };
    if py_splitlines(&text)
        .into_iter()
        .any(|line| py_strip(line) == pattern)
    {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(pattern);
    text.push('\n');
    write_file_atomic_mode(&path, text.as_bytes(), mode)
}

/// Whether `path` lies inside `root` and resolves to itself: neither it nor any folder
/// between `root` and it is a symlink (`gitops.resolves_in_place`, security review M3).
/// `root`'s own location may be one.
pub fn resolves_in_place(root: &Path, path: &Path) -> bool {
    let (Ok(abs_root), Ok(abs_path)) = (abspath(root), abspath(path)) else {
        return false;
    };
    let Ok(rel) = abs_path.strip_prefix(&abs_root) else {
        return false;
    };
    real_path(path) == normpath(real_path(root).join(rel).as_os_str())
}

/// Creates the run branch at `base_sha` when it is missing (an existing one is left as it
/// is), then the run's worktree at `<top>/.worktrees/<slug>`, and records `run_dir` in the
/// worktree's admin folder (`gitops.prepare_workspace` in worktree mode, plus the run record).
///
/// A worktree registered there is reused only when it is this run's (checked out on
/// `run_branch`, or detached at the base); otherwise the first free `<slug>-2`, `<slug>-3`, …
/// is taken, so a run never takes over another run's worktree. A name is free when neither
/// the worktree nor its evidence base worktree (`<name>--base`) exists. A path that is a
/// symlink, or lies behind one, is refused.
///
/// Side effects in the Python's order: the excludes, the branch, the worktree; then the run
/// record, which is new.
pub fn prepare_worktree(
    id: &RepoIdentity,
    run_branch: &str,
    base_sha: &str,
    slug: &str,
    run_dir: &Path,
) -> Result<PathBuf> {
    let git = Git::at(&id.top)?;
    check_branch_name(&git, run_branch)?;
    if slug.is_empty() || slug.contains(['/', '\0']) || slug.starts_with('.') {
        return Err(Error::msg(format!(
            "not a valid workspace slug: {}",
            py_repr(slug)
        )));
    }
    let branch_ref = format!("refs/heads/{run_branch}");
    ensure_excluded(&id.common_dir, ".worktrees/")?;
    ensure_excluded(&id.common_dir, ".leadhand/")?;
    if !ref_exists(&git, OsStr::new(&branch_ref))? {
        // Empty old value: create only, never overwrite a branch made meanwhile.
        let sha = head(&git, OsStr::new(base_sha))?;
        git.run(&args(&[
            "update-ref",
            "-m",
            "leadhand: create run branch",
            &branch_ref,
            &sha,
            "",
        ]))?;
    }
    let registered: HashMap<PathBuf, WorktreeEntry> = worktrees(&git)?
        .into_iter()
        .filter_map(|w| {
            let path = PathBuf::from(OsStr::from_bytes(w.get(&b"worktree"[..])?));
            Some((real_path(&path), w))
        })
        .collect();
    let base = head(&git, OsStr::new(base_sha))?;
    let folder = id.top.join(".worktrees");
    let (mut name, mut n) = (slug.to_string(), 1);
    let ws = loop {
        let ws = folder.join(&name);
        if !resolves_in_place(&id.top, &ws) {
            return Err(Error::msg(format!(
                "{} is a symlink or lies behind one; move it aside",
                ws.display()
            )));
        }
        let entry = if ws.exists() {
            registered.get(&real_path(&ws))
        } else {
            None
        };
        let field = |e: &WorktreeEntry, key: &[u8]| e.get(key).cloned();
        if let Some(e) = entry
            && (field(e, b"branch") == Some(branch_ref.clone().into_bytes())
                || (e.contains_key(&b"detached"[..])
                    && field(e, b"HEAD") == Some(base.clone().into_bytes())))
        {
            write_run_record(&ws, run_dir)?;
            return Ok(ws);
        }
        if !lexists(&ws) && !lexists(&folder.join(format!("{name}--base"))) {
            break ws;
        }
        n += 1;
        name = format!("{slug}-{n}");
    };
    git.run(&[
        OsStr::new("worktree"),
        OsStr::new("add"),
        OsStr::new("--quiet"),
        ws.as_os_str(),
        OsStr::new(run_branch),
    ])?;
    write_run_record(&ws, run_dir)?;
    Ok(ws)
}

/// Records `run_dir` in the admin folder of the worktree at `ws` (`<common>/worktrees/<name>`),
/// beside Git's own files: its path's bytes and a newline (spec, "Which repository and run a
/// directory belongs to"). A file of its own: the evidence base worktree's ownership marker,
/// `leadhand-run`, is never written or read here.
fn write_run_record(ws: &Path, run_dir: &Path) -> Result<()> {
    let Some(wt) = linked_worktree(ws)? else {
        return Err(Error::msg(format!(
            "{} is not a linked worktree",
            ws.display()
        )));
    };
    let mut data = run_dir.as_os_str().as_bytes().to_vec();
    data.push(b'\n');
    write_file_atomic_mode(&wt.git_dir.join(RUN_RECORD), &data, 0o644)
}

/// `os.path.lexists`: whether anything, a dangling symlink included, is at `path`.
fn lexists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn args<'a>(words: &[&'a str]) -> Vec<&'a OsStr> {
    words.iter().map(|w| OsStr::new(*w)).collect()
}

/// Git's output without its trailing newlines (the Python's `stdout.rstrip("\n")`).
fn trim_newlines(out: &[u8]) -> &[u8] {
    let end = out.iter().rposition(|b| *b != b'\n').map_or(0, |i| i + 1);
    &out[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::git::repo_identity;

    const RUN: &str = "leadhand/run-1";

    /// A repository whose only branch is `branch` (`gitfixture.make_repo(default_branch=…)`).
    fn repo_on(f: &Fixture, name: &str, branch: &str) -> PathBuf {
        let repo = f.make_repo(name);
        f.git(&repo, &["branch", "-m", branch]);
        repo
    }

    /// A bare clone of `repo` beside it, added as `origin` with origin/HEAD set
    /// (`gitfixture.make_origin`).
    fn make_origin(f: &Fixture, repo: &Path) -> PathBuf {
        let mut name = repo.file_name().unwrap().to_os_string();
        name.push(" origin.git");
        let origin = repo.parent().unwrap().join(name);
        let (r, o) = (repo.to_str().unwrap(), origin.to_str().unwrap());
        f.git(
            repo.parent().unwrap(),
            &["clone", "--quiet", "--bare", r, o],
        );
        f.git(repo, &["remote", "add", "origin", o]);
        f.git(repo, &["fetch", "--quiet", "origin"]);
        f.git(repo, &["remote", "set-head", "origin", "--auto"]);
        origin
    }

    /// Moves `branch` in the bare origin to `repo`'s `branch` (`gitfixture.update_origin`).
    fn update_origin(f: &Fixture, origin: &Path, repo: &Path, branch: &str) -> String {
        let spec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        f.git(origin, &["fetch", "--quiet", repo.to_str().unwrap(), &spec]);
        f.git(origin, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    fn base(git: &Git) -> (String, String) {
        let (name, sha) = default_base(git).unwrap();
        (name.into_string().unwrap(), sha)
    }

    // CATALOG: test_gitops.py::GitopsTests::test_default_base_uses_origin_default_branch
    #[test]
    fn default_base_uses_origin_default_branch() {
        let f = Fixture::new();
        let repo = repo_on(&f, "my repo", "develop");
        let origin = make_origin(&f, &repo);
        let stale = f.git(&repo, &["rev-parse", "refs/remotes/origin/develop"]);
        let upstream = f.commit(&repo, &[("up.txt", "1\n")], "upstream work");
        assert_eq!(update_origin(&f, &origin, &repo, "develop"), upstream);
        f.commit(&repo, &[("local.txt", "1\n")], "local only"); // local develop is ahead

        assert_ne!(stale, upstream);
        let git = Git::at(&repo).unwrap();
        assert_eq!(base(&git), ("origin/develop".into(), upstream.clone())); // fetched
        assert_eq!(
            f.git(&repo, &["rev-parse", "refs/remotes/origin/develop"]),
            upstream
        );

        // Without refs/remotes/origin/HEAD it asks the remote.
        f.git(&repo, &["remote", "set-head", "origin", "--delete"]);
        assert_eq!(base(&git), ("origin/develop".into(), upstream));
    }

    // CATALOG: test_gitops.py::GitopsTests::test_default_base_without_origin_uses_local_default
    #[test]
    fn default_base_without_origin_uses_local_default() {
        let f = Fixture::new();
        let repo = repo_on(&f, "my repo", "trunk");
        let trunk = f.git(&repo, &["rev-parse", "HEAD"]);
        f.git(&repo, &["branch", "feature"]);
        assert_eq!(
            base(&Git::at(&repo).unwrap()),
            ("trunk".into(), trunk.clone())
        );

        // From a linked worktree on another branch, the main repo's branch still wins.
        let other = f.root().join("feature tree");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                other.to_str().unwrap(),
                "feature",
            ],
        );
        f.commit(&other, &[("f.txt", "f\n")], "feature work");
        assert_eq!(base(&Git::at(&other).unwrap()), ("trunk".into(), trunk));
    }

    fn ref_exists(f: &Fixture, repo: &Path, name: &str) -> bool {
        let refs = f.git(repo, &["for-each-ref", "--format=%(refname)"]);
        refs.lines().any(|r| r == name)
    }

    // CATALOG: test_gitops.py::GitopsTests::test_prepare_workspace_rejects_unsafe_slugs
    #[test]
    fn prepare_workspace_rejects_unsafe_slugs() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let id = repo_identity(&repo).unwrap();
        let run_dir = f.root().join("run dir");
        for slug in ["", "a/b", ".hidden", "..", "../escape", "nul\0byte"] {
            let e = prepare_worktree(&id, RUN, &base, slug, &run_dir).unwrap_err();
            assert!(
                e.message.starts_with("not a valid workspace slug: "),
                "{slug:?}: {e}"
            );
        }
        assert!(!repo.join(".worktrees").exists());
        assert!(!ref_exists(&f, &repo, &format!("refs/heads/{RUN}")));
    }

    #[test]
    fn prepare_worktree_makes_the_branch_worktree_excludes_and_record() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let id = repo_identity(&repo).unwrap();
        // An exclude file without a final newline, and a mode of its own: both kept.
        let exclude = id.common_dir.join("info/exclude");
        std::fs::write(&exclude, "*.tmp").unwrap();
        std::fs::set_permissions(&exclude, std::fs::Permissions::from_mode(0o600)).unwrap();
        let run_dir = f.root().join("docs dir/runs/2026-11-10-demo");
        let ws = prepare_worktree(&id, RUN, &base, "2026-11-10-demo", &run_dir).unwrap();
        assert_eq!(ws, id.top.join(".worktrees/2026-11-10-demo"));
        assert_eq!(f.git(&ws, &["symbolic-ref", "--short", "HEAD"]), RUN);
        assert_eq!(f.git(&ws, &["rev-parse", "HEAD"]), base);
        let admin = linked_worktree(&ws).unwrap().unwrap().git_dir;
        let mut want = run_dir.as_os_str().as_bytes().to_vec();
        want.push(b'\n');
        assert_eq!(std::fs::read(admin.join(RUN_RECORD)).unwrap(), want);
        assert!(!admin.join("leadhand-run").exists());

        // Again: the run's own worktree is reused, and its advanced branch is not reset.
        let advanced = f.commit(&ws, &[("run.txt", "1\n")], "run work");
        let again = prepare_worktree(&id, RUN, &base, "2026-11-10-demo", &run_dir).unwrap();
        assert_eq!(again, ws);
        assert_eq!(f.git(&repo, &["rev-parse", RUN]), advanced);
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            "*.tmp\n.worktrees/\n.leadhand/\n"
        );
        let mode = std::fs::metadata(&exclude).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(f.git(&repo, &["status", "--porcelain"]), "");

        // Another run's branch never takes this worktree, nor a name whose evidence base
        // worktree folder exists: the first free suffix.
        let other = prepare_worktree(&id, "other", &base, "2026-11-10-demo", &run_dir).unwrap();
        assert_eq!(other, id.top.join(".worktrees/2026-11-10-demo-2"));
        std::fs::create_dir(id.top.join(".worktrees/2026-11-10-demo-3--base")).unwrap();
        let third = prepare_worktree(&id, "third", &base, "2026-11-10-demo", &run_dir).unwrap();
        assert_eq!(third, id.top.join(".worktrees/2026-11-10-demo-4"));

        // An existing branch is never moved: create only.
        f.git(&repo, &["branch", "kept", "HEAD"]);
        let kept = f.commit(&repo, &[("more.txt", "1\n")], "more");
        f.git(&repo, &["branch", "-f", "kept", &kept]);
        prepare_worktree(&id, "kept", &base, "kept-run", &run_dir).unwrap();
        assert_eq!(f.git(&repo, &["rev-parse", "kept"]), kept);

        // A worktree path that is a symlink, or lies behind one, is refused.
        let elsewhere = f.root().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, id.top.join(".worktrees/linked")).unwrap();
        let e = prepare_worktree(&id, "fourth", &base, "linked", &run_dir).unwrap_err();
        assert!(
            e.message
                .ends_with("is a symlink or lies behind one; move it aside"),
            "{e}"
        );
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn resolves_in_place_matches_python() {
        let f = Fixture::new();
        let root = f.root().join("root");
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(&root, f.root().join("root link")).unwrap();
        let cases = [
            (root.clone(), root.join("real/inner")),
            (root.clone(), root.join("link/inner")),
            (root.clone(), root.join("link")),
            (root.clone(), root.join("missing/x")),
            (root.clone(), root.clone()),
            (root.clone(), f.root().join("outside")),
            (root.join("real"), root.join("real/../link")),
            (
                f.root().join("root link"),
                f.root().join("root link/real/inner"),
            ),
        ];
        let code = "import json, sys\nfrom pathlib import Path\nfrom leadhand.gitops import resolves_in_place\n\
                    a = sys.argv[1:]\n\
                    print(json.dumps([resolves_in_place(Path(a[i]), Path(a[i + 1])) for i in range(0, len(a), 2)]))";
        let args: Vec<&OsStr> = cases
            .iter()
            .flat_map(|(r, p)| [r.as_os_str(), p.as_os_str()])
            .collect();
        let theirs: Vec<bool> =
            serde_json::from_slice(&crate::wire::tests::python(code, &args)).unwrap();
        let ours: Vec<bool> = cases.iter().map(|(r, p)| resolves_in_place(r, p)).collect();
        assert_eq!(ours, theirs);
        assert_eq!(ours[..3], [true, false, false]);
    }
}
