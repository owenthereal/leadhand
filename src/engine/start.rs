//! `start` (`engine.start`, `engine._start`, `engine._new_run`): a new run for a deck, or the
//! unfinished run already started from it.
//!
//! The repository is always the top of a main working tree (deliberate change 1): `--repo`
//! given a subdirectory or a linked worktree resolves to it, and the configuration, `{repo}`
//! in `docs_dir` and the `.worktrees/` folder all come from there. Every run gets its own
//! worktree: `run.worktree = false` is refused with the other unsupported settings.
//!
//! A new run's side effects, in the Python's order (a start interrupted between two of them is
//! retried on a free branch, `contract_recovery`): the run directory, `deck.md`, the
//! configuration snapshot, the fetch of `origin` when the base is the default branch, the
//! excludes, the run branch, the worktree and its run record, `untracked-at-start.json`,
//! `run.json`, and the `run_started` event. The files are what the Python writes, so its
//! `next` continues a run this started.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::human;
use crate::config::{
    Config, ConfigSnapshot, SNAPSHOT, docs_dir_for, load_config, unsupported_warnings,
};
use crate::deck::{Deck, Level, check_deck, parse_deck};
use crate::dispatch::commit::UNTRACKED_AT_START;
use crate::error::{Error, Result};
use crate::git::workspace::head;
use crate::git::{
    Git, RepoIdentity, default_base, prepare_worktree, repo_identity, untracked_files,
};
use crate::pytext::{py_splitlines_bytes, real_path};
use crate::state::events::RunStarted;
use crate::state::{Event, RunMeta, append_event, new_run_dir, read_events, run_paths, utc_now};
use crate::store::{write_file_atomic, write_json_atomic};
use crate::usage::meter::VendorStr;
use crate::wire::{self, PyStr};

/// The longest slug a run's title gives (`engine.SLUG_MAX`).
const SLUG_MAX: usize = 40;
/// What separates a run branch from its card's number in a card branch (`gitops.CARD_INFIX`).
const CARD_INFIX: &[u8] = b"--card-";

/// What `start` did: the run directory, whether it is an existing unfinished run, and the line
/// to print about it, if any (the existing run, or the suffixed branch of a new one).
///
/// The sketch's `note: Option<String>` is an `OsString`: the existing run's note embeds its
/// path (the global type rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartResult {
    pub run_dir: PathBuf,
    pub existing: bool,
    pub note: Option<OsString>,
}

/// Starts a run for the deck at `deck` in the repository `repo_arg` belongs to, or returns the
/// unfinished run already started from that deck there (no outcome, or blocked: R25), unless
/// `new`. Warnings about settings this version does not act on go to stderr.
pub fn start(deck: &Path, repo_arg: &Path, new: bool) -> Result<StartResult> {
    let deck_path = real_path(or_dot(deck));
    let raw = std::fs::read(&deck_path).map_err(|e| Error::io(&deck_path, e))?;
    let parsed = parse_deck(&deck_path)?;
    let given = or_dot(repo_arg);
    let given = std::path::absolute(given).map_err(|e| Error::io(given, e))?;
    let id = repo_identity(&given)?;
    let config = load_config(&id.top)?;
    if hex(&Sha256::digest(&raw)) != parsed.digest {
        return Err(Error::msg(
            "the deck changed while it was being read; start again",
        ));
    }
    refuse_errors(&parsed, &id, &config)?;
    let mut err = std::io::stderr().lock();
    for msg in unsupported_warnings(&config) {
        writeln!(err, "warning: {msg}")?;
    }
    drop(err);
    let docs = docs_dir_for(&config, &id.top);
    if !new {
        let source = PyStr::from_os(deck_path.as_os_str());
        if let Some(existing) = find_unfinished(&docs, &source, &parsed.digest, &id.top)? {
            let mut note = OsString::from("existing run: ");
            note.push(&existing);
            return Ok(StartResult {
                run_dir: existing,
                existing: true,
                note: Some(note),
            });
        }
    }
    new_run(&deck_path, &id, &config, &parsed, &raw, &docs)
}

/// `""` as `.`, as `pathlib` reads an empty path.
fn or_dot(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The deck's errors for a run here (`check_deck`), as one error: `the deck has errors: …`,
/// with the one command that fixes them all, when there is one, as `check` gives it.
fn refuse_errors(deck: &Deck, id: &RepoIdentity, config: &Config) -> Result<()> {
    let problems = check_deck(deck, &id.top, config)?;
    let errors: Vec<_> = problems
        .iter()
        .filter(|p| p.level == Level::Error)
        .collect();
    let Some(first) = errors.first() else {
        return Ok(());
    };
    let shown: Vec<String> = errors.iter().map(|p| p.message.shown()).collect();
    let e = Error::msg(format!("the deck has errors: {}", shown.join("; ")));
    match &first.fix {
        Some(fix) if errors.iter().all(|p| p.fix.as_ref() == Some(fix)) => {
            Err(e.with_fix(human::command_line(fix).to_string_lossy().into_owned()))
        }
        _ => Err(e),
    }
}

// --------------------------------------------------------------------------
// The runs already under docs_dir

/// The fields of a run's `run.json` that `start` compares, read leniently: a field that is
/// not a string reads as none (`meta.get(...)` compared with strings).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Listed {
    repo: VendorStr,
    deck_source: VendorStr,
    deck_digest: VendorStr,
    branch: VendorStr,
    created_at: VendorStr,
}

/// Every run directory under `<docs>/runs` with a readable `run.json` holding a JSON object
/// (`engine._runs`).
fn list_runs(docs: &Path) -> Vec<(PathBuf, Listed)> {
    let runs = docs.join("runs");
    let Ok(entries) = std::fs::read_dir(&runs) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let dir = runs.join(entry.file_name());
            let bytes = std::fs::read(run_paths(&dir).run_json).ok()?;
            let text = String::from_utf8(bytes).ok()?;
            if !text.trim_start().starts_with('{') {
                return None;
            }
            let listed = serde_json::from_str(&text).ok()?;
            Some((dir, listed))
        })
        .collect()
}

/// The newest run of this deck in this repository that has no outcome, or whose latest
/// outcome is blocked (`engine._find_unfinished`, R25). A run of another repository (a second
/// clone sharing the docs_dir) is never returned; one whose event log is unreadable is
/// skipped, with a warning.
fn find_unfinished(
    docs: &Path,
    source: &PyStr,
    digest: &str,
    top: &Path,
) -> Result<Option<PathBuf>> {
    let real_top = real_path(top);
    let mut found: Vec<((PyStr, PyStr), PathBuf)> = Vec::new();
    for (dir, run) in list_runs(docs) {
        if run.deck_source.0.as_ref() != Some(source) || !run.deck_digest.is(digest) {
            continue;
        }
        let repo = run.repo.0.as_ref().and_then(|r| r.to_os().ok());
        if repo.is_none_or(|r| real_path(Path::new(&r)) != real_top) {
            continue;
        }
        let events = match read_events(&run_paths(&dir)) {
            Ok(events) => events,
            Err(e) => {
                let mut err = std::io::stderr().lock();
                writeln!(
                    err,
                    "warning: skipping the earlier run {}: its event log is unreadable ({e})",
                    dir.display()
                )?;
                continue;
            }
        };
        let mut ended = None;
        for (e, _) in &events {
            match e {
                Event::Outcome(o) => ended = Some(o.outcome.clone()),
                Event::Resumed(_) => ended = None,
                _ => {}
            }
        }
        if ended.is_none_or(|o| o.as_deref() == Some("blocked")) {
            let created = run.created_at.0.clone().unwrap_or_default();
            found.push(((created, PyStr::from_os(dir.as_os_str())), dir));
        }
    }
    Ok(found.into_iter().max().map(|(_, dir)| dir))
}

/// The deck's branch, or its first free numeric suffix (`-2`, `-3`, …) when another run of
/// this repository owns it or the branch already exists (`engine._free_branch`, R26'). A name
/// is also taken when a branch under its card prefix (`<name>--card-*`) exists, so the run
/// never takes up a card branch it did not cut (R54).
fn free_branch(docs: &Path, git: &Git, top: &Path, requested: &str) -> Result<String> {
    let repo = PyStr::from_os(top.as_os_str());
    let mut taken: HashSet<PyStr> = list_runs(docs)
        .into_iter()
        .filter(|(_, run)| run.repo.0.as_ref() == Some(&repo))
        .filter_map(|(_, run)| run.branch.0)
        .collect();
    let listing = git.run(&[
        OsStr::new("for-each-ref"),
        OsStr::new("--format=%(refname)"),
        OsStr::new("refs/heads/"),
    ])?;
    for line in py_splitlines_bytes(&listing) {
        let name = line.strip_prefix(b"refs/heads/").unwrap_or(line);
        taken.insert(PyStr::from_os(OsStr::from_bytes(name)));
        for at in 0..name.len() {
            if name[at..].starts_with(CARD_INFIX) {
                taken.insert(PyStr::from_os(OsStr::from_bytes(&name[..at])));
            }
        }
    }
    let (mut branch, mut n) = (requested.to_string(), 1);
    while taken.contains(&PyStr::from(branch.as_str())) {
        n += 1;
        branch = format!("{requested}-{n}");
    }
    Ok(branch)
}

/// The run's slug from its title (`engine._slug`): lowercase, each run of characters other
/// than `a-z` and `0-9` a `-`, trimmed of `-` and cut to `SLUG_MAX`; `run` when nothing is left.
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in title.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            sep = false;
        } else if !sep {
            out.push('-');
            sep = true;
        }
    }
    let cut: String = out.trim_matches('-').chars().take(SLUG_MAX).collect();
    let slug = cut.trim_matches('-');
    if slug.is_empty() {
        "run".into()
    } else {
        slug.into()
    }
}

// --------------------------------------------------------------------------
// A new run

/// `untracked-at-start.json`.
#[derive(Serialize)]
struct UntrackedAtStart {
    paths: Vec<PyStr>,
}

fn new_run(
    deck_path: &Path,
    id: &RepoIdentity,
    config: &Config,
    deck: &Deck,
    raw: &[u8],
    docs: &Path,
) -> Result<StartResult> {
    let git = Git::at(&id.top)?;
    let branch = free_branch(docs, &git, &id.top, &deck.branch)?;
    let slug = slug(&deck.title);
    let root = new_run_dir(docs, &slug, &today()?)?;
    let paths = run_paths(&root);
    write_file_atomic(&paths.deck, raw)?;
    // The effective configuration, read by the run from now on instead of the live files (I5).
    write_json_atomic(&root.join(SNAPSHOT), &ConfigSnapshot(config))?;
    let (base_ref, base_sha) = if deck.base_sha.is_empty() {
        let (name, sha) = default_base(&git)?;
        (PyStr::from_os(&name), sha)
    } else {
        let sha = head(&git, OsStr::new(&deck.base_sha))?;
        (PyStr::from(deck.base_ref.as_str()), sha)
    };
    // The worktree is named after the run directory, which is unique, so no two runs share it.
    let name = root
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| Error::msg(format!("not a run directory name: {}", root.display())))?;
    let ws = prepare_worktree(id, &branch, &base_sha, name, &root)?;
    // What the workspace holds untracked at the start: never swept, never committed (R48').
    let untracked = UntrackedAtStart {
        paths: untracked_files(&ws)?,
    };
    write_json_atomic(&root.join(UNTRACKED_AT_START), &untracked)?;
    let run_id = new_run_id()?;
    let meta = RunMeta {
        run_id: Some(run_id.clone()),
        repo: Some(PyStr::from_os(id.top.as_os_str())),
        deck_source: Some(PyStr::from_os(deck_path.as_os_str())),
        deck_digest: Some(deck.digest.clone()),
        base_ref: Some(Some(base_ref)),
        base_sha: Some(base_sha.clone()),
        branch: Some(Some(PyStr::from(branch.as_str()))),
        branch_requested: Some(PyStr::from(deck.branch.as_str())),
        workspace: Some(Some(PyStr::from_os(ws.as_os_str()))),
        slug: Some(slug),
        created_at: Some(utc_now()),
        supervisor_sessions: Some(Vec::new()),
    };
    write_json_atomic(&paths.run_json, &meta)?;
    let started = RunStarted {
        run_id: Some(run_id),
        base_sha: Some(base_sha),
        branch: Some(PyStr::from(branch.as_str())),
        ..RunStarted::default()
    };
    append_event(&paths, Event::RunStarted(started))?;
    let note = (branch != deck.branch).then(|| {
        OsString::from(format!(
            "run branch: {branch} ({} belongs to another run)",
            deck.branch
        ))
    });
    Ok(StartResult {
        run_dir: root,
        existing: false,
        note,
    })
}

/// Today's local date as `YYYY-MM-DD` (Python's `date.today().isoformat()`): the real clock,
/// as the Python's.
fn today() -> Result<String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = i64::try_from(secs)
        .ok()
        .and_then(crate::sys::local_date)
        .ok_or_else(|| Error::msg("cannot read today's local date"))?;
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

/// A fresh run ID: 12 lowercase hex digits from 6 random bytes (`secrets.token_hex(6)`).
fn new_run_id() -> Result<String> {
    use std::io::Read;
    let path = Path::new("/dev/urandom");
    let mut bytes = [0u8; 6];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| Error::io(path, e))?;
    Ok(hex(&bytes))
}

// --------------------------------------------------------------------------
// The command

/// `start --json`: the same information as the text output.
#[derive(Serialize)]
struct StartReport {
    run: PyStr,
    existing: bool,
    note: Option<PyStr>,
}

/// `start` (`engine._cmd_start`): the note, if any, then `run: <dir>` for a new run; with
/// `json`, one JSON object instead.
pub fn command(deck: &OsStr, repo: &OsStr, new: bool, json: bool) -> Result<ExitCode> {
    let started = start(Path::new(deck), Path::new(repo), new)?;
    let mut out = std::io::stdout().lock();
    if json {
        let report = StartReport {
            run: PyStr::from_os(started.run_dir.as_os_str()),
            existing: started.existing,
            note: started.note.as_deref().map(PyStr::from_os),
        };
        writeln!(out, "{}", wire::to_output(&report)?)?;
    } else {
        let mut text = Vec::new();
        if let Some(note) = &started.note {
            text.extend_from_slice(note.as_bytes());
            text.push(b'\n');
        }
        if !started.existing {
            text.extend_from_slice(b"run: ");
            text.extend_from_slice(started.run_dir.as_os_str().as_bytes());
            text.push(b'\n');
        }
        out.write_all(&text)?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_the_python_slug() {
        let titles = [
            "Demo deck",
            "  --Retry uploads!! (v2) -- ",
            "",
            "!!!",
            "Caf\u{e9} \u{212a}elvin \u{130}stanbul",
            "a very long title that goes well past the forty character limit",
            "exactly forty characters long title ab-c-",
            "\u{3a3}\u{3a3} ok",
        ];
        let inputs: Vec<serde_json::Value> = titles.iter().map(|t| serde_json::json!(t)).collect();
        let theirs = crate::pytext::tests::python_eval(
            "from leadhand.engine import _slug",
            "_slug(x)",
            &inputs,
        );
        for (title, want) in titles.iter().zip(theirs) {
            assert_eq!(slug(title), want.as_str().unwrap(), "{title:?}");
        }
    }

    #[test]
    fn run_ids_are_token_hex_6() {
        let (a, b) = (new_run_id().unwrap(), new_run_id().unwrap());
        assert_ne!(a, b);
        for id in [&a, &b] {
            assert_eq!(id.len(), 12);
            assert!(id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        }
    }
}
