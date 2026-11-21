//! `pause`, `cancel` and `unpark` in Rust: the conventions for the commands a person types.
//! Rust-only.
//!
//! The Rust form of `tests/contract/contract_rust_control.py`. Each finds its run as
//! `status` does (spec, "Commands you type"): by its directory, its `deck.md`, its run ID,
//! its directory's name or its slug, or with nothing named, the run whose worktree contains
//! the current directory, else the only unfinished run of the repository containing it, else
//! it lists the candidates, each with its exact command, and exits 1. Each takes `--json`:
//! the same information as the text output, as one JSON object. An error's first line is
//! `error: ...`; when one command fixes it, that command is alone on the last line.
//!
//! The flags, events and texts themselves are the Python's (contract_recovery,
//! contract_finish): a Python process may still read them in a mixed configuration.

#[macro_use]
mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use common::engine::NOTE;
use common::json::Json;
use common::rundir::{append_event, other_repos_run, outcome};
use common::supervisor::{FakeSupervisor, card, run_dir, text};
use common::{
    Out, cli, is_bin_command, key_set, lines, one, os_vec, path_str as s, pick, read_json, set_of,
    shlex_join, shlex_quote, shlex_split,
};

const PAUSE_HEAD: &str = "pause requested: running workers finish, nothing new starts, and the run ends blocked (paused) with a handoff. Continue it later with: ";
const CANCEL_HEAD: &str = "cancel requested: every running worker is stopped and the run ends incomplete (cancelled); partial work stays on the card branches. If no supervisor is running, ";
const CANCEL_TAIL: &str = " finishes the cancel.";

/// What the text output of `request` (pause or cancel) is around the CLI's own path.
fn flag_text(request: &str, run_dir: &Path) -> (&'static str, String) {
    let run = format!(" run {}", shlex_quote(s(run_dir)));
    if request == "pause" {
        (PAUSE_HEAD, run)
    } else {
        (CANCEL_HEAD, run + CANCEL_TAIL)
    }
}

/// A run whose card 1 is dispatched, then parked by the supervisor.
#[track_caller]
fn parked_world() -> FakeSupervisor {
    let mut w = FakeSupervisor::new(&[card(1), card(2)]);
    w.start();
    w.next();
    park(&w, "hold");
    w
}

/// `record RUN park --card 1 --reason REASON`, which must succeed.
#[track_caller]
fn park(w: &FakeSupervisor, reason: &str) {
    w.cli(&os_args![
        "record",
        run_dir(w),
        "park",
        "--card",
        "1",
        "--reason",
        reason
    ])
    .ok();
}

/// The binary with `args` in the world's own environment (no clock offset), from `cwd`
/// (`run_cli(..., env=w.env, cwd=cwd)`).
#[track_caller]
fn run_cli(w: &FakeSupervisor, args: &[&OsStr], cwd: &Path) -> Out {
    cli(args, &w.env, cwd)
}

/// The flag file `request` writes.
fn flag_of(w: &FakeSupervisor, request: &str) -> PathBuf {
    let paths = w.paths();
    if request == "pause" {
        paths.pause_flag
    } else {
        paths.cancel_flag
    }
}

/// The text with its trailing newlines removed (`str.rstrip("\n")`).
fn rstrip_newlines(text: &str) -> &str {
    text.trim_end_matches('\n')
}

mod control_finds_the_run {
    use super::*;

    #[test]
    fn test_pause_and_cancel_without_argument_take_the_current_repos_only_unfinished_run() {
        if common::rust_only() {
            return;
        }
        for request in ["pause", "cancel"] {
            let mut w = FakeSupervisor::new(&[card(1)]);
            let run_dir = w.start();
            let (_other, elsewhere) = other_repos_run(&w.docs);
            let flag = flag_of(&w, request);
            let sub = w.repo.join("src deep");
            fs::create_dir(&sub).unwrap();
            let ws = w.workspace();
            let deep = ws.join("deep dir");
            fs::create_dir(&deep).unwrap();
            // From the repository's top, a subdirectory of it, and inside the run's worktree:
            // never the other repository's newer unfinished run.
            for cwd in [&w.repo, &sub, &ws, &deep] {
                let at = format!("request={request} cwd={}", cwd.display());
                let r = run_cli(&w, &os_args![request], cwd).ok();
                assert_eq!(r.stderr_text(), "", "{at}\n{r}");
                let (before, after) = flag_text(request, &run_dir);
                assert!(
                    is_bin_command(rstrip_newlines(&r.stdout_text()), before, &after),
                    "{at}\n{r}"
                );
                assert!(flag.is_file(), "{at}\n{r}");
                assert!(!elsewhere.join(request).exists(), "{at}");
                let written = read_json(&flag);
                assert_eq!(
                    key_set(&written),
                    set_of(&["requested_at", "by_pid"]),
                    "{at}"
                );
                fs::remove_file(&flag).unwrap();
            }
        }
    }

    #[test]
    fn test_unpark_without_argument_takes_the_run_of_the_worktree_it_is_in() {
        if common::rust_only() {
            return;
        }
        let w = parked_world();
        let deep = w.workspace().join("deep dir");
        fs::create_dir(&deep).unwrap();
        let r = run_cli(
            &w,
            &os_args!["unpark", "--card", "1", "--note", NOTE],
            &deep,
        )
        .ok();
        assert_eq!(r.stdout_text(), "card 1 unparked\n", "{r}");
        let event = one(w.events("unparked"));
        assert_json_eq!(
            pick(&event, &["card", "note"]),
            Json::Arr(vec![1.into(), NOTE.into()])
        );
    }

    #[test]
    fn test_each_command_names_a_run_by_id_name_slug_or_deck() {
        if common::rust_only() {
            return;
        }
        let w = parked_world();
        let rd = run_dir(&w);
        let meta = w.meta();
        let names: Vec<OsString> = vec![
            text(&meta["run_id"]).into(),
            rd.file_name().unwrap().to_os_string(),
            text(&meta["slug"]).into(),
            rd.join("deck.md").into_os_string(),
        ];
        for arg in &names {
            for request in ["pause", "cancel"] {
                let flag = flag_of(&w, request);
                let r = run_cli(&w, &os_args![request, arg], &w.repo).ok();
                let (before, after) = flag_text(request, &rd);
                assert!(
                    is_bin_command(rstrip_newlines(&r.stdout_text()), before, &after),
                    "arg={arg:?}\n{r}"
                );
                fs::remove_file(&flag).unwrap();
            }
        }
        for arg in &names {
            if !w.events("unparked").is_empty() {
                park(&w, "hold again");
            }
            run_cli(
                &w,
                &os_args!["unpark", arg, "--card", "1", "--note", NOTE],
                &w.repo,
            )
            .ok();
        }
        assert_eq!(w.events("unparked").len(), names.len());
        let r = run_cli(&w, &os_args!["pause", "no-such-run"], &w.repo);
        assert_eq!(r.code, 1, "{r}");
        assert!(
            r.stderr_text()
                .starts_with("error: no run named no-such-run"),
            "{r}"
        );
    }

    #[test]
    fn test_candidates_are_listed_with_their_own_commands_when_ambiguous() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        let first = w.start();
        let second = w.start_new();
        assert_ne!(first, second);
        let unpark_after = format!(" --card 1 --note {}", shlex_quote(NOTE));
        for (request, after) in [
            ("pause", String::new()),
            ("cancel", String::new()),
            ("unpark", unpark_after),
        ] {
            let mut args: Vec<&OsStr> = vec![OsStr::new(request)];
            if request == "unpark" {
                args.extend(os_args!["--card", "1", "--note", NOTE]);
            }
            let r = run_cli(&w, &args, &w.repo);
            assert_eq!(r.code, 1, "request={request}\n{r}");
            assert_eq!(r.stdout_text(), "", "request={request}\n{r}");
            let lines = lines(&r.stderr_text());
            assert!(
                lines[0].starts_with("error: 2 unfinished runs of "),
                "request={request}\n{r}"
            );
            let commands = &lines[1..];
            assert_eq!(commands.len(), 2, "request={request}\n{r}");
            for run_dir in [&first, &second] {
                let want = format!(" {request} {}{after}", shlex_quote(s(run_dir)));
                assert!(
                    commands.iter().any(|line| is_bin_command(line, "", &want)),
                    "request={request} run_dir={}\n{r}",
                    run_dir.display()
                );
            }
            assert!(
                !(first.join("pause").exists() || second.join("pause").exists()),
                "request={request}"
            );
            assert!(
                !(first.join("cancel").exists() || second.join("cancel").exists()),
                "request={request}"
            );
        }
        // A printed command works as printed.
        let r = run_cli(&w, &os_args!["pause"], &w.repo);
        let printed = &lines(&r.stderr_text())[1];
        let argv = shlex_split(printed).unwrap_or_else(|| panic!("not shell words: {printed}"));
        let rest: Vec<&OsStr> = argv[1..].iter().map(OsStr::new).collect();
        run_cli(&w, &rest, &w.tmp).ok();
        assert_eq!(
            [&first, &second]
                .iter()
                .filter(|d| d.join("pause").exists())
                .count(),
            1
        );
    }
}

/// `r` printed one ASCII JSON object on one line and nothing on stderr; gives it.
macro_rules! assert_one_object {
    ($r:expr) => {{
        let r: Out = $r;
        assert_eq!(r.stderr_text(), "", "{r}");
        assert_eq!(r.stdout_text().matches('\n').count(), 1, "{r}");
        assert!(r.stdout.is_ascii(), "{r}");
        r.json()
    }};
}

mod json {
    use super::*;

    #[test]
    fn test_pause_and_cancel_json() {
        if common::rust_only() {
            return;
        }
        for request in ["pause", "cancel"] {
            let mut w = FakeSupervisor::new(&[card(1)]);
            let run_dir = w.start();
            let flag = flag_of(&w, request);
            let out = run_cli(&w, &os_args![request, run_dir], &w.tmp)
                .ok()
                .stdout_text();
            let text_ = rstrip_newlines(&out);
            fs::remove_file(&flag).unwrap();
            let s_ =
                assert_one_object!(run_cli(&w, &os_args![request, run_dir, "--json"], &w.tmp).ok());
            assert!(flag.is_file(), "request={request}");
            assert_eq!(
                key_set(&s_),
                set_of(&["run", "requested", "command", "message"]),
                "{s_}"
            );
            assert_json_eq!(
                pick(&s_, &["run", "requested", "message"]),
                Json::Arr(vec![s(&run_dir).into(), request.into(), text_.into()])
            );
            // The command the text names, alone.
            let command = text(&s_["command"]);
            let want = format!(" run {}", shlex_quote(s(&run_dir)));
            assert!(is_bin_command(command, "", &want), "{s_}");
            assert!(text_.contains(command), "{s_}");
        }
    }

    #[test]
    fn test_unpark_json() {
        if common::rust_only() {
            return;
        }
        let note = "Use JSON; keep the décision \u{1f600}";
        let w = parked_world();
        let rd = run_dir(&w);
        let padded = format!("  {note}\n");
        let s_ = assert_one_object!(
            run_cli(
                &w,
                &os_args!["unpark", rd, "--card", "1", "--note", padded, "--json"],
                &w.tmp
            )
            .ok()
        );
        assert_json_eq!(
            s_,
            Json::object([
                ("run", Json::from(s(&rd))),
                ("card", Json::from(1)),
                ("note", Json::from(note)),
                ("message", Json::from("card 1 unparked")),
            ])
        );
        let event = one(w.events("unparked"));
        assert_json_eq!(
            pick(&event, &["card", "note"]),
            Json::Arr(vec![1.into(), note.into()])
        );
    }

    #[test]
    fn test_an_error_with_json_is_still_an_error_line() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.next();
        let r = run_cli(
            &w,
            &os_args![
                "unpark",
                run_dir(&w),
                "--card",
                "1",
                "--note",
                NOTE,
                "--json"
            ],
            &w.tmp,
        );
        assert_eq!((r.code, r.stdout_text().as_str()), (1, ""), "{r}");
        assert_eq!(
            r.stderr_text(),
            "error: card 1 is not parked (it is requested)\n",
            "{r}"
        );
    }
}

mod errors {
    use super::*;

    #[test]
    fn test_unpark_on_a_run_that_cannot_continue_ends_with_the_start_command() {
        if common::rust_only() {
            return;
        }
        let w = parked_world();
        let rd = run_dir(&w);
        append_event(&rd, "outcome", outcome("incomplete", "", &[]));
        let r = run_cli(
            &w,
            &os_args!["unpark", rd, "--card", "1", "--note", NOTE],
            &w.tmp,
        );
        assert_eq!((r.code, r.stdout_text().as_str()), (1, ""), "{r}");
        let lines = lines(&r.stderr_text());
        assert_eq!(
            lines[0],
            "error: this run has ended: incomplete; a parked card can only continue in a \
             blocked or unfinished run. Start a new run from the deck with:",
            "{r}"
        );
        assert_eq!(lines.len(), 2, "{r}");
        let meta = w.meta();
        let want = format!(
            " {}",
            shlex_join(&os_vec(&[
                "start",
                text(&meta["deck_source"]),
                "--repo",
                text(&meta["repo"])
            ]))
        );
        assert!(is_bin_command(&lines[1], "", &want), "{r}");
        assert_empty!(w.events("unparked"));
        // The command works as printed: it starts a new run of the deck.
        let argv =
            shlex_split(&lines[1]).unwrap_or_else(|| panic!("not shell words: {}", lines[1]));
        let rest: Vec<&OsStr> = argv[1..].iter().map(OsStr::new).collect();
        let r = run_cli(&w, &rest, &w.tmp).ok();
        assert!(r.stdout_text().contains("run: "), "{r}");
    }

    #[test]
    fn test_pause_and_cancel_on_an_ended_run_are_one_error_line() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let rd = run_dir(&w);
        append_event(&rd, "outcome", outcome("complete", "", &[]));
        for request in ["pause", "cancel"] {
            let r = run_cli(&w, &os_args![request, rd], &w.tmp);
            assert_eq!((r.code, r.stdout_text().as_str()), (1, ""), "{r}");
            assert_eq!(
                r.stderr_text(),
                format!("error: this run has ended: complete; there is nothing to {request}\n"),
                "{r}"
            );
            assert!(!rd.join(request).exists(), "request={request}");
        }
    }

    #[test]
    fn test_cancel_of_a_blocked_run_is_requested_and_pause_is_refused() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let rd = run_dir(&w);
        append_event(&rd, "outcome", outcome("blocked", "paused", &[]));
        let r = run_cli(&w, &os_args!["pause", rd], &w.tmp);
        assert_eq!(
            r.stderr_text(),
            "error: this run has ended: blocked (paused); there is nothing to pause\n",
            "{r}"
        );
        run_cli(&w, &os_args!["cancel", rd], &w.tmp).ok();
        assert!(w.paths().cancel_flag.is_file());
    }

    #[test]
    fn test_a_directory_that_is_not_a_run_is_refused() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        for request in ["pause", "cancel"] {
            let r = run_cli(&w, &os_args![request, w.tmp], &w.tmp);
            assert_eq!(
                r.stderr_text(),
                format!(
                    "error: not a run directory (no run.json): {}\n",
                    w.tmp.display()
                ),
                "{r}"
            );
        }
    }
}
