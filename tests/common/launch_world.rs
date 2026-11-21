//! A run `start` made, and the launches on it: the Rust form of `contract_launch.World` and
//! its module helpers (`wait_for`, `snapshot`, `lockable`, `path_without_self`), shared by
//! `tests/launch.rs` and `tests/rust_launch.rs`.
//!
//! Every scenario runs on a run that `start` made (one card, a worktree workspace, the
//! supervisor route `sonnet low` in the run's config.json), with the Rust fake `claude` as the
//! `claude` found on PATH (a link in the world's `fake bin` directory, where the Python puts
//! its fake). The keep-awake tools on that PATH are shell scripts that do nothing, so no real
//! `caffeinate` or `systemd-inhibit` runs; a test that watches one replaces it with
//! `LOGGING_TOOL`.
//!
//! Whether a supervisor or another process is alive is judged from the fake's own log (pid and
//! the time of the line) against the process table (`started_by`, `same_process`), never from
//! holder.json alone. Launchers that must outlive a step run in the background in their own
//! process group, their output in files; dropping the world kills them, then every process a
//! fake's log names that is still the process it named, then (the fake supervisor) the temp
//! root.
//!
//! **Inherited descriptors** (Python's `pass_fds`). std opens every file close-on-exec, and a
//! test must not clear the flag: another test's child, started meanwhile on another thread,
//! would inherit the descriptor (and with it a run.lock flock). So the file reaches the child
//! as one of its standard streams, which std dups onto 0, 1 or 2 in the child alone, and
//! `/bin/sh` moves it to `PASSED_FD` before it `exec`s the command, which keeps its pid. The
//! dup shares the open file description, so a flock the test took on it is the child's too.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, kill_process};

use super::dispatch_world::{at, wait_for_with};
use super::json::{self, Json};
use super::rundir::{RunPaths, read_records, run_paths};
use super::supervisor::{FakeSupervisor, Options, Proc, card};
use super::{
    Env, Out, cli_with, hold_lock, names_bin, read_json, read_jsonl, same_process, started_by,
};

/// How long a launch test waits for anything (`contract_launch.WAIT`).
pub const WAIT: Duration = Duration::from_secs(15);
pub const SUPERVISOR_ROUTE: &str = "[roles.supervisor]\nclaude = \"sonnet low\"\n";
pub const AGENT: &str = "leadhand:supervisor";
pub const FIRST_MESSAGE_END: &str =
    ". First invoke the Skill tool with leadhand:run, then follow it.";
pub const SPACES_WARNING_TAIL: &str =
    "the sandbox exclusion may not match; install leadhand under a path without spaces";

/// A keep-awake tool that does nothing: the default on the world's PATH.
pub const NOOP_TOOL: &str = "#!/bin/sh\nexit 0\n";

/// Shell for a tool's JSON log line: `q ARG` prints ARG as a JSON string (the tools' words are
/// paths and flags: only `\` and `"` need escaping), `now` the wall-clock time, and `argv` is
/// the members of the tool's argv as a JSON list. `date` has no sub-second precision on
/// macOS, so `now` is the next whole second: `started_by` (a start no later than the line's
/// time) still accepts the process that wrote the line, and a later one reusing its pid only
/// within that second.
macro_rules! sh_json {
    () => {
        r#"q() { printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"; }
now() { echo $(( $(date +%s) + 1 )); }
argv=$(q "$0"); for a in "$@"; do argv="$argv, $(q "$a")"; done
"#
    };
}

/// Shell that sets FDS to the members of a JSON list of this shell's open descriptors, each
/// `{"fd": n, "dev": d, "ino": i}` (Python's `open_fds`, without Python). `ls /dev/fd` in a
/// child lists them (every descriptor the shell inherited reaches it, plus `ls`'s own, which
/// the next step drops), and `stat` reads each through its stdin (an `fstat`: a `stat` of
/// `/dev/fd/N` on macOS gives devfs's device). The shell's 0, 1 and 2 are first copied to
/// three free descriptors, so the redirections around `stat` do not hide them. GNU `stat`
/// (Linux's, or one first on a Mac's PATH) and BSD `stat` take different flags.
macro_rules! sh_open_fds {
    () => {
        r#"open_fds() {
    fds_list=$(ls /dev/fd)
    fds_free=""
    for fds_n in 3 4 5 6 7 8 9; do
        case " $(echo $fds_list) " in *" $fds_n "*) ;; *) fds_free="$fds_free $fds_n" ;; esac
    done
    set -- $fds_free
    fds_a=$1 fds_b=$2 fds_c=$3
    eval "exec $fds_a<&0 $fds_b<&1 $fds_c<&2"
    if stat -L -c %i - </dev/null >/dev/null 2>&1; then fds_gnu=1; else fds_gnu=; fi
    FDS=""
    for fds_n in $fds_list; do
        case "$fds_n" in 0) fds_src=$fds_a ;; 1) fds_src=$fds_b ;; 2) fds_src=$fds_c ;; *) fds_src=$fds_n ;; esac
        if [ -n "$fds_gnu" ]; then
            fds_id=$(eval "stat -L -c '%d %i' - <&$fds_src" 2>/dev/null)
        else
            fds_id=$(eval "stat -f '%d %i' <&$fds_src" 2>/dev/null)
        fi
        [ -n "$fds_id" ] || continue
        set -- $fds_id
        FDS="$FDS${FDS:+, }{\"fd\": $fds_n, \"dev\": $1, \"ino\": $2}"
    done
    eval "exec $fds_a<&- $fds_b<&- $fds_c<&-"
}
"#
    };
}

/// A keep-awake tool that logs how it was started and runs until SIGTERM (Python's
/// LOGGING_TOOL, as a shell script: the suite runs no Python).
pub const LOGGING_TOOL: &str = concat!(
    "#!/bin/sh\n",
    "trap 'kill $! 2>/dev/null; exit 0' TERM\n",
    sh_json!(),
    r#"printf '{"time": %s, "pid": %s, "ppid": %s, "argv": [%s]}\n' "$(now)" "$$" "$PPID" "$argv" >> "$FAKE_KEEP_AWAKE_LOG"
"#,
    "sleep 60 &\nwait\n",
);

/// A fake `claude` that logs its descriptors, its LEADHAND_LOCK_FD and its pid, then waits
/// (at most 15 s) for the keep-awake tool's line, so the tool runs while the supervisor does
/// (`contract_rust_launch.FD_CLAUDE`).
pub const FD_CLAUDE: &str = concat!(
    "#!/bin/sh\n",
    sh_json!(),
    sh_open_fds!(),
    r#"open_fds
if [ -n "${LEADHAND_LOCK_FD+set}" ]; then lock_fd=$(q "$LEADHAND_LOCK_FD"); else lock_fd=null; fi
printf '{"time": %s, "pid": %s, "ppid": %s, "argv": [%s], "lock_fd": %s, "fds": [%s]}\n' "$(now)" "$$" "$PPID" "$argv" "$lock_fd" "$FDS" >> "$FAKE_CLAUDE_LOG"
i=0
while [ ! -e "$FAKE_KEEP_AWAKE_LOG" ] && [ "$i" -lt 750 ]; do sleep 0.02; i=$((i + 1)); done
"#,
);

/// A keep-awake tool that logs its descriptors, then runs until SIGTERM
/// (`contract_rust_launch.FD_TOOL`).
pub const FD_TOOL: &str = concat!(
    "#!/bin/sh\n",
    "trap 'kill $! 2>/dev/null; exit 0' TERM\n",
    sh_json!(),
    sh_open_fds!(),
    r#"open_fds
printf '{"time": %s, "pid": %s, "argv": [%s], "fds": [%s]}\n' "$(now)" "$$" "$argv" "$FDS" > "$FAKE_KEEP_AWAKE_LOG.tmp"
mv "$FAKE_KEEP_AWAKE_LOG.tmp" "$FAKE_KEEP_AWAKE_LOG"
"#,
    "sleep 60 &\nwait\n",
);

/// The descriptor a passed file has in the child (Python's `pass_fds` kept the parent's
/// number; the number itself is never what a test checks).
pub const PASSED_FD: i32 = 9;

/// Polls `cond` until it gives Some, and returns that; panics after `WAIT`, or at once when
/// `proc` (a command the test is waiting on) has exited (`contract_launch.wait_for`).
#[track_caller]
pub fn wait_for<T>(what: &str, proc: Option<&Proc>, cond: impl FnMut() -> Option<T>) -> T {
    wait_for_with(what, WAIT, proc, cond)
}

/// The wall-clock time in seconds since the epoch (`time.time()`).
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// One path's mtime (ns), size and content (None for a directory).
pub type Entry = (i128, u64, Option<Vec<u8>>);

/// Every path under `root` with its mtime, size and (for a file) content (`snapshot`).
#[track_caller]
pub fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Entry> {
    let mut out = std::collections::BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("listing {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            let st = fs::symlink_metadata(&path)
                .unwrap_or_else(|e| panic!("lstat {}: {e}", path.display()));
            let mtime = i128::from(st.mtime()) * 1_000_000_000 + i128::from(st.mtime_nsec());
            let content = if path.is_file() {
                Some(fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display())))
            } else {
                None
            };
            if st.is_dir() {
                dirs.push(path.clone());
            }
            out.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                (mtime, st.len(), content),
            );
        }
    }
    out
}

/// PATH without the directories whose `leadhand` is the binary under test.
pub fn path_without_self(path: &OsStr) -> OsString {
    let kept: Vec<&[u8]> = path
        .as_bytes()
        .split(|&b| b == b':')
        .filter(|d| {
            let exe = Path::new(OsStr::from_bytes(d)).join("leadhand");
            !(!d.is_empty() && exe.is_file() && names_bin(&exe))
        })
        .collect();
    OsString::from_vec(kept.join(&b':'))
}

/// Writes all of `data` to `fd` in non-blocking chunks of 64 KiB, polled as `wait_for` polls
/// (failing at once if `proc` exits, or after `WAIT` if a reader stays alive but stops
/// reading), instead of blocking the test. `fd` is left blocking again (`write_big`).
#[track_caller]
pub fn write_big(fd: &mut (impl Write + AsFd), data: &[u8], proc: &Proc, what: &str) {
    let flags = fcntl_getfl(fd.as_fd()).expect("reading the descriptor's flags");
    fcntl_setfl(fd.as_fd(), flags | OFlags::NONBLOCK).expect("making the descriptor non-blocking");
    let mut rest = data;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(what, Some(proc), || {
            match fd.write(&rest[..rest.len().min(1 << 16)]) {
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("writing to the command: {e}"),
            }
            rest.is_empty().then_some(())
        })
    }));
    fcntl_setfl(fd.as_fd(), flags).expect("making the descriptor blocking again");
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// `argv` started with `file` inherited at `PASSED_FD`, carried on stdin, whose place
/// /dev/null then takes (see the module docs).
pub fn passing_on_stdin(argv: &[OsString], file: File) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("exec {PASSED_FD}<&0 0</dev/null; exec \"$@\""))
        .arg("sh")
        .args(argv)
        .stdin(Stdio::from(file));
    cmd
}

/// `argv` run to its end with `file` inherited at `PASSED_FD`, stdin /dev/null, both outputs
/// collected (`subprocess.run(argv, pass_fds=(fd,), stdin=DEVNULL, capture_output=True,
/// timeout=...)`); panics, as TimeoutExpired fails the test, when it runs past `timeout`.
#[track_caller]
pub fn run_passing(argv: &[OsString], file: File, env: &Env, cwd: &Path, timeout: Duration) -> Out {
    let mut cmd = passing_on_stdin(argv, file);
    let child = cmd
        .current_dir(cwd)
        .env_clear()
        .envs(env.vars())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap_or_else(|e| panic!("could not start {argv:?}: {e}"));
    drop(cmd); // the test's copy of the passed file
    let p = Proc::new(child);
    match p.communicate(timeout) {
        Some((stdout, stderr)) => Out {
            argv: argv.to_vec(),
            code: p.returncode().unwrap(),
            stdout,
            stderr,
        },
        None => {
            if let Some(group) = Pid::from_raw(p.pid()) {
                let _ = rustix::process::kill_process_group(group, Signal::KILL);
            }
            p.wait();
            panic!("{argv:?} timed out after {} s", timeout.as_secs_f64());
        }
    }
}

/// A background command (Python's `Popen` with its output in files).
pub struct Bg {
    pub proc: Proc,
    pub argv: Vec<OsString>,
    pub stdin: Option<ChildStdin>,
    out: PathBuf,
    err: PathBuf,
}

impl Bg {
    pub fn pid(&self) -> i32 {
        self.proc.pid()
    }

    /// Waits for it to exit, at most `timeout` (`p.wait(timeout=...)`, whose TimeoutExpired
    /// fails the test); its exit code.
    #[track_caller]
    pub fn wait(&self, timeout: Duration) -> i32 {
        if self.proc.communicate(timeout).is_none() {
            panic!(
                "{:?} did not exit within {} s",
                self.argv,
                timeout.as_secs_f64()
            );
        }
        self.proc.returncode().unwrap()
    }
}

/// A fake supervisor's log line that started one: the record, its pid, and its start time as
/// the process table had it when the line was written (None once it is gone).
#[derive(Clone, Debug)]
pub struct Start {
    pub rec: Json,
    pub pid: i32,
    pub start: Option<String>,
}

impl Start {
    /// `argv`, as strings.
    #[track_caller]
    pub fn argv(&self) -> Vec<String> {
        strings(&self.rec["argv"])
    }
}

/// A JSON list of strings.
#[track_caller]
pub fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|a| {
            a.as_str()
                .unwrap_or_else(|| panic!("not a string: {a}"))
                .to_string()
        })
        .collect()
}

/// A lock `hold` took; `World::release` drops it.
#[derive(Clone, Copy, Debug)]
pub struct Held(usize);

/// `run`'s keyword arguments: the target (default the world's run), the working directory
/// (default `elsewhere`), and variables over the world's environment.
#[derive(Default)]
pub struct RunOpts<'a> {
    pub target: Option<&'a Path>,
    pub cwd: Option<&'a Path>,
    pub env: &'a [(&'a str, &'a str)],
}

/// A run `start` made, a PATH with the fake `claude` and no-op keep-awake tools, and the
/// launches on it. `env` is every command's environment.
pub struct World {
    pub run_dir: PathBuf,
    pub paths: RunPaths,
    pub run_id: String,
    pub workspace: PathBuf,
    pub repo: PathBuf,
    pub tmp: PathBuf,
    pub elsewhere: PathBuf,
    pub bindir: PathBuf,
    pub log: PathBuf,
    pub keep_awake_log: PathBuf,
    pub env: Env,
    background: Vec<Bg>,
    outputs: u32,
    held: Vec<Option<File>>,
    /// Last, so it drops (killing its leftovers, removing the temp root) after the rest.
    pub sup: FakeSupervisor,
}

impl World {
    #[track_caller]
    pub fn new() -> World {
        World::with_config(SUPERVISOR_ROUTE)
    }

    #[track_caller]
    pub fn with_config(config: &str) -> World {
        let mut sup = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config,
                ..Options::default()
            },
        );
        let run_dir = sup.start();
        let paths = run_paths(&run_dir);
        let meta = sup.meta();
        let run_id = meta["run_id"].as_str().unwrap().to_string();
        let workspace = PathBuf::from(meta["workspace"].as_os().unwrap());
        let (repo, tmp) = (sup.repo.clone(), sup.tmp.clone());
        let elsewhere = tmp.join("some other dir");
        fs::create_dir(&elsewhere).unwrap();
        let bindir = tmp.join("fake bin");
        fs::create_dir(&bindir).unwrap();
        std::os::unix::fs::symlink(sup.env.fakes().join("claude"), bindir.join("claude")).unwrap();
        let log = tmp.join("fake claude.log");
        let keep_awake_log = tmp.join("keep awake.log");
        let mut path = bindir.clone().into_os_string();
        path.push(":");
        path.push(path_without_self(sup.env.get("PATH").unwrap_or_default()));
        let mut env = sup.env.clone();
        env.set("PATH", path)
            .set("FAKE_CLAUDE_LOG", &log)
            .set("FAKE_CLAUDE_SLEEP", "0")
            .set("FAKE_CLAUDE_EXIT", "0")
            .set("FAKE_KEEP_AWAKE_LOG", &keep_awake_log);
        let w = World {
            run_dir,
            paths,
            run_id,
            workspace,
            repo,
            tmp,
            elsewhere,
            bindir,
            log,
            keep_awake_log,
            env,
            background: Vec::new(),
            outputs: 0,
            held: Vec::new(),
            sup,
        };
        for name in ["caffeinate", "systemd-inhibit"] {
            w.tool(name, NOOP_TOOL);
        }
        w
    }

    /// Writes an executable `name` into the world's `fake bin` directory.
    #[track_caller]
    pub fn tool(&self, name: &str, body: &str) {
        let path = self.bindir.join(name);
        fs::write(&path, body).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // -- driving ----------------------------------------------------------------

    /// Gives the run events beyond run_started, as a run a supervisor already worked: `next`
    /// cuts card 1 and asks for its dispatch. Returns the dispatch ID.
    #[track_caller]
    pub fn progress(&self) -> String {
        let a = self.sup.next();
        assert_eq!(a["action"], "dispatch", "{a}");
        a["dispatch_id"].as_str().unwrap().to_string()
    }

    /// `run TARGET ARGS` in the foreground, from elsewhere.
    #[track_caller]
    pub fn run(&self, args: &[&OsStr]) -> Out {
        self.run_with(args, RunOpts::default())
    }

    /// `run TARGET ARGS` with `o`'s target, working directory and environment; 60 s at most.
    #[track_caller]
    pub fn run_with(&self, args: &[&OsStr], o: RunOpts) -> Out {
        let target = o.target.unwrap_or(&self.run_dir);
        let mut argv = vec![OsStr::new("run"), target.as_os_str()];
        argv.extend_from_slice(args);
        let mut env = self.env.clone();
        for (k, v) in o.env {
            env.set(k, v);
        }
        cli_with(
            &argv,
            &env,
            o.cwd.unwrap_or(&self.elsewhere),
            None,
            Duration::from_secs(60),
        )
    }

    #[track_caller]
    pub fn launch_line(&self, args: &[&OsStr]) -> Out {
        self.launch_line_in(args, &self.elsewhere)
    }

    #[track_caller]
    pub fn launch_line_in(&self, args: &[&OsStr], cwd: &Path) -> Out {
        let mut argv = vec![OsStr::new("launch-line")];
        argv.extend_from_slice(args);
        cli_with(&argv, &self.env, cwd, None, super::TIMEOUT)
    }

    /// A background command from elsewhere in its own process group, its output in files (an
    /// orphaned child that inherits them never blocks a pipe); dropping the world kills it.
    #[track_caller]
    pub fn popen(&mut self, argv: &[OsString], env: &Env) -> Bg {
        self.popen_with(argv, env, None, false)
    }

    /// `popen` with `pass` inherited at `PASSED_FD` (carried on stdout, whose file /bin/sh
    /// then opens itself) and stdin a pipe when `stdin_pipe` (else /dev/null).
    #[track_caller]
    pub fn popen_with(
        &mut self,
        argv: &[OsString],
        env: &Env,
        pass: Option<File>,
        stdin_pipe: bool,
    ) -> Bg {
        self.outputs += 1;
        let out = self.tmp.join(format!("background {} out", self.outputs));
        let err = self.tmp.join(format!("background {} err", self.outputs));
        let err_file = File::create(&err).unwrap();
        let mut cmd = match pass {
            Some(file) => {
                File::create(&out).unwrap();
                let mut cmd = Command::new("/bin/sh");
                cmd.arg("-c")
                    .arg(format!("exec {PASSED_FD}<&1 1>>\"$0\"; exec \"$@\""))
                    .arg(&out)
                    .args(argv)
                    .stdout(Stdio::from(file));
                cmd
            }
            None => {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]).stdout(File::create(&out).unwrap());
                cmd
            }
        };
        cmd.current_dir(&self.elsewhere)
            .env_clear()
            .envs(env.vars())
            .stdin(if stdin_pipe {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(err_file)
            .process_group(0);
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("could not start {argv:?}: {e}"));
        drop(cmd); // the test's copy of a passed file
        let stdin = child.stdin.take();
        let proc = Proc::new(child);
        self.background.push(Bg {
            proc: proc.clone(),
            argv: argv.to_vec(),
            stdin: None,
            out: out.clone(),
            err: err.clone(),
        });
        Bg {
            proc,
            argv: argv.to_vec(),
            stdin,
            out,
            err,
        }
    }

    /// `run TARGET` in the background with a fake supervisor that sleeps `sleep` seconds.
    #[track_caller]
    pub fn launcher(&mut self) -> Bg {
        self.launcher_with(None, "30", &[])
    }

    #[track_caller]
    pub fn launcher_with(
        &mut self,
        target: Option<&Path>,
        sleep: &str,
        env: &[(&str, &str)],
    ) -> Bg {
        let target = target.unwrap_or(&self.run_dir).to_path_buf();
        let mut e = self.env.with("FAKE_CLAUDE_SLEEP", sleep);
        for (k, v) in env {
            e.set(k, v);
        }
        let argv = [
            super::bin().into_os_string(),
            "run".into(),
            target.into_os_string(),
        ];
        self.popen(&argv, &e)
    }

    /// Waits for a background command (at most `timeout`) and returns what it did.
    #[track_caller]
    pub fn finish(&self, p: &Bg, timeout: Duration) -> Out {
        let code = p.wait(timeout);
        Out {
            argv: p.argv.clone(),
            code,
            stdout: fs::read(&p.out).unwrap(),
            stderr: fs::read(&p.err).unwrap(),
        }
    }

    /// Signals a background command of the test's own while it is still running.
    pub fn stop(p: &Bg, sig: Signal) {
        if p.proc.poll().is_none()
            && let Some(pid) = Pid::from_raw(p.pid())
        {
            let _ = kill_process(pid, sig);
        }
    }

    /// Takes an exclusive flock on `path`, as another leadhand process would; `release` (or
    /// dropping the world) drops it.
    #[track_caller]
    pub fn hold(&mut self, path: &Path) -> Held {
        self.held.push(Some(hold_lock(path)));
        Held(self.held.len() - 1)
    }

    pub fn release(&mut self, h: Held) {
        self.held[h.0] = None;
    }

    #[track_caller]
    pub fn write_holder(&self, holder: &Json) {
        fs::write(&self.paths.holder, json::dumps_indent(holder, 2) + "\n").unwrap();
    }

    // -- reading ----------------------------------------------------------------

    #[track_caller]
    pub fn records(&self) -> Vec<Json> {
        if self.log.exists() {
            read_jsonl(&self.log)
        } else {
            Vec::new()
        }
    }

    /// The fake supervisors that started.
    #[track_caller]
    pub fn starts(&self) -> Vec<Start> {
        self.records()
            .into_iter()
            .filter(|r| r.get("argv").is_some())
            .map(|rec| {
                let pid = rec["pid"].as_i64().unwrap() as i32;
                let start = started_by(pid, rec["time"].as_f64().unwrap());
                Start { rec, pid, start }
            })
            .collect()
    }

    #[track_caller]
    pub fn signals(&self, pid: i32) -> Vec<i64> {
        self.records()
            .into_iter()
            .filter(|r| r.get("pid").and_then(Json::as_i64) == Some(i64::from(pid)))
            .filter_map(|r| r.get("signal").and_then(Json::as_i64))
            .collect()
    }

    /// holder.json, or `{}` when it is missing or not JSON.
    pub fn holder(&self) -> Json {
        fs::read(&self.paths.holder)
            .ok()
            .and_then(|b| json::try_parse(&b).ok())
            .unwrap_or_else(|| Json::object([]))
    }

    /// holder.json with `key` set to `value` (`{**holder(), key: value}`).
    pub fn holder_with(&self, key: &str, value: Json) -> Json {
        let mut h = self.holder();
        *at(&mut h, &[key]) = value;
        h
    }

    /// The n-th fake supervisor (from 1), once it has started (its log line), is alive by the
    /// process table, and holder.json names it.
    #[track_caller]
    pub fn wait_supervisor(&self, n: usize, proc: Option<&Proc>) -> Start {
        let sup = wait_for("the fake supervisor to start", proc, || {
            let starts = self.starts();
            starts.get(n - 1).filter(|s| s.start.is_some()).cloned()
        });
        wait_for("holder.json to name the supervisor", proc, || {
            (self.holder().get("supervisor_pid").and_then(Json::as_i64) == Some(i64::from(sup.pid)))
                .then_some(())
        });
        sup
    }

    pub fn alive(rec: &Start) -> bool {
        same_process(rec.pid, rec.start.as_deref())
    }

    #[track_caller]
    pub fn wait_gone(&self, rec: &Start) {
        wait_for(&format!("pid {} to exit", rec.pid), None, || {
            (!World::alive(rec)).then_some(())
        });
    }

    #[track_caller]
    pub fn events<'k>(&self, kind: impl Into<Option<&'k str>>) -> Vec<Json> {
        let kind = kind.into();
        read_records(&self.paths.events)
            .into_iter()
            .filter(|e| kind.is_none_or(|k| e["kind"] == k))
            .collect()
    }

    /// The kinds of the run's events.
    #[track_caller]
    pub fn kinds(&self) -> Vec<String> {
        self.events(None)
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect()
    }

    /// The n-th fake supervisor's argv (from 1).
    #[track_caller]
    pub fn argv(&self, n: usize) -> Vec<String> {
        self.starts()[n - 1].argv()
    }

    // -- cleanup ----------------------------------------------------------------

    /// Kills the background commands, then every process the fakes' logs name that is still
    /// the process named (it started no later than its log line); waits for them to go.
    fn cleanup(&mut self) {
        for p in &mut self.background {
            if p.proc.poll().is_none() {
                p.proc.kill();
            }
            let _ = p.proc.communicate(Duration::from_secs(10));
        }
        let mut killed: Vec<(i32, String)> = Vec::new();
        for log in [&self.log, &self.keep_awake_log] {
            let recs = match fs::read(log) {
                Ok(_) => std::panic::catch_unwind(|| read_jsonl(log)).unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            for rec in recs {
                let pid = rec
                    .get("spawned")
                    .and_then(Json::as_i64)
                    .filter(|p| *p != 0)
                    .or_else(|| rec.get("argv").and(rec.get("pid")).and_then(Json::as_i64));
                let (Some(pid), Some(time)) = (pid, rec.get("time").and_then(Json::as_f64)) else {
                    continue;
                };
                let pid = pid as i32;
                if let Some(start) = started_by(pid, time)
                    && let Some(p) = Pid::from_raw(pid)
                    && kill_process(p, Signal::KILL).is_ok()
                {
                    killed.push((pid, start));
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while killed
            .iter()
            .any(|(pid, start)| same_process(*pid, Some(start)))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.held.clear(); // the hold cleanups, registered after the world's, run first
        self.cleanup();
    }
}

/// The run's holder.json, read (`read_json(run_paths(run_dir).holder)`).
#[track_caller]
pub fn holder_of(run_dir: &Path) -> Json {
    read_json(&run_paths(run_dir).holder)
}
