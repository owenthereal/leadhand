//! Hermetic Git repositories for tests: the Rust form of `tests/gitfixture.py`.
//!
//! Every Git command here runs with the operator's global and system configuration shut
//! out, so commits never sign, prompt or run the operator's hooks.
//!
//! Every helper is confined to the system temp dir, where every `TempRoot` lives: a helper
//! given a path outside it (such as the project's own checkout) fails before running
//! anything. gitfixture.py confines to the roots a test registered; without process-wide
//! state (tests run in parallel), the Rust form confines to the folder those roots share.
//! GIT_CEILING_DIRECTORIES is that folder, the roots' parent, so Git never discovers an
//! enclosing repository.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const HERMETIC_ENV: [(&str, &str); 3] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
];
/// Variables that would point Git at some other repository than the cwd.
pub const LEAKY_VARS: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];
pub const IDENTITY: [&str; 8] = [
    "-c",
    "user.name=Fixture User",
    "-c",
    "user.email=fixture@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "tag.gpgsign=false",
];

/// The system temp dir, resolved.
fn temp_dir() -> PathBuf {
    let tmp = std::env::temp_dir();
    fs::canonicalize(&tmp)
        .unwrap_or_else(|e| panic!("resolving the temp dir {}: {e}", tmp.display()))
}

/// `path` with every symlink in its existing part resolved (Python's non-strict `resolve`).
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => absolute,
    }
}

/// `root` resolved, after checking it lies inside the system temp dir.
#[track_caller]
pub fn add_root(root: &Path) -> PathBuf {
    let resolved = resolve(root);
    let tmp = temp_dir();
    if resolved == tmp || !resolved.starts_with(&tmp) {
        panic!(
            "fixture root {} is not inside the temp dir {}",
            root.display(),
            tmp.display()
        );
    }
    resolved
}

/// `path` resolved, after checking it lies inside the temp dir (a fixture root).
#[track_caller]
fn confined(path: &Path) -> PathBuf {
    let resolved = resolve(path);
    let tmp = temp_dir();
    if resolved == tmp || !resolved.starts_with(&tmp) {
        panic!(
            "{} is outside every Git fixture root (the temp dir {})",
            path.display(),
            tmp.display()
        );
    }
    resolved
}

/// Runs `git <IDENTITY> <args>` in `cwd`: (exit code, stdout without trailing newlines,
/// stderr).
#[track_caller]
fn git(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    confined(cwd);
    let ceiling = temp_dir();
    let vars = std::env::vars_os()
        .filter(|(k, _)| !LEAKY_VARS.iter().any(|v| OsStr::new(v) == k))
        .chain(HERMETIC_ENV.iter().map(|(k, v)| (k.into(), v.into())))
        .chain([("GIT_CEILING_DIRECTORIES".into(), ceiling.into_os_string())]);
    let mut argv: Vec<&OsStr> = vec![OsStr::new("git")];
    argv.extend(IDENTITY.iter().chain(args).map(OsStr::new));
    match super::run_process(&argv, vars, cwd, None, Duration::from_secs(60), true) {
        Ok(out) => (
            out.code,
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches('\n')
                .to_string(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(out) => panic!("git {} timed out after 60 s\n{out}", args.join(" ")),
    }
}

/// `git <args>` in `cwd`; its stdout without trailing newlines. Panics when it fails.
#[track_caller]
pub fn run_git(cwd: &Path, args: &[&str]) -> String {
    let (code, out, err) = git(cwd, args);
    if code != 0 {
        panic!("git {} failed ({code}): {err}", args.join(" "));
    }
    out
}

/// `run_git` that does not check the exit code (gitfixture's `check=False`).
#[track_caller]
pub fn run_git_unchecked(cwd: &Path, args: &[&str]) -> String {
    git(cwd, args).1
}

/// A repo `my repo` in `root` with one commit (README.md) on `main`.
#[track_caller]
pub fn make_repo(root: &Path) -> PathBuf {
    make_repo_named(root, "my repo", "main")
}

/// A repo `name` in `parent` with one commit (README.md) on `default_branch`.
#[track_caller]
pub fn make_repo_named(parent: &Path, name: &str, default_branch: &str) -> PathBuf {
    add_root(parent);
    let repo = parent.join(name);
    confined(&repo);
    if repo.exists() {
        panic!("{} already exists", repo.display());
    }
    fs::create_dir_all(&repo).unwrap_or_else(|e| panic!("making {}: {e}", repo.display()));
    run_git(
        &repo,
        &["init", "--quiet", "--initial-branch", default_branch],
    );
    for (key, value) in [
        ("user.name", "Fixture User"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ] {
        run_git(&repo, &["config", key, value]);
    }
    commit(&repo, &[("README.md", "hello\n")], "initial");
    repo
}

/// A bare clone of `repo` beside it, added as `origin` with origin/HEAD set.
#[track_caller]
pub fn make_origin(repo: &Path) -> PathBuf {
    let parent = repo.parent().unwrap();
    let origin = parent.join(format!(
        "{} origin.git",
        repo.file_name().unwrap().to_string_lossy()
    ));
    let (repo_s, origin_s) = (repo.to_str().unwrap(), origin.to_str().unwrap());
    run_git(parent, &["clone", "--quiet", "--bare", repo_s, origin_s]);
    run_git(repo, &["remote", "add", "origin", origin_s]);
    run_git(repo, &["fetch", "--quiet", "origin"]);
    run_git(repo, &["remote", "set-head", "origin", "--auto"]);
    origin
}

/// Moves `branch` in the bare origin to `repo`'s `branch` (no push needed).
#[track_caller]
pub fn update_origin(origin: &Path, repo: &Path, branch: &str) -> String {
    let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
    run_git(
        origin,
        &["fetch", "--quiet", repo.to_str().unwrap(), &refspec],
    );
    run_git(origin, &["rev-parse", &format!("refs/heads/{branch}")])
}

/// Writes each (relative path, text) under `root`, making folders as needed.
#[track_caller]
pub fn write(root: &Path, files: &[(&str, &str)]) {
    confined(root);
    for (rel, _) in files {
        confined(&root.join(rel));
    }
    for (rel, text) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    }
}

/// Writes `files`, stages them and commits; returns the new HEAD sha.
#[track_caller]
pub fn commit(cwd: &Path, files: &[(&str, &str)], message: &str) -> String {
    write(cwd, files);
    let mut add = vec!["add", "--"];
    add.extend(files.iter().map(|(rel, _)| *rel));
    run_git(cwd, &add);
    run_git(cwd, &["commit", "--quiet", "-m", message]);
    run_git(cwd, &["rev-parse", "HEAD"])
}

/// The checked-out branch, or None on a detached HEAD.
#[track_caller]
pub fn current_branch(cwd: &Path) -> Option<String> {
    let out = run_git_unchecked(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    (!out.is_empty()).then_some(out)
}
