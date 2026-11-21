//! The Codex worker's environment (spec contract item 9), against real Git and `gh`.
//!
//! The Rust form of `tests/contract/contract_worker_env.py` (converted from
//! tests/test_worker_env.py). Each scenario runs one `dispatch` with a parent environment the
//! test chooses (credentials, Git configuration, SSH and askpass programs), and takes the
//! environment the worker really got from the fake Codex's log (FAKE_CODEX_LOG). Git and `gh`
//! then run with exactly that environment, so what they do is the evidence: a refused push
//! leaves a local bare repository without the ref, and a program the parent configured leaves
//! no marker file. No network: remote hosts are `.invalid`, and the HTTP remote is a local
//! server answering 401.

#[macro_use]
mod common;

mod worker_environment {
    use std::ffi::OsString;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::common::dispatch_world::World;
    use crate::common::git::{make_repo_named, run_git};
    use crate::common::json::Json;
    use crate::common::{Env, Out, run_argv, which};

    /// What a parent may hold that the worker must not see.
    const TOKENS: [&str; 7] = [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "SSH_AUTH_SOCK",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
    ];

    /// One dispatch's worker: the world, the environment the fake Codex was started with (as
    /// logged, and as an `Env` to run Git in), the `shell_environment_policy.set` entries on
    /// its command line, and a fresh repository for Git to act in.
    struct Worker {
        w: World,
        logged: Json,
        env: Env,
        policy: Json,
        repo: PathBuf,
    }

    /// Dispatches once with every credential in TOKENS and `parent(tmp)` (variables, made with
    /// files under the world's temp dir) in the parent environment (`worker_env`).
    #[track_caller]
    fn worker_env(parent: impl FnOnce(&Path) -> Vec<(String, String)>) -> Worker {
        let mut w = World::new();
        w.scenario("ready", &[]);
        for name in TOKENS {
            w.env.set(name, format!("parent-{}", name.to_lowercase()));
        }
        for (k, v) in parent(&w.tmp) {
            w.env.set(k, v);
        }
        w.dispatch("d1").ok();
        let mut starts = w.records("start");
        assert_eq!(starts.len(), 1, "{starts:?}");
        let start = starts.remove(0);
        let logged = start["env"].clone();
        let vars: Vec<(OsString, OsString)> = logged
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (crate::common::os(k), v.as_os().unwrap()))
            .collect();
        let repo = make_repo_named(&w.tmp, "worker repo", "main");
        Worker {
            env: Env::from_parts(vars, PathBuf::new()),
            policy: start["policy_set"].clone(),
            logged,
            repo,
            w,
        }
    }

    impl Worker {
        /// Git with the worker's environment and a commit identity, in the fresh repository.
        #[track_caller]
        fn git(&self, args: &[&str]) -> Out {
            let mut argv = vec![
                "git",
                "-c",
                "user.name=W",
                "-c",
                "user.email=w@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ];
            argv.extend_from_slice(args);
            self.run(&argv, Duration::from_secs(10))
        }

        /// `argv` with exactly the worker's environment, in the fresh repository.
        #[track_caller]
        fn run(&self, argv: &[&str], timeout: Duration) -> Out {
            let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
            run_argv(&argv, &self.env, &self.repo, None, timeout)
        }

        fn var(&self, name: &str) -> &str {
            self.logged[name].as_str().unwrap()
        }
    }

    fn script(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A local HTTP server answering every request 401 with a Basic challenge, as Python's
    /// `Answer401` handler (HTTP/1.0: one request per connection). Stopped on drop.
    struct Server401 {
        port: u16,
        stop: Arc<AtomicBool>,
        accept: Option<thread::JoinHandle<()>>,
    }

    impl Server401 {
        fn start() -> Server401 {
            let listener = TcpListener::bind("127.0.0.1:0").expect("binding a local port");
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let accept = thread::spawn(move || {
                for conn in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Ok(conn) = conn {
                        thread::spawn(move || deny(conn));
                    }
                }
            });
            Server401 {
                port,
                stop,
                accept: Some(accept),
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/owner/repo.git", self.port)
        }
    }

    /// Reads the request head, answers 401 and closes.
    fn deny(mut conn: TcpStream) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
        let _ = conn.write_all(
            b"HTTP/1.0 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"leadhand test\"\r\n\
              Content-Length: 0\r\n\r\n",
        );
    }

    impl Drop for Server401 {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(("127.0.0.1", self.port)); // wakes the accept loop
            if let Some(t) = self.accept.take() {
                let _ = t.join();
            }
        }
    }

    #[test]
    fn test_worker_env_removes_tokens_and_sets_empty_gh_config() {
        let wk = worker_env(|tmp| {
            let gh = tmp.join("parent gh config");
            fs::create_dir(&gh).unwrap();
            fs::write(
                gh.join("hosts.yml"),
                "github.com:\n    oauth_token: parent-hosts-token\n",
            )
            .unwrap();
            pairs(&[("GH_CONFIG_DIR", gh.to_str().unwrap())])
        });
        let parent_gh = wk.w.tmp.join("parent gh config");
        for name in TOKENS {
            assert!(
                wk.logged.get(name).is_none(),
                "{name} reached the worker env"
            );
            assert!(
                wk.policy.get(name).is_none(),
                "{name} reached the shell policy"
            );
        }
        assert_eq!(wk.var("LEADHAND_RUN_ID"), wk.w.run_id);
        let gh_dir = PathBuf::from(wk.var("GH_CONFIG_DIR"));
        assert!(gh_dir.is_dir(), "{}", gh_dir.display());
        assert_ne!(
            fs::canonicalize(&gh_dir).unwrap(),
            fs::canonicalize(&parent_gh).unwrap()
        );
        assert_eq!(fs::read_dir(&gh_dir).unwrap().count(), 0);
        assert_eq!(wk.policy["GH_CONFIG_DIR"], wk.var("GH_CONFIG_DIR"));
        assert_eq!(wk.var("GIT_SSH_COMMAND"), "false");
        assert_eq!(wk.var("GIT_TERMINAL_PROMPT"), "0");
        assert_eq!(wk.var("PATH"), wk.w.env.get_str("PATH"));
        let r = wk.git(&["config", "--get-all", "credential.helper"]);
        let stdout = r.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[lines.len().saturating_sub(1)..], [""], "{r}");
    }

    #[test]
    fn test_existing_git_config_env_entries_preserved() {
        // Covers limit M5: the parent's GIT_CONFIG_KEY_n/VALUE_n entries reach Codex's
        // environment and its command line.
        let wk = worker_env(|_| {
            pairs(&[
                ("GIT_CONFIG_COUNT", "2"),
                ("GIT_CONFIG_KEY_0", "user.name"),
                ("GIT_CONFIG_VALUE_0", "Kept Name"),
                ("GIT_CONFIG_KEY_1", "core.abbrev"),
                ("GIT_CONFIG_VALUE_1", "12"),
            ])
        });
        assert_eq!(wk.var("GIT_CONFIG_COUNT"), "3");
        assert_eq!(
            (wk.var("GIT_CONFIG_KEY_2"), wk.var("GIT_CONFIG_VALUE_2")),
            ("credential.helper", "")
        );
        let mut names = vec!["GIT_CONFIG_COUNT".to_string()];
        for kv in ["KEY", "VALUE"] {
            names.extend((0..3).map(|i| format!("GIT_CONFIG_{kv}_{i}")));
        }
        for name in &names {
            assert_eq!(wk.policy[name.as_str()], wk.var(name), "{name}");
        }
        let get = |key: &str| {
            wk.run(&["git", "config", key], Duration::from_secs(10))
                .stdout_text()
                .trim()
                .to_string()
        };
        assert_eq!(get("user.name"), "Kept Name");
        assert_eq!(get("core.abbrev"), "12");
    }

    #[test]
    fn test_existing_git_config_parameters_kept_and_askpass_cleared() {
        // Covers R23.
        let wk = worker_env(|_| {
            pairs(&[("GIT_CONFIG_PARAMETERS", "'user.email=kept@example.invalid'")])
        });
        assert_eq!(
            wk.policy["GIT_CONFIG_PARAMETERS"],
            wk.var("GIT_CONFIG_PARAMETERS")
        );
        let ten = Duration::from_secs(10);
        assert_eq!(
            wk.run(&["git", "config", "user.email"], ten)
                .stdout_text()
                .trim(),
            "kept@example.invalid"
        );
        let r = wk.run(&["git", "config", "--get-all", "core.askPass"], ten);
        let stdout = r.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[lines.len().saturating_sub(1)..], [""], "{r}");
    }

    #[test]
    fn test_ssh_push_fails_for_ssh_scp_and_explicit_pushurl() {
        let wk = worker_env(|tmp| {
            let fake_ssh = tmp.join("fake ssh.sh");
            script(
                &fake_ssh,
                &format!(
                    "#!/bin/sh\ntouch '{}'\nexit 0\n",
                    tmp.join("ssh was called").display()
                ),
            );
            let fake = fake_ssh.to_str().unwrap();
            pairs(&[("GIT_SSH_COMMAND", &format!("'{fake}'")), ("GIT_SSH", fake)])
        });
        let tmp = &wk.w.tmp;
        let marker = tmp.join("ssh was called");
        let repo = &wk.repo;
        run_git(
            repo,
            &[
                "remote",
                "add",
                "sshurl",
                "ssh://git@example.invalid/owner/repo.git",
            ],
        );
        run_git(
            repo,
            &["remote", "add", "scp", "git@example.invalid:owner/repo.git"],
        );
        let bare = tmp.join("local bare.git");
        run_git(tmp, &["init", "--quiet", "--bare", bare.to_str().unwrap()]);
        run_git(repo, &["remote", "add", "pushurl", bare.to_str().unwrap()]);
        run_git(
            repo,
            &[
                "remote",
                "set-url",
                "--push",
                "pushurl",
                "git@example.invalid:owner/repo.git",
            ],
        );
        for remote in ["sshurl", "scp", "pushurl"] {
            let t0 = Instant::now();
            let r = wk.git(&["push", remote, "HEAD:refs/heads/x"]);
            assert_ne!(r.code, 0, "{remote}\n{r}");
            assert!(t0.elapsed() < Duration::from_secs(10), "{remote}");
        }
        assert!(!marker.exists(), "the parent's ssh command ran");
        assert_eq!(run_git(&bare, &["for-each-ref"]), "");
    }

    #[test]
    fn test_https_push_without_credentials_fails_without_prompting() {
        let server = Server401::start();
        let url = server.url();
        let wk = worker_env(|tmp| {
            let helper = tmp.join("cred helper.sh");
            script(
                &helper,
                &format!(
                    "#!/bin/sh\ntouch '{}'\necho username=u\necho password=p\n",
                    tmp.join("helper was called").display()
                ),
            );
            // The parent's Git config names a credential helper; the worker env must reset
            // it.
            pairs(&[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "credential.helper"),
                ("GIT_CONFIG_VALUE_0", &format!("!'{}'", helper.display())),
                ("NO_PROXY", "127.0.0.1"),
                ("no_proxy", "127.0.0.1"),
            ])
        });
        let helper_marker = wk.w.tmp.join("helper was called");
        let t0 = Instant::now();
        let r = wk.git(&["push", &url, "HEAD:refs/heads/x"]);
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
        assert_ne!(r.code, 0, "{r}");
        assert!(
            !helper_marker.exists(),
            "the parent's credential helper ran"
        );
        let stderr = r.stderr_text();
        assert!(
            [
                "terminal prompts disabled",
                "Authentication failed",
                "could not read Username"
            ]
            .iter()
            .any(|p| stderr.contains(p)),
            "{r}"
        );
    }

    #[test]
    fn test_configured_askpass_is_not_run() {
        let server = Server401::start();
        let url = server.url();
        let wk = worker_env(|tmp| {
            let askpass = tmp.join("ask pass.sh");
            script(
                &askpass,
                &format!(
                    "#!/bin/sh\ntouch '{}'\necho secret\n",
                    tmp.join("askpass was called").display()
                ),
            );
            // The user's Git config names an askpass program (here through the parent's
            // GIT_CONFIG_*).
            pairs(&[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "core.askPass"),
                ("GIT_CONFIG_VALUE_0", askpass.to_str().unwrap()),
                ("NO_PROXY", "127.0.0.1"),
                ("no_proxy", "127.0.0.1"),
            ])
        });
        let marker = wk.w.tmp.join("askpass was called");
        let r = wk.git(&["push", &url, "HEAD:refs/heads/x"]);
        assert_ne!(r.code, 0, "{r}");
        assert!(!marker.exists(), "the configured askpass program ran");
    }

    #[test]
    fn test_gh_reports_no_login_with_parent_token() {
        // The Python is `skipUnless(shutil.which("gh"))`: the same condition skips here.
        if which("gh").is_none() {
            println!("skipped: gh is not installed");
            return;
        }
        let wk =
            worker_env(|_| pairs(&[("GH_TOKEN", "ghp_parenttokenvalue0000000000000000000000")]));
        let r = wk.run(&["gh", "auth", "status"], Duration::from_secs(30));
        let both = r.stdout_text() + &r.stderr_text();
        assert_ne!(r.code, 0, "{both}");
        assert!(both.contains("not logged into any GitHub hosts"), "{both}");
        assert!(!both.contains("GH_TOKEN"), "{both}");
    }

    #[test]
    fn test_local_path_push_still_allowed() {
        // The documented limit: a push to a local path needs no credentials, so it works.
        let wk = worker_env(|_| Vec::new());
        let bare = wk.w.tmp.join("local bare.git");
        run_git(
            &wk.w.tmp,
            &["init", "--quiet", "--bare", bare.to_str().unwrap()],
        );
        let r = wk.git(&["push", bare.to_str().unwrap(), "HEAD:refs/heads/x"]);
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(
            run_git(&bare, &["rev-parse", "refs/heads/x"]),
            run_git(&wk.repo, &["rev-parse", "HEAD"])
        );
    }
}
