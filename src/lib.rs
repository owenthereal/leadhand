#![deny(unsafe_code)]
//! leadhand: carries an approved deck through implementation and review with bounded workers.
//! The binary (`src/main.rs`) is `cli::main`.

pub mod briefs;
pub mod briefs_write;
pub mod cli;
pub mod config;
pub mod deck;
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod git;
pub mod launch;
pub mod procs;
pub mod pytext;
pub mod report;
pub mod route;
pub mod runsel;
pub mod safedir;
pub mod state;
pub mod status;
pub mod store;
pub mod sys;
pub mod testing;
pub mod usage;
pub mod wire;

// The test harness's executable writer, for the library's own tests that write a file
// and run it (tests/common/executable.rs says why).
#[cfg(all(test, feature = "testing"))]
#[path = "../tests/common/executable.rs"]
mod test_executable;

pub use error::{Error, Result};
