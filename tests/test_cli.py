import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / "scripts" / "leadhand")


class CliTests(unittest.TestCase):
    def run_cli(self, *args):
        return subprocess.run(
            [sys.executable, "-B", CLI, *args], capture_output=True, text=True, cwd=ROOT, timeout=60
        )

    def test_version_prints_package_version(self):
        r = self.run_cli("version")
        self.assertEqual(r.stdout, "leadhand 0.1.0\n")
        self.assertEqual(r.returncode, 0)

    def test_unknown_command_exits_2(self):
        r = self.run_cli("nonsense")
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
