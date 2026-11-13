#!/usr/bin/env python3
"""Safety behaviours of a worktree run that the Python suite checks only in place or only
through internals, and the operator-checkout invariant (ruling S1-R16).

Written for stage 1 from tests/test_engine_in_place.py, the in-place cases of
tests/test_gitops.py and the rulings they rest on (core run: Codex round 10, R30, R54', R60,
R63, R64; docs/compatibility.md, Security). In-place runs are gone (deliberate change 1); what
they tested that also holds for a run in its own worktree is a worktree scenario here.

FakeSupervisor plays the run skill; a worker misbehaves through its step's `tamper`, which
runs in the harness against the run's worktree. Every safety outcome is judged from Git and
the filesystem directly: refs and the trees they hold, `ls-files -v` tags (assume-unchanged is
a lowercase tag, skip-worktree `S`), `core.sparseCheckout`, the files under a folder a worker
pointed elsewhere, and the operator's own checkout. The binary's events are asserted beside
that, never as the only evidence.
"""
import os
import subprocess
import unittest
from pathlib import Path

from contract_cards import CODEX_CARD, codex_world, commit_on, relaunch_env
from contract_finish import base_wt, drive_until_refused, gate_names, marker
from gitfixture import IDENTITY, commit, current_branch, run_git, write
from supervisor import FakeSupervisor, card

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
CARD1_FILE = "src/card1/c1-a1-implement-1.txt"
SPARSE_REFUSAL = "sparse checkouts are not supported"


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
        # UTF-8 (through Git's index alone: the file system here may refuse such a name), so the
        # diff check's `paths` carries it.
        name = b"tests/notes-\xff.txt"

        def odd_name(world, ws, a):
            notes = world.tmp / "notes blob.txt"
            notes.write_text("notes\n", encoding="utf-8")
            env = dict(os.environ)
            blob = subprocess.run(["git", "hash-object", "-w", str(notes)], cwd=ws, env=env, check=True,
                                  capture_output=True, timeout=60).stdout.strip()
            for argv in ([b"git", b"update-index", b"--add", b"--cacheinfo", b"100644," + blob + b"," + name],
                         [b"git", *(x.encode() for x in IDENTITY), b"commit", b"--quiet", b"-m", b"odd name"]):
                subprocess.run(argv, cwd=ws, env=env, check=True, capture_output=True, timeout=60)

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
        self.assertEqual(sorted(a["paths"]), sorted([name.decode("utf-8", "surrogateescape"), "tests/test_widget.py"]))
        self.assertIn(name, run_git_bytes(w.repo, "ls-tree", "-r", "--name-only", "-z", f"refs/heads/{CARD1}"))


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


if __name__ == "__main__":
    unittest.main()
