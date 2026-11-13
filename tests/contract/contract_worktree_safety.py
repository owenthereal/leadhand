#!/usr/bin/env python3
"""Safety behaviours of a worktree run that the Python suite checks only in place or only
through internals, and the operator-checkout invariant (ruling S1-R16).

Written for stage 1 from tests/test_engine_in_place.py, the in-place cases of
tests/test_gitops.py and the rulings they rest on (core run: Codex round 10, R30, R54', R60,
R63, R64; docs/compatibility.md, Security). In-place runs are gone (deliberate change 1); what
they tested that also holds for a run in its own worktree is a worktree scenario here.
SavedState and Workspace, at the end, convert the rest of tests/test_gitops.py's GitopsTests that
a run shows from outside: a violation's saved commit and sibling refs, the run's worktree and
the excludes, and leadhand's own commits made unsigned.

FakeSupervisor plays the run skill; a worker misbehaves through its step's `tamper`, which
runs in the harness against the run's worktree. Every safety outcome is judged from Git and
the filesystem directly: refs and the trees they hold, `ls-files -v` tags (assume-unchanged is
a lowercase tag, skip-worktree `S`), `core.sparseCheckout`, the files under a folder a worker
pointed elsewhere, and the operator's own checkout. The binary's events are asserted beside
that, never as the only evidence.
"""
import json
import os
import subprocess
import unittest
from pathlib import Path

from contract_cards import BY_RISK, CODEX_CARD, codex_world, commit_on, relaunch_env
from contract_finish import base_wt, drive_until_refused, gate_names, marker
from contract_integration import NOTE, parked_world, unpark
from gitfixture import IDENTITY, commit, current_branch, make_repo, run_git, write
from harness import read_json
from supervisor import FakeSupervisor, card

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
CARD1_FILE = "src/card1/c1-a1-implement-1.txt"
SPARSE_REFUSAL = "sparse checkouts are not supported"


ODD_NAME = b"tests/notes-\xff.txt"          # not UTF-8


def odd_name(world, ws, a):
    """A worker's tamper: commit ODD_NAME through Git's index alone (the file system here may
    refuse such a name), so the file is in the commit but not in the worktree."""
    notes = world.tmp / "notes blob.txt"
    notes.write_text("notes\n", encoding="utf-8")
    env = dict(os.environ)
    blob = subprocess.run(["git", "hash-object", "-w", str(notes)], cwd=ws, env=env, check=True,
                          capture_output=True, timeout=60).stdout.strip()
    for argv in ([b"git", b"update-index", b"--add", b"--cacheinfo", b"100644," + blob + b"," + ODD_NAME],
                 [b"git", *(x.encode() for x in IDENTITY), b"commit", b"--quiet", b"-m", b"odd name"]):
        subprocess.run(argv, cwd=ws, env=env, check=True, capture_output=True, timeout=60)


def found(w: FakeSupervisor, by: str = "check") -> list[dict]:
    return [e for e in w.events("violation") if e.get("stage") == "found" and (e.get("by") or "check") == by]


def requested(w: FakeSupervisor, dispatch_id: str) -> dict:
    return next(e for e in w.events("dispatch_requested") if e["dispatch_id"] == dispatch_id)


def tree_paths(repo: Path, rev: str) -> list[str]:
    return run_git(repo, "ls-tree", "-r", "--name-only", rev).splitlines()


def tags(cwd: Path) -> list[str]:
    """`git ls-files -v` lines: tag, space, path."""
    return run_git(cwd, "ls-files", "-v").splitlines()


def run_git_bytes(cwd: Path, *args: str) -> list[bytes]:
    """A NUL-separated Git listing as bytes (names that are not UTF-8 kept as they are)."""
    out = subprocess.run(["git", *args], cwd=cwd, env=dict(os.environ), check=True, capture_output=True,
                         timeout=60, stdin=subprocess.DEVNULL).stdout
    return [p for p in out.split(b"\0") if p]


def sparse_setting(cwd: Path) -> str:
    """core.sparseCheckout as that checkout sees it ("" when unset)."""
    return run_git(cwd, "config", "--bool", "--get", "core.sparseCheckout", check=False)


# --------------------------------------------------------------------------
# A reviewer hiding an edit behind an index flag (Codex round 10)

class HiddenIndexEntries(unittest.TestCase):
    """An index entry a reviewer marks assume-unchanged or skip-worktree hides its edit from
    `status` and `diff`. Leadhand clears the mark before the check, so the edit is a violation:
    preserved on refs/leadhand/<run id>/violations/<n>, the worktree restored, and the review
    run again by a fresh worker. The operator's own checkout (another index) is never touched."""

    def reviewed(self, tamper) -> tuple[FakeSupervisor, dict, str]:
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"tamper": tamper}, {})
        done = w.run()[-1]
        self.assertEqual(done["action"], "done")
        (v,) = found(w)
        self.assertEqual((v["dispatch_id"], v["dispatch_kind"]), ("c1-a1-review-1", "review"))
        self.assertEqual(v["ref"], f"refs/leadhand/{w.meta()['run_id']}/violations/1")
        tip = requested(w, "c1-a1-review-1")["expected_head"]
        # The edit is kept, on top of the card tip the reviewer was given.
        self.assertEqual(run_git(w.repo, "rev-parse", f"{v['ref']}^1"), tip)
        # Run again by a fresh worker; only the second review's verdict counts.
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"])
        self.assertIsNone(requested(w, "c1-a1-review-2")["resume_worker"])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-2"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assertEqual(w.branch_tip(RUN), tip)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")
        # The worktree is back on the card tip, every entry visible again and clean.
        ws = w.workspace()
        self.assertEqual(current_branch(ws), CARD1)
        self.assertEqual(run_git(ws, "rev-parse", "HEAD"), tip)
        self.assertEqual([t for t in tags(ws) if not t.startswith("H ")], [])
        self.assertEqual(run_git(ws, "status", "--porcelain", "--untracked-files=no"), "")
        # The operator's checkout keeps its own index and files.
        self.assertEqual((w.repo / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertEqual(tags(w.repo), ["H README.md"])
        self.assertEqual(current_branch(w.repo), "main")
        return w, v, tip

    def test_reviewer_assume_unchanged_edit_is_a_violation(self):
        def tamper(world, ws, a):
            run_git(ws, "update-index", "--assume-unchanged", "README.md")
            write(ws, {"README.md": "edited by the reviewer, hidden\n"})
            self.assertEqual(run_git(ws, "status", "--porcelain"), "")       # hidden indeed

        w, v, _ = self.reviewed(tamper)
        self.assertEqual(v["hidden"], ["README.md"])
        self.assertEqual(v["paths"], ["README.md"])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "edited by the reviewer, hidden")
        self.assertEqual((w.workspace() / "README.md").read_text(encoding="utf-8"), "hello\n")

    def test_reviewer_skip_worktree_edit_is_a_violation(self):
        # One entry edited, another deleted, both behind skip-worktree.
        def tamper(world, ws, a):
            run_git(ws, "update-index", "--skip-worktree", "README.md", CARD1_FILE)
            write(ws, {"README.md": "edited by the reviewer, hidden\n"})
            (ws / CARD1_FILE).unlink()
            self.assertEqual(run_git(ws, "status", "--porcelain"), "")       # hidden indeed

        w, v, tip = self.reviewed(tamper)
        self.assertEqual(sorted(v["hidden"]), ["README.md", CARD1_FILE])
        self.assertEqual(sorted(v["paths"]), ["README.md", CARD1_FILE])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "edited by the reviewer, hidden")
        self.assertNotIn(CARD1_FILE, tree_paths(w.repo, v["ref"]))
        ws = w.workspace()
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertEqual((ws / CARD1_FILE).read_text(encoding="utf-8"), "c1-a1-implement-1\n")
        self.assertIn(CARD1_FILE, tree_paths(w.repo, tip))


# --------------------------------------------------------------------------
# Sparse checkouts (R63, R64)

class SparseCheckouts(unittest.TestCase):
    def test_sparse_checkout_refused_by_check_start_and_run(self):
        # R63: a worktree run's worktrees would inherit a sparse checkout, so `check`, `start` and
        # every launch refuse a repository whose checkout is sparse, with short advice.
        w = FakeSupervisor(self, [card(1)])
        w.base = commit(w.repo, {"far/away.txt": "outside the sparse patterns\n"}, "far")
        w.write_deck([card(1)])
        sparse = ("sparse-checkout", "set", "--no-cone", "/README.md", "/src/")
        run_git(w.repo, *sparse)
        self.assertFalse((w.repo / "far" / "away.txt").exists())

        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        refusals = [ln for ln in r.stdout.splitlines() if SPARSE_REFUSAL in ln]
        self.assertEqual(len(refusals), 1, r)
        self.assertTrue(refusals[0].startswith("error: not supported in this version: "), r)

        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertIn(SPARSE_REFUSAL, r.stderr)
        self.assertNotIn("--no-skip-worktree", r.stderr)
        self.assertLess(len(r.stderr), 600, r)
        self.assertFalse((w.docs / "runs").exists())
        self.assertEqual(run_git(w.repo, "worktree", "list", "--porcelain").count("worktree "), 1)
        self.assertEqual(run_git(w.repo, "branch", "--list", "leadhand/*"), "")

        # Not sparse: the same deck starts.
        run_git(w.repo, "sparse-checkout", "disable")
        w.start()
        # Sparse again before the launch: `run` refuses it, and no supervisor starts.
        run_git(w.repo, *sparse)
        claude_log = w.tmp / "fake claude.log"
        r = w.cli("run", w.run_dir, env=dict(relaunch_env(), FAKE_CLAUDE_LOG=str(claude_log), FAKE_CLAUDE_SLEEP="0"))
        self.assertEqual(r.code, 1, r)
        self.assertIn(SPARSE_REFUSAL, r.stderr)
        self.assertFalse(claude_log.exists(), "a supervisor was started")
        self.assertEqual(w.events("resumed"), [])

    def test_worker_enabled_sparse_checkout_is_disabled_again(self):
        # R64: an implementer turns sparse checkout on in the run's worktree, which hides
        # README.md (skip-worktree, file removed). Before the review, leadhand clears the mark,
        # turns sparse checkout off again in that worktree, and keeps the deletion as the
        # implementer's leftovers; the operator's checkout never becomes sparse.
        def sparse_on(world, ws, a):
            run_git(ws, "sparse-checkout", "set", "--no-cone", "/src/")
            self.assertFalse((ws / "README.md").exists())
            self.assertIn("S README.md", tags(ws))

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": sparse_on})
        self.assertEqual(w.run()[-1]["action"], "done")
        (v,) = found(w, by="implementer")
        self.assertIn("README.md", v["hidden"])
        self.assertIn("README.md", v["paths"])
        self.assertNotIn("README.md", tree_paths(w.repo, v["ref"]))
        ws = w.workspace()
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertEqual([t for t in tags(ws) if not t.startswith("H ")], [])
        self.assertEqual(sparse_setting(ws), "false")
        self.assertNotEqual(sparse_setting(w.repo), "true")
        self.assertEqual((w.repo / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertEqual(tags(w.repo), ["H README.md"])
        # Leftovers void nothing: the review ran once and the card was accepted with README.md.
        self.assertEqual(found(w), [])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")


# --------------------------------------------------------------------------
# A symlink planted at a later dispatch's scratch folder (L2)

class ScratchFolders(unittest.TestCase):
    def test_symlinked_scratch_folder_refused_without_writing(self):
        # The implementer plants a symlink where the review's scratch folder goes. `next` refuses
        # with the path and writes nothing through it, as long as it is there; once the operator
        # removes it, the review's brief is written in the worktree and the run goes on.
        w = FakeSupervisor(self, [card(1)])
        outside = w.tmp / "outside"
        outside.mkdir()
        kept = {"brief.md": "the operator's brief\n", "report.md": "keep me\n", "reply.txt": "keep me too\n"}
        for name, text in kept.items():
            (outside / name).write_text(text, encoding="utf-8")
        planted = {}

        def plant(world, ws, a):
            link = ws / ".leadhand" / "1" / "c1-a1-review-1"
            link.symlink_to(outside, target_is_directory=True)
            planted["link"] = link

        w.start()
        w.script(1, "implement", {"tamper": plant})
        w.act(w.next())
        link = planted["link"]
        for _ in range(2):
            r = w.cli("next", w.run_dir)
            self.assertEqual(r.code, 1, r)
            self.assertTrue(r.stderr.startswith("error: "), r)
            self.assertIn(str(Path(".leadhand") / "1" / "c1-a1-review-1"), r.stderr)
        self.assertEqual(sorted(p.name for p in outside.iterdir()), sorted(kept))
        for name, text in kept.items():
            self.assertEqual((outside / name).read_text(encoding="utf-8"), text, name)
        self.assertEqual([e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "review"], [])

        link.unlink()                           # the operator removes it
        a = w.next()
        self.assertEqual((a["action"], a["dispatch_id"]), ("dispatch", "c1-a1-review-1"))
        brief = Path(a["brief"])
        self.assertEqual(brief, link / "brief.md")
        self.assertFalse(os.path.islink(link))
        self.assertTrue(brief.is_file())
        self.assertEqual(os.path.realpath(brief), str(Path(os.path.realpath(w.workspace())) / ".leadhand" / "1"
                                                      / "c1-a1-review-1" / "brief.md"))
        w.act(a)
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        for name, text in kept.items():
            self.assertEqual((outside / name).read_text(encoding="utf-8"), text, name)


# --------------------------------------------------------------------------
# Implementer leftovers before a check (R30)

class Leftovers(unittest.TestCase):
    def test_leftovers_preserved_on_ref_before_review(self):
        # The implementer commits, then leaves a staged edit, a different unstaged edit of the
        # same file, and an edit hidden behind assume-unchanged. Before the review they are kept
        # on refs/leadhand/<run id>/leftovers/1 (a stash-style commit: the work tree's files, the
        # index as its second parent), the worktree is put back on the card tip, and the review
        # is not voided.
        def leave(world, ws, a):
            write(ws, {"README.md": "staged by the implementer\n"})
            run_git(ws, "add", "--", "README.md")
            write(ws, {"README.md": "unstaged by the implementer\n"})
            run_git(ws, "update-index", "--assume-unchanged", CARD1_FILE)
            write(ws, {CARD1_FILE: "hidden by the implementer\n"})

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": leave})
        self.assertEqual(w.run()[-1]["action"], "done")
        (v,) = found(w, by="implementer")
        ref = f"refs/leadhand/{w.meta()['run_id']}/leftovers/1"
        self.assertEqual(v["ref"], ref)
        self.assertEqual(v["why"], "before review c1-a1-review-1")
        self.assertEqual(sorted(v["paths"]), ["README.md", CARD1_FILE])
        self.assertTrue(v["index_dirty"])
        self.assertEqual(v["hidden"], [CARD1_FILE])
        self.assertLess(v["seq"], requested(w, "c1-a1-review-1")["seq"])
        (restored,) = [e for e in w.events("violation") if e.get("by") == "implementer" and e.get("stage") == "restored"]
        self.assertEqual(run_git(w.repo, "rev-parse", ref), restored["saved"])

        tip = requested(w, "c1-a1-review-1")["expected_head"]
        self.assertEqual(run_git(w.repo, "rev-parse", f"{ref}^1"), tip)
        self.assertEqual(run_git(w.repo, "show", f"{ref}:README.md"), "unstaged by the implementer")
        self.assertEqual(run_git(w.repo, "show", f"{ref}^2:README.md"), "staged by the implementer")
        self.assertEqual(run_git(w.repo, "show", f"{ref}:{CARD1_FILE}"), "hidden by the implementer")
        self.assertEqual(run_git(w.repo, "show", f"{tip}:{CARD1_FILE}"), "c1-a1-implement-1")

        ws = w.workspace()
        self.assertEqual(run_git(ws, "rev-parse", "HEAD"), tip)
        self.assertEqual(run_git(ws, "status", "--porcelain", "--untracked-files=no"), "")
        self.assertEqual([t for t in tags(ws) if not t.startswith("H ")], [])
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertEqual((ws / CARD1_FILE).read_text(encoding="utf-8"), "c1-a1-implement-1\n")
        self.assertEqual(found(w), [])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-1"])
        self.assertEqual(w.branch_tip(RUN), tip)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")


# --------------------------------------------------------------------------
# The evidence base worktree's identity (R60)

class BaseWorktreeIdentity(unittest.TestCase):
    def test_foreign_worktree_at_base_path_is_never_reset(self):
        # A worktree this run did not add sits where the evidence base worktree goes, on the
        # operator's branch with a commit of its own and work of every kind. The evidence gate
        # is refused, again on every `next`, and nothing in that worktree changes.
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        path = base_wt(w)
        run_git(w.repo, "worktree", "add", "--quiet", "-b", "operator-notes", str(path), w.base)
        commit(path, {".gitignore": "*.log\n", "notes.md": "committed notes\n"}, "the operator's commit")
        write(path, {"staged.txt": "staged\n"})
        run_git(path, "add", "--", "staged.txt")
        write(path, {"README.md": "an unstaged edit\n", "untracked.txt": "untracked\n", "build.log": "ignored\n"})

        def state():
            return {"head": run_git(path, "rev-parse", "HEAD"),
                    "branch": run_git(path, "symbolic-ref", "--short", "HEAD"),
                    "index": run_git(path, "ls-files", "-s", "-v"),
                    "status": run_git(path, "status", "--porcelain", "--ignored", "--untracked-files=all"),
                    "files": {p: (path / p).read_text(encoding="utf-8")
                              for p in ("README.md", "notes.md", "staged.txt", "untracked.txt", "build.log")}}

        before = state()
        self.assertIn("!! build.log", before["status"].splitlines())
        for attempt in range(2):
            r = drive_until_refused(w) if attempt == 0 else w.cli("next", w.run_dir)
            self.assertEqual(r.code, 1, r)
            self.assertIn(f"error: {path} is a worktree this run did not add", r.stderr)
        self.assertEqual(state(), before)
        self.assertFalse(marker(path).exists())
        self.assertNotIn("evidence", gate_names(w))
        self.assertEqual([e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "evidence"], [])


# --------------------------------------------------------------------------
# A card branch the run did not record (R54')

class UnrecordedCardBranch(unittest.TestCase):
    def test_unrecorded_card_branch_adopted_only_at_run_tip(self):
        # A branch named like card 1's exists before the run cuts it. Holding a commit of its
        # own, it is not the run's: `next` refuses and leaves it alone. Exactly at the run tip
        # (a cut whose record a crash lost holds nothing of its own) it is taken up.
        w = FakeSupervisor(self, [card(1)])
        w.start()
        theirs = commit_on(w.repo, w.base)
        run_git(w.repo, "branch", CARD1, theirs)
        for _ in range(2):
            r = w.cli("next", w.run_dir)
            self.assertEqual(r.code, 1, r)
            self.assertIn(f"error: the card branch {CARD1} already exists but this run did not cut it", r.stderr)
            self.assertEqual(run_git(w.repo, "rev-parse", f"refs/heads/{CARD1}"), theirs)
            self.assertEqual(current_branch(w.workspace()), RUN)
            self.assertEqual(w.events("card_cut"), [])

        run_git(w.repo, "branch", "-f", CARD1, RUN)
        a = w.next()
        self.assertEqual((a["action"], a["dispatch_id"]), ("dispatch", "c1-a1-implement-1"))
        (cut,) = w.events("card_cut")
        self.assertEqual((cut["card"], cut["branch"], cut["base"]), (1, CARD1, w.base))
        self.assertEqual(current_branch(w.workspace()), CARD1)
        w.act(a)
        self.assertEqual(w.run()[-1]["action"], "done")
        (accepted,) = w.events("accepted")
        self.assertEqual(w.branch_tip(RUN), accepted["sha"])
        self.assertEqual(run_git(w.repo, "rev-parse", f"{accepted['sha']}^"), w.base)
        self.assertNotEqual(run_git(w.repo, "merge-base", theirs, RUN), theirs)


# --------------------------------------------------------------------------
# In-place behaviour that holds in a worktree run too

class WorktreeRewrites(unittest.TestCase):
    def test_an_ignored_file_a_worker_left_does_not_block_a_card_cut(self):
        # In a worktree run an ignored file in the run's worktree is a worker's or a build's,
        # never the operator's, so cutting a card branch may overwrite it (an in-place run
        # refused to). Card 1 tracks local.env (ignored by .gitignore) and is accepted; card 2
        # stops tracking it, leaves its own version behind and is parked; card 3 is cut at the
        # run tip, which tracks it again.
        def track(world, ws, a):
            write(ws, {"local.env": "card one's settings\n"})
            run_git(ws, "add", "--force", "--", "local.env")
            run_git(ws, "commit", "--quiet", "-m", "track local.env")

        def untrack(world, ws, a):
            run_git(ws, "rm", "--quiet", "--cached", "--", "local.env")
            run_git(ws, "commit", "--quiet", "-m", "stop tracking local.env")
            write(ws, {"local.env": "a worker's leftover\n"})
            self.assertIn("!! local.env", run_git(ws, "status", "--porcelain", "--ignored").splitlines())

        seen = {}

        def look(world, ws, a):
            seen["local.env"] = (ws / "local.env").read_text(encoding="utf-8")

        w = FakeSupervisor(self, [card(1), card(2), card(3)])
        w.base = commit(w.repo, {".gitignore": "local.env\n"}, "ignore local.env")
        w.write_deck([card(1), card(2), card(3)])
        w.start()
        w.script(1, "implement", {"tamper": track})
        w.script(2, "implement", {"tamper": untrack, "status": "QUESTION"})
        w.script(3, "implement", {"tamper": look})
        w.parks[2] = "decision needed: settings format"
        done = w.run()[-1]
        self.assertEqual(done["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 3])
        accepted = {e["card"]: e["sha"] for e in w.events("accepted")}
        cut = {e["card"]: e for e in w.events("card_cut")}
        self.assertEqual(cut[3]["base"], accepted[1])
        self.assertEqual(seen["local.env"], "card one's settings\n")
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:local.env"), "card one's settings")
        self.assertEqual(found(w, by="implementer"), [])

    def test_accept_moves_a_run_branch_checked_out_in_the_worktree(self):
        # A low-risk card under by-risk review is accepted straight from its implementer's
        # result, with no workspace check between. An implementer that commits on its card
        # branch and then leaves the run's worktree on the run branch has the acceptance
        # fast-forward a branch checked out there: the branch moves, and the worktree with it,
        # clean and holding the card's work.
        def to_run_branch(world, ws, a):
            run_git(ws, "checkout", "--quiet", RUN)

        w = FakeSupervisor(self, [card(1)], config=BY_RISK)
        w.start()
        w.script(1, "implement", {"tamper": to_run_branch})
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1"])
        (accepted,) = w.events("accepted")
        tip = accepted["sha"]
        self.assertEqual(run_git(w.repo, "rev-parse", f"refs/heads/{CARD1}"), tip)
        self.assertEqual(w.branch_tip(RUN), tip)
        ws = w.workspace()
        self.assertEqual(current_branch(ws), RUN)
        self.assertEqual(run_git(ws, "rev-parse", "HEAD"), tip)
        self.assertEqual(run_git(ws, "status", "--porcelain"), "")
        self.assertEqual((ws / CARD1_FILE).read_text(encoding="utf-8"), "c1-a1-implement-1\n")

    def test_check_diff_patch_keeps_the_standard_prefixes_whatever_the_repo_diff_settings(self):
        # The operator's diff settings in the repository's configuration, which the run's
        # worktree shares, never change the patch a test-only fix's diff check reads.
        w = FakeSupervisor(self, [card(1)])
        for key, value in (("diff.noprefix", "true"), ("diff.mnemonicPrefix", "true"),
                           ("diff.srcPrefix", "old/"), ("diff.dstPrefix", "new/"), ("diff.relative", "true")):
            run_git(w.repo, "config", key, value)
        w.start()
        w.script(1, "implement", {"files": {"src/card1/widget.py": "W = 1\n", "tests/test_widget.py": "assert True\n"}})
        w.script(1, "review", {"verdict": "defects", "findings": ["[medium] tests/test_widget.py — weak — assert more"]})
        w.script(1, "fix", {"files": {"tests/test_widget.py": "assert more\n"}})
        for _ in range(30):
            a = w.next()
            if a["action"] == "check_diff":
                break
            w.act(a)
        self.assertEqual((a["action"], a["dispatch_id"]), ("check_diff", "c1-a1-fix-1"))
        patch = Path(a["diff"]).read_text(encoding="utf-8")
        self.assertIn("diff --git a/tests/test_widget.py b/tests/test_widget.py\n", patch)
        self.assertIn("--- a/tests/test_widget.py\n+++ b/tests/test_widget.py\n", patch)
        self.assertIn("+assert more\n", patch)

    def test_json_output_escapes_undecodable_names(self):
        # Spec contract item 13: an undecodable byte 0xXX of a name Git lists appears in the JSON
        # the CLI prints as \udcXX, in ASCII JSON. The fix commits a test file whose name is not
        # UTF-8 (odd_name), so the diff check's `paths` carries it. The diff check's note,
        # non-ASCII text, goes the other way: `record` prints it, and writes it to events.jsonl,
        # as ASCII JSON escapes that parse back to the same text.
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "defects", "findings": ["[medium] tests/test_widget.py — weak — assert more"]})
        w.script(1, "fix", {"files": {"tests/test_widget.py": "assert more\n"}, "tamper": odd_name})
        for _ in range(30):
            r = w.cli("next", w.run_dir).ok()
            a = r.json()
            if a["action"] == "check_diff":
                break
            w.act(a)
        self.assertEqual((a["action"], a["dispatch_id"]), ("check_diff", "c1-a1-fix-1"))
        self.assertTrue(r.stdout.isascii(), r)
        self.assertIn('"tests/notes-\\udcff.txt"', r.stdout)
        self.assertEqual(sorted(a["paths"]), sorted([ODD_NAME.decode("utf-8", "surrogateescape"),
                                                     "tests/test_widget.py"]))
        self.assertIn(ODD_NAME, run_git_bytes(w.repo, "ls-tree", "-r", "--name-only", "-z", f"refs/heads/{CARD1}"))

        note = "café — the test-only fix looks right"
        r = w.cli("record", w.run_dir, "diff-check", "--card", "1", "--ok", "yes", "--note", note).ok()
        self.assertTrue(r.stdout.isascii(), r)
        self.assertEqual((r.json()["kind"], r.json()["note"]), ("diff_checked", note), r)
        raw = (w.run_dir / "events.jsonl").read_text(encoding="utf-8")
        (line,) = [line for line in raw.splitlines() if '"kind":"diff_checked"' in line]
        self.assertTrue(line.isascii(), line)
        self.assertEqual(json.loads(line)["note"], note)

    def test_events_jsonl_keeps_undecodable_names_as_escapes(self):
        # Spec contract item 13, on disk: an implementer that commits a name that is not UTF-8
        # (odd_name) leaves the run's worktree with that file missing, so the check before the
        # review finds a violation. Its `paths` is written to events.jsonl as \udcff in an
        # ASCII line, and a later reader (`status`) reads it back. Whether the worktree can then
        # be restored depends on the file system (APFS refuses the name), so that `next`'s
        # outcome is not asserted.
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": odd_name})
        w.act(w.next())
        w.cli("next", w.run_dir)                 # the check before the review
        (v,) = found(w, by="implementer")
        self.assertEqual(v["paths"], [ODD_NAME.decode("utf-8", "surrogateescape")])
        self.assertEqual(v["why"], "before review c1-a1-review-1")
        self.assertIn(ODD_NAME, run_git_bytes(w.repo, "ls-tree", "-r", "--name-only", "-z", f"refs/heads/{CARD1}"))
        run_git(w.repo, "rev-parse", "--verify", "--quiet", v["ref"])
        raw = (w.run_dir / "events.jsonl").read_text(encoding="utf-8")
        (line,) = [line for line in raw.splitlines() if '"kind":"violation"' in line]
        self.assertTrue(line.isascii(), line)
        self.assertIn('"paths":["tests/notes-\\udcff.txt"]', line)
        st = w.cli("status", w.run_dir)
        self.assertEqual(st.code, 0, st)
        self.assertIn(f"Violation by a implementer worker (dispatch unknown): its changes were preserved at "
                      f"{v['ref']} — next: inspect {v['ref']}", st.stdout.splitlines(), st)


# --------------------------------------------------------------------------
# The operator's checkout (ruling S1-R16)

class OperatorCheckout(unittest.TestCase):
    def test_operator_checkout_untouched_by_a_complete_run(self):
        # A gated three-card run (Codex implementers, the second leaving its work for the
        # runner to stage and commit) never touches the operator's checkout: its HEAD, its
        # index (tags included: an assume-unchanged entry stays one), `status --porcelain
        # --ignored` and the content of its tracked files are the same before and after, and
        # none of the operator's files reach the run branch.
        w = codex_world(self, [card(1), card(2, route=CODEX_CARD), card(3)], gates=True)
        repo = w.repo
        w.base = commit(repo, {".gitignore": "*.log\n", "notes/plan.md": "the plan\n",
                               "config/settings.toml": "level = 1\n"}, "operator files")
        w.write_deck([card(1), card(2, route=CODEX_CARD), card(3)])
        write(repo, {"notes/plan.md": "the plan, staged edit\n"})
        run_git(repo, "add", "--", "notes/plan.md")
        write(repo, {"README.md": "an unsaved edit\n", "my-notes.txt": "operator notes\n",
                     "scratch/todo list.txt": "later\n", "build.log": "the operator's build log\n"})
        run_git(repo, "update-index", "--assume-unchanged", "config/settings.toml")
        write(repo, {"config/settings.toml": "level = 2  # local, hidden\n"})
        tracked = ("README.md", "notes/plan.md", "config/settings.toml", ".gitignore")

        def state() -> dict:
            return {"HEAD": (run_git(repo, "symbolic-ref", "HEAD"), run_git(repo, "rev-parse", "HEAD")),
                    "index": run_git(repo, "ls-files", "-s", "-v"),
                    "status": run_git(repo, "status", "--porcelain", "--ignored").splitlines(),
                    "files": {p: (repo / p).read_bytes() for p in tracked}}

        before = state()
        self.assertIn("h config/settings.toml", run_git(repo, "ls-files", "-v").splitlines())
        w.start()
        # Every run records the untracked files its workspace holds at start (R48'), a worktree
        # run too: a fresh worktree holds none, and the operator's own untracked files are in
        # their checkout, not in it.
        self.assertEqual(read_json(w.run_dir / "untracked-at-start.json"), {"paths": []})
        w.script(2, "implement", {"commit": False})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done)
        self.assertEqual([e["card"] for e in w.events("accepted") if e["card"] != "final"], [1, 2, 3])
        after = state()
        # S1-R16: by design (spec contract item 7) the run's worktree and its evidence base
        # worktree live in <repo>/.worktrees/, which start excludes through info/exclude. That
        # one ignored entry is new; it is the only line taken out, and only from the listing
        # after the run.
        self.assertIn("!! .worktrees/", after["status"])
        after["status"] = [ln for ln in after["status"] if ln != "!! .worktrees/"]
        self.assertEqual(after, before)
        landed = run_git(repo, "diff", "--name-only", w.base, f"refs/heads/{RUN}").splitlines()
        for path in ("my-notes.txt", "scratch/todo list.txt", "build.log", "README.md", "notes/plan.md",
                     "config/settings.toml"):
            self.assertNotIn(path, landed)
        self.assertIn("src/card2/c2-a1-implement-1.txt", landed)


# --------------------------------------------------------------------------
# A violation's saved state: what tests/test_gitops.py checks of preserve_and_restore

def git_path(ws: Path, name: str) -> Path:
    return Path(run_git(ws, "rev-parse", "--path-format=absolute", "--git-path", name))


def ref_exists(repo: Path, ref: str) -> bool:
    return run_git(repo, "rev-parse", "--verify", "--quiet", ref, check=False) != ""


def sign_nothing_or_fail(repo: Path) -> None:
    """Every commit made with the repository's own settings tries to sign, and signing fails."""
    run_git(repo, "config", "commit.gpgsign", "true")
    run_git(repo, "config", "gpg.program", "false")


class SavedState(unittest.TestCase):
    """A reviewer leaves card 1's worktree in some state: a moved or deleted branch, a Git
    operation stopped on a conflict, an autostash, staged work. The violation saves it under
    refs/leadhand/<run id>/violations/<n> (a stash-style commit: first parent the HEAD it found,
    second the index) with sibling refs `<ref>-branch` (the card branch's moved tip, when HEAD
    was elsewhere) and `<ref>-autostash` (an operation's autostash), which the `restored` event
    lists as `siblings`; then the worktree is back on the card branch at the expected head,
    clean, with no operation in progress, and the review runs again. Judged from Git."""

    def reviewed(self, tamper, *, files=None, before=None) -> tuple[FakeSupervisor, dict, dict]:
        """Card 1's implementer writes `files` (or its default file); the first review tampers.
        `before(world)` runs before `start`. Returns the world, the `found` and `restored` events."""
        w = FakeSupervisor(self, [card(1)])
        if before:
            before(w)
        w.start()
        if files:
            w.script(1, "implement", {"files": files})
        w.script(1, "review", {"tamper": tamper}, {})
        self.assertEqual(w.run()[-1]["action"], "done")
        (v,) = found(w)
        self.assertEqual((v["dispatch_id"], v["ref"]),
                         ("c1-a1-review-1", f"refs/leadhand/{w.meta()['run_id']}/violations/1"))
        (done,) = [e for e in w.events("violation") if e.get("stage") == "restored"]
        self.assertEqual(run_git(w.repo, "rev-parse", v["ref"]), done["saved"])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        return w, v, done

    def assert_back_on_the_card(self, w: FakeSupervisor, v: dict) -> Path:
        ws = w.workspace()
        self.assertEqual(current_branch(ws), CARD1)
        self.assertEqual(run_git(ws, "rev-parse", "HEAD"), v["expected_head"])
        self.assertEqual(w.events("accepted")[0]["sha"], v["expected_head"])
        self.assertEqual(run_git(ws, "status", "--porcelain", "--untracked-files=no"), "")
        for name in ("MERGE_HEAD", "CHERRY_PICK_HEAD", "rebase-merge", "rebase-apply", "sequencer"):
            self.assertFalse(git_path(ws, name).exists(), name)
        self.assertEqual(run_git(ws, "stash", "list"), "")              # refs/stash untouched
        return ws

    def test_a_moved_card_tip_is_kept_when_head_is_detached(self):
        def tamper(world, ws, a):
            h = run_git(ws, "rev-parse", "HEAD")
            commit(ws, {"src/stray.txt": "stray\n"}, "reviewer commit on the card branch")
            run_git(ws, "checkout", "--quiet", "--detach", h)

        w, v, done = self.reviewed(tamper)
        stray = v["refs"][f"refs/heads/{CARD1}"]["new"]
        self.assertTrue(v["head_moved"])
        self.assertEqual(done["saved"], v["expected_head"])
        self.assertEqual(done["siblings"], [f"{v['ref']}-branch"])
        self.assertEqual(run_git(w.repo, "rev-parse", f"{v['ref']}-branch"), stray)
        self.assertEqual(run_git(w.repo, "show", f"{stray}:src/stray.txt"), "stray")
        self.assert_back_on_the_card(w, v)

    def test_a_moved_card_tip_is_kept_when_another_branch_is_checked_out(self):
        def tamper(world, ws, a):
            commit(ws, {"src/stray.txt": "stray\n"}, "reviewer commit on the card branch")
            run_git(ws, "checkout", "--quiet", RUN)

        w, v, done = self.reviewed(tamper)
        stray = v["refs"][f"refs/heads/{CARD1}"]["new"]
        self.assertEqual(done["saved"], w.base)                          # the run branch's head
        self.assertEqual(done["siblings"], [f"{v['ref']}-branch"])
        self.assertEqual(run_git(w.repo, "rev-parse", f"{v['ref']}-branch"), stray)
        self.assert_back_on_the_card(w, v)

    def test_a_deleted_card_branch_is_recreated(self):
        def tamper(world, ws, a):
            run_git(ws, "checkout", "--quiet", RUN)
            run_git(ws, "branch", "--quiet", "-D", CARD1)

        w, v, done = self.reviewed(tamper)
        self.assertIsNone(v["refs"][f"refs/heads/{CARD1}"]["new"])
        self.assertEqual(done["siblings"], [])
        self.assertFalse(ref_exists(w.repo, f"{v['ref']}-branch"))
        self.assert_back_on_the_card(w, v)

    def test_a_card_tip_already_in_the_saved_commit_is_not_kept_twice(self):
        def tamper(world, ws, a):
            commit(ws, {"README.md": "committed\n"}, "reviewer commit")
            write(ws, {"README.md": "dirty\n"})

        w, v, done = self.reviewed(tamper)
        moved = v["refs"][f"refs/heads/{CARD1}"]["new"]
        self.assertEqual(run_git(w.repo, "rev-parse", f"{v['ref']}^1"), moved)
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "dirty")
        self.assertEqual(done["siblings"], [])
        self.assertFalse(ref_exists(w.repo, f"{v['ref']}-branch"))
        ws = self.assert_back_on_the_card(w, v)
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "hello\n")

    def test_a_conflicted_merge_is_saved_and_cleared_without_signing(self):
        # The saved commit of an unmerged index is leadhand's own commit: never signed.
        side = {}

        def make_side(w):
            (side["sha"],) = side_branch(w.repo, "side", [{"README.md": "side\n"}])
            sign_nothing_or_fail(w.repo)

        def tamper(world, ws, a):
            run_git(ws, "merge", "--no-edit", "side", check=False)
            self.assertTrue(run_git(ws, "ls-files", "-u"))              # an unmerged index

        w, v, done = self.reviewed(tamper, files={"README.md": "card\n"}, before=make_side)
        saved = v["ref"]
        self.assertEqual(run_git(w.repo, "rev-parse", f"{saved}^1"), v["expected_head"])
        self.assertEqual(run_git(w.repo, "rev-parse", f"{saved}^2"), side["sha"])
        self.assertIn("<<<<<<<", run_git(w.repo, "show", f"{saved}:README.md"))
        ws = self.assert_back_on_the_card(w, v)
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "card\n")

    def test_a_conflicted_rebase_or_cherry_pick_is_saved_and_cleared(self):
        for operation in ("rebase", "cherry-pick"):
            with self.subTest(operation=operation):
                picks = {}

                def make_other(w):
                    picks["1"], picks["2"] = side_branch(w.repo, "other", [{"README.md": "other\n"},
                                                                           {"src/g.txt": "g\n"}])

                def tamper(world, ws, a, operation=operation):
                    if operation == "rebase":
                        run_git(ws, "rebase", "other", check=False)
                        self.assertTrue(git_path(ws, "rebase-merge").exists() or git_path(ws, "rebase-apply").exists())
                    else:
                        run_git(ws, "cherry-pick", picks["1"], picks["2"], check=False)
                        self.assertTrue(git_path(ws, "sequencer").exists())

                w, v, done = self.reviewed(tamper, files={"README.md": "card\n"}, before=make_other)
                self.assertIn("<<<<<<<", run_git(w.repo, "show", f"{v['ref']}:README.md"))
                ws = self.assert_back_on_the_card(w, v)
                self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "card\n")

    def test_an_operations_autostash_is_kept_beside_the_saved_commit(self):
        for operation in ("merge", "rebase"):
            with self.subTest(operation=operation):
                def make_side(w):
                    side_branch(w.repo, "side", [{"README.md": "side\n"}])

                def tamper(world, ws, a, operation=operation):
                    write(ws, {CARD1_FILE: "dirty\n"})                  # the reviewer's edit, autostashed
                    if operation == "merge":
                        run_git(ws, "merge", "--autostash", "--no-edit", "side", check=False)
                        self.assertTrue(ref_exists(ws, "MERGE_AUTOSTASH"))
                    else:
                        run_git(ws, "rebase", "--autostash", "side", check=False)
                        self.assertTrue(git_path(ws, "rebase-merge/autostash").exists()
                                        or git_path(ws, "rebase-apply/autostash").exists())

                w, v, done = self.reviewed(tamper, files={"README.md": "card\n", CARD1_FILE: "card file\n"},
                                           before=make_side)
                self.assertIn("<<<<<<<", run_git(w.repo, "show", f"{v['ref']}:README.md"))
                self.assertEqual(done["siblings"], [f"{v['ref']}-autostash"])
                self.assertEqual(run_git(w.repo, "show", f"{v['ref']}-autostash:{CARD1_FILE}"), "dirty")
                ws = self.assert_back_on_the_card(w, v)
                self.assertFalse(ref_exists(ws, "MERGE_AUTOSTASH"))
                self.assertEqual((ws / CARD1_FILE).read_text(encoding="utf-8"), "card file\n")

    def test_a_staged_change_whose_working_copy_was_put_back_is_kept(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "staged\n"})
            run_git(ws, "add", "--", "README.md")
            write(ws, {"README.md": "hello\n"})

        w, v, done = self.reviewed(tamper)
        self.assertTrue(v["index_dirty"])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}^2:README.md"), "staged")
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "hello")
        self.assert_back_on_the_card(w, v)

    def test_restore_returns_to_the_card_from_another_branch_and_keeps_untracked_files(self):
        def tamper(world, ws, a):
            run_git(ws, "checkout", "--quiet", "-b", "elsewhere")
            commit(ws, {"src/g.txt": "g\n"}, "elsewhere")
            write(ws, {"README.md": "staged\n", "scratch.log": "keep me\n"})
            run_git(ws, "add", "--", "README.md")

        w, v, done = self.reviewed(tamper)
        moved = run_git(w.repo, "rev-parse", "refs/heads/elsewhere")
        self.assertEqual(run_git(w.repo, "rev-parse", f"{v['ref']}^1"), moved)
        ws = self.assert_back_on_the_card(w, v)
        self.assertEqual(run_git(ws, "diff", "--cached", "--name-only"), "")
        self.assertEqual((ws / "scratch.log").read_text(encoding="utf-8"), "keep me\n")
        self.assertFalse((ws / "src" / "g.txt").exists())

    def test_a_tracked_edit_and_a_deletion_are_both_listed(self):
        def tamper(world, ws, a):
            write(ws, {CARD1_FILE: "edited\n"})
            (ws / "README.md").unlink()

        w, v, done = self.reviewed(tamper)
        self.assertEqual((v["head_moved"], v["index_dirty"], v["paths"]), (False, False, ["README.md", CARD1_FILE]))
        ws = self.assert_back_on_the_card(w, v)
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "hello\n")

    def test_paths_with_spaces_and_non_ascii_names(self):
        spaced, accented = "src/my dir/a file.txt", "src/café notes.md"

        def tamper(world, ws, a):
            write(ws, {spaced: "edited\n"})

        w, v, done = self.reviewed(tamper, files={spaced: "a\n", accented: "n\n"})
        self.assertEqual(v["paths"], [spaced])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:{spaced}"), "edited")
        ws = self.assert_back_on_the_card(w, v)
        self.assertEqual((ws / spaced).read_text(encoding="utf-8"), "a\n")
        tip = w.events("accepted")[0]["sha"]
        self.assertEqual(run_git(w.repo, "show", f"{tip}:{accented}"), "n")

    def test_a_nested_repository_at_a_gitlink_is_not_a_violation(self):
        # Leadhand's Git calls ignore submodules (diff.ignoreSubmodules=all, test_git_safety.py):
        # a reviewer who checks out a nested repository at a gitlink path, at another commit than
        # the gitlink records, changes nothing leadhand checks. Plain Git sees the gitlink as a
        # tracked change, so a binary that drops the setting finds a violation here.
        w = FakeSupervisor(self, [card(1)])
        nested = make_repo(w.tmp, name="nested repo")
        recorded = run_git(nested, "rev-parse", "HEAD")
        run_git(w.repo, "update-index", "--add", "--cacheinfo", f"160000,{recorded},src/sub")
        run_git(w.repo, "commit", "--quiet", "-m", "a submodule pointer")
        w.base = run_git(w.repo, "rev-parse", "HEAD")
        w.write_deck([card(1)])
        moved = commit(nested, {"README.md": "moved\n"}, "moved")
        seen = {}

        def populate(world, ws, a):
            write(ws, {"src/sub/.git": f"gitdir: {run_git(nested, 'rev-parse', '--absolute-git-dir')}\n",
                       "src/sub/README.md": "moved\n"})
            seen["sub"] = run_git(ws / "src" / "sub", "rev-parse", "HEAD")
            seen["plain"] = run_git(ws, "diff", "--name-only", "HEAD")

        w.start()
        w.script(1, "review", {"tamper": populate})
        self.assertEqual(w.run()[-1]["action"], "done")
        # The trap is armed: the nested repository is at another commit, and plain Git sees it.
        self.assertEqual(seen["sub"], moved)
        self.assertNotEqual(moved, recorded)
        self.assertEqual(seen["plain"], "src/sub")
        self.assertEqual(w.events("violation"), [])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])
        run_id = w.meta()["run_id"]
        self.assertEqual(run_git(w.repo, "for-each-ref", "--format=%(refname)",
                                 f"refs/leadhand/{run_id}/violations/"), "")
        (accepted,) = w.events("accepted")
        self.assertEqual(run_git(w.repo, "rev-parse", f"{accepted['sha']}:src/sub"), recorded)

    def test_a_stat_only_touch_and_untracked_output_are_not_a_violation(self):
        def tamper(world, ws, a):
            write(ws, {"build/out.o": "bin\n", "new file.txt": "x\n"})
            os.utime(ws / CARD1_FILE)

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"tamper": tamper})
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual(w.events("violation"), [])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])


def side_branch(repo: Path, name: str, files: list[dict]) -> list[str]:
    """Branch `name` off main in the operator's checkout, one commit per entry of `files`, then
    back on main; returns the commits."""
    run_git(repo, "checkout", "--quiet", "-b", name)
    shas = [commit(repo, f, f"{name} {i}") for i, f in enumerate(files, 1)]
    run_git(repo, "checkout", "--quiet", "main")
    return shas


# --------------------------------------------------------------------------
# The run's worktree and the excludes `start` adds (test_gitops.py)

class Workspace(unittest.TestCase):
    def test_start_adds_each_exclude_once_and_the_run_worktree_on_the_run_branch(self):
        w = FakeSupervisor(self, [card(1)])
        first = w.start()
        second = w.start(new=True)
        for run in (first, second):
            meta = read_json(run / "run.json")
            ws = Path(meta["workspace"])
            self.assertEqual(ws, w.repo.resolve() / ".worktrees" / run.name)
            self.assertEqual(current_branch(ws), meta["branch"])
            self.assertEqual(run_git(ws, "rev-parse", "HEAD"), w.base)
        exclude = (w.repo / ".git" / "info" / "exclude").read_text(encoding="utf-8").splitlines()
        self.assertEqual((exclude.count(".worktrees/"), exclude.count(".leadhand/")), (1, 1))
        write(w.repo, {".leadhand/state.json": "{}\n"})
        self.assertEqual(run_git(w.repo, "status", "--porcelain"), "")

    def test_the_integration_rebase_is_never_signed(self):
        # Card 1 is parked while card 2 is accepted; once unparked it is rebased onto the run tip
        # by leadhand itself, in a repository where every signing attempt fails.
        w = parked_world(self)
        sign_nothing_or_fail(w.repo)
        unpark(w, 1, NOTE)
        self.assertEqual(w.run()[-1]["action"], "done")
        (rebased,) = w.events("rebased")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(w.branch_tip(RUN), w.events("accepted")[1]["sha"])
        self.assertEqual(run_git(w.repo, "log", "-1", "--format=%G?", rebased["tip"]), "N")


if __name__ == "__main__":
    unittest.main()
