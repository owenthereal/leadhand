"""R38: the supervisor records a worker's reply through a file, never a heredoc."""
import json
import unittest
from pathlib import Path

from fakeworld import FakeWorld, card
from gitfixture import run_git
from leadhand.state import append_event
from leadhand.briefs import scratch_dir


class ReplyFile(unittest.TestCase):
    def test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        path = Path(a["reply_file"])
        self.assertTrue(path.is_absolute())
        ws = w.workspace()
        self.assertEqual(path, scratch_dir(ws, 1, a["dispatch_id"]) / "reply.txt")
        self.assertIn(ws.resolve(), path.resolve().parents)
        path.write_text("Status: READY\n", encoding="utf-8")
        self.assertEqual(run_git(ws, "check-ignore", str(path)), str(path))     # raises unless ignored
        self.assertNotIn("reply.txt", run_git(ws, "status", "--porcelain", "--untracked-files=all"))

    def test_wait_names_the_same_reply_file(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        w.launch(a)
        wait = w.next()
        self.assertEqual(wait["action"], "wait")
        self.assertEqual(wait["reply_file"], a["reply_file"])

    def test_cli_records_reply_from_a_file_with_spaces_in_its_path(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        self.assertIn(" ", a["reply_file"])
        worker = w.launch(a)
        reply = w.perform(a, worker)
        Path(a["reply_file"]).write_text(reply, encoding="utf-8")
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply-file", a["reply_file"])
        self.assertEqual(r.returncode, 0, r.stderr)
        event = json.loads(r.stdout)
        self.assertEqual(event["status"], "READY")
        self.assertEqual(Path(event["files"]["reply"]).read_text(encoding="utf-8"), reply)

    def test_reply_and_reply_file_are_exclusive(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        Path(a["reply_file"]).write_text(w.perform(a, worker), encoding="utf-8")
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply", "Status: READY", "--reply-file", a["reply_file"])
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(w.events("result_recorded"), [])

    def test_missing_reply_file_falls_back_to_the_report_head_with_a_note(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        w.perform(a, worker)                          # writes the report; the reply file is never written
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply-file", a["reply_file"])
        self.assertEqual(r.returncode, 0, r.stderr)
        event = json.loads(r.stdout)
        self.assertEqual(event["status"], "READY")
        note = Path(event["files"]["reply"]).read_text(encoding="utf-8")
        self.assertIn("reply file", note)
        self.assertIn(a["reply_file"], note)

    def test_new_request_removes_a_stale_reply_file(self):
        # Without worktrees, a later run reuses the workspace and the same dispatch IDs.
        w = FakeWorld(self, [card(1)], config="[run]\nworktree = false\n")
        w.start()
        first = w.next()
        Path(first["reply_file"]).write_text("old reply\n", encoding="utf-8")
        w.run()
        append_event(w.paths, "outcome", outcome="complete", qualifier="", reasons=[])
        w.write_deck([card(1)], branch="leadhand/demo-2")
        w.start()
        a = w.next()
        self.assertEqual(a["reply_file"], first["reply_file"])
        self.assertFalse(Path(a["reply_file"]).exists())


if __name__ == "__main__":
    unittest.main()
