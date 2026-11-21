"""`check` in Rust: in-place runs refused and the repository taken from the main working tree
(deliberate change 1), and the conventions for the commands a person types (`--json`, the fix
command alone on the last line). Rust-only.

`check` keeps its contract output: one problem per line on stdout. The fix command, when one
command fixes every error, follows on stderr, alone.
"""
import shlex
import subprocess
import unittest

from gitfixture import commit, run_git
from harness import rust_only
from supervisor import FakeSupervisor, card

NOT_SUPPORTED = "not supported in this version: "


def problems(result) -> list[tuple[str, str]]:
    """`check`'s problem lines as (level, message)."""
    out = []
    for line in result.stdout.splitlines():
        level, sep, message = line.partition(": ")
        if sep and level in ("error", "warning"):
            out.append((level, message))
    return out


def refusals(result) -> list[str]:
    return [line for line in result.stdout.splitlines() if NOT_SUPPORTED in line]


@rust_only(uses=("check",))
class Check(unittest.TestCase):
    def check(self, w, *flags, repo=None):
        return w.cli("check", w.deck_path, "--repo", repo or w.repo, *flags)

    def test_check_json_lists_problems(self):
        w = FakeSupervisor(self, [card(1), card(2, depends=(9,))], config='[notify]\ncommand = "say done"\n')
        text = self.check(w)
        self.assertEqual(text.code, 1, text)
        r = self.check(w, "--json")
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stderr, "", r)
        report = r.json()
        self.assertEqual(set(report), {"ok", "cards", "problems", "fix"}, r)
        self.assertEqual((report["ok"], report["cards"], report["fix"]), (False, 2, None), r)
        # The same problems as the text output, in the same order, each with its card.
        self.assertEqual([(p["level"], p["message"]) for p in report["problems"]], problems(text), r)
        self.assertIn({"level": "error", "card": 2, "message": "card 2: depends on unknown card 9"},
                      report["problems"], r)
        self.assertIn({"level": "warning", "card": None,
                       "message": "not supported in this version: notify.command = say done"},
                      report["problems"], r)
        for p in report["problems"]:
            self.assertEqual(set(p), {"level", "card", "message"}, p)
            if p["message"].startswith("card "):
                self.assertEqual(p["message"].split(":")[0], f"card {p['card']}", p)

        ok = FakeSupervisor(self, [card(1, allowed="README.md")])
        r = self.check(ok, "--json")
        self.assertEqual(r.code, 0, r)
        self.assertEqual(r.json(), {"ok": True, "cards": 1, "problems": [], "fix": None}, r)
        self.assertEqual(self.check(ok).stdout, "ok: 1 cards\n")

    def test_check_json_reports_a_deck_that_does_not_load(self):
        w = FakeSupervisor(self, [card(1)])
        w.deck_path.write_text(w.deck_path.read_text(encoding="utf-8").replace("Risk: low", "Risk: severe"),
                               encoding="utf-8")
        text = self.check(w)
        self.assertEqual(text.code, 1, text)
        (line,) = text.stdout.splitlines()
        r = self.check(w, "--json")
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.json(), {"ok": False, "cards": None, "fix": None,
                                    "problems": [{"level": "error", "card": None,
                                                  "message": line[len("error: "):]}]}, r)

    def test_check_refuses_run_worktree_false(self):
        w = FakeSupervisor(self, [card(1)], config="[run]\nworktree = false\n")
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(refusals(r), ["error: not supported in this version: run.worktree = false"], r)
        self.assertNotIn("ok: ", r.stdout, r)
        # No single command fixes a setting: nothing follows the error.
        self.assertEqual(r.stderr, "", r)

    def test_check_sparse_refusal_ends_with_fix_command(self):
        w = FakeSupervisor(self, [card(1)])
        run_git(w.repo, "sparse-checkout", "set", "--no-cone", "/README.md")
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertEqual([level for level, _ in problems(r) if level == "error"], ["error"], r)
        (refusal,) = refusals(r)
        self.assertTrue(refusal.startswith(f"error: {NOT_SUPPORTED}{w.repo.resolve()} is a sparse checkout"), r)
        # The last line is the one command that turns the sparse checkout off, alone: it names
        # the repository, quoted (its name has a space), so it works from any directory.
        lines = r.stderr.splitlines()
        self.assertEqual(lines, [f"git -C {shlex.quote(str(w.repo.resolve()))} sparse-checkout disable"], r)
        done = subprocess.run(shlex.split(lines[-1]), env=w.env, cwd=w.tmp, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done)
        r = self.check(w)
        self.assertEqual(r.code, 0, r)
        self.assertEqual((refusals(r), r.stderr), ([], ""), r)

    def test_check_cycle_error_has_no_fix_line(self):
        w = FakeSupervisor(self, [card(1, depends=(2,), allowed="README.md"),
                                  card(2, depends=(1,), allowed="README.md")])
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        # No single command fixes a cycle: the error is the last line, and nothing follows it.
        self.assertEqual(r.stdout.splitlines(), ["error: dependency cycle among cards 1, 2"], r)
        self.assertEqual(r.stderr, "", r)
        self.assertIsNone(self.check(w, "--json").json()["fix"])

    def test_check_resolves_repo_from_linked_worktree(self):
        w = FakeSupervisor(self, [card(1, allowed="src/")])
        commit(w.repo, {"src/a.txt": "a\n"}, "src")
        linked = w.tmp / "elsewhere" / "linked wt"
        run_git(w.repo, "worktree", "add", "--quiet", "-b", "side", str(linked))
        # Config comes from the main working tree, never from a worktree's copy; {repo} in
        # docs_dir is the main working tree's folder name, and a relative docs_dir is resolved
        # against it.
        (w.repo / ".leadhand.toml").write_text("[run]\ncost_limit_usd = 5\n", encoding="utf-8")
        (linked / ".leadhand.toml").write_text('[after_run]\nstep = "pr"\n', encoding="utf-8")
        w.config_file.write_text('docs_dir = "notes/{repo}"\n', encoding="utf-8")
        docs = (w.repo / "notes" / "my repo").resolve()
        expected = [f"error: {NOT_SUPPORTED}docs_dir inside the repository ({docs})",
                    f"error: {NOT_SUPPORTED}run.cost_limit_usd = 5"]
        for repo in (w.repo, w.repo / "src", linked, linked / "src"):
            with self.subTest(repo=repo):
                r = self.check(w, repo=repo)
                self.assertEqual(r.code, 1, r)
                self.assertEqual(refusals(r), expected, r)
                self.assertEqual([m for level, m in problems(r) if level == "error" and NOT_SUPPORTED not in m],
                                 [], r)


if __name__ == "__main__":
    unittest.main()
