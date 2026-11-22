//! Which repository a directory belongs to (spec, "Which repository and run a directory
//! belongs to"), and the sparse-checkout refusal (R63; `gitops.sparse_checkout_problem`).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::Git;
use crate::error::Failure;
use crate::pytext::real_path;

/// The repository a directory belongs to: the main working tree of its Git common directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoIdentity {
    /// The main working tree's top, as Git names it (symlinks resolved).
    pub top: PathBuf,
    /// The Git common directory, absolute.
    pub common_dir: PathBuf,
}

/// The repository `any_dir` lies in. From a subdirectory that's the checkout's top; from
/// anywhere in a linked worktree (a run's `<repo>/.worktrees/<slug>`, or one elsewhere) it's
/// the main working tree's top, not the worktree's. An error outside a Git checkout, and for
/// a bare repository, which has no main working tree.
///
/// This replaces `gitops.checkout_top`, which resolves a linked worktree to that worktree.
///
/// The error's text is kept lossless (`Failure`); `?` turns it into an `Error`.
pub fn repo_identity(any_dir: &Path) -> Result<RepoIdentity, Failure> {
    let git = Git::try_at(any_dir)?;
    let common_dir = rev_parse_path(&git, &["--path-format=absolute", "--git-common-dir"])?;
    let git_dir = rev_parse_path(&git, &["--path-format=absolute", "--git-dir"])?;
    let top = if git_dir == common_dir {
        // The main working tree itself (or a bare repository, which Git refuses here).
        rev_parse_path(&git, &["--show-toplevel"])?
    } else {
        main_worktree(&git, &common_dir)?
    };
    Ok(RepoIdentity { top, common_dir })
}

/// The Git common directory of the checkout `dir` lies in, absolute (`gitops.git_common_dir`):
/// the folder a Codex worker gets as an `--add-dir`.
pub fn git_common_dir(dir: &Path) -> Result<PathBuf, Failure> {
    let git = Git::try_at(dir)?;
    rev_parse_path(&git, &["--path-format=absolute", "--git-common-dir"])
}

/// The linked worktree a directory lies in (a run's workspace or its evidence base
/// worktree), with its admin folder, where the run record is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedWorktree {
    /// The worktree's top, as Git names it (symlinks resolved).
    pub top: PathBuf,
    /// Its admin folder in the common directory (`<common>/worktrees/<name>`), absolute.
    pub git_dir: PathBuf,
    /// The repository it belongs to.
    pub repo: RepoIdentity,
}

/// The linked worktree `any_dir` lies in, or `None` when it lies in a main working tree.
/// An error outside a Git checkout, as `repo_identity`.
pub fn linked_worktree(any_dir: &Path) -> Result<Option<LinkedWorktree>, Failure> {
    let git = Git::try_at(any_dir)?;
    let common_dir = rev_parse_path(&git, &["--path-format=absolute", "--git-common-dir"])?;
    let git_dir = rev_parse_path(&git, &["--path-format=absolute", "--git-dir"])?;
    if git_dir == common_dir {
        return Ok(None);
    }
    Ok(Some(LinkedWorktree {
        top: rev_parse_path(&git, &["--show-toplevel"])?,
        git_dir,
        repo: RepoIdentity {
            top: main_worktree(&git, &common_dir)?,
            common_dir,
        },
    }))
}

/// Why a run cannot use the checkout at `top` because it is sparse (R63), or `None`: the
/// Python's refusal text, with `top`'s bytes kept. `git worktree add` copies a sparse
/// checkout into a run's worktrees, so this holds for every run. `core.sparseCheckout` is read
/// as `top`'s own worktree sees it; a value Git does not read as `true` is not sparse
/// (`gitops.is_sparse`).
pub fn sparse_problem(top: &Path) -> Result<Option<OsString>, Failure> {
    let args = ["config", "--bool", "--get", "core.sparseCheckout"].map(OsStr::new);
    let out = Git::try_at(top)?.try_output(&args)?;
    let sparse = out.status.success() && out.stdout.trim_ascii() == b"true";
    Ok(sparse.then(|| {
        let mut text = top.as_os_str().to_os_string();
        text.push(
            " is a sparse checkout (core.sparseCheckout): sparse checkouts are not supported \
             in this version; turn it off (git sparse-checkout disable) to run leadhand here",
        );
        text
    }))
}

/// One path from `git rev-parse <args>`: its output without the newline Git ends it with.
/// rev-parse has no `-z`, so one path per call keeps a path that holds a newline whole.
fn rev_parse_path(git: &Git, args: &[&str]) -> Result<PathBuf, Failure> {
    let argv: Vec<&OsStr> = std::iter::once("rev-parse")
        .chain(args.iter().copied())
        .map(OsStr::new)
        .collect();
    let out = run(git, &argv)?;
    let path = out.strip_suffix(b"\n").unwrap_or(&out);
    if path.is_empty() {
        let head = format!("git rev-parse {}: no path (in ", args.join(" "));
        return Err(in_cwd(&head, git, ")"));
    }
    Ok(PathBuf::from(OsStr::from_bytes(path)))
}

/// The main working tree's top: the first entry of `git worktree list --porcelain -z`.
///
/// Fail-closed: for a linked worktree of a submodule or of a `--separate-git-dir` repository,
/// Git can list the Git directory itself as the main working tree. A listed path inside
/// `common_dir` is refused, so no worktree is ever made, and no config read, from there.
fn main_worktree(git: &Git, common_dir: &Path) -> Result<PathBuf, Failure> {
    let out = run(
        git,
        &["worktree", "list", "--porcelain", "-z"].map(OsStr::new),
    )?;
    // Each entry is NUL-terminated fields, ended by an empty one; the main worktree is first.
    let first: Vec<&[u8]> = out
        .split(|b| *b == 0)
        .take_while(|f| !f.is_empty())
        .collect();
    let path = first.first().and_then(|f| f.strip_prefix(b"worktree "));
    match path {
        Some(_) if first.contains(&&b"bare"[..]) => Err(in_cwd(
            "",
            git,
            " belongs to a bare repository, which has no main working tree",
        )),
        Some(path) if !path.is_empty() => {
            let path = PathBuf::from(OsStr::from_bytes(path));
            if real_path(&path).starts_with(real_path(common_dir)) {
                return Err(Failure::of(&[
                    git.cwd().as_os_str(),
                    OsStr::new(": Git lists the main working tree as "),
                    path.as_os_str(),
                    OsStr::new(", inside the Git directory "),
                    common_dir.as_os_str(),
                    OsStr::new(
                        " (a submodule or a --separate-git-dir repository), so which checkout \
                         is the repository is unclear; run leadhand from the main working tree",
                    ),
                ]));
            }
            Ok(path)
        }
        _ => Err(in_cwd(
            "git worktree list: no main working tree listed (in ",
            git,
            ")",
        )),
    }
}

/// `git.run`, with a failure's text kept lossless.
fn run(git: &Git, args: &[&OsStr]) -> Result<Vec<u8>, Failure> {
    let out = git.try_output(args)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(git.failure_text(args, &out))
    }
}

/// `<head><git's directory><tail>`.
fn in_cwd(head: &str, git: &Git, tail: &str) -> Failure {
    Failure::of(&[OsStr::new(head), git.cwd().as_os_str(), OsStr::new(tail)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;

    fn real(p: &Path) -> PathBuf {
        p.canonicalize().unwrap()
    }

    #[test]
    fn repo_identity_from_subdirectory_is_top() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(&repo, &[("src/deep/x.txt", "x\n")], "deep");
        let want = RepoIdentity {
            top: real(&repo),
            common_dir: real(&repo.join(".git")),
        };
        for dir in [repo.clone(), repo.join("src"), repo.join("src/deep")] {
            assert_eq!(repo_identity(&dir).unwrap(), want, "{dir:?}");
        }
    }

    #[test]
    fn repo_identity_from_linked_worktree_is_main_top() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(&repo, &[("src/x.txt", "x\n")], "src");
        let want = RepoIdentity {
            top: real(&repo),
            common_dir: real(&repo.join(".git")),
        };
        // A run's worktree inside the repository, and a linked worktree elsewhere.
        let inside = repo.join(".worktrees/my slug");
        let outside = f.root().join("elsewhere/wt");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "run",
                inside.to_str().unwrap(),
            ],
        );
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                outside.to_str().unwrap(),
            ],
        );
        for dir in [&inside, &inside.join("src"), &outside, &outside.join("src")] {
            assert_eq!(repo_identity(dir).unwrap(), want, "{dir:?}");
        }
    }

    #[test]
    fn linked_worktree_names_the_worktree_and_its_admin_folder() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(&repo, &[("src/x.txt", "x\n")], "src");
        let inside = repo.join(".worktrees/my slug");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "run",
                inside.to_str().unwrap(),
            ],
        );
        for dir in [&inside, &inside.join("src")] {
            let wt = linked_worktree(dir).unwrap().unwrap();
            assert_eq!(wt.top, real(&inside), "{dir:?}");
            // Git names the admin folder after the worktree, its space made safe.
            assert_eq!(
                wt.git_dir.parent(),
                Some(&*real(&repo.join(".git/worktrees")))
            );
            let gitdir = std::fs::read(wt.git_dir.join("gitdir")).unwrap();
            assert_eq!(
                gitdir.trim_ascii_end(),
                real(&inside).join(".git").as_os_str().as_bytes()
            );
            assert_eq!(wt.repo, repo_identity(&repo).unwrap());
        }
        // The main working tree is not a linked worktree; outside a checkout is an error.
        assert_eq!(linked_worktree(&repo.join("src")).unwrap(), None);
        let plain = f.root().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(linked_worktree(&plain).is_err());
    }

    #[test]
    fn git_common_dir_matches_python() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(&repo, &[("src/x.txt", "x\n")], "src");
        let inside = repo.join(".worktrees/my slug");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "run",
                inside.to_str().unwrap(),
            ],
        );
        // `git.json`: the reference's `git_common_dir` of each folder, in the same layout
        // under a root written `{root}` (its parent `{parent}`), run with these sealed Git
        // variables.
        let root = f.root().to_str().unwrap();
        let parent = f.root().parent().unwrap().to_str().unwrap();
        let placeheld = |t: &str| t.replace(root, "{root}").replace(parent, "{parent}");
        let fill = |t: &str| t.replace("{root}", root).replace("{parent}", parent);
        let mut env: Vec<[String; 2]> = f
            .hermetic_env()
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| [k, v]))
            .map(|[k, v]| [k.into_string().unwrap(), placeheld(v.to_str().unwrap())])
            .collect();
        env.sort();
        let recorded = crate::wire::tests::reference_json("git.json");
        assert_eq!(
            recorded["env"],
            serde_json::json!([env]),
            "git.json: recorded with other Git variables"
        );
        let dirs = [
            repo.clone(),
            repo.join("src"),
            inside.clone(),
            inside.join("src"),
        ];
        let inputs: Vec<String> = dirs
            .iter()
            .map(|d| placeheld(d.to_str().unwrap()))
            .collect();
        let theirs: Vec<String> =
            crate::wire::tests::reference_for("git.json", "git_common_dir", &inputs);
        for (dir, out) in dirs.iter().zip(theirs) {
            let ours = git_common_dir(dir).unwrap();
            let out = fill(&out);
            assert!(!out.is_empty(), "{dir:?}");
            assert_eq!(ours.as_os_str().as_bytes(), out.as_bytes(), "{dir:?}");
            assert_eq!(ours, real(&repo.join(".git")));
        }
        // Outside a checkout, Git's own refusal.
        let plain = f.root().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(git_common_dir(&plain).is_err());
        assert!(git_common_dir(Path::new("relative")).is_err());
    }

    #[test]
    fn repo_identity_refuses_a_directory_outside_any_repository() {
        let f = Fixture::new();
        let plain = f.root().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let e = repo_identity(&plain).unwrap_err();
        assert!(e.to_string().contains("not a git repository"), "{e}");
        assert!(repo_identity(Path::new("relative")).is_err());
    }

    #[test]
    fn repo_identity_failure_keeps_the_path_bytes() {
        // A directory name that is not UTF-8 (Linux allows it; macOS refuses to create it).
        let f = Fixture::new();
        let odd = f.root().join(OsStr::from_bytes(b"plain-\xff"));
        if std::fs::create_dir(&odd).is_err() {
            return;
        }
        let e = repo_identity(&odd).unwrap_err();
        let text = crate::wire::ascii_json(&e.text);
        assert!(text.contains("plain-\\udcff"), "{text}");
        assert!(!text.contains("\\ufffd"), "{text}");
    }

    #[test]
    fn repo_identity_refuses_a_bare_repository_main() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let bare = f.root().join("bare.git");
        f.git(
            f.root(),
            &[
                "clone",
                "--quiet",
                "--bare",
                repo.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let wt = f.root().join("bare wt");
        f.git(
            &bare,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                wt.to_str().unwrap(),
            ],
        );
        let e = repo_identity(&wt).unwrap_err();
        assert!(e.to_string().contains("bare repository"), "{e}");
        assert!(repo_identity(&bare).is_err());
    }

    #[test]
    fn a_main_working_tree_listed_inside_the_git_directory_is_refused() {
        // A --separate-git-dir repository: from its own working tree it is found as usual; from
        // a linked worktree, Git lists the Git directory itself as the main working tree.
        let f = Fixture::new();
        let gitdir = f.root().join("separate gitdir");
        let checkout = f.root().join("checkout");
        f.git(
            f.root(),
            &[
                "init",
                "--quiet",
                "--initial-branch",
                "main",
                "--separate-git-dir",
                gitdir.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        f.commit(&checkout, &[("README.md", "hello\n")], "initial");
        let id = repo_identity(&checkout).unwrap();
        assert_eq!(id.top, real(&checkout));
        assert_eq!(id.common_dir, real(&gitdir));
        let linked = f.root().join("linked wt");
        f.git(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        let listed = f.git(&linked, &["worktree", "list", "--porcelain"]);
        let main = listed
            .lines()
            .next()
            .unwrap()
            .strip_prefix("worktree ")
            .unwrap();
        assert!(
            real(Path::new(main)).starts_with(real(&gitdir)),
            "Git lists {main} first"
        );
        for e in [
            repo_identity(&linked).unwrap_err(),
            linked_worktree(&linked).unwrap_err(),
        ] {
            assert!(e.to_string().contains("inside the Git directory"), "{e}");
        }
    }

    #[test]
    fn sparse_problem_is_the_python_refusal() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        assert_eq!(sparse_problem(&repo).unwrap(), None);
        f.git(
            &repo,
            &["sparse-checkout", "set", "--no-cone", "/README.md"],
        );
        assert_eq!(
            sparse_problem(&repo).unwrap(),
            Some(OsString::from(format!(
                "{} is a sparse checkout (core.sparseCheckout): sparse checkouts are not \
                 supported in this version; turn it off (git sparse-checkout disable) to run \
                 leadhand here",
                repo.display()
            )))
        );
        f.git(&repo, &["sparse-checkout", "disable"]);
        assert_eq!(sparse_problem(&repo).unwrap(), None);
        f.git(&repo, &["config", "core.sparseCheckout", "maybe"]);
        assert_eq!(
            sparse_problem(&repo).unwrap(),
            None,
            "not a boolean: Git says no"
        );
        assert!(sparse_problem(Path::new("relative")).is_err());
    }
}
