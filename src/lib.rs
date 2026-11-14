#![deny(unsafe_code)]
//! leadhand: carries an approved deck through implementation and review with bounded workers.
//! The binary (`src/main.rs`) is `cli::main`.

pub mod cli;
pub mod error;
pub mod testing;

pub use error::{Error, Result};
