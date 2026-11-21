//! Launcher scenarios: `run`, which owns a run and starts its Claude Code supervisor, and
//! `launch-line`, which prints the command that does.
//!
//! The Rust form of `tests/contract/contract_launch.py` (converted from tests/test_launch.py,
//! with the self-path hook of tests/test_test_hooks.py). Every scenario runs on a run that
//! `start` made (one card, a worktree workspace, the supervisor route `sonnet low` in the
//! run's config.json), with the Rust fake `claude` as the `claude` found on PATH: the world is
//! `common::launch_world::World`. It reads what the launch leaves: the fake's log (its argv,
//! working directory, LEADHAND_* environment, pid and parent, and the signals it got),
//! holder.json, events.jsonl, run.lock (by taking the flock itself), exit codes and stderr.
//!
//! Spec contract item 11 (the `claude` argument order, environment, working directory, first
//! message and settings overlay) and item 12 (every lock is flock(2) on the same files): a
//! test that holds run.lock holds it with flock, as another leadhand process would.
//!
//! Where the binary passes or prints its own path (LEADHAND_CLI, the first message, the
//! sandbox exclusion, the launch line) the path is compared by resolved file (ruling S1-R11);
//! the rest of the text exactly. `launch-line` prints bare `leadhand` in Rust when the
//! `leadhand` on PATH is the binary itself, so the world's PATH holds no such `leadhand`.
//!
//! Tests run in parallel (S3-R19). The tests whose assertions rest on wall-clock budgets hold
//! `common::serial()` for their whole run, each naming its group in a comment: the 3 s
//! supervisor that must outlive its killed launcher while the test relaunches, signal
//! forwarding, and keep-awake beside a 1 s supervisor.

#[macro_use]
mod common;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use common::json::Json;
use common::launch_world::{
    AGENT, FIRST_MESSAGE_END, LOGGING_TOOL, RunOpts, SPACES_WARNING_TAIL, WAIT, World,
};
use common::{Out, names_bin, names_bin_between, realpath, shlex_join};

/// The first message names the run and the binary (by resolved file), then the skill.
fn first_message_ok(text: &str, run_dir: &Path) -> bool {
    names_bin_between(
        text,
        &format!(
            "Run the leadhand deck in {}. leadhand CLI: ",
            run_dir.display()
        ),
        FIRST_MESSAGE_END,
    )
}

/// The settings overlay is `{"sandbox": {"excludedCommands": ["<the binary> *"]}}`.
#[track_caller]
fn settings_ok(settings_json: &str) {
    let settings = common::jv(settings_json);
    assert_eq!(settings.keys(), ["sandbox"]);
    assert_eq!(settings["sandbox"].keys(), ["excludedCommands"]);
    let entries = settings["sandbox"]["excludedCommands"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(
        names_bin_between(entries[0].as_str().unwrap(), "", " *"),
        "{entries:?}"
    );
}

/// argv[0] is the `claude` found on PATH: the fake.
#[track_caller]
fn claude_ok(w: &World, argv0: &str) {
    assert_eq!(
        realpath(Path::new(argv0)),
        realpath(&w.sup.env.fakes().join("claude"))
    );
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

fn os(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn path_str(p: &Path) -> &str {
    p.to_str().unwrap()
}

// --------------------------------------------------------------------------
// argv, settings, first message

mod argv {
    use super::*;

    /// Without --plugin-dir: the settings overlay, the supervisor agent, the route's model
    /// and effort, auto mode, then the first message. Covers P1.
    #[test]
    fn test_argv_order_and_auto_mode() {
        let w = World::new();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        let argv = w.argv(1);
        claude_ok(&w, &argv[0]);
        let (settings, last) = (argv[2].as_str(), argv.last().unwrap().as_str());
        assert_eq!(
            argv[1..],
            s(&[
                "--settings",
                settings,
                "--agent",
                AGENT,
                "--model",
                "sonnet",
                "--effort",
                "low",
                "--permission-mode",
                "auto",
                last
            ])
        );
        settings_ok(settings);
        assert!(first_message_ok(last, &w.run_dir), "{last}");
    }

    /// A run whose configuration snapshot (config.json) has no supervisor role: the bare
    /// argv, with neither --model nor --effort.
    #[test]
    fn test_no_supervisor_route_means_no_model_or_effort() {
        let w = World::new();
        let config = w.run_dir.join("config.json");
        let mut data = common::read_json(&config);
        let Json::Obj(roles) = common::dispatch_world::at(&mut data, &["roles"]) else {
            panic!("config.json has no roles object");
        };
        let before = roles.len();
        roles.retain(|(k, _)| k != b"supervisor");
        assert!(roles.len() < before, "config.json has no supervisor role"); // Python's `del` raises
        fs::write(&config, common::json::dumps_indent(&data, 2) + "\n").unwrap();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        let argv = w.argv(1);
        claude_ok(&w, &argv[0]);
        let (settings, last) = (argv[2].as_str(), argv.last().unwrap().as_str());
        assert_eq!(
            argv[1..],
            s(&[
                "--settings",
                settings,
                "--agent",
                AGENT,
                "--permission-mode",
                "auto",
                last
            ])
        );
        settings_ok(settings);
        assert!(first_message_ok(last, &w.run_dir), "{last}");
    }

    /// Covers P1 and P5 (the settings overlay's CLI exclusion).
    #[test]
    fn test_run_passes_supervisor_model_effort_and_cli_settings() {
        let w = World::new();
        let plugin = w.tmp.join("plugin dir");
        let r = w.run(&os_args!["--plugin-dir", plugin]);
        assert_eq!(r.code, 0, "{r}");
        let argv = w.argv(1);
        claude_ok(&w, &argv[0]);
        let i = argv
            .iter()
            .position(|a| a == "--settings")
            .expect("no --settings");
        let settings = argv[i + 1].as_str();
        settings_ok(settings);
        assert_eq!(
            argv[1..],
            s(&[
                "--plugin-dir",
                path_str(&plugin),
                "--settings",
                settings,
                "--agent",
                AGENT,
                "--model",
                "sonnet",
                "--effort",
                "low",
                "--permission-mode",
                "auto",
                argv.last().unwrap()
            ])
        );
    }

    #[test]
    fn test_print_mode_adds_headless_stream_flags_before_the_prompt() {
        let w = World::new();
        let r = w.run(&os_args!["--print"]);
        assert_eq!(r.code, 0, "{r}");
        let argv = w.argv(1);
        let (settings, last) = (argv[2].as_str(), argv.last().unwrap().as_str());
        assert_eq!(
            argv[1..],
            s(&[
                "--settings",
                settings,
                "--agent",
                AGENT,
                "--model",
                "sonnet",
                "--effort",
                "low",
                "--permission-mode",
                "auto",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                last
            ])
        );
        assert!(first_message_ok(last, &w.run_dir), "{last}");
    }

    #[test]
    fn test_print_option_is_hidden_and_off_by_default() {
        let w = World::new();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert!(!w.argv(1).iter().any(|a| a == "-p"));
        assert!(!w.argv(1).iter().any(|a| a == "--output-format"));
        let h = common::cli(&os_args!["run", "--help"], &w.env, &w.elsewhere).ok();
        assert!(h.stdout_text().contains("--plugin-dir"), "{h}");
        assert!(!h.stdout_text().contains("--print"), "{h}");
    }

    #[test]
    fn test_first_message_names_skill_invocation() {
        let w = World::new();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        let last = w.argv(1).last().unwrap().clone();
        assert!(first_message_ok(&last, &w.run_dir), "{last}");
    }

    #[test]
    fn test_attended_use_warning_printed() {
        let w = World::new();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert!(r.stderr_text().contains("attended"), "{r}");
        assert!(r.stderr_text().contains("excludedCommands"), "{r}");
    }

    /// Covers R28.
    #[test]
    fn test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match() {
        let w = World::new();
        // The CLI's own path, set through the testing hook as the Python test patched CLI_PATH.
        let r = w.run_with(
            &[],
            RunOpts {
                env: &[("LEADHAND_TEST_CLI", "/opt/lead hand/scripts/leadhand")],
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 0, "{r}");
        assert!(r.stderr_text().contains(SPACES_WARNING_TAIL), "{r}");
        let argv = w.argv(1);
        let i = argv.iter().position(|a| a == "--settings").unwrap();
        assert_json_eq!(
            common::jv(&argv[i + 1]),
            common::jv(
                r#"{"sandbox": {"excludedCommands": ["/opt/lead hand/scripts/leadhand *"]}}"#
            )
        );
        let r = w.run_with(
            &[],
            RunOpts {
                env: &[("LEADHAND_TEST_CLI", "/opt/leadhand/scripts/leadhand")],
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 0, "{r}");
        assert!(!r.stderr_text().contains("may not match"), "{r}");
        let argv = w.argv(2);
        let i = argv.iter().position(|a| a == "--settings").unwrap();
        assert_json_eq!(
            common::jv(&argv[i + 1]),
            common::jv(
                r#"{"sandbox": {"excludedCommands": ["/opt/leadhand/scripts/leadhand *"]}}"#
            )
        );
    }
}

// --------------------------------------------------------------------------
// run dir, cwd, environment, exit code

mod child {
    use super::*;

    #[test]
    fn test_supervisor_cwd_is_workspace_when_launched_elsewhere() {
        let w = World::new();
        let r = w.run_with(
            &[],
            RunOpts {
                target: Some(&w.paths.deck),
                cwd: Some(&w.elsewhere),
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 0, "{r}");
        let start = &w.starts()[0];
        assert_eq!(
            realpath(Path::new(&start.rec["cwd"].as_os().unwrap())),
            realpath(&w.workspace)
        );
        let last = start.argv().last().unwrap().clone();
        assert!(
            last.contains(&format!(
                "Run the leadhand deck in {}.",
                w.run_dir.display()
            )),
            "{last}"
        );
    }

    /// Covers P3 (LEADHAND_RUN_ID reaches the supervisor's session).
    #[test]
    fn test_env_has_run_id_dir_and_cli() {
        let w = World::new();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        let env = w.starts()[0].rec["env"].clone();
        assert_eq!(
            (&env["LEADHAND_RUN_ID"], &env["LEADHAND_RUN_DIR"]),
            (
                &Json::from(w.run_id.as_str()),
                &Json::from(path_str(&w.run_dir))
            )
        );
        let cli = env["LEADHAND_CLI"].as_str().unwrap_or_default();
        assert!(!cli.is_empty() && names_bin(cli), "{env}");
    }

    /// Covers R24.
    #[test]
    fn test_run_accepts_deck_md_path() {
        let w = World::new();
        let r = w.run_with(
            &[],
            RunOpts {
                target: Some(&w.paths.deck),
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(
            w.starts()[0].rec["env"]["LEADHAND_RUN_DIR"],
            path_str(&w.run_dir)
        );
    }

    #[test]
    fn test_not_a_run_is_an_error_and_starts_nothing() {
        let w = World::new();
        let r = w.run_with(
            &[],
            RunOpts {
                target: Some(&w.elsewhere),
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().contains("not a run"), "{r}");
        assert!(w.starts().is_empty());
    }

    #[test]
    fn test_child_exit_code_returned() {
        let w = World::new();
        let r = w.run_with(
            &[],
            RunOpts {
                env: &[("FAKE_CLAUDE_EXIT", "7")],
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 7, "{r}");
        assert_eq!(w.starts().len(), 1);
    }

    #[test]
    fn test_a_worktree_run_launch_is_refused_while_the_repository_is_sparse() {
        let w = World::new();
        common::git::commit(&w.repo, &[("far/away.txt", "far\n")], "far");
        common::git::run_git(
            &w.repo,
            &["sparse-checkout", "set", "--no-cone", "/README.md"],
        );
        let r = w.run(&[]);
        assert_eq!(r.code, 1, "{r}");
        assert!(
            r.stderr_text()
                .contains("sparse checkouts are not supported"),
            "{r}"
        );
        assert!(w.starts().is_empty());
    }
}

// --------------------------------------------------------------------------
// launch-line

mod launch_line {
    use super::*;

    /// The one line a `launch-line` printed, without its newline.
    #[track_caller]
    fn line(r: Out) -> String {
        let r = r.ok();
        let out = r.stdout_text();
        assert!(out.ends_with('\n') && out.matches('\n').count() == 1, "{r}");
        out[..out.len() - 1].to_string()
    }

    #[test]
    fn test_launch_line_quotes_paths_with_spaces() {
        let w = World::new();
        let plugin = w.tmp.join("plugin dir");
        let deck = path_str(&w.paths.deck).to_string();
        let l = line(w.launch_line(&os_args![w.run_dir, "--plugin-dir", plugin]));
        assert!(
            common::is_bin_command(
                &l,
                "",
                &format!(
                    " {}",
                    shlex_join(&os(&["run", &deck, "--plugin-dir", path_str(&plugin)]))
                )
            ),
            "{l}"
        );
        let l = line(w.launch_line(&os_args![deck]));
        assert!(
            common::is_bin_command(&l, "", &format!(" {}", shlex_join(&os(&["run", &deck])))),
            "{l}"
        );
    }

    #[test]
    fn test_launch_line_defaults_to_recorded_plugin_dir() {
        let w = World::new();
        let plugin = w.tmp.join("dev plugin");
        let deck = path_str(&w.paths.deck).to_string();
        let r = w.run(&os_args!["--plugin-dir", plugin]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.holder()["plugin_dir"], path_str(&plugin));
        let l = line(w.launch_line(&os_args![w.run_dir]));
        assert!(
            common::is_bin_command(
                &l,
                "",
                &format!(
                    " {}",
                    shlex_join(&os(&["run", &deck, "--plugin-dir", path_str(&plugin)]))
                )
            ),
            "{l}"
        );
        let other = w.tmp.join("other plugin");
        let l = line(w.launch_line(&os_args![deck, "--plugin-dir", other]));
        assert!(
            common::is_bin_command(
                &l,
                "",
                &format!(
                    " {}",
                    shlex_join(&os(&["run", &deck, "--plugin-dir", path_str(&other)]))
                )
            ),
            "{l}"
        );
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert!(w.holder().get("plugin_dir").is_none(), "{}", w.holder());
        assert_eq!(w.holder()["supervisor_pid"], i64::from(w.starts()[1].pid));
    }
}

/// The testing hook LEADHAND_TEST_CLI (converted from tests/test_test_hooks.py): under
/// LEADHAND_TESTING=1 it takes the place of the binary's own path wherever the binary names
/// itself; without LEADHAND_TESTING=1 it is ignored.
mod self_path_hook {
    use super::*;

    const ROUTER: &str = "/x/router";

    /// Covers limit L4 (test hooks need LEADHAND_TESTING=1).
    #[test]
    fn test_self_path_override_is_honoured_only_under_leadhand_testing() {
        let w = World::new();
        let deck = path_str(&w.paths.deck).to_string();
        for (testing, honoured) in [("1", true), ("0", false)] {
            let env = w
                .env
                .with("LEADHAND_TESTING", testing)
                .with("LEADHAND_TEST_CLI", ROUTER);
            let r = common::cli(&os_args!["launch-line", w.run_dir], &env, &w.elsewhere).ok();
            let words = common::shlex_split(&r.stdout_text()).expect("a shell line");
            assert_eq!(words[1..], s(&["run", &deck]), "testing={testing}: {r}");
            if honoured {
                assert_eq!(words[0], ROUTER, "testing={testing}: {r}");
            } else {
                assert!(names_bin(&words[0]), "testing={testing}: {r}");
            }
        }
        // Without the hook, under LEADHAND_TESTING=1, the binary names itself.
        let line = w.launch_line(&os_args![w.run_dir]).ok().stdout_text();
        assert!(
            common::is_bin_command(
                &line[..line.len() - 1],
                "",
                &format!(" {}", shlex_join(&os(&["run", &deck])))
            ),
            "{line}"
        );
    }

    #[test]
    fn test_launch_and_pause_name_the_override() {
        let w = World::new();
        let r = w.run_with(
            &[],
            RunOpts {
                env: &[("LEADHAND_TEST_CLI", ROUTER)],
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 0, "{r}");
        let start = &w.starts()[0];
        let argv = start.argv();
        assert_eq!(
            argv.last().unwrap(),
            &format!(
                "Run the leadhand deck in {}. leadhand CLI: {ROUTER}{FIRST_MESSAGE_END}",
                w.run_dir.display()
            )
        );
        let i = argv.iter().position(|a| a == "--settings").unwrap();
        assert_json_eq!(
            common::jv(&argv[i + 1]),
            Json::object([(
                "sandbox",
                Json::object([("excludedCommands", Json::from(&[format!("{ROUTER} *")][..]))])
            )])
        );
        assert_eq!(start.rec["env"]["LEADHAND_CLI"], ROUTER);
        let r = common::cli(
            &os_args!["pause", w.run_dir],
            &w.env.with("LEADHAND_TEST_CLI", ROUTER),
            &w.elsewhere,
        )
        .ok();
        assert!(
            r.stdout_text().trim_end_matches('\n').ends_with(&format!(
                "Continue it later with: {ROUTER} run {}",
                common::shlex_quote(path_str(&w.run_dir))
            )),
            "{r}"
        );
    }
}

// --------------------------------------------------------------------------
// ownership: run.lock and holder.json

mod ownership {
    use super::*;
    use common::launch_world::{holder_of, now, snapshot, wait_for};
    use common::{lockable, started_by};
    use rustix::process::Signal;

    #[test]
    fn test_refused_launch_changes_nothing() {
        let mut w = World::new();
        w.progress();
        let first = w.launcher();
        let sup = w.wait_supervisor(1, Some(&first.proc));
        assert_eq!(w.events("resumed").len(), 1);
        let before = snapshot(&w.run_dir);
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert!(
            r.stderr_text().contains(&format!(
                "Run already in progress: supervisor pid {} since ",
                sup.pid
            )),
            "{r}"
        );
        assert!(
            r.stderr_text().contains(&format!("(run {})", w.run_id)),
            "{r}"
        );
        assert_eq!(snapshot(&w.run_dir), before);
        assert_eq!(w.starts().len(), 1);
        assert!(World::alive(&sup));
        World::stop(&first, Signal::TERM);
        w.finish(&first, WAIT);
    }

    #[test]
    fn test_cli_run_refusal_exits_3() {
        let mut w = World::new();
        let first = w.launcher();
        let sup = w.wait_supervisor(1, Some(&first.proc));
        let r = w.run_with(
            &[],
            RunOpts {
                target: Some(&w.paths.deck),
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 3, "{r}");
        assert!(
            r.stderr_text().contains(&format!(
                "Run already in progress: supervisor pid {}",
                sup.pid
            )),
            "{r}"
        );
        World::stop(&first, Signal::TERM);
        w.finish(&first, WAIT);
    }

    #[test]
    fn test_lock_held_by_supervisor_after_launcher_killed() {
        let _serial = common::serial(); // group: the 3 s supervisor outliving its launcher
        let mut w = World::new();
        let launcher = w.launcher_with(None, "3", &[]);
        let sup = w.wait_supervisor(1, Some(&launcher.proc));
        launcher.proc.kill();
        launcher.wait(WAIT);
        assert!(World::alive(&sup), "fake supervisor died early");
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert!(
            r.stderr_text()
                .contains(&format!("supervisor pid {}", sup.pid)),
            "{r}"
        );
        w.wait_gone(&sup);
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.starts().len(), 2);
    }

    #[test]
    fn test_lock_fd_alone_blocks_takeover() {
        let _serial = common::serial(); // group: the 3 s supervisor outliving its launcher
        // holder.json names a dead supervisor, but run.lock is still flocked by a live
        // process: the supervisor, through the descriptor it inherited from its killed
        // launcher.
        let mut w = World::new();
        let launcher = w.launcher_with(None, "3", &[]);
        let sup = w.wait_supervisor(1, Some(&launcher.proc));
        launcher.proc.kill();
        launcher.wait(WAIT);
        w.write_holder(&w.holder_with("supervisor_start", "not it".into()));
        assert!(World::alive(&sup), "fake supervisor died early");
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert_eq!(w.starts().len(), 1);
        w.wait_gone(&sup);
    }

    /// Covers limit L3: a relaunch is refused while the recorded supervisor lives.
    #[test]
    fn test_recorded_live_supervisor_blocks_takeover() {
        let mut w = World::new();
        fs::write(&w.paths.lock, "").unwrap(); // stale: nobody holds the flock
        let env = w.env.clone();
        let live = w.popen(&os(&["sleep", "60"]), &env);
        let start = wait_for("the live process", None, || started_by(live.pid(), now()));
        w.write_holder(&Json::object([
            ("run_id", Json::from(w.run_id.as_str())),
            ("launcher_pid", Json::from(1)),
            ("supervisor_pid", Json::from(live.pid())),
            ("supervisor_start", Json::from(start.as_str())),
            ("started_at", Json::from("2026-11-10T00:00:00.000Z")),
        ]));
        w.progress();
        let before = snapshot(&w.run_dir);
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert!(
            r.stderr_text().contains(&format!(
                "Run already in progress: supervisor pid {} since 2026-11-10T00:00:00.000Z (run {})",
                live.pid(),
                w.run_id
            )),
            "{r}"
        );
        assert_eq!(snapshot(&w.run_dir), before);
        assert!(w.starts().is_empty());

        // The same pid with another start time is a reused pid, not the supervisor.
        w.write_holder(&w.holder_with("supervisor_start", "other".into()));
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.starts().len(), 1);
    }

    #[test]
    fn test_resumed_appended_only_after_acquire() {
        // While another process holds run.lock the launch is refused and appends nothing;
        // once it owns the run, it appends `resumed`, once.
        let mut w = World::new();
        w.progress();
        let lock = w.paths.lock.clone();
        let held = w.hold(&lock);
        let kinds = w.kinds();
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert_eq!(w.kinds(), kinds);
        assert!(w.starts().is_empty());
        w.release(held);
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        let resumed = w.events("resumed");
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0]["run_id"], w.run_id.as_str());
        let mut want = kinds.clone();
        want.push("resumed".to_string());
        assert_eq!(w.kinds(), want);
    }

    #[test]
    fn test_first_launch_of_a_fresh_run_appends_no_resumed() {
        let w = World::new();
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.starts().len(), 1);
        assert_eq!(w.kinds(), ["run_started"]);
    }

    /// Covers R7: the recorded start time is the process table's, read without `ps`.
    #[test]
    fn test_holder_records_launcher_and_supervisor() {
        let mut w = World::new();
        let launcher = w.launcher();
        let sup = w.wait_supervisor(1, Some(&launcher.proc));
        let h = w.holder();
        assert_eq!(h["run_id"], w.run_id.as_str());
        assert_eq!(h["launcher_pid"], i64::from(launcher.pid()));
        assert_eq!(h["supervisor_pid"], i64::from(sup.pid));
        // the process table's, via the fake's log
        assert_eq!(h["supervisor_start"].as_str(), sup.start.as_deref());
        assert!(
            h["started_at"].as_str().is_some_and(|t| !t.is_empty()),
            "{h}"
        );
        World::stop(&launcher, Signal::TERM);
        w.finish(&launcher, WAIT);
    }

    #[test]
    fn test_signals_forwarded_to_child() {
        let _serial = common::serial(); // group: signal forwarding
        let mut w = World::new();
        for sig in [Signal::TERM, Signal::INT] {
            let n = w.starts().len() + 1;
            let launcher = w.launcher();
            let sup = w.wait_supervisor(n, Some(&launcher.proc));
            World::stop(&launcher, sig);
            let r = w.finish(&launcher, WAIT);
            assert_eq!(r.code, 128 + sig.as_raw(), "{sig:?}: {r}");
            assert_eq!(w.signals(sup.pid), [i64::from(sig.as_raw())], "{sig:?}");
            w.wait_gone(&sup);
        }
        // The lock was released: a new launch owns the run.
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
    }

    #[test]
    fn test_busy_lock_with_dead_recorded_supervisor_says_not_running() {
        let mut w = World::new();
        let env = w.env.clone();
        let gone = w.popen(&os(&["true"]), &env);
        gone.wait(WAIT);
        w.write_holder(&Json::object([
            ("run_id", Json::from(w.run_id.as_str())),
            ("launcher_pid", Json::from(gone.pid())),
            ("supervisor_pid", Json::from(gone.pid())),
            ("supervisor_start", Json::from("long gone")),
            ("started_at", Json::from("2026-11-10T00:00:00.000Z")),
        ]));
        let lock = w.paths.lock.clone();
        w.hold(&lock);
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert!(
            r.stderr_text().contains(&format!(
                "lock held by another process (supervisor pid {} recorded, not running)",
                gone.pid()
            )),
            "{r}"
        );
        assert!(!r.stderr_text().contains("since"), "{r}");
        assert!(w.starts().is_empty());
    }

    #[test]
    fn test_another_runs_live_supervisor_does_not_hold_back_a_launch() {
        // The worktree counterpart of the in-place tests where another run of the same
        // repository was being supervised: ownership is per run, so a second worktree run of
        // the repository launches while the first run's supervisor lives and holds its own
        // run.lock.
        let mut w = World::new();
        let other = w.sup.start_new();
        assert_ne!(other, w.run_dir);
        let first = w.launcher();
        let sup = w.wait_supervisor(1, Some(&first.proc));
        let r = w.run_with(
            &[],
            RunOpts {
                target: Some(&other),
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 0, "{r}");
        let second = w.starts()[1].clone();
        assert_eq!(second.rec["env"]["LEADHAND_RUN_DIR"], path_str(&other));
        assert_eq!(holder_of(&other)["supervisor_pid"], i64::from(second.pid));
        assert!(World::alive(&sup));
        assert!(!lockable(&w.paths.lock)); // still the first run's
        World::stop(&first, Signal::TERM);
        w.finish(&first, WAIT);
    }
}

// --------------------------------------------------------------------------
// keep-awake beside the supervisor

mod keep_awake {
    use super::*;
    use common::{read_jsonl, started_by};

    /// Python's `skipUnless(darwin or linux)`: true (after saying so) when the test skips.
    fn unsupported() -> bool {
        let skip = !(cfg!(target_os = "macos") || cfg!(target_os = "linux"));
        if skip {
            println!("skipped: keep-awake runs only on macOS and Linux");
        }
        skip
    }

    /// macOS: `caffeinate -i -w <supervisor pid>`; Linux: `systemd-inhibit --what=idle
    /// --why='leadhand run' tail --pid=<supervisor pid> -f /dev/null`. Either is a child of
    /// the launcher beside the supervisor, and is gone when the launch returns. Covers R27.
    #[test]
    fn test_keep_awake_runs_beside_claude_which_stays_the_direct_child() {
        if unsupported() {
            return;
        }
        let _serial = common::serial(); // group: keep-awake beside a 1 s supervisor
        let mut w = World::new();
        let mine = if cfg!(target_os = "macos") {
            "caffeinate"
        } else {
            "systemd-inhibit"
        };
        for name in ["caffeinate", "systemd-inhibit"] {
            w.tool(name, LOGGING_TOOL);
        }
        let launcher = w.launcher_with(None, "1", &[]);
        let r = w.finish(&launcher, WAIT);
        assert_eq!(r.code, 0, "{r}");
        let start = w.starts()[0].clone();
        claude_ok(&w, &start.argv()[0]);
        assert_eq!(start.rec["ppid"], i64::from(launcher.pid()));
        assert_eq!(w.holder()["supervisor_pid"], i64::from(start.pid));
        let recs = read_jsonl(&w.keep_awake_log);
        let names: Vec<String> = recs
            .iter()
            .map(|r| {
                Path::new(r["argv"][0].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, [mine]);
        let rec = &recs[0];
        assert_eq!(rec["ppid"], i64::from(launcher.pid()));
        let args = &common::launch_world::strings(&rec["argv"])[1..];
        if cfg!(target_os = "macos") {
            assert_eq!(args, s(&["-i", "-w", &start.pid.to_string()]));
        } else {
            assert_eq!(
                args,
                s(&[
                    "--what=idle",
                    "--why=leadhand run",
                    "tail",
                    &format!("--pid={}", start.pid),
                    "-f",
                    "/dev/null"
                ])
            );
        }
        assert!(
            started_by(
                rec["pid"].as_i64().unwrap() as i32,
                rec["time"].as_f64().unwrap()
            )
            .is_none(),
            "keep-awake left running"
        );
    }

    /// The other platform's tool on PATH is never started (macOS ignores systemd-inhibit).
    #[test]
    fn test_only_the_platforms_keep_awake_tool_is_used() {
        if unsupported() {
            return;
        }
        let w = World::new();
        let other = if cfg!(target_os = "macos") {
            "systemd-inhibit"
        } else {
            "caffeinate"
        };
        w.tool(other, LOGGING_TOOL);
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.starts().len(), 1);
        assert!(!w.keep_awake_log.exists());
    }
}

// --------------------------------------------------------------------------
// R41(a): CLI children of the supervisor drop the inherited run.lock descriptor

mod inherited_lock {
    use super::*;
    use common::launch_world::{Bg, PASSED_FD, wait_for, write_big};
    use common::{hold_lock, lockable, started_by};
    use rustix::process::{Pid, Signal, kill_process};
    use std::fs::File;
    use std::path::PathBuf;

    /// Far more than a pipe holds: writing it all means it was read.
    fn big() -> Vec<u8> {
        vec![b'x'; 1 << 20]
    }

    /// A dispatch recorded as started, on the world's run the first time (`used` false), then
    /// on a new run of the deck each time (one result each).
    #[track_caller]
    fn started_dispatch(w: &mut World, used: &mut bool) -> (PathBuf, String) {
        if *used {
            w.sup.start_new();
        }
        *used = true;
        let run_dir = w.sup.run_dir.clone().unwrap();
        let did = w.progress();
        w.sup
            .cli(&os_args![
                "record",
                run_dir,
                "started",
                "--dispatch",
                did,
                "--worker",
                "fake-agent"
            ])
            .ok();
        (run_dir, did)
    }

    /// `record RUN result --dispatch ID --reply -` for a started dispatch, started with a
    /// descriptor on `path` that holds its exclusive flock, inherited and named in
    /// LEADHAND_LOCK_FD (and LEADHAND_RUN_DIR when `run_dir`). The test's own copy is
    /// closed, so the command holds the only one. Returns once the command has read most of
    /// its reply from stdin, so it is past its startup; it then waits for the rest.
    #[track_caller]
    fn holding(w: &mut World, used: &mut bool, path: &Path, run_dir: Option<&Path>) -> Bg {
        let (target, did) = started_dispatch(w, used);
        let file: File = hold_lock(path);
        let mut env = w.env.with("LEADHAND_LOCK_FD", PASSED_FD.to_string());
        if let Some(run_dir) = run_dir {
            env.set("LEADHAND_RUN_DIR", run_dir);
        }
        let argv: Vec<OsString> = vec![
            common::bin().into(),
            "record".into(),
            target.into(),
            "result".into(),
            "--dispatch".into(),
            did.into(),
            "--worker".into(),
            "fake-agent".into(),
            "--reply".into(),
            "-".into(),
        ];
        let mut p = w.popen_with(&argv, &env, Some(file), true);
        let mut stdin = p.stdin.take().unwrap();
        write_big(
            &mut stdin,
            &big(),
            &p.proc,
            "the command to read its reply from stdin",
        );
        p.stdin = Some(stdin);
        assert!(
            p.proc.poll().is_none(),
            "the command exited before its stdin was closed"
        );
        p
    }

    /// Closes the command's stdin; it records the result.
    #[track_caller]
    fn done(w: &World, mut p: Bg) {
        drop(p.stdin.take());
        assert_eq!(w.finish(&p, WAIT).ok().json()["kind"], "result_recorded");
    }

    /// Opens `fifo` for writing once a reader has it open (polled while `proc` runs) and
    /// writes BIG into it; returns the open writer. Once the write returns, the reader has
    /// read most of it, so it is past its startup; it then waits for the rest.
    #[track_caller]
    fn feed(fifo: &Path, proc: &common::supervisor::Proc) -> File {
        use rustix::fs::{Mode, OFlags};
        let fd = wait_for("the CLI child to open its stdin", Some(proc), || {
            match rustix::fs::open(
                fifo,
                OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => Some(fd),
                Err(rustix::io::Errno::NXIO) => None, // no reader yet
                Err(e) => panic!("opening {}: {e}", fifo.display()),
            }
        });
        let mut writer = File::from(fd);
        write_big(
            &mut writer,
            &big(),
            proc,
            "the CLI child to read its reply from the FIFO",
        );
        writer
    }

    /// Covers limit L3 (the inherited run lock) and R41(a).
    #[test]
    fn test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs() {
        let mut w = World::new();
        let mut used = false;
        let (run_dir, did) = started_dispatch(&mut w, &mut used);
        let exported = w.tmp.join("lock fd env");
        let fifo = w.tmp.join("reply fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo failed");
        // The supervisor's long-lived CLI command (`record ... --reply -`, reading its reply
        // from a pipe the test writes), started the way Claude Code starts a Bash command:
        // inheriting every descriptor.
        let spawn = common::json::dumps(&Json::from(
            &[
                "/bin/sh",
                "-c",
                "printf %s \"${LEADHAND_LOCK_FD-}\" > \"$0\"; \
                 exec \"$1\" record \"$2\" result --dispatch \"$3\" --worker fake-agent --reply - < \"$4\"",
                path_str(&exported),
                path_str(&common::bin()),
                path_str(&run_dir),
                did.as_str(),
                path_str(&fifo),
            ][..],
        ));
        let launcher = w.launcher_with(None, "60", &[("FAKE_CLAUDE_SPAWN", &spawn)]);
        let sup = w.wait_supervisor(1, Some(&launcher.proc));
        let rec = wait_for("the CLI child to run", None, || {
            w.records()
                .into_iter()
                .find(|r| r.get("spawned").is_some_and(common::supervisor::truthy))
        });
        let child_pid = rec["spawned"].as_i64().unwrap() as i32;
        let child = common::launch_world::Start {
            rec: Json::Null,
            pid: child_pid,
            start: started_by(child_pid, rec["time"].as_f64().unwrap()),
        };
        assert!(child.start.is_some(), "the CLI child exited at once");
        let writer = feed(&fifo, &launcher.proc);
        let text = fs::read_to_string(&exported).unwrap();
        assert!(
            !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
            "launcher did not export LEADHAND_LOCK_FD"
        );
        // The CLI child is past its startup. It closed its copy of the descriptor and never
        // unlocked it: the lock is shared with the launcher and the supervisor, so while they
        // live the run stays locked and a second launch is refused.
        assert!(World::alive(&sup) && World::alive(&child) && launcher.proc.poll().is_none());
        assert!(
            !lockable(&w.paths.lock),
            "a CLI child unlocked the live supervisor's run.lock"
        );
        let r = w.run(&[]);
        assert_eq!(r.code, 3, "{r}");
        assert_eq!(w.starts().len(), 1);
        launcher.proc.kill();
        launcher.wait(WAIT);
        let _ = kill_process(Pid::from_raw(sup.pid).unwrap(), Signal::KILL);
        w.wait_gone(&sup);
        // The supervisor is gone; its CLI child is not, and does not keep the run locked.
        assert!(World::alive(&child), "the CLI child exited early");
        assert!(lockable(&w.paths.lock), "the CLI child kept the run locked");
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert!(World::alive(&child));
        drop(writer); // EOF: the CLI child records the result and exits
        w.wait_gone(&child);
    }

    /// Covers limit L3 and R41(a).
    #[test]
    fn test_cli_closes_inherited_run_lock_fd() {
        let mut w = World::new();
        let mut used = false;
        let (lock, run_dir) = (w.paths.lock.clone(), w.run_dir.clone());
        let p = holding(&mut w, &mut used, &lock, Some(&run_dir));
        assert!(
            lockable(&w.paths.lock),
            "the command kept the inherited run.lock descriptor"
        );
        assert!(p.proc.poll().is_none());
        done(&w, p);
    }

    #[test]
    fn test_cli_leaves_an_unrelated_fd_open() {
        let mut w = World::new();
        let mut used = false;
        let other = w.tmp.join("unrelated.txt");
        fs::write(&other, "x").unwrap();
        let run_dir = w.run_dir.clone();
        let p = holding(&mut w, &mut used, &other, Some(&run_dir));
        assert!(
            !lockable(&other),
            "the command closed an unrelated descriptor"
        );
        done(&w, p);
        assert!(lockable(&other));
        // A run.lock descriptor, but no LEADHAND_RUN_DIR to verify it against: left open too.
        let lock = w.paths.lock.clone();
        let p = holding(&mut w, &mut used, &lock, None);
        assert!(
            !lockable(&w.paths.lock),
            "the command closed a descriptor it could not verify"
        );
        done(&w, p);
        assert!(lockable(&w.paths.lock));
        // Not a number, or not open: ignored.
        let version = common::cli(&os_args!["version"], &w.env, &w.elsewhere)
            .ok()
            .stdout;
        assert!(!version.is_empty());
        for value in ["-1", "nope", "987"] {
            let env = w
                .env
                .with("LEADHAND_LOCK_FD", value)
                .with("LEADHAND_RUN_DIR", &w.run_dir);
            assert_eq!(
                common::cli(&os_args!["version"], &env, &w.elsewhere)
                    .ok()
                    .stdout,
                version,
                "LEADHAND_LOCK_FD={value}"
            );
        }
    }
}

/// R41(a): every command drops LEADHAND_LOCK_FD from its environment, whatever it decides
/// about the descriptor, so nothing it starts inherits the variable. Observed through a Codex
/// dispatch: the fake Codex logs its whole environment.
mod lock_fd_environment {
    use super::*;
    use common::dispatch_world::World as DispatchWorld;
    use common::launch_world::{PASSED_FD, run_passing};
    use common::open_lock_file;

    /// The `dispatch` line run with a descriptor on `path` inherited and named in
    /// LEADHAND_LOCK_FD, and LEADHAND_RUN_DIR naming the run; returns the fake Codex's
    /// environment.
    #[track_caller]
    fn dispatch_with(w: &mut DispatchWorld, did: &str, path: &Path) -> Json {
        w.scenario("ready", &[]);
        let file = open_lock_file(path);
        let env = w
            .env
            .with("LEADHAND_LOCK_FD", PASSED_FD.to_string())
            .with("LEADHAND_RUN_DIR", &w.run_dir);
        let mut argv: Vec<OsString> = vec![common::bin().into()];
        argv.extend(w.args(did));
        let p = run_passing(&argv, file, &env, &w.tmp, Duration::from_secs(120));
        assert_eq!(p.code, 0, "{p}");
        let last = w.wd(did).join("last-message.md");
        let starts: Vec<Json> = w
            .records("start")
            .into_iter()
            .filter(|r| {
                let argv = common::launch_world::strings(&r["argv"]);
                argv.len() >= 3
                    && argv[argv.len() - 3..argv.len() - 1] == s(&["-o", path_str(&last)])
            })
            .collect();
        assert_eq!(starts.len(), 1);
        starts[0]["env"].clone()
    }

    #[test]
    fn test_a_worker_never_inherits_lead_hand_lock_fd() {
        let mut w = DispatchWorld::new();
        let unrelated = w.tmp.join("unrelated.txt");
        fs::write(&unrelated, "x").unwrap();
        for (did, path) in [("d1", w.run_dir.join("run.lock")), ("d2", unrelated)] {
            let fd_on = path.file_name().unwrap().to_string_lossy().into_owned();
            let env = dispatch_with(&mut w, did, &path);
            // the worker's env was logged
            assert_eq!(env["LEADHAND_RUN_ID"], w.run_id.as_str(), "fd_on={fd_on}");
            assert!(
                env.get("LEADHAND_LOCK_FD").is_none(),
                "fd_on={fd_on}: the worker inherited LEADHAND_LOCK_FD"
            );
        }
    }
}
