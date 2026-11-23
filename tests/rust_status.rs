//! `status` and `report` in Rust: the run found from the current directory (deliberate change
//! 3) and the conventions for the commands a person types (`--json`, candidates listed with
//! their commands). Rust-only while the Python reference existed.
//!
//! The Rust form of the Python contract suite's `contract_rust_status`. With no argument, the
//! command takes the run whose worktree contains the current directory, else the only unfinished
//! run of the repository containing it, else it lists the candidates, each with its exact command,
//! and exits 1 (spec, "Commands you type"). The reference-only test
//! `contract_status.StatusWithoutRunDir` pinned the Python's old choice (the newest unfinished run
//! under `docs_dir`, from any repository); it is not converted.

#[macro_use]
mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use common::json::Json;
use common::rundir::{RunDir, append_event, other_repos_run, outcome};
use common::supervisor::{CardOpts, FakeSupervisor, card, card_with};
use common::{Out, STATUS_KEYS, is_bin_command, key_set, read_json, set_of, shlex_quote};

/// stderr's lines.
fn stderr_lines(r: &Out) -> Vec<String> {
    r.stderr_text().lines().map(str::to_string).collect()
}

mod status_finds_the_run {
    use super::*;

    #[track_caller]
    fn status(w: &FakeSupervisor, args: &[&OsStr], cwd: &Path) -> Out {
        let mut argv = vec![OsStr::new("status")];
        argv.extend_from_slice(args);
        common::cli(&argv, &w.env, cwd)
    }

    #[test]
    fn test_status_without_argument_picks_the_current_repos_only_unfinished_run() {
        let mut w = FakeSupervisor::new(&[card_with(
            1,
            CardOpts {
                title: Some("Mine"),
                ..CardOpts::default()
            },
        )]);
        let run_dir = w.start();
        let (_other, elsewhere) = other_repos_run(&w.docs);
        let named = status(&w, &os_args![run_dir], &w.tmp).ok();
        // From the repository's top and from a subdirectory of it, never the other
        // repository's newer run.
        let sub = w.repo.join("src deep");
        fs::create_dir(&sub).unwrap();
        for cwd in [&w.repo, &sub] {
            let r = status(&w, &[], cwd).ok();
            assert_eq!(r.stdout, named.stdout, "cwd={}\n{r}", cwd.display());
            assert!(
                r.stdout_text().contains("Card 1: Mine · "),
                "cwd={}\n{r}",
                cwd.display()
            );
        }
        assert_ne!(
            status(&w, &os_args![elsewhere], &w.tmp).ok().stdout,
            named.stdout
        );

        // A finished run of the same repository is not a candidate.
        append_event(&run_dir, "outcome", outcome("complete", "", &[]));
        let r = status(&w, &[], &w.repo);
        assert_eq!(r.code, 1, "{r}");
        let lines = stderr_lines(&r);
        assert!(
            lines
                .first()
                .is_some_and(|l| l.starts_with("error: no unfinished run of ")),
            "{r}"
        );
        assert_eq!(r.stdout_text(), "", "{r}");
    }

    #[test]
    fn test_status_lists_candidates_when_ambiguous() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        let first = w.start();
        let second = w.start_new();
        assert_ne!(first, second);
        let r = status(&w, &[], &w.repo);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
        let lines = stderr_lines(&r);
        assert!(
            lines
                .first()
                .is_some_and(|l| l.starts_with("error: 2 unfinished runs of ")),
            "{r}"
        );
        // Each candidate's own command, alone on its line, ready to copy, and nothing else.
        let commands = &lines[1..];
        assert_eq!(commands.len(), 2, "{r}");
        for run_dir in [&first, &second] {
            let after = format!(" status {}", shlex_quote(run_dir.to_str().unwrap()));
            assert!(
                commands.iter().any(|line| is_bin_command(line, "", &after)),
                "{}\n{r}",
                run_dir.display()
            );
        }
        // The command works as printed.
        for line in commands {
            let argv = common::shlex_split(line)
                .unwrap_or_else(|| panic!("{line:?} does not split as a shell line"));
            let args: Vec<&OsStr> = argv[1..].iter().map(OsStr::new).collect();
            let done = common::cli(&args, &w.env, &w.tmp);
            assert_eq!(done.code, 0, "{done}");
        }

        // `report` lists its own commands.
        let r = common::cli(&os_args!["report"], &w.env, &w.repo);
        assert_eq!(r.code, 1, "{r}");
        let lines = stderr_lines(&r);
        for run_dir in [&first, &second] {
            let after = format!(" report {}", shlex_quote(run_dir.to_str().unwrap()));
            assert!(
                lines
                    .iter()
                    .skip(1)
                    .any(|line| is_bin_command(line, "", &after)),
                "{}\n{r}",
                run_dir.display()
            );
        }
    }

    #[test]
    fn test_status_inside_a_worktree_finds_its_run_by_scan() {
        let title = |t| {
            card_with(
                1,
                CardOpts {
                    title: Some(t),
                    ..CardOpts::default()
                },
            )
        };
        let mut w = FakeSupervisor::new(&[title("First run")]);
        let first = w.start();
        let first_ws = w.workspace();
        w.write_deck_with(&[title("Second run")], "Other deck", "leadhand/demo");
        let second = w.start_new();
        let second_ws = w.workspace();
        assert_ne!(first_ws, second_ws);
        // From the repository both runs are candidates; from inside a run's worktree (its
        // top or a subdirectory), that run.
        assert_eq!(status(&w, &[], &w.repo).code, 1);
        for (run_dir, ws, title) in [
            (&first, &first_ws, "First run"),
            (&second, &second_ws, "Second run"),
        ] {
            let deep = ws.join("deep dir");
            if !deep.is_dir() {
                fs::create_dir(&deep).unwrap();
            }
            for cwd in [ws, &deep] {
                let r = status(&w, &[], cwd).ok();
                let want = status(&w, &os_args![run_dir], &w.tmp).ok();
                assert_eq!(r.stdout, want.stdout, "cwd={}\n{r}", cwd.display());
                assert!(
                    r.stdout_text().contains(&format!("Card 1: {title} · ")),
                    "cwd={}\n{r}",
                    cwd.display()
                );
            }
            let r = common::cli(&os_args!["report"], &w.env, &deep).ok();
            assert_eq!(
                r.stdout_text(),
                format!("{}\n", run_dir.join("report.md").display()),
                "{r}"
            );
        }
    }

    #[test]
    fn test_status_names_a_run_by_id_name_slug_or_deck() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        let run_dir = w.start();
        let want = status(&w, &os_args![run_dir], &w.tmp).ok().stdout;
        let meta = w.meta();
        let args = [
            meta["run_id"].as_os().unwrap(),
            run_dir.file_name().unwrap().to_os_string(),
            meta["slug"].as_os().unwrap(),
            run_dir.join("deck.md").into_os_string(),
        ];
        for arg in &args {
            let r = status(&w, &os_args![arg], &w.repo).ok();
            assert_eq!(r.stdout, want, "arg={arg:?}\n{r}");
        }
        let r = status(&w, &os_args!["no-such-run"], &w.repo);
        assert_eq!(r.code, 1, "{r}");
        assert!(
            r.stderr_text()
                .starts_with("error: no run named no-such-run"),
            "{r}"
        );
    }
}

mod json {
    use super::*;

    /// `d` without member `key`, which it must have (`dict.pop(key)`).
    #[track_caller]
    fn pop(d: &Json, key: &str) -> Json {
        let members = d
            .as_object()
            .unwrap_or_else(|| panic!("not an object: {d}"));
        assert!(d.get(key).is_some(), "no {key:?} in {d}");
        Json::Obj(
            members
                .iter()
                .filter(|(k, _)| k.as_slice() != key.as_bytes())
                .cloned()
                .collect(),
        )
    }

    #[test]
    fn test_status_json() {
        let reason = "needs a décision \u{1f600}";
        let w = RunDir::with_cards(3);
        w.accept(1);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(2)),
                ("branch", Json::from("leadhand/demo-2/c2")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "parked",
            [("card", Json::from(2)), ("reason", Json::from(reason))],
        );
        w.dispatch(3);
        w.charge_with(
            "c1",
            "implementer",
            common::rundir::ChargeOpts {
                tokens: (1000, 100),
                ..common::rundir::ChargeOpts::default()
            },
        );
        let parent = w.root.parent().unwrap();
        let text = common::cli(&os_args!["status", w.root], &w.env, parent).ok();
        let r = common::cli(&os_args!["status", w.root, "--json"], &w.env, parent).ok();
        assert_eq!(r.stderr_text(), "", "{r}");
        assert_eq!(r.stdout.iter().filter(|&&b| b == b'\n').count(), 1, "{r}");
        assert!(r.stdout.is_ascii(), "{r}"); // non-ASCII text as \u escapes
        let s = r.json();
        assert_eq!(key_set(&s), set_of(&STATUS_KEYS), "{r}");
        // The same information as the text output.
        let stdout = text.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_json_eq!(s["headline"], Json::from(lines[0]));
        let n = s["attention"].as_array().expect("an attention list").len();
        assert_json_eq!(s["attention"], Json::from(&lines[1..1 + n]));
        let first = s["attention"][0].as_str().unwrap_or("");
        assert!(
            first.starts_with(&format!("Card 2 parked: {reason} — next: ")),
            "{s}"
        );
        assert_json_eq!(
            Json::from(vec![
                s["run_id"].clone(),
                s["deck"].clone(),
                s["state"].clone()
            ]),
            Json::from(&["abc123", "Demo deck", "running"][..])
        );
        assert_json_eq!(
            s["progress"],
            common::json::parse(br#"{"accepted": 1, "total": 3}"#)
        );
        assert_json_eq!(
            s["cards"][1],
            Json::object([
                ("number", Json::from(2)),
                ("title", Json::from("Card number 2")),
                ("state", Json::from("parked")),
                ("attempt", Json::from(0)),
                ("route", Json::from("")),
                ("note", Json::from(reason)),
            ])
        );
        assert_json_eq!(
            s["cards"][2]["route"],
            Json::from("codex gpt-6.1-terra high")
        );
        assert_eq!(
            key_set(&s["usage"]),
            set_of(&[
                "estimated_usd",
                "unknown_attempts",
                "by_role",
                "codex_meter"
            ]),
            "{s}"
        );
        assert_json_eq!(
            s["usage"]["by_role"]["implementer"]["input"],
            Json::from(1000)
        );
        assert_json_eq!(
            s["usage"]["codex_meter"],
            common::json::parse(br#"{"percent": null, "observed_at": null}"#)
        );
    }

    #[test]
    fn test_status_json_matches_the_status_json_a_checkpoint_writes() {
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        let run_dir = w.start();
        w.next();
        let written = read_json(&w.paths().status);
        let s = common::cli(&os_args!["status", run_dir, "--json"], &w.env, &w.tmp)
            .ok()
            .json();
        let written = pop(&written, "updated_at");
        let s = pop(&s, "updated_at");
        assert_json_eq!(s, written);
    }

    #[test]
    fn test_report_json() {
        let w = RunDir::with_cards(1);
        w.accept(1);
        w.ev("outcome", outcome("complete", "", &[]));
        let parent = w.root.parent().unwrap();
        // The run under a non-ASCII name, so the report's path in the JSON needs \u escapes.
        let root = parent.join("run dé ✓");
        common::rundir::copy_tree(&w.root, &root);
        let report = common::rundir::run_paths(&root).report;
        let report_text = report.to_str().unwrap();
        assert!(!report_text.is_ascii(), "{report_text}");
        let text = common::cli(&os_args!["report", root], &w.env, parent).ok();
        assert_eq!(
            text.stdout_text(),
            format!("{}\n", report.display()),
            "{text}"
        );
        fs::remove_file(&report).unwrap();
        let r = common::cli(&os_args!["report", root, "--json"], &w.env, parent).ok();
        assert_eq!(r.stderr_text(), "", "{r}");
        assert_json_eq!(
            r.json(),
            Json::object([("report", Json::from(report_text))]),
            "{r}"
        );
        assert!(
            fs::read_to_string(&report)
                .unwrap()
                .starts_with("# Run report: Demo deck\n\nOutcome: complete\n")
        );
        // json.loads(r.stdout) == r.json(): an independent decoder (the output is ASCII)
        assert!(r.stdout.is_ascii(), "{r}"); // non-ASCII text as \u escapes
        let plain: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
        assert_json_eq!(common::json::parse(plain.to_string().as_bytes()), r.json());
        assert_eq!(plain["report"].as_str(), Some(report_text), "{r}");
    }
}
