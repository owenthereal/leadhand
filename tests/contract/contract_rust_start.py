"""`start` in Rust: in-place runs refused and the repository taken from the main working tree
(deliberate change 1), the run record in the run worktree's admin folder (spec, "Which repository
and run a directory belongs to"), and the conventions for the commands a person types (`--json`,
the fix command alone on the last line). Rust-only.

`start` keeps its contract output: `run: <dir>` for a new run, or its note naming the existing
unfinished run. The reference-only test `contract_layouts.InPlace` pins the Python's acceptance of
`run.worktree = false`.
"""
import os
import shlex
import subprocess
import unittest
from pathlib import Path

from gitfixture import commit, run_git
from harness import read_json, run_cli, rust_only
from supervisor import FakeSupervisor, card

NOT_SUPPORTED = "not supported in this version: "
RUN_RECORD = "leadhand-run-dir"
BASE_MARKER = "leadhand-run"


def run_line(result) -> Path:
    """The run directory `start` printed (`run: <dir>`, or `existing run: <dir>`)."""
    for line in result.stdout.splitlines():
        for prefix in ("run: ", "existing run: "):
            if line.startswith(prefix):
                return Path(line[len(prefix):])
    raise AssertionError(f"leadhand start printed no `run: <dir>` line\n{result}")


def branches(repo) -> list[str]:
    return run_git(repo, "for-each-ref", "--format=%(refname)", "refs/heads/").splitlines()


def admin_dir(ws: Path) -> Path:
    """The linked worktree's admin folder in the common directory (`<common>/worktrees/<name>`)."""
    return Path(run_git(ws, "rev-parse", "--absolute-git-dir"))


@rust_only(uses=("start", "status"))
class Start(unittest.TestCase):
    def start(self, w, *flags, repo=None):
        return w.cli("start", w.deck_path, "--repo", repo or w.repo, *flags)

    def assert_nothing_created(self, w, before):
        self.assertFalse((w.docs / "runs").exists())
        self.assertEqual(branches(w.repo), before)
        self.assertFalse((w.repo / ".worktrees").exists())
        self.assertEqual(run_git(w.repo, "worktree", "list", "--porcelain").count("worktree "), 1)

    def test_start_refuses_run_worktree_false(self):
        w = FakeSupervisor(self, [card(1)], config="[run]\nworktree = false\n")
        before = branches(w.repo)
        r = self.start(w)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stdout, "", r)
        # The first line is the error, worded as every other unsupported setting is.
        lines = r.stderr.splitlines()
        self.assertTrue(lines[0].startswith("error: "), r)
        self.assertIn(f"{NOT_SUPPORTED}run.worktree = false", lines[0], r)
        # No single command fixes a setting: nothing follows the error.
        self.assertEqual(len(lines), 1, r)
        self.assert_nothing_created(w, before)

    def test_start_repo_subdirectory_resolves_to_top(self):
        w = FakeSupervisor(self, [card(1)])
        commit(w.repo, {"src/deep/a.txt": "a\n"}, "src")
        w.config_file.write_text(f'docs_dir = "{w.tmp / "docs dir" / "{repo}"}"\n', encoding="utf-8")
        top = w.repo.resolve()
        r = self.start(w, repo=w.repo / "src" / "deep").ok()
        run_dir = run_line(r)
        self.assertIn(f"run: {run_dir}", r.stdout.splitlines(), r)
        # {repo} is the top's folder name, and the run belongs to the top.
        self.assertEqual(run_dir.parent, w.docs / top.name / "runs")
        meta = read_json(run_dir / "run.json")
        self.assertEqual(meta["repo"], str(top))
        # The run's worktree sits at the top's .worktrees/, never under the subdirectory.
        ws = Path(meta["workspace"])
        self.assertEqual(ws, top / ".worktrees" / run_dir.name)
        self.assertEqual(run_git(ws, "symbolic-ref", "--short", "HEAD"), meta["branch"])
        self.assertFalse((w.repo / "src" / ".worktrees").exists())
        self.assertFalse((w.repo / "src" / "deep" / ".worktrees").exists())
        # The same deck from the top is the same run.
        again = self.start(w).ok()
        self.assertIn(f"existing run: {run_dir}", again.stdout.splitlines(), again)

    def test_start_repo_linked_worktree_resolves_to_main_working_tree(self):
        w = FakeSupervisor(self, [card(1)])
        linked = w.tmp / "elsewhere" / "linked wt"
        run_git(w.repo, "worktree", "add", "--quiet", "-b", "side", str(linked))
        # Config comes from the main working tree, never from a worktree's copy: this one would
        # be refused.
        (linked / ".leadhand.toml").write_text('[after_run]\nstep = "pr"\n', encoding="utf-8")
        w.config_file.write_text(f'docs_dir = "{w.tmp / "docs dir" / "{repo}"}"\n', encoding="utf-8")
        top = w.repo.resolve()
        r = self.start(w, repo=linked).ok()
        self.assertNotIn(NOT_SUPPORTED, r.stderr, r)
        run_dir = run_line(r)
        self.assertEqual(run_dir.parent, w.docs / top.name / "runs")
        meta = read_json(run_dir / "run.json")
        self.assertEqual(meta["repo"], str(top))
        self.assertEqual(Path(meta["workspace"]), top / ".worktrees" / run_dir.name)
        self.assertFalse((linked / ".worktrees").exists())
        self.assertFalse((w.tmp / "elsewhere" / ".worktrees").exists())
        # The excludes go to the common directory's info/exclude.
        exclude = (w.repo / ".git" / "info" / "exclude").read_text(encoding="utf-8").splitlines()
        self.assertEqual((exclude.count(".worktrees/"), exclude.count(".leadhand/")), (1, 1))
        # From a subdirectory of the linked worktree too: the same run.
        (linked / "sub dir").mkdir()
        again = self.start(w, repo=linked / "sub dir").ok()
        self.assertIn(f"existing run: {run_dir}", again.stdout.splitlines(), again)

    def test_worktree_records_run_dir_separately_from_leadhand_run(self):
        w = FakeSupervisor(self, [card(1)])
        first = w.start()
        second = w.start(new=True)
        self.assertNotEqual(first, second)
        for run_dir in (first, second):
            ws = Path(read_json(run_dir / "run.json")["workspace"])
            admin = admin_dir(ws)
            self.assertEqual(admin.parent, (w.repo / ".git" / "worktrees").resolve())
            # The run directory, in its own file beside Git's: the path's bytes and a newline.
            self.assertEqual((admin / RUN_RECORD).read_bytes(), os.fsencode(run_dir) + b"\n")
            # The ownership marker (the evidence base worktree's, holding a run ID) is another
            # file, and the run worktree has none.
            self.assertFalse((admin / BASE_MARKER).exists())
            self.assertTrue((admin / "gitdir").is_file())
        # Nothing in the operator's own .git folder or the worktree itself.
        self.assertFalse((w.repo / ".git" / RUN_RECORD).exists())
        self.assertFalse((Path(read_json(first / "run.json")["workspace"]) / RUN_RECORD).exists())

    def test_status_inside_worktree_uses_record_after_docs_dir_changes(self):
        w = FakeSupervisor(self, [card(1, title="Recorded run")])
        run_dir = w.start()
        ws = w.workspace()
        named = run_cli("status", run_dir, env=w.env, cwd=w.tmp).ok()
        # docs_dir moves after the start: a scan of the runs under it finds nothing, so only the
        # record leads from the worktree to the run.
        w.config_file.write_text(f'docs_dir = "{w.tmp / "other docs"}"\n', encoding="utf-8")
        deep = ws / "deep dir"
        deep.mkdir()
        for cwd in (ws, deep):
            with self.subTest(cwd=cwd):
                r = run_cli("status", env=w.env, cwd=cwd).ok()
                self.assertEqual(r.stdout, named.stdout, r)
                self.assertIn("Card 1: Recorded run · ", r.stdout, r)
        # Without the record, the worktree's run is not found.
        (admin_dir(ws) / RUN_RECORD).unlink()
        r = run_cli("status", env=w.env, cwd=ws)
        self.assertEqual(r.code, 1, r)

    def test_start_json(self):
        w = FakeSupervisor(self, [card(1)])
        r = self.start(w, "--json").ok()
        self.assertEqual(r.stdout.count("\n"), 1, r)
        report = r.json()
        self.assertEqual(set(report), {"run", "existing", "note"}, r)
        run_dir = Path(report["run"])
        self.assertEqual((report["existing"], report["note"]), (False, None), r)
        self.assertTrue((run_dir / "run.json").is_file())
        # The same information as the text output: the existing run, with its note.
        text = self.start(w).ok()
        r = self.start(w, "--json").ok()
        self.assertEqual(r.json(), {"run": str(run_dir), "existing": True,
                                    "note": f"existing run: {run_dir}"}, r)
        self.assertEqual(text.stdout, f"existing run: {run_dir}\n", text)
        # A new run on a suffixed branch: its note too.
        r = self.start(w, "--new", "--json").ok()
        fresh = r.json()
        self.assertEqual((fresh["existing"], fresh["note"]),
                         (False, "run branch: leadhand/demo-2 (leadhand/demo belongs to another run)"), r)
        self.assertEqual(read_json(Path(fresh["run"]) / "run.json")["branch"], "leadhand/demo-2")

    def test_start_sparse_refusal_ends_with_fix_command(self):
        w = FakeSupervisor(self, [card(1)])
        before = branches(w.repo)
        run_git(w.repo, "sparse-checkout", "set", "--no-cone", "/README.md")
        r = self.start(w)
        self.assertEqual(r.code, 1, r)
        lines = r.stderr.splitlines()
        self.assertTrue(lines[0].startswith("error: "), r)
        self.assertIn(f"{NOT_SUPPORTED}{w.repo.resolve()} is a sparse checkout", lines[0], r)
        # The last line is the one command that turns it off, alone, and it works as printed.
        self.assertEqual(lines[1:], [f"git -C {shlex.quote(str(w.repo.resolve()))} sparse-checkout disable"], r)
        done = subprocess.run(shlex.split(lines[-1]), env=w.env, cwd=w.tmp, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done)
        self.start(w).ok()
        self.assertNotEqual(branches(w.repo), before)


if __name__ == "__main__":
    unittest.main()
