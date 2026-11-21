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
//! - `integrate`: rebasing a card onto the run tip, which the shell does for `decide`;
//! - `gates`: the end-of-run gates' requests, the fix waves' final branch and acceptance, and
//!   the evidence base worktree, which the shell does for `decide`;
//! - `finish`: the outcome, then the wrap-up (the handoff, the report, the `runs.md` line) and
//!   the `done` action;
//! - `release`: the branch release after a terminal outcome, and the report's branch line;
//! - `violations`: a worker's change to what its dispatch guards, and an implementer's
//!   leftovers, found, kept and undone for `decide`;
//! - `report_text`: what the engine reads out of a worker's report and reply text;
//! - `record`: a worker's start and result, a ruling, a park, a diff check;
//! - `recover`: reconciling a relaunched run, the operator's flags recorded, and a stopped or
//!   interrupted Claude worker's partial work committed (R42);
//! - `control`: the operator's `pause`, `cancel` and `unpark`;
//! - `timer`: waiting on a dispatch.

pub mod cards;
pub mod control;
pub mod finish;
pub mod gates;
pub mod integrate;
pub mod next;
pub mod protocol;
pub mod record;
pub mod recover;
pub mod release;
pub mod report_text;
pub mod shell;
pub mod start;
pub mod timer;
pub mod violations;

pub use control::{cancel, pause, unpark};
pub use next::{Decision, Observations, RunState, decide};
pub use protocol::{Action, RecordOutput};
pub use record::{RecordFields, record};
pub use recover::reconcile;
pub use shell::next_action;
pub use start::{StartResult, start};
pub use timer::{Timer, timer};
