//! Repository layouts this version refuses.
//!
//! The Rust form of `tests/contract/contract_layouts.py` (converted from
//! tests/test_unsupported_layouts.py): in-repo docs dirs are not supported in this version
//! (rulings R48, R48'), so `check` errors and `start` refuses before creating anything.
//! `InPlace.test_python_accepts_run_worktree_false` is reference-only (deliberate change 1
//! removes the in-place layout) and leaves with the Python.

#[macro_use]
mod common;

use std::fs;
use std::path::Path;

use common::git::branches;
use common::resolve;
use common::supervisor::FakeSupervisor;

const NOT_SUPPORTED: &str = "not supported in this version: ";

/// `check` and `start` both refuse with `expected`, and `start` creates nothing: no docs dir,
/// no branch, no worktree (`assert_refused`).
macro_rules! assert_refused {
    ($w:expr, $docs:expr, $expected:expr) => {{
        let w: &FakeSupervisor = &$w;
        let docs: &Path = &$docs;
        let expected: &str = &$expected;
        let before = branches(&w.repo);
        let r = w.cli(&os_args!["check", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        assert!(
            r.stdout_text().contains(&format!("error: {expected}")),
            "{r}"
        );
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().contains(expected), "{r}");
        assert!(!docs.exists(), "{}", docs.display());
        assert_eq!(branches(&w.repo), before);
        assert!(!w.repo.join(".worktrees").exists());
    }};
}

mod unsupported {
    use super::*;
    use crate::common::supervisor::card;

    #[test]
    fn test_docs_dir_inside_the_repo_is_refused() {
        let w = FakeSupervisor::new(&[card(1)]);
        let docs = w.repo.join("runs docs");
        fs::write(
            &w.config_file,
            format!("docs_dir = \"{}\"\n", docs.display()),
        )
        .unwrap();
        assert_refused!(
            w,
            docs,
            format!(
                "not supported in this version: docs_dir inside the repository ({})",
                resolve(&docs).display()
            )
        );
    }

    #[test]
    fn test_relative_docs_dir_is_resolved_against_the_repo() {
        let w = FakeSupervisor::new(&[card(1)]);
        fs::write(&w.config_file, "docs_dir = \"notes/leadhand\"\n").unwrap();
        let docs = resolve(&w.repo.join("notes").join("leadhand"));
        assert_refused!(
            w,
            docs,
            format!(
                "not supported in this version: docs_dir inside the repository ({})",
                docs.display()
            )
        );
    }

    #[test]
    fn test_defaults_are_supported() {
        let w = FakeSupervisor::new(&[card(1)]);
        let r = w.cli(&os_args!["check", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 0, "{r}");
        let stdout = r.stdout_text();
        assert!(stdout.lines().any(|l| l == "ok: 1 cards"), "{r}");
        let refused: Vec<&str> = stdout
            .lines()
            .filter(|l| l.contains(NOT_SUPPORTED))
            .collect();
        assert_eq!(refused, Vec::<&str>::new(), "{r}");
    }
}
