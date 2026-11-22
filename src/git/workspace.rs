//! A run's base and its worktree (`gitops`: `default_base`, `ensure_excluded`,
//! `resolves_in_place`, `prepare_workspace` in worktree mode, `_worktrees`;
//! `dispatch.base_worktree_path`), and the run worktree's identity: the check every command
//! makes before it runs Git there, and the refusal of a run whose workspace fails it.
//!
//! What this writes in the operator's repository (spec, bug class 1) is only: the run branch,
//! created only when missing (`update-ref` with an empty old value, so a branch made meanwhile
//! is never overwritten); the run's worktree under `<top>/.worktrees/`; two lines in
//! `<common dir>/info/exclude`; the run record in that worktree's admin folder; and what a
//! fetch of `origin` updates. Nothing is checked out, reset, cleaned, stashed or staged in the
//! operator's working tree.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;

use super::preserve::lexists;
use super::{FETCH_TIMEOUT, Git, RepoIdentity, linked_worktree};
use crate::cli::human::command_line;
use crate::dispatch::caller::{py_join, py_name, py_parent, py_str_of};
use crate::dispatch::{field, truthy};
use crate::error::{Error, Result};
use crate::pytext::{
    abspath, normpath, py_repr, py_split, py_splitlines, py_splitlines_bytes, py_strip,
    py_strip_bytes, real_path,
};
use crate::runsel::RUN_RECORD;
use crate::store::write_file_atomic_mode;
use crate::wire::{PyMap, PyStr};

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

/// The run's evidence base worktree (`dispatch.base_worktree_path`): `<repo>/.worktrees/
/// <workspace name>--base` when the workspace is a worktree there, else `<repo>/.worktrees/
/// <run dir name>--base` (an in-place run).
pub fn base_worktree_path(repo: &Path, workspace: &Path, run_name: &OsStr) -> PathBuf {
    let worktrees = py_join(repo, ".worktrees");
    // `ws.parent == repo / ".worktrees"`: pathlib compares the spellings (`//x` is not `/x`).
    let name = if py_parent(workspace).as_os_str() == worktrees.as_os_str() {
        py_name(workspace)
    } else {
        run_name.to_os_string()
    };
    let mut base = name;
    base.push("--base");
    worktrees.join(base)
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
pub(super) fn write_run_record(ws: &Path, run_dir: &Path) -> Result<()> {
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

// --------------------------------------------------------------------------
// The run worktree, by identity (bug classes 1 and 4; rulings S2-R49, S2-R54)
//
// The run worktree `<repo>/.worktrees/<slug>` lies inside the operator's checkout. Once its
// `.git` file is gone, Git run there finds the operator's repository, so every write meant for
// the workspace would land in the operator's checkout. Git is run there only once the folder
// has passed this check, once per command.

/// Why a folder is not a run worktree that Git may be run in (`not_a_run_worktree`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotARunWorktree {
    /// What the check found, naming the folder, and Git's top there when Git found one.
    pub problem: PyStr,
    /// The repository still lists the folder as a linked worktree, but Git no longer finds the
    /// worktree from there (its `.git` file is gone or broken): `git worktree repair`, run in
    /// the repository, restores it.
    pub repairable: bool,
}

/// The identity check of the run worktree `ws` of the repository `repo`: Git's top for `ws`
/// must be `ws` itself, and `repo` must list it in `git worktree list --porcelain -z` as a
/// linked worktree, not as its main working tree. Paths are compared as real paths, byte for
/// byte. `None` when it passes. Read only, and it reads no record or marker of leadhand's, so a
/// run the Python started passes too.
pub fn not_a_run_worktree(repo: &Path, ws: &Path) -> Option<NotARunWorktree> {
    let real = real_path(ws);
    let listed = Git::at(repo)
        .and_then(|git| worktrees(&git))
        .map(|entries| {
            entries.iter().position(|e| {
                e.get(&b"worktree"[..])
                    .is_some_and(|p| real_path(Path::new(OsStr::from_bytes(p))) == real)
            })
        });
    let folder = |what: &str| {
        let mut text = PyStr::from_os_parts(&[OsStr::new("the run worktree "), ws.as_os_str()]);
        text.push_str(what);
        text
    };
    let top_problem = match real_top(ws) {
        Ok(top) if top == real => None,
        Ok(top) => {
            let mut text = folder(" is not a worktree top (Git's top there is ");
            text.push(&PyStr::from_os(top.as_os_str()));
            text.push_str(")");
            Some(text)
        }
        Err(why) => {
            let mut text = PyStr::from_os_parts(&[
                OsStr::new("git cannot read the run worktree "),
                ws.as_os_str(),
                OsStr::new(": "),
            ]);
            text.push(&why);
            Some(text)
        }
    };
    let (problem, repairable) = match (top_problem, listed) {
        // The first entry is the main working tree.
        (None, Ok(Some(0))) => (folder(" is the repository's main working tree"), false),
        (None, Ok(Some(_))) => return None,
        (None, Ok(None)) => {
            let mut text = folder(" is not a worktree of ");
            text.push(&PyStr::from_os(repo.as_os_str()));
            (text, false)
        }
        (None, Err(e)) => {
            let mut text = PyStr::from_os_parts(&[
                OsStr::new("git cannot list the worktrees of "),
                repo.as_os_str(),
                OsStr::new(": "),
            ]);
            text.push_str(&e.message);
            (text, false)
        }
        (Some(text), listed) => (text, matches!(listed, Ok(Some(n)) if n > 0)),
    };
    Some(NotARunWorktree {
        problem,
        repairable,
    })
}

/// Whether Git, run in the folder `path`, takes `path` itself as its top (real paths, compared
/// byte for byte): false for a folder whose `.git` is gone, where Git finds the enclosing
/// checkout, and for one Git cannot read. Read only.
pub fn is_own_top(path: &Path) -> bool {
    real_top(path).is_ok_and(|top| top == real_path(path))
}

/// Git's top for the folder `ws` (`rev-parse --show-toplevel`), as a real path; Git's failure,
/// its text kept, when it finds none.
fn real_top(ws: &Path) -> std::result::Result<PathBuf, PyStr> {
    let git = Git::try_at(ws).map_err(|f| f.text)?;
    let argv = args(&["rev-parse", "--show-toplevel"]);
    let out = git.try_output(&argv).map_err(|f| f.text)?;
    if !out.status.success() {
        return Err(git.failure_text(&argv, &out).text);
    }
    let top = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    Ok(real_path(Path::new(OsStr::from_bytes(top))))
}

// --------------------------------------------------------------------------
// The refusals of a run whose workspace is not its own worktree

/// The refusal of a run whose workspace leadhand must not run Git in, with the text `run`
/// prints, or `None` for a run worktree that passes (or whose folder does not exist: the
/// command's own paths handle a missing one). First an in-place run (deliberate change 1,
/// S2-R44: `in_place_refusal`), then a folder that fails the identity check (S2-R54:
/// `worktree_refusal`). `meta` is run.json's entries, `repo` and `ws` its repository and
/// workspace, `cli` the CLI's path.
pub fn workspace_refusal(
    meta: &PyMap<Box<RawValue>>,
    repo: &Path,
    ws: &Path,
    cli: &Path,
) -> Option<Vec<u8>> {
    in_place_refusal(meta, cli).or_else(|| worktree_refusal(meta, repo, ws, cli))
}

/// `workspace_refusal`, as the error the engine commands that act on the workspace (`next`,
/// `record result`, the reconciliation) return before they write anything.
pub fn refuse_unless_run_worktree(
    meta: &PyMap<Box<RawValue>>,
    repo: &Path,
    ws: &Path,
) -> Result<()> {
    match workspace_refusal(meta, repo, ws, &crate::testing::self_path()) {
        Some(text) => Err(Error::printed(text)),
        None => Ok(()),
    }
}

/// The refusal of a run whose workspace folder exists but fails the identity check
/// (`not_a_run_worktree`), or `None`: `error:` and what the check found, then the one command
/// that fixes it alone on the last line: `git -C <repo> worktree repair` while the repository
/// still lists the folder (it restores the `.git` file), else the command that starts a new run
/// of the deck.
fn worktree_refusal(
    meta: &PyMap<Box<RawValue>>,
    repo: &Path,
    ws: &Path,
    cli: &Path,
) -> Option<Vec<u8>> {
    if !ws.is_dir() {
        return None;
    }
    let found = not_a_run_worktree(repo, ws)?;
    let mut head = found.problem.to_os_lossy().into_vec();
    head.extend_from_slice(b"; leadhand will not run Git there. ");
    let fix = if found.repairable {
        head.extend_from_slice(b"Restore its .git file with:");
        command_line(&[
            "git".into(),
            "-C".into(),
            real_path(repo).into_os_string(),
            "worktree".into(),
            "repair".into(),
        ])
    } else {
        head.extend_from_slice(b"Start a new run of the deck with:");
        start_command(meta, cli)
    };
    Some(fix_refusal(&head, &fix))
}

/// The refusal of an in-place run (deliberate change 1: the Python's checkout claim is gone),
/// with the command that starts a new run of its deck in a worktree; `None` for a worktree
/// run.
fn in_place_refusal(meta: &PyMap<Box<RawValue>>, cli: &Path) -> Option<Vec<u8>> {
    let repo = meta_path(meta, "repo").filter(|_| in_place(meta))?;
    let mut head = b"not supported in this version: run.worktree = false (this run works in the \
                     checkout "
        .to_vec();
    head.extend_from_slice(repo.as_os_str().as_bytes());
    head.extend_from_slice(
        b" itself); start a new run of the deck, which works in a worktree, with:",
    );
    Some(fix_refusal(&head, &start_command(meta, cli)))
}

/// `_in_place`: run.json's workspace is the repository's checkout itself (real paths).
fn in_place(meta: &PyMap<Box<RawValue>>) -> bool {
    match (meta_path(meta, "workspace"), meta_path(meta, "repo")) {
        (Some(ws), Some(repo)) => real_path(&ws) == real_path(&repo),
        _ => false,
    }
}

/// A refusal whose one fixing command a person copies (spec, "Commands you type"): the
/// `error: <head>` line, then the command alone on the last line, its bytes kept.
pub fn fix_refusal(head: &[u8], fix: &OsStr) -> Vec<u8> {
    let mut text = b"error: ".to_vec();
    text.extend_from_slice(head);
    text.push(b'\n');
    text.extend_from_slice(fix.as_bytes());
    text.push(b'\n');
    text
}

/// The operator's command to start a new run of this run's deck (`launch._start_command`),
/// every word quoted: `meta` is run.json's entries, `cli` the CLI's path.
pub fn start_command(meta: &PyMap<Box<RawValue>>, cli: &Path) -> OsString {
    let mut argv = vec![
        cli.as_os_str().to_os_string(),
        "start".into(),
        meta_word(meta, "deck_source").unwrap_or_else(|| "<deck>".into()),
    ];
    if let Some(repo) = meta_word(meta, "repo") {
        argv.push("--repo".into());
        argv.push(repo);
    }
    command_line(&argv)
}

/// run.json's non-empty string entry `key`, as a path.
pub fn meta_path(meta: &PyMap<Box<RawValue>>, key: &str) -> Option<PathBuf> {
    let text = field::<PyStr>(meta, key).filter(|t| !t.is_empty())?;
    Some(PathBuf::from(text.to_os().ok()?))
}

/// `str(meta.get(key))` of a true entry, its bytes kept; `None` for a false one.
fn meta_word(meta: &PyMap<Box<RawValue>>, key: &str) -> Option<OsString> {
    let raw = meta.get(&PyStr::from(key)).filter(|raw| truthy(raw))?;
    match serde_json::from_str::<PyStr>(raw.get()) {
        Ok(text) => Some(text.to_os().unwrap_or_else(|_| text.to_os_lossy())),
        Err(_) => Some(py_str_of(raw).into()),
    }
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
        // `git.json`: the reference's `resolves_in_place` of each case, on the same links under
        // a root written `{root}`.
        let top = f.root().to_str().unwrap();
        let placeheld = |p: &Path| p.to_str().unwrap().replacen(top, "{root}", 1);
        let inputs: Vec<serde_json::Value> = cases
            .iter()
            .map(|(r, p)| serde_json::json!([placeheld(r), placeheld(p)]))
            .collect();
        let theirs: Vec<bool> =
            crate::wire::tests::reference_outputs("git.json", "resolves_in_place", &inputs)
                .iter()
                .map(|out| out.as_bool().unwrap())
                .collect();
        let ours: Vec<bool> = cases.iter().map(|(r, p)| resolves_in_place(r, p)).collect();
        assert_eq!(ours, theirs);
        assert_eq!(ours[..3], [true, false, false]);
    }

    #[test]
    fn base_worktree_path_is_named_for_the_workspace_or_the_run() {
        let repo = Path::new("/repo");
        assert_eq!(
            base_worktree_path(repo, Path::new("/repo/.worktrees/ws"), OsStr::new("r1")),
            Path::new("/repo/.worktrees/ws--base")
        );
        assert_eq!(
            base_worktree_path(repo, Path::new("/repo//.worktrees/ws/"), OsStr::new("r1")),
            Path::new("/repo/.worktrees/ws--base")
        );
        assert_eq!(
            base_worktree_path(repo, Path::new("/repo"), OsStr::new("r1")),
            Path::new("/repo/.worktrees/r1--base")
        );
        assert_eq!(
            base_worktree_path(
                repo,
                Path::new("/elsewhere/.worktrees/ws"),
                OsStr::new("r1")
            ),
            Path::new("/repo/.worktrees/r1--base")
        );
    }

    /// A repository and a worktree of it added with plain Git at `<repo>/.worktrees/<name>`, as
    /// a run the Python started has it: no run record, no marker. The repository and the
    /// worktree.
    fn run_worktree(f: &Fixture, name: &OsStr) -> (PathBuf, PathBuf) {
        let repo = f.make_repo("my repo");
        std::fs::write(repo.join(".git/info/exclude"), ".worktrees/\n").unwrap();
        let ws = repo.join(".worktrees").join(name);
        let mut add = args(&["worktree", "add", "-q", "-b", RUN]);
        add.push(ws.as_os_str());
        Git::at(&repo).unwrap().run(&add).unwrap();
        (repo, ws)
    }

    fn problem(found: Option<NotARunWorktree>) -> (String, bool) {
        let found = found.expect("the check fails");
        (found.problem.lossy().into_owned(), found.repairable)
    }

    // S2-R54: a run worktree passes by any spelling of its path (a symlink to it, a folder name
    // with spaces, and one that is not UTF-8 where the system allows it), with no record or
    // marker of leadhand's.
    #[test]
    fn the_identity_check_passes_a_run_worktree_by_any_spelling() {
        let f = Fixture::new();
        // Linux allows a folder name that is not UTF-8; macOS refuses to create one.
        let odd = OsStr::from_bytes(b"odd-\xff run");
        let name = if std::fs::create_dir(f.root().join(odd)).is_ok() {
            odd
        } else {
            OsStr::new("caf\u{e9} run")
        };
        let (repo, ws) = run_worktree(&f, name);
        let admin = linked_worktree(&ws).unwrap().unwrap().git_dir;
        assert!(!admin.join(RUN_RECORD).exists() && !admin.join("leadhand-run").exists());
        assert_eq!(not_a_run_worktree(&repo, &ws), None);
        assert!(is_own_top(&ws));
        let link = f.root().join("link to the worktree");
        std::os::unix::fs::symlink(&ws, &link).unwrap();
        assert_eq!(not_a_run_worktree(&repo, &link), None);
        let repo_link = f.root().join("link to the repo");
        std::os::unix::fs::symlink(&repo, &repo_link).unwrap();
        assert_eq!(not_a_run_worktree(&repo_link, &link), None);
        assert_eq!(
            not_a_run_worktree(&repo_link, &repo_link.join(".worktrees").join(name)),
            None
        );
    }

    // S2-R54, bug classes 1 and 4: what fails the check, and what it says. A folder whose `.git`
    // file is gone (or broken) makes Git find the enclosing checkout, the operator's; while the
    // repository still lists it, `git worktree repair` restores it.
    #[test]
    fn a_folder_that_is_not_a_run_worktree_fails_the_identity_check() {
        let f = Fixture::new();
        let (repo, ws) = run_worktree(&f, OsStr::new("demo run"));
        let shown = |p: &Path| p.display().to_string();
        let top = format!(
            "the run worktree {} is not a worktree top (Git's top there is {})",
            shown(&ws),
            shown(&repo)
        );
        std::fs::remove_file(ws.join(".git")).unwrap();
        assert_eq!(problem(not_a_run_worktree(&repo, &ws)), (top.clone(), true));
        assert!(!is_own_top(&ws));
        f.git(&repo, &["worktree", "repair"]);
        assert_eq!(not_a_run_worktree(&repo, &ws), None);
        // A broken `.git` file: Git cannot read the folder at all.
        std::fs::write(ws.join(".git"), "not a gitfile\n").unwrap();
        let (text, repairable) = problem(not_a_run_worktree(&repo, &ws));
        assert!(
            text.starts_with(&format!(
                "git cannot read the run worktree {}: ",
                shown(&ws)
            )),
            "{text}"
        );
        assert!(repairable);
        f.git(&repo, &["worktree", "repair"]);
        assert_eq!(not_a_run_worktree(&repo, &ws), None);
        // The record pruned too: nothing restores it.
        std::fs::remove_file(ws.join(".git")).unwrap();
        f.git(&repo, &["worktree", "prune"]);
        assert_eq!(problem(not_a_run_worktree(&repo, &ws)), (top, false));
        // The main working tree is never a run worktree.
        assert_eq!(
            problem(not_a_run_worktree(&repo, &repo)),
            (
                format!(
                    "the run worktree {} is the repository's main working tree",
                    shown(&repo)
                ),
                false
            )
        );
        // A top of its own that the repository does not list: another repository's.
        let other = f.make_repo_in(&repo.join(".worktrees"), "other");
        assert_eq!(
            problem(not_a_run_worktree(&repo, &other)),
            (
                format!(
                    "the run worktree {} is not a worktree of {}",
                    shown(&other),
                    shown(&repo)
                ),
                false
            )
        );
        // A folder in no repository at all.
        let outside = f.root().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let (text, repairable) = problem(not_a_run_worktree(&repo, &outside));
        assert!(
            text.starts_with(&format!(
                "git cannot read the run worktree {}: ",
                shown(&outside)
            )),
            "{text}"
        );
        assert!(!repairable);
        // A repository Git cannot list.
        let (text, _) = problem(not_a_run_worktree(&outside, &other));
        assert!(
            text.starts_with(&format!(
                "git cannot list the worktrees of {}: ",
                shown(&outside)
            )),
            "{text}"
        );
    }

    fn meta(json: &str) -> PyMap<Box<RawValue>> {
        serde_json::from_str(json).unwrap()
    }

    fn json(p: &Path) -> String {
        serde_json::to_string(p.to_str().unwrap()).unwrap()
    }

    // S2-R54: the refusal names what the check found and ends with the one command that fixes
    // it: the repair while the repository lists the worktree (real paths), else a new run.
    // A missing folder and a run worktree that passes are not refused here.
    #[test]
    fn the_worktree_refusal_ends_with_the_fix_that_is_true() {
        let f = Fixture::new();
        let (repo, ws) = run_worktree(&f, OsStr::new("demo run"));
        let link = f.root().join("repo link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let m = meta(&format!(
            r#"{{"workspace": {}, "repo": {}, "deck_source": "/d/my deck.md"}}"#,
            json(&link.join(".worktrees/demo run")),
            json(&link)
        ));
        let cli = Path::new("/opt/lead hand/leadhand");
        let ws_link = link.join(".worktrees/demo run");
        assert_eq!(worktree_refusal(&m, &link, &ws_link, cli), None);
        assert_eq!(workspace_refusal(&m, &link, &ws_link, cli), None);
        assert_eq!(
            worktree_refusal(&m, &link, &f.root().join("gone"), cli),
            None
        );
        std::fs::remove_file(ws.join(".git")).unwrap();
        let text = |t: Option<Vec<u8>>| String::from_utf8(t.expect("a refusal")).unwrap();
        let head = format!(
            "error: the run worktree {} is not a worktree top (Git's top there is {}); leadhand \
             will not run Git there. ",
            ws_link.display(),
            repo.display()
        );
        assert_eq!(
            text(worktree_refusal(&m, &link, &ws_link, cli)),
            format!(
                "{head}Restore its .git file with:\ngit -C '{}' worktree repair\n",
                repo.display()
            )
        );
        assert_eq!(
            text(workspace_refusal(&m, &link, &ws_link, cli)),
            text(worktree_refusal(&m, &link, &ws_link, cli))
        );
        f.git(&repo, &["worktree", "prune"]);
        assert_eq!(
            text(worktree_refusal(&m, &link, &ws_link, cli)),
            format!(
                "{head}Start a new run of the deck with:\n'/opt/lead hand/leadhand' start \
                 '/d/my deck.md' --repo '{}'\n",
                link.display()
            )
        );
    }

    #[test]
    fn an_in_place_run_is_refused_with_the_start_command_alone_last() {
        let dir = tempfile::Builder::new()
            .prefix("in place ")
            .tempdir()
            .unwrap();
        let repo = dir.path().join("my repo");
        std::fs::create_dir_all(repo.join(".worktrees/run")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let of = |workspace: &Path| {
            meta(&format!(
                r#"{{"workspace": {}, "repo": {}, "deck_source": "/d/my deck.md"}}"#,
                json(workspace),
                json(&repo)
            ))
        };
        let cli = Path::new("/opt/lead hand/leadhand");
        // The checkout itself, by any spelling: in place.
        for ws in [repo.clone(), link.clone()] {
            let m = of(&ws);
            assert!(in_place(&m), "{ws:?}");
            let text = String::from_utf8(in_place_refusal(&m, cli).unwrap()).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(
                lines,
                [
                    format!(
                        "error: not supported in this version: run.worktree = false (this run \
                         works in the checkout {} itself); start a new run of the deck, which \
                         works in a worktree, with:",
                        repo.display()
                    )
                    .as_str(),
                    &format!(
                        "'/opt/lead hand/leadhand' start '/d/my deck.md' --repo '{}'",
                        repo.display()
                    ),
                ]
            );
            assert!(text.ends_with('\n'));
            // The in-place refusal comes first, as `run` and the engine print it.
            assert_eq!(
                workspace_refusal(&m, &repo, &ws, cli),
                in_place_refusal(&m, cli)
            );
        }
        // A worktree run: not in place, no refusal.
        let m = of(&repo.join(".worktrees/run"));
        assert!(!in_place(&m));
        assert_eq!(in_place_refusal(&m, cli), None);
        // Without a repo or a workspace: not in place.
        assert!(!in_place(&meta("{}")));
    }

    #[test]
    fn the_start_command_quotes_every_word() {
        let cli = Path::new("/opt/lead hand/leadhand");
        assert_eq!(
            start_command(
                &meta(r#"{"deck_source": "/d/my deck.md", "repo": "/r"}"#),
                cli
            ),
            "'/opt/lead hand/leadhand' start '/d/my deck.md' --repo /r"
        );
        assert_eq!(
            start_command(&meta(r#"{"deck_source": "", "repo": null}"#), cli),
            "'/opt/lead hand/leadhand' start '<deck>'"
        );
    }
}
