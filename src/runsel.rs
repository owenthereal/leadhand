//! Which run a command a person types means (spec, "Commands you type"; deliberate change 3).
//!
//! The run may be named by its directory, its `deck.md`, its run ID, its directory's name
//! (`2026-11-10-retry-uploads`) or its slug (`retry-uploads`). With nothing named, it is, in
//! this order:
//!
//! 1. the run whose worktree (its workspace or its evidence base worktree) contains the
//!    current directory: the `leadhand-run-dir` record in that worktree's admin folder, when
//!    the run's `run.json` names the worktree, else a scan of the runs under the main working
//!    tree's `docs_dir`;
//! 2. otherwise, the only unfinished run (no outcome, or blocked) of the repository containing
//!    the current directory;
//! 3. otherwise, an error listing the candidates, each with its exact command.
//!
//! The record is never `leadhand-run`, the evidence worktree's ownership marker, which lookup
//! never reads.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::cli::human;
use crate::config;
use crate::error::{Error, Result};
use crate::git::{self, LinkedWorktree};
use crate::pytext::{pure_path, real_path};
use crate::state::{Event, RunMeta, read_events, read_run_meta, run_paths};
use crate::testing;

/// The run record in a run worktree's admin folder: the run directory's path.
pub const RUN_RECORD: &str = "leadhand-run-dir";

/// How a run was named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunArg {
    /// A run directory, or a path that names none (refused later as not a run directory).
    Dir(PathBuf),
    /// A run's `deck.md`.
    Deck(PathBuf),
    /// A run ID: 12 hex digits.
    Id(String),
    /// A run directory's name, or a slug.
    Name(OsString),
}

impl RunArg {
    /// What `arg` names, with relative paths taken from `cwd`: an existing directory, then a
    /// `deck.md` file, then (for a word with no `/`) a run ID or a name.
    pub fn parse(arg: &OsStr, cwd: &Path) -> RunArg {
        let path = pure_path(arg);
        let at = cwd.join(&path);
        if at.is_dir() {
            return RunArg::Dir(path);
        }
        if at.is_file() && path.file_name() == Some(OsStr::new("deck.md")) {
            return RunArg::Deck(path);
        }
        if at.exists() || arg.as_bytes().contains(&b'/') {
            return RunArg::Dir(path);
        }
        match arg.to_str() {
            Some(id) if is_run_id(id) => RunArg::Id(id.to_string()),
            _ => RunArg::Name(arg.to_os_string()),
        }
    }
}

/// A run ID: 12 lowercase hex digits (`secrets.token_hex(6)`).
fn is_run_id(text: &str) -> bool {
    text.len() == 12 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The run directory `command` (`status`, `report`, …) is about, from what it was given and
/// the current directory `cwd`. Path forms are returned as given (relative to `cwd`); runs
/// found otherwise as the absolute paths under `docs_dir`.
///
/// The sketch's `select_run(arg, cwd)` takes `command` too, so the candidates it lists are
/// each that command's own line.
pub fn select_run(command: &str, arg: Option<&OsStr>, cwd: &Path) -> Result<PathBuf> {
    select_run_with(command, &[], arg, cwd)
}

/// `select_run` for a command whose candidates' lines need the rest of what was typed:
/// each candidate's command is `<cli> <command> <run dir> <after…>` (`unpark`'s `--card` and
/// `--note`), so it works as printed.
pub fn select_run_with(
    command: &str,
    after: &[OsString],
    arg: Option<&OsStr>,
    cwd: &Path,
) -> Result<PathBuf> {
    let command = Command {
        name: command,
        after,
    };
    let Some(arg) = arg else {
        return from_cwd(&command, cwd);
    };
    match RunArg::parse(arg, cwd) {
        RunArg::Dir(dir) => Ok(dir),
        RunArg::Deck(deck) => Ok(match deck.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        }),
        RunArg::Id(id) => {
            let runs = runs_dir_for(cwd, arg)?;
            let found: Vec<Run> = list_runs(&runs)
                .into_iter()
                .filter(|r| r.meta.as_ref().and_then(|m| m.run_id.as_deref()) == Some(&id))
                .collect();
            one_named(&command, arg, &runs, found)
        }
        RunArg::Name(name) => {
            let runs = runs_dir_for(cwd, arg)?;
            let all = list_runs(&runs);
            let by_dir: Vec<Run> = all
                .iter()
                .filter(|r| r.dir.file_name() == Some(name.as_os_str()))
                .cloned()
                .collect();
            let found = if by_dir.is_empty() {
                all.into_iter()
                    .filter(|r| {
                        r.meta
                            .as_ref()
                            .and_then(|m| m.slug.as_deref())
                            .map(OsStr::new)
                            == Some(name.as_os_str())
                    })
                    .collect()
            } else {
                by_dir
            };
            one_named(&command, arg, &runs, found)
        }
    }
}

/// The command a candidate's line runs: its name, then the run, then what follows it.
struct Command<'a> {
    name: &'a str,
    after: &'a [OsString],
}

#[cfg(test)]
impl Command<'_> {
    /// The command `name`, with nothing after the run.
    fn named(name: &str) -> Command<'_> {
        Command { name, after: &[] }
    }
}

/// The one run that matched a name: the only one, else the only unfinished one.
fn one_named(command: &Command, arg: &OsStr, runs: &Path, found: Vec<Run>) -> Result<PathBuf> {
    if found.is_empty() {
        return Err(Error::msg(format!(
            "no run named {} under {}",
            arg.to_string_lossy(),
            runs.display()
        )));
    }
    if found.len() == 1 {
        return Ok(found.into_iter().next().expect("one run").dir);
    }
    let unfinished: Vec<&Run> = found.iter().filter(|r| unfinished(&r.dir)).collect();
    if let [one] = unfinished[..] {
        return Ok(one.dir.clone());
    }
    Err(candidates(
        &format!(
            "{} runs are named {}; name the one you mean:",
            found.len(),
            arg.to_string_lossy()
        ),
        command,
        found.iter().map(|r| r.dir.as_path()),
    ))
}

/// `<docs_dir>/runs` of the repository containing `cwd`, for a run named by ID or name.
fn runs_dir_for(cwd: &Path, arg: &OsStr) -> Result<PathBuf> {
    let top = git::repo_identity(cwd)
        .map_err(|_| {
            Error::msg(format!(
                "no run named {}: {} is not in a Git repository (name the run directory)",
                arg.to_string_lossy(),
                cwd.display()
            ))
        })?
        .top;
    runs_dir(&top)
}

/// `<docs_dir>/runs` for the repository whose main working tree is `top`.
fn runs_dir(top: &Path) -> Result<PathBuf> {
    let config = config::load_config(top)?;
    Ok(config::docs_dir_for(&config, top).join("runs"))
}

fn from_cwd(command: &Command, cwd: &Path) -> Result<PathBuf> {
    let not_in_repo = || {
        Error::msg(format!(
            "no run found from {}: it is not in a Git repository (name the run directory)",
            cwd.display()
        ))
    };
    let linked = git::linked_worktree(cwd).map_err(|_| not_in_repo())?;
    let top = match linked {
        Some(wt) => {
            if let Some(run) = recorded_run(&wt) {
                return Ok(run);
            }
            let runs = runs_dir(&wt.repo.top)?;
            if let Some(run) = worktree_run(command, &wt, &list_runs(&runs))? {
                return Ok(run);
            }
            wt.repo.top
        }
        None => git::repo_identity(cwd).map_err(|_| not_in_repo())?.top,
    };
    let runs = runs_dir(&top)?;
    repo_run(command, &top, &runs, list_runs(&runs))
}

/// The run the worktree's record names, when that run's `run.json` names the worktree.
fn recorded_run(wt: &LinkedWorktree) -> Option<PathBuf> {
    let bytes = std::fs::read(wt.git_dir.join(RUN_RECORD)).ok()?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let dir = PathBuf::from(OsStr::from_bytes(bytes));
    let meta = read_run_meta(&run_paths(&dir)).ok()?;
    names_worktree(&dir, &meta, &wt.top).then_some(dir)
}

/// Whether the run at `dir` has `worktree` as its workspace or its evidence base worktree.
fn names_worktree(dir: &Path, meta: &RunMeta, worktree: &Path) -> bool {
    let worktree = real_path(worktree);
    let workspace = meta
        .workspace
        .clone()
        .flatten()
        .and_then(|w| w.to_os().ok())
        .map(PathBuf::from);
    if workspace
        .as_deref()
        .is_some_and(|w| real_path(w) == worktree)
    {
        return true;
    }
    base_worktree_path(dir, meta).is_some_and(|base| real_path(&base) == worktree)
}

/// The run's evidence base worktree (`dispatch.base_worktree_path`):
/// `<repo>/.worktrees/<workspace name>--base` when the workspace is a worktree there, else
/// `<repo>/.worktrees/<run dir name>--base`.
fn base_worktree_path(dir: &Path, meta: &RunMeta) -> Option<PathBuf> {
    let repo = PathBuf::from(meta.repo.as_ref()?.to_os().ok()?);
    let workspace = meta
        .workspace
        .clone()
        .flatten()
        .and_then(|w| w.to_os().ok())
        .map(PathBuf::from)
        .unwrap_or_default();
    let worktrees = repo.join(".worktrees");
    let name = if workspace.parent() == Some(worktrees.as_path()) {
        workspace.file_name()?.to_os_string()
    } else {
        dir.file_name()?.to_os_string()
    };
    let mut base = name;
    base.push("--base");
    Some(worktrees.join(base))
}

/// The run among `runs` whose worktree is `wt`, if any: the only one, else the only
/// unfinished one; several unfinished ones are listed.
fn worktree_run(command: &Command, wt: &LinkedWorktree, runs: &[Run]) -> Result<Option<PathBuf>> {
    let mine: Vec<&Run> = runs
        .iter()
        .filter(|r| {
            r.meta
                .as_ref()
                .is_some_and(|m| names_worktree(&r.dir, m, &wt.top))
        })
        .collect();
    let open: Vec<&Run> = mine
        .iter()
        .copied()
        .filter(|r| unfinished(&r.dir))
        .collect();
    match (&mine[..], &open[..]) {
        ([], _) => Ok(None),
        ([one], _) | (_, [one]) => Ok(Some(one.dir.clone())),
        _ => Err(candidates(
            &format!(
                "{} runs use the worktree {}; name the one you mean:",
                mine.len(),
                wt.top.display()
            ),
            command,
            mine.iter().map(|r| r.dir.as_path()),
        )),
    }
}

/// The only unfinished run of the repository whose main working tree is `top`.
fn repo_run(command: &Command, top: &Path, runs_dir: &Path, runs: Vec<Run>) -> Result<PathBuf> {
    let top_real = real_path(top);
    let open: Vec<Run> = runs
        .into_iter()
        .filter(|r| {
            r.meta
                .as_ref()
                .and_then(|m| m.repo.as_ref())
                .and_then(|repo| repo.to_os().ok())
                .is_some_and(|repo| real_path(Path::new(&repo)) == top_real)
        })
        .filter(|r| unfinished(&r.dir))
        .collect();
    match &open[..] {
        [one] => Ok(one.dir.clone()),
        [] => Err(Error::msg(format!(
            "no unfinished run of {} under {}",
            top.display(),
            runs_dir.display()
        ))),
        _ => Err(candidates(
            &format!(
                "{} unfinished runs of {}; name the one you mean:",
                open.len(),
                top.display()
            ),
            command,
            open.iter().map(|r| r.dir.as_path()),
        )),
    }
}

/// An error listing the run directories `dirs`, each with its command, one per line after
/// the error line.
fn candidates<'a>(
    head: &str,
    command: &Command,
    dirs: impl IntoIterator<Item = &'a Path>,
) -> Error {
    let mut message = head.to_string();
    for dir in dirs {
        let mut argv = vec![
            testing::self_path().into_os_string(),
            OsString::from(command.name),
            dir.as_os_str().to_os_string(),
        ];
        argv.extend_from_slice(command.after);
        message.push('\n');
        message.push_str(&human::command_line(&argv).to_string_lossy());
    }
    Error::msg(message)
}

/// A run directory under `docs_dir/runs`, with its `run.json` when it reads.
#[derive(Clone, Debug)]
struct Run {
    dir: PathBuf,
    meta: Option<RunMeta>,
}

/// The runs under `runs`: every directory there with a `run.json` file, by name.
fn list_runs(runs: &Path) -> Vec<Run> {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return Vec::new();
    };
    let mut out: Vec<Run> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|dir| run_paths(dir).run_json.is_file())
        .map(|dir| {
            let meta = read_run_meta(&run_paths(&dir)).ok();
            Run { dir, meta }
        })
        .collect();
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// Whether the run at `root` is unfinished: no outcome since its last relaunch, or a blocked
/// one; a log that does not read counts as unfinished (`status._unfinished`).
fn unfinished(root: &Path) -> bool {
    let Ok(events) = read_events(&run_paths(root)) else {
        return true;
    };
    let mut last = None;
    for (e, _) in &events {
        match e {
            Event::Outcome(o) => last = Some(o.outcome.clone()),
            Event::Resumed(_) => last = None,
            _ => {}
        }
    }
    last.is_none_or(|o| o.as_deref() == Some("blocked"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::git::fixture::Fixture;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn run_args_parse_by_what_they_name() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        fs::create_dir_all(cwd.join("runs/2026-11-10-retry")).unwrap();
        fs::write(cwd.join("runs/2026-11-10-retry/deck.md"), "").unwrap();
        fs::write(cwd.join("notes.md"), "").unwrap();
        let parse = |s: &str| RunArg::parse(OsStr::new(s), cwd);
        assert_eq!(
            parse("runs/2026-11-10-retry/"),
            RunArg::Dir("runs/2026-11-10-retry".into())
        );
        assert_eq!(
            parse("./runs/2026-11-10-retry/deck.md"),
            RunArg::Deck("runs/2026-11-10-retry/deck.md".into())
        );
        assert_eq!(parse("notes.md"), RunArg::Dir("notes.md".into()));
        assert_eq!(parse("/no/such/run"), RunArg::Dir("/no/such/run".into()));
        assert_eq!(parse("0123456789ab"), RunArg::Id("0123456789ab".into()));
        assert_eq!(parse("0123456789AB"), RunArg::Name(os("0123456789AB")));
        assert_eq!(
            parse("2026-11-10-retry"),
            RunArg::Name(os("2026-11-10-retry"))
        );
        assert_eq!(parse("retry"), RunArg::Name(os("retry")));
        assert_eq!(
            select_run(
                "status",
                Some(OsStr::new("runs/2026-11-10-retry/deck.md")),
                cwd
            )
            .unwrap(),
            PathBuf::from("runs/2026-11-10-retry")
        );
    }

    /// A run directory `name` under `runs`, for the repository `repo` with workspace `ws`,
    /// and its events.
    fn make_run(runs: &Path, name: &str, repo: &Path, ws: &Path, events: &str) -> PathBuf {
        let dir = runs.join(name);
        fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "run_id": format!("{:0>12}", name.len()),
            "repo": repo.to_str().unwrap(),
            "workspace": ws.to_str().unwrap(),
            "slug": "retry",
        });
        fs::write(dir.join("run.json"), meta.to_string()).unwrap();
        fs::write(dir.join("events.jsonl"), events).unwrap();
        dir
    }

    const OPEN: &str = "{\"seq\":1,\"ts\":\"t\",\"kind\":\"run_started\"}\n";
    const DONE: &str = "{\"seq\":1,\"ts\":\"t\",\"kind\":\"run_started\"}\n\
                        {\"seq\":2,\"ts\":\"t\",\"kind\":\"outcome\",\"outcome\":\"complete\"}\n";
    const BLOCKED: &str = "{\"seq\":1,\"ts\":\"t\",\"kind\":\"outcome\",\"outcome\":\"blocked\"}\n";
    const RESUMED: &str = "{\"seq\":1,\"ts\":\"t\",\"kind\":\"outcome\",\"outcome\":\"failed\"}\n\
                           {\"seq\":2,\"ts\":\"t\",\"kind\":\"resumed\"}\n";

    #[test]
    fn unfinished_is_no_outcome_since_a_relaunch_or_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, ws) = (Path::new("/r"), Path::new("/r/.worktrees/x"));
        assert!(unfinished(&make_run(dir.path(), "a", repo, ws, OPEN)));
        assert!(!unfinished(&make_run(dir.path(), "b", repo, ws, DONE)));
        assert!(unfinished(&make_run(dir.path(), "c", repo, ws, BLOCKED)));
        assert!(unfinished(&make_run(dir.path(), "d", repo, ws, RESUMED)));
        assert!(unfinished(&make_run(
            dir.path(),
            "e",
            repo,
            ws,
            "{corrupt\n"
        )));
    }

    #[test]
    fn the_repository_run_is_its_only_unfinished_one() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let repo = dir.path().join("my repo");
        fs::create_dir(&repo).unwrap();
        let ws = repo.join(".worktrees/x");
        make_run(&runs, "2026-01-01-done", &repo, &ws, DONE);
        make_run(
            &runs,
            "2026-01-03-elsewhere",
            Path::new("/other repo"),
            &ws,
            OPEN,
        );
        assert!(
            repo_run(&Command::named("status"), &repo, &runs, list_runs(&runs))
                .unwrap_err()
                .message
                .starts_with("no unfinished run of ")
        );
        let live = make_run(&runs, "2026-01-02-live", &repo, &ws, OPEN);
        assert_eq!(
            repo_run(&Command::named("status"), &repo, &runs, list_runs(&runs)).unwrap(),
            live
        );
        let second = make_run(&runs, "2026-01-04-live", &repo, &ws, BLOCKED);
        let e = repo_run(&Command::named("report"), &repo, &runs, list_runs(&runs)).unwrap_err();
        let lines: Vec<&str> = e.message.lines().collect();
        assert!(lines[0].starts_with("2 unfinished runs of "), "{lines:?}");
        let cli = human::shell_quote(testing::self_path().as_os_str());
        for (line, run) in lines[1..].iter().zip([&live, &second]) {
            assert_eq!(
                *line,
                format!(
                    "{} report {}",
                    cli.to_string_lossy(),
                    human::shell_quote(run.as_os_str()).to_string_lossy()
                )
            );
        }
    }

    #[test]
    fn a_worktree_finds_its_run_by_record_or_by_scan() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(&repo, &[("src/x.txt", "x\n")], "src");
        let ws = repo.join(".worktrees/retry");
        let base = repo.join(".worktrees/retry--base");
        for (path, branch) in [(&ws, "run"), (&base, "base")] {
            f.git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        }
        let runs = f.root().join("docs/runs");
        let mine = make_run(&runs, "2026-01-02-retry", &repo, &ws, OPEN);
        make_run(
            &runs,
            "2026-01-03-other",
            &repo,
            &repo.join(".worktrees/other"),
            OPEN,
        );
        for dir in [&ws, &ws.join("src"), &base] {
            let wt = git::linked_worktree(dir).unwrap().unwrap();
            assert_eq!(recorded_run(&wt), None);
            assert_eq!(
                worktree_run(&Command::named("status"), &wt, &list_runs(&runs)).unwrap(),
                Some(mine.clone()),
                "{dir:?}"
            );
        }
        // The record is read when present, and believed only when that run names the worktree.
        let wt = git::linked_worktree(&ws).unwrap().unwrap();
        let elsewhere = f.root().join("moved docs/runs/2026-01-02-retry");
        fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        fs::rename(&mine, &elsewhere).unwrap();
        fs::write(
            wt.git_dir.join(RUN_RECORD),
            elsewhere.as_os_str().as_bytes(),
        )
        .unwrap();
        assert_eq!(recorded_run(&wt), Some(elsewhere.clone()));
        let stranger = make_run(
            &runs,
            "2026-01-05-stranger",
            &repo,
            &repo.join(".worktrees/z"),
            OPEN,
        );
        fs::write(wt.git_dir.join(RUN_RECORD), stranger.as_os_str().as_bytes()).unwrap();
        assert_eq!(recorded_run(&wt), None);
        // The ownership marker is not the record.
        fs::remove_file(wt.git_dir.join(RUN_RECORD)).unwrap();
        fs::write(wt.git_dir.join("leadhand-run"), "000000000016").unwrap();
        assert_eq!(recorded_run(&wt), None);
    }

    #[test]
    fn the_evidence_base_is_named_as_the_python_names_it() {
        let meta = |ws: &str| RunMeta {
            repo: Some("/r".into()),
            workspace: Some(Some(ws.into())),
            ..RunMeta::default()
        };
        let dir = Path::new("/docs/runs/2026-01-02-retry");
        assert_eq!(
            base_worktree_path(dir, &meta("/r/.worktrees/retry")),
            Some(PathBuf::from("/r/.worktrees/retry--base"))
        );
        assert_eq!(
            base_worktree_path(dir, &meta("/elsewhere/retry")),
            Some(PathBuf::from("/r/.worktrees/2026-01-02-retry--base"))
        );
    }
}
