//! Helpers that several engine test files share: the module helpers of
//! `tests/contract/contract_cards.py` that other contract files import (`codex_world`,
//! `record`, `recorded`, `result`, `run_line`, `relaunch_env`, `status_lines`, `commit_on` and
//! the deck constants), with `contract_deck.card_section` and `contract_status.is_unpark_line`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::git::run_git;
use super::json::Json;
use super::supervisor::{CliOpts, FakeSupervisor, Options, Step, run_dir, text};
use super::{Out, is_bin_command, process_env, shlex_quote};

/// The run branch of the demo deck.
pub const RUN: &str = "leadhand/demo";
/// Card 1's branch on the run branch `RUN`.
pub const CARD1: &str = "leadhand/demo--card-1";
/// A card route to Codex.
pub const CODEX_CARD: &str = "codex gpt-6-luna low";
/// The user config that reviews cards by risk.
pub const BY_RISK: &str = "[run]\ncard_review = \"by-risk\"\n";

/// A world with Claude and Codex installed, and the fake `codex` as Codex
/// (LEADHAND_TEST_CODEX_BIN). `o.installed` is ignored.
#[track_caller]
pub fn codex_world(cards: &[String], o: Options) -> FakeSupervisor {
    let mut w = FakeSupervisor::with_options(
        cards,
        Options {
            installed: &["claude", "codex"],
            ..o
        },
    );
    let codex = w.env.fakes().join("codex");
    w.env.set("LEADHAND_TEST_CODEX_BIN", codex);
    w
}

/// A step whose report says `Status: <status>` (Python's `{"status": status}`).
pub fn with_status(status: &str) -> Step {
    Step {
        status: Some(status.into()),
        ..Step::default()
    }
}

/// A review step with `verdict` (Python's `{"verdict": verdict}`).
pub fn with_verdict(verdict: &str) -> Step {
    Step {
        verdict: Some(verdict.into()),
        ..Step::default()
    }
}

/// `record RUN KIND FLAGS` through the world's `cli`.
#[track_caller]
pub fn record(w: &FakeSupervisor, kind: &str, flags: &[&OsStr]) -> Out {
    record_with(w, kind, flags, None)
}

/// `record`, with `stdin` written to the binary's standard input.
#[track_caller]
pub fn record_with(w: &FakeSupervisor, kind: &str, flags: &[&OsStr], stdin: Option<&str>) -> Out {
    let rd = run_dir(w);
    let mut args = vec![OsStr::new("record"), rd.as_os_str(), OsStr::new(kind)];
    args.extend_from_slice(flags);
    w.cli_with(
        &args,
        CliOpts {
            stdin,
            ..CliOpts::default()
        },
    )
}

/// `record KIND FLAGS`, which must succeed; the event it prints.
#[track_caller]
pub fn recorded(w: &FakeSupervisor, kind: &str, flags: &[&OsStr]) -> Json {
    record(w, kind, flags).ok().json()
}

/// `recorded` with `stdin`.
#[track_caller]
pub fn recorded_with(w: &FakeSupervisor, kind: &str, flags: &[&OsStr], stdin: &str) -> Json {
    record_with(w, kind, flags, Some(stdin)).ok().json()
}

/// `record result` for the dispatch action `a` by `worker`, with the reply `reply` and
/// `flags`; it must succeed.
#[track_caller]
pub fn result(w: &FakeSupervisor, a: &Json, worker: &str, reply: &str, flags: &[&OsStr]) -> Json {
    let mut args = vec![
        OsStr::new("--dispatch"),
        OsStr::new(text(&a["dispatch_id"])),
        OsStr::new("--worker"),
        OsStr::new(worker),
        OsStr::new("--reply"),
        OsStr::new(reply),
    ];
    args.extend_from_slice(flags);
    recorded(w, "result", &args)
}

/// The run directory `start` names: its `run: <dir>` or `existing run: <dir>` line.
#[track_caller]
pub fn run_line(r: &Out) -> PathBuf {
    let stdout = r.stdout_text();
    for line in stdout.lines() {
        for prefix in ["run: ", "existing run: "] {
            if let Some(dir) = line.strip_prefix(prefix) {
                return PathBuf::from(dir);
            }
        }
    }
    panic!("leadhand start printed no `run: <dir>` line\n{r}");
}

/// `run` with the fake `claude` as the `claude` found on PATH (it exits at once): the
/// variables for `CliOpts::env`.
pub fn relaunch_env(w: &FakeSupervisor) -> Vec<(OsString, OsString)> {
    let mut path = w.env.fakes().as_os_str().to_os_string();
    path.push(":");
    if let Some((_, own)) = process_env().into_iter().rev().find(|(k, _)| k == "PATH") {
        path.push(own);
    }
    vec![("PATH".into(), path)]
}

/// `status RUN`'s lines; it must succeed.
#[track_caller]
pub fn status_lines(w: &FakeSupervisor) -> Vec<String> {
    let rd = run_dir(w);
    w.cli(&[OsStr::new("status"), rd.as_os_str()])
        .ok()
        .stdout_text()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A commit with `parent` as its parent, made without touching any branch or worktree.
#[track_caller]
pub fn commit_on(repo: &Path, parent: &str) -> String {
    let tree = run_git(repo, &["rev-parse", &format!("{parent}^{{tree}}")]);
    run_git(
        repo,
        &["commit-tree", &tree, "-p", parent, "-m", "outside commit"],
    )
}

/// The lines of card `number` as the brief renders it, from its heading to the next heading.
#[track_caller]
pub fn card_section(brief: &str, number: i64) -> Vec<String> {
    let lines: Vec<&str> = brief.lines().collect();
    let heading = format!("## Card {number}:");
    let start = lines
        .iter()
        .position(|l| l.starts_with(&heading))
        .unwrap_or_else(|| panic!("no {heading:?} in the brief:\n{brief}"));
    let end = (start + 1..lines.len())
        .find(|&i| lines[i].starts_with('#'))
        .unwrap_or(lines.len());
    lines[start..end].iter().map(|s| s.to_string()).collect()
}

/// The parked card's attention line, its next step naming the binary (by file: S1-R11).
pub fn is_unpark_line(line: &str, n: i64, root: &Path, reason: &str) -> bool {
    is_bin_command(
        line,
        &format!("Card {n} parked: {reason} — next: "),
        &format!(
            " unpark {} --card {n} --note '…'",
            shlex_quote(root.to_str().unwrap())
        ),
    )
}
