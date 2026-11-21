"""Check that every Python contract test has a Rust twin that asserts at least as much, or a
recorded reason it has none.

    python3 -B tests/contract/check_conversion.py [--pending FILE...]

Runs from any directory. It lists the Rust tests with
`cargo test --locked --features testing --tests -- --list` in the repository root, so set
CARGO_HOME as for any cargo command.

What it compares:

1. The Python tests: what `unittest discover -s tests/contract -p 'contract_*.py'` loads,
   less those tagged @reference_only, which leave with the Python and are never converted.
   The Rust tests: what that `cargo test --list` lists for each integration test file,
   `tests/<file>.rs` (or `tests/<file>/main.rs`), named `<file>::<path in the file>`.
2. The default twin name (ruling S3-R6): `contract_<file>.<Class>.<test_name>` maps to
   `<file>::<snake_case(Class)>::<test_name>` in `tests/<file>.rs`, the method name
   verbatim (its `test_` prefix kept). snake_case puts `_` before each capital that follows
   a lower-case letter or digit, and before the last capital of a run that a lower-case
   letter follows (`HTTPServer` -> `http_server`), then lower-cases; a result that is a Rust
   keyword gets a trailing `_` (`Loop` -> `loop_`), as no module can be named `loop`.
   `tests/contract/conversion-map.txt` overrides the default; its header gives its syntax.
3. Assertion counts (rulings S3-R6, S3-R21), per test, in the test function's own body:
   everything lexically inside the `def` or `fn`, nested functions and closures included,
   each written assertion counted once however often it runs.
   - Python: every call `<expr>.assert*(...)`, whatever its receiver (`self.assertEqual`,
     a helper object's `s.assert_refused`), and `assert` statements.
   - Rust: invocations of macros whose name starts with `assert` (`assert!`, `assert_eq!`,
     `assert_ne!`, and harness macros such as `assert_panics!` and `assert_json_eq!`): the
     name, `!`, then `(`, `[` or `{` (so `asserts != 0` is no assertion). A Rust assertion
     helper therefore counts only when it is a macro; a helper function's assertions do not
     count, as a Python helper method's (`self.rewritten(...)`) do not.
   A twin with fewer assertions than its Python test is an error. A Python test that counts
   0 (its assertions all in helper methods) is listed in the output, for the reviewer to
   compare by hand, and its twin must count at least 1.
4. What a twin may not be (ruling S3-R21): `#[ignore]` or `#[should_panic]`, either of which
   lets it pass without checking anything; nor may it call `rust_only()` (and so skip under
   LEADHAND_IMPL=python) unless its Python test is itself Rust-only (@rust_only on the
   method or its class).

Errors (exit 1): a Python test with neither a twin nor a `retired` entry (allowed while its
file is --pending); a twin with fewer assertions, with none for a Python test that counts 0,
ignored, should-panic, or skipping a parity test (rust_only); a Rust test claimed by two
Python tests; a
map line that does not parse, names an unknown or reference-only Python test, repeats one,
retires one without a reason, or names a Rust test that does not exist (allowed while the
Python test's file is --pending). A Rust twin whose source the parser cannot find is an
error too: twins live in inline modules of `tests/<file>.rs`.

A twin that exists is checked even while its file is --pending.

It prints the counts per Python file, then every `retired` entry with its reason, every note
and every Python test that counts 0, for the reviewer. --pending takes file names (`contract_cards.py`, a path to it, or
`contract_cards`), once each or several after one flag. Exit 2 on bad usage.
"""
from __future__ import annotations

import argparse
import ast
import inspect
import os
import re
import subprocess
import sys
import unittest
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
MAP = HERE / "conversion-map.txt"

PY_ID = re.compile(r"^(?P<file>contract_\w+)\.(?P<cls>\w+)\.(?P<method>test_\w+)$")
RUST_ID = re.compile(r"^[a-z_][a-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)+$")
ENTRY = re.compile(r"^(?P<py>\S+)\s+->\s+(?P<target>.*?)(?:\s+#\s+(?P<note>.*))?$")
RUNNING = re.compile(r"^\s*Running (?P<unit>unittests )?(?P<path>\S+) \(")
LISTED = re.compile(r"^(?P<name>\S+): test$")
RUST_KEYWORDS = frozenset(
    "as async await break const continue crate dyn else enum extern false fn for gen if impl in let loop "
    "match mod move mut pub ref return self Self static struct super trait true type unsafe use where while "
    "abstract become box do final macro override priv try typeof unsized virtual yield".split())


def snake(name: str) -> str:
    out = re.sub(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", name).lower()
    return out + "_" if out in RUST_KEYWORDS else out


def default_twin(py_id: str) -> str:
    m = PY_ID.match(py_id)
    file = m["file"][len("contract_"):]
    file = file + "_" if file in RUST_KEYWORDS else file
    return f"{file}::{snake(m['cls'])}::{m['method']}"


# --------------------------------------------------------------------------
# The Python side

@dataclass
class PyTest:
    file: str          # contract_<x>
    asserts: int
    rust_only: bool    # tagged @rust_only (on the method or its class)


def _iter_tests(suite):
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            yield from _iter_tests(t)
        else:
            yield t


def _skip_reason(cls, func) -> str:
    why = getattr(func, "__unittest_skip_why__", "") if getattr(func, "__unittest_skip__", False) else ""
    if not why and getattr(cls, "__unittest_skip__", False):
        why = getattr(cls, "__unittest_skip_why__", "")
    return why or ""


_module_trees: dict[str, ast.Module] = {}


def _python_asserts(func) -> int:
    """The assertions in `func`'s own `def` (its decorators' line is where its code starts)."""
    func = inspect.unwrap(func)
    path = inspect.getsourcefile(func)
    if path not in _module_trees:
        _module_trees[path] = ast.parse(Path(path).read_text(encoding="utf-8"), filename=path)
    first = func.__code__.co_firstlineno
    for node in ast.walk(_module_trees[path]):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == func.__name__:
            start = node.decorator_list[0].lineno if node.decorator_list else node.lineno
            if first in (start, node.lineno):
                return sum(1 for n in ast.walk(node) if _is_assertion(n))
    raise SystemExit(f"check_conversion: no source found for {func.__qualname__} in {path}")


def _is_assertion(node: ast.AST) -> bool:
    """An `assert` statement, or a call `<expr>.assert*(...)` whatever the receiver."""
    if isinstance(node, ast.Assert):
        return True
    return (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
            and node.func.attr.startswith("assert"))


def python_tests() -> tuple[dict[str, PyTest], set[str]]:
    """({test id: PyTest} for the convertible tests, {ids of the reference-only tests})."""
    # Under LEADHAND_IMPL=rust with no Rust commands, both tags skip, each with its own reason.
    os.environ["LEADHAND_IMPL"] = "rust"
    os.environ.pop("LEADHAND_RUST_COMMANDS", None)
    sys.path[:0] = [str(HERE), str(ROOT / "tests")]
    loader = unittest.TestLoader()
    suite = loader.discover(str(HERE), pattern="contract_*.py", top_level_dir=str(HERE))
    if loader.errors:
        raise SystemExit("check_conversion: the contract files failed to load:\n" + "\n".join(loader.errors))
    tests: dict[str, PyTest] = {}
    reference: set[str] = set()
    for test in _iter_tests(suite):
        tid = test.id()
        if not PY_ID.match(tid):
            raise SystemExit(f"check_conversion: unexpected test id {tid}")
        cls = type(test)
        func = getattr(cls, test._testMethodName)
        why = _skip_reason(cls, func)
        if why.startswith("reference-only"):
            reference.add(tid)
            continue
        tests[tid] = PyTest(tid.split(".")[0], _python_asserts(func), why.startswith("rust-only"))
    return tests, reference


# --------------------------------------------------------------------------
# The Rust side

def rust_listing() -> set[str]:
    """Every test in the integration test files, as `<file>::<path>`."""
    cmd = ["cargo", "test", "--locked", "--features", "testing", "--tests", "--", "--list"]
    r = subprocess.run(cmd, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT, text=True)
    if r.returncode != 0:
        raise SystemExit(f"check_conversion: {' '.join(cmd)} failed (exit {r.returncode})\n{r.stdout}")
    ids: set[str] = set()
    current: str | None = None
    for line in r.stdout.splitlines():
        m = RUNNING.match(line)
        if m:
            path = Path(m["path"])
            current = None
            if not m["unit"] and path.parts[:1] == ("tests",) and path.suffix == ".rs":
                if len(path.parts) == 2:
                    current = path.stem
                elif len(path.parts) == 3 and path.name == "main.rs":
                    current = path.parts[1]
            continue
        m = LISTED.match(line)
        if m and current:
            ids.add(f"{current}::{m['name']}")
    return ids


def _blank_rust(src: str) -> str:
    """Rust source with comments and string and character literals blanked (newlines kept)."""
    out: list[str] = []
    i, n = 0, len(src)

    def blank(text: str) -> None:
        out.append("".join("\n" if c == "\n" else " " for c in text))

    while i < n:
        c = src[i]
        prev_ident = i > 0 and (src[i - 1].isalnum() or src[i - 1] == "_")
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(src[i:j])
            i = j
        elif src.startswith("/*", i):
            depth, j = 0, i
            while j < n:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                    if depth == 0:
                        break
                else:
                    j += 1
            blank(src[i:j])
            i = j
        elif not prev_ident and (m := RAW_STRING.match(src, i)):
            close = '"' + m[1]
            j = src.find(close, m.end())
            j = n if j < 0 else j + len(close)
            blank(src[i:j])
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(src[i:j + 1])
            i = j + 1
        elif c == "'" and (src.startswith("'\\", i) or (i + 2 < n and src[i + 2] == "'")):
            j = i + 1
            if src[j] == "\\":
                j += 2
            while j < n and src[j] != "'":
                j += 1
            blank(src[i:j + 1])
            i = j + 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


RAW_STRING = re.compile(r'[bc]?r(#*)"')
TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|::|->|=>|\S")


@dataclass
class RustFn:
    asserts: int
    is_test: bool
    ignored: bool        # #[ignore]
    should_panic: bool   # #[should_panic]
    rust_only: bool      # calls rust_only()


def rust_functions(path: Path) -> dict[tuple[str, ...], RustFn]:
    """{(module, ..., fn): RustFn} for the functions in `path`'s inline modules; a `#[test]`
    function wins over another function of the same path."""
    toks = TOKEN.findall(_blank_rust(path.read_text(encoding="utf-8")))
    ident = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
    found: dict[tuple[str, ...], RustFn] = {}
    mods: list[tuple[str, int]] = []
    attrs: list[str] = []
    depth = i = 0

    def matching(j: int, open_: str, close: str) -> int:
        """The index just past the bracket that closes the one at j."""
        level = 0
        while j < len(toks):
            if toks[j] == open_:
                level += 1
            elif toks[j] == close:
                level -= 1
                if level == 0:
                    return j + 1
            j += 1
        return j

    while i < len(toks):
        t = toks[i]
        if t == "#" and i + 1 < len(toks) and toks[i + 1] in ("[", "!"):
            j = i + 1 if toks[i + 1] == "[" else i + 2
            end = matching(j, "[", "]")
            attrs.append(" ".join(toks[j + 1:end - 1]))
            i = end
            continue
        if t == "mod" and i + 2 < len(toks) and ident.match(toks[i + 1]) and toks[i + 2] == "{":
            mods.append((toks[i + 1], depth))
            depth += 1
            attrs = []
            i += 3
            continue
        if t == "fn" and i + 1 < len(toks) and ident.match(toks[i + 1]):
            name, j = toks[i + 1], i + 2
            if j < len(toks) and toks[j] == "<":
                j = matching(j, "<", ">")
            while j < len(toks) and toks[j] != "(":
                j += 1
            j = matching(j, "(", ")")
            while j < len(toks) and toks[j] not in ("{", ";"):
                j += 1
            if j >= len(toks) or toks[j] == ";":
                attrs = []
                i = j + 1
                continue
            end = matching(j, "{", "}")
            body = toks[j:end]
            count = sum(1 for k in range(len(body) - 2)
                        if body[k].startswith("assert") and ident.match(body[k]) and body[k + 1] == "!"
                        and body[k + 2] in ("(", "[", "{"))
            calls_rust_only = any(body[k] == "rust_only" and body[k + 1] == "("
                                  for k in range(len(body) - 1))
            heads = [a.split(" ")[0] for a in attrs]
            fn = RustFn(count, "test" in attrs, "ignore" in heads, "should_panic" in heads, calls_rust_only)
            key = tuple(m[0] for m in mods) + (name,)
            if key not in found or (fn.is_test and not found[key].is_test):
                found[key] = fn
            attrs = []
            i = end
            continue
        if t == "{":
            depth += 1
        elif t == "}":
            depth -= 1
            while mods and mods[-1][1] == depth:
                mods.pop()
        if t in (";", "{", "}"):
            attrs = []
        i += 1
    return found


_rust_sources: dict[str, dict[tuple[str, ...], RustFn] | None] = {}


def rust_fn(rust_id: str) -> RustFn | None:
    """What the Rust test's source says of it; None when its source is not found."""
    file, *path = rust_id.split("::")
    if file not in _rust_sources:
        for candidate in (ROOT / "tests" / f"{file}.rs", ROOT / "tests" / file / "main.rs"):
            if candidate.is_file():
                _rust_sources[file] = rust_functions(candidate)
                break
        else:
            _rust_sources[file] = None
    functions = _rust_sources[file]
    return None if functions is None else functions.get(tuple(path))


# --------------------------------------------------------------------------
# The map

@dataclass
class Entry:
    line: int
    py: str
    rust: str | None       # None: retired
    reason: str | None
    note: str | None


def read_map(errors: list[str]) -> dict[str, Entry]:
    entries: dict[str, Entry] = {}
    for n, raw in enumerate(MAP.read_text(encoding="utf-8").splitlines(), 1):
        text = raw.strip()
        if not text or text.startswith("#"):
            continue
        m = ENTRY.match(text)
        if not m or not PY_ID.match(m["py"]):
            errors.append(f"conversion-map.txt:{n}: not an entry: {raw!r}")
            continue
        target, note = m["target"].strip(), m["note"]
        if target.startswith("retired:"):
            reason = target[len("retired:"):].strip()
            if not reason:
                errors.append(f"conversion-map.txt:{n}: {m['py']} is retired without a reason")
            entry = Entry(n, m["py"], None, reason, note)
        elif RUST_ID.match(target):
            entry = Entry(n, m["py"], target, None, note)
        else:
            errors.append(f"conversion-map.txt:{n}: not a Rust test id or `retired: <reason>`: {target!r}")
            continue
        if m["py"] in entries:
            errors.append(f"conversion-map.txt:{n}: {m['py']} already has an entry (line {entries[m['py']].line})")
            continue
        entries[m["py"]] = entry
    return entries


# --------------------------------------------------------------------------

def pending_name(arg: str) -> str:
    name = Path(arg).name
    return name[:-3] if name.endswith(".py") else name


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Check the Python contract tests' Rust twins.")
    parser.add_argument("--pending", action="extend", nargs="+", default=[], metavar="FILE",
                        help="a contract file whose converting task is still to come")
    args = parser.parse_args(argv)
    pending = {pending_name(p) for p in args.pending}
    files = {p.stem for p in HERE.glob("contract_*.py")}
    for name in sorted(pending):
        if name not in files:
            parser.error(f"--pending {name}: no tests/contract/{name}.py")

    errors: list[str] = []
    py, reference = python_tests()
    rust = rust_listing()
    entries = read_map(errors)

    for pid, entry in sorted(entries.items(), key=lambda kv: kv[1].line):
        if pid in reference:
            errors.append(f"conversion-map.txt:{entry.line}: {pid} is reference-only: it is never converted")
        elif pid not in py:
            errors.append(f"conversion-map.txt:{entry.line}: no Python test {pid}")

    claimed: dict[str, list[str]] = {}
    per_file: dict[str, dict[str, int]] = {}
    missing_pending: list[str] = []
    for pid, test in sorted(py.items()):
        counts = per_file.setdefault(test.file, {"tests": 0, "twinned": 0, "retired": 0, "pending": 0})
        counts["tests"] += 1
        entry = entries.get(pid)
        if entry is not None and entry.rust is None:
            counts["retired"] += 1
            continue
        twin = entry.rust if entry is not None else default_twin(pid)
        claimed.setdefault(twin, []).append(pid)
        if twin not in rust:
            if test.file in pending:
                counts["pending"] += 1
                missing_pending.append(pid)
            elif entry is not None:
                errors.append(f"conversion-map.txt:{entry.line}: {pid}: no Rust test {twin}")
            else:
                errors.append(f"{pid}: no Rust twin {twin} (and no conversion-map.txt entry)")
            continue
        fn = rust_fn(twin)
        if fn is None:
            errors.append(f"{pid}: the source of its twin {twin} was not found in an inline module of "
                          f"tests/{twin.split('::')[0]}.rs")
            continue
        bad = []
        if fn.asserts < max(test.asserts, 1):
            if test.asserts:
                bad.append(f"has {fn.asserts} assertion(s), fewer than its {test.asserts}")
            else:
                bad.append("has no assertion; its Python test counts 0 (its checks are in helpers), so the "
                           "twin needs at least 1")
        if fn.ignored:
            bad.append("is #[ignore]")
        if fn.should_panic:
            bad.append("is #[should_panic]")
        if fn.rust_only and not test.rust_only:
            bad.append("calls rust_only(), so it skips under LEADHAND_IMPL=python, but its Python test is a "
                       "parity test, not @rust_only")
        if bad:
            errors.append(f"{pid}: its twin {twin} " + "; ".join(bad))
            continue
        counts["twinned"] += 1
    for twin, pids in sorted(claimed.items()):
        if len(pids) > 1:
            errors.append(f"{twin} is claimed by {len(pids)} Python tests: {', '.join(pids)}")

    print("Per Python file: tests (reference-only excluded), twinned, retired, pending")
    for file in sorted(per_file):
        c = per_file[file]
        mark = "  (pending)" if file in pending else ""
        print(f"  {file}: {c['tests']} tests, {c['twinned']} twinned, {c['retired']} retired, "
              f"{c['pending']} pending{mark}")
    total = {k: sum(c[k] for c in per_file.values()) for k in ("tests", "twinned", "retired", "pending")}
    print(f"  total: {total['tests']} tests, {total['twinned']} twinned, {total['retired']} retired, "
          f"{total['pending']} pending; {len(reference)} reference-only not converted; "
          f"{len(rust)} Rust tests listed")
    retired = [e for e in entries.values() if e.rust is None]
    print(f"Retired ({len(retired)}):")
    for e in sorted(retired, key=lambda e: e.line):
        print(f"  {e.py}: {e.reason}" + (f"  [note: {e.note}]" if e.note else ""))
    notes = [e for e in entries.values() if e.rust is not None and e.note]
    print(f"Notes ({len(notes)}):")
    for e in sorted(notes, key=lambda e: e.line):
        print(f"  {e.py} -> {e.rust}: {e.note}")
    retired_ids = {e.py for e in entries.values() if e.rust is None}
    zero = sorted(pid for pid, t in py.items() if t.asserts == 0 and pid not in retired_ids)
    print(f"Python tests that count 0 assertions, to compare with their twins by hand ({len(zero)}):")
    for pid in zero:
        print(f"  {pid}")
    stale = sorted(f for f in pending if f in per_file and per_file[f]["pending"] == 0)
    if stale:
        print(f"Pending files with no missing twin (drop --pending): {', '.join(stale)}")
    if errors:
        print(f"FAILED: {len(errors)} error(s)")
        for e in errors:
            print(f"  {e}")
        return 1
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
