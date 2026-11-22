//! `Git`: the only way leadhand runs `git` (spec, bug class 3; `gitops._run`).
//!
//! Every call gets an argument vector, never a shell string, and:
//! - runs only in an existing absolute directory;
//! - prepends the pinned settings (`SAFE_CONFIG`), which win over the user's Git config: no
//!   repository hook or fsmonitor command, nested repositories never looked into, and diff
//!   output named from the top with `a/` and `b/`;
//! - drops the variables that point Git at another repository (`REPO_VARS`), and sets
//!   `SAFE_ENV` (no terminal prompt, no git-lfs smudge);
//! - reads nothing from stdin unless the call gives it input (`run_input`, the Python's
//!   `input=`), and is killed at its deadline;
//! - returns stdout as bytes, with Git's stderr carried in the error.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::error::{Error, Failure, Result};

/// How long a Git call may take (`gitops.GIT_TIMEOUT`).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a fetch or `ls-remote` may take (`gitops.FETCH_TIMEOUT`).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(600);

/// Prepended to every Git call (`gitops.SAFE_CONFIG`, ruling R45): no repository hook, no
/// fsmonitor command (command-line config reaches Git's child processes, so a nested
/// repository's too), nested repositories never looked into, and none of the diff settings
/// that would change output leadhand reads (paths scoped to the cwd, or other prefixes).
pub const SAFE_CONFIG: [&str; 16] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "diff.ignoreSubmodules=all",
    "-c",
    "diff.relative=false",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.srcPrefix=a/",
    "-c",
    "diff.dstPrefix=b/",
];

/// Set in the environment of every Git call (`gitops.SAFE_ENV`).
pub const SAFE_ENV: [(&str, &str); 2] =
    [("GIT_TERMINAL_PROMPT", "0"), ("GIT_LFS_SKIP_SMUDGE", "1")];

/// Removed from the environment of every Git call (`gitops._REPO_VARS`).
pub const REPO_VARS: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

/// How often the end of a Git process is polled once it has closed its output.
const EXIT_POLL: Duration = Duration::from_millis(1);

/// A working directory to run Git in: an existing absolute directory, checked before
/// anything runs (None, "", "." or a relative path would run Git against the process cwd,
/// the operator's repository), and a deadline for each call.
#[derive(Clone, Debug)]
pub struct Git {
    cwd: PathBuf,
    deadline: Duration,
}

impl Git {
    /// Git in `cwd`, with the default deadline. Refuses a relative or missing `cwd`, or one
    /// that is not a directory.
    pub fn at(cwd: &Path) -> Result<Git> {
        Ok(Git::try_at(cwd)?)
    }

    /// As `at`, with the refusal's text kept lossless.
    pub fn try_at(cwd: &Path) -> std::result::Result<Git, Failure> {
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(Failure::of(&[
                OsStr::new("git: workspace '"),
                cwd.as_os_str(),
                OsStr::new("' is not an existing absolute directory"),
            ]));
        }
        Ok(Git {
            cwd: cwd.to_path_buf(),
            deadline: GIT_TIMEOUT,
        })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The deadline of each call.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// The same Git with another deadline per call (`FETCH_TIMEOUT` for a fetch).
    pub fn with_deadline(&self, deadline: Duration) -> Git {
        Git {
            deadline,
            ..self.clone()
        }
    }

    /// Run `git <SAFE_CONFIG> <args>`; its stdout, exactly as Git wrote it. A non-zero exit is
    /// an error carrying Git's stderr.
    pub fn run(&self, args: &[&OsStr]) -> Result<Vec<u8>> {
        self.run_env(args, &[])
    }

    /// As `run`, with `env` set last (the Python's `env` overlay), so it can set a variable
    /// the runner otherwise removes, such as a temporary `GIT_INDEX_FILE`.
    pub fn run_env(&self, args: &[&OsStr], env: &[(&str, &OsStr)]) -> Result<Vec<u8>> {
        let out = self.spawn(args, env, None)?;
        self.succeeded(args, out)
    }

    /// As `run`, with `input` fed on Git's stdin (the Python's `git(..., input=...)`), then end
    /// of file. It is written from a thread of its own, so a Git that writes before it has read
    /// everything never blocks on a full pipe.
    pub fn run_input(&self, args: &[&OsStr], input: &[u8]) -> Result<Vec<u8>> {
        let out = self.spawn(args, &[], Some(input))?;
        self.succeeded(args, out)
    }

    /// As `run_input`, with `env` set last as `run_env` sets it: a pathspec list on stdin for a
    /// command that works in a temporary index.
    pub fn run_env_input(
        &self,
        args: &[&OsStr],
        env: &[(&str, &OsStr)],
        input: &[u8],
    ) -> Result<Vec<u8>> {
        let out = self.spawn(args, env, Some(input))?;
        self.succeeded(args, out)
    }

    fn succeeded(&self, args: &[&OsStr], out: Output) -> Result<Vec<u8>> {
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(self.failure(args, &out))
        }
    }

    /// Whether the command exits 0. Anything else is false, a Git that cannot be run or misses
    /// its deadline included.
    pub fn ok(&self, args: &[&OsStr]) -> bool {
        self.output(args).is_ok_and(|out| out.status.success())
    }

    /// The command's exit status, stdout and stderr, whatever its exit (the Python's `_run`):
    /// for commands whose exit status is part of the answer. An error only when Git could
    /// not be run or missed its deadline.
    pub fn output(&self, args: &[&OsStr]) -> Result<Output> {
        Ok(self.spawn(args, &[], None)?)
    }

    /// As `output`, with the error's text kept lossless.
    pub fn try_output(&self, args: &[&OsStr]) -> std::result::Result<Output, Failure> {
        Ok(self.spawn(args, &[], None)?)
    }

    /// As `output`, telling a missed deadline apart: `None` when Git was killed at its
    /// deadline (the Python's `subprocess.TimeoutExpired`, which its caller words itself).
    pub fn output_or_timeout(&self, args: &[&OsStr]) -> Result<Option<Output>> {
        match self.spawn(args, &[], None) {
            Ok(out) => Ok(Some(out)),
            Err(e) if e.timed_out => Ok(None),
            Err(e) => Err(e.failure.into()),
        }
    }

    /// The error for a command that failed (the Python's `_fail`): its arguments, exit, the
    /// working directory and Git's stderr (else its stdout), shown lossily.
    pub fn failure(&self, args: &[&OsStr], out: &Output) -> Error {
        self.failure_text(args, out).into()
    }

    /// As `failure`, with the text kept lossless.
    pub fn failure_text(&self, args: &[&OsStr], out: &Output) -> Failure {
        let code = out
            .status
            .code()
            .or_else(|| out.status.signal().map(|s| -s))
            .unwrap_or(-1);
        let detail = if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        let head = format!(" failed with exit {code} (in ");
        let mut parts = vec![OsStr::new("git ")];
        parts.extend(joined(args));
        parts.extend([
            OsStr::new(&head),
            self.cwd.as_os_str(),
            OsStr::new("): "),
            OsStr::from_bytes(trim_text(detail)),
        ]);
        Failure::of(&parts)
    }

    fn spawn(
        &self,
        args: &[&OsStr],
        env: &[(&str, &OsStr)],
        input: Option<&[u8]>,
    ) -> std::result::Result<Output, SpawnError> {
        let started = Instant::now();
        let mut child = Command::new("git")
            .args(SAFE_CONFIG)
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(child_env(std::env::vars_os(), env))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.error(args, &e.to_string()))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // Written on its own thread, then closed (end of file). A Git that exits before
            // reading it all breaks the pipe, which ends the write; its exit status tells.
            let input = input.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
        }
        // Each pipe is read to its end on its own thread, so neither fills while Git waits,
        // and the deadline covers the reading too.
        let (tx, rx) = mpsc::channel();
        read_on_thread(0, child.stdout.take(), tx.clone());
        read_on_thread(1, child.stderr.take(), tx);
        let mut got: [Option<Vec<u8>>; 2] = [None, None];
        while got.iter().any(Option::is_none) {
            let left = self.deadline.saturating_sub(started.elapsed());
            match rx.recv_timeout(left) {
                Ok((i, Ok(buf))) => got[i] = Some(buf),
                Ok((_, Err(e))) => return Err(self.stop(&mut child, args, &e.to_string()).into()),
                Err(RecvTimeoutError::Timeout) => return Err(self.timed_out(&mut child, args)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.stop(&mut child, args, "its output was lost").into());
                }
            }
        }
        // Both pipes are closed, so Git has exited or is about to.
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= self.deadline => {
                    return Err(self.timed_out(&mut child, args));
                }
                Ok(None) => std::thread::sleep(EXIT_POLL),
                Err(e) => return Err(self.stop(&mut child, args, &e.to_string()).into()),
            }
        };
        let [stdout, stderr] = got.map(Option::unwrap_or_default);
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Kill Git, reap it, and the error. A process Git started that still holds the pipes is
    /// left to finish on its own, as the Python leaves it; its reader threads end with it.
    fn stop(&self, child: &mut Child, args: &[&OsStr], what: &str) -> Failure {
        let _ = child.kill();
        let _ = child.wait();
        self.error(args, what)
    }

    fn timed_out(&self, child: &mut Child, args: &[&OsStr]) -> SpawnError {
        let secs = self.deadline.as_secs_f64();
        SpawnError {
            failure: self.stop(child, args, &format!("timed out after {secs}s")),
            timed_out: true,
        }
    }

    fn error(&self, args: &[&OsStr], what: &str) -> Failure {
        let tail = format!(": {what} (in ");
        let mut parts = vec![OsStr::new("git ")];
        parts.extend(joined(args));
        parts.extend([OsStr::new(&tail), self.cwd.as_os_str(), OsStr::new(")")]);
        Failure::of(&parts)
    }
}

/// Why a Git call gave no output: its error, and whether that was its deadline.
struct SpawnError {
    failure: Failure,
    timed_out: bool,
}

impl From<Failure> for SpawnError {
    fn from(failure: Failure) -> SpawnError {
        SpawnError {
            failure,
            timed_out: false,
        }
    }
}

impl From<SpawnError> for Failure {
    fn from(e: SpawnError) -> Failure {
        e.failure
    }
}

impl From<SpawnError> for Error {
    fn from(e: SpawnError) -> Error {
        e.failure.into()
    }
}

/// The arguments as an error message shows them: separated by spaces, bytes kept.
fn joined<'a>(args: &[&'a OsStr]) -> Vec<&'a OsStr> {
    let mut out = Vec::with_capacity(args.len() * 2);
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push(OsStr::new(" "));
        }
        out.push(*a);
    }
    out
}

/// `bytes` without leading and trailing whitespace (ASCII, and the Unicode whitespace of any
/// valid UTF-8 at either end), as `str::trim` trims the text.
fn trim_text(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid();
        let kept = valid.trim_start();
        start += valid.len() - kept.len();
        if !kept.is_empty() || !chunk.invalid().is_empty() {
            break;
        }
    }
    let rest = &bytes[start..];
    let mut end = rest.len();
    loop {
        // The last character, when the tail ends in valid UTF-8.
        let last = (1..=end.min(4)).find_map(|n| {
            let mut chars = std::str::from_utf8(&rest[end - n..end]).ok()?.chars();
            let c = chars.next()?;
            chars.next().is_none().then_some((c, n))
        });
        match last {
            Some((c, n)) if c.is_whitespace() => end -= n,
            _ => break,
        }
    }
    &rest[..end]
}

/// Read `pipe` to its end on a thread of its own, and send the bytes as field `i`.
fn read_on_thread(
    i: usize,
    pipe: Option<impl Read + Send + 'static>,
    tx: Sender<(usize, io::Result<Vec<u8>>)>,
) {
    let mut pipe = pipe.expect("both pipes are requested");
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = pipe.read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send((i, read));
    });
}

/// A Git call's environment: the inherited one without `REPO_VARS`, then `SAFE_ENV`, then
/// the caller's `extra`.
fn child_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[(&str, &OsStr)],
) -> Vec<(OsString, OsString)> {
    let mut env: BTreeMap<OsString, OsString> = inherited
        .into_iter()
        .filter(|(k, _)| !REPO_VARS.iter().any(|v| k == v))
        .collect();
    #[cfg(test)]
    TEST_ENV.with(|vars| {
        for (k, v) in vars.borrow().iter() {
            match v {
                Some(v) => env.insert(k.clone(), v.clone()),
                None => env.remove(k),
            };
        }
    });
    for (k, v) in SAFE_ENV {
        env.insert(k.into(), v.into());
    }
    for (k, v) in extra {
        env.insert(k.into(), v.into());
    }
    env.into_iter().collect()
}

/// The fields of `-z` output, empty ones dropped (`gitops._nul_list`).
pub fn split_nul(out: &[u8]) -> Vec<Vec<u8>> {
    out.split(|b| *b == 0)
        .filter(|field| !field.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

#[cfg(test)]
thread_local! {
    /// The fixture's hermetic variables for `Git` calls on this thread (`git::fixture`).
    static TEST_ENV: std::cell::RefCell<Vec<(OsString, Option<OsString>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Seal the `Git` calls this thread makes with `vars` (`Some` sets, `None` removes); an
/// empty list ends it.
#[cfg(test)]
pub(crate) fn set_test_env(vars: Vec<(OsString, Option<OsString>)>) {
    TEST_ENV.with(|v| *v.borrow_mut() = vars);
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::time::Instant;

    use super::*;
    use crate::git::fixture::Fixture;

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    fn text(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn refuses_relative_cwd() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        for cwd in ["", ".", "tests", "my repo", "src/../src"] {
            let e = Git::at(Path::new(cwd)).unwrap_err();
            assert!(
                e.message.contains("is not an existing absolute directory"),
                "{cwd:?}: {e}"
            );
        }
        assert!(Git::at(&repo).is_ok());
    }

    // CATALOG: test_gitops.py::GitopsTests::test_git_refuses_a_missing_workspace_directory
    #[test]
    fn git_refuses_a_missing_workspace_directory() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let missing = f.root().join("missing");
        let not_a_dir = repo.join("README.md");
        // "", "." and relative paths would resolve against the process cwd.
        for cwd in [
            &missing,
            &not_a_dir,
            Path::new(""),
            Path::new("."),
            Path::new("tests"),
            Path::new("my repo"),
        ] {
            let e = Git::at(cwd).unwrap_err();
            assert!(
                e.message.contains("is not an existing absolute directory"),
                "{cwd:?}: {e}"
            );
            assert!(e.message.contains(&*cwd.to_string_lossy()), "{e}");
        }
    }

    // CATALOG: test_gitops.py::GitopsTests::test_git_error_includes_stderr
    #[test]
    fn git_error_includes_stderr() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let git = Git::at(&repo).unwrap();
        let e = git
            .run(&[os("rev-parse"), os("--verify"), os("no-such-ref")])
            .unwrap_err();
        assert!(e.message.contains("no-such-ref"), "{e}");
        assert!(e.message.contains("failed with exit 128"), "{e}");
        assert!(e.message.contains(&*repo.to_string_lossy()), "{e}");
        assert!(
            e.message.contains("fatal: "),
            "Git's stderr is carried: {e}"
        );
        assert_eq!(
            text(
                git.run(&[os("rev-parse"), os("--abbrev-ref"), os("HEAD")])
                    .unwrap()
            ),
            "main\n"
        );
        let id = crate::git::repo_identity(&repo).unwrap();
        assert_eq!(id.common_dir, repo.join(".git").canonicalize().unwrap());
    }

    #[test]
    fn pinned_config_wins_over_user_diff_relative_and_prefixes() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(
            &repo,
            &[("top.txt", "1\n"), ("sub/a.txt", "1\n")],
            "two files",
        );
        f.write(&repo, &[("top.txt", "2\n"), ("sub/a.txt", "2\n")]);
        let user = f.root().join("user.gitconfig");
        std::fs::write(
            &user,
            "[diff]\n\trelative = true\n\tnoprefix = true\n\tmnemonicPrefix = true\n\
             \tsrcPrefix = x/\n\tdstPrefix = y/\n",
        )
        .unwrap();
        let sub = repo.join("sub");
        let env = [("GIT_CONFIG_GLOBAL", user.as_os_str())];
        // The user config is in effect: Git alone scopes the diff to `sub` and drops prefixes.
        let theirs = f.git_with(&sub, &["diff"], &env);
        assert!(theirs.contains("diff --git a.txt a.txt"), "{theirs}");
        assert!(!theirs.contains("top.txt"), "{theirs}");
        // Through the runner the same diff covers the checkout, named from the top, a/ and b/.
        let ours = text(Git::at(&sub).unwrap().run_env(&[os("diff")], &env).unwrap());
        assert!(
            ours.contains("diff --git a/sub/a.txt b/sub/a.txt\n"),
            "{ours}"
        );
        assert!(ours.contains("diff --git a/top.txt b/top.txt\n"), "{ours}");
        assert!(ours.contains("--- a/top.txt\n+++ b/top.txt\n"), "{ours}");
    }

    #[test]
    fn pinned_settings_match_the_python_reference() {
        // `git.json`: the reference's `[list(SAFE_CONFIG), sorted(SAFE_ENV.items()),
        // list(_REPO_VARS)]`.
        let recorded = crate::wire::tests::reference_json("git.json");
        let rows = recorded["pinned_settings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let mut env: Vec<[&str; 2]> = SAFE_ENV.iter().map(|(k, v)| [*k, *v]).collect();
        env.sort();
        let ours = serde_json::to_string(&(SAFE_CONFIG, env, REPO_VARS)).unwrap();
        let theirs = &rows[0];
        assert_eq!(
            &serde_json::from_str::<serde_json::Value>(&ours).unwrap(),
            theirs
        );
    }

    #[test]
    fn repo_vars_are_removed_and_safe_env_set() {
        let inherited = [
            ("PATH", "/usr/bin"),
            ("GIT_DIR", "/elsewhere/.git"),
            ("GIT_WORK_TREE", "/elsewhere"),
            ("GIT_INDEX_FILE", "/elsewhere/index"),
            ("GIT_COMMON_DIR", "/elsewhere/.git"),
            ("GIT_OBJECT_DIRECTORY", "/elsewhere/objects"),
            ("GIT_TERMINAL_PROMPT", "1"),
            ("GIT_LFS_SKIP_SMUDGE", "0"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = child_env(inherited.clone(), &[]);
        let get = |env: &[(OsString, OsString)], k: &str| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        for var in REPO_VARS {
            assert_eq!(get(&env, var), None, "{var}");
        }
        assert_eq!(get(&env, "PATH"), Some("/usr/bin".into()));
        assert_eq!(get(&env, "GIT_TERMINAL_PROMPT"), Some("0".into()));
        assert_eq!(get(&env, "GIT_LFS_SKIP_SMUDGE"), Some("1".into()));
        // A caller's own variables come last, as the Python's `env` overlay does: a temporary
        // index for a commit, say.
        let env = child_env(inherited, &[("GIT_INDEX_FILE", os("/run/tmp-index"))]);
        assert_eq!(get(&env, "GIT_INDEX_FILE"), Some("/run/tmp-index".into()));
        assert_eq!(get(&env, "GIT_DIR"), None);
    }

    #[test]
    fn lfs_smudge_skipped() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        // A stand-in for the operator's git-lfs filter: it leaves the content alone only when
        // told to skip the smudge, as git-lfs does with GIT_LFS_SKIP_SMUDGE=1.
        let smudge = r#"if [ "$GIT_LFS_SKIP_SMUDGE" = 1 ]; then cat; else echo SMUDGED; fi"#;
        f.git(&repo, &["config", "filter.lfs.smudge", smudge]);
        f.git(&repo, &["config", "filter.lfs.clean", "cat"]);
        f.commit(
            &repo,
            &[
                (".gitattributes", "*.bin filter=lfs\n"),
                ("big.bin", "pointer\n"),
            ],
            "lfs",
        );
        let big = repo.join("big.bin");
        std::fs::remove_file(&big).unwrap();
        f.git(&repo, &["checkout", "--", "big.bin"]);
        assert_eq!(
            std::fs::read_to_string(&big).unwrap(),
            "SMUDGED\n",
            "the stand-in smudges"
        );
        std::fs::remove_file(&big).unwrap();
        Git::at(&repo)
            .unwrap()
            .run(&[os("checkout"), os("--"), os("big.bin")])
            .unwrap();
        assert_eq!(std::fs::read_to_string(&big).unwrap(), "pointer\n");
    }

    #[test]
    fn ok_is_the_exit_status() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let git = Git::at(&repo).unwrap();
        assert!(git.ok(&[os("rev-parse"), os("--verify"), os("--quiet"), os("HEAD")]));
        assert!(!git.ok(&[
            os("rev-parse"),
            os("--verify"),
            os("--quiet"),
            os("no-such-ref")
        ]));
        assert!(!git.ok(&[os("no-such-command")]));
        let out = git
            .output(&[os("rev-parse"), os("--verify"), os("--quiet"), os("nope")])
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let e = git.failure(
            &[os("rev-parse"), os("--verify"), os("--quiet"), os("nope")],
            &out,
        );
        assert!(
            e.message
                .starts_with("git rev-parse --verify --quiet nope failed with exit 1 (in "),
            "{e}"
        );
    }

    #[test]
    fn failure_text_keeps_bytes_and_failure_shows_them_as_before() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let git = Git::at(&repo).unwrap();
        let args = [
            os("rev-parse"),
            os("--verify"),
            OsStr::from_bytes(b"no-\xff"),
        ];
        let out = git.output(&args).unwrap();
        let lossless = git.failure_text(&args, &out);
        let json = crate::wire::ascii_json(&lossless.text);
        assert!(
            json.starts_with("\"git rev-parse --verify no-\\udcff failed with exit 128 (in "),
            "{json}"
        );
        // The Error's text is what it was: the arguments, cwd and stderr shown lossily, trimmed.
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            git.failure(&args, &out).message,
            format!(
                "git rev-parse --verify no-\u{fffd} failed with exit 128 (in {}): {}",
                repo.display(),
                stderr.trim()
            )
        );
        assert_eq!(trim_text(b" \t\xc2\xa0x \xff y\n\xe2\x80\x83"), b"x \xff y");
        assert_eq!(trim_text(b"\xff \n"), b"\xff");
        assert_eq!(trim_text(b" \n "), b"");
    }

    #[test]
    fn args_are_os_strings_and_output_is_bytes() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let arg = OsStr::from_bytes(b"caf\xff \xc3\xa9");
        let out = Git::at(&repo)
            .unwrap()
            .run(&[os("rev-parse"), os("--sq-quote"), arg])
            .unwrap();
        assert_eq!(out, b" 'caf\xff \xc3\xa9'\n");
    }

    #[test]
    fn deadline_stops_a_hung_git() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let git = Git::at(&repo)
            .unwrap()
            .with_deadline(Duration::from_millis(300));
        let started = Instant::now();
        let e = git
            .run(&[os("-c"), os("alias.hang=!sleep 2"), os("hang")])
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert!(e.message.contains("timed out after 0.3s (in "), "{e}");
        assert_eq!(Git::at(&repo).unwrap().deadline, GIT_TIMEOUT);
        // The same deadline, told apart from any other failure.
        let hang = [os("-c"), os("alias.hang=!sleep 2"), os("hang")];
        assert!(git.output_or_timeout(&hang).unwrap().is_none());
        let fine = git
            .output_or_timeout(&[os("rev-parse"), os("HEAD")])
            .unwrap();
        assert!(fine.is_some_and(|out| out.status.success()));
    }

    #[test]
    fn input_is_fed_on_stdin_then_end_of_file() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let git = Git::at(&repo).unwrap();
        let args = [os("hash-object"), os("--stdin")];
        // `printf 'hello\n' | git hash-object --stdin`.
        let out = git.run_input(&args, b"hello\n").unwrap();
        assert_eq!(out, b"ce013625030ba8dba906f756967f9e9ca394464a\n");
        // More than a pipe holds, with bytes that are not UTF-8: written whole, never blocked.
        let big: Vec<u8> = (0..4_000_000u32).map(|i| (i % 251) as u8).collect();
        let got = git.run_input(&args, &big).unwrap();
        let file = f.root().join("big");
        std::fs::write(&file, &big).unwrap();
        let want = f.git(&repo, &["hash-object", file.to_str().unwrap()]) + "\n";
        assert_eq!(got, want.as_bytes());
        // Without input, stdin is still nothing at all: Git reads end of file at once.
        assert_eq!(
            git.run(&args).unwrap(),
            b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n"
        );
    }

    #[test]
    fn split_nul_drops_empty_fields() {
        assert_eq!(split_nul(b""), Vec::<Vec<u8>>::new());
        assert_eq!(
            split_nul(b"a\0b c\0\0\xff\0"),
            vec![b"a".to_vec(), b"b c".to_vec(), b"\xff".to_vec()]
        );
    }
}
