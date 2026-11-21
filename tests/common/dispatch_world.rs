//! A run `start` made, and the dispatch commands on it: the Rust form of
//! `contract_dispatch.World` (contract_dispatch.py:104-241) and its module helpers
//! (`wait_for`, `alive`, `logged_alive`).
//!
//! Every scenario runs on a run that `start` made (one card, Claude and Codex installed, so
//! the workspace is a worktree and config.json is the run's snapshot), with the Rust fake
//! Codex as Codex (LEADHAND_TEST_CODEX_BIN). Python's keyword arguments to `args`,
//! `dispatch`, `popen` and `start_detached` are a `Req` with `Default`; each has a plain form
//! (the defaults) and a `_with` form.
//!
//! Cleanup, as Python's (LIFO): dropping the world first resumes every process a test
//! stopped (`resume_on_drop`) and kills its decoys, then the fake supervisor kills its
//! proxies and every process the worker folders and the fake's log name, and removes the
//! temp root.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::{Pid, Signal, kill_process};

use super::json::{self, Json};
use super::rundir::read_records;
use super::supervisor::{FakeSupervisor, Options, Proc, card, recorded_alive};
use super::{Env, Out, TIMEOUT, bin, cli_with, read_json, read_jsonl, set_mtime, started_by};

/// The fake's thread ID when the scenario names none.
pub const DEFAULT_THREAD: &str = "0f0f0f0f-0000-4000-8000-00000000c0de";

/// The scenario fixtures, `tests/fixtures/scenarios/<name>.jsonl`.
pub fn scenario_fixture(name: &str) -> PathBuf {
    super::plugin::root().join(format!("tests/fixtures/scenarios/{name}.jsonl"))
}

/// Python's `wait_for`'s default bound.
pub const WAIT: Duration = Duration::from_secs(30);

/// Polls `cond` every 20 ms until it gives Some, and returns that (`wait_for(cond, what)`).
/// Panics after `WAIT`.
#[track_caller]
pub fn wait_for<T>(what: &str, cond: impl FnMut() -> Option<T>) -> T {
    wait_for_with(what, WAIT, None, cond)
}

/// `wait_for` with its own bound, and a command the test is waiting on: it panics at once,
/// with the command's outputs, when that command has exited (`wait_for(cond, what, timeout,
/// proc)`).
#[track_caller]
pub fn wait_for_with<T>(
    what: &str,
    timeout: Duration,
    proc: Option<&Proc>,
    mut cond: impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = cond() {
            return value;
        }
        if let Some(p) = proc
            && let Some(code) = p.poll()
        {
            let (out, err) = match p.communicate(Duration::from_secs(10)) {
                Some((out, err)) => (
                    String::from_utf8_lossy(&out).into_owned(),
                    String::from_utf8_lossy(&err).into_owned(),
                ),
                None => (
                    "(not read: a descendant held the pipes)".to_string(),
                    String::new(),
                ),
            };
            panic!(
                "the command exited ({code}) while waiting for {what}\nstdout:\n{out}\nstderr:\n{err}"
            );
        }
        if Instant::now() > deadline {
            panic!(
                "timed out after {} s waiting for {what}",
                timeout.as_secs_f64()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// `Some(())` when `b` (for `wait_for` on a condition with no value).
pub fn when(b: bool) -> Option<()> {
    b.then_some(())
}

/// Whether the process a `{pid, start}` record names is still that process (`alive`).
pub fn alive(rec: &Json) -> bool {
    recorded_alive(rec)
}

/// Whether the process a fake Codex log record names (`child_pid` or `escaped_pid`) is still
/// alive: judged from the log line's own time, never from anything the binary wrote
/// (`logged_alive`).
#[track_caller]
pub fn logged_alive(rec: &Json) -> bool {
    let pid = ["child_pid", "escaped_pid"]
        .iter()
        .find_map(|k| rec.get(k).and_then(Json::as_i64).filter(|p| *p != 0))
        .unwrap_or_else(|| panic!("a log record naming no process: {rec}"));
    let when = rec["time"]
        .as_f64()
        .unwrap_or_else(|| panic!("a log record with no time: {rec}"));
    started_by(pid as i32, when).is_some()
}

/// The member at `path` of a JSON object, for editing; a missing member is added as null on
/// the way (so `*at(c, &["codex", "network"]) = true.into()` is Python's
/// `c["codex"]["network"] = True`, and `update` one key at a time).
#[track_caller]
pub fn at<'j>(v: &'j mut Json, path: &[&str]) -> &'j mut Json {
    let mut v = v;
    for key in path {
        let Json::Obj(members) = v else {
            panic!("not an object at {key:?}: {v}")
        };
        let i = match members.iter().rposition(|(k, _)| k == key.as_bytes()) {
            Some(i) => i,
            None => {
                members.push((key.as_bytes().to_vec(), Json::Null));
                members.len() - 1
            }
        };
        v = &mut members[i].1;
    }
    v
}

/// Rewrites the JSON file `path` (indent 2, then a newline, as `json.dumps(data, indent=2)`)
/// after `change` edits it.
#[track_caller]
pub fn edit_json(path: &Path, change: impl FnOnce(&mut Json)) {
    let mut data = read_json(path);
    change(&mut data);
    fs::write(path, json::dumps_indent(&data, 2) + "\n")
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}

/// The request flags of `World::args`; `Req::default()` is Python's defaults.
#[derive(Clone, Copy, Debug)]
pub struct Req<'a> {
    /// None: a run-level dispatch (no `--card`).
    pub card: Option<i64>,
    pub attempt: i64,
    pub role: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub resume: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub branch: Option<&'a str>,
}

impl Default for Req<'_> {
    fn default() -> Self {
        Req {
            card: Some(1),
            attempt: 1,
            role: "implementer",
            model: "gpt-6-luna",
            effort: "low",
            resume: None,
            kind: None,
            branch: None,
        }
    }
}

/// The dispatch's brief and report in its scratch folder, <ws>/.leadhand/<card or run>/<id>/;
/// writes the brief (`brief_text`, by default "# Brief") unless the test already has
/// (`World.scratch`, for callers holding only the workspace).
#[track_caller]
pub fn scratch_in(
    ws: &Path,
    did: &str,
    card: Option<i64>,
    brief_text: Option<&str>,
) -> (PathBuf, PathBuf) {
    let folder = ws
        .join(".leadhand")
        .join(card.map_or("run".to_string(), |n| n.to_string()))
        .join(did);
    fs::create_dir_all(&folder).unwrap_or_else(|e| panic!("making {}: {e}", folder.display()));
    let brief = folder.join("brief.md");
    if brief_text.is_some() || !brief.exists() {
        fs::write(
            &brief,
            brief_text.filter(|t| !t.is_empty()).unwrap_or("# Brief\n"),
        )
        .unwrap_or_else(|e| panic!("writing {}: {e}", brief.display()));
    }
    (brief, folder.join("report.md"))
}

/// The `dispatch` arguments for `did` on the run `run_dir` with workspace `ws`
/// (`World.args`, for callers holding only the paths, such as a thread).
#[track_caller]
pub fn args_in(run_dir: &Path, ws: &Path, did: &str, req: &Req) -> Vec<OsString> {
    let (brief, report) = scratch_in(ws, did, req.card, None);
    let mut argv: Vec<OsString> = vec![
        "dispatch".into(),
        "--run".into(),
        run_dir.into(),
        "--dispatch-id".into(),
        did.into(),
    ];
    if let Some(n) = req.card {
        argv.extend(["--card".into(), n.to_string().into()]);
    }
    argv.extend([
        "--attempt".into(),
        req.attempt.to_string().into(),
        "--role".into(),
        req.role.into(),
        "--model".into(),
        req.model.into(),
        "--effort".into(),
        req.effort.into(),
        "--brief".into(),
        brief.into(),
        "--report".into(),
        report.into(),
        "--cwd".into(),
        ws.into(),
    ]);
    for (flag, value) in [
        ("--resume", req.resume),
        ("--kind", req.kind),
        ("--branch", req.branch),
    ] {
        if let Some(v) = value {
            argv.extend([flag.into(), v.into()]);
        }
    }
    argv
}

/// Borrowed `&OsStr`s of `args`, for `cli`.
pub fn os_refs(args: &[OsString]) -> Vec<&OsStr> {
    args.iter().map(OsString::as_os_str).collect()
}

/// A run `start` made, and the dispatch commands on it.
///
/// `env` is every command's environment: the supervisor's sealed one, the fake Codex, and
/// credentials Codex must never see. `scenario` sets the fake's script and knobs for the
/// commands that follow, as the Python tests' `scenario` set os.environ.
pub struct World {
    pub sup: FakeSupervisor,
    pub run_dir: PathBuf,
    pub run_id: String,
    pub ws: PathBuf,
    pub repo: PathBuf,
    pub tmp: PathBuf,
    pub log: PathBuf,
    pub env: Env,
    scenarios: u32,
    decoys: Vec<Proc>,
    stopped: Vec<Json>,
}

impl World {
    #[track_caller]
    pub fn new() -> World {
        let mut sup = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                installed: &["claude", "codex"],
                ..Options::default()
            },
        );
        let run_dir = sup.start();
        let meta = sup.meta();
        let run_id = meta["run_id"].as_str().unwrap().to_string();
        let ws = PathBuf::from(meta["workspace"].as_os().unwrap());
        let (repo, tmp) = (sup.repo.clone(), sup.tmp.clone());
        let log = PathBuf::from(sup.env.get("FAKE_CODEX_LOG").unwrap());
        let mut env = sup.env.clone();
        env.set("LEADHAND_TEST_CODEX_BIN", sup.env.fakes().join("codex")) // R49: Codex from the test env
            .set("GH_TOKEN", "parent-gh-token")
            .set("GITHUB_TOKEN", "parent-github-token")
            .set("SSH_AUTH_SOCK", "/nonexistent/agent.sock");
        World {
            sup,
            run_dir,
            run_id,
            ws,
            repo,
            tmp,
            log,
            env,
            scenarios: 0,
            decoys: Vec::new(),
            stopped: Vec::new(),
        }
    }

    // -- setup ----------------------------------------------------------------

    fn clear_knobs(&mut self) {
        let stale: Vec<OsString> = self
            .env
            .vars()
            .map(|(k, _)| k.to_os_string())
            .filter(|k| {
                let k = k.to_string_lossy();
                k.starts_with("FAKE_CODEX_") && k != "FAKE_CODEX_LOG"
            })
            .collect();
        for k in stale {
            self.env.remove(k);
        }
    }

    fn set_knobs(&mut self, knobs: &[(&str, &str)]) {
        for (k, v) in knobs {
            self.env.set(format!("FAKE_CODEX_{}", k.to_uppercase()), v);
        }
    }

    /// The fake's scenario, a name in tests/fixtures/scenarios, and its FAKE_CODEX_* knobs
    /// (thread, delay, no_rollout, orphan_child, ...).
    pub fn scenario(&mut self, name: &str, knobs: &[(&str, &str)]) {
        self.clear_knobs();
        self.env.set("FAKE_CODEX_SCENARIO", scenario_fixture(name));
        self.set_knobs(knobs);
    }

    /// The fake's scenario as a list of steps, written to `<tmp>/scenario <n>.jsonl`, and its
    /// knobs.
    #[track_caller]
    pub fn scenario_steps(&mut self, steps: &[serde_json::Value], knobs: &[(&str, &str)]) {
        self.clear_knobs();
        self.scenarios += 1;
        let path = self.tmp.join(format!("scenario {}.jsonl", self.scenarios));
        let text: String = steps.iter().map(|s| format!("{s}\n")).collect();
        fs::write(&path, text).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        self.env.set("FAKE_CODEX_SCENARIO", path);
        self.set_knobs(knobs);
    }

    /// Edits the run's configuration snapshot (config.json), which `dispatch` reads.
    #[track_caller]
    pub fn set_config(&self, change: impl FnOnce(&mut Json)) {
        edit_json(&self.run_dir.join("config.json"), change);
    }

    /// Sets the role's limits in config.json (`c["limits"][role].update(limits)`).
    #[track_caller]
    pub fn set_limits(&self, role: &str, limits: &[(&str, Json)]) {
        self.set_config(|c| {
            for (k, v) in limits {
                *at(c, &["limits", role, k]) = v.clone();
            }
        });
    }

    /// `scratch_with(did, Some(1), None)`.
    #[track_caller]
    pub fn scratch(&self, did: &str) -> (PathBuf, PathBuf) {
        self.scratch_with(did, Some(1), None)
    }

    /// The dispatch's brief and report in its scratch folder (see `scratch_in`).
    #[track_caller]
    pub fn scratch_with(
        &self,
        did: &str,
        card: Option<i64>,
        brief_text: Option<&str>,
    ) -> (PathBuf, PathBuf) {
        scratch_in(&self.ws, did, card, brief_text)
    }

    #[track_caller]
    pub fn args(&self, did: &str) -> Vec<OsString> {
        self.args_with(did, &Req::default())
    }

    #[track_caller]
    pub fn args_with(&self, did: &str, req: &Req) -> Vec<OsString> {
        args_in(&self.run_dir, &self.ws, did, req)
    }

    /// request.json as `dispatch` writes it, for a worker folder a test sets up by hand.
    #[track_caller]
    pub fn request_json(&self, did: &str) -> Json {
        let (brief, report) = self.scratch(did);
        let path = |p: &Path| Json::from(p.to_str().unwrap());
        Json::object([
            ("model", "gpt-6-luna".into()),
            ("effort", "low".into()),
            ("role", "implementer".into()),
            ("brief", path(&brief)),
            ("report", path(&report)),
            ("cwd", path(&self.ws)),
            ("run_dir", path(&self.run_dir)),
            ("card", 1.into()),
            ("attempt", 1.into()),
            ("dispatch_id", did.into()),
            ("steps", 50.into()),
            ("minutes", 5.into()),
            ("network", false.into()),
            ("writable_dirs", Json::Arr(Vec::new())),
            ("resume", Json::Null),
            ("kind", Json::Null),
            ("branch", Json::Null),
        ])
    }

    /// An unrelated process of the test's own, which nothing may signal: `sleep 60` in a
    /// process group of its own. Killed when the world drops.
    #[track_caller]
    pub fn decoy(&mut self) -> Proc {
        let child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting the decoy");
        let p = Proc::new(child);
        self.decoys.push(p.clone());
        p
    }

    /// Sends SIGCONT to `rec` when the world drops (Python's `addCleanup(w.signal, rec,
    /// SIGCONT)` after a SIGSTOP).
    pub fn resume_on_drop(&mut self, rec: &Json) {
        self.stopped.push(rec.clone());
    }

    // -- commands -------------------------------------------------------------

    #[track_caller]
    pub fn dispatch(&self, did: &str) -> Out {
        self.dispatch_with(did, &Req::default())
    }

    #[track_caller]
    pub fn dispatch_with(&self, did: &str, req: &Req) -> Out {
        self.dispatch_timeout(did, req, TIMEOUT)
    }

    #[track_caller]
    pub fn dispatch_timeout(&self, did: &str, req: &Req, timeout: Duration) -> Out {
        let args = self.args_with(did, req);
        cli_with(&os_refs(&args), &self.env, &self.tmp, None, timeout)
    }

    #[track_caller]
    pub fn popen(&mut self, did: &str) -> Proc {
        self.popen_with(did, &Req::default())
    }

    /// The `dispatch` line in the background, in its own process group; cleanup kills it.
    #[track_caller]
    pub fn popen_with(&mut self, did: &str, req: &Req) -> Proc {
        let args = self.args_with(did, req);
        let child = Command::new(bin())
            .args(&args)
            .current_dir(&self.tmp)
            .env_clear()
            .envs(self.env.vars())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap_or_else(|e| panic!("starting {}: {e}", bin().display()));
        let p = Proc::new(child);
        self.sup.proxies.push(p.clone());
        p
    }

    #[track_caller]
    pub fn start_detached(&mut self, did: &str) -> Json {
        self.start_detached_with(did, &Req::default())
    }

    /// Starts the dispatch, waits until its runner has started Codex, then kills the
    /// foreground. Returns runner.json's Codex record.
    #[track_caller]
    pub fn start_detached_with(&mut self, did: &str, req: &Req) -> Json {
        let p = self.popen_with(did, req);
        let codex = wait_for_with(&format!("Codex started for {did}"), WAIT, Some(&p), || {
            self.codex_of(did)
        });
        p.kill();
        // Python's communicate(timeout=10) raises TimeoutExpired here, failing the test, when
        // something (the detached runner) still holds the dispatch line's pipes.
        p.communicate(Duration::from_secs(10)).unwrap_or_else(|| {
            panic!("the foreground's pipes were still held 10 s after it was killed ({did})")
        });
        codex
    }

    /// runner.json's Codex record when it has one (`(runner_json() or {}).get("codex")`,
    /// when truthy).
    pub fn codex_of(&self, did: &str) -> Option<Json> {
        self.runner_json(did)
            .and_then(|r| r.get("codex").cloned())
            .filter(|c| !c.is_null() && c.as_object().is_none_or(|m| !m.is_empty()))
    }

    #[track_caller]
    pub fn stop(&self, did: &str) -> Out {
        let args = [
            OsStr::new("stop-dispatch"),
            self.run_dir.as_os_str(),
            OsStr::new("--dispatch"),
            OsStr::new(did),
        ];
        cli_with(&args, &self.env, &self.tmp, None, TIMEOUT)
    }

    /// Signals a recorded process, only while it is still the process recorded.
    pub fn signal(rec: &Json, sig: Signal) {
        if alive(rec)
            && let Some(pid) = rec["pid"]
                .as_i64()
                .and_then(|p| i32::try_from(p).ok())
                .and_then(Pid::from_raw)
        {
            let _ = kill_process(pid, sig);
        }
    }

    // -- reading --------------------------------------------------------------

    pub fn wd(&self, did: &str) -> PathBuf {
        self.run_dir.join("workers").join(did)
    }

    /// The worker folder's JSON file `name`, when it exists.
    #[track_caller]
    pub fn read(&self, did: &str, name: &str) -> Option<Json> {
        let path = self.wd(did).join(name);
        path.exists().then(|| read_json(&path))
    }

    #[track_caller]
    pub fn runner_json(&self, did: &str) -> Option<Json> {
        self.read(did, "runner.json")
    }

    #[track_caller]
    pub fn result(&self, did: &str) -> Json {
        read_json(&self.wd(did).join("result.json"))
    }

    /// result.json's `stopped` processes, by pid.
    #[track_caller]
    pub fn stopped(&self, did: &str) -> BTreeMap<i64, Json> {
        self.result(did)["stopped"]
            .as_array()
            .unwrap_or_else(|| panic!("result.json's stopped is not a list"))
            .iter()
            .map(|r| (r["pid"].as_i64().unwrap(), r.clone()))
            .collect()
    }

    /// The fake Codex's log records, or only those of `event`.
    #[track_caller]
    pub fn records<'e>(&self, event: impl Into<Option<&'e str>>) -> Vec<Json> {
        let event = event.into();
        let records = if self.log.exists() {
            read_jsonl(&self.log)
        } else {
            Vec::new()
        };
        records
            .into_iter()
            .filter(|r| event.is_none_or(|e| r.get("event").is_some_and(|v| v == e)))
            .collect()
    }

    #[track_caller]
    pub fn attempts(&self) -> Vec<Json> {
        read_records(&self.run_dir.join("attempts.jsonl"))
    }

    #[track_caller]
    pub fn charges(&self) -> Vec<Json> {
        read_records(&self.run_dir.join("charges.jsonl"))
    }

    /// Dates the worker's heartbeat 120 s back.
    #[track_caller]
    pub fn age_heartbeat(&self, did: &str) {
        let old = SystemTime::now() - Duration::from_secs(120);
        set_mtime(&self.wd(did).join("heartbeat"), old);
    }

    /// `git <args>` in the workspace.
    #[track_caller]
    pub fn git(&self, args: &[&str]) -> String {
        super::git::run_git(&self.ws, args)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        for rec in &self.stopped {
            World::signal(rec, Signal::CONT);
        }
        for p in &self.decoys {
            p.kill();
            p.wait();
        }
    }
}
