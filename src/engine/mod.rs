//! The run engine (`engine.py`): starting a run, and the card loop that `next` and `record`
//! drive (Tasks 14–17).
//!
//! - `start`: the run directory and its records, the run branch and the run's worktree;
//! - `protocol`: the actions `next` prints and what `record` prints, the dispatch ID format
//!   and the deadlines;
//! - `cards`: the run as one call loads it, and the dispatches its log records;
//! - `next`: the pure `decide` core and the shell that carries out its steps;
//! - `record`: a worker's start and result, a ruling, a park, a diff check;
//! - `timer`: waiting on a dispatch.

pub mod cards;
pub mod next;
pub mod protocol;
pub mod record;
pub mod start;
pub mod timer;

pub use next::{Decision, Observations, RunState, decide, next_action};
pub use protocol::{Action, RecordOutput};
pub use record::{RecordFields, record};
pub use start::{StartResult, start};
pub use timer::{Timer, timer};
