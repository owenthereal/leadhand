//! Attacks from the security review (docs/compatibility.md, Security), each reproduced with the
//! fakes and scratch repositories; each must fail closed.
//!
//! The Rust form of the Python contract suite's `contract_security` (converted from
//! Python's `test_security`). The dispatch scenarios (H1, H2, M1) run on a run that `start` made
//! (`common::dispatch_world::World`: a worktree workspace, Claude and Codex installed), with the
//! fakes first on PATH and no LEADHAND_TEST_CODEX_BIN, so the `codex` found on PATH is the
//! fake; a second repository, the victim, is what a forged request aims at. `dispatch`,
//! `stop-dispatch` and `dispatch-runner` run with $LEADHAND_RUN_DIR set (a command of the
//! supervisor's session), unset (an operator's terminal) or emptied, as each test says. A
//! refusal is a non-zero exit other than 75 with `refused` on stderr.
//!
//! What a refusal must leave alone is judged from outside the binary: the fake Codex's own log
//! (whether any Codex started), the victim repository's HEAD and status, a decoy process of the
//! test's own (alive over a window, by pid and start time), the files under the folders a
//! worker pointed elsewhere, and Git. The engine scenarios (L1, L2, M3) drive `FakeSupervisor`;
//! L6 drives `run` through `common::launch_world::World`. A path the binary uses for itself
//! comes from the testing hook LEADHAND_TEST_CLI, as `tests/launch.rs` does (S1-R11).
//!
//! M3's checks (the evidence base worktree and the run's own worktree are never reached
//! through a symlink) are worktree scenarios. H3 drives whole runs with an operator's global
//! Git configuration holding a fake git-lfs filter whose smudge logs a "fetch" unless
//! GIT_LFS_SKIP_SMUDGE=1; the control (plain Git with the same configuration does fetch from
//! the worker's URL) runs last, inside the same test, so it never feeds the binary's checks.
//! The Python's fake git-lfs was a Python script; here it is a POSIX shell script that keeps
//! the same store, log and calls files (`FAKE_LFS`).
//!
//! GitSafety, at the end, is converted from Python's `test_git_safety`: leadhand's own Git calls
//! run no hook or fsmonitor command, and the runner never commits a gitlink.

#[macro_use]
mod common;

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::dispatch_world::{Req, WAIT, World, alive, at, os_refs, wait_for_with};
use common::engine::{
    CODEX_CARD, base_wt, codex_world, drive_until_refused, files, gate_names, gated, marker,
    tampering, write_script,
};
use common::git::{commit, make_repo, make_repo_named, run_git, write};
use common::json::{self, Json};
use common::launch_world::{RunOpts, World as LaunchWorld, now};
use common::supervisor::{
    CardOpts, FakeSupervisor, Options, Step, card, card_with, os_path, run_dir, text, truthy,
};
use common::{
    Env, Out, add_days, cli_with, field, holds, jv, lines, mkdir_new, one, path_str, read_json,
    read_text, relpath, resolve, run_argv, set_mtime, started_by, symlink, today, walk, write_text,
};

const SECRET: &str = "SECRET-KEY-MATERIAL";

/// An environment of `env`'s variables less those `drop` names (Python's
/// `{k: v for k, v in env.items() if not drop(k)}`).
fn env_without(env: &Env, drop: impl Fn(&str) -> bool) -> Env {
    Env::from_parts(
        env.vars()
            .filter(|(k, _)| !drop(&k.to_string_lossy()))
            .map(|(k, v)| (k.to_os_string(), v.to_os_string())),
        env.fakes().to_path_buf(),
    )
}

/// Makes the folder `path` unless it is one already; its parent must exist
/// (`Path.mkdir(exist_ok=True)`).
#[track_caller]
fn mkdir_exist_ok(path: &Path) {
    if !path.is_dir() {
        fs::create_dir(path).unwrap_or_else(|e| panic!("making {}: {e}", path.display()));
    }
}

// --------------------------------------------------------------------------
// H1, H2, M1: the dispatch commands act only on the session's run

/// LEADHAND_RUN_DIR for a command: the session's run (`True`), unset (`False`), or a string.
#[derive(Clone, Copy)]
enum Session<'a> {
    Set,
    Unset,
    Value(&'a str),
}

/// `Anchored.dispatch_args`'s keyword arguments; `did` defaults to `d1`.
struct DispatchArgs<'a> {
    run: Option<&'a Path>,
    cwd: Option<&'a Path>,
    brief: Option<&'a Path>,
    report: Option<&'a Path>,
    did: &'a str,
}

impl Default for DispatchArgs<'_> {
    fn default() -> Self {
        DispatchArgs {
            run: None,
            cwd: None,
            brief: None,
            report: None,
            did: "d1",
        }
    }
}

/// A run `start` made, the dispatch commands on it, and a second repository (the victim) a
/// worker must never reach. `cli` runs a command as the CLI found on PATH would with the fake
/// `codex` first on PATH and no test override of the Codex binary.
struct Anchored {
    w: World,
    run_dir: PathBuf,
    ws: PathBuf,
    repo: PathBuf,
    tmp: PathBuf,
    branch: String,
    victim: PathBuf,
    victim_head: String,
}

impl Anchored {
    #[track_caller]
    fn new() -> Anchored {
        let mut w = World::new();
        let (run_dir, ws, repo, tmp) = (
            w.run_dir.clone(),
            w.ws.clone(),
            w.repo.clone(),
            w.tmp.clone(),
        );
        let branch = run_git(&ws, &["symbolic-ref", "--short", "HEAD"]);
        let victim = make_repo(&tmp.join("victim"));
        let victim_head = run_git(&victim, &["rev-parse", "HEAD"]);
        w.scenario("ready", &[]);
        Anchored {
            w,
            run_dir,
            ws,
            repo,
            tmp,
            branch,
            victim,
            victim_head,
        }
    }

    fn env(&self, session: Session) -> Env {
        let mut e = self
            .w
            .env
            .with("PATH", self.w.env.path_with_fakes())
            .without("LEADHAND_TEST_CODEX_BIN");
        match session {
            Session::Set => {
                e.set("LEADHAND_RUN_DIR", &self.run_dir);
            }
            Session::Unset => {
                e.remove("LEADHAND_RUN_DIR");
            }
            Session::Value(v) => {
                e.set("LEADHAND_RUN_DIR", v);
            }
        }
        e
    }

    /// Runs the CLI as a worker in the supervisor's session would (`Session::Set`), with
    /// LEADHAND_RUN_DIR unset, or with it set to a string; 60 s at most.
    #[track_caller]
    fn cli(&self, args: &[&OsStr], session: Session) -> Out {
        cli_with(
            args,
            &self.env(session),
            &self.tmp,
            None,
            Duration::from_secs(60),
        )
    }

    #[track_caller]
    fn dispatch_args(&self, o: DispatchArgs) -> Vec<OsString> {
        let cwd = o.cwd.unwrap_or(&self.ws);
        let folder = cwd.join(".leadhand/1").join(o.did);
        fs::create_dir_all(&folder).unwrap_or_else(|e| panic!("making {}: {e}", folder.display()));
        let brief = match o.brief {
            Some(b) => b.to_path_buf(),
            None => {
                let b = folder.join("brief.md");
                write_text(&b, "# Brief\n");
                b
            }
        };
        let report = o
            .report
            .map_or_else(|| folder.join("report.md"), Path::to_path_buf);
        let run = o.run.unwrap_or(&self.run_dir);
        let mut argv: Vec<OsString> = vec!["dispatch".into(), "--run".into(), run.into()];
        for (flag, value) in [
            ("--dispatch-id", OsString::from(o.did)),
            ("--card", "1".into()),
            ("--attempt", "1".into()),
            ("--role", "implementer".into()),
            ("--model", "gpt-6-luna".into()),
            ("--effort", "low".into()),
            ("--brief", brief.into()),
            ("--report", report.into()),
            ("--cwd", cwd.into()),
            ("--kind", "implement".into()),
            ("--branch", self.branch.clone().into()),
        ] {
            argv.extend([flag.into(), value]);
        }
        argv
    }

    /// `cli` of a dispatch line built by `dispatch_args`.
    #[track_caller]
    fn dispatch(&self, o: DispatchArgs, session: Session) -> Out {
        let args = self.dispatch_args(o);
        self.cli(&os_refs(&args), session)
    }

    #[track_caller]
    fn starts(&self) -> Vec<Json> {
        self.w.records("start")
    }

    /// An unrelated process of the test's own: `{pid, pgid, start}`, the start time read from
    /// the process table.
    #[track_caller]
    fn decoy(&mut self) -> Json {
        let p = self.w.decoy();
        let start = started_by(p.pid(), now());
        assert!(start.is_some(), "the decoy {} has no start time", p.pid());
        Json::object([
            ("pid", p.pid().into()),
            ("pgid", p.pid().into()),
            ("start", start.unwrap().into()),
        ])
    }

    #[track_caller]
    fn victim_request(&self, root: &Path, did: &str) -> Json {
        let mut req = self.w.request_json(did);
        *at(&mut req, &["cwd"]) = path_str(&self.victim).into();
        *at(&mut req, &["kind"]) = "implement".into();
        *at(&mut req, &["branch"]) = "main".into();
        *at(&mut req, &["run_dir"]) = path_str(root).into();
        *at(&mut req, &["dispatch_id"]) = did.into();
        req
    }

    /// A worker folder under `root` whose runner.json names a live decoy process group and
    /// whose request.json commits in the victim repository.
    #[track_caller]
    fn forged_folder(&mut self, root: &Path, did: &str) -> (PathBuf, Json) {
        let wd = root.join("workers").join(did);
        mkdir_new(&wd);
        let rec = self.decoy();
        write_text(
            &wd.join("request.json"),
            json::dumps(&self.victim_request(root, did)),
        );
        write_runner_json(&wd, &rec);
        write(&self.victim, &[("stray.env", "SECRET=1\n")]);
        (wd, rec)
    }
}

/// runner.json naming `rec` as both the runner and Codex.
#[track_caller]
fn write_runner_json(wd: &Path, rec: &Json) {
    let runner = Json::object([
        ("runner", rec.clone()),
        ("codex", rec.clone()),
        ("generation", 1.into()),
    ]);
    write_text(&wd.join("runner.json"), json::dumps(&runner));
}

/// A refusal: a non-zero exit other than 75, with `refused` on stderr.
macro_rules! assert_refused {
    ($r:expr) => {{
        let r: Out = $r;
        assert!(r.code != 0 && r.code != 75, "{r}");
        assert!(r.stderr_text().contains("refused"), "{r}");
    }};
}

/// No Codex worker started, and the victim's HEAD is where it was.
macro_rules! assert_nothing_ran {
    ($s:expr) => {{
        let s: &Anchored = &$s;
        let starts = s.starts();
        assert!(starts.is_empty(), "a Codex worker was started: {starts:?}");
        assert_eq!(run_git(&s.victim, &["rev-parse", "HEAD"]), s.victim_head);
    }};
}

/// The decoy stays alive over a window, and the victim's HEAD and stray file are as they were.
macro_rules! assert_untouched {
    ($s:expr, $rec:expr) => {{
        let s: &Anchored = &$s;
        let rec: &Json = &$rec;
        holds(|| alive(rec), 0.3, "the decoy outside the run stays alive");
        assert_eq!(run_git(&s.victim, &["rev-parse", "HEAD"]), s.victim_head);
        assert!(run_git(&s.victim, &["status", "--porcelain"]).contains("stray.env"));
    }};
}

/// H1: request.json is never a source of an executable (ruling R49).
mod h1_forged_codex_binary {
    use super::*;

    #[test]
    fn test_runner_never_runs_a_binary_named_in_request_json() {
        let s = Anchored::new();
        let flag = s.tmp.join("PWNED");
        let evil = s.tmp.join("evil.sh");
        write_script(
            &evil,
            &format!("#!/bin/sh\necho \"ran $0\" >> \"{}\"\n", flag.display()),
        );
        let wd = s.run_dir.join("workers/d1");
        mkdir_new(&wd);
        let mut forged = s.w.request_json("d1");
        *at(&mut forged, &["codex_bin"]) = path_str(&evil).into();
        write_text(&wd.join("request.json"), json::dumps(&forged));
        let r = s.cli(&os_args!["dispatch-runner", wd], Session::Unset);
        assert!(!flag.exists(), "the forged binary ran\n{r}");
        // The `codex` on PATH (the fake) ran instead.
        assert_eq!(s.starts().len(), 1, "{r}");
    }

    #[test]
    fn test_test_override_is_honoured_only_under_leadhand_testing() {
        // Covers limit L4 (test hooks need LEADHAND_TESTING=1).
        // Observed through the program that runs: an override that leaves a mark, then runs the
        // fake, against the fake found on PATH.
        let s = Anchored::new();
        let flag = s.tmp.join("override ran");
        let override_ = s.tmp.join("fake codex override");
        write_script(
            &override_,
            &format!(
                "#!/bin/sh\necho ran >> \"{}\"\nexec \"{}\" \"$@\"\n",
                flag.display(),
                s.w.env.fakes().join("codex").display()
            ),
        );
        let over = path_str(&override_);
        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], bool); // did, overrides, honoured
        let cases: [Case; 3] = [
            (
                "d1",
                &[("LEADHAND_TEST_CODEX_BIN", over), ("LEADHAND_TESTING", "0")],
                false,
            ),
            (
                "d2",
                &[("LEADHAND_TEST_CODEX_BIN", over), ("LEADHAND_TESTING", "1")],
                true,
            ),
            ("d3", &[("LEADHAND_TESTING", "1")], false),
        ];
        for (n, (did, overrides, _)) in cases.iter().enumerate() {
            let n = n + 1;
            let mut env = s.env(Session::Unset);
            for (k, v) in *overrides {
                env.set(k, v);
            }
            let args = s.dispatch_args(DispatchArgs {
                did,
                ..DispatchArgs::default()
            });
            cli_with(&os_refs(&args), &env, &s.tmp, None, Duration::from_secs(60)).ok();
            assert_eq!(s.starts().len(), n, "did={did}");
            let lines = if flag.exists() {
                lines(&read_text(&flag))
            } else {
                Vec::new()
            };
            let honoured = cases[..n].iter().filter(|c| c.2).count();
            assert_eq!(lines.len(), honoured, "did={did}: {lines:?}");
        }
    }

    #[test]
    fn test_request_json_carries_no_executable() {
        let s = Anchored::new();
        s.dispatch(DispatchArgs::default(), Session::Set).ok();
        let request = read_json(&s.run_dir.join("workers/d1/request.json"));
        assert!(request.get("codex_bin").is_none(), "{request}");
        assert_eq!(s.starts().len(), 1);
    }
}

/// H2: `leadhand dispatch` acts only on the session's run, in its workspace.
mod h2_confined_dispatch {
    use super::*;

    #[test]
    fn test_cwd_in_another_repository_is_refused() {
        let s = Anchored::new();
        let victim = s.victim.clone();
        assert_refused!(s.dispatch(
            DispatchArgs {
                cwd: Some(&victim),
                ..DispatchArgs::default()
            },
            Session::Set
        ));
        assert!(!s.run_dir.join("workers/d1/request.json").exists());
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_run_dir_other_than_the_sessions_is_refused() {
        let s = Anchored::new();
        let forged = s.ws.join(".leadhand/fake");
        mkdir_new(&forged);
        let meta = Json::object([
            ("run_id", "fake".into()),
            ("repo", path_str(&s.victim).into()),
            ("workspace", path_str(&s.victim).into()),
        ]);
        write_text(&forged.join("run.json"), json::dumps(&meta));
        assert_refused!(s.dispatch(
            DispatchArgs {
                run: Some(&forged),
                cwd: Some(&s.victim),
                ..DispatchArgs::default()
            },
            Session::Set
        ));
        assert!(!forged.join("workers").exists());
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_without_the_session_variable_a_dir_without_run_json_is_refused() {
        let s = Anchored::new();
        let forged = s.ws.join(".leadhand/fake");
        mkdir_new(&forged);
        assert_refused!(s.dispatch(
            DispatchArgs {
                run: Some(&forged),
                ..DispatchArgs::default()
            },
            Session::Unset
        ));
        assert!(!forged.join("workers").exists());
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused() {
        let mut s = Anchored::new();
        let rec = s.decoy();
        let holder = Json::object([
            ("run_id", s.w.run_id.as_str().into()),
            ("supervisor_pid", rec["pid"].clone()),
            ("supervisor_start", rec["start"].clone()),
        ]);
        write_text(&s.run_dir.join("holder.json"), json::dumps(&holder));
        assert_refused!(s.dispatch(DispatchArgs::default(), Session::Unset));
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_an_empty_session_variable_is_refused_not_read_as_unset() {
        // An emptied anchor is a worker clearing it, never an operator's terminal.
        let mut s = Anchored::new();
        for blank in ["", " "] {
            assert_refused!(s.dispatch(DispatchArgs::default(), Session::Value(blank)));
        }
        assert_nothing_ran!(s);
        let wd = s.run_dir.join("workers/y");
        mkdir_new(&wd);
        let rec = s.decoy();
        write_text(
            &wd.join("request.json"),
            json::dumps(&s.w.request_json("y")),
        );
        write_runner_json(&wd, &rec);
        assert_refused!(s.cli(
            &os_args!["stop-dispatch", s.run_dir, "--dispatch", "y"],
            Session::Value("")
        ));
        holds(
            || alive(&rec),
            0.3,
            "the process runner.json names stays alive",
        );
        fs::remove_file(wd.join("runner.json")).expect("removing runner.json");
        assert_refused!(s.cli(&os_args!["dispatch-runner", wd], Session::Value("")));
        assert!(!wd.join("result.json").exists());
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_run_dir_inside_its_own_workspace_is_refused() {
        let s = Anchored::new();
        let inside = s.ws.join(".leadhand/run");
        mkdir_new(&inside);
        let meta = Json::object([
            ("run_id", "inside".into()),
            ("repo", path_str(&s.repo).into()),
            ("workspace", path_str(&s.ws).into()),
        ]);
        write_text(&inside.join("run.json"), json::dumps(&meta));
        assert_refused!(s.dispatch(
            DispatchArgs {
                run: Some(&inside),
                ..DispatchArgs::default()
            },
            Session::Unset
        ));
        assert!(!inside.join("workers").exists());
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_brief_or_report_outside_the_workspace_scratch_dir_is_refused() {
        let s = Anchored::new();
        let outside = s.tmp.join("evil brief.md");
        write_text(&outside, "# Brief\n");
        assert_refused!(s.dispatch(
            DispatchArgs {
                brief: Some(&outside),
                ..DispatchArgs::default()
            },
            Session::Set
        ));
        let report = s.ws.join("report.md");
        assert_refused!(s.dispatch(
            DispatchArgs {
                report: Some(&report),
                ..DispatchArgs::default()
            },
            Session::Set
        ));
        assert_nothing_ran!(s);
    }

    #[test]
    fn test_the_sessions_run_in_its_workspace_is_accepted() {
        let s = Anchored::new();
        let r = s.dispatch(DispatchArgs::default(), Session::Set);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(s.starts().len(), 1);
    }

    #[test]
    fn test_the_runs_evidence_base_worktree_is_accepted_as_cwd() {
        let s = Anchored::new();
        let mut name = s.ws.file_name().unwrap().to_os_string();
        name.push("--base");
        let base = s.repo.join(".worktrees").join(name);
        run_git(
            &s.repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path_str(&base),
                "HEAD",
            ],
        );
        let r = s.dispatch(
            DispatchArgs {
                cwd: Some(&base),
                ..DispatchArgs::default()
            },
            Session::Set,
        );
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(s.starts().len(), 1);
    }
}

/// M1 (and H1/H2's runner half): stop-dispatch and dispatch-runner act only on worker
/// folders of the session's run, and signal only what those record.
mod m1_confined_stop {
    use super::*;

    #[test]
    fn test_stop_dispatch_refuses_a_forged_folder_outside_the_run() {
        let mut s = Anchored::new();
        let fk = s.ws.join(".leadhand/fk");
        let (_, rec) = s.forged_folder(&fk, "y");
        for session in [Session::Set, Session::Unset] {
            assert_refused!(s.cli(&os_args!["stop-dispatch", fk, "--dispatch", "y"], session));
        }
        assert_untouched!(s, rec);
    }

    #[test]
    fn test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder() {
        let mut s = Anchored::new();
        let fk = s.ws.join(".leadhand/fk");
        let (wd, rec) = s.forged_folder(&fk, "y");
        let workers = s.run_dir.join("workers");
        mkdir_exist_ok(&workers);
        let escape = relpath(&wd, &workers);
        assert_refused!(s.cli(
            &os_args!["stop-dispatch", s.run_dir, "--dispatch", escape],
            Session::Set
        ));
        assert_untouched!(s, rec);
    }

    #[test]
    fn test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run() {
        let mut s = Anchored::new();
        let fk = s.ws.join(".leadhand/fk");
        let (wd, rec) = s.forged_folder(&fk, "y");
        let workers = s.run_dir.join("workers");
        mkdir_exist_ok(&workers);
        symlink(&wd, &workers.join("y"));
        assert_refused!(s.cli(
            &os_args!["stop-dispatch", s.run_dir, "--dispatch", "y"],
            Session::Set
        ));
        assert_untouched!(s, rec);
    }

    #[test]
    fn test_stop_dispatch_refuses_a_request_for_another_repository() {
        // A worker folder inside the run whose request.json was forged to name the victim.
        let mut s = Anchored::new();
        let root = s.run_dir.clone();
        let (_, rec) = s.forged_folder(&root, "y");
        assert_refused!(s.cli(
            &os_args!["stop-dispatch", s.run_dir, "--dispatch", "y"],
            Session::Set
        ));
        assert_untouched!(s, rec);
    }

    #[test]
    fn test_runner_commit_refuses_a_request_for_another_repository() {
        // The runner's entry check already refuses a request naming the victim, so the
        // commit's own re-check is reached by changing the run instead: while Codex works,
        // run.json is rewritten to name the victim as the workspace. The commit then refuses
        // the request, and commits nothing anywhere.
        let mut s = Anchored::new();
        write(&s.victim, &[("stray.env", "SECRET=1\n")]);
        let ws_head = run_git(&s.ws, &["rev-parse", "HEAD"]);
        // The window: run.json must be rewritten within the fake Codex's 5 s delay.
        s.w.scenario("ready", &[("delay", "5")]);
        let branch = s.branch.clone();
        let p = s.w.popen_with(
            "d1",
            &Req {
                kind: Some("implement"),
                branch: Some(&branch),
                ..Req::default()
            },
        );
        wait_for_with("Codex started", WAIT, Some(&p), || s.w.codex_of("d1"));
        let run_json = s.run_dir.join("run.json");
        let mut meta = read_json(&run_json);
        *at(&mut meta, &["workspace"]) = path_str(&s.victim).into();
        write_text(&run_json, json::dumps(&meta));
        // communicate(timeout=120) raises TimeoutExpired, failing the test.
        p.communicate(Duration::from_secs(120))
            .expect("the dispatch line exited within 120 s");
        let res = s.w.result("d1");
        assert!(res["committed"].is_null(), "{res}");
        assert!(text(&res["commit_error"]).contains("refused"));
        assert_eq!(s.starts().len(), 1);
        assert_eq!(run_git(&s.victim, &["rev-parse", "HEAD"]), s.victim_head);
        assert!(run_git(&s.victim, &["status", "--porcelain"]).contains("stray.env"));
        assert_eq!(run_git(&s.ws, &["rev-parse", "HEAD"]), ws_head);
        assert!(
            lines(&run_git(&s.ws, &["status", "--porcelain"])).contains(&"?? done.txt".to_string())
        );
    }

    #[test]
    fn test_runner_refuses_a_folder_outside_the_run() {
        let mut s = Anchored::new();
        let fk = s.ws.join(".leadhand/fk");
        let (wd, _) = s.forged_folder(&fk, "y");
        fs::remove_file(wd.join("runner.json")).expect("removing runner.json");
        for session in [Session::Set, Session::Unset] {
            assert_refused!(s.cli(&os_args!["dispatch-runner", wd], session));
        }
        assert!(!wd.join("result.json").exists());
        assert_nothing_ran!(s);
        assert!(run_git(&s.victim, &["status", "--porcelain"]).contains("stray.env"));
    }

    #[test]
    fn test_runner_refuses_a_request_for_another_repository() {
        let mut s = Anchored::new();
        let root = s.run_dir.clone();
        let (wd, _) = s.forged_folder(&root, "y");
        fs::remove_file(wd.join("runner.json")).expect("removing runner.json");
        assert_refused!(s.cli(&os_args!["dispatch-runner", wd], Session::Set));
        assert!(!wd.join("result.json").exists());
        assert_nothing_ran!(s);
        assert!(run_git(&s.victim, &["status", "--porcelain"]).contains("stray.env"));
    }
}

// --------------------------------------------------------------------------
// H3: a worker's .gitattributes and .lfsconfig never make leadhand's Git calls fetch

/// A git-lfs stand-in for single-file clean/smudge, called as `FAKE_LFS STORE LOG CALLS
/// clean|smudge`: clean stores content and prints a pointer; smudge logs that it ran (with
/// GIT_LFS_SKIP_SMUDGE), prints stored content, prints the pointer as is under
/// GIT_LFS_SKIP_SMUDGE=1 (as git-lfs does), and otherwise "fetches" the object from
/// .lfsconfig's URL: logged, then failed. The input is kept in a file beside CALLS, as a shell
/// variable cannot hold every byte.
const FAKE_LFS: &str = r#"#!/bin/sh
STORE=$1 LOG=$2 CALLS=$3 MODE=$4
POINTER='version https://git-lfs.github.com/spec/v1'
data="$CALLS.$$.in"
trap 'rm -f "$data"' EXIT
cat > "$data" || exit 1
# Whether the input starts with the pointer line, its newline included.
is_pointer() {
    [ $(( $(wc -l < "$data") )) -ge 1 ] && [ "$(head -n 1 "$data")" = "$POINTER" ]
}
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -d ' ' -f 1
}
if [ "$MODE" = clean ]; then
    if is_pointer; then
        cat "$data"
    else
        oid=$(sha256 < "$data")
        mkdir -p "$STORE"
        cp "$data" "$STORE/$oid"
        printf '%s\noid sha256:%s\nsize %s\n' "$POINTER" "$oid" $(( $(wc -c < "$data") ))
    fi
    exit 0
fi
printf 'SMUDGE skip=%s\n' "${GIT_LFS_SKIP_SMUDGE-(unset)}" >> "$CALLS"
oid=$(sed -n 's/.*oid sha256:\([0-9a-f]\{64\}\).*/\1/p' "$data" | head -n 1)
if ! is_pointer || [ "${GIT_LFS_SKIP_SMUDGE-}" = 1 ]; then
    cat "$data"
elif [ -n "$oid" ] && [ -e "$STORE/$oid" ]; then
    cat "$STORE/$oid"
else
    url=""
    if [ -e .lfsconfig ]; then
        set -f
        set -- $(cat .lfsconfig)
        url="$*"
    fi
    printf 'FETCH %s\n' "$url" >> "$LOG"
    exit 1
fi
"#;
const POINTER: &str = "version https://git-lfs.github.com/spec/v1\n";
const EXFIL: &str = "127.0.0.1:9/exfil/SECRET-DATA";
const ATTRIBUTES: &str = "*.bin filter=lfs diff=lfs merge=lfs -text\n";

/// The operator's LFS filter: its `-c` settings, and the files its fake writes.
struct Lfs {
    fetch_log: PathBuf,
    calls: PathBuf,
    filters: [String; 3],
}

impl Lfs {
    /// Plain git with the operator's filter (run_git shuts the global config out).
    #[track_caller]
    fn with_filter(&self, cwd: &Path, args: &[&str]) -> String {
        let mut all: Vec<&str> = Vec::new();
        for f in &self.filters {
            all.extend(["-c", f.as_str()]);
        }
        all.extend_from_slice(args);
        run_git(cwd, &all)
    }
}

/// No smudge fetched anything.
macro_rules! assert_no_fetch {
    ($lfs:expr) => {{
        let lfs: &Lfs = &$lfs;
        let log = if lfs.fetch_log.exists() {
            read_text(&lfs.fetch_log)
        } else {
            String::new()
        };
        assert!(!lfs.fetch_log.exists(), "{log}");
    }};
}

/// leadhand's checkouts ran the smudge, each time with GIT_LFS_SKIP_SMUDGE=1.
macro_rules! assert_every_smudge_skipped {
    ($lfs:expr) => {{
        let lfs: &Lfs = &$lfs;
        let calls = if lfs.calls.exists() {
            lines(&read_text(&lfs.calls))
        } else {
            Vec::new()
        };
        assert!(
            !calls.is_empty(),
            "leadhand's checkouts ran no smudge at all"
        );
        let set: BTreeSet<&str> = calls.iter().map(String::as_str).collect();
        assert_eq!(set, BTreeSet::from(["SMUDGE skip=1"]));
    }};
}

/// H3: a worker's .gitattributes and .lfsconfig never make leadhand's own Git calls fetch LFS
/// objects (a smudge POSTs to the URL in .lfsconfig, outside every sandbox), while the
/// operator's LFS clean filter keeps working (no LFS file reads as modified).
///
/// The operator's global configuration (GIT_CONFIG_GLOBAL, as `git lfs install` writes it)
/// reaches only the binary; the harness's own Git calls and the fake workers' commits shut it
/// out, as a worker's commit made without the filter would be.
mod h3_filter_drivers {
    use super::*;

    /// Claude workers only (their steps can tamper), or with Codex installed too.
    #[track_caller]
    fn world(cards: &[String], codex: bool) -> (FakeSupervisor, Lfs) {
        let mut w = if codex {
            codex_world(cards, Options::default())
        } else {
            FakeSupervisor::new(cards)
        };
        let fetch_log = w.tmp.join("LFS FETCHES");
        let calls = w.tmp.join("LFS CALLS");
        let fake = w.tmp.join("fake-lfs");
        write_script(&fake, FAKE_LFS);
        let store = w.tmp.join("lfs store");
        let cmd = format!(
            "'{}' '{}' '{}' '{}'",
            fake.display(),
            store.display(),
            fetch_log.display(),
            calls.display()
        );
        let filters = [
            format!("filter.lfs.clean={cmd} clean"),
            format!("filter.lfs.smudge={cmd} smudge"),
            "filter.lfs.required=true".to_string(),
        ];
        let config = w.tmp.join("global gitconfig");
        let mut text = "[filter \"lfs\"]\n".to_string();
        for f in &filters {
            let (key, value) = f.split_once('=').unwrap();
            text += &format!("\t{} = {value}\n", key.trim_start_matches("filter.lfs."));
        }
        write_text(&config, text);
        w.env.set("GIT_CONFIG_GLOBAL", &config);
        w.env.remove("GIT_LFS_SKIP_SMUDGE");
        let lfs = Lfs {
            fetch_log,
            calls,
            filters,
        };
        // The operator's repository uses LFS: big.bin committed through the clean filter (a
        // pointer in the tree, its content in the store and in the operator's checkout).
        write(
            &w.repo,
            &[
                (".gitattributes", ATTRIBUTES),
                ("big.bin", "large binary content\n"),
            ],
        );
        lfs.with_filter(&w.repo, &["add", ".gitattributes", "big.bin"]);
        lfs.with_filter(&w.repo, &["commit", "--quiet", "-m", "big"]);
        assert!(run_git(&w.repo, &["cat-file", "-p", "HEAD:big.bin"]).starts_with(POINTER));
        w.base = run_git(&w.repo, &["rev-parse", "HEAD"]);
        w.write_deck(cards);
        (w, lfs)
    }

    #[test]
    fn test_checkouts_resets_and_stashes_fetch_nothing() {
        // Covers R50.
        // Card 1's worker commits a pointer to an object nobody has and an .lfsconfig naming
        // its URL; card 2's worker deletes that file and leaves the deletion uncommitted, so
        // before its review leadhand saves the leftovers (stash create) and resets the
        // workspace, which writes the pointer file back through the smudge filter.
        let plant = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            let lfsconfig = format!("[lfs]\n\turl = http://{EXFIL}\n");
            let pointer = format!("{POINTER}oid sha256:{}\nsize 12\n", "ab".repeat(32));
            write(ws, &[(".lfsconfig", &lfsconfig), ("p.bin", &pointer)]);
            run_git(ws, &["add", "--", ".lfsconfig", "p.bin"]);
            run_git(ws, &["commit", "--quiet", "-m", "worker's lfs files"]);
        };
        let delete = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            fs::remove_file(ws.join("p.bin")).expect("removing p.bin");
        };
        let (mut w, lfs) = world(&[card(1), card(2)], false);
        w.start();
        w.script(1, "implement", [tampering(plant)]);
        w.script(2, "implement", [tampering(delete)]);
        assert_eq!(w.run().last().unwrap()["action"], "done");
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1, 2]"));
        let left = one(w
            .events("violation")
            .into_iter()
            .filter(|e| {
                e.get("stage").is_some_and(|v| v == "found")
                    && e.get("by").is_some_and(|v| v == "implementer")
            })
            .collect());
        assert_json_eq!(left["paths"], jv(r#"["p.bin"]"#));
        assert!(
            !lines(&run_git(
                &w.repo,
                &["ls-tree", "--name-only", text(&left["ref"])]
            ))
            .contains(&"p.bin".to_string())
        );
        let ws = w.workspace();
        assert!(read_text(&ws.join("p.bin")).starts_with(POINTER));
        assert_no_fetch!(lfs);
        assert_every_smudge_skipped!(lfs);
        // Control: without leadhand's environment, a checkout of the same run branch fetches
        // from the worker's URL.
        let env = env_without(&w.env, |k| {
            k.starts_with("LEADHAND") || k == "GIT_LFS_SKIP_SMUDGE"
        });
        let plain = w.tmp.join("plain checkout");
        run_argv(
            &os_args![
                "git",
                "worktree",
                "add",
                "--quiet",
                "--detach",
                plain,
                "refs/heads/leadhand/demo"
            ],
            &env,
            &w.repo,
            None,
            Duration::from_secs(60),
        );
        assert!(read_text(&lfs.fetch_log).contains(EXFIL));
    }

    #[test]
    fn test_checked_out_lfs_content_does_not_read_as_modified() {
        // The reviewer pulls the real content of big.bin into the workspace (as `git lfs pull`
        // would), its stat data stale: Git must run the clean filter to compare, and finds the
        // pointer the index holds. Not a change, so not a violation.
        let pull = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[("big.bin", "large binary content\n")]);
            let old = SystemTime::now() - Duration::from_secs(3600);
            set_mtime(&ws.join("big.bin"), old);
        };
        let (mut w, lfs) = world(&[card(1)], false);
        w.start();
        w.script(1, "review", [tampering(pull)]);
        assert_eq!(w.run().last().unwrap()["action"], "done");
        assert_empty!(w.events("violation"));
        assert_json_eq!(
            field(&w.events("verdict_recorded"), "dispatch_id"),
            jv(r#"["c1-a1-review-1"]"#)
        );
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1]"));
        assert_eq!(
            read_text(&w.workspace().join("big.bin")),
            "large binary content\n"
        );
        assert!(
            run_git(&w.repo, &["cat-file", "-p", "leadhand/demo:big.bin"]).starts_with(POINTER)
        );
        assert_no_fetch!(lfs);
    }

    #[test]
    fn test_runner_commits_lfs_files_as_pointers_and_fetches_nothing() {
        let codex_card = card_with(
            1,
            CardOpts {
                route: Some(CODEX_CARD),
                ..CardOpts::default()
            },
        );
        let (mut w, lfs) = world(&[codex_card], true);
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                commit: Some(false),
                ..files(&[
                    ("q.bin", "new binary\n"),
                    ("big.bin", "changed large content\n"),
                    ("README.md", "changed\n"),
                ])
            }],
        );
        assert_eq!(w.run().last().unwrap()["action"], "done");
        let accepted = one(w.events("accepted"));
        let tip = text(&accepted["sha"]).to_string();
        for path in ["q.bin", "big.bin"] {
            assert!(
                run_git(&w.repo, &["cat-file", "-p", &format!("{tip}:{path}")])
                    .starts_with(POINTER),
                "{path}"
            );
        }
        assert_eq!(
            run_git(&w.repo, &["show", &format!("{tip}:README.md")]),
            "changed"
        );
        let trailer = run_git(
            &w.repo,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Leadhand-Dispatch,valueonly)",
                &tip,
            ],
        );
        assert_eq!(trailer, "c1-a1-implement-1");
        assert_no_fetch!(lfs);
        assert_every_smudge_skipped!(lfs);
    }
}

// --------------------------------------------------------------------------
// M3: the evidence base worktree and the run's worktree are never reached through a symlink

/// The operator's other worktree, on its own branch `other`, with uncommitted and untracked
/// work; and its HEAD.
#[track_caller]
fn other_worktree(w: &FakeSupervisor) -> (PathBuf, String) {
    let other = w.tmp.join("other worktree");
    run_git(
        &w.repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "other",
            path_str(&other),
        ],
    );
    write(
        &other,
        &[
            ("README.md", "uncommitted work\n"),
            ("untracked.env", "KEEP=1\n"),
        ],
    );
    let head = run_git(&other, &["rev-parse", "HEAD"]);
    (other, head)
}

/// The other worktree is as `other_worktree` left it, and holds no run's marker.
macro_rules! assert_other_untouched {
    ($other:expr, $head:expr) => {{
        let other: &Path = &$other;
        assert_eq!(read_text(&other.join("README.md")), "uncommitted work\n");
        assert_eq!(read_text(&other.join("untracked.env")), "KEEP=1\n");
        assert_eq!(
            run_git(other, &["symbolic-ref", "--short", "HEAD"]),
            "other"
        );
        assert_eq!(run_git(other, &["rev-parse", "HEAD"]), $head);
        assert!(!marker(other).exists());
    }};
}

/// M3: a symlink at the evidence base worktree's path, or at a folder above it, must never
/// turn the evidence reset (checkout --force, reset --hard, clean -fdx) on another worktree;
/// nor may `start` take a symlink at the run worktree's path. The other worktree holds the
/// operator's uncommitted and untracked work on its own branch.
mod m3_symlinked_base_worktree {
    use super::*;

    #[test]
    fn test_evidence_reset_refuses_a_symlinked_base_worktree() {
        let mut w = gated(&[card(1)]);
        w.start();
        let (other, other_head) = other_worktree(&w);
        let link = base_wt(&w);
        symlink(&other, &link);
        let r = drive_until_refused(&mut w);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        assert!(r.stderr_text().contains("symlink"));
        assert_eq!(fs::read_link(&link).unwrap(), other);
        assert_other_untouched!(other, other_head);
        assert!(!gate_names(&w).iter().any(|g| g == "evidence"));
    }

    #[test]
    fn test_evidence_reset_refuses_a_symlinked_worktrees_folder() {
        // While the final reviewer runs, <repo>/.worktrees is moved aside and replaced by a
        // symlink to a decoy folder, where the run's worktree is a symlink to its real place
        // and the base worktree's own name a symlink to the other worktree.
        let mut w = gated(&[card(1)]);
        w.start();
        let (other, other_head) = other_worktree(&w);
        let target = other.clone();
        let swap = move |world: &FakeSupervisor, ws: &Path, _a: &Json| {
            let repo = &world.repo;
            let name = ws.file_name().unwrap();
            let moved = repo.join(".worktrees moved");
            fs::rename(repo.join(".worktrees"), &moved).expect("moving .worktrees aside");
            run_git(repo, &["worktree", "repair", path_str(&moved.join(name))]);
            let decoy = repo.join("decoy");
            fs::create_dir(&decoy).unwrap();
            symlink(&moved.join(name), &decoy.join(name));
            let mut base = name.to_os_string();
            base.push("--base");
            symlink(&target, &decoy.join(base));
            symlink(&decoy, &repo.join(".worktrees"));
        };
        w.script(None, "final_review", [tampering(swap)]);
        let r = drive_until_refused(&mut w);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        assert!(r.stderr_text().contains("symlink"));
        assert_other_untouched!(other, other_head);
        assert!(!gate_names(&w).iter().any(|g| g == "evidence"));
    }

    #[test]
    fn test_start_refuses_a_symlinked_worktree() {
        // The run's worktree goes to <repo>/.worktrees/<run directory name>, named
        // <date>-<deck slug> (the deck is "Demo deck"): a symlink there, to another worktree,
        // is refused. Named for yesterday, today and tomorrow, so neither the date turning
        // before `start` nor a binary that dates its runs in UTC misses it.
        let w = FakeSupervisor::new(&[card(1)]);
        let (other, other_head) = other_worktree(&w);
        fs::create_dir(w.repo.join(".worktrees")).unwrap();
        let today = today();
        for day in [add_days(&today, -1), today.clone(), add_days(&today, 1)] {
            symlink(
                &other,
                &w.repo.join(".worktrees").join(format!("{day}-demo-deck")),
            );
        }
        let listed = run_git(&w.repo, &["worktree", "list", "--porcelain"]);
        let r = w.cli(&os_args!["start", w.deck_path, "--repo", w.repo]);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().starts_with("error: "), "{r}");
        assert!(r.stderr_text().contains("symlink"));
        assert_eq!(
            run_git(&w.repo, &["worktree", "list", "--porcelain"]),
            listed
        );
        assert_other_untouched!(other, other_head);
    }
}

// --------------------------------------------------------------------------
// L1: `record result --reply-file` reads only the dispatch's own reply file

/// A run whose first dispatch's worker has done its work, and a secret outside the run.
struct ReplyWorld {
    w: FakeSupervisor,
    a: Json,
    worker: String,
    secret: PathBuf,
}

impl ReplyWorld {
    #[track_caller]
    fn new() -> ReplyWorld {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let worker = w.launch(&a);
        w.perform(&a, &worker);
        let secret = w.tmp.join("id_secret");
        write_text(&secret, format!("{SECRET}\n"));
        ReplyWorld {
            w,
            a,
            worker,
            secret,
        }
    }

    #[track_caller]
    fn record(&self, path: &Path) -> Out {
        self.w.cli(&os_args![
            "record",
            run_dir(&self.w),
            "result",
            "--dispatch",
            text(&self.a["dispatch_id"]),
            "--worker",
            self.worker,
            "--reply-file",
            path
        ])
    }
}

/// No file under the run directory holds the secret.
macro_rules! assert_no_secret_in_the_run {
    ($w:expr) => {{
        let w: &FakeSupervisor = &$w;
        for f in walk(&run_dir(w)) {
            if f.is_file() {
                let data = fs::read(&f).unwrap_or_else(|e| panic!("reading {}: {e}", f.display()));
                assert!(
                    !String::from_utf8_lossy(&data).contains(SECRET),
                    "{}",
                    f.display()
                );
            }
        }
    }};
}

/// `r` was refused, no result was recorded, and the secret reached no file of the run.
macro_rules! assert_nothing_copied {
    ($w:expr, $r:expr) => {{
        let w: &FakeSupervisor = &$w;
        let r: Out = $r;
        assert_ne!(r.code, 0, "{r}");
        assert!(r.stderr_text().contains("refused"), "{r}");
        assert_empty!(w.events("result_recorded"));
        assert_no_secret_in_the_run!(w);
    }};
}

mod l1_reply_file {
    use super::*;

    #[test]
    fn test_another_path_is_refused() {
        let t = ReplyWorld::new();
        assert_nothing_copied!(t.w, t.record(&t.secret));
    }

    #[test]
    fn test_the_reply_file_as_a_symlink_is_refused() {
        let t = ReplyWorld::new();
        let link = os_path(&t.a["reply_file"]);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        match fs::remove_file(&link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("removing {}: {e}", link.display()),
        }
        symlink(&t.secret, &link);
        assert_nothing_copied!(t.w, t.record(&link));
    }

    #[test]
    fn test_a_symlinked_scratch_folder_is_refused() {
        let t = ReplyWorld::new();
        let reply = os_path(&t.a["reply_file"]);
        let folder = reply.parent().unwrap().to_path_buf();
        let elsewhere = t.w.tmp.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        write_text(
            &elsewhere.join(reply.file_name().unwrap()),
            format!("Status: READY\n{SECRET}\n"),
        );
        for entry in fs::read_dir(&folder).unwrap() {
            let f = entry.unwrap().path();
            let dest = elsewhere.join(f.file_name().unwrap());
            if dest.exists() {
                fs::remove_file(&f).unwrap();
            } else {
                fs::rename(&f, &dest).unwrap();
            }
        }
        fs::remove_dir(&folder).unwrap();
        symlink(&elsewhere, &folder);
        assert_nothing_copied!(t.w, t.record(&reply));
    }

    #[test]
    fn test_a_report_behind_a_symlinked_scratch_folder_is_never_copied() {
        // Codex 4546946519: O_NOFOLLOW guards only the last component; the folder is walked
        // too.
        let t = ReplyWorld::new();
        let folder = os_path(&t.a["reply_file"]).parent().unwrap().to_path_buf();
        let elsewhere = t.w.tmp.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        write_text(
            &elsewhere.join("report.md"),
            format!("Status: READY\n{SECRET}\n"),
        );
        write_text(&elsewhere.join("brief.md"), format!("{SECRET}\n"));
        for entry in fs::read_dir(&folder).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        fs::remove_dir(&folder).unwrap();
        symlink(&elsewhere, &folder);
        t.w.cli(&os_args![
            "record",
            run_dir(&t.w),
            "result",
            "--dispatch",
            text(&t.a["dispatch_id"]),
            "--worker",
            t.worker,
            "--reply",
            "Status: READY\n"
        ])
        .ok();
        let done = one(t.w.events("result_recorded"));
        let skipped = done
            .get("files_skipped")
            .filter(|v| truthy(v))
            .and_then(Json::as_array)
            .unwrap_or_default();
        assert!(
            skipped.iter().any(|note| text(note).contains("report")),
            "{done}"
        );
        assert_no_secret_in_the_run!(t.w);
    }
}

// --------------------------------------------------------------------------
// L2: a symlink planted at a later dispatch's scratch path

/// What the implementer plants where the review's scratch goes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Plant {
    DispatchFolder,
    CardFolder,
    File,
}

/// L2 (Codex review round 1): an implementer plants a symlink at the next dispatch's scratch
/// path; the engine must not write the review brief, nor remove a report, through it. The
/// implementer's result is recorded; gives the world and the folder outside the run.
#[track_caller]
fn plant(which: Plant) -> (FakeSupervisor, PathBuf) {
    let mut w = FakeSupervisor::new(&[card(1)]);
    let outside = w.tmp.join("outside");
    fs::create_dir(&outside).unwrap();
    write_text(&outside.join("report.md"), "keep me\n");
    write_text(&outside.join("reply.txt"), "keep me too\n");
    let target = outside.clone();
    let tamper = move |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
        let scratch = ws.join(".leadhand/1");
        match which {
            Plant::DispatchFolder => symlink(&target, &scratch.join("c1-a1-review-1")),
            Plant::CardFolder => {
                fs::rename(&scratch, ws.join(".leadhand/1-moved")).unwrap();
                symlink(&target, &scratch);
            }
            // A file where the dispatch folder should be.
            Plant::File => write_text(&scratch.join("c1-a1-review-1"), "not a folder\n"),
        }
    };
    w.script(1, "implement", [tampering(tamper)]);
    w.start();
    let a = w.next();
    assert_eq!(a["dispatch_id"], "c1-a1-implement-1");
    let worker = w.launch(&a);
    let reply = w.perform(&a, &worker).expect("the implementer's reply");
    // The reply inline, as the Python world recorded it: the card folder may be a symlink now.
    w.cli(&os_args![
        "record",
        run_dir(&w),
        "result",
        "--dispatch",
        text(&a["dispatch_id"]),
        "--worker",
        worker,
        "--reply",
        reply
    ])
    .ok();
    (w, outside)
}

/// `next` refuses, naming `path_part`, as long as the plant is there; nothing outside the run
/// was written or removed, and no review was requested.
macro_rules! assert_planted_refused {
    ($w:expr, $outside:expr, $path_part:expr) => {{
        let w: &FakeSupervisor = &$w;
        let outside: &Path = &$outside;
        let path_part: &str = $path_part;
        for _ in 0..2 {
            // The run waits: refused again until the operator acts.
            let r = w.cli(&os_args!["next", run_dir(w)]);
            assert_eq!(r.code, 1, "{r}");
            assert!(r.stderr_text().starts_with("error: "), "{r}");
            assert!(r.stderr_text().contains(path_part), "{r}");
        }
        let briefs: Vec<PathBuf> = walk(outside)
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "brief.md"))
            .collect();
        assert_empty!(briefs);
        assert_eq!(read_text(&outside.join("report.md")), "keep me\n");
        assert_eq!(read_text(&outside.join("reply.txt")), "keep me too\n");
        let reviews: Vec<Json> = w
            .events("dispatch_requested")
            .into_iter()
            .filter(|e| e["dispatch_kind"] == "review")
            .collect();
        assert_empty!(reviews);
    }};
}

mod l2_symlinked_scratch {
    use super::*;

    #[test]
    fn test_a_symlinked_dispatch_folder_is_refused() {
        // Covers bug class 5 (files workers can write).
        let (w, outside) = plant(Plant::DispatchFolder);
        assert_planted_refused!(w, outside, ".leadhand/1/c1-a1-review-1");
    }

    #[test]
    fn test_a_symlinked_card_folder_is_refused() {
        let (w, outside) = plant(Plant::CardFolder);
        assert_planted_refused!(w, outside, ".leadhand/1");
    }

    #[test]
    fn test_a_file_in_place_of_the_dispatch_folder_is_refused() {
        let (w, outside) = plant(Plant::File);
        assert_planted_refused!(w, outside, ".leadhand/1/c1-a1-review-1");
    }
}

// --------------------------------------------------------------------------
// L6: neither the CLI nor the plugin directory may live in the workspace

/// L6: workers can edit their workspace, so neither the CLI (which runs outside the sandbox)
/// nor the plugin the supervisor loads may live in it. The CLI's own path is the testing hook
/// LEADHAND_TEST_CLI's.
mod l6_tools_inside_the_workspace {
    use super::*;

    #[test]
    fn test_run_refuses_a_cli_inside_the_workspace() {
        let w = LaunchWorld::new();
        let cli = w.workspace.join("bin/leadhand");
        let r = w.run_with(
            &[],
            RunOpts {
                env: &[("LEADHAND_TEST_CLI", path_str(&cli))],
                ..RunOpts::default()
            },
        );
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().contains("inside the run's workspace"));
        assert_empty!(w.starts());
    }

    #[test]
    fn test_run_refuses_a_plugin_dir_inside_the_workspace() {
        let w = LaunchWorld::new();
        let r = w.run(&os_args!["--plugin-dir", w.workspace.join("plugin")]);
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().contains("inside the run's workspace"));
        assert_empty!(w.starts());
    }

    #[test]
    fn test_run_accepts_a_plugin_dir_outside_the_workspace() {
        let w = LaunchWorld::new();
        let r = w.run(&os_args!["--plugin-dir", w.elsewhere]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(w.starts().len(), 1);
    }
}

// --------------------------------------------------------------------------
// M2: a filter driver in the repository's Git configuration still runs in leadhand's Git calls

mod m2_config_defined_commands {
    use super::*;

    #[test]
    fn test_a_repository_config_filter_driver_runs_in_leadhands_checkout() {
        // Covers limit M2 (config-defined commands): leadhand pins hooks, fsmonitor and
        // git-lfs, but a filter driver in the repository's own Git configuration (other than
        // git-lfs) still runs in its Git calls. Seen from the driver's own log: `start` checks
        // out the run's worktree, and the smudge command runs there.
        let mut w = FakeSupervisor::new(&[card(1)]);
        let log = w.tmp.join("filter log");
        let driver = w.tmp.join("probe filter");
        write_script(
            &driver,
            &format!(
                "#!/bin/sh\necho \"$1 $(pwd)\" >> \"{}\"\nexec cat\n",
                log.display()
            ),
        );
        commit(
            &w.repo,
            &[
                (".gitattributes", "*.dat filter=probe\n"),
                ("data.dat", "payload\n"),
            ],
            "a filtered file",
        );
        w.base = run_git(&w.repo, &["rev-parse", "HEAD"]);
        w.write_deck(&[card(1)]);
        for kind in ["smudge", "clean"] {
            run_git(
                &w.repo,
                &[
                    "config",
                    &format!("filter.probe.{kind}"),
                    &format!("'{}' {kind}", driver.display()),
                ],
            );
        }
        assert!(!log.exists());
        w.start();
        let ws = w.workspace();
        let want = format!("smudge {}", resolve(&ws).display());
        assert!(lines(&read_text(&log)).contains(&want), "{want}");
        assert_eq!(read_text(&ws.join("data.dat")), "payload\n");
    }
}

// --------------------------------------------------------------------------
// I6, I7 (R45): leadhand's Git calls run no hook or fsmonitor, and the runner commits no gitlink

const HOOKS: [&str; 4] = [
    "post-checkout",
    "pre-commit",
    "post-commit",
    "reference-transaction",
];

/// A script at `path` that logs that it ran, and where, to `marker`.
#[track_caller]
fn marker_script(path: &Path, marker: &Path) -> PathBuf {
    write_script(
        path,
        &format!(
            "#!/bin/sh\necho \"ran $0 in $(pwd)\" >> \"{}\"\n",
            marker.display()
        ),
    );
    path.to_path_buf()
}

/// Converted from Python's `test_git_safety`. The operator's repository has hooks that log to a
/// marker file (in its hooks folder, which the run's worktrees share) and an fsmonitor command
/// that logs to it too, both in its own configuration and in a dirty nested repository
/// committed as a gitlink, as in the Python test. A whole run (start, card cuts, a reviewer's
/// violation preserved and restored, the runner's commit of Codex's work, accepts, the
/// end-of-run gates with the evidence base worktree) must leave no mark. Nothing else runs Git
/// in the run: the Codex implementers leave their work for the runner to commit, and the
/// Claude reviewers run no Git. Whether the trap is armed is checked last, inside the same
/// test, with plain Git in the binary's environment.
mod git_safety {
    use super::*;

    #[test]
    fn test_leadhand_git_calls_run_no_fsmonitor_or_hook() {
        // Covers bug class 3 (user Git config) and R45.
        let mut w = codex_world(
            &[card(1), card(2)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        let marker = w.tmp.join("MARKER");
        let evil = marker_script(&w.tmp.join("evil.sh"), &marker);
        let quoted = format!("'{}'", evil.display()); // fsmonitor runs through the shell; the path has a space
        let sub = make_repo_named(&w.repo, "sub", "main");
        run_git(&sub, &["config", "core.fsmonitor", &quoted]);
        run_git(&w.repo, &["add", "sub"]);
        run_git(&w.repo, &["commit", "--quiet", "-m", "add the nested repo"]);
        run_git(&w.repo, &["config", "core.fsmonitor", &quoted]);
        let hooks = PathBuf::from(run_git(
            &w.repo,
            &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
        ));
        fs::create_dir_all(&hooks).unwrap();
        for name in HOOKS {
            marker_script(&hooks.join(name), &marker);
        }
        w.base = run_git(&w.repo, &["rev-parse", "HEAD"]);
        w.write_deck(&[card(1), card(2)]);
        match fs::remove_file(&marker) {
            // unlink(missing_ok=True): only a missing file is ignored
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("removing {}: {e}", marker.display()),
        }
        write(&sub, &[("README.md", "dirty\n")]); // a dirty nested repo is what triggers its fsmonitor

        let leave_a_change = |_world: &FakeSupervisor, ws: &Path, _a: &Json| {
            write(ws, &[("README.md", "a reviewer's edit\n")]);
        };

        w.start();
        w.script(1, "review", [tampering(leave_a_change)]);
        for n in [1, 2] {
            // Codex leaves its work for the runner to commit
            w.script(
                n,
                "implement",
                [Step {
                    commit: Some(false),
                    ..Step::default()
                }],
            );
        }
        assert_eq!(w.run().last().unwrap()["action"], "done");
        assert_json_eq!(field(&w.events("accepted"), "card"), jv("[1, 2]"));
        let found = one(w
            .events("violation")
            .into_iter()
            .filter(|e| e.get("stage").is_some_and(|v| v == "found"))
            .collect());
        assert_json_eq!(found["paths"], jv(r#"["README.md"]"#));
        assert_eq!(
            run_git(
                &w.repo,
                &["show", &format!("{}:README.md", text(&found["ref"]))]
            ),
            "a reviewer's edit"
        );
        let second = w.events("accepted")[1]["sha"].clone();
        assert_eq!(
            run_git(
                &w.repo,
                &[
                    "log",
                    "-1",
                    "--format=%(trailers:key=Leadhand-Dispatch,valueonly)",
                    text(&second)
                ]
            ),
            "c2-a1-implement-1"
        );
        assert!(gate_names(&w).iter().any(|g| g == "evidence"));
        let marks = if marker.exists() {
            read_text(&marker)
        } else {
            String::new()
        };
        assert!(!marker.exists(), "{marks}");
        // Control: plain Git in the binary's environment runs both.
        let env = env_without(&w.env, |k| k.starts_with("LEADHAND"));
        for args in [
            &["status", "--porcelain"][..],
            &["update-ref", "refs/control", "HEAD"][..],
        ] {
            let mut argv = vec![OsStr::new("git")];
            argv.extend(args.iter().map(OsStr::new));
            run_argv(&argv, &env, &w.repo, None, Duration::from_secs(60));
        }
        let ran = if marker.exists() {
            read_text(&marker)
        } else {
            String::new()
        };
        assert!(ran.contains("evil.sh"), "{ran}");
        assert!(ran.contains("reference-transaction"), "{ran}");
    }

    #[test]
    fn test_runner_never_commits_a_gitlink() {
        // The Codex worker moves a tracked nested repository, makes a new one and edits a
        // file; each nested repository is a folder whose .git file points at a repository
        // outside the workspace, as a submodule's does. The runner commits the file, and
        // neither gitlink.
        let codex_card = || {
            card_with(
                1,
                CardOpts {
                    route: Some(CODEX_CARD),
                    ..CardOpts::default()
                },
            )
        };
        let mut w = codex_world(&[codex_card()], Options::default());
        let tracked = make_repo_named(&w.tmp, "tracked nested", "main");
        let old = run_git(&tracked, &["rev-parse", "HEAD"]);
        run_git(
            &w.repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{old},src/tracked sub"),
            ],
        );
        run_git(&w.repo, &["commit", "--quiet", "-m", "a submodule pointer"]);
        w.base = run_git(&w.repo, &["rev-parse", "HEAD"]);
        w.write_deck(&[codex_card()]);
        commit(&tracked, &[("README.md", "moved\n")], "moved");
        let fresh = make_repo_named(&w.tmp, "new nested", "main");
        w.start();
        let tracked_git = format!(
            "gitdir: {}\n",
            run_git(&tracked, &["rev-parse", "--absolute-git-dir"])
        );
        let fresh_git = format!(
            "gitdir: {}\n",
            run_git(&fresh, &["rev-parse", "--absolute-git-dir"])
        );
        w.script(
            1,
            "implement",
            [Step {
                commit: Some(false),
                ..files(&[
                    ("src/tracked sub/.git", &tracked_git),
                    ("src/tracked sub/README.md", "moved\n"),
                    ("src/new sub/.git", &fresh_git),
                    ("src/new sub/README.md", "hello\n"),
                    ("src/widget.py", "W = 1\n"),
                ])
            }],
        );
        assert_eq!(w.run().last().unwrap()["action"], "done");
        let accepted = one(w.events("accepted"));
        let tip = text(&accepted["sha"]).to_string();
        let tree = run_git(&w.repo, &["ls-tree", "-r", &tip]);
        assert!(!tree.contains("new sub"), "{tree}");
        assert!(tree.contains("src/widget.py"), "{tree}");
        assert_eq!(
            run_git(&w.repo, &["rev-parse", &format!("{tip}:src/tracked sub")]),
            old
        );
        let result = read_json(&run_dir(&w).join("workers/c1-a1-implement-1/result.json"));
        assert_eq!(result["committed"], tip.as_str());
        assert_json_eq!(
            result
                .get("gitlinks_skipped")
                .cloned()
                .unwrap_or(Json::Null),
            jv(r#"["src/tracked sub"]"#)
        );
    }
}
