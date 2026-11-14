//! Git: the runner every Git call goes through (spec, bug class 3), which repository a
//! directory belongs to (spec, "Which repository and run a directory belongs to"), and what a
//! worker changed and how its work is staged for the runner's commit (`stage`), and a run's
//! base and worktree (`workspace`).
//!
//! Nothing outside this module starts a `git` process: later modules call `Git` only. The
//! one other place that runs `git` is the unit tests' fixture below, which plays the operator
//! and the workers (the Python's `tests/gitfixture.py`), and exists only under `cfg(test)`.

mod repo;
mod runner;
pub mod stage;
pub mod workspace;

pub use repo::{
    LinkedWorktree, RepoIdentity, git_common_dir, linked_worktree, repo_identity, sparse_problem,
};
pub use runner::{FETCH_TIMEOUT, GIT_TIMEOUT, Git, REPO_VARS, SAFE_CONFIG, SAFE_ENV, split_nul};
pub use stage::{
    stage_work, status_entries, top_and_prefix, unhide_index_entries, untracked_files,
};
pub use workspace::{default_base, prepare_worktree};

#[cfg(test)]
pub(crate) mod fixture {
    //! Hermetic Git repositories for unit tests, as the Python's `tests/gitfixture.py` makes
    //! them.
    //!
    //! - **Hermetic.** Every Git command the fixture runs shuts out the operator's global and
    //!   system configuration and `HOME`, and commits as a fixed identity without signing. So
    //!   that library calls are sealed the same way, without changing the process environment
    //!   (`set_var` is `unsafe` in edition 2024 and racy across libtest's threads), a
    //!   `Fixture` also gives the runner the same variables for every `Git` call made on its
    //!   own thread, until it is dropped. A `Git` call's own `run_env` variables still win.
    //! - **Confined.** Every helper is confined to the fixture's root, a fresh directory inside
    //!   the system temp dir: a helper given any other path, or a relative one (which would
    //!   resolve against the process cwd, the project's own checkout), panics before running
    //!   or writing anything. `GIT_CEILING_DIRECTORIES` is the root's parent, so Git never
    //!   discovers an enclosing repository.

    use std::ffi::{OsStr, OsString};
    use std::path::{Component, Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::REPO_VARS;

    const NAME: &str = "Fixture User";
    const EMAIL: &str = "fixture@example.invalid";
    const IDENTITY: [&str; 8] = [
        "-c",
        "user.name=Fixture User",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "tag.gpgsign=false",
    ];

    pub(crate) struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        /// A fresh fixture root inside the system temp dir; `Git` calls on this thread are
        /// sealed until the fixture is dropped.
        pub(crate) fn new() -> Fixture {
            let dir = tempfile::tempdir().expect("a temp dir");
            let root = dir.path().canonicalize().expect("the temp dir resolves");
            let tmp = std::env::temp_dir()
                .canonicalize()
                .expect("the temp dir resolves");
            assert!(
                root.starts_with(&tmp) && root != tmp,
                "fixture root {} is not inside the temp dir {}",
                root.display(),
                tmp.display()
            );
            let fixture = Fixture { _dir: dir, root };
            super::runner::set_test_env(fixture.hermetic_env());
            fixture
        }

        /// The fixture root (resolved).
        pub(crate) fn root(&self) -> &Path {
            &self.root
        }

        /// The variables that seal a Git call: `Some` sets one, `None` removes it.
        pub(crate) fn hermetic_env(&self) -> Vec<(OsString, Option<OsString>)> {
            let ceiling = self.root.parent().expect("the root is inside the temp dir");
            let set = [
                ("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null")),
                ("GIT_CONFIG_NOSYSTEM", OsStr::new("1")),
                ("GIT_TERMINAL_PROMPT", OsStr::new("0")),
                ("GIT_CEILING_DIRECTORIES", ceiling.as_os_str()),
                ("HOME", self.root.as_os_str()),
                ("GIT_AUTHOR_NAME", OsStr::new(NAME)),
                ("GIT_AUTHOR_EMAIL", OsStr::new(EMAIL)),
                ("GIT_COMMITTER_NAME", OsStr::new(NAME)),
                ("GIT_COMMITTER_EMAIL", OsStr::new(EMAIL)),
            ];
            // XDG_CONFIG_HOME would name another global config, ignore and attributes file;
            // GIT_CONFIG_PARAMETERS and GIT_CONFIG_COUNT carry a parent Git's `-c` settings.
            let removed = [
                "XDG_CONFIG_HOME",
                "GIT_CONFIG_PARAMETERS",
                "GIT_CONFIG_COUNT",
            ];
            let set = set.map(|(k, v)| (OsString::from(k), Some(v.to_os_string())));
            let removed = REPO_VARS
                .iter()
                .chain(&removed)
                .map(|k| (OsString::from(k), None));
            set.into_iter().chain(removed).collect()
        }

        /// `path` resolved, after checking it lies inside the fixture root.
        pub(crate) fn confined(&self, path: &Path) -> PathBuf {
            match resolve(path) {
                Some(real) if real.starts_with(&self.root) => real,
                Some(_) => panic!(
                    "{} is outside the Git fixture root {}",
                    path.display(),
                    self.root.display()
                ),
                None => panic!(
                    "Git fixture helper given the relative path {path:?}; it would act in the \
                     process cwd"
                ),
            }
        }

        /// Run `git` hermetically in `cwd`; stdout without trailing newlines. Panics on
        /// failure.
        pub(crate) fn git(&self, cwd: &Path, args: &[&str]) -> String {
            self.git_with(cwd, args, &[])
        }

        /// As `git`, with `env` set on top of the hermetic variables.
        pub(crate) fn git_with(&self, cwd: &Path, args: &[&str], env: &[(&str, &OsStr)]) -> String {
            let cwd = self.confined(cwd);
            let mut cmd = Command::new("git");
            cmd.args(IDENTITY)
                .args(args)
                .current_dir(&cwd)
                .stdin(Stdio::null());
            for (k, v) in self.hermetic_env() {
                match v {
                    Some(v) => cmd.env(k, v),
                    None => cmd.env_remove(k),
                };
            }
            cmd.envs(env.iter().copied());
            let out = cmd.output().expect("git runs");
            assert!(
                out.status.success(),
                "git {} failed ({}) in {}: {}",
                args.join(" "),
                out.status,
                cwd.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8(out.stdout).expect("fixture output is UTF-8");
            text.trim_end_matches('\n').to_string()
        }

        /// Write `files` (relative path, text) under `dir`, after checking every one of them.
        pub(crate) fn write(&self, dir: &Path, files: &[(&str, &str)]) {
            self.confined(dir);
            let paths: Vec<PathBuf> = files
                .iter()
                .map(|(rel, _)| self.confined(&dir.join(rel)))
                .collect();
            for (path, (_, text)) in paths.iter().zip(files) {
                std::fs::create_dir_all(path.parent().expect("a file has a folder")).unwrap();
                std::fs::write(path, text).unwrap();
            }
        }

        /// Write `files`, stage them and commit; the new HEAD sha.
        pub(crate) fn commit(&self, cwd: &Path, files: &[(&str, &str)], message: &str) -> String {
            self.write(cwd, files);
            let mut add = vec!["add", "--"];
            add.extend(files.iter().map(|(rel, _)| *rel));
            self.git(cwd, &add);
            self.git(cwd, &["commit", "--quiet", "-m", message]);
            self.git(cwd, &["rev-parse", "HEAD"])
        }

        /// A repository `<root>/<name>` with one commit (README.md) on `main`.
        pub(crate) fn make_repo(&self, name: &str) -> PathBuf {
            self.make_repo_in(&self.root.clone(), name)
        }

        /// A repository `<parent>/<name>` with one commit (README.md) on `main`.
        pub(crate) fn make_repo_in(&self, parent: &Path, name: &str) -> PathBuf {
            let repo = self.confined(&parent.join(name));
            std::fs::create_dir_all(&repo).unwrap();
            self.git(&repo, &["init", "--quiet", "--initial-branch", "main"]);
            for (key, value) in [
                ("user.name", NAME),
                ("user.email", EMAIL),
                ("commit.gpgsign", "false"),
                ("tag.gpgsign", "false"),
            ] {
                self.git(&repo, &["config", key, value]);
            }
            self.commit(&repo, &[("README.md", "hello\n")], "initial");
            repo
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            super::runner::set_test_env(Vec::new());
        }
    }

    /// `path` with `.`, `..` and symlinks resolved as far as it exists (Python's
    /// `Path.resolve`); `None` for a relative path.
    fn resolve(path: &Path) -> Option<PathBuf> {
        if !path.is_absolute() {
            return None;
        }
        let mut real = PathBuf::from("/");
        for part in path.components() {
            match part {
                Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
                Component::ParentDir => {
                    real.pop();
                }
                Component::Normal(name) => {
                    real.push(name);
                    if let Ok(resolved) = real.canonicalize() {
                        real = resolved;
                    }
                }
            }
        }
        Some(real)
    }

    mod tests {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        use super::*;

        /// Removes what a regressed guard would leave behind, even when the test fails.
        struct Cleanup(Vec<PathBuf>);

        impl Drop for Cleanup {
            fn drop(&mut self) {
                for p in &self.0 {
                    let _ = std::fs::remove_file(p);
                    let _ = std::fs::remove_dir_all(p);
                }
            }
        }

        fn refused(f: impl FnOnce()) -> bool {
            catch_unwind(AssertUnwindSafe(f)).is_err()
        }

        // CATALOG: test_gitops.py::GitopsTests::test_git_helpers_refuse_paths_outside_the_fixture
        #[test]
        fn git_helpers_refuse_paths_outside_the_fixture() {
            let f = Fixture::new();
            let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"); // the project's checkout
            let above = f.root().parent().unwrap().to_path_buf();
            let _cleanup = Cleanup(vec![
                here.join("escaped-fixture-file.txt"),
                here.join("escaped repo"),
                above.join("escaped.txt"),
            ]);
            // "" and "." stand for the Python's None: both would run in the process cwd.
            for cwd in [Path::new(""), Path::new("."), &here, &above, Path::new("/")] {
                assert!(refused(|| drop(f.git(cwd, &["status"]))), "{cwd:?}");
            }
            assert!(refused(
                || f.write(&here, &[("escaped-fixture-file.txt", "x\n")])
            ));
            assert!(!here.join("escaped-fixture-file.txt").exists());
            assert!(refused(|| f.write(f.root(), &[("../escaped.txt", "x\n")])));
            assert!(!above.join("escaped.txt").exists());
            assert!(refused(|| drop(f.make_repo_in(&here, "escaped repo"))));
            assert!(!here.join("escaped repo").exists());
            let ceiling = f
                .hermetic_env()
                .into_iter()
                .find(|(k, _)| k == "GIT_CEILING_DIRECTORIES")
                .and_then(|(_, v)| v);
            assert_eq!(ceiling.as_deref(), Some(above.as_os_str()));
        }
    }
}
