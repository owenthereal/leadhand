//! Static checks of the plugin files: the agents, the supervisor and the run skill. Static:
//! these read only plugin files and never run LEADHAND_BIN. The run skill's relation to the
//! binary (its commands, flags and record kinds) is checked against `<bin> <name> --help` in
//! `tests/cli.rs` (`cli::plugin_commands`).
//!
//! The Rust form of `tests/contract/contract_plugin_files.py`.

#[macro_use]
mod common;

use std::collections::BTreeMap;

/// The contract's names: the actions `next` prints, the kinds `record` takes, the end-of-run
/// gates.
const ACTIONS: [&str; 6] = [
    "dispatch",
    "wait",
    "check_diff",
    "answer",
    "stop_worker",
    "done",
];
const RECORD_KINDS: [&str; 5] = ["started", "result", "ruling", "park", "diff-check"];
const GATES: [&str; 4] = ["final_review", "evidence", "security", "verify"];

fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

mod agent_files {
    use super::*;
    use crate::common::plugin::{agents, contract, read, split};

    const TOOLS_READ_BASH: &str = "Read, Grep, Glob, Bash";
    const CARRY_CONTRACT: [&str; 3] = ["implementer", "verifier", "scout"];

    fn expected() -> BTreeMap<&'static str, BTreeMap<String, String>> {
        BTreeMap::from([
            (
                "implementer",
                fields(&[
                    ("name", "implementer"),
                    ("model", "sonnet"),
                    ("effort", "medium"),
                    ("maxTurns", "150"),
                    ("autoCompactWindow", "200000"),
                    ("experimental", "{cacheTtl: 5m}"),
                    ("tools", "Read, Edit, Write, Bash, Grep, Glob"),
                ]),
            ),
            (
                "reviewer",
                fields(&[
                    ("name", "reviewer"),
                    ("model", "sonnet"),
                    ("effort", "medium"),
                    ("maxTurns", "60"),
                    ("tools", TOOLS_READ_BASH),
                ]),
            ),
            (
                "verifier",
                fields(&[
                    ("name", "verifier"),
                    ("model", "sonnet"),
                    ("effort", "medium"),
                    ("maxTurns", "40"),
                    ("omitClaudeMd", "true"),
                    ("tools", TOOLS_READ_BASH),
                ]),
            ),
            (
                "scout",
                fields(&[
                    ("name", "scout"),
                    ("model", "haiku"),
                    ("effort", "low"),
                    ("maxTurns", "30"),
                    ("omitClaudeMd", "true"),
                    ("tools", TOOLS_READ_BASH),
                ]),
            ),
            (
                "codex",
                fields(&[
                    ("name", "codex"),
                    ("model", "haiku"),
                    ("effort", "low"),
                    ("maxTurns", "12"),
                    ("tools", "Bash"),
                ]),
            ),
        ])
    }

    fn body(stem: &str) -> String {
        split(&agents().join(format!("{stem}.md"))).1
    }

    #[test]
    fn test_agent_frontmatter_matches_table() {
        // The supervisor is not a worker; its frontmatter is checked in supervisor_files.
        let expected = expected();
        let mut stems: Vec<String> = std::fs::read_dir(agents())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        stems.sort();
        let mut want: Vec<String> = expected.keys().map(|s| s.to_string()).collect();
        want.push("supervisor".to_string());
        want.sort();
        assert_eq!(stems, want);
        for (stem, want) in &expected {
            let (mut fields, _) = split(&agents().join(format!("{stem}.md")));
            let desc = fields.remove("description").unwrap_or_default();
            assert!(desc.starts_with("leadhand worker:"), "{stem}");
            assert!(desc.contains("leadhand supervisor only"), "{stem}");
            assert_eq!(&fields, want, "{stem}");
        }
    }

    #[test]
    fn test_contract_carriers_contain_contract_verbatim() {
        let contract = read(&contract()).trim().to_string();
        assert!(!contract.is_empty());
        for stem in CARRY_CONTRACT {
            assert!(body(stem).contains(&contract), "{stem}");
        }
        for stem in ["reviewer", "codex"] {
            assert!(!body(stem).contains(&contract), "{stem}");
        }
    }

    #[test]
    fn test_reviewer_rules() {
        let body = body("reviewer");
        for phrase in [
            "Done when",
            "allowed paths",
            "Never edit",
            "Verdict: clean",
            "Report: <path of the report file>",
            "implementer's report",
            "`## Tests`",
            "throwaway worktree",
            "never in the workspace",
        ] {
            assert!(body.contains(phrase), "{phrase:?} not in the reviewer");
        }
    }

    #[test]
    fn test_verifier_evidence_rule_follows_the_spec() {
        // Ruling R46: only a failing assertion on the base counts as failing-before.
        let body = body("verifier");
        assert!(body.contains("fails on the base by an assertion"));
        for phrase in ["compile", "import", "missing module", "did not run"] {
            assert!(body.contains(phrase), "{phrase:?} not in the verifier");
        }
        assert!(!body.contains("fails (or is absent) on the base"));
    }

    #[test]
    fn test_codex_proxy_runs_one_command() {
        let body = body("codex");
        for phrase in [
            "exactly the one command",
            "verbatim",
            "Never run another command",
            "If the command prints `still running`, run the exact\nsame command again.",
        ] {
            assert!(body.contains(phrase), "{phrase:?} not in the codex proxy");
        }
    }
}

mod supervisor_files {
    use super::*;
    use crate::common::plugin::{COMMANDS, cli_calls, has_word, section, skill, split, supervisor};

    #[test]
    fn test_supervisor_frontmatter() {
        let (fields, body) = split(&supervisor());
        assert_eq!(
            fields,
            super::fields(&[
                ("name", "supervisor"),
                (
                    "description",
                    "leadhand supervisor: runs one deck. Start it with the leadhand launch command, never by hand."
                ),
                ("model", "opus"),
                ("effort", "medium"),
                ("skills", "[leadhand:run]"),
            ])
        );
        // P2: the preload is not relied on; the body invokes the skill first.
        assert!(body.contains("First invoke the Skill tool with `leadhand:run`, then follow it."));
        assert!(body.contains("Follow the run skill."));
        let (skill_fields, _) = split(&skill());
        assert_eq!(skill_fields.get("name").map(String::as_str), Some("run"));
        assert!(
            skill_fields
                .get("description")
                .is_some_and(|d| !d.is_empty())
        );
    }

    #[test]
    fn test_run_skill_covers_every_action() {
        let (_, body) = split(&skill());
        for action in ACTIONS {
            assert!(body.contains(&format!("`{action}`")), "{action}");
        }
        for kind in RECORD_KINDS {
            assert!(body.contains(&format!("record <run_dir> {kind}")), "{kind}");
        }
        // The required parts, in the brief's order.
        let order: Vec<usize> = ["## Preflight", "## The loop", "## Hygiene", "## Steering"]
            .iter()
            .map(|h| body.find(h).unwrap_or_else(|| panic!("no {h:?}")))
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }

    #[test]
    fn test_run_skill_cli_calls_are_plain_commands() {
        // The file-only half of test_run_skill_commands_match_cli; the binary's half is
        // cli::plugin_commands.
        let (_, body) = split(&skill());
        let calls = cli_calls(&body);
        assert!(!calls.is_empty());
        for (name, args) in &calls {
            assert!(COMMANDS.contains(&name.as_str()), "<cli> {name}");
            if name == "record" {
                let kind = args.split_whitespace().nth(1).unwrap();
                assert!(RECORD_KINDS.contains(&kind), "<cli> {name}{args}");
            }
        }
        // P5/R28/R38: CLI calls are plain commands, no exceptions (the sandbox exclusion
        // matched no heredoc in the first live run).
        for (name, args) in &calls {
            let plain = drop_placeholders(args); // drop placeholders such as <run_dir>
            assert!(
                !(plain.contains(['|', ';', '&', '<', '>', '$']) || has_word(&plain, "cd")),
                "<cli> {name}{args}"
            );
        }
        assert!(!body.contains("<<"));
        // R38: a worker's reply goes through the action's reply file.
        let results: Vec<&String> = calls
            .iter()
            .filter(|(name, args)| {
                name == "record" && args.split_whitespace().nth(1) == Some("result")
            })
            .map(|(_, args)| args)
            .collect();
        assert!(
            results
                .iter()
                .any(|a| a.contains("--reply-file '<reply_file>'")),
            "{results:?}"
        );
        assert!(
            !results.iter().any(|a| a.contains("--reply -")),
            "{results:?}"
        );
    }

    /// `re.sub(r"<[a-z_ ]+>", "", text)`.
    fn drop_placeholders(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let len = after
                .find(|c: char| !(c.is_ascii_lowercase() || c == '_' || c == ' '))
                .unwrap_or(after.len());
            if len > 0 && after[len..].starts_with('>') {
                out.push_str(&rest[..i]);
                rest = &after[len + 1..];
            } else {
                out.push_str(&rest[..=i]);
                rest = after;
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn test_run_skill_covers_run_level_dispatches() {
        let (_, body) = split(&skill());
        let text = section(&body, "### Run-level dispatches");
        for name in GATES
            .iter()
            .chain(&["final_fix", "card", "gate", "wave", "resume"])
        {
            assert!(text.contains(&format!("`{name}`")), "{name}");
        }
        assert!(text.contains("no `--card`"));
        let dispatch = section(&body, "### `dispatch` without `resume`");
        assert!(dispatch.contains("`card <card> <kind> (attempt <attempt>)`"));
        assert!(dispatch.contains("`run <kind> (wave <wave>)`"));
        // `done` prints the report the action names, not a guessed path.
        let done = section(&body, "### `done`");
        assert!(done.contains("the action's `report`"));
        assert!(!done.contains("report.md"));
    }

    #[test]
    fn test_run_skill_steers_with_plain_commands() {
        // Converted from test_recovery.PauseCancel.test_run_skill_steers_with_plain_commands.
        let (_, body) = split(&skill());
        let steering = &body[body.find("## Steering").unwrap()..];
        assert!(steering.contains(
            "to pause, run `<cli> pause <run_dir>`; to cancel, `<cli> cancel <run_dir>`"
        ));
        assert!(steering.contains("`<cli> unpark <run_dir> --card N --note '…'`"));
    }

    #[test]
    fn test_run_skill_records_started_before_waiting() {
        let (_, body) = split(&skill());
        for heading in [
            "### `dispatch` without `resume`",
            "### `dispatch` with `resume`",
        ] {
            let text = section(&body, heading);
            let started = text
                .find("<cli> record <run_dir> started --dispatch <dispatch_id> --worker <agent id>")
                .unwrap_or_else(|| panic!("no `record started` under {heading}"));
            let timer = text
                .find("<cli> timer <run_dir> --dispatch <dispatch_id>")
                .unwrap_or_else(|| panic!("no `timer` under {heading}"));
            assert!(started < timer, "{heading}");
        }
        // P8: waiting never means sleeping (`(?m)^\s*`?sleep `).
        assert!(!body.lines().any(|line| {
            let line = line.trim_start();
            line.strip_prefix('`').unwrap_or(line).starts_with("sleep ")
        }));
    }
}
