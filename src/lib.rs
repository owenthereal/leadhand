#![deny(unsafe_code)]
//! leadhand: carries an approved deck through implementation and review with bounded workers.
//! The binary (`src/main.rs`) is `cli::main`.

pub mod cli;
pub mod config;
pub mod deck;
pub mod error;
pub mod git;
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

pub use error::{Error, Result};
