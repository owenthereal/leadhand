//! The run directory's file names (`state.RunPaths`, `state.run_paths`; contract item 4).

use std::path::{Path, PathBuf};

use crate::pytext::pure_path;

/// The files of one run directory, named as the Python names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPaths {
    pub root: PathBuf,
    pub deck: PathBuf,
    pub run_json: PathBuf,
    pub events: PathBuf,
    pub status: PathBuf,
    pub attempts: PathBuf,
    pub charges: PathBuf,
    pub state_lock: PathBuf,
    pub report: PathBuf,
    pub handoff: PathBuf,
    pub lock: PathBuf,
    pub holder: PathBuf,
    pub pause_flag: PathBuf,
    pub cancel_flag: PathBuf,
    pub cards_dir: PathBuf,
    pub workers_dir: PathBuf,
}

/// The files of the run directory `root`, spelled as `pathlib` spells them
/// (`Path(root) / name`): `root` normalized as `PurePath` normalizes it, and a `.` root
/// dropped, so `run_paths(".").report` is `report.md`, as the Python prints it.
pub fn run_paths(root: &Path) -> RunPaths {
    let root = pure_path(root.as_os_str());
    let join = |name: &str| {
        if root == Path::new(".") {
            PathBuf::from(name)
        } else {
            root.join(name)
        }
    };
    RunPaths {
        deck: join("deck.md"),
        run_json: join("run.json"),
        events: join("events.jsonl"),
        status: join("status.json"),
        attempts: join("attempts.jsonl"),
        charges: join("charges.jsonl"),
        state_lock: join("state.lock"),
        report: join("report.md"),
        handoff: join("handoff.md"),
        lock: join("run.lock"),
        holder: join("holder.json"),
        pause_flag: join("pause"),
        cancel_flag: join("cancel"),
        cards_dir: join("cards"),
        workers_dir: join("workers"),
        root,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CATALOG: test_state.py::RunPathsTests::test_run_paths_names
    #[test]
    fn run_paths_names() {
        let root = Path::new("/tmp/run with spaces");
        let p = run_paths(root);
        assert_eq!(p.root, root);
        assert_eq!(p.deck, root.join("deck.md"));
        assert_eq!(p.run_json, root.join("run.json"));
        assert_eq!(p.events, root.join("events.jsonl"));
        assert_eq!(p.status, root.join("status.json"));
        assert_eq!(p.attempts, root.join("attempts.jsonl"));
        assert_eq!(p.charges, root.join("charges.jsonl"));
        assert_eq!(p.state_lock, root.join("state.lock"));
        assert_eq!(p.report, root.join("report.md"));
        assert_eq!(p.handoff, root.join("handoff.md"));
        assert_eq!(p.lock, root.join("run.lock"));
        assert_eq!(p.holder, root.join("holder.json"));
        assert_eq!(p.pause_flag, root.join("pause"));
        assert_eq!(p.cancel_flag, root.join("cancel"));
        assert_eq!(p.cards_dir, root.join("cards"));
        assert_eq!(p.workers_dir, root.join("workers"));
    }

    /// Every field, in `RunPaths` order, as text.
    fn fields(p: &RunPaths) -> Vec<String> {
        [
            &p.root,
            &p.deck,
            &p.run_json,
            &p.events,
            &p.status,
            &p.attempts,
            &p.charges,
            &p.state_lock,
            &p.report,
            &p.handoff,
            &p.lock,
            &p.holder,
            &p.pause_flag,
            &p.cancel_flag,
            &p.cards_dir,
            &p.workers_dir,
        ]
        .iter()
        .map(|f| f.to_str().unwrap().to_string())
        .collect()
    }

    #[test]
    fn run_paths_are_spelled_as_python_spells_them() {
        let roots = [".", "", "./run dir/", "a//b/./c", "/runs/r 1", "//r"];
        let code = "import json, sys\nfrom dataclasses import fields\nfrom leadhand import state\n\
                    print(json.dumps([[str(getattr(state.run_paths(r), f.name)) \
                    for f in fields(state.RunPaths)] for r in sys.argv[1:]]))\n";
        let args: Vec<&std::ffi::OsStr> = roots.iter().map(std::ffi::OsStr::new).collect();
        let theirs: Vec<Vec<String>> =
            serde_json::from_slice(&crate::wire::tests::python(code, &args)).unwrap();
        for (root, want) in roots.iter().zip(theirs) {
            assert_eq!(fields(&run_paths(Path::new(root))), want, "{root:?}");
        }
        assert_eq!(run_paths(Path::new(".")).report, Path::new("report.md"));
        assert_eq!(run_paths(Path::new("")).deck, Path::new("deck.md"));
    }
}
