"""Hermetic Git repositories for tests.

Every Git command here and in the library under test runs with the operator's
global and system configuration shut out, so commits never sign, prompt or run
the operator's hooks.
"""
from __future__ import annotations

import os
import subprocess
import unittest
from pathlib import Path
from unittest import mock

HERMETIC_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
}
# Variables that would point Git at some other repository than the cwd.
LEAKY_VARS = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY")
IDENTITY = (
    "-c", "user.name=Fixture User",
    "-c", "user.email=fixture@example.invalid",
    "-c", "commit.gpgsign=false",
    "-c", "tag.gpgsign=false",
)


def hermetic(test: unittest.TestCase) -> None:
    """Patch os.environ for the test so library Git calls are hermetic too."""
    patcher = mock.patch.dict(os.environ, HERMETIC_ENV)
    patcher.start()
    test.addCleanup(patcher.stop)
    for name in LEAKY_VARS:
        os.environ.pop(name, None)


def run_git(cwd: Path, *args: str, check: bool = True) -> str:
    env = {k: v for k, v in os.environ.items() if k not in LEAKY_VARS}
    env.update(HERMETIC_ENV)
    r = subprocess.run(
        ["git", *IDENTITY, *args],
        cwd=cwd, env=env, capture_output=True, text=True, timeout=60,
        stdin=subprocess.DEVNULL,
    )
    if check and r.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed ({r.returncode}): {r.stderr}")
    return r.stdout.rstrip("\n")


def make_repo(parent: Path, *, name: str = "my repo", default_branch: str = "main") -> Path:
    """A repo with one commit (README.md) on `default_branch`."""
    repo = parent / name
    repo.mkdir(parents=True)
    run_git(repo, "init", "--quiet", "--initial-branch", default_branch)
    for key, value in (
        ("user.name", "Fixture User"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ):
        run_git(repo, "config", key, value)
    commit(repo, {"README.md": "hello\n"}, "initial")
    return repo


def make_origin(repo: Path) -> Path:
    """A bare clone of `repo` beside it, added as `origin` with origin/HEAD set."""
    origin = repo.parent / f"{repo.name} origin.git"
    run_git(repo.parent, "clone", "--quiet", "--bare", str(repo), str(origin))
    run_git(repo, "remote", "add", "origin", str(origin))
    run_git(repo, "fetch", "--quiet", "origin")
    run_git(repo, "remote", "set-head", "origin", "--auto")
    return origin


def update_origin(origin: Path, repo: Path, branch: str) -> str:
    """Move `branch` in the bare origin to `repo`'s `branch` (no push needed)."""
    run_git(origin, "fetch", "--quiet", str(repo), f"+refs/heads/{branch}:refs/heads/{branch}")
    return run_git(origin, "rev-parse", f"refs/heads/{branch}")


def write(root: Path, files: dict[str, str]) -> None:
    for rel, text in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)


def commit(cwd: Path, files: dict[str, str], message: str) -> str:
    """Write `files`, stage them and commit; return the new HEAD sha."""
    write(cwd, files)
    run_git(cwd, "add", "--", *files)
    run_git(cwd, "commit", "--quiet", "-m", message)
    return run_git(cwd, "rev-parse", "HEAD")


def current_branch(cwd: Path) -> str | None:
    out = run_git(cwd, "symbolic-ref", "--quiet", "--short", "HEAD", check=False)
    return out or None
