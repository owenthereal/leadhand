//! The run a dispatch command may act on (`dispatch.anchored_run`, `check_request`,
//! `_worker_dir`, `_anchored_worker`, `_request_from_args`, `_cmd_runner`; security review
//! H1, H2, M1).
//!
//! The supervisor runs the CLI outside Claude Code's sandbox, so any Claude worker of the
//! session can run `dispatch`, `stop-dispatch` and `dispatch-runner` too. Each acts only on
//! the session's own run, its workspace and its own worker folders:
//! - With `$LEADHAND_RUN_DIR` set (every command of a supervisor session, and every runner its
//!   `dispatch` starts): the run directory must be that one (and `$LEADHAND_RUN_ID`, when
//!   set, its `run.json`'s `run_id`).
//! - Set but empty: refused (a cleared anchor). `cli::resolve_caller` makes that refusal;
//!   it surfaces here, where the Python's `anchored_run` makes it.
//! - Without it (an operator's terminal): the run directory must hold `run.json` with a
//!   `run_id`, and no live supervisor may own it (`holder.json` naming the same `run_id` and
//!   a supervisor whose pid and start time still match): that supervisor's own commands
//!   carry the variable.
//! - Either way the run directory must not lie inside its own workspace or repository, the
//!   worker's cwd must be the run's workspace or its evidence base worktree, the brief and
//!   report must resolve under `<cwd>/.leadhand/`, and a worker folder must be exactly
//!   `<run>/workers/<dispatch id>` (no symlink out of the run).
//!
//! Paths compare as the Python compares them: `os.path.realpath` strings, byte for byte.
//! Every refusal is the Python's text, `refused: …`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;

use crate::briefs::SCRATCH;
use crate::cli::Caller;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::procs;
use crate::pytext::{
    abspath, normpath, pure_path, py_float, py_repr, py_splitlines_bytes, real_path,
};
use crate::wire::{PyMap, PyStr};

use super::{DispatchRequest, REQUEST, read_json_object, read_request};

/// The run a dispatch command acts on: its real path, and what its `run.json` says (string
/// values only; anything else reads as `None`, as the Python's `isinstance` checks do).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// `os.path.realpath` of the run directory.
    pub real: PathBuf,
    pub run_id: Option<PyStr>,
    pub workspace: Option<PyStr>,
    pub repo: Option<PyStr>,
}

impl Anchor {
    /// The run at real path `real`, as its `run.json` says (`_read_json`: nothing when the
    /// file is missing or no JSON object).
    pub fn read(real: &Path) -> Result<Anchor> {
        let meta = read_json_object(&real.join("run.json"))?.unwrap_or_default();
        Ok(Anchor {
            real: real.to_path_buf(),
            run_id: text_field(&meta, "run_id"),
            workspace: text_field(&meta, "workspace"),
            repo: text_field(&meta, "repo"),
        })
    }
}

/// `DispatchError(f"refused: {msg}")`.
fn refuse(msg: impl std::fmt::Display) -> Error {
    Error::msg(format!("refused: {msg}"))
}

/// A string value of a JSON object; `None` when missing or not a string.
fn text_field(map: &PyMap<Box<RawValue>>, key: &str) -> Option<PyStr> {
    map.get(&PyStr::from(key))
        .filter(|raw| raw.get().starts_with('"'))
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
}

/// `os.path.isabs` of text.
fn is_abs(text: &PyStr) -> bool {
    text.starts_with("/")
}

/// `os.path.realpath` of text, as the OS gets it.
fn real_of(text: &PyStr) -> Result<PathBuf> {
    Ok(real_path(Path::new(&text.to_os()?)))
}

/// `_inside`: `path` is `root` (unless `strict`) or below it, comparing the strings.
fn inside(path: &Path, root: &Path, strict: bool) -> bool {
    let path = path.as_os_str().as_bytes();
    let root = root.as_os_str().as_bytes();
    // `root.rstrip("/") or "/"`.
    let root = match root.iter().rposition(|b| *b != b'/') {
        Some(last) => &root[..=last],
        None => b"/",
    };
    if !strict && path == root {
        return true;
    }
    if root == b"/" {
        path.starts_with(b"/")
    } else {
        path.len() > root.len() && path.starts_with(root) && path[root.len()] == b'/'
    }
}

/// `Path(p).name`, of the `pathlib` spelling.
pub(crate) fn py_name(p: &Path) -> OsString {
    let pure = pure_path(p.as_os_str()).into_os_string().into_vec();
    let name = match pure.iter().rposition(|b| *b == b'/') {
        Some(at) => &pure[at + 1..],
        None if pure == b"." => b"",
        None => &pure[..],
    };
    OsString::from_vec(name.to_vec())
}

/// `Path(p).parent`, of the `pathlib` spelling.
pub(crate) fn py_parent(p: &Path) -> PathBuf {
    let pure = pure_path(p.as_os_str()).into_os_string().into_vec();
    let parent = match pure.iter().rposition(|b| *b == b'/') {
        // `/x` and `//x` keep their root; `/` and `//` are their own parent.
        Some(at) if pure[..at].iter().all(|b| *b == b'/') => {
            if pure[at + 1..].is_empty() {
                pure.clone()
            } else {
                pure[..=at].to_vec()
            }
        }
        Some(at) => pure[..at].to_vec(),
        None => b".".to_vec(),
    };
    PathBuf::from(OsString::from_vec(parent))
}

/// `Path(base) / name` for a relative `name`, as `pathlib` spells it (`.` drops away).
fn py_join(base: &Path, name: &str) -> PathBuf {
    let base = pure_path(base.as_os_str());
    if base == Path::new(".") {
        PathBuf::from(name)
    } else {
        base.join(name)
    }
}

/// A path, for a message (shown lossily).
fn shown(p: &Path) -> std::path::Display<'_> {
    p.display()
}

/// `os.path.abspath`, failing as `os.getcwd` does: `[Errno N] <text>`.
fn absolute(p: &Path) -> Result<PathBuf> {
    abspath(p).map_err(|e| {
        let text = e.to_string();
        Error::msg(match e.raw_os_error() {
            Some(n) => format!(
                "[Errno {n}] {}",
                text.strip_suffix(&format!(" (os error {n})"))
                    .unwrap_or(&text)
            ),
            None => text,
        })
    })
}

/// `_check_dispatch_id`: refused when empty, `.` or `..`, or holding `/` or NUL.
pub fn check_dispatch_id(dispatch_id: &str) -> Result<()> {
    if dispatch_id.is_empty()
        || dispatch_id.contains(['/', '\0'])
        || dispatch_id == "."
        || dispatch_id == ".."
    {
        return Err(refuse(format!("bad dispatch id {}", py_repr(dispatch_id))));
    }
    Ok(())
}

/// `anchored_run`: the run `run_dir` names, if the caller may act on it.
pub fn anchored_run(caller: &Result<Caller>, run_dir: &Path) -> Result<Anchor> {
    // `Path(run_dir)`, as the Python's commands pass it.
    let shown_run = pure_path(run_dir.as_os_str());
    let real = real_path(&shown_run);
    // Set but empty: refused where `resolve_caller` read it.
    let caller = caller.as_ref().map_err(Clone::clone)?;
    if let Some(session) = &caller.run_dir
        && real_path(session).as_os_str() != real.as_os_str()
    {
        return Err(refuse(format!(
            "{} is not this session's run ($LEADHAND_RUN_DIR is {})",
            shown(&shown_run),
            session.display()
        )));
    }
    let anchor = Anchor::read(&real)?;
    let Some(run_id) = anchor.run_id.clone().filter(|id| !id.is_empty()) else {
        return Err(refuse(format!(
            "{} is not a run directory (no run.json with a run_id)",
            shown(&shown_run)
        )));
    };
    if caller.run_dir.is_some() {
        if let Some(session_id) = &caller.run_id
            && PyStr::from_os(session_id) != run_id
        {
            return Err(refuse(format!(
                "{} holds run {}, not this session's run {}",
                shown(&shown_run),
                run_id.lossy(),
                session_id.to_string_lossy()
            )));
        }
    } else {
        check_holder(&real, &shown_run, &run_id)?;
    }
    let workspace = anchor.workspace.clone().filter(is_abs).ok_or_else(|| {
        refuse(format!(
            "{}/run.json names no absolute workspace",
            shown(&shown_run)
        ))
    })?;
    for (key, root) in [
        ("workspace", Some(&workspace)),
        ("repo", anchor.repo.as_ref()),
    ] {
        if let Some(root) = root.filter(|r| is_abs(r))
            && inside(&real, &real_of(root)?, false)
        {
            return Err(refuse(format!(
                "the run directory {} lies inside the run's {key} {}",
                shown(&shown_run),
                root.lossy()
            )));
        }
    }
    Ok(anchor)
}

/// Without the session's anchor: refused when `holder.json` names another run, or a
/// supervisor that is still running (its pid and start time match).
fn check_holder(real: &Path, shown_run: &Path, run_id: &PyStr) -> Result<()> {
    let holder = read_json_object(&real.join("holder.json"))?.unwrap_or_default();
    let field = |key: &str| {
        holder
            .get(&PyStr::from(key))
            .filter(|raw| raw.get() != "null")
    };
    if let Some(named) = field("run_id")
        && text_field(&holder, "run_id").as_ref() != Some(run_id)
    {
        return Err(refuse(format!(
            "{}/holder.json names run {}, not {}",
            shown(shown_run),
            py_str_of(named),
            run_id.lossy()
        )));
    }
    // `isinstance(pid, int) and not isinstance(pid, bool)`: an integer literal.
    let pid = field("supervisor_pid").and_then(|raw| serde_json::from_str::<i64>(raw.get()).ok());
    let start = text_field(&holder, "supervisor_start");
    if let (Some(pid), Some(start)) = (pid, start) {
        // `procs.process_info` knows no pid below 1.
        let live = i32::try_from(pid).is_ok_and(|pid| {
            pid > 0
                && std::str::from_utf8(start.as_wtf8())
                    .is_ok_and(|start| procs::same_process(Some(pid), Some(start)))
        });
        if live {
            return Err(refuse(format!(
                "supervisor pid {pid} owns run {}; only commands from its session \
                 ($LEADHAND_RUN_DIR set) act on it",
                run_id.lossy()
            )));
        }
    }
    Ok(())
}

/// `str(value)` of a JSON value, for a message: text as it is, `True`/`False`, a number as
/// Python prints it; anything else as its JSON text.
pub(crate) fn py_str_of(raw: &RawValue) -> String {
    let text = raw.get();
    if let Ok(s) = serde_json::from_str::<PyStr>(text) {
        return s.lossy().into_owned();
    }
    match text {
        "true" => "True".into(),
        "false" => "False".into(),
        _ if serde_json::from_str::<i64>(text).is_ok() => text.into(),
        _ => serde_json::from_str::<f64>(text).map_or_else(|_| text.into(), py_float),
    }
}

/// `base_worktree_path`: the run's evidence base worktree, `<repo>/.worktrees/<workspace
/// name>--base` when the workspace is a worktree there, else `<repo>/.worktrees/<run dir
/// name>--base` (an in-place run).
pub fn base_worktree_path(repo: &Path, workspace: &Path, run_name: &OsStr) -> PathBuf {
    let worktrees = py_join(repo, ".worktrees");
    // `ws.parent == repo / ".worktrees"`: pathlib compares the spellings (`//x` is not `/x`).
    let name = if py_parent(workspace).as_os_str() == worktrees.as_os_str() {
        py_name(workspace)
    } else {
        run_name.to_os_string()
    };
    let mut base = name;
    base.push("--base");
    worktrees.join(base)
}

/// `_allowed_cwds`: the run's workspace, and its evidence base worktree reached through no
/// symlink below the repository (real paths).
fn allowed_cwds(anchor: &Anchor, workspace: &PyStr, run_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = vec![real_of(workspace)?];
    if let Some(repo) = anchor.repo.as_ref().filter(|r| is_abs(r)) {
        let repo_os = repo.to_os()?;
        let ws_os = workspace.to_os()?;
        let real_repo = real_path(Path::new(&repo_os));
        for name in [py_name(run_dir), py_name(&anchor.real)] {
            let base = base_worktree_path(Path::new(&repo_os), Path::new(&ws_os), &name);
            let base_name = py_name(&base);
            out.push(real_repo.join(".worktrees").join(base_name));
        }
    }
    Ok(out)
}

/// `evidence_dirs` (security review M3): for an evidence dispatch, the run's evidence base
/// worktree, where the worker runs the evidence checks (Codex's sandbox lets it write only its
/// cwd and its `--add-dir` folders); nothing for any other kind. Refused unless the run names
/// its repository, and the base worktree is a registered worktree of it reached through no
/// symlink below it (as the engine checks before resetting it). Its real path.
pub fn evidence_dirs(req: &DispatchRequest) -> Result<Vec<PathBuf>> {
    if req.kind.as_deref() != Some("evidence") {
        return Ok(Vec::new());
    }
    let paths = crate::state::run_paths(&req.run_dir);
    let meta = read_json_object(&paths.run_json)?.unwrap_or_default();
    let Some(repo) = text_field(&meta, "repo").filter(is_abs) else {
        return Err(refuse(format!(
            "the run {} names no repository for its evidence base worktree",
            shown(&paths.root)
        )));
    };
    let repo_path = PathBuf::from(repo.to_os()?);
    // `Path(meta.get("workspace") or "")`.
    let workspace = match text_field(&meta, "workspace") {
        Some(ws) => PathBuf::from(ws.to_os()?),
        None => PathBuf::new(),
    };
    let path = base_worktree_path(&repo_path, &workspace, &py_name(&req.run_dir));
    if !resolves_in_place(&repo_path, &path)? {
        return Err(refuse(format!(
            "the evidence base worktree {} is a symlink or lies behind one",
            shown(&path)
        )));
    }
    let listed = Git::at(&repo_path)?.run(&["worktree", "list", "--porcelain"].map(OsStr::new))?;
    let real = real_path(&path);
    let registered = py_splitlines_bytes(&listed).into_iter().any(|line| {
        line.strip_prefix(b"worktree ")
            .is_some_and(|p| real_path(Path::new(OsStr::from_bytes(p))) == real)
    });
    if !registered {
        return Err(refuse(format!(
            "the evidence base worktree {} is not a worktree of {}",
            shown(&path),
            repo.shown()
        )));
    }
    Ok(vec![real])
}

/// `gitops.resolves_in_place`: whether `path` lies inside `root` and resolves to itself, so
/// that neither it nor any folder between `root` and it is a symlink (`root`'s own location
/// may be).
fn resolves_in_place(root: &Path, path: &Path) -> Result<bool> {
    let rel = relpath(&absolute(path)?, &absolute(root)?);
    let rel_bytes = rel.as_os_str().as_bytes();
    if rel_bytes == b".." || rel_bytes.starts_with(b"../") || rel_bytes.starts_with(b"/") {
        return Ok(false);
    }
    let mut joined = real_path(root).into_os_string().into_vec();
    if !joined.ends_with(b"/") {
        joined.push(b'/');
    }
    joined.extend_from_slice(rel_bytes);
    Ok(real_path(path) == normpath(OsStr::from_bytes(&joined)))
}

/// `os.path.relpath(path, start)` of two absolute, normalized paths.
pub(crate) fn relpath(path: &Path, start: &Path) -> PathBuf {
    let parts = |p: &Path| -> Vec<Vec<u8>> {
        p.as_os_str()
            .as_bytes()
            .split(|b| *b == b'/')
            .filter(|c| !c.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    };
    let (start, path) = (parts(start), parts(path));
    let common = start.iter().zip(&path).take_while(|(a, b)| a == b).count();
    let mut rel: Vec<Vec<u8>> = vec![b"..".to_vec(); start.len() - common];
    rel.extend(path[common..].iter().cloned());
    if rel.is_empty() {
        return PathBuf::from(".");
    }
    PathBuf::from(OsString::from_vec(rel.join(&b'/')))
}

/// `check_request`: refused when the request leaves the anchored run (another run
/// directory, a cwd that is not the run's workspace or evidence base worktree, a brief or
/// report outside `<cwd>/.leadhand/`). An error, not a refusal, when `anchor` names no
/// workspace (the Python's `KeyError`; `anchored_run` never gives such an anchor).
pub fn check_request(req: &DispatchRequest, anchor: &Anchor) -> Result<()> {
    check_dispatch_id(&req.dispatch_id)?;
    let run_dir = pure_path(req.run_dir.as_os_str());
    if real_path(&run_dir).as_os_str() != anchor.real.as_os_str() {
        return Err(refuse(format!(
            "the request's run {} is not {}",
            shown(&run_dir),
            shown(&anchor.real)
        )));
    }
    let Some(workspace) = &anchor.workspace else {
        return Err(Error::msg(format!(
            "{}/run.json names no workspace",
            shown(&anchor.real)
        )));
    };
    let cwd_path = pure_path(req.cwd.as_os_str());
    let cwd = real_path(&cwd_path);
    let allowed = allowed_cwds(anchor, workspace, &run_dir)?;
    if !allowed.iter().any(|a| a.as_os_str() == cwd.as_os_str()) {
        return Err(refuse(format!(
            "--cwd {} is not this run's workspace ({}) or its evidence base worktree",
            shown(&cwd_path),
            workspace.lossy()
        )));
    }
    let scratch = cwd.join(SCRATCH);
    for (name, path) in [("brief", &req.brief), ("report", &req.report)] {
        let path = pure_path(path.as_os_str());
        if !inside(&real_path(&path), &scratch, true) {
            return Err(refuse(format!(
                "the {name} {} is not under {}",
                shown(&path),
                shown(&py_join(&cwd_path, SCRATCH))
            )));
        }
    }
    Ok(())
}

/// `_worker_dir`: `<abspath(run_dir)>/workers/<dispatch id>`, refused unless it resolves
/// exactly to `<real run>/workers/<dispatch id>`.
pub fn worker_dir(run_dir: &Path, anchor: &Anchor, dispatch_id: &str) -> Result<PathBuf> {
    check_dispatch_id(dispatch_id)?;
    let wd = absolute(&pure_path(run_dir.as_os_str()))?
        .join("workers")
        .join(dispatch_id);
    let want = anchor.real.join("workers").join(dispatch_id);
    if real_path(&wd).as_os_str() != want.as_os_str() {
        return Err(refuse(format!(
            "the worker folder {} resolves outside the run",
            shown(&wd)
        )));
    }
    Ok(wd)
}

/// The run anchor of `dispatch` (`_request_from_args`, before the request is built): the
/// dispatch ID, the run, and the worker folder it would use. `check_request` then checks the
/// request built for it.
pub fn check_caller(caller: &Result<Caller>, run_dir: &Path, dispatch_id: &str) -> Result<Anchor> {
    check_dispatch_id(dispatch_id)?;
    let anchor = anchored_run(caller, run_dir)?;
    worker_dir(run_dir, &anchor, dispatch_id)?;
    Ok(anchor)
}

/// `_anchored_worker` (`stop-dispatch`): the worker folder of an anchored run and its checked
/// request (`None` when it has none).
pub fn anchored_worker(
    caller: &Result<Caller>,
    run_dir: &Path,
    dispatch_id: &str,
) -> Result<(PathBuf, Option<DispatchRequest>)> {
    let anchor = anchored_run(caller, run_dir)?;
    let wd = worker_dir(run_dir, &anchor, dispatch_id)?;
    if !wd.join(REQUEST).exists() {
        return Ok((wd, None));
    }
    let req = read_request(&wd)?
        .ok_or_else(|| Error::msg(format!("no request.json in {}", shown(&wd))))?;
    if req.dispatch_id != dispatch_id {
        return Err(refuse(format!(
            "{} is for dispatch {}, not {dispatch_id}",
            shown(&wd.join(REQUEST)),
            req.dispatch_id
        )));
    }
    check_request(&req, &anchor)?;
    Ok((wd, Some(req)))
}

/// `dispatch-runner`'s check (`_cmd_runner`): `worker_dir` must be `<run>/workers/<id>` of an
/// anchored run, with a checked request. A folder name that is not UTF-8 is refused as a
/// bad dispatch ID.
pub fn runner_worker(
    caller: &Result<Caller>,
    worker_dir: &Path,
) -> Result<(PathBuf, DispatchRequest)> {
    let wd = absolute(worker_dir)?;
    let parent = py_parent(&wd);
    if py_name(&parent) != "workers" {
        return Err(refuse(format!(
            "{} is not a run's worker folder",
            shown(&wd)
        )));
    }
    let name = py_name(&wd);
    let Some(dispatch_id) = name.to_str() else {
        return Err(refuse(format!(
            "bad dispatch id {}",
            PyStr::from_os(&name).repr()
        )));
    };
    let (wd, req) = anchored_worker(caller, &py_parent(&parent), dispatch_id)?;
    let req = req.ok_or_else(|| Error::msg(format!("no request.json in {}", shown(&wd))))?;
    Ok((wd, req))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::symlink;

    use serde::Serialize;

    use super::*;
    use crate::cli::resolve_caller_in;
    use crate::wire::tests::python;
    use crate::wire::{ascii_json, to_line};

    /// The run directories, workspaces and worker folders every scenario runs against.
    struct World {
        _tmp: tempfile::TempDir,
        t: PathBuf,
        ws: PathBuf,
    }

    fn json_path(p: &Path) -> String {
        ascii_json(&PyStr::from_os(p.as_os_str()))
    }

    impl World {
        fn runs(&self, name: &str) -> PathBuf {
            self.t.join("runs").join(name)
        }

        /// A run directory at `dir` with `run_json` (raw text) and `holder` when given.
        fn run_at(&self, dir: &Path, run_json: Option<String>, holder: Option<String>) {
            fs::create_dir_all(dir).unwrap();
            if let Some(text) = run_json {
                fs::write(dir.join("run.json"), text).unwrap();
            }
            if let Some(text) = holder {
                fs::write(dir.join("holder.json"), text).unwrap();
            }
        }

        fn meta(&self, run_id: &str, ws: &Path) -> String {
            format!(
                r#"{{"run_id": "{run_id}", "workspace": {}, "repo": {}}}"#,
                json_path(ws),
                json_path(&self.t.join("repo"))
            )
        }

        /// A request of the good run, for dispatch `id`.
        fn request(&self, id: &str) -> DispatchRequest {
            let scratch = self.ws.join(".leadhand/1").join(id);
            DispatchRequest {
                model: PyStr::from("gpt-6-luna"),
                effort: PyStr::from("medium"),
                role: "implementer".to_string(),
                brief: scratch.join("brief.md"),
                report: scratch.join("report.md"),
                cwd: self.ws.clone(),
                run_dir: self.runs("good"),
                card: Some(1),
                attempt: 1,
                dispatch_id: id.to_string(),
                steps: 40,
                minutes: 20,
                network: false,
                writable_dirs: Vec::new(),
                resume: None,
                kind: Some("implement".to_string()),
                branch: Some(PyStr::from("lh/good")),
            }
        }

        fn write_request(&self, run: &str, folder: &str, req: Option<&DispatchRequest>) {
            let wd = self.runs(run).join("workers").join(folder);
            fs::create_dir_all(&wd).unwrap();
            if let Some(req) = req {
                fs::write(wd.join("request.json"), to_line(req).unwrap()).unwrap();
            }
        }
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        // Not resolved: on macOS the temporary folder lies behind a symlink, so the real paths
        // the checks compare differ from these.
        let t = tmp.path().to_path_buf();
        let repo = t.join("repo");
        let ws = repo.join(".worktrees/ws");
        fs::create_dir_all(ws.join(".leadhand/1/c1-a1")).unwrap();
        fs::create_dir_all(t.join("elsewhere")).unwrap();
        fs::create_dir_all(t.join("ws2/.leadhand")).unwrap();
        symlink(t.join("elsewhere"), ws.join(".leadhand/link")).unwrap();
        let w = World { _tmp: tmp, t, ws };
        let me = std::process::id() as i32;
        let start = crate::procs::identify(me).unwrap().start;
        let ws = w.ws.clone();
        let ws2 = w.t.join("ws2");
        w.run_at(&w.runs("good"), Some(w.meta("r-good", &ws)), None);
        w.run_at(&w.runs("inplace"), Some(w.meta("r-inplace", &ws2)), None);
        w.run_at(&w.runs("norun"), None, None);
        w.run_at(&w.runs("emptyid"), Some(w.meta("", &ws)), None);
        let int_id = format!(r#"{{"run_id": 5, "workspace": {}}}"#, json_path(&ws));
        w.run_at(&w.runs("intid"), Some(int_id), None);
        w.run_at(&w.runs("listjson"), Some("[1]".into()), None);
        w.run_at(&w.runs("badjson"), Some("{".into()), None);
        w.run_at(
            &w.runs("relws"),
            Some(r#"{"run_id": "r", "workspace": "rel/ws"}"#.into()),
            None,
        );
        w.run_at(&w.runs("nows"), Some(r#"{"run_id": "r"}"#.into()), None);
        w.run_at(
            &w.runs("intws"),
            Some(r#"{"run_id": "r", "workspace": 5}"#.into()),
            None,
        );
        w.run_at(&ws.join("inside-run"), Some(w.meta("r-inws", &ws)), None);
        w.run_at(
            &w.t.join("repo/docs/runs/x"),
            Some(w.meta("r-inrepo", &ws2)),
            None,
        );
        let ws3 = w.t.join("ws3");
        w.run_at(&ws3, Some(w.meta("r-isws", &ws3)), None);
        let holder = |run_id: &str, rest: &str| Some(format!(r#"{{"run_id": {run_id}{rest}}}"#));
        let live = format!(r#", "supervisor_pid": {me}, "supervisor_start": "{start}""#);
        let named = |name: &str| w.meta(name, &ws);
        w.run_at(
            &w.runs("held-other"),
            Some(named("r-ho")),
            holder(r#""someone""#, ""),
        );
        w.run_at(&w.runs("held-int"), Some(named("r-hi")), holder("7", ""));
        w.run_at(
            &w.runs("held-live"),
            Some(named("r-hl")),
            holder(r#""r-hl""#, &live),
        );
        w.run_at(
            &w.runs("held-null"),
            Some(named("r-hn")),
            holder("null", &live),
        );
        let dead = format!(r#", "supervisor_pid": {me}, "supervisor_start": "0.000001""#);
        w.run_at(
            &w.runs("held-dead"),
            Some(named("r-hd")),
            holder(r#""r-hd""#, &dead),
        );
        let as_bool = format!(r#", "supervisor_pid": true, "supervisor_start": "{start}""#);
        w.run_at(
            &w.runs("held-bool"),
            Some(named("r-hb")),
            holder(r#""r-hb""#, &as_bool),
        );
        let as_text = format!(r#", "supervisor_pid": "{me}", "supervisor_start": "{start}""#);
        w.run_at(
            &w.runs("held-str"),
            Some(named("r-hs")),
            holder(r#""r-hs""#, &as_text),
        );
        w.run_at(&w.runs("held-bad"), Some(named("r-hx")), Some("{".into()));
        let dup = format!(
            r#"{{"run_id": "a", "workspace": {}, "run_id": "r-dup"}}"#,
            json_path(&ws)
        );
        w.run_at(&w.runs("dup"), Some(dup), None);
        w.run_at(&w.runs("escape"), Some(named("r-esc")), None);
        symlink(w.t.join("elsewhere"), w.runs("escape").join("workers")).unwrap();
        symlink(w.runs("good"), w.runs("goodlink")).unwrap();
        // The good run's worker folders.
        w.write_request("good", "c1-a1", Some(&w.request("c1-a1")));
        w.write_request("good", "mismatch", Some(&w.request("c1-a1")));
        let mut far = w.request("badcwd");
        far.cwd = w.t.join("elsewhere");
        w.write_request("good", "badcwd", Some(&far));
        w.write_request("good", "broken", None);
        fs::write(w.runs("good").join("workers/broken/request.json"), "{").unwrap();
        w.write_request("good", "none", None);
        symlink(w.t.join("elsewhere"), w.runs("good").join("workers/linked")).unwrap();
        w
    }

    enum Call {
        /// `anchored_run(run)`.
        Anchored(PathBuf),
        /// `dispatch`'s check: `check_caller(run, id)`.
        Caller(PathBuf, &'static str),
        /// `check_request` of a request, against the anchored run.
        Request(PathBuf, Box<DispatchRequest>),
        /// `stop-dispatch`'s check: `anchored_worker(run, id)`.
        Worker(PathBuf, &'static str),
        /// `dispatch-runner`'s check: `runner_worker(worker_dir)`.
        Runner(PathBuf),
    }

    struct Scenario {
        env: Vec<(&'static str, OsString)>,
        call: Call,
        /// What the outcome starts with, beyond agreeing with the Python.
        expect: String,
    }

    #[derive(Serialize)]
    struct PyScenario<'a> {
        #[serde(rename = "fn")]
        kind: &'static str,
        env: Vec<(&'static str, PyStr)>,
        run: Option<PyStr>,
        id: Option<&'static str>,
        req: Option<&'a DispatchRequest>,
        wd: Option<PyStr>,
    }

    fn py(p: &Path) -> Option<PyStr> {
        Some(PyStr::from_os(p.as_os_str()))
    }

    impl Scenario {
        fn for_python(&self) -> PyScenario<'_> {
            let env = self
                .env
                .iter()
                .map(|(k, v)| (*k, PyStr::from_os(v)))
                .collect();
            let blank = PyScenario {
                kind: "",
                env,
                run: None,
                id: None,
                req: None,
                wd: None,
            };
            match &self.call {
                Call::Anchored(run) => PyScenario {
                    kind: "anchored",
                    run: py(run),
                    ..blank
                },
                Call::Caller(run, id) => PyScenario {
                    kind: "caller",
                    run: py(run),
                    id: Some(id),
                    ..blank
                },
                Call::Request(run, req) => PyScenario {
                    kind: "request",
                    run: py(run),
                    req: Some(req),
                    ..blank
                },
                Call::Worker(run, id) => PyScenario {
                    kind: "worker",
                    run: py(run),
                    id: Some(id),
                    ..blank
                },
                Call::Runner(wd) => PyScenario {
                    kind: "runner",
                    wd: py(wd),
                    ..blank
                },
            }
        }

        fn ours(&self) -> String {
            let caller = resolve_caller_in(&|name| {
                self.env
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            });
            let got = match &self.call {
                Call::Anchored(run) => {
                    anchored_run(&caller, run).map(|a| format!("ok {}", a.real.display()))
                }
                Call::Caller(run, id) => {
                    check_caller(&caller, run, id).map(|a| format!("ok {}", a.real.display()))
                }
                Call::Request(run, req) => anchored_run(&caller, run)
                    .and_then(|a| check_request(req, &a))
                    .map(|()| "ok".to_string()),
                Call::Worker(run, id) => {
                    anchored_worker(&caller, run, id).map(|(wd, req)| match req {
                        None => format!("ok none {}", wd.display()),
                        Some(_) => format!("ok req {}", wd.display()),
                    })
                }
                Call::Runner(wd) => {
                    runner_worker(&caller, wd).map(|(wd, _)| format!("ok {}", wd.display()))
                }
            };
            got.unwrap_or_else(|e| format!("err:{}", e.message))
        }
    }

    /// Each scenario's Python outcome: `ok …` or `err:<the DispatchError's text>`. The
    /// Python's `dispatch` check is `_request_from_args` without `request_for` (which reads
    /// the run's configuration); `dispatch-runner`'s is `_cmd_runner` with `run_runner`
    /// replaced, its `leadhand: ` prefix dropped.
    const PY_SCENARIOS: &str = "import argparse, contextlib, io, json, os, sys\n\
        from pathlib import Path\n\
        os.environ.pop('LEADHAND_TESTING', None)\n\
        from leadhand import dispatch as d\n\
        seen = []\n\
        d.run_runner = lambda wd: (seen.append(str(wd)), 0)[1]\n\
        out = []\n\
        for sc in json.loads(sys.argv[1]):\n\
        \x20 for k in ('LEADHAND_RUN_DIR', 'LEADHAND_RUN_ID'):\n\
        \x20   os.environ.pop(k, None)\n\
        \x20 os.environ.update(dict(sc['env']))\n\
        \x20 f = sc['fn']\n\
        \x20 try:\n\
        \x20   if f == 'anchored':\n\
        \x20     real, meta = d.anchored_run(Path(sc['run']))\n\
        \x20     res = 'ok ' + str(real)\n\
        \x20   elif f == 'caller':\n\
        \x20     d._check_dispatch_id(sc['id'])\n\
        \x20     real, meta = d.anchored_run(Path(sc['run']))\n\
        \x20     d._worker_dir(Path(sc['run']), real, sc['id'])\n\
        \x20     res = 'ok ' + str(real)\n\
        \x20   elif f == 'request':\n\
        \x20     real, meta = d.anchored_run(Path(sc['run']))\n\
        \x20     d.check_request(d.request_from_json(sc['req']), real, meta)\n\
        \x20     res = 'ok'\n\
        \x20   elif f == 'worker':\n\
        \x20     wd, req = d._anchored_worker(Path(sc['run']), sc['id'])\n\
        \x20     res = ('ok none ' if req is None else 'ok req ') + str(wd)\n\
        \x20   else:\n\
        \x20     err = io.StringIO()\n\
        \x20     seen.clear()\n\
        \x20     with contextlib.redirect_stderr(err):\n\
        \x20       code = d._cmd_runner(argparse.Namespace(worker_dir=sc['wd']))\n\
        \x20     res = ('ok ' + seen[0]) if code == 0 else 'err:' + err.getvalue().removeprefix('leadhand: ').rstrip('\\n')\n\
        \x20 except d.DispatchError as e:\n\
        \x20   res = 'err:' + str(e)\n\
        \x20 out.append(res)\n\
        print(json.dumps(out))\n";

    fn scenarios(w: &World) -> Vec<Scenario> {
        let real = |p: &Path| crate::pytext::real_path(p).display().to_string();
        let session = |run: &str| vec![("LEADHAND_RUN_DIR", w.runs(run).into_os_string())];
        let none = Vec::new;
        let sc = |env, call, expect: String| Scenario { env, call, expect };
        let good = w.runs("good");
        let ok_good = format!("ok {}", real(&good));
        let ws = w.ws.clone();
        let base = w.t.join("repo/.worktrees/ws--base");
        let mut out = vec![
            // H1: the session's anchor.
            sc(
                vec![("LEADHAND_RUN_DIR", "".into())],
                Call::Anchored(good.clone()),
                "err:refused: $LEADHAND_RUN_DIR is set but empty".into(),
            ),
            sc(
                vec![("LEADHAND_RUN_DIR", " \t".into())],
                Call::Anchored(good.clone()),
                "err:refused: $LEADHAND_RUN_DIR is set but empty".into(),
            ),
            // Python's whitespace includes U+001C..U+001F, which Rust's `trim` keeps.
            sc(
                vec![("LEADHAND_RUN_DIR", "\u{1c}\u{1f}\u{a0}".into())],
                Call::Anchored(good.clone()),
                "err:refused: $LEADHAND_RUN_DIR is set but empty".into(),
            ),
            sc(
                session("norun"),
                Call::Anchored(good.clone()),
                format!(
                    "err:refused: {} is not this session's run ($LEADHAND_RUN_DIR is {})",
                    good.display(),
                    w.runs("norun").display()
                ),
            ),
            sc(
                session("goodlink"),
                Call::Anchored(good.clone()),
                ok_good.clone(),
            ),
            sc(
                session("good"),
                Call::Anchored(w.runs("goodlink")),
                ok_good.clone(),
            ),
            sc(
                vec![
                    ("LEADHAND_RUN_DIR", good.clone().into_os_string()),
                    ("LEADHAND_RUN_ID", "r-other".into()),
                ],
                Call::Anchored(good.clone()),
                format!(
                    "err:refused: {} holds run r-good, not this session's run r-other",
                    good.display()
                ),
            ),
            sc(
                vec![
                    ("LEADHAND_RUN_DIR", good.clone().into_os_string()),
                    ("LEADHAND_RUN_ID", "".into()),
                ],
                Call::Anchored(good.clone()),
                ok_good.clone(),
            ),
            sc(none(), Call::Anchored(good.clone()), ok_good.clone()),
            // The holder, without the session's anchor.
            sc(
                none(),
                Call::Anchored(w.runs("held-other")),
                format!(
                    "err:refused: {}/holder.json names run someone, not r-ho",
                    w.runs("held-other").display()
                ),
            ),
            sc(
                none(),
                Call::Anchored(w.runs("held-int")),
                format!(
                    "err:refused: {}/holder.json names run 7, not r-hi",
                    w.runs("held-int").display()
                ),
            ),
            sc(
                none(),
                Call::Anchored(w.runs("held-live")),
                format!(
                    "err:refused: supervisor pid {} owns run r-hl; only commands from its \
                     session ($LEADHAND_RUN_DIR set) act on it",
                    std::process::id()
                ),
            ),
            sc(
                none(),
                Call::Anchored(w.runs("held-null")),
                "err:refused: supervisor pid".into(),
            ),
            // The live supervisor's own commands carry the anchor.
            sc(
                session("held-live"),
                Call::Anchored(w.runs("held-live")),
                "ok ".into(),
            ),
            sc(none(), Call::Anchored(w.runs("dup")), "ok ".into()),
            sc(
                vec![
                    ("LEADHAND_RUN_DIR", w.runs("dup").into_os_string()),
                    ("LEADHAND_RUN_ID", "r-dup".into()),
                ],
                Call::Anchored(w.runs("dup")),
                "ok ".into(),
            ),
        ];
        for name in ["held-dead", "held-bool", "held-str", "held-bad"] {
            out.push(sc(none(), Call::Anchored(w.runs(name)), "ok ".into()));
        }
        for name in ["norun", "emptyid", "intid", "listjson", "badjson"] {
            out.push(sc(
                none(),
                Call::Anchored(w.runs(name)),
                format!(
                    "err:refused: {} is not a run directory (no run.json with a run_id)",
                    w.runs(name).display()
                ),
            ));
        }
        for name in ["relws", "nows", "intws"] {
            out.push(sc(
                none(),
                Call::Anchored(w.runs(name)),
                format!(
                    "err:refused: {}/run.json names no absolute workspace",
                    w.runs(name).display()
                ),
            ));
        }
        // The run directory inside its own workspace or repository.
        for (dir, key, root) in [
            (ws.join("inside-run"), "workspace", ws.clone()),
            (w.t.join("repo/docs/runs/x"), "repo", w.t.join("repo")),
            (w.t.join("ws3"), "workspace", w.t.join("ws3")),
        ] {
            let expect = format!(
                "err:refused: the run directory {} lies inside the run's {key} {}",
                dir.display(),
                root.display()
            );
            out.push(sc(none(), Call::Anchored(dir), expect));
        }
        // The dispatch ID, checked before the anchor.
        for id in ["", "a/b", ".", "..", "a\0b"] {
            out.push(sc(
                vec![("LEADHAND_RUN_DIR", "".into())],
                Call::Caller(good.clone(), id),
                format!(
                    "err:refused: bad dispatch id {}",
                    crate::pytext::py_repr(id)
                ),
            ));
        }
        out.push(sc(
            session("good"),
            Call::Caller(good.clone(), "c1-a1"),
            ok_good.clone(),
        ));
        // M1: a worker folder that resolves out of the run.
        out.push(sc(
            session("escape"),
            Call::Caller(w.runs("escape"), "c1"),
            format!(
                "err:refused: the worker folder {}/workers/c1 resolves outside the run",
                w.runs("escape").display()
            ),
        ));
        out.push(sc(
            session("good"),
            Call::Caller(good.clone(), "linked"),
            "err:refused: the worker folder ".into(),
        ));
        // H2: the request stays in the run's workspace.
        let req = |f: &dyn Fn(&mut DispatchRequest)| {
            let mut r = w.request("c1-a1");
            f(&mut r);
            r
        };
        let request = |r: DispatchRequest, expect: String| {
            sc(
                session("good"),
                Call::Request(good.clone(), Box::new(r)),
                expect,
            )
        };
        out.push(request(req(&|_| {}), "ok".into()));
        out.push(request(
            req(&|r| r.run_dir = w.runs("goodlink")),
            "ok".into(),
        ));
        out.push(request(
            req(&|r| r.run_dir = w.runs("inplace")),
            format!(
                "err:refused: the request's run {} is not {}",
                w.runs("inplace").display(),
                real(&good)
            ),
        ));
        out.push(request(
            req(&|r| r.cwd = w.t.join("elsewhere")),
            format!(
                "err:refused: --cwd {} is not this run's workspace ({}) or its evidence base \
                 worktree",
                w.t.join("elsewhere").display(),
                ws.display()
            ),
        ));
        out.push(request(
            req(&|r| {
                r.cwd = base.clone();
                r.brief = base.join(".leadhand/1/c1-a1/brief.md");
                r.report = base.join(".leadhand/1/c1-a1/report.md");
            }),
            "ok".into(),
        ));
        out.push(request(
            req(&|r| {
                let mut cwd = ws.clone().into_os_string();
                cwd.push("/./");
                r.cwd = cwd.into();
            }),
            "ok".into(),
        ));
        out.push(request(
            req(&|r| r.brief = w.t.join("elsewhere/brief.md")),
            format!(
                "err:refused: the brief {} is not under {}/.leadhand",
                w.t.join("elsewhere/brief.md").display(),
                ws.display()
            ),
        ));
        out.push(request(
            req(&|r| r.report = ws.join("report.md")),
            format!(
                "err:refused: the report {} is not under {}/.leadhand",
                ws.join("report.md").display(),
                ws.display()
            ),
        ));
        out.push(request(
            req(&|r| r.brief = ws.join(".leadhand")),
            "err:refused: the brief ".into(),
        ));
        out.push(request(
            req(&|r| r.brief = ws.join(".leadhand/link/brief.md")),
            "err:refused: the brief ".into(),
        ));
        out.push(request(
            req(&|r| r.dispatch_id = "../x".into()),
            "err:refused: bad dispatch id '../x'".into(),
        ));
        // An in-place run's evidence base worktree is named for the run directory.
        let inplace_base = w.t.join("repo/.worktrees/inplace--base");
        let mut r = w.request("c1-a1");
        r.run_dir = w.runs("inplace");
        r.cwd = inplace_base.clone();
        r.brief = inplace_base.join(".leadhand/b.md");
        r.report = inplace_base.join(".leadhand/r.md");
        out.push(sc(
            session("inplace"),
            Call::Request(w.runs("inplace"), Box::new(r.clone())),
            "ok".into(),
        ));
        r.cwd = base.clone();
        r.brief = base.join(".leadhand/b.md");
        r.report = base.join(".leadhand/r.md");
        out.push(sc(
            session("inplace"),
            Call::Request(w.runs("inplace"), Box::new(r)),
            "err:refused: --cwd ".into(),
        ));
        // stop-dispatch: the worker folder and its request.
        let wd = |id: &str| good.join("workers").join(id);
        let worker = |id: &'static str, expect: String| {
            sc(session("good"), Call::Worker(good.clone(), id), expect)
        };
        out.push(worker("none", format!("ok none {}", wd("none").display())));
        out.push(worker("c1-a1", format!("ok req {}", wd("c1-a1").display())));
        out.push(worker(
            "mismatch",
            format!(
                "err:refused: {}/request.json is for dispatch c1-a1, not mismatch",
                wd("mismatch").display()
            ),
        ));
        out.push(worker("badcwd", "err:refused: --cwd ".into()));
        out.push(worker(
            "broken",
            format!("err:no request.json in {}", wd("broken").display()),
        ));
        out.push(sc(
            none(),
            Call::Worker(w.runs("norun"), "c1-a1"),
            "err:refused: ".into(),
        ));
        // dispatch-runner: the folder must be <run>/workers/<id>.
        out.push(sc(
            session("good"),
            Call::Runner(good.join("other/c1-a1")),
            format!(
                "err:refused: {} is not a run's worker folder",
                good.join("other/c1-a1").display()
            ),
        ));
        out.push(sc(
            session("good"),
            Call::Runner(wd("c1-a1")),
            format!("ok {}", wd("c1-a1").display()),
        ));
        out.push(sc(
            session("good"),
            Call::Runner(wd("none")),
            format!("err:no request.json in {}", wd("none").display()),
        ));
        out.push(sc(
            session("good"),
            Call::Runner(wd("badcwd")),
            "err:refused: --cwd ".into(),
        ));
        out.push(sc(
            vec![("LEADHAND_RUN_DIR", "".into())],
            Call::Runner(wd("c1-a1")),
            "err:refused: $LEADHAND_RUN_DIR is set but empty".into(),
        ));
        out.push(sc(
            session("good"),
            Call::Runner(PathBuf::from("/")),
            "err:refused: / is not a run's worker folder".into(),
        ));
        out
    }

    #[test]
    fn every_anchor_refusal_matches_python() {
        let w = world();
        let scenarios = scenarios(&w);
        let py: Vec<PyScenario> = scenarios.iter().map(Scenario::for_python).collect();
        let arg = to_line(&py).unwrap();
        let out = python(PY_SCENARIOS, &[OsStr::new(&arg)]);
        let theirs: Vec<String> = serde_json::from_str(std::str::from_utf8(&out).unwrap()).unwrap();
        assert_eq!(theirs.len(), scenarios.len());
        for (s, want) in scenarios.iter().zip(&theirs) {
            let ours = s.ours();
            assert_eq!(&ours, want);
            assert!(
                ours.starts_with(&s.expect),
                "{ours:?} does not start with {:?}",
                s.expect
            );
        }
    }

    #[test]
    fn base_worktree_path_is_named_for_the_workspace_or_the_run() {
        let repo = Path::new("/repo");
        assert_eq!(
            base_worktree_path(repo, Path::new("/repo/.worktrees/ws"), OsStr::new("r1")),
            Path::new("/repo/.worktrees/ws--base")
        );
        assert_eq!(
            base_worktree_path(repo, Path::new("/repo//.worktrees/ws/"), OsStr::new("r1")),
            Path::new("/repo/.worktrees/ws--base")
        );
        assert_eq!(
            base_worktree_path(repo, Path::new("/repo"), OsStr::new("r1")),
            Path::new("/repo/.worktrees/r1--base")
        );
        assert_eq!(
            base_worktree_path(
                repo,
                Path::new("/elsewhere/.worktrees/ws"),
                OsStr::new("r1")
            ),
            Path::new("/repo/.worktrees/r1--base")
        );
    }

    #[test]
    fn a_request_against_an_anchor_without_a_workspace_is_an_error() {
        let anchor = Anchor {
            real: PathBuf::from("/runs/r"),
            run_id: None,
            workspace: None,
            repo: None,
        };
        let mut req = World {
            _tmp: tempfile::tempdir().unwrap(),
            t: PathBuf::from("/t"),
            ws: PathBuf::from("/ws"),
        }
        .request("c1-a1");
        req.run_dir = PathBuf::from("/runs/r");
        let err = check_request(&req, &anchor).unwrap_err();
        assert!(!err.message.starts_with("refused"), "{}", err.message);
    }

    #[test]
    fn evidence_dirs_match_python() {
        // Security review M3: the evidence base worktree, as `dispatch.evidence_dirs` finds it
        // or refuses it.
        let f = crate::git::fixture::Fixture::new();
        let root = f.root().to_path_buf();
        let repo = f.make_repo("my repo");
        let add = |path: &Path| {
            f.git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    path.to_str().unwrap(),
                ],
            );
        };
        let worktrees = repo.join(".worktrees");
        add(&worktrees.join("ws"));
        add(&worktrees.join("ws--base"));
        add(&worktrees.join("run in place--base"));
        add(&root.join("elsewhere"));
        fs::create_dir(worktrees.join("plain")).unwrap();
        fs::create_dir(worktrees.join("plain--base")).unwrap();
        add(&worktrees.join("linked"));
        symlink(root.join("elsewhere"), worktrees.join("linked--base")).unwrap();
        // A run per case: (run dir name, run.json).
        let quoted = |p: &Path| ascii_json(&PyStr::from_os(p.as_os_str()));
        let meta = |ws: &Path| {
            format!(
                "{{\"run_id\": \"r\", \"repo\": {}, \"workspace\": {}}}",
                quoted(&repo),
                quoted(ws)
            )
        };
        let runs: Vec<(&str, String)> = vec![
            ("no repo", "{\"run_id\": \"r\"}".into()),
            (
                "relative repo",
                "{\"run_id\": \"r\", \"repo\": \"my repo\"}".into(),
            ),
            ("worktree run", meta(&worktrees.join("ws"))),
            ("run in place", meta(&repo)),
            ("unregistered", meta(&worktrees.join("plain"))),
            ("behind a link", meta(&worktrees.join("linked"))),
            ("missing", meta(&worktrees.join("gone"))),
        ];
        let mut reqs = Vec::new();
        for (name, json) in &runs {
            let run = root.join(name);
            fs::create_dir(&run).unwrap();
            fs::write(run.join("run.json"), json).unwrap();
            for kind in [Some("evidence"), Some("review"), None] {
                let mut req = World {
                    _tmp: tempfile::tempdir().unwrap(),
                    t: root.clone(),
                    ws: repo.clone(),
                }
                .request("c1-a1-evidence-1");
                req.run_dir = run.clone();
                req.kind = kind.map(str::to_string);
                reqs.push(req);
            }
        }
        let code = "import json, os, sys\nfrom leadhand import dispatch as d\nout = []\n\
                    for c in json.loads(sys.argv[1]):\n\
                    \x20 try:\n\
                    \x20   dirs = d.evidence_dirs(d.request_from_json(c))\n\
                    \x20   out.append('ok ' + ' '.join(os.fsencode(p).hex() for p in dirs))\n\
                    \x20 except d.DispatchError as e:\n\
                    \x20   out.append('err ' + str(e))\n\
                    print(json.dumps(out))\n";
        let arg = to_line(&reqs).unwrap();
        let theirs: Vec<String> =
            serde_json::from_str(std::str::from_utf8(&python(code, &[OsStr::new(&arg)])).unwrap())
                .unwrap();
        assert_eq!(theirs.len(), reqs.len());
        let mut found = 0;
        for (req, want) in reqs.iter().zip(&theirs) {
            let ours = match evidence_dirs(req) {
                Ok(dirs) => {
                    let hex: Vec<String> = dirs
                        .iter()
                        .map(|d| {
                            d.as_os_str()
                                .as_bytes()
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect()
                        })
                        .collect();
                    format!("ok {}", hex.join(" "))
                }
                Err(e) => format!("err {}", e.message),
            };
            assert_eq!(&ours, want, "{:?} {:?}", req.run_dir, req.kind);
            found += usize::from(want.starts_with("ok ") && want.len() > 3);
        }
        // The worktree run and the in-place run find theirs; every other kind gets nothing.
        assert_eq!(found, 2, "{theirs:#?}");
    }
}
