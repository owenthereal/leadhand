//! `start` in Rust: in-place runs refused and the repository taken from the main working tree
//! (deliberate change 1), the run record in the run worktree's admin folder (spec, "Which
//! repository and run a directory belongs to"), and the conventions for the commands a person
//! types (`--json`, the fix command alone on the last line). Rust-only.
//!
//! The Rust form of the Python contract suite's `contract_rust_start`. `start` keeps its contract
//! output: `run: <dir>` for a new run, or its note naming the existing unfinished run. The
//! reference-only test `contract_layouts.InPlace` pinned the Python's acceptance of
//! `run.worktree = false`, and was not converted.

#[macro_use]
mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use common::engine::run_line;
use common::git::{branches, run_git};
use common::supervisor::{FakeSupervisor, Options, card};
use common::{Out, lines, path_str as s, read_json, realpath};

const NOT_SUPPORTED: &str = "not supported in this version: ";
const RUN_RECORD: &str = "leadhand-run-dir";
const BASE_MARKER: &str = "leadhand-run";

/// The linked worktree's admin folder in the common directory (`<common>/worktrees/<name>`).
#[track_caller]
fn admin_dir(ws: &Path) -> PathBuf {
    PathBuf::from(run_git(ws, &["rev-parse", "--absolute-git-dir"]))
}

/// `start` the world's deck in `repo` (default the world's repo) with `flags`.
#[track_caller]
fn start(w: &FakeSupervisor, flags: &[&str], repo: Option<&Path>) -> Out {
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("start"),
        w.deck_path.as_os_str(),
        OsStr::new("--repo"),
        repo.unwrap_or(&w.repo).as_os_str(),
    ];
    args.extend(flags.iter().map(OsStr::new));
    w.cli(&args)
}

/// `start` created nothing: no runs folder, no branch, no worktree (`assert_nothing_created`).
macro_rules! assert_nothing_created {
    ($w:expr, $before:expr) => {{
        let w: &FakeSupervisor = &$w;
        let before: &[String] = &$before;
        assert!(!w.docs.join("runs").exists());
        assert_eq!(branches(&w.repo), before);
        assert!(!w.repo.join(".worktrees").exists());
        assert_eq!(
            run_git(&w.repo, &["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
    }};
}

mod start {
    use super::*;
    use crate::common::git::commit;
    use crate::common::{TIMEOUT, cli, run_argv, shlex_quote, shlex_split};

    #[test]
    fn test_start_refuses_run_worktree_false() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                config: "[run]\nworktree = false\n",
                ..Options::default()
            },
        );
        let before = branches(&w.repo);
        let r = start(&w, &[], None);
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
        // The first line is the error, worded as every other unsupported setting is.
        let lines = lines(&r.stderr_text());
        assert!(lines[0].starts_with("error: "), "{r}");
        assert!(
            lines[0].contains(&format!("{NOT_SUPPORTED}run.worktree = false")),
            "{r}"
        );
        // No single command fixes a setting: nothing follows the error.
        assert_eq!(lines.len(), 1, "{r}");
        assert_nothing_created!(w, before);
    }

    #[test]
    fn test_start_repo_subdirectory_resolves_to_top() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        commit(&w.repo, &[("src/deep/a.txt", "a\n")], "src");
        fs::write(
            &w.config_file,
            format!(
                "docs_dir = \"{}\"\n",
                s(&w.tmp.join("docs dir").join("{repo}"))
            ),
        )
        .unwrap();
        let top = realpath(&w.repo);
        let r = start(&w, &[], Some(&w.repo.join("src/deep"))).ok();
        let run_dir = run_line(&r);
        assert!(
            lines(&r.stdout_text()).contains(&format!("run: {}", s(&run_dir))),
            "{r}"
        );
        // {repo} is the top's folder name, and the run belongs to the top.
        assert_eq!(
            run_dir.parent().unwrap(),
            w.docs.join(top.file_name().unwrap()).join("runs")
        );
        let meta = read_json(&run_dir.join("run.json"));
        assert_eq!(meta["repo"].as_os(), Some(top.clone().into_os_string()));
        // The run's worktree sits at the top's .worktrees/, never under the subdirectory.
        let ws = PathBuf::from(meta["workspace"].as_os().unwrap());
        assert_eq!(
            ws,
            top.join(".worktrees").join(run_dir.file_name().unwrap())
        );
        assert_eq!(
            run_git(&ws, &["symbolic-ref", "--short", "HEAD"]),
            meta["branch"].as_str().unwrap()
        );
        assert!(!w.repo.join("src/.worktrees").exists());
        assert!(!w.repo.join("src/deep/.worktrees").exists());
        // The same deck from the top is the same run.
        let again = start(&w, &[], None).ok();
        assert!(
            lines(&again.stdout_text()).contains(&format!("existing run: {}", s(&run_dir))),
            "{again}"
        );
    }

    #[test]
    fn test_start_repo_linked_worktree_resolves_to_main_working_tree() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        let linked = w.tmp.join("elsewhere/linked wt");
        run_git(
            &w.repo,
            &["worktree", "add", "--quiet", "-b", "side", s(&linked)],
        );
        // Config comes from the main working tree, never from a worktree's copy: this one
        // would be refused.
        fs::write(
            linked.join(".leadhand.toml"),
            "[after_run]\nstep = \"pr\"\n",
        )
        .unwrap();
        fs::write(
            &w.config_file,
            format!(
                "docs_dir = \"{}\"\n",
                s(&w.tmp.join("docs dir").join("{repo}"))
            ),
        )
        .unwrap();
        let top = realpath(&w.repo);
        let r = start(&w, &[], Some(&linked)).ok();
        assert!(!r.stderr_text().contains(NOT_SUPPORTED), "{r}");
        let run_dir = run_line(&r);
        assert_eq!(
            run_dir.parent().unwrap(),
            w.docs.join(top.file_name().unwrap()).join("runs")
        );
        let meta = read_json(&run_dir.join("run.json"));
        assert_eq!(meta["repo"].as_os(), Some(top.clone().into_os_string()));
        assert_eq!(
            PathBuf::from(meta["workspace"].as_os().unwrap()),
            top.join(".worktrees").join(run_dir.file_name().unwrap())
        );
        assert!(!linked.join(".worktrees").exists());
        assert!(!w.tmp.join("elsewhere/.worktrees").exists());
        // The excludes go to the common directory's info/exclude.
        let exclude = lines(&fs::read_to_string(w.repo.join(".git/info/exclude")).unwrap());
        let count = |line: &str| exclude.iter().filter(|l| *l == line).count();
        assert_eq!((count(".worktrees/"), count(".leadhand/")), (1, 1));
        // From a subdirectory of the linked worktree too: the same run.
        fs::create_dir(linked.join("sub dir")).unwrap();
        let again = start(&w, &[], Some(&linked.join("sub dir"))).ok();
        assert!(
            lines(&again.stdout_text()).contains(&format!("existing run: {}", s(&run_dir))),
            "{again}"
        );
    }

    #[test]
    fn test_worktree_records_run_dir_separately_from_leadhand_run() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        let first = w.start();
        let second = w.start_new();
        assert_ne!(first, second);
        for run_dir in [&first, &second] {
            let ws = PathBuf::from(
                read_json(&run_dir.join("run.json"))["workspace"]
                    .as_os()
                    .unwrap(),
            );
            let admin = admin_dir(&ws);
            assert_eq!(
                admin.parent().unwrap(),
                realpath(&w.repo.join(".git/worktrees"))
            );
            // The run directory, in its own file beside Git's: the path's bytes and a newline.
            let mut want = run_dir.as_os_str().as_bytes().to_vec();
            want.push(b'\n');
            assert_eq!(fs::read(admin.join(RUN_RECORD)).unwrap(), want);
            // The ownership marker (the evidence base worktree's, holding a run ID) is another
            // file, and the run worktree has none.
            assert!(!admin.join(BASE_MARKER).exists());
            assert!(admin.join("gitdir").is_file());
        }
        // Nothing in the operator's own .git folder or the worktree itself.
        assert!(!w.repo.join(".git").join(RUN_RECORD).exists());
        let ws = PathBuf::from(
            read_json(&first.join("run.json"))["workspace"]
                .as_os()
                .unwrap(),
        );
        assert!(!ws.join(RUN_RECORD).exists());
    }

    #[test]
    fn test_status_inside_worktree_uses_record_after_docs_dir_changes() {
        if common::rust_only() {
            return;
        }
        let mut w = FakeSupervisor::new(&[crate::common::supervisor::card_with(
            1,
            crate::common::supervisor::CardOpts {
                title: Some("Recorded run"),
                ..Default::default()
            },
        )]);
        let run_dir = w.start();
        let ws = w.workspace();
        let named = cli(&os_args!["status", run_dir], &w.env, &w.tmp).ok();
        // docs_dir moves after the start: a scan of the runs under it finds nothing, so only
        // the record leads from the worktree to the run.
        fs::write(
            &w.config_file,
            format!("docs_dir = \"{}\"\n", s(&w.tmp.join("other docs"))),
        )
        .unwrap();
        let deep = ws.join("deep dir");
        fs::create_dir(&deep).unwrap();
        for cwd in [&ws, &deep] {
            let r = cli(&os_args!["status"], &w.env, cwd).ok();
            assert_eq!(r.stdout, named.stdout, "cwd={}: {r}", cwd.display());
            assert!(
                r.stdout_text().contains("Card 1: Recorded run · "),
                "cwd={}: {r}",
                cwd.display()
            );
        }
        // Without the record, the worktree's run is not found.
        let record = admin_dir(&ws).join(RUN_RECORD);
        fs::remove_file(&record).unwrap_or_else(|e| panic!("removing {}: {e}", record.display()));
        let r = cli(&os_args!["status"], &w.env, &ws);
        assert_eq!(r.code, 1, "{r}");
    }

    #[test]
    fn test_start_json() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        let r = start(&w, &["--json"], None).ok();
        assert_eq!(r.stdout_text().matches('\n').count(), 1, "{r}");
        let report = r.json();
        assert_eq!(
            common::key_set(&report),
            common::set_of(&["run", "existing", "note"]),
            "{r}"
        );
        let run_dir = PathBuf::from(report["run"].as_os().unwrap());
        assert_json_eq!(
            common::pick(&report, &["existing", "note"]),
            common::jv("[false, null]"),
            "{r}"
        );
        assert!(run_dir.join("run.json").is_file());
        // The same information as the text output: the existing run, with its note.
        let text = start(&w, &[], None).ok();
        let r = start(&w, &["--json"], None).ok();
        assert_json_eq!(
            r.json(),
            common::json::Json::object([
                ("run", s(&run_dir).into()),
                ("existing", true.into()),
                ("note", format!("existing run: {}", s(&run_dir)).into()),
            ]),
            "{r}"
        );
        assert_eq!(
            text.stdout_text(),
            format!("existing run: {}\n", s(&run_dir)),
            "{text}"
        );
        // A new run on a suffixed branch: its note too.
        let r = start(&w, &["--new", "--json"], None).ok();
        let fresh = r.json();
        assert_json_eq!(
            common::pick(&fresh, &["existing", "note"]),
            common::jv(
                r#"[false, "run branch: leadhand/demo-2 (leadhand/demo belongs to another run)"]"#
            ),
            "{r}"
        );
        let fresh_run = PathBuf::from(fresh["run"].as_os().unwrap());
        assert_eq!(
            read_json(&fresh_run.join("run.json"))["branch"],
            "leadhand/demo-2"
        );
    }

    #[test]
    fn test_start_sparse_refusal_ends_with_fix_command() {
        if common::rust_only() {
            return;
        }
        let w = FakeSupervisor::new(&[card(1)]);
        let before = branches(&w.repo);
        run_git(
            &w.repo,
            &["sparse-checkout", "set", "--no-cone", "/README.md"],
        );
        let r = start(&w, &[], None);
        assert_eq!(r.code, 1, "{r}");
        let lines = lines(&r.stderr_text());
        assert!(lines[0].starts_with("error: "), "{r}");
        let repo = realpath(&w.repo);
        assert!(
            lines[0].contains(&format!("{NOT_SUPPORTED}{} is a sparse checkout", s(&repo))),
            "{r}"
        );
        // The last line is the one command that turns it off, alone, and it works as printed.
        assert_eq!(
            lines[1..],
            [format!(
                "git -C {} sparse-checkout disable",
                shlex_quote(s(&repo))
            )],
            "{r}"
        );
        let words = shlex_split(lines.last().unwrap()).expect("shell words");
        let argv: Vec<&OsStr> = words.iter().map(OsStr::new).collect();
        let done = run_argv(&argv, &w.env, &w.tmp, None, TIMEOUT);
        assert_eq!(done.code, 0, "{done}");
        start(&w, &[], None).ok();
        assert_ne!(branches(&w.repo), before);
    }
}
