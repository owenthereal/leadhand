//! The catalogue's integrity: `docs/test-catalogue.md` maps every unit test of the removed
//! Python implementation to where its behaviour is tested now, and every row resolves.
//!
//! The Rust form of the Python contract suite's `contract_catalogue` (ruling S3-R11). Static:
//! it reads files and never runs LEADHAND_BIN or cargo. At the cut-over the catalogue moved to
//! `docs/` with Rust test IDs; its check that every Python unit test has a row retired with
//! those tests.
//!
//! - **Rows.** `| <key> | <disposition> |`, below the table's header.
//! - **Keys.** `test_<x>::<Class>::test_<y>`, the Python test's file name without its
//!   extension, its class and its method. No key appears twice.
//! - **Dispositions.** A fixed one (`FIXED`), `internal: Rust unit test (<module>)`, or one or
//!   more parts separated by `; `. A part is `reference-only (removed with the Python
//!   reference)` or a Rust test, `<file>::<path>`, that exists: a `#[test]` function at that
//!   path in `tests/<file>.rs` (or `tests/<file>/main.rs`), found by
//!   `common::rust_src::test_names`.

#[macro_use]
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use common::plugin::root;
use common::rust_src::test_names;

/// The dispositions that name no Rust test.
const FIXED: [&str; 2] = [
    "dropped with in-place mode",
    "plugin file check (plugin_files)",
];

/// The part of a disposition that names a test which pinned a Python behaviour the Rust binary
/// changes on purpose. It left with the Python and has no Rust form.
const REFERENCE_ONLY: &str = "reference-only (removed with the Python reference)";

fn catalogue_path() -> PathBuf {
    root().join("docs/test-catalogue.md")
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

/// KEY: `^test_\w+::\w+::test_\w+$`.
fn is_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split("::").collect();
    parts.len() == 3
        && word_after(parts[0], "test_")
        && is_word(parts[1])
        && word_after(parts[2], "test_")
}

/// A Rust test ID, `<file>::<path>`: a file stem `[a-z_][a-z0-9_]*`, then one or more `::`
/// segments, each an identifier. As (file, path).
fn rust_id(part: &str) -> Option<(&str, &str)> {
    let (file, path) = part.split_once("::")?;
    let file_ok = file
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && file
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let ident = |s: &str| {
        s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    (file_ok && path.split("::").all(ident)).then_some((file, path))
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

/// (Python test, disposition) for every table row below the header.
fn catalogue_rows() -> Vec<(String, String)> {
    read(&catalogue_path())
        .lines()
        .filter_map(row)
        .filter(|(key, _)| *key != "Python test" && *key != "---")
        .map(|(k, d)| (k.to_string(), d.to_string()))
        .collect()
}

/// What resolving one part of a disposition found.
#[derive(Clone, Debug, PartialEq)]
enum Resolved {
    /// A Rust test that exists.
    Rust(String),
    /// A reference-only test: no Rust form, and none needed.
    ReferenceOnly,
}

/// Resolves disposition parts to Rust tests, reading each test file once.
#[derive(Default)]
struct Resolver {
    rust: BTreeMap<String, Option<BTreeSet<String>>>,
}

impl Resolver {
    fn new() -> Resolver {
        Resolver::default()
    }

    /// Whether `id` (`<file>::<path>`) is a `#[test]` function in tests/<file>.rs (or
    /// tests/<file>/main.rs).
    fn rust_test_exists(&mut self, id: &str) -> bool {
        let (file, path) = id.split_once("::").unwrap_or((id, ""));
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

    /// The part resolved, or why it does not resolve.
    fn resolve(&mut self, part: &str) -> Result<Resolved, String> {
        if part == REFERENCE_ONLY {
            return Ok(Resolved::ReferenceOnly);
        }
        let (file, _) = rust_id(part).ok_or_else(|| format!("not a disposition: {part:?}"))?;
        if self.rust_test_exists(part) {
            Ok(Resolved::Rust(part.to_string()))
        } else if self.rust.get(file).is_some_and(Option::is_none) {
            Err(format!("{part}: no file tests/{file}.rs"))
        } else {
            Err(format!("{part}: not a test in tests/{file}.rs"))
        }
    }
}

// --------------------------------------------------------------------------

/// The row-integrity half of `Catalogue.test_every_python_test_is_catalogued`: every key is
/// well formed, and no key repeats. Its other half, that every Python unit test has a row,
/// retired with those tests at the cut-over.
#[test]
fn every_catalogue_key_is_well_formed_and_unique() {
    let rows = catalogue_rows();
    // The catalogue holds 655 rows: a reader that found few would check little.
    assert!(rows.len() > 600, "{} catalogue rows", rows.len());
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, _) in &rows {
        *counts.entry(key.as_str()).or_insert(0) += 1;
    }
    let duplicated: Vec<&str> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, _)| *k)
        .collect();
    let malformed: Vec<&str> = counts.keys().copied().filter(|k| !is_key(k)).collect();
    assert_eq!(
        duplicated,
        Vec::<&str>::new(),
        "Python tests with more than one row"
    );
    assert_eq!(
        malformed,
        Vec::<&str>::new(),
        "rows whose key is not test_<x>::<Class>::test_<y>"
    );
}

mod catalogue {
    use super::*;

    #[test]
    fn test_every_disposition_is_valid_and_every_rust_test_exists() {
        let mut resolver = Resolver::new();
        let mut bad = Vec::new();
        let (mut rust, mut reference_only) = (0, 0);
        for (key, disposition) in catalogue_rows() {
            if FIXED.contains(&disposition.as_str()) || is_internal(&disposition) {
                continue;
            }
            for part in disposition.split("; ") {
                match resolver.resolve(part) {
                    Ok(Resolved::Rust(_)) => rust += 1,
                    Ok(Resolved::ReferenceOnly) => reference_only += 1,
                    Err(why) => bad.push(format!("{key}: {why}")),
                }
            }
        }
        assert_eq!(bad, Vec::<String>::new());
        // The rows name hundreds of Rust tests: a reader that found none would pass the check
        // above with nothing checked.
        assert!(
            rust > 500,
            "{rust} parts resolved to a Rust test ({reference_only} reference-only)"
        );
    }
}

mod checker_is_strict {
    //! The readers above see what they must: a real row and its Rust test, a reference-only
    //! part, a missing test, a missing file, and keys and dispositions in the wrong form.
    use super::*;

    #[test]
    fn test_parsers_find_known_entries() {
        let key = "test_cli::CliTests::test_unknown_command_exits_2";
        assert!(is_key(key));
        assert!(catalogue_rows().contains(&(
            key.to_string(),
            "cli::usage::test_unknown_command_exits_2".to_string()
        )));
        for wrong in [
            "test_widget.py::WidgetTests::test_x",
            "test_widget::WidgetTests",
            "widget::WidgetTests::test_x",
            "test_widget::WidgetTests::x",
        ] {
            assert!(!is_key(wrong), "{wrong}");
        }
        for good in [
            "internal: Rust unit test (state)",
            "internal: Rust unit test (testing-tools)",
        ] {
            assert!(is_internal(good), "{good}");
        }
        for wrong in [
            "internal: Rust unit test",
            "internal: state",
            "plugin file check (contract_plugin_files)",
        ] {
            assert!(
                !(is_internal(wrong) || FIXED.contains(&wrong) || rust_id(wrong).is_some()),
                "{wrong}"
            );
        }

        // The Rust side: a test that exists, a reference-only part, a missing test, a missing
        // file, and a contract-test ID of the old form.
        let mut r = Resolver::new();
        assert_eq!(
            r.resolve("cli::usage::test_unknown_command_exits_2"),
            Ok(Resolved::Rust(
                "cli::usage::test_unknown_command_exits_2".to_string()
            ))
        );
        assert_eq!(r.resolve(REFERENCE_ONLY), Ok(Resolved::ReferenceOnly));
        let missing = r.resolve("cli::usage::test_no_such_test");
        assert!(
            missing
                .as_ref()
                .is_err_and(|e| e.contains("not a test in tests/cli.rs")),
            "{missing:?}"
        );
        let no_file = r.resolve("no_such_file::usage::test_x");
        assert!(
            no_file
                .as_ref()
                .is_err_and(|e| e.contains("no file tests/no_such_file.rs")),
            "{no_file:?}"
        );
        let old_form = r.resolve("contract_cli.Usage.test_unknown_command_exits_2");
        assert!(
            old_form
                .as_ref()
                .is_err_and(|e| e.contains("not a disposition")),
            "{old_form:?}"
        );
        assert!(!r.rust_test_exists("cli::usage::test_no_such_twin"));
        let src = "mod m {\n    #[test]\n    fn a() {}\n    fn helper() {}\n    // #[test] fn b() {}\n    #[test]\n    fn c() { let s = \"#[test] fn d() {}\"; }\n}\n#[test]\nfn top() {}\n";
        assert_eq!(
            test_names(src),
            BTreeSet::from(["m::a".to_string(), "m::c".to_string(), "top".to_string()])
        );
    }
}
