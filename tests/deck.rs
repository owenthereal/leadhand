//! The deck format and its checks, as `check` (one problem per line) and `route` report them,
//! and the parsed card as the worker's brief renders it ("The card, verbatim").
//!
//! The Rust form of the Python contract suite's `contract_deck` (converted from Python's
//! `test_deck`). Dependency order, the digest and the spec example's header are catalogued as Rust
//! unit tests.

#[macro_use]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::Out;

fn fixture() -> PathBuf {
    common::plugin::root()
        .join("tests")
        .join("fixtures")
        .join("deck-basic.md")
}

fn card_text(n: i64, role: &str, hard: &str, deps: &str) -> String {
    format!(
        "\n## Card {n}: Title {n}\n- Covers: R1\n- Role: {role}\n- Hard: {hard}\n- Risk: low\n- Depends on: {deps}\n- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n"
    )
}

const REQUIREMENTS: &str = "- R1 Thing → cards 1\n";

fn deck_text_with(
    cards: &[(i64, &str, &str, &str)],
    requirements: &str,
    build: &str,
    header_extra: &str,
) -> String {
    let head = format!(
        "# Deck: T\nSource: s\nBase: main @ abc1234\nBranch: b\nBuild: {build}\nTest: make test\n{header_extra}\n## Requirements\n{requirements}"
    );
    let cards: String = cards
        .iter()
        .map(|(n, r, h, d)| card_text(*n, r, h, d))
        .collect();
    head + &cards
}

fn deck_text(cards: &[(i64, &str, &str, &str)]) -> String {
    deck_text_with(cards, REQUIREMENTS, "make build", "")
}

/// `check`'s problem lines as (level, message); its closing `ok:` line is not a problem.
fn problems(result: &Out) -> Vec<(String, String)> {
    let stdout = result.stdout_text();
    let mut out = Vec::new();
    for line in stdout.lines() {
        if let Some((level, message)) = line.split_once(": ")
            && (level == "error" || level == "warning")
        {
            out.push((level.to_string(), message.to_string()));
        }
    }
    out
}

fn has(msgs: &[(String, String)], level: &str, message: &str) -> bool {
    msgs.iter().any(|(l, m)| l == level && m == message)
}

/// Each test's temp root and sealed environment (`DeckTestCase`).
struct DeckCase {
    tmp: common::TempRoot,
    env: common::Env,
    repos: usize,
}

impl DeckCase {
    fn new() -> DeckCase {
        let tmp = common::TempRoot::new("lh deck ");
        let env = common::Env::new(&tmp);
        DeckCase { tmp, env, repos: 0 }
    }

    /// Appends `text` to the user configuration file (LEADHAND_CONFIG).
    fn configure(&self, text: &str) {
        let path = Path::new(self.env.get("LEADHAND_CONFIG").unwrap());
        let mut old = fs::read_to_string(path).unwrap();
        old.push_str(text);
        fs::write(path, old).unwrap();
    }

    fn write(&self, text: &str) -> PathBuf {
        self.write_named(text, "deck.md")
    }

    fn write_named(&self, text: &str, name: &str) -> PathBuf {
        let p = self.tmp.join(name);
        fs::write(&p, text).unwrap();
        p
    }

    fn make_repo(&mut self) -> (PathBuf, String) {
        self.repos += 1;
        let repo = common::git::make_repo_named(&self.tmp, &format!("repo {}", self.repos), "main");
        let sha = common::git::commit(&repo, &[("src/a.txt", "a")], "init");
        (repo, sha)
    }

    fn check(&mut self, deck: &Path, repo: Option<&Path>) -> Out {
        let made;
        let repo = match repo {
            Some(r) => r,
            None => {
                made = self.make_repo().0;
                &made
            }
        };
        common::cli(
            &os_args!["check", deck, "--repo", repo],
            &self.env,
            &self.tmp,
        )
    }

    #[track_caller]
    fn messages(&mut self, deck: &Path, repo: Option<&Path>) -> Vec<(String, String)> {
        let r = self.check(deck, repo);
        assert!(r.code == 0 || r.code == 1, "{r}");
        problems(&r)
    }

    fn route(&self, deck: &Path, card: i64) -> Out {
        common::cli(
            &os_args![
                "route",
                deck,
                card.to_string(),
                "implementer",
                "--repo",
                self.tmp.path(),
                "--installed",
                "claude,codex"
            ],
            &self.env,
            &self.tmp,
        )
    }
}

mod deck {
    use super::*;

    #[test]
    fn test_hard_yes_requires_reason() {
        let mut t = DeckCase::new();
        let d = t.write(&deck_text(&[
            (1, "implementer", "no", "none"),
            (3, "implementer", "yes", "1"),
        ]));
        assert!(has(
            &t.messages(&d, None),
            "error",
            "card 3: Hard: yes needs a reason"
        ));
    }

    #[test]
    fn test_hard_yes_with_reason_forms() {
        // The reason itself goes into the brief; here the card counts as Hard (route) and the
        // reason is found (check).
        let mut t = DeckCase::new();
        for sep in ["—", "–", "-", ":"] {
            let hard = format!("yes {sep} no test can check it");
            let d = t.write(&deck_text(&[(1, "implementer", &hard, "none")]));
            let msgs = t.messages(&d, None);
            assert!(
                !has(&msgs, "error", "card 1: Hard: yes needs a reason"),
                "sep {sep}: {msgs:?}"
            );
            assert!(
                !msgs.iter().any(|(_, m)| m.starts_with("card 1:")),
                "sep {sep}: {msgs:?}"
            );
            let r = t.route(&d, 1).ok();
            let stdout = r.stdout_text();
            assert!(
                stdout
                    .lines()
                    .skip(1)
                    .any(|l| l == "role implementer_hard: card is Hard"),
                "sep {sep}: {r}"
            );
        }
    }

    #[test]
    fn test_fields_after_context_are_parsed() {
        let mut t = DeckCase::new();
        let text = deck_text(&[
            (1, "implementer", "no", "none"),
            (2, "implementer", "no", "none"),
        ])
        .replace(
            "- Depends on: none\n- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n",
            "- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n",
        ) + "\nContext: see plan.\n- Depends on: 1\n- Route: codex gpt-6.1-sol xhigh\n- Inferred: one\n- Inferred: two\n";
        let d = t.write(&text);
        let msgs = t.messages(&d, None);
        // Depends on was taken from after the context: card 2 is not missing it.
        assert!(
            !has(&msgs, "error", "card 2: missing Depends on"),
            "{msgs:?}"
        );
        assert!(
            !msgs.iter().any(|(_, m)| m.starts_with("card 2:")),
            "{msgs:?}"
        );
        let r = t.route(&d, 2).ok();
        let stdout = r.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], "codex gpt-6.1-sol xhigh", "{r}");
        assert!(
            lines[1..].contains(&"pinned: codex gpt-6.1-sol xhigh"),
            "{r}"
        );
    }

    #[test]
    fn test_unknown_key_bullet_warns() {
        // Covers R10.
        let mut t = DeckCase::new();
        let text = deck_text(&[(1, "implementer", "no", "none")])
            + "\n- Dependson: 2\n- Files: src/a.py\n";
        let p = t.write(&text);
        let base = text.lines().count() - 1;
        let msgs = t.messages(&p, None);
        assert!(
            has(
                &msgs,
                "warning",
                &format!(
                    "card 1: line {base}: \"Dependson\" is not a card field; treated as context"
                )
            ),
            "{msgs:?}"
        );
        assert!(
            has(
                &msgs,
                "warning",
                &format!(
                    "card 1: line {}: \"Files\" is not a card field; treated as context",
                    base + 1
                )
            ),
            "{msgs:?}"
        );
        assert!(
            !msgs
                .iter()
                .any(|(l, m)| l == "error" && m.contains("card field")),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_scout_card_rejected_in_this_version() {
        let mut t = DeckCase::new();
        let d = t.write(&deck_text(&[(1, "scout", "no", "none")]));
        let msgs = t.messages(&d, None);
        assert!(
            msgs.iter()
                .any(|(l, m)| l == "error"
                    && m.contains("not supported in this version: scout cards")),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_unsupported_config_reported_as_errors() {
        let mut t = DeckCase::new();
        t.configure("[run]\ncost_limit_usd = 5\n");
        let d = t.write(&deck_text(&[(1, "implementer", "no", "none")]));
        let msgs = t.messages(&d, None);
        assert!(
            has(
                &msgs,
                "error",
                "not supported in this version: run.cost_limit_usd = 5"
            ),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_dependency_cycle_is_error() {
        let mut t = DeckCase::new();
        let d = t.write(&deck_text(&[
            (1, "implementer", "no", "2"),
            (2, "implementer", "no", "1"),
        ]));
        let msgs = t.messages(&d, None);
        assert!(
            msgs.iter()
                .any(|(l, m)| l == "error" && m.contains("cycle")),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_unknown_dependency_and_duplicates_and_finish_and_build() {
        let mut t = DeckCase::new();
        let text = deck_text_with(
            &[
                (1, "implementer", "no", "9"),
                (1, "implementer", "no", "none"),
            ],
            REQUIREMENTS,
            "",
            "Finish: merge\n",
        );
        let d = t.write(&text);
        let msgs = t.messages(&d, None);
        let joined: Vec<&str> = msgs
            .iter()
            .filter(|(l, _)| l == "error")
            .map(|(_, m)| m.as_str())
            .collect();
        let joined = joined.join("\n");
        for needle in [
            "unknown card 9",
            "duplicate card number 1",
            "Finish",
            "Build",
        ] {
            assert!(joined.contains(needle), "{needle:?} not in\n{joined}");
        }
    }

    #[test]
    fn test_requirement_to_missing_card_is_error() {
        let mut t = DeckCase::new();
        let d = t.write(&deck_text_with(
            &[(1, "implementer", "no", "none")],
            "- R1 Thing → cards 1, 7\n",
            "make build",
            "",
        ));
        let msgs = t.messages(&d, None);
        assert!(
            msgs.iter()
                .any(|(l, m)| l == "error" && m.contains("card 7")),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_missing_required_fields_are_errors() {
        let mut t = DeckCase::new();
        let text = deck_text(&[(1, "implementer", "no", "none")]).replace("- Stop if: stuck\n", "");
        let d = t.write(&text);
        let msgs = t.messages(&d, None);
        assert!(has(&msgs, "error", "card 1: missing Stop if"), "{msgs:?}");
    }

    #[test]
    fn test_route_pin_parsed() {
        let t = DeckCase::new();
        let text =
            deck_text(&[(1, "implementer", "no", "none")]) + "- Route: codex gpt-6.1-sol xhigh\n";
        let r = t.route(&t.write(&text), 1).ok();
        let stdout = r.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], "codex gpt-6.1-sol xhigh", "{r}");
        assert!(
            lines[1..].contains(&"pinned: codex gpt-6.1-sol xhigh"),
            "{r}"
        );
    }

    #[test]
    fn test_parse_error_reports_line_number() {
        let mut t = DeckCase::new();
        let text =
            deck_text(&[(1, "implementer", "no", "none")]).replace("Risk: low", "Risk: severe");
        let d = t.write(&text);
        let r = t.check(&d, None);
        assert_eq!(r.code, 1, "{r}");
        let line = text.lines().position(|l| l == "- Risk: severe").unwrap() + 1;
        assert!(
            r.stdout_text()
                .starts_with(&format!("error: line {line}: ")),
            "{r}"
        );
    }

    #[test]
    fn test_uncovered_requirement_is_warning() {
        let mut t = DeckCase::new();
        let d = t.write(&deck_text_with(
            &[(1, "implementer", "no", "none")],
            "- R1 Thing → cards 1\n- R2 Other → cards none\n",
            "make build",
            "",
        ));
        let msgs = t.messages(&d, None);
        assert!(
            msgs.iter().any(|(l, m)| l == "warning" && m.contains("R2")),
            "{msgs:?}"
        );
        assert!(
            !msgs.iter().any(|(l, m)| l == "error" && m.contains("R2")),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_base_commit_and_paths() {
        let mut t = DeckCase::new();
        let (repo, sha) = t.make_repo();
        let good = deck_text(&[(1, "implementer", "no", "none")]).replace("abc1234", &sha);
        let d = t.write(&good);
        let msgs = t.messages(&d, Some(&repo));
        assert!(!msgs.iter().any(|(l, _)| l == "error"), "{msgs:?}");
        let bad = deck_text(&[(1, "implementer", "no", "none")]);
        let d = t.write(&bad);
        let msgs = t.messages(&d, Some(&repo));
        assert!(
            msgs.iter()
                .any(|(l, m)| l == "error" && m.contains("base commit")),
            "{msgs:?}"
        );
        let gone = good.replace("Allowed paths: src/", "Allowed paths: nope/");
        let d = t.write(&gone);
        let msgs = t.messages(&d, Some(&repo));
        assert!(
            msgs.iter()
                .any(|(l, m)| l == "warning" && m.contains("nope/")),
            "{msgs:?}"
        );
    }

    #[test]
    fn test_a_base_that_is_not_a_commit_hash_is_an_error() {
        // A branch name that resolves, and hex too short to be a hash, are both refused as
        // not a commit hash, before Git is asked.
        let mut t = DeckCase::new();
        let (repo, _) = t.make_repo();
        for base in ["main", "abc"] {
            let d =
                t.write(&deck_text(&[(1, "implementer", "no", "none")]).replace("abc1234", base));
            let msgs = t.messages(&d, Some(&repo));
            assert!(
                has(
                    &msgs,
                    "error",
                    &format!("base commit '{base}' is not a commit hash")
                ),
                "the base {base:?}: {msgs:?}"
            );
        }
    }

    #[test]
    fn test_compact_header_line_parses() {
        // Covers C5: the spec's deck example puts `Base:` and `Branch:` on one line; a known
        // header key after two or more spaces starts a new field. `check` finds no error and
        // `start` takes the base and the run branch from that line.
        let mut t = DeckCase::new();
        let (repo, sha) = t.make_repo();
        let text = deck_text(&[(1, "implementer", "no", "none")]).replace(
            "Base: main @ abc1234\nBranch: b\n",
            &format!("Base: main @ {sha}  Branch: leadhand/compact\n"),
        );
        let deck = t.write(&text);
        let r = t.check(&deck, Some(&repo));
        assert_eq!(r.code, 0, "{r}");
        let errors: Vec<String> = problems(&r)
            .into_iter()
            .filter(|(l, _)| l == "error")
            .map(|(_, m)| m)
            .collect();
        assert_eq!(errors, Vec::<String>::new(), "{r}");
        let r = common::cli(&os_args!["start", deck, "--repo", repo], &t.env, &t.tmp).ok();
        let stdout = r.stdout_text();
        let run_dir = PathBuf::from(
            stdout
                .lines()
                .find_map(|l| l.strip_prefix("run: "))
                .unwrap_or_else(|| panic!("no `run: ` line\n{r}")),
        );
        let meta = common::read_json(&run_dir.join("run.json"));
        assert_json_eq!(
            common::json::Json::from(vec![
                meta["base_ref"].clone(),
                meta["base_sha"].clone(),
                meta["branch"].clone()
            ]),
            common::json::Json::from(&["main", sha.as_str(), "leadhand/compact"][..])
        );
        assert_eq!(
            common::git::run_git(&repo, &["rev-parse", "refs/heads/leadhand/compact"]),
            sha
        );
    }

    #[test]
    fn test_check_cli_exits_1_on_error() {
        let mut t = DeckCase::new();
        let (repo, sha) = t.make_repo();
        let good = t.write_named(
            &deck_text(&[(1, "implementer", "no", "none")]).replace("abc1234", &sha),
            "good.md",
        );
        let bad = t.write_named(
            &deck_text(&[(1, "scout", "no", "none")]).replace("abc1234", &sha),
            "bad.md",
        );
        let r = t.check(&good, Some(&repo));
        assert_eq!(r.code, 0, "{r}");
        let r = t.check(&bad, Some(&repo));
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stdout_text().contains("scout"), "{r}");
    }
}

mod briefs {
    //! What the parser took from a card, read back from the card's first implement brief.
    use super::*;
    use crate::common::engine::card_section;
    use crate::common::supervisor::{FakeSupervisor, Options, card};

    /// The card's section of its first implement brief (`brief_card`).
    #[track_caller]
    fn brief_card(cards: &[String], number: i64, installed: &[&str]) -> Vec<String> {
        let mut w = FakeSupervisor::with_options(
            cards,
            Options {
                installed,
                ..Options::default()
            },
        );
        w.start();
        for _ in 0..20 {
            let a = w.next();
            if a["action"] == "dispatch" && a["card"] == number && a["kind"] == "implement" {
                let brief = fs::read_to_string(a["brief"].as_str().unwrap()).unwrap();
                return card_section(&brief, number);
            }
            assert!(a["action"] != "done", "{a}");
            w.act(&a);
        }
        panic!("card {number} was never dispatched");
    }

    fn contains(lines: &[String], line: &str) -> bool {
        lines.iter().any(|l| l == line)
    }

    #[test]
    fn test_basic_fixture_cards_reach_the_brief() {
        let text = fs::read_to_string(fixture()).unwrap();
        let cards: Vec<String> = text
            .split("\n## Card ")
            .skip(1)
            .map(|part| format!("## Card {part}"))
            .collect();
        assert_eq!(cards.len(), 2);
        let one = brief_card(&cards, 1, &["claude"]);
        assert!(contains(&one, "- Hard: no"), "{one:?}");
        assert!(contains(&one, "- Allowed paths: src/, tests/"), "{one:?}");
        assert_eq!(one.last().unwrap(), "see plan Task 1.", "{one:?}");
        let two = brief_card(&cards, 2, &["claude"]);
        assert!(contains(&two, "- Hard: no"), "{two:?}");
        assert!(contains(&two, "- Depends on: 1"), "{two:?}");
        assert!(!two.iter().any(|l| l.starts_with("- Route:")), "{two:?}");
        assert_eq!(
            two.iter().filter(|l| l.starts_with("- Inferred: ")).count(),
            2,
            "{two:?}"
        );
        // The requirement maps to both cards, and the base is the header's commit.
        let tmp = common::TempRoot::new("lh deck ");
        let env = common::Env::new(&tmp);
        let repo = common::git::make_repo(&tmp);
        let r = common::cli(&os_args!["check", fixture(), "--repo", repo], &env, &tmp);
        let stdout = r.stdout_text();
        let resolved = fs::canonicalize(&repo).unwrap();
        let want = format!(
            "error: base commit 3f2a9c1 not found in {}",
            resolved.display()
        );
        assert!(stdout.lines().any(|l| l == want), "{r}");
        assert!(!stdout.lines().any(|l| l.contains("requirement")), "{r}");
    }

    #[test]
    fn test_hard_yes_reason_is_rendered() {
        for sep in ["—", "–", "-", ":"] {
            let one = card(1).replace(
                "- Hard: no\n",
                &format!("- Hard: yes {sep} no test can check it\n"),
            );
            let lines = brief_card(&[one], 1, &["claude"]);
            assert!(
                contains(&lines, "- Hard: yes — no test can check it"),
                "sep {sep}: {lines:?}"
            );
        }
    }

    #[test]
    fn test_fields_after_context_are_parsed() {
        let two = card(2).replace("- Depends on: none\n", "")
            + "\nContext: see plan.\n- Depends on: 1\n- Route: codex gpt-6.1-sol xhigh\n- Inferred: one\n- Inferred: two\n";
        let lines = brief_card(&[card(1), two], 2, &["claude", "codex"]);
        assert!(contains(&lines, "- Depends on: 1"), "{lines:?}");
        assert!(
            contains(&lines, "- Route: codex gpt-6.1-sol xhigh"),
            "{lines:?}"
        );
        let inferred: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with("- Inferred: "))
            .collect();
        assert_eq!(inferred, ["- Inferred: one", "- Inferred: two"]);
        assert_eq!(lines[lines.len() - 2..], ["", "see plan."], "{lines:?}");
    }

    #[test]
    fn test_unknown_key_bullet_is_context() {
        let lines = brief_card(&[card(1) + "\n- Files: src/a.py\n"], 1, &["claude"]);
        assert_eq!(
            lines[lines.len() - 2..],
            ["", "- Files: src/a.py"],
            "{lines:?}"
        );
    }
}
