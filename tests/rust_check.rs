//! `check` in Rust: in-place runs refused and the repository taken from the main working tree
//! (deliberate change 1), and the conventions for the commands a person types (`--json`, the
//! fix command alone on the last line). Rust-only.
//!
//! `check` keeps its contract output: one problem per line on stdout. The fix command, when
//! one command fixes every error, follows on stderr, alone.
//!
//! The Rust form of `tests/contract/contract_rust_check.py`.

#[macro_use]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::json::{self, Json};
use common::supervisor::FakeSupervisor;
use common::{Out, key_set, set_of};

const NOT_SUPPORTED: &str = "not supported in this version: ";

/// `check`'s problem lines as (level, message).
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

fn refusals(result: &Out) -> Vec<String> {
    result
        .stdout_text()
        .lines()
        .filter(|l| l.contains(NOT_SUPPORTED))
        .map(str::to_string)
        .collect()
}

fn check(w: &FakeSupervisor, flags: &[&str]) -> Out {
    check_in(w, &w.repo, flags)
}

fn check_in(w: &FakeSupervisor, repo: &Path, flags: &[&str]) -> Out {
    let mut args = vec![
        std::ffi::OsStr::new("check"),
        w.deck_path.as_os_str(),
        std::ffi::OsStr::new("--repo"),
        repo.as_os_str(),
    ];
    args.extend(flags.iter().map(std::ffi::OsStr::new));
    w.cli(&args)
}

/// `path` with every symlink in its existing part resolved (Python's non-strict `resolve`).
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Python's `str()` of a JSON value read back (`None`, an int, a string).
fn py_str(v: &Json) -> String {
    match v {
        Json::Null => "None".to_string(),
        Json::Num(n) => n.clone(),
        Json::Str(_) => v.as_str().unwrap().to_string(),
        other => other.to_string(),
    }
}

mod check {
    use super::*;
    use crate::common::git::{commit, run_git};
    use crate::common::supervisor::{CardOpts, Options, card, card_with};

    #[test]
    fn test_check_json_lists_problems() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::with_options(
            &[
                card(1),
                card_with(
                    2,
                    CardOpts {
                        depends: &[9],
                        ..CardOpts::default()
                    },
                ),
            ],
            Options {
                config: "[notify]\ncommand = \"say done\"\n",
                ..Options::default()
            },
        );
        let text = check(&w, &[]);
        assert_eq!(text.code, 1, "{text}");
        let r = check(&w, &["--json"]);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stderr_text(), "", "{r}");
        let report = r.json();
        assert_eq!(
            key_set(&report),
            set_of(&["ok", "cards", "problems", "fix"]),
            "{r}"
        );
        assert_json_eq!(
            Json::from(vec![
                report["ok"].clone(),
                report["cards"].clone(),
                report["fix"].clone()
            ]),
            json::parse(b"[false, 2, null]"),
            "{r}"
        );
        // The same problems as the text output, in the same order, each with its card.
        let listed: Vec<(String, String)> = report["problems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["level"].as_str().unwrap().to_string(),
                    p["message"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(listed, problems(&text), "{r}");
        let problems_of = report["problems"].as_array().unwrap();
        let unknown = json::parse(
            br#"{"level": "error", "card": 2, "message": "card 2: depends on unknown card 9"}"#,
        );
        assert!(problems_of.iter().any(|p| json::py_eq(p, &unknown)), "{r}");
        let notify = json::parse(
            br#"{"level": "warning", "card": null, "message": "not supported in this version: notify.command = say done"}"#,
        );
        assert!(problems_of.iter().any(|p| json::py_eq(p, &notify)), "{r}");
        for p in problems_of {
            assert_eq!(key_set(p), set_of(&["level", "card", "message"]), "{p}");
            let message = p["message"].as_str().unwrap();
            if message.starts_with("card ") {
                assert_eq!(
                    message.split(':').next().unwrap(),
                    format!("card {}", py_str(&p["card"])),
                    "{p}"
                );
            }
        }

        let ok = FakeSupervisor::new(&[card_with(
            1,
            CardOpts {
                allowed: "README.md",
                ..CardOpts::default()
            },
        )]);
        let r = check(&ok, &["--json"]);
        assert_eq!(r.code, 0, "{r}");
        assert_json_eq!(
            r.json(),
            json::parse(br#"{"ok": true, "cards": 1, "problems": [], "fix": null}"#),
            "{r}"
        );
        assert_eq!(check(&ok, &[]).stdout_text(), "ok: 1 cards\n");
    }

    #[test]
    fn test_check_json_reports_a_deck_that_does_not_load() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        let deck = fs::read_to_string(&w.deck_path).unwrap();
        fs::write(&w.deck_path, deck.replace("Risk: low", "Risk: severe")).unwrap();
        let text = check(&w, &[]);
        assert_eq!(text.code, 1, "{text}");
        let stdout = text.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        let line = lines[0];
        let r = check(&w, &["--json"]);
        assert_eq!(r.code, 1, "{r}");
        let want = Json::object([
            ("ok", Json::from(false)),
            ("cards", Json::Null),
            ("fix", Json::Null),
            (
                "problems",
                Json::from(vec![Json::object([
                    ("level", Json::from("error")),
                    ("card", Json::Null),
                    ("message", Json::from(&line["error: ".len()..])),
                ])]),
            ),
        ]);
        assert_json_eq!(r.json(), want, "{r}");
    }

    #[test]
    fn test_check_refuses_run_worktree_false() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: "[run]\nworktree = false\n",
                ..Options::default()
            },
        );
        let r = check(&w, &[]);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(
            refusals(&r),
            ["error: not supported in this version: run.worktree = false"],
            "{r}"
        );
        assert!(!r.stdout_text().contains("ok: "), "{r}");
        // No single command fixes a setting: nothing follows the error.
        assert_eq!(r.stderr_text(), "", "{r}");
    }

    #[test]
    fn test_check_sparse_refusal_ends_with_fix_command() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        run_git(
            &w.repo,
            &["sparse-checkout", "set", "--no-cone", "/README.md"],
        );
        let r = check(&w, &[]);
        assert_eq!(r.code, 1, "{r}");
        let levels: Vec<String> = problems(&r)
            .into_iter()
            .filter(|(l, _)| l == "error")
            .map(|(l, _)| l)
            .collect();
        assert_eq!(levels, ["error"], "{r}");
        let refused = refusals(&r);
        assert_eq!(refused.len(), 1, "{r}");
        let repo = resolve(&w.repo);
        assert!(
            refused[0].starts_with(&format!(
                "error: {NOT_SUPPORTED}{} is a sparse checkout",
                repo.display()
            )),
            "{r}"
        );
        // The last line is the one command that turns the sparse checkout off, alone: it
        // names the repository, quoted (its name has a space), so it works from any directory.
        let stderr = r.stderr_text();
        let lines: Vec<&str> = stderr.lines().collect();
        let want = format!(
            "git -C {} sparse-checkout disable",
            common::shlex_quote(repo.to_str().unwrap())
        );
        assert_eq!(lines, [want.as_str()], "{r}");
        let argv = common::shlex_split(lines[lines.len() - 1]).unwrap();
        let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
        let done = common::run_argv(&argv, &w.env, &w.tmp, None, common::TIMEOUT);
        assert_eq!(done.code, 0, "{done}");
        let r = check(&w, &[]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(
            (refusals(&r), r.stderr_text()),
            (Vec::<String>::new(), String::new()),
            "{r}"
        );
    }

    #[test]
    fn test_check_cycle_error_has_no_fix_line() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[
            card_with(
                1,
                CardOpts {
                    depends: &[2],
                    allowed: "README.md",
                    ..CardOpts::default()
                },
            ),
            card_with(
                2,
                CardOpts {
                    depends: &[1],
                    allowed: "README.md",
                    ..CardOpts::default()
                },
            ),
        ]);
        let r = check(&w, &[]);
        assert_eq!(r.code, 1, "{r}");
        // No single command fixes a cycle: the error is the last line, and nothing follows it.
        assert_eq!(
            r.stdout_text().lines().collect::<Vec<_>>(),
            ["error: dependency cycle among cards 1, 2"],
            "{r}"
        );
        assert_eq!(r.stderr_text(), "", "{r}");
        assert!(check(&w, &["--json"]).json()["fix"].is_null());
    }

    #[test]
    fn test_check_resolves_repo_from_linked_worktree() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card_with(
            1,
            CardOpts {
                allowed: "src/",
                ..CardOpts::default()
            },
        )]);
        commit(&w.repo, &[("src/a.txt", "a\n")], "src");
        let linked = w.tmp.join("elsewhere").join("linked wt");
        run_git(
            &w.repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        // Config comes from the main working tree, never from a worktree's copy; {repo} in
        // docs_dir is the main working tree's folder name, and a relative docs_dir is resolved
        // against it.
        fs::write(w.repo.join(".leadhand.toml"), "[run]\ncost_limit_usd = 5\n").unwrap();
        fs::write(
            linked.join(".leadhand.toml"),
            "[after_run]\nstep = \"pr\"\n",
        )
        .unwrap();
        fs::write(&w.config_file, "docs_dir = \"notes/{repo}\"\n").unwrap();
        let docs = resolve(&w.repo.join("notes").join("my repo"));
        let expected = [
            format!(
                "error: {NOT_SUPPORTED}docs_dir inside the repository ({})",
                docs.display()
            ),
            format!("error: {NOT_SUPPORTED}run.cost_limit_usd = 5"),
        ];
        for repo in [
            w.repo.clone(),
            w.repo.join("src"),
            linked.clone(),
            linked.join("src"),
        ] {
            let r = check_in(&w, &repo, &[]);
            assert_eq!(r.code, 1, "{}: {r}", repo.display());
            assert_eq!(refusals(&r), expected, "{}: {r}", repo.display());
            let other: Vec<String> = problems(&r)
                .into_iter()
                .filter(|(l, m)| l == "error" && !m.contains(NOT_SUPPORTED))
                .map(|(_, m)| m)
                .collect();
            assert_eq!(other, Vec::<String>::new(), "{}: {r}", repo.display());
        }
    }
}
