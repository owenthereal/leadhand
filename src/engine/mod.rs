//! The run engine (`engine.py`): starting a run, and the card loop that `next` and `record`
//! drive (Tasks 14–17).
//!
//! - `start`: the run directory and its records, the run branch and the run's worktree;
//! - `protocol`: the actions `next` prints and what `record` prints, the dispatch ID format
//!   and the deadlines;
//! - `cards`: the run as one call loads it, and the dispatches its log records;
//! - `next`: the pure `decide` core, which reads the run and its observations and names
//!   the next step;
//! - `shell`: the engine shell around it, which gathers what `decide` asks for and carries
//!   out its steps;
//! - `report_text`: what the engine reads out of a worker's report and reply text;
//! - `record`: a worker's start and result, a ruling, a park, a diff check;
//! - `timer`: waiting on a dispatch.

pub mod cards;
pub mod next;
pub mod protocol;
pub mod record;
pub mod report_text;
pub mod shell;
pub mod start;
pub mod timer;

pub use next::{Decision, Observations, RunState, decide};
pub use protocol::{Action, RecordOutput};
pub use record::{RecordFields, record};
pub use shell::next_action;
pub use start::{StartResult, start};
pub use timer::{Timer, timer};
