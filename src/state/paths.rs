//! The run directory's file names (`state.RunPaths`, `state.run_paths`; contract item 4).

use std::path::{Path, PathBuf};

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

/// The files of the run directory `root`, joined to it as given.
pub fn run_paths(root: &Path) -> RunPaths {
    RunPaths {
        root: root.to_path_buf(),
        deck: root.join("deck.md"),
        run_json: root.join("run.json"),
        events: root.join("events.jsonl"),
        status: root.join("status.json"),
        attempts: root.join("attempts.jsonl"),
        charges: root.join("charges.jsonl"),
        state_lock: root.join("state.lock"),
        report: root.join("report.md"),
        handoff: root.join("handoff.md"),
        lock: root.join("run.lock"),
        holder: root.join("holder.json"),
        pause_flag: root.join("pause"),
        cancel_flag: root.join("cancel"),
        cards_dir: root.join("cards"),
        workers_dir: root.join("workers"),
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
}
