//! The runner's commit of a Codex worker's work (ruling R39; `dispatch._commit_work`,
//! `untracked_to_take`, `_commit_subject`, `_scratch_text`).
//!
//! Codex's workspace-write sandbox keeps the Git directory read-only, so Codex workers never
//! commit: the runner commits their work for the commit kinds once Codex is confirmed stopped,
//! outside every sandbox. So it never commits outside the run's own workspace, off the expected
//! branch or during a Git operation; never a file the workspace had when Codex launched, a
//! nested repository, or anything under the run's folders; and it is two-phase: `commit.json`
//! records the intent and HEAD before, and a restart that finds HEAD moved to a commit carrying
//! this dispatch's `Leadhand-Dispatch:` trailer does not commit again. Every Git call goes
//! through the Git runner; the commit is unsigned and runs no hook.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::value::RawValue;

use super::caller::{Anchor, check_request, py_parent, relpath};
use super::finalize::{CommitOutcome, Commits};
use super::own::COMMIT_INTENT;
use super::{DispatchRequest, UNTRACKED, field, read_json_object};
use crate::briefs::{SCRATCH, scratch_dir};
use crate::error::{Error, Result};
use crate::git::{Git, stage_work, status_entries, top_and_prefix, unhide_index_entries};
use crate::pytext::{pure_path, py_isspace, py_splitlines, py_strip, real_path};
use crate::safedir::SafeDir;
use crate::state::{run_paths, utc_now};
use crate::store::write_json_atomic;
use crate::wire::{PyMap, PyStr};

/// The trailer naming the dispatch whose work a commit holds.
pub const TRAILER: &str = "Leadhand-Dispatch";
/// How long one of the commit's Git calls may take (`COMMIT_TIMEOUT`).
pub const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);
/// In the run directory: the workspace's untracked files when the run started (the
/// operator's, in an in-place run). Never swept or committed (ruling R48').
pub const UNTRACKED_AT_START: &str = "untracked-at-start.json";
/// The Git operations no commit is made during.
const GIT_OPERATIONS: [&str; 6] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// The runner's commits (R39), for `run_runner_with`, a cancel and a dead runner's finalize.
pub struct RunnerCommits;

impl Commits for RunnerCommits {
    fn commit_work(
        &self,
        wd: &Path,
        req: &DispatchRequest,
        deadline: Option<Instant>,
    ) -> CommitOutcome {
        commit_work(wd, req, deadline)
    }
}

/// `commit.json`: the intent, in the Python's key order.
#[derive(Serialize)]
struct Intent<'a> {
    at: String,
    branch: &'a Option<PyStr>,
    head_before: &'a str,
}

/// How the commit runs its own Git calls (`dispatch._git`): the command's output whatever its
/// exit, `None` when it missed its deadline. Tests put a failing Git in its place.
type RunGit<'a> = &'a dyn Fn(&Git, &[&OsStr]) -> Result<Option<Output>>;

/// `_commit_work` for a commit kind with a branch (the first rule is `finalize`'s): commit what
/// the Codex worker left in the workspace, on `req.branch`. `deadline` bounds every Git call,
/// for a cancel.
pub fn commit_work(wd: &Path, req: &DispatchRequest, deadline: Option<Instant>) -> CommitOutcome {
    commit_work_with(wd, req, deadline, &|git, args| git.output_or_timeout(args))
}

fn commit_work_with(
    wd: &Path,
    req: &DispatchRequest,
    deadline: Option<Instant>,
    git: RunGit<'_>,
) -> CommitOutcome {
    // Never commit outside the run's own workspace, whatever request.json says (M1). Changes
    // unknown: reported as uncommitted work, so the result cannot pass for finished.
    if let Err(e) = check_in_run(req) {
        return CommitOutcome {
            changes: true,
            commit_error: Some(PyStr::from(e.message)),
            ..CommitOutcome::default()
        };
    }
    let mut changes = false;
    let calls = Calls {
        ws: &req.cwd,
        deadline,
        git,
    };
    match commit_steps(wd, req, &calls, &mut changes) {
        Ok(outcome) => outcome,
        Err(e) => CommitOutcome {
            changes,
            commit_error: Some(PyStr::from(e.message)),
            ..CommitOutcome::default()
        },
    }
}

/// `check_request` against the run the request names, as the Python's `_commit_work` reads it:
/// a run with no `run.json` (or an empty one) names a workspace nothing is.
fn check_in_run(req: &DispatchRequest) -> Result<()> {
    let real = real_path(&pure_path(req.run_dir.as_os_str()));
    let meta = read_json_object(&run_paths(&req.run_dir).run_json)?;
    let anchor = match meta {
        Some(meta) if !meta.is_empty() => Anchor::read(&real)?,
        _ => Anchor {
            real,
            run_id: None,
            workspace: Some(PyStr::from("/nonexistent run.json")),
            repo: None,
        },
    };
    check_request(req, &anchor)
}

/// The commit's own Git calls, bounded by the cancel's deadline.
struct Calls<'a> {
    ws: &'a Path,
    deadline: Option<Instant>,
    git: RunGit<'a>,
}

impl Calls<'_> {
    /// `bound()`: the deadline of the next Git call.
    fn bound(&self) -> Result<Duration> {
        let Some(deadline) = self.deadline else {
            return Ok(COMMIT_TIMEOUT);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::msg("no time left before the cancel deadline"));
        }
        Ok(left.min(COMMIT_TIMEOUT))
    }

    /// Git in the workspace, with the next call's deadline.
    fn at(&self) -> Result<Git> {
        Ok(Git::at(self.ws)?.with_deadline(self.bound()?))
    }

    /// `run(*args, ok=...)`: a Git call; with `ok`, a non-zero exit is an error naming it.
    fn run(&self, args: &[&str], ok: bool) -> Result<Output> {
        let git = self.at()?;
        let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let Some(out) = (self.git)(&git, &argv)? else {
            let before = if self.deadline.is_some() {
                " before the cancel deadline"
            } else {
                ""
            };
            return Err(Error::msg(format!("git {} timed out{before}", args[0])));
        };
        if ok && !out.status.success() {
            let detail = if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            return Err(Error::msg(format!(
                "git {} failed: {}",
                args.join(" "),
                py_strip(&String::from_utf8_lossy(detail))
            )));
        }
        Ok(out)
    }
}

/// Git's output as the Python's `_git` reads it (undecodable bytes replaced), stripped.
fn stripped(out: &[u8]) -> String {
    py_strip(&String::from_utf8_lossy(out)).to_string()
}

fn skipped(changes: bool, why: impl Into<PyStr>) -> CommitOutcome {
    CommitOutcome {
        changes,
        commit_skipped: Some(why.into()),
        ..CommitOutcome::default()
    }
}

/// `untracked.json`'s paths: `None` when there is no record (a direct call, before any launch);
/// `Some(None)` for a record without paths (the listing failed at launch, so no untracked file
/// is taken).
fn untracked_record(wd: &Path) -> Result<Option<Option<Vec<PyStr>>>> {
    let Some(snap) = read_json_object(&wd.join(UNTRACKED))? else {
        return Ok(None);
    };
    Ok(Some(paths_field(&snap, &wd.join(UNTRACKED))?))
}

/// A record's `paths`: `None` when missing or `null`; else its list of strings.
fn paths_field(map: &PyMap<Box<RawValue>>, file: &Path) -> Result<Option<Vec<PyStr>>> {
    match map.get(&PyStr::from("paths")) {
        None => Ok(None),
        Some(raw) => serde_json::from_str::<Option<Vec<PyStr>>>(raw.get())
            .map_err(|e| Error::msg(format!("{}: paths: {e}", file.display()))),
    }
}

fn commit_steps(
    wd: &Path,
    req: &DispatchRequest,
    calls: &Calls<'_>,
    changes: &mut bool,
) -> Result<CommitOutcome> {
    let ws = calls.ws;
    // Tracked changes, and the untracked files that were not there when Codex launched.
    let snap = untracked_record(wd)?;
    let earlier: Vec<PyStr> = snap.clone().flatten().unwrap_or_default();
    // An edit the worker hid behind assume-unchanged or skip-worktree is its work too.
    unhide_index_entries(&calls.at()?)?;
    let (tracked, loose) = status_entries(&calls.at()?)?;
    let new: Vec<PyStr> = match &snap {
        None | Some(Some(_)) => untracked_to_take(&req.run_dir, ws, &loose, calls.bound()?)?
            .into_iter()
            .filter(|p| !earlier.contains(p))
            .collect(),
        Some(None) => Vec::new(),
    };
    *changes = tracked || !new.is_empty();
    let branch = req.branch.clone().unwrap_or_default();
    let r = calls.run(&["symbolic-ref", "--quiet", "--short", "HEAD"], false)?;
    let current = r.status.success().then(|| PyStr::from(stripped(&r.stdout)));
    if current.as_ref() != Some(&branch) {
        let mut why = PyStr::from("workspace is on ");
        match current.filter(|c| !c.is_empty()) {
            Some(c) => why.push(&c),
            None => why.push_str("a detached HEAD"),
        }
        why.push_str(", not ");
        why.push(&branch);
        return Ok(skipped(*changes, why));
    }
    for name in GIT_OPERATIONS {
        let out = calls.run(
            &["rev-parse", "--path-format=absolute", "--git-path", name],
            true,
        )?;
        let at = stripped(&out.stdout);
        if at.is_empty() {
            return Err(Error::msg(format!(
                "git rev-parse --git-path {name} gave no path"
            )));
        }
        if Path::new(&at).exists() {
            return Ok(skipped(
                *changes,
                format!("a Git operation is in progress ({name})"),
            ));
        }
    }
    let head_now = stripped(&calls.run(&["rev-parse", "--verify", "HEAD"], false)?.stdout);
    let intent = read_json_object(&wd.join(COMMIT_INTENT))?;
    if let Some(intent) = &intent {
        let before = field::<PyStr>(intent, "head_before");
        if !head_now.is_empty() && before != Some(PyStr::from(head_now.as_str())) {
            let body = calls.run(&["log", "-1", "--format=%B", "HEAD"], true)?;
            let body = String::from_utf8_lossy(&body.stdout).into_owned();
            let trailer = format!("{TRAILER}: {}", req.dispatch_id);
            if py_splitlines(&body).contains(&trailer.as_str()) {
                // Committed before a restart.
                return Ok(CommitOutcome {
                    committed: Some(head_now),
                    changes: *changes,
                    ..CommitOutcome::default()
                });
            }
        }
    }
    if !*changes {
        return Ok(skipped(false, "no changes"));
    }
    if intent.is_none() {
        let record = Intent {
            at: utc_now(),
            branch: &req.branch,
            head_before: &head_now,
        };
        write_json_atomic(&wd.join(COMMIT_INTENT), &record)?;
    }
    let (staged, gitlinks) = stage_work(ws, &new, calls.deadline)?;
    let gitlinks_skipped = (!gitlinks.is_empty()).then(|| gitlinks.clone());
    if !staged {
        let why = if gitlinks.is_empty() {
            PyStr::from("no changes")
        } else {
            let mut why =
                PyStr::from("only nested repositories changed, and gitlinks are never committed: ");
            for (i, g) in gitlinks.iter().enumerate() {
                if i > 0 {
                    why.push_str(", ");
                }
                why.push(g);
            }
            why
        };
        return Ok(CommitOutcome {
            changes: false,
            commit_skipped: Some(why),
            gitlinks_skipped,
            ..CommitOutcome::default()
        });
    }
    // Unsigned (signing may prompt) and without hooks: this is leadhand's own bookkeeping.
    let subject = commit_subject(req);
    let trailer = format!("{TRAILER}: {}", req.dispatch_id);
    calls.run(
        &[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "--no-verify",
            "-m",
            &subject,
            "-m",
            &trailer,
        ],
        true,
    )?;
    let committed = stripped(&calls.run(&["rev-parse", "--verify", "HEAD"], true)?.stdout);
    Ok(CommitOutcome {
        committed: Some(committed),
        changes: true,
        gitlinks_skipped,
        ..CommitOutcome::default()
    })
}

/// `untracked_to_take`: of the workspace's untracked `paths` (named from the checkout's top),
/// the ones leadhand may sweep or commit: never a nested repository, a file the workspace had
/// when the run started, anything under the run directory or its docs folder when they lie
/// inside the checkout (R48'), or anything outside the workspace when it is a subdirectory of
/// its checkout. An error when the checkout's top cannot be found (`timeout` bounds the call).
pub fn untracked_to_take(
    run_dir: &Path,
    ws: &Path,
    paths: &[PyStr],
    timeout: Duration,
) -> Result<Vec<PyStr>> {
    let at_start = read_json_object(&run_dir.join(UNTRACKED_AT_START))?.unwrap_or_default();
    let start = paths_field(&at_start, &run_dir.join(UNTRACKED_AT_START))?.unwrap_or_default();
    let (root, prefix) = top_and_prefix(&Git::at(ws)?.with_deadline(timeout))?;
    let top = real_path(&root);
    let mut fenced: Vec<PyStr> = Vec::new();
    // <docs_dir>/runs/<run>
    for d in [run_dir.to_path_buf(), py_parent(&py_parent(run_dir))] {
        let rel = relpath(&real_path(&d), &top);
        let rel = rel.as_os_str();
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(rel);
        if bytes != b".." && !bytes.starts_with(b"../") {
            let mut f = PyStr::from("");
            if bytes != b"." {
                f = PyStr::from_os(rel);
                f.push_str("/");
            }
            fenced.push(f);
        }
    }
    Ok(paths
        .iter()
        .filter(|p| {
            let b = p.as_wtf8();
            !b.ends_with(b"/")
                && b.starts_with(prefix.as_wtf8())
                && !start.contains(p)
                && !fenced.iter().any(|f| b.starts_with(f.as_wtf8()))
        })
        .cloned()
        .collect())
}

/// `_scratch_text`: the dispatch's scratch file `path` (its brief or report), read through the
/// scratch folder's checked descriptors: never through a symlink, and never anything but a
/// regular file (a FIFO would block the runner). Empty when missing or refused.
fn scratch_text(req: &DispatchRequest, path: &Path) -> String {
    let path = pure_path(path.as_os_str());
    // `<ws>/.leadhand/<card or run>/<dispatch id>/<name>`
    let parts: Vec<Component> = path.components().collect();
    let Some(Component::Normal(name)) = parts.last().copied() else {
        return String::new();
    };
    let named = parts
        .iter()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if named < 4 {
        return String::new();
    }
    let ws: PathBuf = parts[..parts.len() - 4].iter().collect();
    let Ok(folder) = scratch_dir(&ws, req.card, &req.dispatch_id) else {
        return String::new();
    };
    if folder.join(name) != path {
        return String::new();
    }
    let card = req
        .card
        .map_or_else(|| "run".to_string(), |n| n.to_string());
    let rel = [
        OsStr::new(SCRATCH),
        OsStr::new(&card),
        OsStr::new(&req.dispatch_id),
    ];
    match SafeDir::open_under(&ws, &rel) {
        Ok(Some(dir)) => match dir.read_file(name, u64::MAX) {
            Ok(Some(data)) => String::from_utf8_lossy(&data).into_owned(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// The Unicode decimal digits' zeros (category Nd, each a run of ten), for Python's `\d`.
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66,
    0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
    0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650,
    0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60, 0x16AC0,
    0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];

fn is_decimal(c: char) -> bool {
    let c = u32::from(c);
    DECIMAL_ZEROS.iter().any(|z| (*z..*z + 10).contains(&c))
}

/// `re.search(r"^## Card \d+: (.+)$", brief, re.MULTILINE)`: the first such line's title.
fn card_title(brief: &str) -> Option<&str> {
    brief.split('\n').find_map(|line| {
        let rest = line.strip_prefix("## Card ")?;
        let digits = rest.len() - rest.trim_start_matches(is_decimal).len();
        if digits == 0 {
            return None;
        }
        rest[digits..].strip_prefix(": ").filter(|t| !t.is_empty())
    })
}

/// `_commit_subject`: the first line of the report's `## Summary`, else the card title from the
/// brief, after `<kind> card <n>: `; at most 100 characters.
fn commit_subject(req: &DispatchRequest) -> String {
    let report = scratch_text(req, &req.report);
    let lines = py_splitlines(&report);
    let mut subject = String::new();
    for (i, line) in lines.iter().enumerate() {
        if py_strip(line).to_lowercase() == "## summary" {
            subject = lines[i + 1..]
                .iter()
                .map(|l| py_strip(l))
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .to_string();
            if subject.starts_with('#') {
                subject.clear();
            }
            break;
        }
    }
    if subject.is_empty() {
        let brief = scratch_text(req, &req.brief);
        subject = match card_title(&brief) {
            Some(title) => py_strip(title).to_string(),
            None => format!("leadhand dispatch {}", req.dispatch_id),
        };
    }
    let kind = req.kind.as_deref().unwrap_or("None");
    let prefix = match req.card {
        Some(card) => format!("{kind} card {card}"),
        None => kind.to_string(),
    };
    let text = format!("{prefix}: {subject}");
    if text.chars().count() <= 100 {
        return text;
    }
    let head: String = text.chars().take(99).collect();
    format!("{}…", head.trim_end_matches(py_isspace))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;
    use crate::git::fixture::Fixture;

    const CARD_BRANCH: &str = "leadhand/demo--card-1";

    /// A run (outside the repository) whose workspace is a repository on the card branch, with
    /// `.leadhand/` excluded, and an implement dispatch `d1`'s worker folder.
    struct World {
        f: Fixture,
        repo: PathBuf,
        run_dir: PathBuf,
        base: String,
    }

    impl World {
        fn new() -> World {
            let f = Fixture::new();
            let repo = f.make_repo("repo");
            f.git(&repo, &["checkout", "--quiet", "-b", CARD_BRANCH]);
            let base = f.git(&repo, &["rev-parse", "HEAD"]);
            fs::write(repo.join(".git/info/exclude"), ".leadhand/\n").unwrap();
            let run_dir = f.root().join("runs").join("r1");
            fs::create_dir_all(run_dir.join("workers").join("d1")).unwrap();
            let meta = format!(
                "{{\"run_id\": \"r1\", \"repo\": {repo:?}, \"workspace\": {repo:?}}}",
                repo = repo.to_str().unwrap()
            );
            fs::write(run_dir.join("run.json"), meta).unwrap();
            World {
                f,
                repo,
                run_dir,
                base,
            }
        }

        fn wd(&self) -> PathBuf {
            self.run_dir.join("workers").join("d1")
        }

        fn request(&self) -> DispatchRequest {
            let folder = self.repo.join(".leadhand").join("1").join("d1");
            fs::create_dir_all(&folder).unwrap();
            fs::write(
                folder.join("brief.md"),
                "# Brief\n## Card 1: Widget card\n- Card branch: leadhand/demo--card-1\n",
            )
            .unwrap();
            DispatchRequest {
                model: "gpt-6-luna".into(),
                effort: "low".into(),
                role: "implementer".into(),
                brief: folder.join("brief.md"),
                report: folder.join("report.md"),
                cwd: self.repo.clone(),
                run_dir: self.run_dir.clone(),
                card: Some(1),
                attempt: 1,
                dispatch_id: "d1".into(),
                steps: 50,
                minutes: 5,
                network: false,
                writable_dirs: vec![],
                resume: None,
                kind: Some("implement".into()),
                branch: Some(CARD_BRANCH.into()),
            }
        }

        fn head(&self) -> String {
            self.f.git(&self.repo, &["rev-parse", "HEAD"])
        }
    }

    // CATALOG: test_dispatch.py::RunnerCommitTests::test_untracked_to_take_fails_closed_when_the_checkout_top_is_unknown
    #[test]
    fn untracked_to_take_fails_closed_when_the_checkout_top_is_unknown() {
        let w = World::new();
        let outside = w.f.root().join("not a checkout");
        fs::create_dir(&outside).unwrap();
        let got = untracked_to_take(&w.run_dir, &outside, &["new.py".into()], COMMIT_TIMEOUT);
        assert!(got.is_err(), "{got:?}");
    }

    // CATALOG: test_dispatch.py::RunnerCommitTests::test_unlocatable_git_path_is_an_error_not_an_operation
    #[test]
    fn unlocatable_git_path_is_an_error_not_an_operation() {
        let w = World::new();
        let req = w.request();
        w.f.write(&w.repo, &[("src/widget.py", "WIDGET = 1\n")]);
        let failing = |git: &Git, args: &[&OsStr]| -> Result<Option<Output>> {
            if args.contains(&OsStr::new("--git-path")) {
                return Ok(Some(Output {
                    status: ExitStatus::from_raw(128 << 8),
                    stdout: Vec::new(),
                    stderr: b"fatal: broken repository\n".to_vec(),
                }));
            }
            git.output_or_timeout(args)
        };
        let out = commit_work_with(&w.wd(), &req, None, &failing);
        assert_eq!(out.committed, None);
        assert!(
            !out.commit_skipped
                .as_ref()
                .is_some_and(|s| s.contains("in progress"))
        );
        let error = out.commit_error.expect("an error");
        assert!(error.contains("broken repository"), "{error:?}");
        assert_eq!(w.head(), w.base);
    }

    #[test]
    fn commits_the_work_with_the_trailer_once() {
        let w = World::new();
        let req = w.request();
        w.f.write(&w.repo, &[("src/widget.py", "WIDGET = 1\n")]);
        let first = commit_work(&w.wd(), &req, None);
        let sha = w.head();
        assert_eq!(first.committed.as_deref(), Some(sha.as_str()), "{first:?}");
        assert!(w.wd().join(COMMIT_INTENT).exists());
        assert_eq!(
            w.f.git(&w.repo, &["log", "-1", "--format=%s%n%b"]),
            "implement card 1: Widget card\nLeadhand-Dispatch: d1"
        );
        // A restart after the commit (intent there, HEAD carries the trailer): nothing new.
        let again = commit_work(&w.wd(), &req, None);
        assert_eq!(again.committed.as_deref(), Some(sha.as_str()));
        assert_eq!(w.head(), sha);
        // A restart after the intent but before the commit: it commits.
        w.f.git(&w.repo, &["reset", "--quiet", "--soft", &w.base]);
        let late = commit_work(&w.wd(), &req, None);
        assert_ne!(late.committed.as_deref(), Some(w.base.as_str()));
        let count = w.f.git(
            &w.repo,
            &["rev-list", "--count", &format!("{}..HEAD", w.base)],
        );
        assert_eq!(count, "1");
    }

    #[test]
    fn no_time_left_commits_nothing() {
        let w = World::new();
        let req = w.request();
        w.f.write(&w.repo, &[("src/widget.py", "WIDGET = 1\n")]);
        let past = Instant::now() - Duration::from_secs(1);
        let out = commit_work(&w.wd(), &req, Some(past));
        assert_eq!(out.committed, None);
        assert_eq!(
            out.commit_error,
            Some("no time left before the cancel deadline".into())
        );
        assert_eq!(w.head(), w.base);
    }

    #[test]
    fn a_request_outside_the_run_is_refused_as_changes() {
        let w = World::new();
        let mut req = w.request();
        let victim = w.f.make_repo("victim");
        req.cwd = victim;
        let out = commit_work(&w.wd(), &req, None);
        assert!(out.changes);
        assert!(out.commit_error.unwrap().starts_with("refused: --cwd "));
        // No run.json at all: no workspace matches.
        let req = w.request();
        fs::remove_file(w.run_dir.join("run.json")).unwrap();
        let error = commit_work(&w.wd(), &req, None).commit_error.unwrap();
        assert!(error.contains("(/nonexistent run.json)"), "{error:?}");
    }

    #[test]
    fn subject_from_the_summary_else_the_card_title_truncated() {
        let w = World::new();
        let req = w.request();
        assert_eq!(commit_subject(&req), "implement card 1: Widget card");
        fs::write(
            &req.report,
            "Status: READY\n## Summary\n\n  Added it.  \nMore.\n",
        )
        .unwrap();
        assert_eq!(commit_subject(&req), "implement card 1: Added it.");
        fs::write(&req.report, "## SUMMARY\n# heading\n").unwrap();
        assert_eq!(commit_subject(&req), "implement card 1: Widget card");
        let long = format!("## Summary\n{} tail\n", "x".repeat(95));
        fs::write(&req.report, long).unwrap();
        let subject = commit_subject(&req);
        assert_eq!(subject.chars().count(), 100);
        assert!(subject.ends_with('…'));
        // A report that is not a regular file is no report.
        fs::remove_file(&req.report).unwrap();
        std::os::unix::fs::symlink(w.repo.join("README.md"), &req.report).unwrap();
        assert_eq!(commit_subject(&req), "implement card 1: Widget card");
        assert_eq!(
            card_title("## Card \u{663}: Arabic three\n"),
            Some("Arabic three")
        );
        assert_eq!(card_title("## Card \u{bd}: half\n## Card x: no\n"), None);
        assert_eq!(
            card_title("x\n## Card 12: \n## Card 7: seven\r\n"),
            Some("seven\r")
        );
    }

    #[test]
    fn untracked_to_take_fences_the_run_folder_from_the_checkout_top() {
        let w = World::new();
        let sub = w.repo.join("src");
        fs::create_dir(&sub).unwrap();
        let run_dir = w
            .repo
            .join("notes")
            .join("runs")
            .join("2026-11-10-demo run");
        fs::create_dir_all(&run_dir).unwrap();
        let paths: Vec<PyStr> = [
            "notes/runs/2026-11-10-demo run/run.json",
            "notes/runs.md",
            "src/new.py",
        ]
        .map(PyStr::from)
        .to_vec();
        assert_eq!(
            untracked_to_take(&run_dir, &sub, &paths, COMMIT_TIMEOUT).unwrap(),
            vec![PyStr::from("src/new.py")]
        );
        let paths: Vec<PyStr> = [
            "other/user notes.txt",
            "srcx/near.py",
            "src/new.py",
            "src/pkg/deep.py",
        ]
        .map(PyStr::from)
        .to_vec();
        assert_eq!(
            untracked_to_take(&w.run_dir, &sub, &paths, COMMIT_TIMEOUT).unwrap(),
            vec![PyStr::from("src/new.py"), PyStr::from("src/pkg/deep.py")]
        );
        assert_eq!(
            untracked_to_take(&w.run_dir, &w.repo, &paths, COMMIT_TIMEOUT).unwrap(),
            paths
        );
    }
}
