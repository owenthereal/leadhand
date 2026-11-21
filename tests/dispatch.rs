//! Dispatcher scenarios: `dispatch`, its detached runner, `stop-dispatch` and the worker
//! folder.
//!
//! The Rust form of `tests/contract/contract_dispatch.py` (converted from
//! tests/test_dispatch.py). Every scenario runs on a run that `start` made (one card, Claude
//! and Codex installed, so the workspace is a worktree and config.json is the run's
//! snapshot), with the Rust fake Codex as Codex (LEADHAND_TEST_CODEX_BIN): the world is
//! `common::dispatch_world::World`. It calls only `dispatch ...`, `stop-dispatch RUN_DIR
//! --dispatch ID` and `dispatch-runner WORKER_DIR`, and reads what they leave: stdout, stderr
//! and exit codes, `workers/<dispatch id>/`, charges.jsonl, attempts.jsonl, the fake's log
//! (its argv, environment, prompt and processes) and Git.
//!
//! Waits poll for a condition with a bound; a check that something does not happen watches
//! for it over a stated window (`holds`). Processes a test starts are identity-checked before
//! any signal, and the world's cleanup kills every process a worker folder records.
//!
//! Tests run in parallel (S3-R19). The tests whose assertions rest on the runner's wall-clock
//! budgets (the 5 s second look at a stale heartbeat, the stop ladder's steps, the foreground
//! wait) hold `common::serial()` for their whole run, so no two of them share the machine's
//! load; each names its group in a comment.

#[macro_use]
mod common;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use common::dispatch_world::{
    CARD_BRANCH, CARD_BRIEF, World, alive, at, implementer_prompt, logged_alive, wait_for,
    wait_for_with, when,
};
use common::json::{self, Json};
use common::supervisor::truthy;
use common::{index, jv, key_set, lines, one, pick, read_json, read_text, realpath, strs};

const ATTEMPT_FIELDS: [&str; 19] = [
    "dispatch",
    "run",
    "card",
    "attempt",
    "role",
    "vendor",
    "model",
    "effort",
    "worker_id",
    "started_at",
    "ended_at",
    "steps",
    "charge_ids",
    "usage",
    "usage_known",
    "cost_estimate",
    "exit_reason",
    "status",
    "blocked_by",
];

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// The values after each `--add-dir`.
fn add_dirs(argv: &[String]) -> Vec<String> {
    argv.iter()
        .enumerate()
        .filter(|(_, a)| *a == "--add-dir")
        .map(|(i, _)| argv[i + 1].clone())
        .collect()
}

/// Python's `a >= b` on two JSON numbers or two strings.
#[track_caller]
fn ge(a: &Json, b: &Json) -> bool {
    match (a.as_f64(), b.as_f64(), a.as_str(), b.as_str()) {
        (Some(x), Some(y), _, _) => x >= y,
        (_, _, Some(x), Some(y)) => x >= y,
        _ => panic!("cannot compare {a} >= {b}"),
    }
}

mod codex_command_line {
    //! The `codex exec` line the runner starts (spec contract item 10), as the fake logs it
    //! (its argv after the program), with its environment and prompt.
    use super::*;
    use common::dispatch_world::Req;
    use common::git::run_git;

    #[track_caller]
    fn argv_of(w: &World, did: &str, req: &Req) -> Vec<String> {
        w.dispatch_with(did, req).ok();
        strs(&w.records("start").last().expect("a start record")["argv"])
    }

    #[test]
    fn test_argv_passes_network_false_explicitly() {
        // Covers P6.
        let mut w = World::new();
        w.scenario("ready", &[]);
        let argv = argv_of(&w, "d1", &Req::default());
        let i = index(&argv, "sandbox_workspace_write.network_access=false");
        assert_eq!(argv[i - 1], "-c");
        assert!(
            !argv.contains(&"sandbox_workspace_write.network_access=true".to_string()),
            "{argv:?}"
        );
    }

    #[test]
    fn test_argv_network_true_when_enabled() {
        let mut w = World::new();
        w.set_config(|c| *at(c, &["codex", "network"]) = true.into());
        w.scenario("ready", &[]);
        let argv = argv_of(&w, "d1", &Req::default());
        assert!(
            argv.contains(&"sandbox_workspace_write.network_access=true".to_string()),
            "{argv:?}"
        );
        assert!(
            !argv.contains(&"sandbox_workspace_write.network_access=false".to_string()),
            "{argv:?}"
        );
    }

    #[test]
    fn test_argv_workspace_write_with_git_common_dir_and_effort() {
        let mut w = World::new();
        let extra = w.tmp.join("extra dir");
        w.set_config(|c| *at(c, &["codex", "writable_dirs"]) = Json::from(&[s(&extra)][..]));
        w.scenario("ready", &[]);
        let argv = argv_of(
            &w,
            "d1",
            &Req {
                effort: "high",
                ..Req::default()
            },
        );
        let common = w.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
        // The program itself (argv[0]) is LEADHAND_TEST_CODEX_BIN: the fake is what ran.
        assert_eq!(
            argv[..17],
            [
                "exec",
                "-m",
                "gpt-6-luna",
                "-c",
                "model_reasoning_effort=\"high\"",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_workspace_write.network_access=false",
                "-C",
                s(&w.ws),
                "-s",
                "workspace-write",
                "--add-dir",
                &common,
                "--add-dir",
                s(&extra),
            ]
        );
        let last_message = w.wd("d1").join("last-message.md");
        assert_eq!(
            argv[argv.len() - 4..],
            ["--json", "-o", s(&last_message), "-"]
        );
        let compact: Vec<&String> = argv.iter().filter(|a| a.contains("compact")).collect();
        assert!(compact.is_empty(), "{compact:?}");
    }

    #[test]
    fn test_argv_scout_is_read_only() {
        let mut w = World::new();
        w.scenario("ready", &[]);
        let argv = argv_of(
            &w,
            "d1",
            &Req {
                role: "scout",
                ..Req::default()
            },
        );
        assert_eq!(argv[index(&argv, "-s") + 1], "read-only");
        assert!(!argv.contains(&"workspace-write".to_string()), "{argv:?}");
    }

    #[test]
    fn test_argv_sets_blocking_env_through_shell_policy() {
        let mut w = World::new();
        w.scenario("ready", &[]);
        w.env
            .set("GIT_CONFIG_COUNT", "1")
            .set("GIT_CONFIG_KEY_0", "user.name")
            .set("GIT_CONFIG_VALUE_0", "Some One");
        w.dispatch("d1").ok();
        let start = w.records("start").remove(0);
        let (argv, policy) = (strs(&start["argv"]), &start["policy_set"]);
        assert_eq!(policy["GIT_SSH_COMMAND"], "false");
        assert_eq!(policy["GIT_TERMINAL_PROMPT"], "0");
        assert_json_eq!(policy["GH_CONFIG_DIR"], start["env"]["GH_CONFIG_DIR"]);
        // The parent's GIT_CONFIG_* entries are kept in order, then credential.helper is reset.
        assert_eq!(policy["GIT_CONFIG_COUNT"], "2");
        assert_eq!(policy["GIT_CONFIG_KEY_0"], "user.name");
        let count: usize = policy["GIT_CONFIG_COUNT"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_json_eq!(
            Json::from(vec![
                policy[format!("GIT_CONFIG_KEY_{}", count - 1).as_str()].clone(),
                policy[format!("GIT_CONFIG_VALUE_{}", count - 1).as_str()].clone(),
            ]),
            jv(r#"["credential.helper", ""]"#)
        );
        for (name, value) in json::dict(policy.as_object().unwrap()) {
            let name = String::from_utf8_lossy(name);
            let i = index(
                &argv,
                &format!("shell_environment_policy.set.{name}={}", json::dumps(value)),
            );
            assert_eq!(argv[i - 1], "-c", "{name}");
        }
        for name in ["GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"] {
            assert!(start["env"].get(name).is_none(), "{name} reached Codex");
        }
        let gh = PathBuf::from(start["env"]["GH_CONFIG_DIR"].as_os().unwrap());
        assert_eq!(fs::read_dir(&gh).unwrap().count(), 0, "{}", gh.display());
    }

    #[test]
    fn test_run_id_passed_through_shell_policy() {
        let mut w = World::new();
        w.scenario("ready", &[]);
        w.dispatch("d1").ok();
        let start = w.records("start").remove(0);
        assert_eq!(start["policy_set"]["LEADHAND_RUN_ID"], w.run_id.as_str());
        assert_eq!(start["env"]["LEADHAND_RUN_ID"], w.run_id.as_str());
    }

    #[test]
    fn test_evidence_worker_may_write_the_evidence_base_worktree_only() {
        let mut w = World::new();
        // The workspace is a worktree in <repo>/.worktrees, so its base worktree is named
        // after it.
        let base = w.repo.join(".worktrees").join(format!(
            "{}--base",
            w.ws.file_name().unwrap().to_string_lossy()
        ));
        run_git(
            &w.repo,
            &["worktree", "add", "--quiet", "--detach", s(&base), "HEAD"],
        );
        w.scenario("ready", &[]);
        let run_level = |kind| Req {
            role: "verifier",
            card: None,
            kind: Some(kind),
            ..Req::default()
        };
        w.dispatch_with("e1", &run_level("evidence")).ok();
        w.dispatch_with("v1", &run_level("verify")).ok();
        let starts = w.records("start");
        let (evidence, verify) = (strs(&starts[0]["argv"]), strs(&starts[1]["argv"]));
        let base_real = realpath(&base).to_str().unwrap().to_string();
        assert!(
            add_dirs(&evidence).contains(&base_real),
            "{base_real} not in {evidence:?}"
        );
        let others: Vec<String> = add_dirs(&evidence)
            .into_iter()
            .filter(|d| *d != base_real)
            .collect();
        assert_eq!(add_dirs(&verify), others);
        let verify_real: Vec<PathBuf> = add_dirs(&verify)
            .iter()
            .map(|d| realpath(Path::new(d)))
            .collect();
        assert!(
            !verify_real.contains(&PathBuf::from(&base_real)),
            "{verify_real:?}"
        );
    }

    #[test]
    fn test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink() {
        let mut w = World::new();
        let base = w.repo.join(".worktrees").join(format!(
            "{}--base",
            w.ws.file_name().unwrap().to_string_lossy()
        ));
        let other = w.tmp.join("other worktree");
        run_git(
            &w.repo,
            &["worktree", "add", "--quiet", "--detach", s(&other), "HEAD"],
        );
        w.scenario("ready", &[]);
        let makes: [&dyn Fn(); 2] = [
            &|| std::os::unix::fs::symlink(&other, &base).unwrap(),
            &|| {
                fs::remove_file(&base).unwrap();
                fs::create_dir(&base).unwrap();
            },
        ];
        for (n, make) in makes.iter().enumerate() {
            make();
            let did = format!("e{n}");
            w.dispatch_with(
                &did,
                &Req {
                    role: "verifier",
                    card: None,
                    kind: Some("evidence"),
                    ..Req::default()
                },
            )
            .ok();
            let stderr = read_text(&w.wd(&did).join("codex.stderr"));
            assert!(
                stderr.contains("cannot prepare the Codex worker: refused"),
                "{stderr}"
            );
        }
        assert_eq!(w.records("start"), Vec::<Json>::new()); // Codex never started
    }

    #[test]
    fn test_resume_argv_matches_findings() {
        // Covers P11.
        let mut w = World::new();
        w.scenario("ready", &[]);
        w.dispatch("d1").ok();
        let r = w
            .dispatch_with(
                "d2",
                &Req {
                    resume: Some("thread-123"),
                    ..Req::default()
                },
            )
            .ok();
        let starts = w.records("start");
        let (fresh, resumed) = (strs(&starts[0]["argv"]), strs(&starts[1]["argv"]));
        assert_eq!(resumed[resumed.len() - 3..], ["resume", "thread-123", "-"]);
        // The same options, before `resume`: the fresh line with d2's worker folder and tag.
        let (wd1, wd2) = (w.wd("d1"), w.wd("d2"));
        let same: Vec<String> = fresh
            .iter()
            .map(|a| {
                a.replace(s(&wd1), s(&wd2))
                    .replace(&format!("{}:d1:", w.run_id), &format!("{}:d2:", w.run_id))
            })
            .collect();
        assert_eq!(resumed[..resumed.len() - 3], same[..same.len() - 1]);
        assert!(
            resumed[..index(&resumed, "resume")].contains(&"-m".to_string()),
            "{resumed:?}"
        );
        let start = &starts[1];
        assert_eq!(
            start["prompt"],
            implementer_prompt(w.scratch("d2")).as_str()
        );
        assert_eq!(
            lines(&r.stdout_text()).last().map(String::as_str),
            Some("session: thread-123")
        );
    }
}

mod results {
    //! How a dispatch ends: result.json, the reply `dispatch` prints, the attempt record.
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_ready_reply_parsed_and_attempt_recorded() {
        let mut w = World::new();
        let thread = "11111111-2222-4333-8444-555555555555";
        w.scenario("ready", &[("thread", thread)]);
        let r = w.dispatch("d1").ok();
        let lines = lines(&r.stdout_text());
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by", "exit_reason"]),
            jv(r#"["READY", "", "completed"]"#)
        );
        assert_eq!(saved["session_id"], thread);
        assert_eq!(lines[lines.len() - 1], format!("session: {thread}"));
        assert_eq!(saved["steps"], 2);
        assert!(
            saved["reply"]
                .as_str()
                .is_some_and(|t| t.starts_with("Status: READY")),
            "{}",
            saved["reply"]
        );
        assert_eq!(lines[0], "Status: READY");
        assert!(truthy(&saved["usage_known"]), "{saved}");
        assert_eq!(saved["usage"]["output"], 50);
        let start = w.records("start").remove(0);
        assert_eq!(
            start["prompt"],
            implementer_prompt(w.scratch("d1")).as_str()
        );
        assert_eq!(
            realpath(&PathBuf::from(start["cwd"].as_os().unwrap())),
            realpath(&w.ws)
        );
        assert_eq!(read_text(&w.ws.join("done.txt")), "done\n");
        let attempt = one(w.attempts());
        let keys = key_set(&attempt);
        let missing: Vec<&str> = ATTEMPT_FIELDS
            .iter()
            .copied()
            .filter(|f| !keys.contains(*f))
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
        assert_json_eq!(
            pick(&attempt, &["dispatch", "card", "attempt", "role"]),
            jv(r#"["d1", 1, 1, "implementer"]"#)
        );
        assert_json_eq!(
            pick(&attempt, &["vendor", "model", "effort"]),
            jv(r#"["codex", "gpt-6-luna", "low"]"#)
        );
        assert_eq!(attempt["worker_id"], thread);
        assert_json_eq!(
            pick(&attempt, &["status", "exit_reason", "steps"]),
            jv(r#"["READY", "completed", 2]"#)
        );
        assert_json_eq!(
            attempt["charge_ids"],
            Json::from(&[format!("d1:codex:{thread}")][..])
        );
        assert!(truthy(&attempt["usage_known"]), "{attempt}");
        assert!(attempt["cost_estimate"].is_null(), "{attempt}"); // Codex models are not priced
        assert_eq!(attempt["run"], w.run_id.as_str());
    }

    #[test]
    fn test_reply_without_status_is_blocked_and_raw_kept() {
        let mut w = World::new();
        w.scenario("no-status", &[]);
        w.dispatch("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by", "exit_reason"]),
            jv(r#"["BLOCKED", "", "completed"]"#)
        );
        let raw = read_text(&w.wd("d1").join("reply.raw.md"));
        assert_eq!(raw, "I looked around and stopped.\nNothing else to say.\n");
        assert_eq!(w.attempts()[0]["status"], "BLOCKED");
    }

    #[test]
    fn test_step_limit_stops_process_and_marks_spent() {
        // Covers P6 (steps are counted on item.completed).
        let mut w = World::new();
        w.set_limits("implementer", &[("steps", 3.into())]);
        w.scenario("many-steps", &[]);
        let t0 = Instant::now();
        w.dispatch("d1").ok();
        assert!(t0.elapsed().as_secs_f64() < 20.0, "{:?}", t0.elapsed());
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "step_limit"]"#)
        );
        let steps = saved["steps"].as_i64().unwrap();
        assert!(steps >= 3, "{steps}");
        assert!(steps < 9, "{steps}");
        assert!(!alive(&w.runner_json("d1").unwrap()["codex"]));
        assert_eq!(w.records("exit").last().unwrap()["how"], "sigint");
    }

    #[test]
    fn test_time_limit_stops_process_and_marks_spent() {
        let mut w = World::new();
        w.set_limits("implementer", &[("minutes", 1.into())]);
        w.env.set("LEADHAND_MINUTE_SECONDS", "1");
        w.scenario("slow", &[]);
        let t0 = Instant::now();
        w.dispatch("d1").ok();
        assert!(t0.elapsed().as_secs_f64() < 20.0, "{:?}", t0.elapsed());
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "time_limit"]"#)
        );
        assert!(!alive(&w.runner_json("d1").unwrap()["codex"]));
        let child = w.records("child").remove(0);
        assert!(
            w.stopped("d1")
                .contains_key(&child["child_pid"].as_i64().unwrap()),
            "{child} not in {saved}"
        );
        assert!(!logged_alive(&child), "{child}");
    }

    #[test]
    fn test_usage_limit_is_blocked_by_usage_limit() {
        let mut w = World::new();
        w.scenario("usage-limit", &[]);
        w.dispatch("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by", "exit_reason"]),
            jv(r#"["BLOCKED", "usage limit", "usage_limit"]"#)
        );
        assert_eq!(w.attempts()[0]["blocked_by"], "usage limit");
    }

    #[test]
    fn test_usage_limit_text_does_not_override_clean_ready_reply() {
        let mut w = World::new();
        w.scenario("ready-limit-warning", &[]);
        w.dispatch("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by", "exit_reason"]),
            jv(r#"["READY", "", "completed"]"#)
        );
    }

    #[test]
    fn test_missing_codex_binary_is_blocked_environment() {
        let mut w = World::new();
        let missing = w.tmp.join("no such codex");
        w.env.set("LEADHAND_TEST_CODEX_BIN", &missing);
        w.dispatch("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by", "exit_reason"]),
            jv(r#"["BLOCKED", "environment", "error"]"#)
        );
        let stderr = read_text(&w.wd("d1").join("codex.stderr"));
        assert!(stderr.contains("cannot start"), "{stderr}");
    }

    #[test]
    fn test_paths_with_spaces() {
        let mut w = World::new();
        let extra = w.tmp.join("extra dir");
        fs::create_dir(&extra).unwrap();
        w.set_config(|c| *at(c, &["codex", "writable_dirs"]) = Json::from(&[s(&extra)][..]));
        w.scenario("ready", &[]);
        w.dispatch("d1").ok();
        assert_eq!(w.result("d1")["status"], "READY");
        let argv = strs(&w.records("start")[0]["argv"]);
        assert_eq!(argv[index(&argv, "-C") + 1], s(&w.ws));
        assert!(argv.contains(&s(&extra).to_string()), "{argv:?}");
        assert_eq!(
            argv[index(&argv, "-o") + 1],
            s(&w.wd("d1").join("last-message.md"))
        );
        assert!(s(&w.wd("d1")).contains(' '));
        assert!(w.wd("d1").join("stream.jsonl").exists());
    }
}

mod usage {
    //! What a dispatch charges (charges.jsonl) and records (attempts.jsonl).
    use super::*;

    #[test]
    fn test_killed_worker_usage_from_session_or_unknown() {
        let mut w = World::new();
        w.set_limits("implementer", &[("steps", 1.into())]);
        // With a session file: usage comes from it although no turn completed.
        w.scenario(
            "killed",
            &[("thread", "00000000-0000-4000-8000-000000000001")],
        );
        w.dispatch("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "step_limit"]"#)
        );
        assert!(truthy(&saved["usage_known"]), "{saved}");
        let usage = &saved["usage"];
        assert!(
            usage["input"].as_i64().unwrap() + usage["cached"].as_i64().unwrap() > 0,
            "{usage}"
        );
        assert_eq!(
            w.charges()[0]["charge_id"],
            "d1:codex:00000000-0000-4000-8000-000000000001"
        );
        // Without one: unknown, never zero.
        w.scenario(
            "killed",
            &[
                ("thread", "00000000-0000-4000-8000-000000000002"),
                ("no_rollout", "1"),
            ],
        );
        w.dispatch("d2").ok();
        assert!(!truthy(&w.result("d2")["usage_known"]));
        let attempt = w.attempts().pop().unwrap();
        assert_json_eq!(
            pick(
                &attempt,
                &["dispatch", "usage", "usage_known", "cost_estimate"]
            ),
            jv(r#"["d2", null, false, null]"#)
        );
    }

    #[test]
    fn test_stream_usage_is_charged_when_session_missing() {
        let mut w = World::new();
        let thread = "22222222-2222-4333-8444-555555555555";
        w.scenario("ready", &[("thread", thread)]);
        w.dispatch("d1").ok();
        let c = one(w.charges());
        assert_json_eq!(
            pick(&c, &["charge_id", "source", "key"]),
            Json::from(
                &[
                    format!("d1:codex:{thread}").as_str(),
                    "codex-stream",
                    thread
                ][..]
            )
        );
        assert!(truthy(&c["usage"]["known"]), "{c}");
        let attempt = one(w.attempts());
        assert_json_eq!(attempt["usage"], c["usage"]);
        assert_json_eq!(c["usage"]["output"], w.result("d1")["usage"]["output"]);
    }
}

mod runner_lifecycle {
    //! The detached runner: who owns a dispatch, what a repeated `dispatch` line attaches to,
    //! and the exit codes (0 with the reply; 1 with `error: `; 3 for a second runner).
    use super::*;
    use common::dispatch_world::{Req, WAIT};
    use common::{cli, holds};
    use rustix::process::{Pid, Signal, kill_process_group};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn test_runner_survives_foreground_death_and_records_result() {
        // Covers P10.
        let mut w = World::new();
        w.scenario("ready", &[("delay", "2")]);
        let fg = w.popen("d1");
        wait_for_with("Codex started", WAIT, Some(&fg), || w.codex_of("d1"));
        // The foreground's own group: never the runner's session.
        let _ = kill_process_group(Pid::from_raw(fg.pid()).unwrap(), Signal::KILL);
        fg.communicate(Duration::from_secs(10))
            .expect("the foreground's pipes were still held 10 s after it was killed");
        wait_for_with("result.json", Duration::from_secs(20), None, || {
            when(w.wd("d1").join("result.json").exists())
        });
        assert_eq!(w.result("d1")["status"], "READY");
        let attempt = one(w.attempts());
        assert_eq!(attempt["dispatch"], "d1");
        // Limits came from the run's config (the default preset's implementer limits).
        let request = w.read("d1", "request.json").unwrap();
        assert_json_eq!(
            pick(&request, &["steps", "minutes", "network"]),
            jv("[150, 60, false]")
        );
    }

    #[test]
    fn test_second_dispatch_attaches_to_live_runner() {
        let mut w = World::new();
        w.scenario("ready", &[("delay", "2")]);
        let first = w.start_detached("d1");
        let runner = w.runner_json("d1").unwrap()["runner"].clone();
        w.dispatch("d1").ok();
        assert_eq!(w.result("d1")["status"], "READY");
        assert_eq!(w.records("start").len(), 1);
        assert_json_eq!(w.runner_json("d1").unwrap()["runner"], runner);
        assert_json_eq!(w.runner_json("d1").unwrap()["codex"], first);
    }

    #[test]
    fn test_repeated_dispatch_returns_existing_result() {
        let mut w = World::new();
        w.scenario("ready", &[]);
        let first = w.dispatch("d1").ok();
        let second = w.dispatch("d1").ok();
        assert_eq!(first.stdout_text(), second.stdout_text());
        assert_eq!(w.records("start").len(), 1);
        assert_eq!(w.attempts().len(), 1);
    }

    #[test]
    fn test_cli_prints_reply_and_session_or_still_running() {
        // The first call is left before its runner finishes. (Its exit 75 and `still
        // running` line, after the foreground wait, are
        // test_foreground_wait_ends_with_exit_75_….)
        let mut w = World::new();
        w.scenario("ready", &[("delay", "1")]);
        w.start_detached("d1");
        let r = w.dispatch("d1").ok();
        let lines = lines(&r.stdout_text());
        assert_eq!(lines[0], "Status: READY");
        assert_eq!(
            lines[lines.len() - 1],
            format!("session: {}", common::dispatch_world::DEFAULT_THREAD)
        );
        assert_eq!(w.records("start").len(), 1);
    }

    #[test]
    fn test_foreground_wait_ends_with_exit_75_and_the_still_running_line() {
        // Covers R20.
        // LEADHAND_TEST_FOREGROUND_WAIT shortens the 540 s FOREGROUND_WAIT to 3 s (ruling
        // S1-R12); unit tests pin the 540 s default.
        let _serial = common::serial(); // group: the foreground wait
        let mut w = World::new();
        w.scenario("ready", &[("delay", "120")]); // Codex outlasts the foreground wait
        w.env.set("LEADHAND_TEST_FOREGROUND_WAIT", "3");
        let r = w.dispatch_timeout("d1", &Req::default(), Duration::from_secs(60));
        assert_eq!(r.code, 75, "{r}");
        assert_eq!(
            lines(&r.stdout_text()),
            ["still running: run the same command again (dispatch d1)"]
        );
        assert!(!w.wd("d1").join("result.json").exists());
        let runner = w.runner_json("d1").unwrap();
        assert!(alive(&runner["runner"]), "{runner}");
        assert!(alive(&runner["codex"]), "{runner}");
        assert_eq!(w.records("start").len(), 1);
    }

    #[test]
    fn test_concurrent_dispatches_start_one_runner() {
        let mut w = World::new();
        w.scenario("ready", &[("delay", "2")]);
        let procs = [w.popen("d1"), w.popen("d1")];
        let outs: Vec<(Vec<u8>, Vec<u8>)> = procs
            .iter()
            .map(|p| {
                p.communicate(Duration::from_secs(60))
                    .expect("a dispatch line still running after 60 s")
            })
            .collect();
        for (p, (out, err)) in procs.iter().zip(&outs) {
            assert_eq!(p.returncode(), Some(0), "{}", String::from_utf8_lossy(err));
            assert_eq!(
                String::from_utf8_lossy(out).lines().next(),
                Some("Status: READY")
            );
        }
        assert_eq!(w.records("start").len(), 1);
        assert_eq!(w.attempts().len(), 1);
        assert!(!w.wd("d1").join("superseded").exists());
    }

    #[test]
    fn test_second_runner_refuses_while_first_holds_lock() {
        let mut w = World::new();
        w.scenario("slow", &[]);
        w.start_detached("d1");
        let before = read_text(&w.wd("d1").join("runner.json"));
        let r = cli(&os_args!["dispatch-runner", w.wd("d1")], &w.env, &w.tmp);
        assert_eq!(r.code, 3, "{r}");
        assert_eq!(read_text(&w.wd("d1").join("runner.json")), before);
        assert_eq!(w.records("start").len(), 1);
    }

    #[test]
    fn test_runner_record_without_start_time_is_not_dead_while_lock_held() {
        // A runner whose start time could not be read still owns the dispatch while it holds
        // runner.lock: `dispatch` waits on it and replaces it only once the lock is free.
        let mut w = World::new();
        w.scenario("ready", &[]);
        let decoy = w.decoy();
        let wd = w.wd("d1");
        fs::create_dir_all(&wd).unwrap();
        let record = Json::object([
            (
                "runner",
                Json::object([
                    ("pid", decoy.pid().into()),
                    ("pgid", decoy.pid().into()),
                    ("start", Json::Null),
                ]),
            ),
            ("codex", Json::Null),
            ("generation", 1.into()),
        ]);
        fs::write(wd.join("runner.json"), json::dumps(&record)).unwrap();
        fs::write(wd.join("heartbeat"), "").unwrap();
        let lock = common::hold_lock(&wd.join("runner.lock"));
        let fg = w.popen("d1");
        wait_for_with("dispatch to write request.json", WAIT, Some(&fg), || {
            when(wd.join("request.json").exists())
        });
        holds(
            || {
                fg.poll().is_none()
                    && !wd.join("spawn.json").exists()
                    && !wd.join("superseded").exists()
            },
            1.0,
            "dispatch waits on the runner holding runner.lock",
        );
        assert_eq!(w.records("start"), Vec::<Json>::new());
        drop(lock);
        let (out, err) = fg
            .communicate(Duration::from_secs(60))
            .expect("dispatch still running 60 s after the lock was freed");
        assert_eq!(
            fg.returncode(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert_eq!(
            String::from_utf8_lossy(&out).lines().next(),
            Some("Status: READY")
        );
        assert_json_eq!(read_json(&wd.join("superseded/1/runner.json")), record);
        assert!(decoy.poll().is_none(), "the decoy was signalled");
    }

    #[test]
    fn test_launch_intent_recorded_before_codex_starts() {
        let mut w = World::new();
        w.scenario("ready", &[]);
        w.env.set("LEADHAND_CRASH_AT", "before_worker_launch");
        let r = w.dispatch("d1");
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        let runner = w.runner_json("d1").unwrap();
        assert!(runner["codex"].is_null(), "{runner}");
        assert!(runner.get("launching").is_some_and(truthy), "{runner}");
        assert_eq!(w.records("start"), Vec::<Json>::new());
    }

    #[test]
    fn test_ready_result_landing_during_replacement_cancel_is_kept() {
        let _serial = common::serial(); // group: the stop ladder (a hung runner replaced)
        let mut w = World::new();
        w.scenario("ready", &[("delay", "1")]);
        let codex = w.start_detached("d1");
        let runner = w.runner_json("d1").unwrap()["runner"].clone();
        World::signal(&runner, Signal::STOP);
        w.resume_on_drop(&runner);
        wait_for("Codex to exit", || {
            when(
                w.records("exit")
                    .iter()
                    .any(|r| json::py_eq(&r["pid"], &codex["pid"])),
            )
        });
        w.age_heartbeat("d1");
        let wd = w.wd("d1");
        let done = Arc::new(AtomicBool::new(false));
        let t = {
            let (wd, done, runner) = (wd.clone(), done.clone(), runner.clone());
            std::thread::spawn(move || {
                let end = Instant::now() + Duration::from_secs(60);
                while !done.load(Ordering::SeqCst)
                    && !wd.join("cancel").exists()
                    && Instant::now() < end
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                World::signal(&runner, Signal::CONT);
            })
        };
        let r = w.dispatch("d1");
        done.store(true, Ordering::SeqCst);
        t.join().unwrap();
        r.ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["READY", "completed"]"#)
        );
        assert_eq!(w.records("start").len(), 1);
        assert!(!wd.join("superseded").exists());
    }

    #[test]
    fn test_heartbeat_refreshed_before_second_look_is_not_hung() {
        let _serial = common::serial(); // group: the 5 s second look at a stale heartbeat
        let mut w = World::new();
        w.scenario("slow", &[]);
        w.start_detached("d1");
        let runner = w.runner_json("d1").unwrap()["runner"].clone();
        World::signal(&runner, Signal::STOP);
        w.resume_on_drop(&runner);
        w.age_heartbeat("d1");
        let wd = w.wd("d1");
        let fg = w.popen("d1");
        let untouched = || {
            fg.poll().is_none()
                && !wd.join("cancel").exists()
                && w.runner_json("d1")
                    .and_then(|r| r.get("runner").cloned())
                    .is_some_and(|r| json::py_eq(&r, &runner))
        };
        // The runner wakes 2 s after the second `dispatch` started: after that command's
        // first look found the heartbeat stale, and well inside the 5 s it waits before
        // calling it hung. It is still running, untouched, 8 s after it started (the Python
        // test's wait).
        holds(
            untouched,
            2.0,
            "the stale runner is left alone before it wakes",
        );
        World::signal(&runner, Signal::CONT);
        holds(
            untouched,
            6.0,
            "the runner that refreshed its heartbeat is left alone",
        );
        assert!(alive(&runner), "{runner}");
        assert_eq!(w.records("start").len(), 1);
    }
}

mod foreground_wait_hook {
    use super::*;
    use common::cli_with;
    use std::time::Duration;

    #[test]
    fn test_foreground_wait_hook_is_honoured_only_under_leadhand_testing() {
        // Covers limit L4 (test hooks need LEADHAND_TESTING=1).
        // Observed through what `dispatch` prints when a 1 s hook meets a Codex that answers
        // after 4 s. Without LEADHAND_TESTING=1, LEADHAND_TEST_CODEX_BIN is ignored too, so in
        // both cases the Codex is the fake found first on PATH as `codex`.
        let _serial = common::serial(); // group: the foreground wait
        let mut w = World::new();
        w.scenario("ready", &[("delay", "4")]);
        let env = w
            .env
            .with("PATH", w.env.path_with_fakes())
            .with("LEADHAND_TEST_FOREGROUND_WAIT", "1")
            .without("LEADHAND_TEST_CODEX_BIN");
        for (did, testing, honoured) in [("d1", "1", true), ("d2", "0", false)] {
            let args: Vec<OsString> = w.args(did);
            let refs = common::dispatch_world::os_refs(&args);
            let r = cli_with(
                &refs,
                &env.with("LEADHAND_TESTING", testing),
                &w.tmp,
                None,
                Duration::from_secs(120),
            );
            if honoured {
                assert_eq!(r.code, 75, "testing={testing}\n{r}");
                assert_eq!(
                    lines(&r.stdout_text()),
                    [format!(
                        "still running: run the same command again (dispatch {did})"
                    )],
                    "testing={testing}"
                );
                assert!(!w.wd(did).join("result.json").exists(), "testing={testing}");
            } else {
                let r = r.ok();
                let lines = lines(&r.stdout_text());
                assert_eq!(lines[0], "Status: READY", "testing={testing}\n{r}");
                assert_eq!(
                    lines[lines.len() - 1],
                    format!("session: {}", common::dispatch_world::DEFAULT_THREAD),
                    "testing={testing}\n{r}"
                );
                assert_eq!(w.result(did)["status"], "READY", "testing={testing}");
            }
        }
        assert_eq!(w.records("start").len(), 2);
    }
}

mod cancellation {
    //! `stop-dispatch`, a cancel request, and a hung runner's replacement.
    use super::*;
    use common::{holds, start_time};
    use rustix::process::Signal;

    #[test]
    fn test_signal_only_after_pid_and_start_time_match() {
        // Covers bug class 4 (ownership by identity: pid and start time).
        let mut w = World::new();
        let decoy = w.decoy();
        let wrong = Json::object([
            ("pid", decoy.pid().into()),
            ("pgid", decoy.pid().into()),
            (
                "start",
                format!("{}-not", start_time(decoy.pid()).unwrap()).into(),
            ),
        ]);
        let runner_record = || {
            json::dumps(&Json::object([
                ("runner", wrong.clone()),
                ("codex", wrong.clone()),
                ("generation", 1.into()),
            ]))
        };
        let wd = w.wd("d1");
        fs::create_dir_all(&wd).unwrap();
        fs::write(wd.join("request.json"), json::dumps(&w.request_json("d1"))).unwrap();
        fs::write(wd.join("runner.json"), runner_record()).unwrap();
        fs::write(
            wd.join("procs.json"),
            json::dumps(&Json::object([(
                "descendants",
                Json::Arr(vec![wrong.clone()]),
            )])),
        )
        .unwrap();
        w.stop("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "cancelled"]"#)
        );
        holds(
            || decoy.poll().is_none(),
            0.3,
            "a process whose start time did not match was not signalled",
        );
        // dispatch treats the mismatched runner as gone: a fresh dispatch id runs normally.
        w.scenario("ready", &[]);
        let wd2 = w.wd("d2");
        fs::create_dir_all(&wd2).unwrap();
        fs::write(wd2.join("runner.json"), runner_record()).unwrap();
        w.dispatch("d2").ok();
        assert_eq!(w.result("d2")["status"], "READY");
        assert!(decoy.poll().is_none(), "the decoy was signalled");
    }

    #[test]
    fn test_cancel_detached_worker_stops_every_process_and_writes_nothing_after() {
        // Covers bug class 6 (process lifetime) and P10.
        let _serial = common::serial(); // group: the stop ladder
        let mut w = World::new();
        w.scenario("slow", &[]);
        let codex = w.start_detached("d1");
        let child = wait_for("the tool child", || w.records("child").into_iter().next());
        let progress = w.ws.join("progress.log");
        wait_for("progress.log", || {
            when(fs::metadata(&progress).is_ok_and(|m| m.len() > 0))
        });
        let r = w.stop("d1").ok();
        assert_eq!(r.stdout_text().lines().next(), Some("Status: SPENT"), "{r}");
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "cancelled"]"#)
        );
        assert!(!alive(&codex), "{codex}");
        assert!(
            w.stopped("d1")
                .contains_key(&child["child_pid"].as_i64().unwrap()),
            "{child} not in {saved}"
        );
        assert!(!logged_alive(&child), "{child}");
        let stamp = |m: fs::Metadata| (m.len(), m.modified().unwrap());
        let before = stamp(fs::metadata(&progress).unwrap());
        holds(
            || stamp(fs::metadata(&progress).unwrap()) == before,
            1.5,
            "nothing writes progress.log after the cancel",
        );
        assert_eq!(w.attempts()[0]["exit_reason"], "cancelled");
    }

    #[test]
    fn test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside() {
        let _serial = common::serial(); // group: the stop ladder (a hung runner replaced)
        let mut w = World::new();
        w.scenario("slow", &[]);
        let first_codex = w.start_detached("d1");
        let runner = w.runner_json("d1").unwrap()["runner"].clone();
        World::signal(&runner, Signal::STOP); // hung: alive, but no heartbeat
        w.resume_on_drop(&runner);
        w.age_heartbeat("d1");
        w.scenario("ready", &[]);
        w.dispatch("d1").ok();
        assert_eq!(w.result("d1")["status"], "READY");
        assert!(!alive(&runner), "{runner}");
        assert!(!alive(&first_codex), "{first_codex}");
        let superseded = read_json(&w.wd("d1").join("superseded/1/result.json"));
        assert_json_eq!(
            pick(&superseded, &["status", "exit_reason"]),
            jv(r#"["SPENT", "cancelled"]"#)
        );
        let second = w.runner_json("d1").unwrap();
        assert_ne!(second["runner"]["pid"], runner["pid"]);
        assert!(
            ge(&second["started_wall"], &superseded["confirmed_at"]),
            "{second} {superseded}"
        );
        let starts = w.records("start");
        let exits: Vec<Json> = w
            .records("exit")
            .into_iter()
            .filter(|r| json::py_eq(&r["pid"], &first_codex["pid"]))
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(!exits.is_empty(), "the first Codex logged no exit");
        assert!(
            ge(&starts[1]["time"], &exits[0]["time"]),
            "{} {}",
            starts[1],
            exits[0]
        );
        assert_eq!(w.attempts().len(), 2);
    }

    #[test]
    fn test_cancel_requested_during_orphan_is_not_resurrected() {
        let _serial = common::serial(); // group: the stop ladder
        let mut w = World::new();
        w.scenario("slow", &[]);
        let codex = w.start_detached("d1");
        // The one start counted below must have happened before Codex is orphaned and
        // cancelled (stage 2's S2-R59/S2-R60).
        wait_for("the fake Codex's start record", || {
            w.records("start").into_iter().next()
        });
        let runner = w.runner_json("d1").unwrap()["runner"].clone();
        World::signal(&runner, Signal::KILL);
        wait_for("the runner to die", || when(!alive(&runner)));
        // stop-dispatch has made its request (step 1) and is still stopping Codex.
        fs::write(
            w.wd("d1").join("cancel"),
            json::dumps(&jv(r#"{"requested_at": "earlier"}"#)),
        )
        .unwrap();
        w.dispatch("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "cancelled"]"#)
        );
        assert!(!alive(&codex), "{codex}");
        assert_eq!(w.records("start").len(), 1);
        assert!(!w.wd("d1").join("superseded").exists());
    }
}

mod process_control {
    //! Codex's processes outlive neither the runner nor the dispatch.
    use super::*;
    use common::holds;
    use rustix::process::Signal;

    #[test]
    fn test_orphaned_codex_group_is_terminated_before_finalizing() {
        let _serial = common::serial(); // group: the stop ladder
        let mut w = World::new();
        w.scenario("slow", &[("orphan_child", "1")]);
        let codex = w.start_detached("d1");
        let child_rec = wait_for("the tool child", || w.records("child").into_iter().next());
        let child = child_rec["child_pid"].as_i64().unwrap();
        wait_for("procs.json to list the tool child", || {
            when(
                w.read("d1", "procs.json")
                    .and_then(|p| p.get("descendants").cloned())
                    .and_then(|d| d.as_array().map(<[Json]>::to_vec))
                    .unwrap_or_default()
                    .iter()
                    .any(|d| d["pid"] == child),
            )
        });
        let runner = w.runner_json("d1").unwrap()["runner"].clone();
        World::signal(&runner, Signal::KILL);
        wait_for("the runner to die", || when(!alive(&runner)));
        assert!(alive(&codex), "{codex}");
        w.stop("d1").ok();
        let saved = w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "cancelled"]"#)
        );
        assert!(!alive(&codex), "{codex}");
        assert!(
            saved["stopped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["pid"] == child),
            "{child} not in {saved}"
        );
        assert!(!logged_alive(&child_rec), "{child_rec}");
        assert_eq!(w.attempts().len(), 1);
    }

    #[test]
    fn test_double_forked_child_is_stopped_before_finalizing() {
        // Covers R51 and limit M4.
        // Codex review round 1 (M4): a process that leaves Codex's tree (double fork, new
        // session) just before Codex exits is found by the dispatch tag in its environment
        // and stopped before the result is written.
        let mut w = World::new();
        w.scenario("ready-escaped-writer", &[]);
        w.dispatch("d1").ok();
        let rec = one(w.records("escaped"));
        let pid = rec["escaped_pid"].as_i64().unwrap();
        assert_eq!(w.result("d1")["status"], "READY");
        assert!(w.stopped("d1").contains_key(&pid), "{pid} was not stopped");
        assert!(
            !logged_alive(&rec),
            "the escaped writer outlived the dispatch"
        );
        let log = w.ws.join("escaped.log");
        let size = fs::metadata(&log).unwrap().len();
        holds(
            || fs::metadata(&log).unwrap().len() == size,
            0.5,
            "the escaped writer writes nothing after finalize",
        );
        let tag = w.runner_json("d1").unwrap()["tag"].clone();
        assert_eq!(tag, format!("{}:d1:1", w.run_id).as_str());
        let start = w.records("start").remove(0);
        assert_json_eq!(start["env"]["LEADHAND_DISPATCH_TAG"], tag);
        assert_json_eq!(start["policy_set"]["LEADHAND_DISPATCH_TAG"], tag);
    }
}

mod runner_commits {
    //! Ruling R39: Codex cannot write the Git directory, so the runner commits its work.
    use super::*;
    use common::dispatch_world::Req;
    use common::git::run_git;
    use rustix::process::Signal;
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The world of `setUp`: a card branch checked out in the workspace, and a repository
    /// that signs commits and has a failing pre-commit hook.
    struct Commits {
        w: World,
        base_sha: String,
    }

    impl Commits {
        fn new() -> Commits {
            let w = World::new();
            w.git(&["checkout", "--quiet", "-b", CARD_BRANCH]);
            let base_sha = w.git(&["rev-parse", "HEAD"]);
            // The runner's commit must be unsigned and skip hooks, whatever the repo says.
            run_git(&w.repo, &["config", "commit.gpgsign", "true"]);
            run_git(&w.repo, &["config", "gpg.program", "false"]);
            let hooks = w.git(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"]);
            let hook = PathBuf::from(hooks).join("pre-commit");
            fs::create_dir_all(hook.parent().unwrap()).unwrap();
            fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            Commits { w, base_sha }
        }

        /// The request flags of an implement dispatch on the card branch, with the card's
        /// brief.
        fn commit_request(&self, did: &str) -> Req<'static> {
            self.w.scratch_with(did, Some(1), Some(CARD_BRIEF));
            Req {
                kind: Some("implement"),
                branch: Some(CARD_BRANCH),
                ..Req::default()
            }
        }

        fn report(&self, did: &str) -> PathBuf {
            self.w.scratch(did).1
        }

        /// Codex's edits: two files, the report unless `report` is false, `steps` counted
        /// steps then a long sleep, and the reply.
        fn edits(&mut self, did: &str, report: bool, steps: usize, status: &str) {
            let mut lines: Vec<Value> = vec![
                json!({"type": "thread.started", "thread_id": "{thread}"}),
                json!({"type": "turn.started"}),
                json!({"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"}),
                json!({"action": "write_file", "path": "notes/new file.txt", "text": "untracked\n"}),
            ];
            if report {
                lines.push(json!({"action": "write_file", "path": s(&self.report(did)),
                    "text": format!("Status: {status}\n## Summary\nAdded the widget constant.\nMore.\n")}));
            }
            for i in 0..steps {
                lines.push(
                    json!({"type": "item.completed", "item": {"id": format!("c{i}"),
                    "type": "command_execution", "status": "completed"}}),
                );
                lines.push(json!({"action": "sleep", "seconds": 0.3}));
            }
            if steps > 0 {
                lines.push(json!({"action": "sleep", "seconds": 30}));
            }
            lines.push(json!({"action": "last_message", "text": format!("Status: {status}\n")}));
            lines.push(
                json!({"type": "turn.completed", "usage": {"input_tokens": 10,
                "cached_input_tokens": 0, "output_tokens": 1}}),
            );
            self.w.scenario_steps(&lines, &[]);
        }

        fn edits_default(&mut self, did: &str) {
            self.edits(did, true, 0, "READY");
        }

        fn head(&self) -> String {
            self.w.git(&["rev-parse", "HEAD"])
        }

        fn head_files(&self) -> Vec<String> {
            lines(&self.w.git(&["show", "--name-only", "--format=", "HEAD"]))
        }
    }

    fn now_wall() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    #[test]
    fn test_runner_commits_codex_work_on_card_branch() {
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.edits_default("d1");
        c.w.dispatch_with("d1", &req).ok();
        let w = &c.w;
        let saved = w.result("d1");
        assert_eq!(saved["status"], "READY");
        let head = c.head();
        assert_eq!(saved["committed"], head.as_str());
        assert_eq!(
            w.git(&["rev-parse", &format!("refs/heads/{CARD_BRANCH}")]),
            head
        );
        assert_eq!(w.git(&["rev-parse", "HEAD^"]), c.base_sha);
        assert_eq!(
            w.git(&["log", "-1", "--format=%s"]),
            "implement card 1: Added the widget constant."
        );
        let body = w.git(&["log", "-1", "--format=%B"]);
        assert!(body.contains("Leadhand-Dispatch: d1"), "{body}");
        let raw = w.git(&["cat-file", "commit", "HEAD"]);
        assert!(!raw.contains("gpgsig"), "{raw}");
        let mut files = c.head_files();
        files.sort();
        assert_eq!(files, ["notes/new file.txt", "src/widget.py"]);
        assert_eq!(w.git(&["status", "--porcelain"]), "");
        assert!(c.report("d1").exists()); // the scratch folder stays, untracked and ignored
    }

    #[test]
    fn test_runner_commits_partial_work_when_spent() {
        let mut c = Commits::new();
        c.w.set_limits("implementer", &[("steps", 2.into())]);
        let req = c.commit_request("d1");
        c.edits("d1", false, 5, "READY");
        c.w.dispatch_with("d1", &req).ok();
        let saved = c.w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "step_limit"]"#)
        );
        assert_eq!(saved["committed"], c.head().as_str());
        assert_eq!(
            c.w.git(&["log", "-1", "--format=%s"]),
            "implement card 1: Widget card"
        );
        assert!(
            c.head_files().contains(&"src/widget.py".to_string()),
            "{:?}",
            c.head_files()
        );
    }

    #[test]
    fn test_commit_intent_restart_after_the_commit_does_not_commit_again() {
        // A runner that dies after its commit (the intent in commit.json, HEAD carrying the
        // trailer) is finalized by the next `dispatch`, which reports that commit and makes no
        // other.
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.edits_default("d1");
        c.w.env
            .set("LEADHAND_CRASH_AT", "after_charge_before_attempt");
        let r = c.w.dispatch_with("d1", &req);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        let sha = c.head();
        assert_ne!(sha, c.base_sha);
        assert!(c.w.wd("d1").join("commit.json").exists());
        assert!(!c.w.wd("d1").join("result.json").exists());
        c.w.env.remove("LEADHAND_CRASH_AT");
        c.w.dispatch_with("d1", &req).ok();
        let interrupted = read_json(&c.w.wd("d1").join("superseded/1/result.json"));
        assert_eq!(interrupted["committed"], sha.as_str());
        assert_eq!(c.head(), sha);
        assert_eq!(
            c.w.git(&["rev-list", "--count", &format!("{}..HEAD", c.base_sha)]),
            "1"
        );
    }

    #[test]
    fn test_commit_intent_restart_before_the_commit_still_commits() {
        // The intent is there but HEAD has not moved: the commit was never made, so it is.
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        fs::create_dir_all(c.w.wd("d1")).unwrap();
        let intent = Json::object([
            ("at", "2026-01-01T00:00:00.000Z".into()),
            ("branch", CARD_BRANCH.into()),
            ("head_before", c.base_sha.as_str().into()),
        ]);
        fs::write(c.w.wd("d1").join("commit.json"), json::dumps(&intent)).unwrap();
        c.edits_default("d1");
        c.w.dispatch_with("d1", &req).ok();
        let saved = c.w.result("d1");
        assert_ne!(saved["committed"], c.base_sha.as_str());
        assert_eq!(saved["committed"], c.head().as_str());
        assert_eq!(
            c.w.git(&["rev-list", "--count", &format!("{}..HEAD", c.base_sha)]),
            "1"
        );
    }

    /// Opens the FIFO for writing without blocking and closes it, so a reader blocked in its
    /// open returns (the Python's cleanup `unblock`).
    struct Unblock(PathBuf);

    impl Drop for Unblock {
        fn drop(&mut self) {
            use rustix::fs::{Mode, OFlags, open};
            let _ = open(&self.0, OFlags::WRONLY | OFlags::NONBLOCK, Mode::empty());
        }
    }

    #[test]
    fn test_a_fifo_left_at_the_report_never_hangs_the_commit() {
        // Codex review 9: the runner reads the report outside the sandbox; a FIFO would block
        // it.
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        let report = c.report("d1");
        let made = Command::new("mkfifo").arg(&report).status().unwrap();
        assert!(made.success(), "mkfifo {}", report.display());
        let _unblock = Unblock(report.clone());
        c.edits("d1", false, 0, "READY");
        c.w.dispatch_with("d1", &req).ok();
        assert_eq!(c.w.result("d1")["committed"], c.head().as_str());
        assert_eq!(
            c.w.git(&["log", "-1", "--format=%s"]),
            "implement card 1: Widget card"
        );
    }

    #[test]
    fn test_runner_commits_an_edit_hidden_behind_an_index_flag() {
        let mut c = Commits::new();
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            c.w.git(&["reset", "--quiet", "--hard", &c.base_sha]);
            let did = format!("d{}", &flag[2..5]);
            let req = c.commit_request(&did);
            c.w.git(&["update-index", flag, "README.md"]);
            c.w.scenario_steps(
                &[
                    json!({"type": "thread.started", "thread_id": "{thread}"}),
                    json!({"type": "turn.started"}),
                    json!({"action": "write_file", "path": "README.md", "text": "edited, hidden from status\n"}),
                    json!({"action": "last_message", "text": "Status: READY\n"}),
                ],
                &[],
            );
            c.w.dispatch_with(&did, &req).ok();
            assert_eq!(
                c.w.result(&did)["committed"],
                c.head().as_str(),
                "flag={flag}"
            );
            assert_eq!(
                c.w.git(&["show", "HEAD:README.md"]),
                "edited, hidden from status",
                "flag={flag}"
            );
            assert_eq!(
                c.w.git(&["ls-files", "-v", "--", "README.md"]),
                "H README.md",
                "flag={flag}"
            );
        }
    }

    #[test]
    fn test_failed_commit_is_blocked_environment_with_reason() {
        let mut c = Commits::new();
        for name in [
            "EMAIL",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            c.w.env.remove(name);
        }
        run_git(&c.w.repo, &["config", "--unset", "user.name"]);
        run_git(&c.w.repo, &["config", "--unset", "user.email"]);
        run_git(&c.w.repo, &["config", "user.useConfigOnly", "true"]);
        let req = c.commit_request("d1");
        c.edits_default("d1");
        let r = c.w.dispatch_with("d1", &req).ok();
        let saved = c.w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by", "exit_reason"]),
            jv(r#"["BLOCKED", "environment", "error"]"#)
        );
        let reply = saved["reply"].as_str().unwrap();
        assert!(reply.contains("could not commit"), "{reply}");
        assert!(r.stdout_text().contains("could not commit"), "{r}");
        assert!(saved["committed"].is_null(), "{saved}");
        assert!(truthy(&saved["commit_error"]), "{saved}");
        let first: String = saved["commit_error"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();
        assert!(reply.contains(&first), "{first:?} not in {reply:?}");
        assert_eq!(c.head(), c.base_sha);
        let status = c.w.git(&["status", "--porcelain"]);
        assert!(status.contains("src/"), "{status}");
        assert_eq!(c.w.attempts()[0]["status"], "BLOCKED");
    }

    #[test]
    fn test_commit_respects_the_cancel_deadline() {
        // A cancel whose deadline has passed leaves the runner no time to commit.
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.w.scenario_steps(
            &[
                json!({"type": "thread.started", "thread_id": "{thread}"}),
                json!({"type": "turn.started"}),
                json!({"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"}),
                json!({"action": "sleep", "seconds": 60}),
            ],
            &[],
        );
        c.w.start_detached_with("d1", &req);
        let widget = c.w.ws.join("src/widget.py");
        wait_for("Codex's edit", || when(widget.exists()));
        let cancel = Json::object([
            ("requested_at", "2026-01-01T00:00:00.000Z".into()),
            ("deadline_wall", (now_wall() - 1.0).into()),
        ]);
        fs::write(c.w.wd("d1").join("cancel"), json::dumps(&cancel)).unwrap();
        let result = c.w.wd("d1").join("result.json");
        wait_for("result.json", || when(result.exists()));
        let saved = c.w.result("d1");
        assert!(saved["committed"].is_null(), "{saved}");
        assert!(
            saved["commit_error"]
                .as_str()
                .is_some_and(|e| e.contains("deadline")),
            "{saved}"
        );
        assert_eq!(c.head(), c.base_sha);
    }

    #[test]
    fn test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed() {
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.w.scenario_steps(
            &[
                json!({"type": "thread.started", "thread_id": "{thread}"}),
                json!({"type": "turn.started"}),
                json!({"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"}),
                json!({"type": "item.completed", "item": {"id": "c1", "type": "command_execution",
                    "status": "completed"}}),
                json!({"action": "sleep", "seconds": 60}),
            ],
            &[],
        );
        let codex = c.w.start_detached_with("d1", &req);
        let widget = c.w.ws.join("src/widget.py");
        wait_for("Codex's edit", || when(widget.exists()));
        World::signal(&codex, Signal::KILL); // from outside, like Task 22
        c.w.dispatch_with("d1", &req).ok();
        let saved = c.w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "exit_reason"]),
            jv(r#"["SPENT", "interrupted"]"#)
        );
        let charge = one(c.w.charges());
        assert!(
            charge.get("possibly_incomplete").is_some_and(truthy),
            "{charge}"
        );
        let attempt = one(c.w.attempts());
        assert!(truthy(&attempt["possibly_incomplete"]), "{attempt}");
        assert!(truthy(&saved["possibly_incomplete"]), "{saved}");
        assert_eq!(saved["committed"], c.head().as_str());
        assert!(
            c.head_files().contains(&"src/widget.py".to_string()),
            "{:?}",
            c.head_files()
        );
    }

    #[test]
    fn test_exit_without_final_turn_completed_is_possibly_incomplete() {
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.w.scenario_steps(
            &[
                json!({"type": "thread.started", "thread_id": "{thread}"}),
                json!({"type": "turn.started"}),
                json!({"type": "turn.completed", "usage": {"input_tokens": 5,
                    "cached_input_tokens": 0, "output_tokens": 1}}),
                json!({"type": "turn.started"}),
                json!({"action": "last_message", "text": "Status: READY\n"}),
            ],
            &[],
        );
        c.w.dispatch_with("d1", &req).ok();
        assert_eq!(c.w.result("d1")["status"], "READY");
        let charge = one(c.w.charges());
        assert!(
            charge.get("possibly_incomplete").is_some_and(truthy),
            "{charge}"
        );
        assert!(truthy(&c.w.attempts()[0]["possibly_incomplete"]));
    }

    #[test]
    fn test_complete_run_is_not_possibly_incomplete() {
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.edits_default("d1");
        c.w.dispatch_with("d1", &req).ok();
        let charge = one(c.w.charges());
        assert!(
            !charge.get("possibly_incomplete").is_some_and(truthy),
            "{charge}"
        );
        assert!(!truthy(&c.w.attempts()[0]["possibly_incomplete"]));
    }

    #[test]
    fn test_wrong_branch_is_not_committed() {
        // Covers R40.
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.w.git(&["checkout", "--quiet", "-b", "somewhere-else"]);
        c.edits_default("d1");
        c.w.dispatch_with("d1", &req).ok();
        let saved = c.w.result("d1");
        assert_json_eq!(
            pick(&saved, &["status", "blocked_by"]),
            jv(r#"["BLOCKED", "environment"]"#)
        );
        assert!(saved["committed"].is_null(), "{saved}");
        assert!(
            saved["commit_skipped"]
                .as_str()
                .is_some_and(|t| t.contains("somewhere-else")),
            "{saved}"
        );
        assert_eq!(c.head(), c.base_sha);
        let status = c.w.git(&["status", "--porcelain"]);
        assert!(status.contains("src/"), "{status}");
    }

    #[test]
    fn test_operation_in_progress_is_not_committed() {
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        let merge_head = PathBuf::from(c.w.git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "MERGE_HEAD",
        ]));
        fs::write(&merge_head, format!("{}\n", c.base_sha)).unwrap();
        c.edits_default("d1");
        c.w.dispatch_with("d1", &req).ok();
        let saved = c.w.result("d1");
        assert!(saved["committed"].is_null(), "{saved}");
        assert!(
            saved["commit_skipped"]
                .as_str()
                .is_some_and(|t| t.contains("in progress")),
            "{saved}"
        );
        assert_eq!(c.head(), c.base_sha);
    }

    #[test]
    fn test_check_dispatch_is_not_committed() {
        let mut c = Commits::new();
        c.edits_default("r1");
        c.w.dispatch_with(
            "r1",
            &Req {
                kind: Some("review"),
                branch: Some(CARD_BRANCH),
                ..Req::default()
            },
        )
        .ok();
        assert!(c.w.result("r1")["committed"].is_null());
        assert_eq!(c.head(), c.base_sha);
    }

    #[test]
    fn test_cli_passes_kind_and_branch() {
        let mut c = Commits::new();
        let req = c.commit_request("d1");
        c.edits_default("d1");
        c.w.dispatch_with("d1", &req).ok();
        let request = c.w.read("d1", "request.json").unwrap();
        assert_json_eq!(
            pick(&request, &["kind", "branch"]),
            Json::from(&["implement", CARD_BRANCH][..])
        );
        assert_eq!(c.w.result("d1")["committed"], c.head().as_str());
    }
}
