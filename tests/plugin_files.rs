//! Static checks of the plugin files: the agents, the supervisor and the run skill; and the
//! cut-over checks, that no tracked file still needs the removed Python reference, that every
//! embedded file is a kept plugin file, and that the manifests agree on the version. Static:
//! these read files (the cut-over checks list them with `git ls-files`) and never run
//! LEADHAND_BIN. The run skill's relation to the binary (its commands, flags and record kinds)
//! is checked against `<bin> <name> --help` in `tests/cli.rs` (`cli::plugin_commands`).
//!
//! The Rust form of the Python contract suite's `contract_plugin_files`.

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

// --------------------------------------------------------------------------
// The cut-over checks: the Python reference is gone, and nothing may still need it.

/// Every tracked file, from `git ls-files -z` in the package root.
fn tracked_files() -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(common::plugin::root())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("running git ls-files");
    assert!(
        out.status.success(),
        "git ls-files: {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    // An empty listing would pass the check below with nothing checked.
    assert!(files.len() > 100, "{} tracked files", files.len());
    files
}

/// Paths of the removed Python reference, and its interpreter, matched anywhere in a line.
const REMOVED_PATHS: [&str; 4] = [
    "scripts/leadhand",
    "scripts/lib/",
    "tests/contract/",
    "python3",
];

/// The basenames of the Python files the cut-over removed, matched as whole words (no letter,
/// digit or `_` on either side).
const REMOVED_BASENAMES: [&str; 97] = [
    "__init__.py",
    "briefs.py",
    "check_conversion.py",
    "check_vacuity.py",
    "cli.py",
    "codex_push_probe.py",
    "config.py",
    "contract_briefs.py",
    "contract_cards.py",
    "contract_catalogue.py",
    "contract_cli.py",
    "contract_config.py",
    "contract_deck.py",
    "contract_dispatch.py",
    "contract_finish.py",
    "contract_integration.py",
    "contract_launch.py",
    "contract_layouts.py",
    "contract_plugin_files.py",
    "contract_recovery.py",
    "contract_report.py",
    "contract_route.py",
    "contract_rust_check.py",
    "contract_rust_cli.py",
    "contract_rust_control.py",
    "contract_rust_launch.py",
    "contract_rust_release.py",
    "contract_rust_start.py",
    "contract_rust_status.py",
    "contract_rust_worktree_identity.py",
    "contract_security.py",
    "contract_self.py",
    "contract_smoke.py",
    "contract_status.py",
    "contract_usage.py",
    "contract_worker_env.py",
    "contract_worktree_safety.py",
    "deck.py",
    "dispatch.py",
    "engine.py",
    "fakeworld.py",
    "generate.py",
    "gitfixture.py",
    "gitops.py",
    "harness.py",
    "launch.py",
    "make_fixture.py",
    "probe_launch.py",
    "probe_lifecycle.py",
    "probe_limits.py",
    "probe_usage.py",
    "procs.py",
    "report.py",
    "route.py",
    "run_e2e.py",
    "rundir.py",
    "runworld.py",
    "state.py",
    "status.py",
    "supervisor.py",
    "test_agents.py",
    "test_briefs.py",
    "test_cli.py",
    "test_config_snapshot.py",
    "test_config.py",
    "test_deck.py",
    "test_dispatch.py",
    "test_engine_by_risk.py",
    "test_engine_cards.py",
    "test_engine_codex_e2e.py",
    "test_engine_codex_result.py",
    "test_engine_decisions.py",
    "test_engine_finish.py",
    "test_engine_in_place.py",
    "test_engine_integration.py",
    "test_engine_recovery.py",
    "test_engine_reply_file.py",
    "test_engine_run_branch.py",
    "test_engine_untracked.py",
    "test_final_review_minors.py",
    "test_git_safety.py",
    "test_gitops.py",
    "test_launch.py",
    "test_procs.py",
    "test_recovery.py",
    "test_report.py",
    "test_route.py",
    "test_security.py",
    "test_state.py",
    "test_status.py",
    "test_supervisor_files.py",
    "test_test_hooks.py",
    "test_unsupported_layouts.py",
    "test_usage_models.py",
    "test_usage.py",
    "test_worker_env.py",
    "usage.py",
];

/// The tracked files the no-Python check does not read:
/// - `docs/compatibility.md`, whose history notes name the removed files;
/// - `scripts/assemble-dist.sh`, the release's own script;
/// - this file, for the patterns above;
/// - the live tests' fixture project (`tests/e2e/fixture-deck.md`, `tests/live/fixture.rs`),
///   whose `python3` and `calc/__init__.py` are that project's own toolchain, not leadhand's.
const NO_PYTHON_EXEMPT: [&str; 5] = [
    "docs/compatibility.md",
    "scripts/assemble-dist.sh",
    "tests/plugin_files.rs",
    "tests/e2e/fixture-deck.md",
    "tests/live/fixture.rs",
];

/// Whether `word` appears in `line` with no `[A-Za-z0-9_]` on either side.
fn has_whole_word(line: &str, word: &str) -> bool {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = line.as_bytes();
    line.match_indices(word).any(|(at, _)| {
        let end = at + word.len();
        !(at > 0 && is_word(bytes[at - 1])) && !(end < bytes.len() && is_word(bytes[end]))
    })
}

/// The forbidden strings `line` holds.
fn python_references(line: &str) -> Vec<&'static str> {
    REMOVED_PATHS
        .iter()
        .filter(|p| line.contains(*p))
        .chain(REMOVED_BASENAMES.iter().filter(|b| has_whole_word(line, b)))
        .copied()
        .collect()
}

/// Review Focus 5 (ruling S3-R12): after the cut-over, no tracked file names the removed
/// Python reference (`scripts/leadhand`, `scripts/lib/`, `tests/contract/`, any of its `.py`
/// files) or `python3`, outside the exempt files above. Generic example paths that name no
/// removed file (`path/to/file.py`, `src/a.py`) stay allowed.
#[test]
fn no_python_references_in_shipped_files() {
    assert_eq!(
        python_references("see scripts/lib/leadhand/state.py, then run python3"),
        ["scripts/lib/", "python3", "state.py"]
    );
    assert_eq!(
        python_references("path/to/file.py, src/a.py, mystate.py, state.pyc"),
        Vec::<&str>::new()
    );
    let root = common::plugin::root();
    let mut hits = Vec::new();
    for file in tracked_files() {
        if NO_PYTHON_EXEMPT.contains(&file.as_str()) {
            continue;
        }
        let path = root.join(&file);
        if !path.is_file() {
            continue; // a tracked file deleted in the working tree, or a submodule
        }
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {file}: {e}"));
        for (n, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            for found in python_references(line) {
                hits.push(format!("{file}:{}: {found}", n + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "{} references to the Python reference, which was removed at the cut-over (ruling \
         S3-R12). No tracked file may name scripts/leadhand, scripts/lib/, tests/contract/, \
         python3, or the basename of a removed .py file (REMOVED_BASENAMES in \
         tests/plugin_files.rs; some are common words, such as config.py or state.py, so test \
         data must use another name). Only docs/compatibility.md's history notes, \
         scripts/assemble-dist.sh and the live fixture project are exempt:\n{}",
        hits.len(),
        hits.join("\n")
    );
}

/// The `.rs` files under `dir`, recursively, relative to the package root.
fn rust_files(dir: &str) -> Vec<String> {
    let root = common::plugin::root();
    let mut out = Vec::new();
    let mut todo = vec![root.join(dir)];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                let rel = path.strip_prefix(&root).unwrap();
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// `path` with `.` and `..` folded away, lexically.
fn normalized(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The path argument of each `include_str!` and `include_bytes!` call in Rust source, with
/// its line: the literal, or `Err` with what stands there instead. Comments and literals are
/// not searched.
fn include_paths(src: &str) -> Vec<(usize, Result<String, String>)> {
    let code: Vec<char> = common::rust_src::code_only(src).chars().collect();
    let text: Vec<char> = src.chars().collect();
    let skip_space = |i: &mut usize| {
        while text.get(*i).is_some_and(|c| c.is_whitespace()) {
            *i += 1;
        }
    };
    let names: Vec<Vec<char>> = ["include_str!", "include_bytes!"]
        .iter()
        .map(|name| name.chars().collect())
        .collect();
    let mut out = Vec::new();
    for at in 0..code.len() {
        let Some(name) = names.iter().find(|name| {
            code[at..].starts_with(name)
                && !(at > 0 && (code[at - 1].is_alphanumeric() || code[at - 1] == '_'))
        }) else {
            continue;
        };
        let mut i = at + name.len();
        skip_space(&mut i);
        if text.get(i) != Some(&'(') {
            continue; // the macro's name, not a call
        }
        i += 1;
        skip_space(&mut i);
        let rest: String = text[i..].iter().collect();
        let arg = match rest.strip_prefix('"').map(|r| (r, r.find('"'))) {
            Some((r, Some(end))) if !r[..end].contains('\\') => Ok(r[..end].to_string()),
            Some(_) => Err("not a plain string literal".to_string()),
            None => Err(format!(
                "not a string literal: {:?}",
                rest.chars().take(20).collect::<String>()
            )),
        };
        let line = text[..at].iter().filter(|c| **c == '\n').count() + 1;
        out.push((line, arg));
    }
    out
}

/// Every file compiled into the binary or the tests comes from the plugin's kept folders,
/// `templates/`, `presets/` or `agents/`: nothing embeds a file the cut-over removed.
#[test]
fn no_include_str_outside_kept_paths() {
    let sample = "// include_str!(\"../no.md\")\nlet s = \"include_str!(\\\"x\\\")\";\n\
                  const A: &str = include_str!(\n    \"../templates/a.md\");\n\
                  const B: &[u8] = include_bytes!(P);\n";
    assert_eq!(
        include_paths(sample),
        [
            (3, Ok("../templates/a.md".to_string())),
            (5, Err("not a string literal: \"P);\\n\"".to_string())),
        ]
    );
    let root = common::plugin::root();
    let kept = ["templates", "presets", "agents"].map(|d| root.join(d));
    let (mut found, mut bad) = (0, Vec::new());
    for file in rust_files("src").into_iter().chain(rust_files("tests")) {
        let path = root.join(&file);
        let dir = path.parent().unwrap().to_path_buf();
        for (line, arg) in include_paths(&common::plugin::read(&path)) {
            found += 1;
            match arg {
                Ok(rel)
                    if kept
                        .iter()
                        .any(|k| normalized(&dir.join(&rel)).starts_with(k)) => {}
                Ok(rel) => bad.push(format!(
                    "{file}:{line}: {rel:?} is outside templates/, presets/ and agents/"
                )),
                Err(why) => bad.push(format!("{file}:{line}: {why}")),
            }
        }
    }
    assert_eq!(bad, Vec::<String>::new());
    // The two presets, four worker agents and twelve brief templates the crate embeds: a reader
    // that found fewer would pass with less checked.
    assert!(found >= 18, "{found} embeds found");
}

/// Ruling S3-R13: every manifest is valid JSON at version 0.2.0, and so is the package.
#[test]
fn manifests_are_valid_json_with_version_0_2_0() {
    const VERSION: &str = "0.2.0";
    let root = common::plugin::root();
    let json = |rel: &str| -> serde_json::Value {
        serde_json::from_str(&common::plugin::read(&root.join(rel)))
            .unwrap_or_else(|e| panic!("{rel} is not valid JSON: {e}"))
    };
    for rel in [".claude-plugin/plugin.json", ".codex-plugin/plugin.json"] {
        assert_eq!(json(rel)["version"], VERSION, "{rel}");
    }
    let marketplace = json(".claude-plugin/marketplace.json");
    let entries: Vec<&serde_json::Value> = marketplace["plugins"]
        .as_array()
        .expect("marketplace.json has a plugins array")
        .iter()
        .filter(|p| p["name"] == "leadhand")
        .collect();
    assert_eq!(entries.len(), 1, "marketplace.json's leadhand entries");
    assert_eq!(
        entries[0]["version"], VERSION,
        "marketplace.json's leadhand entry"
    );
    let cargo: toml::Table = toml::from_str(&common::plugin::read(&root.join("Cargo.toml")))
        .expect("Cargo.toml is valid TOML");
    assert_eq!(
        cargo["package"]["version"].as_str(),
        Some(VERSION),
        "Cargo.toml's package version"
    );
}
