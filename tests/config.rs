//! Configuration: presets, the user and repository files, validation, the settings this
//! version refuses, and the snapshot `start` takes.
//!
//! The Rust form of the Python contract suite's `contract_config` (converted from Python's
//! `test_config` and `test_config_snapshot`). The effective configuration is read from the run's
//! `config.json` snapshot after `start`, from `route`, or from `check`'s problem lines.

#[macro_use]
mod common;

use std::collections::BTreeSet;
use std::fs;

use common::json::Json;
use common::supervisor::{FakeSupervisor, Options, card};
use common::{Out, set_of};

const NOT_SUPPORTED: &str = "not supported in this version: ";

/// `check`'s "not supported in this version" lines, in order.
fn refusals(result: &Out) -> Vec<String> {
    result
        .stdout_text()
        .lines()
        .filter(|l| l.contains(NOT_SUPPORTED))
        .map(str::to_string)
        .collect()
}

fn world() -> FakeSupervisor {
    FakeSupervisor::new(&[card(1)])
}

fn world_with(config: &str) -> FakeSupervisor {
    FakeSupervisor::with_options(
        &[card(1)],
        Options {
            config,
            ..Options::default()
        },
    )
}

fn repo_config(w: &FakeSupervisor, text: &str) {
    fs::write(w.repo.join(".leadhand.toml"), text).unwrap();
}

fn check(w: &FakeSupervisor) -> Out {
    w.cli(&os_args!["check", w.deck_path, "--repo", w.repo])
}

/// The effective configuration, as the run's snapshot records it.
fn snapshot(w: &mut FakeSupervisor) -> Json {
    let run_dir = w.start();
    common::read_json(&run_dir.join("config.json"))
}

#[track_caller]
fn route(w: &FakeSupervisor, role: &str, flags: &[&str]) -> (String, Vec<String>) {
    let mut args = os_args!["route", w.deck_path, "1", role, "--repo", w.repo].to_vec();
    args.extend(flags.iter().map(std::ffi::OsStr::new));
    let r = w.cli(&args).ok();
    let stdout = r.stdout_text();
    let lines: Vec<String> = stdout.lines().map(str::to_string).collect();
    assert!(!lines.is_empty(), "{r}");
    (lines[0].clone(), lines[1..].to_vec())
}

/// An object's keys, less `prefer` (`set(d) - {"prefer"}`).
fn keys_but_prefer(v: &Json) -> BTreeSet<String> {
    v.keys().into_iter().filter(|k| k != "prefer").collect()
}

mod config {
    use super::*;
    use crate::common::json;

    #[test]
    fn test_default_preset_matches_spec() {
        // Covers C4.
        let mut w = world();
        assert_eq!(
            route(&w, "implementer", &["--installed", "claude,codex"]),
            (
                "codex gpt-6.1-terra high".to_string(),
                vec!["vendor codex: prefer".to_string()]
            )
        );
        let snap = snapshot(&mut w);
        let roles = &snap["roles"];
        assert_json_eq!(
            roles["implementer"]["codex"],
            Json::from("gpt-6.1-terra high")
        );
        assert_json_eq!(roles["implementer"]["prefer"], Json::from("codex"));
        assert_json_eq!(roles["reviewer"]["prefer"], Json::from("other"));
        assert_json_eq!(roles["dealer"]["prefer"], Json::from("claude"));
        assert_eq!(keys_but_prefer(&roles["dealer"]), set_of(&["claude"]));
        assert_json_eq!(snap["run"]["branch_prefix"], Json::from("leadhand/"));
        assert_json_eq!(snap["after_run"]["step"], Json::from("none"));
        assert_json_eq!(snap["capacity"]["codex_pause_percent"], Json::from(95));
        assert_eq!(snap["codex"]["network"].as_bool(), Some(false));
        assert_json_eq!(
            snap["limits"]["implementer"],
            json::parse(br#"{"steps": 150, "minutes": 60}"#)
        );
        assert_json_eq!(snap["prices"]["opus"]["cache_write_1h"], Json::from(8));
        assert_json_eq!(snap["prices"]["haiku"]["cache_read"], json::parse(b"0.01"));
        assert_json_eq!(snap["notify"]["command"], Json::from(""));
    }

    #[test]
    fn test_repo_file_overrides_user_file_key_by_key() {
        let mut w =
            world_with("profile = \"lean\"\n[run]\nbranch_prefix = \"u/\"\ntest_first = \"off\"\n");
        repo_config(
            &w,
            "[run]\nbranch_prefix = \"r/\"\n[roles.implementer]\nclaude = \"opus low\"\n",
        );
        let snap = snapshot(&mut w);
        assert_json_eq!(snap["run"]["branch_prefix"], Json::from("r/"));
        assert_json_eq!(snap["run"]["test_first"], Json::from("off"));
        assert_json_eq!(snap["profile"], Json::from("lean"));
        assert_json_eq!(
            snap["roles"]["implementer"]["claude"],
            Json::from("opus low")
        );
        assert_eq!(
            snap["roles"]["implementer"]["codex"]
                .as_str()
                .unwrap()
                .split(' ')
                .next(),
            Some("gpt-6.1-terra")
        );
    }

    #[test]
    fn test_preset_key_selects_claude_only() {
        let mut w = world();
        repo_config(&w, "preset = \"claude-only\"\n");
        let snap = snapshot(&mut w);
        assert_eq!(
            keys_but_prefer(&snap["roles"]["implementer"]),
            set_of(&["claude"])
        );
        assert_json_eq!(
            snap["roles"]["final_reviewer"]["claude"],
            Json::from("fable medium")
        );
        assert_json_eq!(snap["roles"]["implementer"]["prefer"], Json::from("claude"));
    }

    #[test]
    fn test_invalid_effort_raises_config_error_naming_key() {
        let w = world();
        repo_config(&w, "[roles.implementer]\nclaude = \"sonnet extreme\"\n");
        let r = check(&w);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stdout_text().starts_with("error: "), "{r}");
        assert!(r.stdout_text().contains("roles.implementer.claude"), "{r}");
    }

    #[test]
    fn test_unknown_top_level_key_raises() {
        let w = world();
        repo_config(&w, "bogus = 1\n");
        let r = check(&w);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stdout_text().starts_with("error: "), "{r}");
        assert!(r.stdout_text().contains("bogus"), "{r}");
    }

    #[test]
    fn test_docs_dir_expands_repo_and_home() {
        // The default docs_dir, ~/.leadhand/{repo}: the run directory lands under it.
        let mut w = world();
        let home = w.tmp.join("home dir");
        fs::create_dir(&home).unwrap();
        w.env.set("HOME", &home);
        fs::write(&w.config_file, "").unwrap();
        let run_dir = w.start();
        let docs = home.join(".leadhand").join("my repo");
        assert!(
            fs::canonicalize(&run_dir)
                .unwrap()
                .starts_with(fs::canonicalize(&docs).unwrap()),
            "{}",
            run_dir.display()
        );
    }

    #[test]
    fn test_after_run_step_validation() {
        let w = world();
        repo_config(&w, "[after_run]\nstep = \"none\"\n");
        let r = check(&w);
        assert_eq!(r.code, 0, "{r}");
        assert!(!r.stdout_text().contains("after_run.step"), "{r}");
        for ok in ["pr", "skill:x", "command:x"] {
            // accepted, then refused by this version under its own value
            repo_config(&w, &format!("[after_run]\nstep = \"{ok}\"\n"));
            let r = check(&w);
            assert_eq!(
                refusals(&r),
                [format!("error: {NOT_SUPPORTED}after_run.step = {ok}")],
                "step {ok}: {r}"
            );
        }
        repo_config(&w, "[after_run]\nstep = \"merge\"\n");
        let r = check(&w);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stdout_text().starts_with("error: "), "{r}");
        assert!(r.stdout_text().contains("after_run.step"), "{r}");
        assert_eq!(refusals(&r), Vec::<String>::new(), "{r}");
    }

    #[test]
    fn test_unsupported_settings_lists_each_rejection() {
        let w = world();
        let r = check(&w);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(refusals(&r), Vec::<String>::new(), "{r}");
        repo_config(
            &w,
            "[after_run]\nstep = \"pr\"\non_incomplete = \"draft-pr\"\n[run]\ncost_limit_usd = 5\n",
        );
        let r = check(&w);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(
            refusals(&r),
            [
                "error: not supported in this version: after_run.step = pr",
                "error: not supported in this version: after_run.on_incomplete = draft-pr",
                "error: not supported in this version: run.cost_limit_usd = 5",
            ],
            "{r}"
        );
    }

    #[test]
    fn test_step_limits_claude_workers_cannot_honour_are_rejected() {
        // Ruling R52: a Claude worker's turn cap is its agent file's maxTurns.
        let w = world();
        for preset in ["claude-codex", "claude-only"] {
            repo_config(&w, &format!("preset = \"{preset}\"\n"));
            let r = check(&w);
            assert_eq!(r.code, 0, "preset {preset}: {r}");
            assert_eq!(refusals(&r), Vec::<String>::new(), "preset {preset}: {r}");
        }
        repo_config(
            &w,
            "[limits]\nreviewer = { steps = 80, minutes = 20 }\nscout = { steps = 30, minutes = 5 }\n",
        );
        let r = check(&w);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(
            refusals(&r),
            [
                "error: not supported in this version: limits.reviewer.steps = 80 (Claude workers use the agent file's maxTurns = 60)"
            ],
            "{r}"
        );
    }

    #[test]
    fn test_leadhand_config_env_names_the_user_file() {
        let mut w = world();
        let other = w.tmp.join("other user config.toml");
        fs::write(&other, "profile = \"thorough\"\n").unwrap();
        w.env.set("LEADHAND_CONFIG", &other);
        let (_, reasons) = route(&w, "implementer", &["--installed", "claude,codex"]);
        assert!(
            reasons.iter().any(|r| r == "profile thorough: +1"),
            "{reasons:?}"
        );
    }
}

mod snapshot {
    //! The run's configuration is snapshotted at `start` (final review I5):
    //! `<run>/config.json` holds the effective configuration, and the run reads only that
    //! afterwards.
    use super::*;
    use crate::common::json;
    use crate::common::supervisor::{CardOpts, Step, card_with};

    #[test]
    fn test_start_writes_the_snapshot_and_the_engine_reads_only_it() {
        let mut w = world_with("[codex]\nnetwork = false\nwritable_dirs = []\n");
        let run_dir = w.start();
        let snap = json::parse(&fs::read(run_dir.join("config.json")).unwrap());
        assert_json_eq!(
            snap["codex"],
            json::parse(br#"{"network": false, "writable_dirs": []}"#)
        );
        assert!(snap.get("limits").is_some(), "{snap}");
        assert!(snap.get("roles").is_some(), "{snap}");
        assert!(snap.get("prices").is_some(), "{snap}");
        let text = fs::read_to_string(&w.config_file).unwrap();
        fs::write(
            &w.config_file,
            text + "\n[run]\ncard_review = \"nonsense\"\n",
        )
        .unwrap();
        assert_json_eq!(w.next()["action"], Json::from("dispatch")); // the broken live file is never read
    }

    #[test]
    fn test_worker_editing_the_repo_config_changes_nothing() {
        // The reference ran in place, so the worker's .leadhand.toml was the repository's. In
        // a worktree run the worker writes the repository's file by its absolute path.
        let mut w = FakeSupervisor::with_options(
            &[card_with(
                1,
                CardOpts {
                    route: Some("codex gpt-6-luna low"),
                    ..CardOpts::default()
                },
            )],
            Options {
                installed: &["claude", "codex"],
                ..Options::default()
            },
        );
        w.start();
        let repo_config = w.repo.join(".leadhand.toml");
        w.script(
            1,
            "implement",
            [Step {
                commit: Some(false),
                files: Some(vec![
                    ("src/card1/a.txt".to_string(), "x\n".to_string()),
                    (
                        repo_config.to_str().unwrap().to_string(),
                        "[codex]\nnetwork = true\nwritable_dirs = [\"/\"]\n".to_string(),
                    ),
                ]),
                ..Step::default()
            }],
        );
        w.script(
            1,
            "review",
            [Step {
                verdict: Some("defects".to_string()),
                ..Step::default()
            }],
        );
        w.script(
            1,
            "fix",
            [Step {
                commit: Some(false),
                ..Step::default()
            }],
        );
        for _ in 0..12 {
            let a = w.next();
            if a["action"] != "dispatch" {
                break;
            }
            w.act(&a);
            if a["kind"] == "fix" {
                break;
            }
        }
        assert!(repo_config.exists());
        let runs = common::read_jsonl(&w.tmp.join("codex log.jsonl"));
        let argvs: Vec<&[Json]> = runs
            .iter()
            .filter_map(|r| r.get("argv").and_then(Json::as_array))
            .filter(|argv| !argv.is_empty())
            .collect();
        assert!(argvs.len() >= 2, "{runs:?}");
        for argv in argvs {
            assert!(
                argv.iter()
                    .any(|x| x == "sandbox_workspace_write.network_access=false"),
                "{argv:?}"
            );
            let adds: Vec<&Json> = argv
                .windows(2)
                .filter(|pair| pair[0] == "--add-dir")
                .map(|pair| &pair[1])
                .collect();
            assert!(!adds.iter().any(|x| **x == "/"), "{adds:?}");
        }
    }
}
