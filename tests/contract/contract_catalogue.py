"""The catalogue's completeness: every Python unit test has exactly one row in CATALOG.md,
and every contract test a row names exists. Static: this parses files with `ast` and never
runs the binary or imports a contract module."""
import ast
import re
import unittest
from pathlib import Path

from harness import ROOT

HERE = Path(__file__).resolve().parent
CATALOG = HERE / "CATALOG.md"
UNIT_TESTS = ROOT / "tests"

ROW = re.compile(r"^\| (?P<key>[^|]+?) \| (?P<disposition>[^|]+?) \|$")
KEY = re.compile(r"^test_\w+\.py::\w+::test_\w+$")
CONTRACT_ID = re.compile(r"^(?P<module>contract_\w+)\.(?P<cls>\w+)\.(?P<method>test_\w+)$")
INTERNAL = re.compile(r"^internal: Rust unit test \([a-z][a-z-]*\)$")
FIXED = ("dropped with in-place mode", "plugin file check (contract_plugin_files)")


def python_tests() -> list[str]:
    """`file::Class::name` for every `def test_` in a top-level class of tests/test_*.py."""
    keys = []
    for path in sorted(UNIT_TESTS.glob("test_*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        for node in tree.body:
            if isinstance(node, ast.ClassDef):
                keys += [f"{path.name}::{node.name}::{item.name}" for item in node.body
                         if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef))
                         and item.name.startswith("test_")]
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
                keys.append(f"{path.name}::<module>::{node.name}")
    return keys


def catalogue_rows() -> list[tuple[str, str]]:
    """(Python test, disposition) for every table row below the header."""
    rows = []
    for line in CATALOG.read_text(encoding="utf-8").splitlines():
        m = ROW.match(line)
        if m and m["key"] not in ("Python test", "---"):
            rows.append((m["key"], m["disposition"]))
    return rows


def contract_methods(module: str) -> dict[str, set[str]] | None:
    """{class: {test methods}} defined in tests/contract/<module>.py, or None if no such file."""
    path = HERE / f"{module}.py"
    if not path.is_file():
        return None
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    return {node.name: {item.name for item in node.body
                        if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef))
                        and item.name.startswith("test_")}
            for node in tree.body if isinstance(node, ast.ClassDef)}


class Catalogue(unittest.TestCase):
    maxDiff = None

    def test_every_python_test_is_catalogued(self):
        tests = python_tests()
        self.assertGreater(len(tests), 600)
        rows = catalogue_rows()
        counts: dict[str, int] = {}
        for key, _ in rows:
            counts[key] = counts.get(key, 0) + 1
        known = set(tests)
        missing = [k for k in tests if k not in counts]
        duplicated = sorted(k for k, n in counts.items() if n > 1)
        stale = sorted(k for k in counts if k not in known)
        malformed = sorted(k for k in counts if not KEY.match(k))
        self.assertEqual(missing, [], f"{len(missing)} Python tests have no catalogue row")
        self.assertEqual(duplicated, [], "Python tests with more than one row")
        self.assertEqual(stale, [], "rows naming no Python test")
        self.assertEqual(malformed, [], "rows whose key is not file::Class::test")

    def test_every_disposition_is_valid_and_every_contract_id_exists(self):
        modules: dict[str, dict[str, set[str]] | None] = {}
        bad = []
        for key, disposition in catalogue_rows():
            if disposition in FIXED or INTERNAL.match(disposition):
                continue
            for part in disposition.split("; "):
                m = CONTRACT_ID.match(part)
                if not m:
                    bad.append(f"{key}: not a disposition: {part!r}")
                    continue
                if m["module"] not in modules:
                    modules[m["module"]] = contract_methods(m["module"])
                classes = modules[m["module"]]
                if classes is None:
                    bad.append(f"{key}: no file tests/contract/{m['module']}.py")
                elif m["method"] not in classes.get(m["cls"], set()):
                    bad.append(f"{key}: no test {part}")
        self.assertEqual(bad, [])


class CheckerIsStrict(unittest.TestCase):
    """The parsers above see what they must: a real row, a real contract test, a missing one."""

    def test_parsers_find_known_entries(self):
        self.assertIn("test_cli.py::CliTests::test_unknown_command_exits_2", python_tests())
        self.assertIn(("test_cli.py::CliTests::test_unknown_command_exits_2",
                       "contract_cli.Usage.test_unknown_command_exits_2"), catalogue_rows())
        self.assertIn("test_unknown_command_exits_2", contract_methods("contract_cli")["Usage"])
        self.assertIsNone(contract_methods("contract_no_such_file"))
        for good in ("internal: Rust unit test (state)", "internal: Rust unit test (testing-tools)"):
            self.assertRegex(good, INTERNAL)
        for wrong in ("internal: Rust unit test", "internal: state", "contract_cli.Usage"):
            self.assertFalse(INTERNAL.match(wrong) or CONTRACT_ID.match(wrong), wrong)


if __name__ == "__main__":
    unittest.main()
