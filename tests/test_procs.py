import os
import subprocess
import sys
import time
import unittest
from unittest import mock

from leadhand import procs


class ProcsTests(unittest.TestCase):
    def spawn(self, *, new_session: bool = False) -> subprocess.Popen:
        p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                             start_new_session=new_session, stdin=subprocess.DEVNULL)
        self.addCleanup(lambda: (p.kill(), p.wait()))
        return p

    def test_own_process_info(self):
        info = procs.process_info(os.getpid())
        self.assertIsNotNone(info)
        self.assertEqual(info.pid, os.getpid())
        self.assertEqual(info.ppid, os.getppid())
        self.assertEqual(info.pgid, os.getpgid(0))
        self.assertFalse(info.zombie)

    def test_start_time_is_stable_and_differs_between_processes(self):
        a, b = self.spawn(), self.spawn()
        sa = procs.process_start_time(a.pid)
        self.assertTrue(sa)
        self.assertEqual(sa, procs.process_start_time(a.pid))
        self.assertNotEqual(procs.process_start_time(os.getpid()), sa)
        self.assertTrue(procs.process_start_time(b.pid))

    def test_child_in_new_session_has_own_group_and_is_a_descendant(self):
        child = self.spawn(new_session=True)
        info = procs.process_info(child.pid)
        self.assertEqual((info.ppid, info.pgid), (os.getpid(), child.pid))
        self.assertIn(child.pid, [p.pid for p in procs.descendants(os.getpid())])
        self.assertIn(child.pid, [p.pid for p in procs.list_processes()])

    def test_same_process_checks_start_time_and_zombies(self):
        child = self.spawn()
        start = procs.process_start_time(child.pid)
        self.assertTrue(procs.same_process(child.pid, start))
        self.assertFalse(procs.same_process(child.pid, start + "x"))
        self.assertFalse(procs.same_process(child.pid, None))
        child.kill()
        deadline = time.monotonic() + 5
        while procs.same_process(child.pid, start) and time.monotonic() < deadline:
            time.sleep(0.02)
        # Killed but not yet reaped: a zombie counts as gone.
        self.assertFalse(procs.same_process(child.pid, start))
        child.wait()
        self.assertIsNone(procs.process_start_time(child.pid))
        self.assertNotIn(child.pid, [p.pid for p in procs.descendants(os.getpid())])

    def test_ps_fallback_parses_lstart_with_spaces(self):
        table = ("  101     1   101 Ss   Tue Nov 10 07:00:01 2026\n"
                 "  102   101   102 Z    Tue Nov 10 07:00:02 2026\n")
        with mock.patch.object(procs, "_ps", return_value=table):
            got = procs._ps_all()
        self.assertEqual(got, [procs.ProcInfo(101, 1, 101, "lstart:Tue Nov 10 07:00:01 2026"),
                               procs.ProcInfo(102, 101, 102, "lstart:Tue Nov 10 07:00:02 2026", zombie=True)])
        with mock.patch.object(procs, "_ps", return_value=table.splitlines()[0]):
            self.assertEqual(procs._ps_one(101).start, "lstart:Tue Nov 10 07:00:01 2026")
        with mock.patch.object(procs, "_ps", return_value=None):
            self.assertIsNone(procs._ps_one(101))

    def test_missing_pid(self):
        self.assertIsNone(procs.process_info(0))
        self.assertIsNone(procs.process_info(2 ** 22 + 12345))


if __name__ == "__main__":
    unittest.main()
