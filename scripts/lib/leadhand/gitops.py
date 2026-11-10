"""Git operations for runs and cards.

A run works on a run branch. Each card gets a card branch cut from the run tip;
accepted cards are fast-forwarded into the run branch. Nothing here pushes, and
nothing here touches the shared stash stack.
"""
from __future__ import annotations

import os
import subprocess
import tempfile
from dataclasses import dataclass
from fnmatch import fnmatchcase
from pathlib import Path, PurePosixPath
from typing import Mapping

GIT_TIMEOUT = 120.0
FETCH_TIMEOUT = 600.0
# Commits the library itself creates (rebase, stash create) are never signed:
# signing may prompt, and these commits are leadhand's own bookkeeping.
NO_SIGN = ("-c", "commit.gpgsign=false")
# Variables that would point Git at another repository than the cwd.
_REPO_VARS = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY")


class GitError(RuntimeError):
    pass


def _run(cwd: Path, args: tuple[str, ...] | list[str], env: Mapping[str, str] | None = None,
         timeout: float = GIT_TIMEOUT) -> subprocess.CompletedProcess[str]:
    full_env = {k: v for k, v in os.environ.items() if k not in _REPO_VARS}
    full_env["GIT_TERMINAL_PROMPT"] = "0"
    if env:
        full_env.update(env)
    try:
        return subprocess.run(
            ["git", *args], cwd=cwd, env=full_env, capture_output=True, stdin=subprocess.DEVNULL,
            text=True, encoding="utf-8", errors="surrogateescape", timeout=timeout,
        )
    except subprocess.TimeoutExpired as e:
        raise GitError(f"git {' '.join(args)}: timed out after {timeout:g}s (in {cwd})") from e
    except OSError as e:
        raise GitError(f"git {' '.join(args)}: {e} (in {cwd})") from e


def _fail(r: subprocess.CompletedProcess[str], cwd: Path) -> GitError:
    cmd = " ".join(r.args[1:])
    detail = (r.stderr or r.stdout).strip()
    return GitError(f"git {cmd} failed with exit {r.returncode} (in {cwd}): {detail}")


def git(cwd: Path, *args: str, env: Mapping[str, str] | None = None, timeout: float = GIT_TIMEOUT) -> str:
    """Run git in `cwd`; return stdout without trailing newlines. `env` overlays os.environ."""
    r = _run(cwd, args, env, timeout)
    if r.returncode != 0:
        raise _fail(r, cwd)
    return r.stdout.rstrip("\n")


def _test(cwd: Path, *args: str) -> bool:
    """Run a git command whose exit status answers yes (0) or no (1)."""
    r = _run(cwd, args)
    if r.returncode not in (0, 1):
        raise _fail(r, cwd)
    return r.returncode == 0


def _nul_list(out: str) -> list[str]:
    return [p for p in out.split("\0") if p]


def git_common_dir(repo: Path) -> Path:
    return Path(git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir"))


def head(ws: Path, ref: str = "HEAD") -> str:
    return git(ws, "rev-parse", "--verify", f"{ref}^{{commit}}")


def _ref_exists(cwd: Path, ref: str) -> bool:
    return _test(cwd, "show-ref", "--verify", "--quiet", ref)


def _check_branch_name(cwd: Path, name: str) -> None:
    if name.startswith("-") or _run(cwd, ("check-ref-format", "--branch", name)).returncode != 0:
        raise GitError(f"not a valid branch name: {name!r}")


def _current_branch(ws: Path) -> str | None:
    r = _run(ws, ("symbolic-ref", "--quiet", "HEAD"))
    if r.returncode == 1:
        return None  # detached
    if r.returncode != 0:
        raise _fail(r, ws)
    ref = r.stdout.strip()
    return ref.removeprefix("refs/heads/") if ref.startswith("refs/heads/") else None


def _worktrees(cwd: Path) -> list[dict[str, str]]:
    """Entries of `git worktree list --porcelain -z`; the main worktree comes first."""
    entries: list[dict[str, str]] = []
    for field in git(cwd, "worktree", "list", "--porcelain", "-z").split("\0"):
        if not field:
            continue
        key, _, value = field.partition(" ")
        if key == "worktree":
            entries.append({})
        if entries:
            entries[-1][key] = value
    return entries


# -- base ------------------------------------------------------------------


def default_base(repo: Path) -> tuple[str, str]:
    """The run's base: ("origin/<default>", sha) after a fetch when `origin` exists,
    otherwise (<local default branch>, sha). The ref string records which."""
    if "origin" in git(repo, "remote").split():
        git(repo, "fetch", "--quiet", "origin", timeout=FETCH_TIMEOUT)
        branch = _origin_default_branch(repo)
        return f"origin/{branch}", head(repo, f"refs/remotes/origin/{branch}")
    branch = _local_default_branch(repo)
    return branch, head(repo, f"refs/heads/{branch}")


def _origin_default_branch(repo: Path) -> str:
    r = _run(repo, ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"))
    if r.returncode == 0 and r.stdout.strip().startswith("refs/remotes/origin/"):
        return r.stdout.strip().removeprefix("refs/remotes/origin/")
    for line in git(repo, "ls-remote", "--symref", "origin", "HEAD", timeout=FETCH_TIMEOUT).splitlines():
        target, _, name = line.partition("\t")
        if name == "HEAD" and target.startswith("ref: refs/heads/"):
            return target.removeprefix("ref: refs/heads/")
    for candidate in ("main", "master"):
        if _ref_exists(repo, f"refs/remotes/origin/{candidate}"):
            return candidate
    raise GitError(f"cannot determine the default branch of origin (in {repo})")


def _local_default_branch(repo: Path) -> str:
    worktrees = _worktrees(repo)
    ref = worktrees[0].get("branch", "") if worktrees else ""
    if ref.startswith("refs/heads/") and _ref_exists(repo, ref):
        return ref.removeprefix("refs/heads/")
    branches = _nul_list(git(repo, "for-each-ref", "--format=%(refname:short)%00", "refs/heads/"))
    if len(branches) == 1:
        return branches[0]
    for candidate in ("main", "master"):
        if candidate in branches:
            return candidate
    raise GitError(f"cannot determine the local default branch (in {repo})")


# -- workspace -------------------------------------------------------------


def ensure_excluded(repo: Path, pattern: str) -> None:
    """Add `pattern` to <git common dir>/info/exclude once (atomic replace)."""
    path = git_common_dir(repo) / "info" / "exclude"
    path.parent.mkdir(parents=True, exist_ok=True)
    text = path.read_text(encoding="utf-8") if path.exists() else ""
    if pattern in (line.strip() for line in text.splitlines()):
        return
    if text and not text.endswith("\n"):
        text += "\n"
    mode = path.stat().st_mode & 0o777 if path.exists() else 0o644
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=".exclude.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text + pattern + "\n")
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def prepare_workspace(repo: Path, *, run_branch: str, base_sha: str, slug: str, use_worktree: bool) -> Path:
    """Idempotent: create `run_branch` at `base_sha` when missing (an existing one is
    left as is), then a worktree at <repo>/.worktrees/<slug> or a checkout in `repo`."""
    _check_branch_name(repo, run_branch)
    ensure_excluded(repo, ".worktrees/")
    ensure_excluded(repo, ".leadhand/")
    ref = f"refs/heads/{run_branch}"
    if not _ref_exists(repo, ref):
        # Empty old value: create only, never overwrite a branch made meanwhile.
        git(repo, "update-ref", "-m", "leadhand: create run branch", ref, head(repo, base_sha), "")
    if not use_worktree:
        if _current_branch(repo) != run_branch:
            git(repo, "checkout", "--quiet", run_branch, "--")
        return repo
    ws = repo / ".worktrees" / slug
    registered = {Path(w["worktree"]).resolve() for w in _worktrees(repo) if "worktree" in w}
    if ws.exists() and ws.resolve() in registered:
        return ws
    git(repo, "worktree", "add", "--quiet", str(ws), run_branch)
    return ws


# -- cards -----------------------------------------------------------------


def card_branch_name(run_branch: str, card: int | str) -> str:
    return f"{run_branch}--card-{card}"


def cut_card_branch(ws: Path, run_branch: str, card: int | str) -> tuple[str, str]:
    """Check out the card branch in `ws`, creating it at the run tip when missing.
    Returns (branch, base); for a reused branch the base is its merge base with the run."""
    branch = card_branch_name(run_branch, card)
    _check_branch_name(ws, branch)
    ref = f"refs/heads/{branch}"
    if _ref_exists(ws, ref):
        base = git(ws, "merge-base", f"refs/heads/{run_branch}", ref)
    else:
        base = head(ws, f"refs/heads/{run_branch}")
        git(ws, "update-ref", "-m", "leadhand: cut card branch", ref, base, "")
    if _current_branch(ws) != branch:
        git(ws, "checkout", "--quiet", branch, "--")
    return branch, base


def needs_integration(ws: Path, run_branch: str, card_branch: str) -> bool:
    """True when the run tip is not an ancestor of the card tip."""
    return not _test(ws, "merge-base", "--is-ancestor", f"refs/heads/{run_branch}", f"refs/heads/{card_branch}")


def rebase_onto(ws: Path, card_branch: str, onto: str) -> list[str]:
    """Rebase `card_branch` onto `onto` in `ws`. [] on success; on conflict the rebase is
    aborted (the branch is left unchanged) and the conflicted paths are returned."""
    r = _run(ws, (*NO_SIGN, "rebase", "--quiet", "--no-autostash", "--no-update-refs",
                  onto, card_branch))
    if r.returncode == 0:
        return []
    conflicts = _nul_list(_run(ws, ("diff", "--name-only", "-z", "--diff-filter=U")).stdout)
    if _rebase_in_progress(ws):
        git(ws, "rebase", "--abort")
    if not conflicts:
        raise _fail(r, ws)
    return sorted(set(conflicts))


def _rebase_in_progress(ws: Path) -> bool:
    for state in ("rebase-merge", "rebase-apply"):
        if Path(git(ws, "rev-parse", "--path-format=absolute", "--git-path", state)).exists():
            return True
    return False


def fast_forward(ws: Path, run_branch: str, to_sha: str) -> str:
    """Move `run_branch` forward to `to_sha` and return the run tip. Idempotent: a run
    branch that already contains `to_sha` is left alone. GitError if not a fast-forward."""
    ref = f"refs/heads/{run_branch}"
    target = head(ws, to_sha)
    current = head(ws, ref)
    if current == target or _test(ws, "merge-base", "--is-ancestor", target, current):
        return current
    if not _test(ws, "merge-base", "--is-ancestor", current, target):
        raise GitError(f"not a fast-forward: {run_branch} is at {current}, which {target} does not contain")
    if _current_branch(ws) == run_branch:
        git(ws, "merge", "--quiet", "--ff-only", target)
        return head(ws, ref)
    for w in _worktrees(ws):
        if w.get("branch") == ref:
            raise GitError(f"{run_branch} is checked out in {w['worktree']}; cannot move it from {ws}")
    git(ws, "update-ref", "-m", "leadhand: fast-forward", ref, target, current)
    return target


def changed_paths(ws: Path, a: str, b: str) -> list[str]:
    """Paths that differ between commits `a` and `b` (renames as delete + add)."""
    return _nul_list(git(ws, "diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", a, b, "--"))


# -- workspace violations --------------------------------------------------


@dataclass(frozen=True)
class Violation:
    head_moved: bool  # HEAD is not `expected_head` on `expected_branch`
    index_dirty: bool  # the index differs from HEAD
    tracked_changes: tuple[str, ...]  # tracked files whose working copy differs from the index


def detect_violation(ws: Path, expected_head: str, expected_branch: str) -> Violation | None:
    """None when `ws` is on `expected_branch` at `expected_head` with a clean index and
    clean tracked files. Untracked files (build output) are not a violation."""
    head_moved = _current_branch(ws) != expected_branch or head(ws) != expected_head
    index_dirty = not _test(ws, "diff", "--cached", "--quiet", "--no-ext-diff", "HEAD", "--")
    tracked = tuple(_nul_list(git(ws, "diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--")))
    if head_moved or index_dirty or tracked:
        return Violation(head_moved, index_dirty, tracked)
    return None


def preserve_and_restore(ws: Path, *, ref: str, expected_head: str, expected_branch: str) -> str:
    """Save the workspace state under `ref`, then put `ws` back on `expected_branch` at
    `expected_head` with a clean index and tracked files. Returns the saved commit.

    The saved commit comes from `git stash create`: its first parent is HEAD (so moved
    commits stay reachable), its second parent holds the index, and its tree holds the
    tracked working tree. With nothing to stash, the saved commit is HEAD itself.
    `refs/stash` is never touched. Untracked files are left alone."""
    if not ref.startswith("refs/") or _run(ws, ("check-ref-format", ref)).returncode != 0:
        raise GitError(f"not a valid ref to preserve under: {ref!r}")
    saved = git(ws, *NO_SIGN, "stash", "create", "leadhand: preserved workspace").strip() or head(ws)
    git(ws, "update-ref", "-m", "leadhand: preserve workspace", ref, saved)
    if _rebase_in_progress(ws):
        git(ws, "rebase", "--quit")
    git(ws, "checkout", "--quiet", "--force", expected_branch, "--")
    git(ws, "reset", "--quiet", "--hard", expected_head)
    return saved


# -- paths -----------------------------------------------------------------

_TEST_DOC_DIRS = frozenset({"tests", "test", "spec", "docs"})
_TEST_NAMES = ("test_*", "*_test.*", "*.test.*", "*.spec.*")
_DOC_SUFFIXES = (".md", ".rst", ".txt")


def is_test_or_doc_path(path: str) -> bool:
    """Under a tests/, test/, spec/ or docs/ directory (at any depth); or named test_*,
    *_test.*, *.test.* or *.spec.*; or a *.md, *.rst or *.txt file."""
    parts = PurePosixPath(path).parts
    if not parts:
        return False
    if any(p in _TEST_DOC_DIRS for p in parts[:-1]):
        return True
    name = parts[-1]
    return name.lower().endswith(_DOC_SUFFIXES) or any(fnmatchcase(name, pat) for pat in _TEST_NAMES)
