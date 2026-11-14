//! The run engine (`engine.py`): starting a run, and (Tasks 14–17) the card loop that `next`
//! and `record` drive.
//!
//! - `start`: the run directory and its records, the run branch and the run's worktree.

pub mod start;

pub use start::{StartResult, start};
