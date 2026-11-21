//! Helpers that several engine test files share: the module helpers of
//! `tests/contract/contract_cards.py` that other contract files import (`codex_world`,
//! `record`, `recorded`, `result`, `run_line`, `relaunch_env`, `status_lines`, `commit_on` and
//! the deck constants), with `contract_deck.card_section` and `contract_status.is_unpark_line`;
//! those of `contract_integration.py` (`NOTE`, `parked_world`, `unpark`, with `requested`,
//! `found_by` and `found`, which `contract_worktree_safety.py` defines again) and of
//! `contract_finish.py` (`base_wt`, `marker`, `gate_names`, `drive_to_run`,
//! `drive_until_refused`, `to_last_step`) that later files import; and the step literals the
//! files script (`with_status`, `with_verdict`, `files`, `tampering`), with the small helpers
//! those files share (`gated`, `gated_with`, `run_id`, `seq`, `last`, `write_script`,
//! `path_before`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::git::run_git;
use super::json::Json;
use super::supervisor::{CliOpts, FakeSupervisor, Options, Step, card, run_dir, text, truthy};
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

/// A world with `cards` that runs the real end of run (`FakeSupervisor(..., gates=True)`).
#[track_caller]
pub fn gated(cards: &[String]) -> FakeSupervisor {
    gated_with(cards, "")
}

/// `gated` with the user config `config`.
#[track_caller]
pub fn gated_with(cards: &[String], config: &str) -> FakeSupervisor {
    FakeSupervisor::with_options(
        cards,
        Options {
            config,
            gates: true,
            ..Options::default()
        },
    )
}

/// The run's id (`w.meta()["run_id"]`).
#[track_caller]
pub fn run_id(w: &FakeSupervisor) -> String {
    text(&w.meta()["run_id"]).to_string()
}

/// An event's `seq`.
#[track_caller]
pub fn seq(e: &Json) -> i64 {
    e["seq"]
        .as_i64()
        .unwrap_or_else(|| panic!("no integer seq: {e}"))
}

/// The last `n` items (`items[-n:]`).
pub fn last<T>(items: &[T], n: usize) -> &[T] {
    &items[items.len().saturating_sub(n)..]
}

/// Writes an executable script at `path` (`write_text`, then `chmod(0o755)`).
#[track_caller]
pub fn write_script(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("making {} executable: {e}", path.display()));
}

/// `"<dir>:<the world's PATH>"` (`f"{dir}{os.pathsep}{w.env['PATH']}"`).
#[track_caller]
pub fn path_before(w: &FakeSupervisor, dir: &Path) -> OsString {
    let mut path = dir.as_os_str().to_os_string();
    path.push(":");
    path.push(w.env.get("PATH").expect("PATH in the world's environment"));
    path
}

/// A step that writes and commits `files` (Python's `{"files": {...}}`).
pub fn files(pairs: &[(&str, &str)]) -> Step {
    Step {
        files: Some(
            pairs
                .iter()
                .map(|(p, t)| (p.to_string(), t.to_string()))
                .collect(),
        ),
        ..Step::default()
    }
}

/// A step whose worker runs `tamper(world, workspace, action)` before its report (Python's
/// `{"tamper": tamper}`).
pub fn tampering(tamper: impl Fn(&FakeSupervisor, &Path, &Json) + 'static) -> Step {
    Step {
        tamper: Some(Box::new(tamper)),
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

// --------------------------------------------------------------------------
// contract_integration.py

/// The operator's note in an unpark.
pub const NOTE: &str = "Use JSON; it is simpler.";

/// `parked_world_with(&[], &[], &["claude"])`.
#[track_caller]
pub fn parked_world() -> FakeSupervisor {
    parked_world_with(&[], &[], &["claude"])
}

/// Card 1 asks a product question and is parked; independent card 2 is accepted, so the run
/// tip moves past card 1's base. `card1_files` and `card2_files` (none when empty) are what
/// each card's implementer writes; with Codex `installed`, the fake `codex` is Codex, as in
/// `codex_world`.
#[track_caller]
pub fn parked_world_with(
    card1_files: &[(&str, &str)],
    card2_files: &[(&str, &str)],
    installed: &[&str],
) -> FakeSupervisor {
    let mut w = FakeSupervisor::with_options(
        &[card(1), card(2)],
        Options {
            installed,
            ..Options::default()
        },
    );
    if installed.contains(&"codex") {
        let codex = w.env.fakes().join("codex");
        w.env.set("LEADHAND_TEST_CODEX_BIN", codex);
    }
    w.session("supervisor-1"); // the supervisor session the workers start in
    w.start();
    let first = if card1_files.is_empty() {
        Step::default()
    } else {
        files(card1_files)
    };
    w.script(
        1,
        "implement",
        [Step {
            status: Some("QUESTION".into()),
            ..first
        }],
    );
    if !card2_files.is_empty() {
        w.script(2, "implement", [files(card2_files)]);
    }
    w.parks
        .insert(1, "product decision: storage format".to_string());
    w.run();
    w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
    w
}

/// The `dispatch_requested` event of `dispatch_id` (Python's `next(...)`: a panic when none).
#[track_caller]
pub fn requested(w: &FakeSupervisor, dispatch_id: &str) -> Json {
    w.events("dispatch_requested")
        .into_iter()
        .find(|e| e["dispatch_id"] == dispatch_id)
        .unwrap_or_else(|| panic!("no dispatch_requested for {dispatch_id}"))
}

/// The `found` violations recorded by `by` (an event without `by` is the check's).
#[track_caller]
pub fn found_by(w: &FakeSupervisor, by: &str) -> Vec<Json> {
    w.events("violation")
        .into_iter()
        .filter(|e| {
            e.get("stage").is_some_and(|s| s == "found")
                && e.get("by").filter(|b| truthy(b)).map_or("check", text) == by
        })
        .collect()
}

/// `found_by(w, "check")`.
#[track_caller]
pub fn found(w: &FakeSupervisor) -> Vec<Json> {
    found_by(w, "check")
}

/// `unpark RUN_DIR --card N --note NOTE`, which must succeed.
#[track_caller]
pub fn unpark(w: &FakeSupervisor, n: i64, note: &str) -> Out {
    let rd = run_dir(w);
    let card = n.to_string();
    w.cli(&[
        OsStr::new("unpark"),
        rd.as_os_str(),
        OsStr::new("--card"),
        OsStr::new(&card),
        OsStr::new("--note"),
        OsStr::new(note),
    ])
    .ok()
}

// --------------------------------------------------------------------------
// contract_finish.py

/// In the evidence base worktree's admin folder: the run that added it (R60).
pub const MARKER: &str = "leadhand-run";

/// The evidence base worktree: `<repo>/.worktrees/<workspace name>--base`.
#[track_caller]
pub fn base_wt(w: &FakeSupervisor) -> PathBuf {
    let ws = w.workspace();
    let mut name = ws
        .file_name()
        .unwrap_or_else(|| panic!("a workspace with no name: {}", ws.display()))
        .to_os_string();
    name.push("--base");
    ws.parent().expect("the workspace's parent").join(name)
}

/// The run's marker in the admin folder of the worktree at `path`.
#[track_caller]
pub fn marker(path: &Path) -> PathBuf {
    PathBuf::from(run_git(path, &["rev-parse", "--absolute-git-dir"])).join(MARKER)
}

/// Every gate result's gate, in order.
#[track_caller]
pub fn gate_names(w: &FakeSupervisor) -> Vec<String> {
    w.events("gate_result")
        .iter()
        .map(|e| text(&e["gate"]).to_string())
        .collect()
}

/// Acts on every action until a run-level dispatch matching `pred`; returns it unacted.
#[track_caller]
pub fn drive_to_run(w: &mut FakeSupervisor, pred: impl Fn(&Json) -> bool) -> Json {
    for _ in 0..80 {
        let a = w.next();
        if a["action"] == "dispatch" && a["card"].is_null() && pred(&a) {
            return a;
        }
        w.act(&a);
    }
    panic!("no matching run-level dispatch");
}

/// Acts on every action until `next` fails (within 80 actions); returns that failing result.
#[track_caller]
pub fn drive_until_refused(w: &mut FakeSupervisor) -> Out {
    let limit = 80;
    let rd = run_dir(w);
    for _ in 0..limit {
        let r = w.cli(&[OsStr::new("next"), rd.as_os_str()]);
        if r.code != 0 {
            return r;
        }
        let a = r.json();
        if a["action"] == "done" {
            panic!("the run ended without the refusal: {a}");
        }
        w.act(&a);
    }
    panic!("`next` was never refused in {limit} actions");
}

/// Acts on every action up to and including the run's verify gate: the next `next` ends the
/// run.
#[track_caller]
pub fn to_last_step(w: &mut FakeSupervisor) {
    let a = drive_to_run(w, |a| a["kind"] == "verify");
    w.act(&a);
}
