"""Git operations for runs and cards.

A run works on a run branch. Each card gets a card branch cut from the run tip;
accepted cards are fast-forwarded into the run branch. Nothing here pushes, and
nothing here touches the shared stash stack.

Requires git 2.36 or newer (`git worktree list --porcelain -z`).
"""
from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass
from fnmatch import fnmatchcase
from pathlib import Path, PurePosixPath
from typing import Collection, Mapping, Sequence

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
    # Every git call in this module goes through here. None, "", "." or any relative
    # path would make git run against the process cwd (the operator's repo), so only
    # an existing absolute directory is accepted, before anything runs.
    if cwd is None or not Path(cwd).is_absolute() or not Path(cwd).is_dir():
        raise GitError(f"git {' '.join(args)}: workspace {cwd!r} is not an existing absolute directory")
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
    """init.defaultBranch, main, master (the first that exists), then the main
    worktree's branch, then the only branch. The main worktree's branch comes late
    because after a run without a worktree it is the run branch."""
    # Ref names cannot contain newlines.
    branches = [r.removeprefix("refs/heads/")
                for r in git(repo, "for-each-ref", "--format=%(refname)", "refs/heads/").splitlines()]
    configured = _run(repo, ("config", "--get", "init.defaultBranch")).stdout.strip()
    for candidate in (configured, "main", "master"):
        if candidate and candidate in branches:
            return candidate
    worktrees = _worktrees(repo)
    ref = worktrees[0].get("branch", "") if worktrees else ""
    if ref.startswith("refs/heads/") and ref.removeprefix("refs/heads/") in branches:
        return ref.removeprefix("refs/heads/")
    if len(branches) == 1:
        return branches[0]
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
    left as is), then a worktree at <repo>/.worktrees/<slug> or a checkout in `repo`.
    Without a worktree, a repo with uncommitted tracked changes is refused (unless it is
    already on the run branch, i.e. a resumed run)."""
    _check_branch_name(repo, run_branch)
    if not slug or "/" in slug or "\0" in slug or slug.startswith("."):
        raise GitError(f"not a valid workspace slug: {slug!r}")
    if not use_worktree and _current_branch(repo) != run_branch and _has_changes(repo):
        # Checking out the run branch would carry them into the run.
        raise GitError(f"uncommitted changes to tracked files in {repo}; "
                       "commit or set them aside before a run without a worktree")
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


def is_ancestor(ws: Path, ancestor: str, descendant: str) -> bool:
    """True when commit `ancestor` is in the history of `descendant` (or is it)."""
    return _test(ws, "merge-base", "--is-ancestor", ancestor, descendant)


def rebase_onto(ws: Path, card_branch: str, onto: str) -> list[str]:
    """Rebase `card_branch` onto `onto` in `ws`. [] on success; on conflict the rebase is
    aborted (the branch is left unchanged) and the conflicted paths are returned."""
    r = _run(ws, (*NO_SIGN, "-c", "rebase.updateRefs=false", "rebase", "--quiet", "--no-autostash",
                  onto, card_branch))
    if r.returncode == 0:
        return []
    conflicts = _nul_list(_run(ws, ("diff", "--name-only", "-z", "--diff-filter=U")).stdout)
    if _rebase_in_progress(ws):
        git(ws, "rebase", "--abort")
    if not conflicts:
        raise _fail(r, ws)
    return sorted(set(conflicts))


def _git_path(ws: Path, name: str) -> Path:
    return Path(git(ws, "rev-parse", "--path-format=absolute", "--git-path", name))


def _rebase_in_progress(ws: Path) -> bool:
    return _git_path(ws, "rebase-merge").exists() or _git_path(ws, "rebase-apply").exists()


def _clear_operations(ws: Path) -> None:
    """Forget an in-progress rebase, am, cherry-pick or revert. HEAD, the index and
    files are untouched; merge and pick heads go with the following hard reset."""
    if (_git_path(ws, "rebase-apply") / "applying").exists():
        git(ws, "am", "--quit")
    elif _rebase_in_progress(ws):
        git(ws, "rebase", "--quit")
    if _git_path(ws, "sequencer").exists():
        git(ws, "cherry-pick", "--quit")


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


def _index_dirty(ws: Path) -> bool:
    return not _test(ws, "diff", "--cached", "--quiet", "--no-ext-diff", "HEAD", "--")


def _unstaged_paths(ws: Path) -> list[str]:
    # --name-only rather than --quiet: it ignores stat-only changes.
    return _nul_list(git(ws, "diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", "--"))


def _has_changes(ws: Path) -> bool:
    return _index_dirty(ws) or bool(_unstaged_paths(ws))


@dataclass(frozen=True)
class Violation:
    head_moved: bool  # HEAD is not `expected_head` on `expected_branch`
    index_dirty: bool  # the index differs from HEAD
    tracked_changes: tuple[str, ...]  # tracked files whose working copy differs from the index


def detect_violation(ws: Path, expected_head: str, expected_branch: str) -> Violation | None:
    """None when `ws` is on `expected_branch` at `expected_head` with a clean index and
    clean tracked files. Untracked files (build output) are not a violation."""
    head_moved = _current_branch(ws) != expected_branch or head(ws) != expected_head
    index_dirty = _index_dirty(ws)
    tracked = tuple(_unstaged_paths(ws))
    if head_moved or index_dirty or tracked:
        return Violation(head_moved, index_dirty, tracked)
    return None


def untracked_files(ws: Path) -> list[str]:
    """Untracked paths Git does not ignore, relative to the worktree root. A nested
    repository is listed as its folder with a trailing slash (Git never looks inside)."""
    return _nul_list(git(ws, "ls-files", "--others", "--exclude-standard", "-z", "--full-name", "--", ":/"))


def status_entries(ws: Path, timeout: float = GIT_TIMEOUT) -> tuple[bool, list[str]]:
    """(whether any tracked file changed, the untracked paths Git does not ignore)."""
    out = git(ws, "status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames", timeout=timeout)
    tracked, untracked = False, []
    for entry in _nul_list(out):
        if entry.startswith("?? "):
            untracked.append(entry[3:])
        else:
            tracked = True
    return tracked, untracked


def _pathspec_file(ws: Path, paths: Sequence[str]) -> str:
    """A NUL-separated pathspec file for --pathspec-from-file (argv stays short)."""
    fd, tmp = tempfile.mkstemp(prefix="leadhand-paths.")
    with os.fdopen(fd, "wb") as f:
        f.write(b"".join(p.encode("utf-8", "surrogateescape") + b"\0" for p in paths))
    return tmp


def stage_work(ws: Path, *, new_paths: Sequence[str], deadline: float | None = None) -> bool:
    """Stage a worker's changes for leadhand's own commit: every tracked change (`add -u`)
    and the untracked files in `new_paths` (those that appeared during the dispatch), never
    any other untracked file. `deadline` (time.monotonic) bounds every Git call. Returns
    whether anything is staged."""
    def timeout() -> float:
        if deadline is None:
            return GIT_TIMEOUT
        left = deadline - time.monotonic()
        if left <= 0:
            raise GitError("no time left before the deadline")
        return min(GIT_TIMEOUT, left)

    git(ws, "add", "-u", "--", ":/", timeout=timeout())
    files = [p for p in new_paths if not p.endswith("/")]
    if files:
        spec = _pathspec_file(ws, files)
        try:
            git(ws, "--literal-pathspecs", "add", f"--pathspec-from-file={spec}", "--pathspec-file-nul",
                timeout=timeout())
        finally:
            Path(spec).unlink(missing_ok=True)
    return not _test(ws, "diff", "--cached", "--quiet", "--no-ext-diff", "HEAD", "--")


def preserve_and_restore(ws: Path, *, ref: str, expected_head: str, expected_branch: str,
                         untracked: Collection[str] = ()) -> str:
    """Save the workspace state under `ref`, then put `ws` back on `expected_branch` at
    `expected_head` with a clean index and tracked files. Returns the saved commit.

    The saved commit comes from `git stash create`: its first parent is HEAD (so moved
    commits stay reachable), its second parent holds the index, and its tree holds the
    tracked working tree. With nothing to stash, the saved commit is HEAD itself. With
    an unmerged index (a conflicted merge, rebase or cherry-pick), stash cannot record
    the index, so the saved commit holds the tracked working tree (conflict markers
    included) with HEAD and, during a merge, MERGE_HEAD as parents.

    When `expected_branch` itself moved to a commit the saved commit does not contain
    (HEAD was elsewhere), that tip is kept on the sibling ref `<ref>-branch`. A deleted
    `expected_branch` is recreated. The autostash of a conflicted merge or rebase
    (`--autostash` or the autoStash settings) is kept on `<ref>-autostash` instead of
    landing on the shared stash. A sibling ref this call does not need is deleted, so
    `ref` and its siblings always describe one violation. In-progress rebase, am,
    cherry-pick and merge states are cleared. `refs/stash` is never touched. Untracked
    files are left alone, except the ones named in `untracked` (files Git does not ignore;
    a nested repository's folder is skipped): those are kept in the saved commit, a child
    of the saved state whose tree adds them, and removed from the worktree."""
    if not ref.startswith("refs/") or _run(ws, ("check-ref-format", ref)).returncode != 0:
        raise GitError(f"not a valid ref to preserve under: {ref!r}")
    _check_branch_name(ws, expected_branch)
    r = _run(ws, (*NO_SIGN, "stash", "create", "leadhand: preserved workspace"))
    if r.returncode == 0:
        saved = r.stdout.strip() or head(ws)
    elif _nul_list(git(ws, "ls-files", "--unmerged", "-z")):
        saved = _snapshot_conflicted(ws)
    else:
        raise _fail(r, ws)
    files = [p for p in untracked if not p.endswith("/") and os.path.lexists(ws / p)]
    if files:
        saved = _snapshot_untracked(ws, saved, files)
    git(ws, "update-ref", "-m", "leadhand: preserve workspace", ref, saved)
    branch_ref = f"refs/heads/{expected_branch}"
    moved_tip = None
    if _ref_exists(ws, branch_ref):
        tip = head(ws, branch_ref)
        if not (_test(ws, "merge-base", "--is-ancestor", tip, saved)
                or _test(ws, "merge-base", "--is-ancestor", tip, expected_head)):
            moved_tip = tip
    _set_sibling(ws, f"{ref}-branch", moved_tip, "leadhand: preserve moved branch")
    # Take any autostash out of the operation first: quitting it or resetting would
    # push it onto the shared refs/stash.
    _set_sibling(ws, f"{ref}-autostash", _take_autostash(ws), "leadhand: preserve autostash")
    _clear_operations(ws)
    git(ws, "checkout", "--quiet", "--force", "-B", expected_branch, expected_head, "--")
    git(ws, "reset", "--quiet", "--hard", expected_head)
    for p in files:
        _remove_untracked(ws, p)
    return saved


def _snapshot_untracked(ws: Path, saved: str, paths: Sequence[str]) -> str:
    """A commit whose parent is `saved` and whose tree is `saved`'s plus the untracked
    `paths`, built in a scratch index so the real one is untouched."""
    index = _git_path(ws, "index")
    fd, tmp = tempfile.mkstemp(dir=index.parent, prefix="leadhand-index.")
    os.close(fd)
    spec = _pathspec_file(ws, paths)
    try:
        env = {"GIT_INDEX_FILE": tmp}
        git(ws, "read-tree", f"{saved}^{{tree}}", env=env)
        git(ws, "--literal-pathspecs", "add", "--force", f"--pathspec-from-file={spec}", "--pathspec-file-nul",
            env=env)
        tree = git(ws, "write-tree", env=env)
    finally:
        Path(tmp).unlink(missing_ok=True)
        Path(spec).unlink(missing_ok=True)
    return git(ws, *NO_SIGN, "commit-tree", tree, "-p", saved, "-m", "leadhand: preserved untracked files")


def _remove_untracked(ws: Path, rel: str) -> None:
    """Remove one untracked file (a symlink itself, never its target), then any folders it
    leaves empty, up to the worktree root."""
    path = ws / rel
    if path.is_dir() and not path.is_symlink():
        return
    path.unlink(missing_ok=True)
    parent = path.parent
    root = Path(ws).resolve()
    while parent.resolve() != root and root in parent.resolve().parents:
        try:
            parent.rmdir()
        except OSError:
            break
        parent = parent.parent


def _set_sibling(ws: Path, name: str, sha: str | None, message: str) -> None:
    """Point `name` at `sha`, or delete it when None, so a ref set from an earlier
    call never lingers beside a later violation."""
    if sha is not None:
        git(ws, "update-ref", "-m", message, name, sha)
    elif _ref_exists(ws, name):
        git(ws, "update-ref", "-d", name)


def _take_autostash(ws: Path) -> str | None:
    """Detach the autostash of a conflicted merge or rebase and return its commit.
    Only one is expected; any further one is left for git to stash."""
    r = _run(ws, ("rev-parse", "--verify", "--quiet", "MERGE_AUTOSTASH"))
    if r.returncode == 0 and r.stdout.strip():
        git(ws, "update-ref", "-d", "MERGE_AUTOSTASH")
        return r.stdout.strip()
    for state in ("rebase-merge", "rebase-apply"):
        marker = _git_path(ws, state) / "autostash"
        if marker.exists():
            sha = head(ws, marker.read_text(encoding="utf-8").strip())
            marker.unlink()
            return sha
    return None


def _snapshot_conflicted(ws: Path) -> str:
    """Commit the tracked working tree of a workspace whose index has conflicts,
    using a copy of the index so the real one is untouched."""
    index = _git_path(ws, "index")
    fd, tmp = tempfile.mkstemp(dir=index.parent, prefix="leadhand-index.")
    os.close(fd)
    try:
        shutil.copyfile(index, tmp)
        env = {"GIT_INDEX_FILE": tmp}
        git(ws, "add", "--update", env=env)
        tree = git(ws, "write-tree", env=env)
    finally:
        Path(tmp).unlink(missing_ok=True)
    parents = ["-p", head(ws)]
    merge = _run(ws, ("rev-parse", "--verify", "--quiet", "MERGE_HEAD^{commit}"))
    if merge.returncode == 0:
        parents += ["-p", merge.stdout.strip()]
    return git(ws, *NO_SIGN, "commit-tree", tree, *parents, "-m", "leadhand: preserved conflicted workspace")


# -- paths -----------------------------------------------------------------

_TEST_DOC_DIRS = frozenset({"tests", "test", "spec", "docs"})
_TEST_NAMES = ("test_*", "*_test.*", "*.test.*", "*.spec.*")
_DOC_SUFFIXES = (".md", ".rst", ".txt")
_PRODUCTION_NAMES = ("requirements*.txt", "constraints*.txt", "cmakelists.txt")  # matched lowercased


def is_test_or_doc_path(path: str) -> bool:
    """Under a top-level tests/, test/, spec/ or docs/ directory; or named test_*,
    *_test.*, *.test.* or *.spec.*; or a *.md, *.rst or *.txt file. Dependency and
    build inputs (requirements*.txt, constraints*.txt, CMakeLists.txt) are production
    anywhere. Narrow on purpose: a wrong "test or doc" verdict skips re-review."""
    parts = PurePosixPath(path).parts
    if not parts:
        return False
    name = parts[-1]
    if any(fnmatchcase(name.lower(), pat) for pat in _PRODUCTION_NAMES):
        return False
    if len(parts) > 1 and parts[0] in _TEST_DOC_DIRS:
        return True
    return name.lower().endswith(_DOC_SUFFIXES) or any(fnmatchcase(name, pat) for pat in _TEST_NAMES)
