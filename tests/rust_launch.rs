//! `run` and `launch-line` in Rust: the conventions for the commands a person types (the run
//! found from the current directory, `--json`, bare `leadhand` when PATH finds this binary),
//! a CLI path with a space in it (R28), and the run-lock descriptor, which reaches `claude`
//! alone. Rust-only while the Python reference existed.
//!
//! The Rust form of the Python contract suite's `contract_rust_launch`, on the launch world
//! (`common::launch_world::World`).
//!
//! With no argument, the command takes the run whose worktree contains the current
//! directory, else the only unfinished run of the repository containing it, else it lists the
//! candidates, each with its exact command, and exits 1 (spec, "Commands you type").
//! `launch-line` prints the binary's absolute path, or bare `leadhand` when the `leadhand`
//! found on PATH is this same binary (spec, "Calls made in planning"); the Python always
//! prints its path.
//!
//! The descriptors (spec, bug class 6): every file Rust opens is close-on-exec, and the
//! launcher clears the flag on run.lock only in `claude`'s pre-exec step, so the keep-awake
//! tool beside it never holds the run lock; like Python's `Popen(close_fds=True)`, neither
//! child gets anything else the launcher inherited. The fakes (shell scripts, `FD_CLAUDE`
//! and `FD_TOOL`) list their open descriptors from /dev/fd (no `lsof`).

#[macro_use]
mod common;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use common::json::Json;
use common::launch_world::{AGENT, FIRST_MESSAGE_END, World};
use common::{
    names_bin, one_line, os_vec, path_str, shlex_join, shlex_quote, shlex_split, str_vec,
};

const ATTENDED: &str = "leadhand: attended use only. Until the Claude-side run guard lands, keep this session \
     in view and run it only on fixture and trial repositories.\n\
     leadhand: the supervisor runs with --permission-mode auto. Its --settings overlay holds one \
     sandbox.excludedCommands entry, the leadhand CLI ({cli} *); whether it merges with or \
     replaces your own excludedCommands is not established.\n";
const SPACES: &str = "leadhand: warning: the leadhand CLI path contains whitespace ({cli}); the sandbox exclusion \
     may not match; install leadhand under a path without spaces.\n";

/// The path the binary names itself by: the router's link behind the router (ruling S2-R5),
/// else the binary. With no router in stage 3, the binary.
fn self_cli(w: &World) -> std::path::PathBuf {
    match w.env.get("LEADHAND_TEST_CLI") {
        Some(cli) if !cli.is_empty() => cli.into(),
        _ => common::bin(),
    }
}

mod launch_line {
    use super::*;

    #[test]
    fn test_launch_line_without_argument_selects_by_cwd() {
        let mut w = World::new();
        let deck = path_str(&w.paths.deck).to_string();
        let want = one_line(w.launch_line(&os_args![w.run_dir]));
        assert!(
            common::is_bin_command(
                &want,
                "",
                &format!(" {}", shlex_join(&os_vec(&["run", &deck])))
            ),
            "{want}"
        );
        // From inside the run's worktree (its top or a subdirectory), and from the
        // repository, whose only unfinished run it is.
        let deep = w.workspace.join("deep dir");
        fs::create_dir(&deep).unwrap();
        for cwd in [&w.workspace, &deep, &w.repo] {
            assert_eq!(
                one_line(w.launch_line_in(&[], cwd)),
                want,
                "cwd={}",
                cwd.display()
            );
        }
        // By run ID, directory name and slug, from the repository.
        let meta = w.sup.meta();
        let name = w.run_dir.file_name().unwrap().to_str().unwrap().to_string();
        for arg in [
            meta["run_id"].as_str().unwrap(),
            name.as_str(),
            meta["slug"].as_str().unwrap(),
        ] {
            assert_eq!(
                one_line(w.launch_line_in(&os_args![arg], &w.repo)),
                want,
                "arg={arg}"
            );
        }
        // `run` finds it the same way: from the worktree it launches this run.
        let r = common::cli_with(
            &os_args!["run"],
            &w.env,
            &deep,
            None,
            Duration::from_secs(60),
        );
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(
            w.starts()[0].rec["env"]["LEADHAND_RUN_DIR"],
            path_str(&w.run_dir)
        );

        // Two unfinished runs of the repository: each candidate is listed with its own
        // command.
        let other = w.sup.start_new();
        let r = w.launch_line_in(&[], &w.repo);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
        let stderr = r.stderr_text();
        let lines: Vec<&str> = stderr.lines().collect();
        assert!(lines[0].starts_with("error: 2 unfinished runs of "), "{r}");
        for run_dir in [&w.run_dir, &other] {
            assert!(
                lines[1..].iter().any(|line| common::is_bin_command(
                    line,
                    "",
                    &format!(" launch-line {}", shlex_quote(path_str(run_dir)))
                )),
                "{}: {r}",
                run_dir.display()
            );
        }
        // Inside the first run's worktree it is still that run.
        assert_eq!(one_line(w.launch_line_in(&[], &deep)), want);
    }

    #[test]
    fn test_launch_line_json() {
        let w = World::new();
        let plugin = w.tmp.join("plugin dir");
        let cases: [Vec<OsString>; 2] = [
            vec![w.run_dir.clone().into()],
            vec![
                w.paths.deck.clone().into(),
                "--plugin-dir".into(),
                plugin.into(),
            ],
        ];
        for args in &cases {
            let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
            let text = one_line(w.launch_line(&refs));
            let mut with_json = refs.clone();
            with_json.push("--json".as_ref());
            let r = w.launch_line(&with_json).ok();
            assert_eq!(r.stderr_text(), "", "{args:?}: {r}");
            assert_eq!(r.stdout_text().matches('\n').count(), 1, "{args:?}: {r}");
            assert!(r.stdout.is_ascii(), "{args:?}: {r}");
            assert_json_eq!(
                r.json(),
                Json::object([("command", Json::from(text.as_str()))]),
                "{args:?}: {r}"
            );
        }
        // A run that cannot be found is the usual error, not JSON.
        let r = w.launch_line(&os_args![w.elsewhere, "--json"]);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
        assert!(
            r.stderr_text()
                .starts_with("error: not a run directory or a run's deck.md: "),
            "{r}"
        );
    }

    #[test]
    fn test_launch_line_prints_bare_leadhand_when_path_resolves_here() {
        let w = World::new();
        let deck = path_str(&w.paths.deck).to_string();
        let on_path = w.tmp.join("on path");
        fs::create_dir(&on_path).unwrap();
        let link = on_path.join("leadhand");
        std::os::unix::fs::symlink(self_cli(&w), &link).unwrap();
        assert!(names_bin(&link));
        let mut path = on_path.clone().into_os_string();
        path.push(":");
        path.push(w.env.get("PATH").unwrap());
        let env = w.env.with("PATH", path);
        let r = common::cli(&os_args!["launch-line", w.run_dir], &env, &w.elsewhere);
        assert_eq!(
            r.stdout_text(),
            shlex_join(&os_vec(&["leadhand", "run", &deck])) + "\n",
            "{r}"
        );
        let r = common::cli(
            &os_args!["launch-line", w.run_dir, "--json"],
            &env,
            &w.elsewhere,
        )
        .ok();
        assert_json_eq!(
            r.json(),
            Json::object([(
                "command",
                Json::from(shlex_join(&os_vec(&["leadhand", "run", &deck])))
            )]),
            "{r}"
        );
        // The launch itself still names the binary by its absolute path.
        let r = common::cli_with(
            &os_args!["run", w.run_dir],
            &env,
            &w.elsewhere,
            None,
            Duration::from_secs(60),
        );
        assert_eq!(r.code, 0, "{r}");
        let cli = w.starts()[0].rec["env"]["LEADHAND_CLI"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(Path::new(&cli).is_absolute() && names_bin(&cli), "{cli}");

        // A `leadhand` on PATH that is some other program: the absolute path, as without one.
        fs::remove_file(&link).unwrap();
        fs::write(&link, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&link, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let line = one_line(common::cli(
            &os_args!["launch-line", w.run_dir],
            &env,
            &w.elsewhere,
        ));
        assert!(
            common::is_bin_command(
                &line,
                "",
                &format!(" {}", shlex_join(&os_vec(&["run", &deck])))
            ),
            "{line}"
        );
        assert_ne!(shlex_split(&line).unwrap()[0], "leadhand");
    }
}

mod paths {
    use super::*;
    use common::rundir::append_event;

    /// Covers R28 and Review Focus 5: a real CLI path with a space in it (a link to the
    /// binary in a spaced directory) launches, warns, and is quoted wherever it is a word.
    #[test]
    fn test_cli_path_with_space_warns_and_quotes() {
        let mut w = World::new();
        let root = common::TempRoot::new("lh cli ");
        let link = root.path().join("lead hand bin/leadhand");
        fs::create_dir(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(self_cli(&w), &link).unwrap();
        assert!(names_bin(&link));
        let cli = path_str(&link).to_string();
        w.env.set("LEADHAND_TEST_CLI", &cli);
        let deck = path_str(&w.paths.deck).to_string();

        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(
            r.stderr_text(),
            ATTENDED.replace("{cli}", &cli) + &SPACES.replace("{cli}", &cli),
            "{r}"
        );
        let start = w.starts()[0].clone();
        let argv = start.argv();
        let (settings, last) = (argv[2].as_str(), argv.last().unwrap().as_str());
        assert_eq!(
            argv[1..],
            str_vec(&[
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
        assert_json_eq!(
            common::jv(settings),
            Json::object([(
                "sandbox",
                Json::object([("excludedCommands", Json::from(&[format!("{cli} *")][..]))])
            )])
        );
        assert_eq!(
            last,
            format!(
                "Run the leadhand deck in {}. leadhand CLI: {cli}{FIRST_MESSAGE_END}",
                w.run_dir.display()
            )
        );
        assert_eq!(start.rec["env"]["LEADHAND_CLI"], cli.as_str());

        // The launch line quotes the path as one word, and runs as printed.
        let line = one_line(w.launch_line(&os_args![w.run_dir]));
        assert_eq!(line, shlex_join(&os_vec(&[&cli, "run", &deck])));
        let words = shlex_split(&line).unwrap();
        assert_eq!(words, str_vec(&[&cli, "run", &deck]));
        let word_refs: Vec<&std::ffi::OsStr> = words.iter().map(|w| w.as_ref()).collect();
        let r = common::run_argv(
            &word_refs,
            &w.env,
            &w.elsewhere,
            None,
            Duration::from_secs(60),
        );
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.starts().len(), 2);
        assert_eq!(
            w.starts()[1].rec["env"]["LEADHAND_RUN_DIR"],
            path_str(&w.run_dir)
        );

        // A run that has ended: the refusal names the start command, the spaced path quoted.
        append_event(
            &w.run_dir,
            "outcome",
            [
                ("outcome", Json::from("complete")),
                ("qualifier", Json::from("")),
                ("reasons", Json::Arr(Vec::new())),
            ],
        );
        let meta = w.sup.meta();
        let r = w.run(&[]);
        assert_eq!(r.code, 1, "{r}");
        // The error line, then the one command that fixes it alone on the last line.
        let fix = str_vec(&[
            &cli,
            "start",
            meta["deck_source"].as_str().unwrap(),
            "--repo",
            meta["repo"].as_str().unwrap(),
        ]);
        let fix_os: Vec<OsString> = fix.iter().map(OsString::from).collect();
        assert_eq!(
            r.stderr_text(),
            format!(
                "error: this run has ended: complete; only a blocked or unfinished run \
                 continues. Start a new run from the deck with:\n{}\n",
                shlex_join(&fix_os)
            ),
            "{r}"
        );
        assert_eq!(
            shlex_split(r.stderr_text().lines().last().unwrap()).unwrap(),
            fix
        );
        assert_eq!(w.starts().len(), 2);
    }
}

mod descriptors {
    use super::*;
    use common::launch_world::{
        FD_CLAUDE, FD_TOOL, keep_awake_tool, keep_awake_unsupported, stat_dev,
    };
    use common::{read_jsonl, started_by};
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt;

    /// A file's (device, inode), the device as the shell stand-in's `stat` prints it.
    fn id_of(path: &Path) -> (i64, i64) {
        let m = fs::metadata(path).unwrap();
        (stat_dev(&m), m.ino() as i64)
    }

    /// A logged descriptor's (device, inode).
    fn id(d: &Json) -> (i64, i64) {
        (d["dev"].as_i64().unwrap(), d["ino"].as_i64().unwrap())
    }

    /// The platform's keep-awake tool (`caffeinate` on macOS, `systemd-inhibit` on Linux)
    /// runs beside the supervisor without run.lock; `claude` has it, at LEADHAND_LOCK_FD. A
    /// descriptor the launcher inherited reaches neither; the tool's stdin, stdout and stderr
    /// are /dev/null.
    #[test]
    fn test_lock_fd_reaches_claude_but_not_caffeinate() {
        if keep_awake_unsupported() {
            return;
        }
        let mut w = World::new();
        fs::remove_file(w.bindir.join("claude")).unwrap();
        w.tool("claude", FD_CLAUDE);
        for name in ["caffeinate", "systemd-inhibit"] {
            w.tool(name, FD_TOOL);
        }
        let stray = w.tmp.join("inherited.txt");
        fs::write(&stray, "x").unwrap();
        let fd = fs::File::open(&stray).unwrap();
        let argv: Vec<OsString> =
            vec![common::bin().into(), "run".into(), w.run_dir.clone().into()];
        let env = w.env.clone();
        let launcher = w.popen_with(&argv, &env, Some(fd), false);
        let r = w.finish(&launcher, Duration::from_secs(30));
        assert_eq!(r.code, 0, "{r}");

        let lock_id = id_of(&w.paths.lock);
        let stray_id = id_of(&stray);
        let null = id_of(Path::new("/dev/null"));

        let sup = common::one(w.records());
        assert_eq!(sup["ppid"], i64::from(launcher.pid()));
        let fds = sup["fds"].as_array().unwrap();
        let held: BTreeMap<(i64, i64), i64> = fds
            .iter()
            .map(|d| (id(d), d["fd"].as_i64().unwrap()))
            .collect();
        assert!(held.contains_key(&lock_id), "{sup}");
        assert_eq!(sup["lock_fd"], held[&lock_id].to_string().as_str(), "{sup}");
        let lock_fds: Vec<i64> = fds
            .iter()
            .filter(|d| id(d) == lock_id)
            .map(|d| d["fd"].as_i64().unwrap())
            .collect();
        assert_eq!(lock_fds, [held[&lock_id]]);
        assert!(!held.contains_key(&stray_id), "{sup}");

        let recs = read_jsonl(&w.keep_awake_log);
        let names: Vec<String> = recs
            .iter()
            .map(|rec| {
                Path::new(&common::strs(&rec["argv"])[0])
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, [keep_awake_tool()]);
        let tool: BTreeMap<i64, (i64, i64)> = recs[0]["fds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["fd"].as_i64().unwrap(), id(d)))
            .collect();
        assert!(!tool.values().any(|v| *v == lock_id), "{}", recs[0]);
        assert!(!tool.values().any(|v| *v == stray_id), "{}", recs[0]);
        assert_eq!(
            [0, 1, 2].map(|n| tool.get(&n).copied()),
            [Some(null); 3],
            "{}",
            recs[0]
        );
        assert!(
            started_by(
                recs[0]["pid"].as_i64().unwrap() as i32,
                recs[0]["time"].as_f64().unwrap()
            )
            .is_none(),
            "keep-awake left running"
        );
        // The launch released the lock once the supervisor exited: the run launches again.
        let r = w.run(&[]);
        assert_eq!(r.code, 0, "{r}");
    }
}
