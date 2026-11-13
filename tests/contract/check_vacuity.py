"""Check that executable contract tests really test the binary.

    python3 -B tests/contract/check_vacuity.py tests/contract/contract_<x>.py...

Runs the given contract files twice, with LEADHAND_BIN=/usr/bin/true and with
LEADHAND_BIN=/usr/bin/false, each time from the repository root with PYTHONPATH=tests.
Prints the ID of every test that passes both times and exits 1 if there is any, else
exits 0. A skipped test is not a pass. A file that fails to import, or a run that
errors outside any test, fails the check loudly. The static files check plugin files
and the harness, never the binary, so they are refused (exit 2).
"""
import importlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
STATIC = ("contract_self.py", "contract_plugin_files.py", "contract_catalogue.py")
BINARIES = ("/usr/bin/true", "/usr/bin/false")


class _Passes(unittest.TestResult):
    def __init__(self):
        super().__init__()
        self.passed: list[str] = []

    def addSuccess(self, test):
        super().addSuccess(test)
        self.passed.append(test.id())

    def addUnexpectedSuccess(self, test):
        super().addUnexpectedSuccess(test)
        self.passed.append(test.id())


def _collect(out: str, files: list[str]) -> int:
    """Child mode: run `files` and write {"passed": [...], "ran": n} to `out`."""
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for f in files:
        module = importlib.import_module(Path(f).stem)    # an import error fails the child
        if Path(module.__file__).resolve() != Path(f):
            print(f"error: {f} is not a file in tests/contract", file=sys.stderr)
            return 1
        tests = loader.loadTestsFromModule(module)
        if tests.countTestCases() == 0:
            print(f"error: {f} has no tests", file=sys.stderr)
            return 1
        suite.addTests(tests)
    if loader.errors:
        print("\n".join(loader.errors), file=sys.stderr)
        return 1
    result = _Passes()
    suite.run(result)
    # Errors outside any test (setUpModule, setUpClass) mean tests never ran.
    outside = [(t, tb) for t, tb in result.errors if not isinstance(t, unittest.TestCase)]
    if outside:
        for t, tb in outside:
            print(f"error outside any test: {t}\n{tb}", file=sys.stderr)
        return 1
    Path(out).write_text(json.dumps({"passed": result.passed, "ran": result.testsRun}), encoding="utf-8")
    return 0


def _run(binary: str, files: list[str], out: Path) -> dict:
    env = dict(os.environ, LEADHAND_BIN=binary)
    env["PYTHONPATH"] = os.pathsep.join(p for p in ("tests", os.environ.get("PYTHONPATH")) if p)
    r = subprocess.run([sys.executable, "-B", str(Path(__file__).resolve()), "--collect", str(out), *files],
                       cwd=ROOT, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True)
    if r.returncode != 0 or not out.exists():
        sys.exit(f"check_vacuity: the run with LEADHAND_BIN={binary} failed (exit {r.returncode})\n"
                 f"{r.stdout}{r.stderr}")
    return json.loads(out.read_text(encoding="utf-8"))


def main(argv: list[str]) -> int:
    if argv[:1] == ["--collect"]:
        return _collect(argv[1], argv[2:])
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    static = [f for f in argv if Path(f).name in STATIC]
    if static:
        for f in static:
            print(f"static checks never run the binary: {f}", file=sys.stderr)
        return 2
    files = [str(Path(f).resolve()) for f in argv]
    with tempfile.TemporaryDirectory(prefix="lh vacuity ") as tmp:
        runs = [_run(b, files, Path(tmp) / f"{Path(b).name}.json") for b in BINARIES]
    both = sorted(set(runs[0]["passed"]) & set(runs[1]["passed"]))
    if both:
        print(f"{len(both)} test(s) pass against both {' and '.join(BINARIES)}:")
        for test_id in both:
            print(f"  {test_id}")
        return 1
    print(f"ok: none of {runs[0]['ran']} test(s) passes against both {' and '.join(BINARIES)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
