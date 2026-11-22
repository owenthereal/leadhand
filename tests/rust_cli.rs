//! The Rust binary's own command line: the version, clap's closest-match suggestion, the help
//! of every command a person types, and a release build that ignores every test hook
//! (deliberate changes 4 and 5, and the conventions for the commands a person types).
//! Rust-only.
//!
//! The Rust form of `tests/contract/contract_rust_cli.py`. In stage 2 the surface tests ran
//! the Rust binary behind the router (LEADHAND_RUST_BIN); here the binary under test is
//! `common::bin()` itself.

#[macro_use]
mod common;

use std::path::PathBuf;

/// The commands a person types (spec, "Commands you type").
const HUMAN_COMMANDS: [&str; 10] = [
    "run",
    "start",
    "status",
    "pause",
    "cancel",
    "unpark",
    "report",
    "check",
    "launch-line",
    "version",
];

fn release_bin() -> PathBuf {
    common::plugin::root()
        .join("target")
        .join("release")
        .join("leadhand")
}

#[track_caller]
fn require_release_bin() -> PathBuf {
    let bin = release_bin();
    if !bin.is_file() {
        panic!(
            "no release build at {}: build it with `cargo build --locked --release`",
            bin.display()
        );
    }
    bin
}

/// What the reference CLI printed for `version`, recorded by the generator in
/// `tests/fixtures/reference/`.
fn reference_version() -> String {
    common::plugin::read(&common::plugin::root().join("tests/fixtures/reference/cli-version.txt"))
}

mod version {
    use super::*;
    use crate::common::{Env, TempRoot, cli};

    #[test]
    fn test_version_matches_reference() {
        if common::rust_only() {
            return;
        }
        let tmp = TempRoot::new("lh rust cli ");
        let env = Env::new(&tmp);
        let r = cli(&os_args!["version"], &env, &tmp).ok();
        assert_eq!(r.stdout_text(), "leadhand 0.1.0\n", "{r}");
        assert_eq!(r.stderr_text(), "", "{r}");
        let reference = reference_version();
        assert_eq!(r.stdout, reference.as_bytes(), "{reference}");
    }

    #[test]
    fn test_version_reaches_the_rust_binary() {
        // The help of a listed command comes from clap, with its example: not from argparse.
        if common::rust_only() {
            return;
        }
        let tmp = TempRoot::new("lh rust cli ");
        let r = cli(&os_args!["version", "--help"], &Env::new(&tmp), &tmp).ok();
        assert!(
            r.stdout_text().contains("\nExample:\n  leadhand version\n"),
            "{r}"
        );
    }
}

mod surface {
    use super::*;
    use crate::common::{Env, TempRoot, cli};

    #[test]
    fn test_unknown_command_suggests_closest() {
        if common::rust_only() {
            return;
        }
        let tmp = TempRoot::new("lh rust cli ");
        let r = cli(&os_args!["statu"], &Env::new(&tmp), &tmp);
        assert_eq!(r.code, 2, "{r}");
        let stderr = r.stderr_text();
        assert!(
            stderr.starts_with("error: unrecognized subcommand 'statu'\n"),
            "{r}"
        );
        // clap names every close match: "a similar subcommand exists: 'x'" or "some similar
        // subcommands exist: 'start', 'status'".
        let tips: Vec<&str> = stderr
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("tip: "))
            .collect();
        assert_eq!(tips.len(), 1, "{r}");
        assert!(
            [
                "tip: a similar subcommand exists: ",
                "tip: some similar subcommands exist: "
            ]
            .iter()
            .any(|p| tips[0].starts_with(p)),
            "{r}"
        );
        assert!(tips[0].contains("'status'"), "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
    }

    #[test]
    fn test_unknown_flag_suggests_closest() {
        if common::rust_only() {
            return;
        }
        let tmp = TempRoot::new("lh rust cli ");
        let r = cli(
            &os_args!["check", "deck.md", "--rpeo", "."],
            &Env::new(&tmp),
            &tmp,
        );
        assert_eq!(r.code, 2, "{r}");
        assert!(
            r.stderr_text()
                .starts_with("error: unexpected argument '--rpeo' found\n"),
            "{r}"
        );
        assert!(
            r.stderr_text()
                .contains("tip: a similar argument exists: '--repo'"),
            "{r}"
        );
    }

    #[test]
    fn test_help_has_example_for_each_human_command() {
        if common::rust_only() {
            return;
        }
        let tmp = TempRoot::new("lh rust cli ");
        let env = Env::new(&tmp);
        for name in HUMAN_COMMANDS {
            let r = cli(&os_args![name, "--help"], &env, &tmp).ok();
            let stdout = r.stdout_text();
            let lines: Vec<&str> = stdout.lines().collect();
            // The first line is the one-line purpose.
            assert!(
                !lines.is_empty() && !lines[0].trim().is_empty() && !lines[0].starts_with("Usage:"),
                "{name}: {r}"
            );
            assert_eq!(lines[1], "", "{name}: {r}");
            // An "Example:" section whose command runs this command.
            let at = lines.iter().position(|l| *l == "Example:");
            assert!(at.is_some(), "{name}: {r}");
            let example = lines[at.unwrap() + 1];
            assert!(
                example == format!("  leadhand {name}")
                    || example.starts_with(&format!("  leadhand {name} ")),
                "{name}: {r}"
            );
        }
    }
}

mod release_build {
    //! Review focus 4: a release build ignores every test hook (deliberate change 5).
    //!
    //! The world is contract_recovery's StableSupervisor, the crash matrix's, from
    //! `common::recovery` (with its `operator`, `resume` and `final_state`).
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use rustix::process::{Pid, Signal, kill_process_group};

    use super::*;
    use crate::common::json::Json;
    use crate::common::recovery::{DATES, final_state, operator, resume, stable_supervisor};
    use crate::common::supervisor::{FakeSupervisor, run_dir};
    use crate::common::{Env, TIMEOUT, TempRoot, cli, jv, patch_env, py_str, run_argv};

    const HOOKS: [&str; 13] = [
        "LEADHAND_TESTING",
        "LEADHAND_CRASH_AT",
        "LEADHAND_CRASH_LOG",
        "LEADHAND_CRASH_COUNTER",
        "LEADHAND_CRASH_SKIP",
        "LEADHAND_CRASH_KILL_PGID",
        "LEADHAND_TEST_CODEX_BIN",
        "LEADHAND_TEST_CARD_LOOP_ONLY",
        "LEADHAND_INSTALLED_VENDORS",
        "LEADHAND_MINUTE_SECONDS",
        "LEADHAND_TEST_CLOCK_OFFSET",
        "LEADHAND_TEST_CLI",
        "LEADHAND_TEST_FOREGROUND_WAIT",
    ];

    /// The crash points `next` itself passes: the rest are a runner's
    /// (before/after_worker_launch) and the harness's workers' (before/after_commit).
    const NEXT_POINTS: [&str; 9] = [
        "before_rebase",
        "after_rebase",
        "before_fast_forward",
        "after_fast_forward",
        "before_result_record",
        "after_result_files",
        "after_charge_before_attempt",
        "after_cancel_request",
        "before_gate_record",
    ];

    /// A process group that LEADHAND_CRASH_KILL_PGID names: it survives only if the hook is
    /// ignored. Killed with its group on drop.
    struct Decoy(Child);

    impl Decoy {
        fn start() -> Decoy {
            let child = Command::new("sleep")
                .arg("60")
                .process_group(0)
                .stdin(Stdio::null())
                .spawn()
                .expect("starting the decoy");
            Decoy(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        /// `poll()`: None while it runs.
        fn poll(&mut self) -> Option<std::process::ExitStatus> {
            self.0.try_wait().expect("polling the decoy")
        }
    }

    impl Drop for Decoy {
        fn drop(&mut self) {
            let _ = kill_process_group(Pid::from_child(&self.0), Signal::KILL);
            let _ = self.0.wait();
        }
    }

    /// The world's drive, with the relaunch, to its end or until `until()`; the actions
    /// `next_action` returned, by call.
    fn drive(
        w: &mut FakeSupervisor,
        next_action: &mut dyn FnMut(&FakeSupervisor, usize) -> Json,
        until: &dyn Fn() -> bool,
    ) -> Vec<Json> {
        w.session("supervisor-1");
        w.start();
        let mut actions: Vec<Json> = Vec::new();
        let mut relaunched = false;
        for _ in 0..200 {
            operator(w);
            let a = next_action(w, actions.len());
            actions.push(a.clone());
            if a["action"] == "done" || until() {
                return actions;
            }
            if a["action"] == "wait" {
                std::thread::sleep(Duration::from_millis(100));
            } else if !relaunched && a["action"] == "dispatch" && a["agent"] == "leadhand:codex" {
                relaunched = true;
                resume(w); // the session died before its proxy ran
            } else {
                w.act(&a);
            }
        }
        panic!("the run did not finish");
    }

    fn action_ids(actions: &[Json]) -> Vec<(String, String)> {
        actions
            .iter()
            .map(|a| {
                let did = a.get("dispatch_id").unwrap_or(&Json::Null);
                (py_str(&a["action"]), py_str(did))
            })
            .collect()
    }

    #[test]
    fn test_release_binary_ignores_every_test_hook() {
        if common::rust_only() {
            return;
        }
        let release = require_release_bin();
        let tmp = TempRoot::new("lh rust release ");
        let env = Env::new(&tmp);
        let mut plain = env.clone();
        for k in HOOKS {
            plain.remove(k);
        }
        let mut decoy = Decoy::start();
        let crash_log = tmp.join("crash log");
        let crash_counter = tmp.join("crash counter");
        let mut hooked = plain.clone();
        hooked
            .set("LEADHAND_TESTING", "1")
            .set("LEADHAND_CRASH_AT", "version")
            .set("LEADHAND_CRASH_LOG", &crash_log)
            .set("LEADHAND_CRASH_COUNTER", &crash_counter)
            .set("LEADHAND_CRASH_SKIP", "0")
            .set("LEADHAND_CRASH_KILL_PGID", decoy.pid().to_string())
            .set("LEADHAND_TEST_CODEX_BIN", tmp.join("fake codex"))
            .set("LEADHAND_TEST_CARD_LOOP_ONLY", "1")
            .set("LEADHAND_INSTALLED_VENDORS", "claude,codex")
            .set("LEADHAND_MINUTE_SECONDS", "1")
            .set("LEADHAND_TEST_CLOCK_OFFSET", "3600")
            .set("LEADHAND_TEST_CLI", "/x/router")
            .set("LEADHAND_TEST_FOREGROUND_WAIT", "1");
        let unset: Vec<&str> = HOOKS
            .iter()
            .copied()
            .filter(|k| hooked.get(k).is_none())
            .collect();
        assert_eq!(unset, Vec::<&str>::new());

        let run_release = |args: &[&std::ffi::OsStr], env: &Env| {
            let mut argv = vec![release.as_os_str()];
            argv.extend_from_slice(args);
            run_argv(&argv, env, &tmp, None, TIMEOUT)
        };
        let plain_out = run_release(&os_args!["version"], &plain);
        let hooked_out = run_release(&os_args!["version"], &hooked);
        assert_eq!(plain_out.stdout_text(), "leadhand 0.1.0\n", "{plain_out}");
        assert_eq!(
            (hooked_out.code, &hooked_out.stdout, &hooked_out.stderr),
            (plain_out.code, &plain_out.stdout, &plain_out.stderr),
            "{hooked_out}"
        );
        assert!(!crash_log.exists());
        assert!(!crash_counter.exists());
        assert!(
            decoy.poll().is_none(),
            "the release build signalled LEADHAND_CRASH_KILL_PGID's group"
        );

        // `next` (Task 17): a whole run driven twice from the same deck with fixed commit
        // dates, as the crash matrix drives it (contract_recovery.StableSupervisor: a parked
        // card unparked and integrated, a Codex card with a fix round, an escalation, the
        // end-of-run gates), and a relaunch while a Codex dispatch was handed out but never
        // run, so `next` reconciles it.
        //
        // The reference run is the testing build, with no hook beyond the world's own; each
        // `next` logs the crash points it passes (LEADHAND_CRASH_LOG). The release run's every
        // `next` gets every hook set, with LEADHAND_CRASH_AT naming a point the same call of
        // the reference run passed, until each such point is named: it must never crash, log,
        // count or signal, and must reach the same final state.
        let _dates = patch_env(&DATES);
        let mut points: Vec<&str> = vec!["before_worker_launch", "after_worker_launch"];
        let mut sorted_next = NEXT_POINTS.to_vec();
        sorted_next.sort();
        points.extend(sorted_next);
        points.extend(["before_commit", "after_commit"]);
        let next_points: BTreeSet<String> = NEXT_POINTS.iter().map(|s| s.to_string()).collect();

        let mut passed: Vec<Vec<String>> = Vec::new();
        let mut reference_next = |w: &FakeSupervisor, i: usize| {
            let log = w.tmp.join(format!("crash log {i}"));
            let r = cli(
                &os_args!["next", run_dir(w)],
                &w.clock_env().with("LEADHAND_CRASH_LOG", &log),
                &w.tmp,
            )
            .ok();
            passed.push(match fs::read_to_string(&log) {
                Ok(text) => text.split_whitespace().map(str::to_string).collect(),
                Err(_) => Vec::new(),
            });
            r.json()
        };
        let mut reference_world = stable_supervisor();
        let reference = drive(&mut reference_world, &mut reference_next, &|| false);
        let expected = final_state(&reference_world);
        assert_json_eq!(expected["outcome"], jv(r#"["complete", ""]"#));
        let on_path: BTreeSet<String> = passed.iter().flatten().cloned().collect();
        assert_eq!(on_path, next_points);

        let named: RefCell<BTreeSet<String>> = RefCell::new(BTreeSet::new());
        let next_log = tmp.join("next crash log");
        let next_counter = tmp.join("next crash counter");
        let mut release_next = |w: &FakeSupervisor, i: usize| {
            let call: Vec<String> = passed.get(i).cloned().unwrap_or_default();
            let point = {
                let named = named.borrow();
                call.iter()
                    .find(|p| !named.contains(*p))
                    .cloned()
                    .or_else(|| call.first().cloned())
                    .unwrap_or_else(|| points[i % points.len()].to_string())
            };
            if call.contains(&point) {
                named.borrow_mut().insert(point.clone());
            }
            let mut env = w.clock_env().with("PATH", w.env.path_with_fakes());
            for k in HOOKS {
                env.set(k, hooked.get(k).unwrap());
            }
            env.set("LEADHAND_CRASH_AT", &point)
                .set("LEADHAND_CRASH_LOG", &next_log)
                .set("LEADHAND_CRASH_COUNTER", &next_counter)
                .set("LEADHAND_INSTALLED_VENDORS", "claude");
            let r = run_release(&os_args!["next", run_dir(w)], &env);
            assert_eq!(r.code, 0, "{point}: {r}");
            r.json()
        };

        let mut w = stable_supervisor();
        let actions = drive(&mut w, &mut release_next, &|| false);
        assert_eq!(action_ids(&actions), action_ids(&reference));
        assert_json_eq!(final_state(&w), expected);
        assert!(!w.events("reconciled").is_empty());
        // One call passes several points (the reconciling one passes four), and one `next`
        // names one: the same run again, as far as it takes to name the rest.
        let all_named = || *named.borrow() == next_points;
        for _ in 0..4 {
            if all_named() {
                break;
            }
            let mut again = stable_supervisor();
            drive(&mut again, &mut release_next, &all_named);
        }
        assert_eq!(*named.borrow(), next_points);
        for path in [&next_log, &next_counter] {
            assert!(!path.exists(), "{}", path.display());
        }
        assert!(
            decoy.poll().is_none(),
            "the release build signalled LEADHAND_CRASH_KILL_PGID's group"
        );
    }
}

mod release_build_dispatch {
    //! Review focus 4 for the dispatch hooks (deliberate change 5, and the foreground-wait
    //! ruling): a release `dispatch`, with LEADHAND_TESTING=1, ignores LEADHAND_TEST_CODEX_BIN,
    //! LEADHAND_MINUTE_SECONDS and LEADHAND_TEST_FOREGROUND_WAIT.
    //!
    //! Each runs on a run `start` made (contract_dispatch.World), with the fakes first on
    //! PATH, so the `codex` a release build looks up is the fake: nothing reaches a real
    //! Codex. The release binary starts its runner as itself (it ignores LEADHAND_TEST_CLI
    //! too). Each case's control runs the same line through LEADHAND_BIN (the testing build),
    //! which honours the hook.
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::common::dispatch_world::{World, os_refs};
    use crate::common::json::Json;
    use crate::common::{Env, Out, cli_with, read_json, run_argv};

    /// What these tests add to the shared dispatch world (`common::dispatch_world::World`):
    /// the hooked environment, and the same dispatch line run by the release build or by the
    /// testing build.
    trait Hooks {
        fn hooked_env(&self, hooks: &[(&str, &Path)]) -> Env;
        fn release(&self, did: &str, env: &Env) -> Out;
        fn run_testing(&self, did: &str, env: &Env) -> Out;
    }

    impl Hooks for World {
        /// The world's environment as it is now (its scenario included), with the fakes first
        /// on PATH, LEADHAND_TESTING=1, no Codex override unless `hooks` names one, and
        /// `hooks`.
        fn hooked_env(&self, hooks: &[(&str, &Path)]) -> Env {
            let mut env = self
                .env
                .with("PATH", self.env.path_with_fakes())
                .with("LEADHAND_TESTING", "1")
                .without("LEADHAND_TEST_CODEX_BIN");
            for (k, v) in hooks {
                env.set(k, v);
            }
            env
        }

        fn release(&self, did: &str, env: &Env) -> Out {
            let bin = require_release_bin();
            let args = self.args(did);
            let mut argv = vec![bin.as_os_str()];
            argv.extend(os_refs(&args));
            run_argv(&argv, env, &self.tmp, None, Duration::from_secs(120))
        }

        fn run_testing(&self, did: &str, env: &Env) -> Out {
            let args = self.args(did);
            cli_with(
                &os_refs(&args),
                env,
                &self.tmp,
                None,
                Duration::from_secs(120),
            )
        }
    }

    /// A dispatch that finished READY: its report's status, the fake's session, and
    /// result.json's exit reason (`assert_ready`).
    macro_rules! assert_ready {
        ($w:expr, $r:expr, $did:expr) => {{
            let w: &World = &$w;
            let r: Out = $r.ok();
            let stdout = r.stdout_text();
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(lines[0], "Status: READY", "{r}");
            assert_eq!(
                lines[lines.len() - 1],
                "session: 0f0f0f0f-0000-4000-8000-00000000c0de",
                "{r}"
            );
            assert_json_eq!(
                read_json(&w.wd($did).join("result.json"))["exit_reason"],
                Json::from("completed"),
                "{r}"
            );
        }};
    }

    #[test]
    fn test_release_dispatch_ignores_the_codex_bin_hook() {
        if common::rust_only() {
            return;
        }
        require_release_bin();
        let mut w = World::new();
        let flag = w.tmp.join("override ran");
        let over = w.tmp.join("fake codex override");
        fs::write(
            &over,
            format!(
                "#!/bin/sh\necho ran >> \"{}\"\nexec \"{}\" \"$@\"\n",
                flag.display(),
                w.env.fakes().join("codex").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&over, fs::Permissions::from_mode(0o755)).unwrap();
        w.scenario("ready", &[]);
        let env = w.hooked_env(&[("LEADHAND_TEST_CODEX_BIN", &over)]);
        assert_ready!(w, w.release("d1", &env), "d1");
        assert!(
            !flag.exists(),
            "the release build ran LEADHAND_TEST_CODEX_BIN"
        );
        assert_eq!(w.records("start").len(), 1); // the fake found on PATH ran
        assert_ready!(w, w.run_testing("d2", &env), "d2");
        assert_eq!(fs::read_to_string(&flag).unwrap(), "ran\n");
        assert_eq!(w.records("start").len(), 2);
    }

    #[test]
    fn test_release_dispatch_ignores_the_minute_hook() {
        // A one-minute limit, a Codex that answers after 2 s, and a "minute" of 0.05 s.
        if common::rust_only() {
            return;
        }
        require_release_bin();
        let mut w = World::new();
        w.set_limits("implementer", &[("minutes", Json::from(1))]);
        w.scenario("ready", &[("delay", "2")]);
        let env = w.hooked_env(&[]).with("LEADHAND_MINUTE_SECONDS", "0.05");
        assert_ready!(w, w.release("d1", &env), "d1");
        let r = w.run_testing("d2", &env).ok();
        assert_eq!(r.stdout_text().lines().next(), Some("Status: SPENT"), "{r}");
        assert_json_eq!(
            read_json(&w.wd("d2").join("result.json"))["exit_reason"],
            Json::from("time_limit"),
            "{r}"
        );
    }

    #[test]
    fn test_release_dispatch_ignores_the_foreground_wait_hook() {
        // The foreground-wait ruling's case: a 1 s hook and a Codex that answers after 4 s.
        if common::rust_only() {
            return;
        }
        let _serial = common::serial(); // group: the foreground wait (S3-R19)
        require_release_bin();
        let mut w = World::new();
        w.scenario("ready", &[("delay", "4")]);
        let env = w.hooked_env(&[]).with("LEADHAND_TEST_FOREGROUND_WAIT", "1");
        assert_ready!(w, w.release("d1", &env), "d1");
        let r = w.run_testing("d2", &env);
        assert_eq!(r.code, 75, "{r}");
        assert_eq!(
            r.stdout_text().lines().collect::<Vec<_>>(),
            ["still running: run the same command again (dispatch d2)"],
            "{r}"
        );
    }
}
