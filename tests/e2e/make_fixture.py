#!/usr/bin/env python3
"""Build the end-to-end fixture: a small Git repo, its bare origin, a deck and the config.

    python3 tests/e2e/make_fixture.py ROOT [--slow-card2]

ROOT is created (it must not exist, or be empty). Everything the fixture needs lives
under it, and several of its paths contain spaces on purpose (Review Focus 2):

    ROOT/fixture repo/        Git repo, branch main: package calc/ with add(), unittest
                              tests, .gitignore, and .leadhand.toml (committed)
    ROOT/origin.git/          bare clone; the repo's `origin` remote (a local path)
    ROOT/leadhand docs/       docs_dir (runs go to "ROOT/leadhand docs/fixture repo/runs/")
    ROOT/plans/fixture deck.md  tests/e2e/fixture-deck.md with the real base commit
    ROOT/user config.toml     empty: pass it as LEADHAND_CONFIG so the operator's own
                              ~/.config/leadhand/config.toml is never read
    ROOT/elsewhere/           an unrelated directory to launch from

Routes are the cheapest ones (Claude haiku/sonnet at low or medium, Codex gpt-6-luna
only); the two cards pin their implementer routes in the deck. Fixture commits are made
with commit signing off, and the repo's local config turns signing off for the run's
commits too. Standard library only; the only binary used is `git`.

--slow-card2 (used by run_e2e.py --interrupt) gives card 2 more work, so its Codex worker
runs long enough to be interrupted.

Prints a JSON object with the paths (also returned by `make_fixture`).
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
DECK_TEMPLATE = HERE / "fixture-deck.md"
BASE_TOKEN = "@BASE_SHA@"
GIT_TIMEOUT = 60

CALC_INIT = '''"""A tiny calculator: the leadhand end-to-end fixture."""


def add(a, b):
    """Return a + b."""
    return a + b
'''

TEST_CALC = '''import unittest

from calc import add


class AddTests(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)


if __name__ == "__main__":
    unittest.main()
'''

README = """# calc

A tiny calculator used as leadhand's end-to-end fixture.

- Build: `python3 -m compileall -q calc`
- Test: `python3 -m unittest discover -s tests`
"""

GITIGNORE = "__pycache__/\n*.pyc\n"

# The interrupted run (run_e2e.py --interrupt) needs card 2's Codex worker to live long
# enough to be killed, so its variant asks for more work on card 2. The default deck
# (tests/e2e/fixture-deck.md) is left unchanged.
CARD2_CONTEXT_END = "and a unittest test for it in tests/test_calc.py."
SLOW_CARD2_EXTRA = (" Write the tests first and run the suite after each one: separate test methods for "
                    "positive numbers, negative numbers, zero, floats (assertAlmostEqual), large integers and "
                    "the identity multiply(x, 1) == x; give multiply() a docstring with two examples; run "
                    "`python3 -m unittest discover -s tests -v` at the end and quote its last line in your report.")

# Cheapest routes. Every Codex route is gpt-6-luna. The deck pins card 1 to
# `claude sonnet low` and card 2 to `codex gpt-6-luna medium`.
ROLES = {
    "supervisor": {"claude": "sonnet low"},
    "implementer": {"claude": "sonnet low", "codex": "gpt-6-luna medium", "prefer": "codex"},
    "implementer_hard": {"claude": "sonnet low", "codex": "gpt-6-luna medium", "prefer": "codex"},
    "reviewer": {"claude": "haiku medium", "codex": "gpt-6-luna medium", "prefer": "other"},
    "reviewer_hard": {"claude": "haiku medium", "codex": "gpt-6-luna medium", "prefer": "other"},
    "final_reviewer": {"claude": "sonnet low", "codex": "gpt-6-luna medium", "prefer": "claude"},
    "verifier": {"claude": "haiku low", "codex": "gpt-6-luna low", "prefer": "claude"},
    "scout": {"claude": "haiku low", "codex": "gpt-6-luna low", "prefer": "claude"},
}


def _toml_str(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)   # a JSON string is a valid TOML basic string


def leadhand_toml(docs_dir: Path) -> str:
    lines = ["# leadhand end-to-end fixture: cheapest routes only.",
             'profile = "standard"',
             f"docs_dir = {_toml_str(str(docs_dir) + '/{repo}')}",
             ""]
    for role, routes in ROLES.items():
        lines.append(f"[roles.{role}]")
        for key, value in routes.items():
            lines.append(f"{key} = {_toml_str(value)}")
        lines.append("")
    return "\n".join(lines)


def git(cwd: Path, *args: str) -> str:
    r = subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True,
                       timeout=GIT_TIMEOUT, stdin=subprocess.DEVNULL)
    if r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed in {cwd}: {r.stderr.strip()}")
    return r.stdout.strip()


def make_fixture(root: Path, *, slow_card2: bool = False) -> dict:
    root = Path(os.path.abspath(root))
    if root.exists() and any(root.iterdir()):
        raise RuntimeError(f"{root} exists and is not empty")
    root.mkdir(parents=True, exist_ok=True)
    repo = root / "fixture repo"
    origin = root / "origin.git"
    docs = root / "leadhand docs"
    plans = root / "plans"
    deck = plans / "fixture deck.md"
    user_config = root / "user config.toml"
    elsewhere = root / "elsewhere"
    for d in (repo / "calc", repo / "tests", docs, plans, elsewhere):
        d.mkdir(parents=True)
    user_config.write_text("", encoding="utf-8")

    (repo / "calc" / "__init__.py").write_text(CALC_INIT, encoding="utf-8")
    (repo / "tests" / "test_calc.py").write_text(TEST_CALC, encoding="utf-8")
    (repo / "README.md").write_text(README, encoding="utf-8")
    (repo / ".gitignore").write_text(GITIGNORE, encoding="utf-8")
    (repo / ".leadhand.toml").write_text(leadhand_toml(docs), encoding="utf-8")

    git(repo, "init", "--quiet", "-b", "main")
    for key, value in (("user.name", "leadhand e2e"), ("user.email", "e2e@example.invalid"),
                       ("commit.gpgsign", "false"), ("tag.gpgsign", "false")):
        git(repo, "config", "--local", key, value)
    git(repo, "add", "-A")
    git(repo, "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "calc: add()")
    base_sha = git(repo, "rev-parse", "HEAD")

    git(root, "clone", "--quiet", "--bare", str(repo), str(origin))
    git(repo, "remote", "add", "origin", str(origin))
    git(repo, "fetch", "--quiet", "origin")
    git(repo, "branch", "--quiet", "--set-upstream-to", "origin/main", "main")
    git(repo, "remote", "set-head", "origin", "main")

    text = DECK_TEMPLATE.read_text(encoding="utf-8")
    if text.count(BASE_TOKEN) != 1:
        raise RuntimeError(f"{DECK_TEMPLATE} must contain {BASE_TOKEN} exactly once")
    text = text.replace(BASE_TOKEN, base_sha)
    if slow_card2:
        if text.count(CARD2_CONTEXT_END) != 2:     # card 1's context ends the same way; card 2's is last
            raise RuntimeError(f"{DECK_TEMPLATE}: card 2's context ending not found")
        head, sep, tail = text.rpartition(CARD2_CONTEXT_END)
        text = head + sep + SLOW_CARD2_EXTRA + tail
    deck.write_text(text, encoding="utf-8")

    return {"root": str(root), "repo": str(repo), "origin": str(origin), "docs_dir": str(docs),
            "runs_dir": str(docs / repo.name / "runs"), "deck": str(deck),
            "user_config": str(user_config), "elsewhere": str(elsewhere), "base_sha": base_sha,
            "branch": "leadhand/calc-e2e", "slow_card2": slow_card2}


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    slow = "--slow-card2" in args
    args = [a for a in args if a != "--slow-card2"]
    if len(args) != 1 or args[0] in ("-h", "--help"):
        print(__doc__.strip())
        return 0 if args and args[0] in ("-h", "--help") else 2
    try:
        info = make_fixture(Path(args[0]), slow_card2=slow)
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    print(json.dumps(info, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
