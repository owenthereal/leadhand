//! The catalogue's completeness: every Python unit test has exactly one row in
//! `tests/contract/CATALOG.md`, and every contract test a row names resolves to a Rust test
//! that exists (or is a reference-only test, which leaves with the Python).
//!
//! The Rust form of `tests/contract/contract_catalogue.py` (ruling S3-R11). Static: it reads
//! files and never runs LEADHAND_BIN, cargo or Python.
//!
//! - **Rows.** CATALOG.md is read as it stands (the Python check keeps passing until Task 10
//!   moves it to `docs/test-catalogue.md` with Rust IDs).
//! - **Python tests.** `tests/test_*.py` and `tests/contract/contract_*.py` are read with a
//!   small Python reader (`python_defs`): comments and string literals blanked, then the
//!   logical lines outside brackets, so a top-level `class` and the `def`s of its body are
//!   what `ast` sees in `node.body`. Decorators are kept, for `@reference_only`.
//! - **Resolution.** A contract ID `contract_<x>.<Class>.<test>` must name a test method of
//!   that class in `tests/contract/contract_<x>.py` (the Python check's rule). It is valid
//!   when that test is `@reference_only` (on the method or its class), or when its Rust
//!   twin exists: the twin is `conversion-map.txt`'s entry for it, or the default name of
//!   ruling S3-R6 with S3-R20's keyword rule (`<x>::<snake_case(Class)>::<test>`). A twin
//!   exists when `tests/<x>.rs` (or `tests/<x>/main.rs`) has a `#[test]` function at that
//!   path (`common::rust_src::test_names`). A test the map retires has no twin, so a row
//!   naming one is an error.

#[macro_use]
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use common::plugin::root;
use common::rust_src::test_names;

/// The dispositions that name no contract test.
const FIXED: [&str; 2] = [
    "dropped with in-place mode",
    "plugin file check (contract_plugin_files)",
];

/// The words `check_conversion.py` suffixes with `_` in a default twin name (S3-R20).
const RUST_KEYWORDS: [&str; 52] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
];

fn catalog_path() -> PathBuf {
    root().join("tests/contract/CATALOG.md")
}

fn map_path() -> PathBuf {
    root().join("tests/contract/conversion-map.txt")
}

#[track_caller]
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Python's `\w`: a Unicode letter or digit, or `_`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `\w+`.
fn is_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_word_char)
}

/// `s` is `start` followed by `\w+`.
fn word_after(s: &str, start: &str) -> bool {
    s.strip_prefix(start).is_some_and(is_word)
}

/// KEY: `^test_\w+\.py::\w+::test_\w+$`.
fn is_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split("::").collect();
    parts.len() == 3
        && parts[0]
            .strip_suffix(".py")
            .is_some_and(|stem| word_after(stem, "test_"))
        && is_word(parts[1])
        && word_after(parts[2], "test_")
}

/// CONTRACT_ID: `^(contract_\w+)\.(\w+)\.(test_\w+)$`, as (module, class, method).
fn contract_id(part: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = part.split('.').collect();
    match parts[..] {
        [module, cls, method]
            if word_after(module, "contract_") && is_word(cls) && word_after(method, "test_") =>
        {
            Some((module, cls, method))
        }
        _ => None,
    }
}

/// INTERNAL: `^internal: Rust unit test \([a-z][a-z-]*\)$`.
fn is_internal(disposition: &str) -> bool {
    disposition
        .strip_prefix("internal: Rust unit test (")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|module| {
            module
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
                && module.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        })
}

/// ROW: `^\| (?P<key>[^|]+?) \| (?P<disposition>[^|]+?) \|$`, as (key, disposition).
fn row(line: &str) -> Option<(&str, &str)> {
    let inner = line.strip_prefix("| ")?.strip_suffix(" |")?;
    let (key, disposition) = inner.split_once(" | ")?;
    (!key.is_empty() && !disposition.is_empty() && !key.contains('|') && !disposition.contains('|'))
        .then_some((key, disposition))
}

/// (Python test, disposition) for every table row below the header (`catalogue_rows`).
fn catalogue_rows() -> Vec<(String, String)> {
    read(&catalog_path())
        .lines()
        .filter_map(row)
        .filter(|(key, _)| *key != "Python test" && *key != "---")
        .map(|(k, d)| (k.to_string(), d.to_string()))
        .collect()
}

// --------------------------------------------------------------------------
// A small Python reader

/// The 1-based line of the character at `i`.
fn line_of(c: &[char], i: usize) -> usize {
    c[..i.min(c.len())].iter().filter(|ch| **ch == '\n').count() + 1
}

/// Python source with comments and string literals blanked out (newlines kept). Panics,
/// naming `file` and the line, on a string that never closes: a one-line string at its
/// line's end, or any string at the end of the file. The reader would misread everything
/// after it.
#[track_caller]
fn python_code_only(file: &str, src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
    let mut i = 0;
    while i < c.len() {
        let prev_word = i > 0 && is_word_char(c[i - 1]);
        if c[i] == '#' {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
            continue;
        }
        // A string: up to two prefix letters (r, b, u, f), then a quote.
        let mut j = i;
        if !prev_word {
            while j < c.len() && j - i < 2 && "rRbBuUfF".contains(c[j]) {
                j += 1;
            }
        }
        if j < c.len() && (c[j] == '\'' || c[j] == '"') && (j == i || !prev_word) {
            let q = c[j];
            let triple = c.get(j + 1) == Some(&q) && c.get(j + 2) == Some(&q);
            let mut k = j + if triple { 3 } else { 1 };
            let unterminated = || -> ! {
                panic!(
                    "{file}:{}: a string that never closes (the Python reader stops here)",
                    line_of(&c, i)
                )
            };
            let end = loop {
                if k >= c.len() {
                    unterminated();
                }
                if c[k] == '\\' {
                    k += 2;
                } else if triple
                    && c[k] == q
                    && c.get(k + 1) == Some(&q)
                    && c.get(k + 2) == Some(&q)
                {
                    break k + 3;
                } else if !triple && c[k] == q {
                    break k + 1;
                } else if !triple && c[k] == '\n' {
                    unterminated();
                } else {
                    k += 1;
                }
            };
            for &ch in &c[i..end] {
                blank(&mut out, ch);
            }
            i = end;
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// The logical lines of blanked Python code that start a statement: (indent, text), the
/// continuation lines inside brackets or after a `\` left out. Panics, naming `file` and the
/// line, on a closing bracket with none open, and on brackets still open at the end of the
/// file: the reader would take the wrong lines for statements.
#[track_caller]
fn logical_lines(file: &str, code: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut depth: usize = 0;
    let mut opened = 0;
    let mut continued = false;
    for (n, line) in code.split('\n').enumerate() {
        if depth == 0 && !continued && !line.trim().is_empty() {
            let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
            out.push((indent, line.trim().to_string()));
        }
        for ch in line.chars() {
            match ch {
                '(' | '[' | '{' => {
                    if depth == 0 {
                        opened = n + 1;
                    }
                    depth += 1;
                }
                ')' | ']' | '}' => {
                    depth = depth.checked_sub(1).unwrap_or_else(|| {
                        panic!("{file}:{}: a closing {ch:?} with no bracket open", n + 1)
                    });
                }
                _ => {}
            }
        }
        continued = line.trim_end().ends_with('\\');
    }
    if depth != 0 {
        panic!("{file}:{opened}: a bracket opened here is still open at the end of the file");
    }
    out
}

/// The name a `def`/`async def` (or `class`) line defines.
fn defined(text: &str, keyword: &str) -> Option<String> {
    let text = if keyword == "def" {
        text.strip_prefix("async ").map_or(text, str::trim_start)
    } else {
        text
    };
    let rest = text.strip_prefix(keyword)?;
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| is_word_char(*c))
        .collect();
    (!name.is_empty()).then_some(name)
}

/// A decorator line's name (`@harness.reference_only` -> `harness.reference_only`).
fn decorator(text: &str) -> String {
    let rest = text.trim_start_matches('@').trim();
    rest.split('(').next().unwrap_or("").trim().to_string()
}

#[derive(Debug, Default)]
struct PyClass {
    name: String,
    decorators: Vec<String>,
    /// The `def`s of the class body, each with its decorators.
    methods: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Default)]
struct PyModule {
    classes: Vec<PyClass>,
    /// The module-level `def`s.
    functions: Vec<String>,
}

/// The top-level classes (with their body's `def`s) and functions of Python source, as
/// `ast.parse(...).body` holds them.
#[track_caller]
fn python_defs(file: &str, src: &str) -> PyModule {
    let mut module = PyModule::default();
    let mut current: Option<usize> = None;
    let mut body_indent: Option<usize> = None;
    let mut top_decorators: Vec<String> = Vec::new();
    let mut body_decorators: Vec<String> = Vec::new();
    for (indent, text) in logical_lines(file, &python_code_only(file, src)) {
        if indent == 0 {
            body_decorators.clear();
            if text.starts_with('@') {
                top_decorators.push(decorator(&text));
                continue;
            }
            if let Some(name) = defined(&text, "def") {
                module.functions.push(name);
                current = None;
            } else if let Some(name) = defined(&text, "class") {
                module.classes.push(PyClass {
                    name,
                    decorators: std::mem::take(&mut top_decorators),
                    methods: Vec::new(),
                });
                current = Some(module.classes.len() - 1);
                body_indent = None;
            } else {
                current = None;
            }
            top_decorators.clear();
            continue;
        }
        let Some(ci) = current else { continue };
        let body = *body_indent.get_or_insert(indent);
        if indent != body {
            continue;
        }
        if text.starts_with('@') {
            body_decorators.push(decorator(&text));
        } else if let Some(name) = defined(&text, "def") {
            let decorators = std::mem::take(&mut body_decorators);
            module.classes[ci].methods.push((name, decorators));
        } else {
            body_decorators.clear();
        }
    }
    module
}

/// `file::Class::name` for every `def test_` in a top-level class of tests/test_*.py, and
/// `file::<module>::name` for a top-level one (`python_tests`).
fn python_tests() -> Vec<String> {
    let mut files: Vec<PathBuf> = fs::read_dir(root().join("tests"))
        .expect("reading tests/")
        .map(|e| e.expect("reading tests/").path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("test_") && n.ends_with(".py"))
        })
        .collect();
    files.sort();
    let mut keys = Vec::new();
    for path in files {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let module = python_defs(&format!("tests/{name}"), &read(&path));
        // ast's body order: classes and functions interleaved; the order does not matter here.
        for class in &module.classes {
            for (method, _) in &class.methods {
                if method.starts_with("test_") {
                    keys.push(format!("{name}::{}::{method}", class.name));
                }
            }
        }
        for function in &module.functions {
            if function.starts_with("test_") {
                keys.push(format!("{name}::<module>::{function}"));
            }
        }
    }
    keys
}

/// A contract file's test classes: {class: {test method: reference-only}}.
type Classes = BTreeMap<String, BTreeMap<String, bool>>;

/// {class: {test method: reference-only}} of tests/contract/<module>.py, or None if there is
/// no such file (`contract_methods`, with each test's `@reference_only`, on the method or its
/// class).
fn contract_methods(module: &str) -> Option<Classes> {
    let path = root().join(format!("tests/contract/{module}.py"));
    if !path.is_file() {
        return None;
    }
    let reference_only = |decorators: &[String]| {
        decorators
            .iter()
            .any(|d| d == "reference_only" || d.ends_with(".reference_only"))
    };
    Some(
        python_defs(&format!("tests/contract/{module}.py"), &read(&path))
            .classes
            .into_iter()
            .map(|class| {
                let class_ref = reference_only(&class.decorators);
                let methods = class
                    .methods
                    .iter()
                    .filter(|(m, _)| m.starts_with("test_"))
                    .map(|(m, d)| (m.clone(), class_ref || reference_only(d)))
                    .collect();
                (class.name, methods)
            })
            .collect(),
    )
}

// --------------------------------------------------------------------------
// The Rust twins

/// `check_conversion.py`'s snake_case: `_` before a capital that follows a lower-case letter
/// or digit, and before the last capital of a run that a lower-case letter follows, then
/// lower-cased; a keyword gets a trailing `_`.
fn snake(name: &str) -> String {
    let c: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &ch) in c.iter().enumerate() {
        if i > 0 {
            let prev = c[i - 1];
            let after_lower =
                (prev.is_ascii_lowercase() || prev.is_ascii_digit()) && ch.is_ascii_uppercase();
            let ends_run = prev.is_ascii_uppercase()
                && ch.is_ascii_uppercase()
                && c.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if after_lower || ends_run {
                out.push('_');
            }
        }
        out.extend(ch.to_lowercase());
    }
    keyword_safe(out)
}

fn keyword_safe(name: String) -> String {
    if RUST_KEYWORDS.contains(&name.as_str()) {
        name + "_"
    } else {
        name
    }
}

/// The default twin of a contract test (rulings S3-R6, S3-R20).
fn default_twin(module: &str, cls: &str, method: &str) -> String {
    let file = keyword_safe(module.trim_start_matches("contract_").to_string());
    format!("{file}::{}::{method}", snake(cls))
}

/// A conversion-map.txt target: a Rust test, or retired (with its reason).
#[derive(Clone, Debug, PartialEq)]
enum Target {
    Rust(String),
    Retired(String),
}

/// conversion-map.txt's entries: Python ID -> target. An entry is
/// `<python id> -> <target>[ # <note>]`.
fn read_map() -> BTreeMap<String, Target> {
    let mut entries = BTreeMap::new();
    for raw in read(&map_path()).lines() {
        let text = raw.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let (py, rest) = text
            .split_once(char::is_whitespace)
            .unwrap_or_else(|| panic!("conversion-map.txt: not an entry: {raw:?}"));
        let rest = rest.trim_start();
        let after = rest
            .strip_prefix("->")
            .filter(|r| r.starts_with(char::is_whitespace))
            .unwrap_or_else(|| panic!("conversion-map.txt: not an entry: {raw:?}"))
            .trim_start();
        let target = note_start(after).map_or(after, |at| &after[..at]).trim();
        let target = match target.strip_prefix("retired:") {
            Some(reason) => Target::Retired(reason.trim().to_string()),
            None => Target::Rust(target.to_string()),
        };
        entries.insert(py.to_string(), target);
    }
    entries
}

/// Where a note starts in an entry's target part: the first whitespace run followed by `#`
/// and whitespace (ENTRY's `(?:\s+#\s+(?P<note>.*))?$` after a lazy target).
fn note_start(s: &str) -> Option<usize> {
    let c: Vec<(usize, char)> = s.char_indices().collect();
    (0..c.len()).find_map(|i| {
        if !c[i].1.is_whitespace() {
            return None;
        }
        let mut j = i;
        while j < c.len() && c[j].1.is_whitespace() {
            j += 1;
        }
        (j < c.len() && c[j].1 == '#' && c.get(j + 1).is_some_and(|(_, ch)| ch.is_whitespace()))
            .then_some(c[i].0)
    })
}

/// What resolving one contract ID found.
#[derive(Clone, Debug, PartialEq)]
enum Resolved {
    /// Its Rust twin, which exists.
    Twin(String),
    /// A reference-only Python test: no twin, and none needed.
    ReferenceOnly,
}

/// Resolves contract IDs to their Rust twins, reading each file once.
#[derive(Default)]
struct Resolver {
    map: BTreeMap<String, Target>,
    modules: BTreeMap<String, Option<Classes>>,
    rust: BTreeMap<String, Option<BTreeSet<String>>>,
}

impl Resolver {
    fn new() -> Resolver {
        Resolver {
            map: read_map(),
            ..Resolver::default()
        }
    }

    /// Whether `twin` (`<file>::<path>`) is a `#[test]` function in tests/<file>.rs (or
    /// tests/<file>/main.rs).
    fn rust_test_exists(&mut self, twin: &str) -> bool {
        let (file, path) = twin.split_once("::").unwrap_or((twin, ""));
        let names = self.rust.entry(file.to_string()).or_insert_with(|| {
            [
                root().join(format!("tests/{file}.rs")),
                root().join(format!("tests/{file}/main.rs")),
            ]
            .iter()
            .find(|p| p.is_file())
            .map(|p| test_names(&read(p)))
        });
        names.as_ref().is_some_and(|n| n.contains(path))
    }

    /// The contract ID `part` resolved, or why it does not resolve.
    fn resolve(&mut self, part: &str) -> Result<Resolved, String> {
        let (module, cls, method) =
            contract_id(part).ok_or_else(|| format!("not a disposition: {part:?}"))?;
        let classes = self
            .modules
            .entry(module.to_string())
            .or_insert_with(|| contract_methods(module));
        let Some(classes) = classes else {
            return Err(format!("no file tests/contract/{module}.py"));
        };
        let Some(&reference_only) = classes.get(cls).and_then(|m| m.get(method)) else {
            return Err(format!("no test {part}"));
        };
        if reference_only {
            return Ok(Resolved::ReferenceOnly);
        }
        let twin = match self.map.get(part) {
            Some(Target::Retired(reason)) => {
                return Err(format!(
                    "{part} is retired in conversion-map.txt ({reason}): it has no Rust twin"
                ));
            }
            Some(Target::Rust(twin)) => twin.clone(),
            None => default_twin(module, cls, method),
        };
        if self.rust_test_exists(&twin) {
            Ok(Resolved::Twin(twin))
        } else {
            Err(format!(
                "{part}: its Rust twin {twin} is not a test in tests/"
            ))
        }
    }
}

// --------------------------------------------------------------------------

/// `Catalogue.test_every_python_test_is_catalogued` (conversion-map.txt): each Python unit
/// test has exactly one row, and every row names a Python unit test, by a well-formed key.
#[test]
fn every_catalogue_row_names_an_existing_test() {
    let tests = python_tests();
    assert!(tests.len() > 600, "{} Python tests", tests.len());
    let rows = catalogue_rows();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, _) in &rows {
        *counts.entry(key.as_str()).or_insert(0) += 1;
    }
    let known: BTreeSet<&str> = tests.iter().map(String::as_str).collect();
    let missing: Vec<&str> = tests
        .iter()
        .map(String::as_str)
        .filter(|k| !counts.contains_key(k))
        .collect();
    let duplicated: Vec<&str> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, _)| *k)
        .collect();
    let stale: Vec<&str> = counts
        .keys()
        .copied()
        .filter(|k| !known.contains(k))
        .collect();
    let malformed: Vec<&str> = counts.keys().copied().filter(|k| !is_key(k)).collect();
    assert_eq!(
        missing,
        Vec::<&str>::new(),
        "{} Python tests have no catalogue row",
        missing.len()
    );
    assert_eq!(
        duplicated,
        Vec::<&str>::new(),
        "Python tests with more than one row"
    );
    assert_eq!(stale, Vec::<&str>::new(), "rows naming no Python test");
    assert_eq!(
        malformed,
        Vec::<&str>::new(),
        "rows whose key is not file::Class::test"
    );
}

mod catalogue {
    use super::*;

    #[test]
    fn test_every_disposition_is_valid_and_every_contract_id_exists() {
        let mut resolver = Resolver::new();
        let mut bad = Vec::new();
        let (mut twins, mut reference_only) = (0, 0);
        for (key, disposition) in catalogue_rows() {
            if FIXED.contains(&disposition.as_str()) || is_internal(&disposition) {
                continue;
            }
            for part in disposition.split("; ") {
                match resolver.resolve(part) {
                    Ok(Resolved::Twin(_)) => twins += 1,
                    Ok(Resolved::ReferenceOnly) => reference_only += 1,
                    Err(why) => bad.push(format!("{key}: {why}")),
                }
            }
        }
        assert_eq!(bad, Vec::<String>::new());
        // The rows name hundreds of contract tests: a reader that found none would pass the
        // check above with nothing checked.
        assert!(
            twins > 500,
            "{twins} contract IDs resolved to a Rust twin ({reference_only} reference-only)"
        );
    }
}

mod checker_is_strict {
    //! The readers above see what they must: a real row, a real contract test and its twin,
    //! a missing one.
    use super::*;

    #[test]
    fn test_parsers_find_known_entries() {
        let key = "test_cli.py::CliTests::test_unknown_command_exits_2";
        assert!(python_tests().iter().any(|k| k == key));
        assert!(catalogue_rows().contains(&(
            key.to_string(),
            "contract_cli.Usage.test_unknown_command_exits_2".to_string()
        )));
        let cli = contract_methods("contract_cli").expect("tests/contract/contract_cli.py");
        assert!(cli["Usage"].contains_key("test_unknown_command_exits_2"));
        assert!(contract_methods("contract_no_such_file").is_none());
        for good in [
            "internal: Rust unit test (state)",
            "internal: Rust unit test (testing-tools)",
        ] {
            assert!(is_internal(good), "{good}");
        }
        for wrong in [
            "internal: Rust unit test",
            "internal: state",
            "contract_cli.Usage",
        ] {
            assert!(
                !(is_internal(wrong) || contract_id(wrong).is_some()),
                "{wrong}"
            );
        }

        // The Rust side: a default twin, a mapped one, a reference-only test, a retired one,
        // a missing one.
        let mut r = Resolver::new();
        assert_eq!(
            r.resolve("contract_cli.Usage.test_unknown_command_exits_2"),
            Ok(Resolved::Twin(
                "cli::usage::test_unknown_command_exits_2".to_string()
            ))
        );
        assert_eq!(
            r.resolve("contract_catalogue.Catalogue.test_every_python_test_is_catalogued"),
            Ok(Resolved::Twin(
                "catalogue::every_catalogue_row_names_an_existing_test".to_string()
            ))
        );
        assert_eq!(
            r.resolve(
                "contract_status.StatusWithoutRunDir.test_no_run_dir_picks_latest_unfinished_run"
            ),
            Ok(Resolved::ReferenceOnly)
        );
        let retired =
            r.resolve("contract_self.Tags.test_rust_only_runs_once_every_command_it_uses_is_rust");
        assert!(
            retired.as_ref().is_err_and(|e| e.contains("retired")),
            "{retired:?}"
        );
        let missing = r.resolve("contract_cli.Usage.test_no_such_test");
        assert!(
            missing.as_ref().is_err_and(|e| e.contains("no test")),
            "{missing:?}"
        );
        assert!(!r.rust_test_exists("cli::usage::test_no_such_twin"));
        assert_eq!(
            [
                snake("HTTPServer"),
                snake("Loop"),
                snake("FixRound1"),
                snake("Catalogue")
            ],
            ["http_server", "loop_", "fix_round1", "catalogue"]
        );
        let src = "mod m {\n    #[test]\n    fn a() {}\n    fn helper() {}\n    // #[test] fn b() {}\n    #[test]\n    fn c() { let s = \"#[test] fn d() {}\"; }\n}\n#[test]\nfn top() {}\n";
        assert_eq!(
            test_names(src),
            BTreeSet::from(["m::a".to_string(), "m::c".to_string(), "top".to_string()])
        );
        let py = "@reference_only\nclass A(unittest.TestCase):\n    s = \"\"\"\nclass B:\n    def test_no(self): pass\n\"\"\"\n\n    @unittest.skipUnless(\n        x,\n        \"why\")\n    def test_one(self):\n        def test_nested(): pass\n    async def test_two(self): pass\n\ndef test_module(): pass\n";
        let module = python_defs("sample.py", py);
        let methods: Vec<&str> = module.classes[0]
            .methods
            .iter()
            .map(|(m, _)| m.as_str())
            .collect();
        assert_eq!(
            (module.classes.len(), methods, module.functions.clone()),
            (
                1,
                vec!["test_one", "test_two"],
                vec!["test_module".to_string()]
            )
        );
        assert_eq!(module.classes[0].decorators, ["reference_only"]);
    }
}
