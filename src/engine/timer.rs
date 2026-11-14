//! `timer` (`engine.timer`): waits until a dispatch has a result, its deadline passes, or the
//! operator asks to pause or cancel the run, whichever comes first, and says which.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use super::cards::{Log, py_absolute};
use super::protocol::{TIMER_INTERVAL, now, parse_time};
use crate::dispatch::load_result;
use crate::error::{Error, Result};
use crate::state::run_paths;
use crate::wire::PyStr;

/// What the timer saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    /// The dispatch has a result, or its Codex runner finished (`next` records it).
    Result,
    /// The worker's deadline passed.
    Deadline,
    /// The operator asked to pause or cancel the run.
    Flag,
}

impl Timer {
    /// The line `timer` prints.
    pub fn line(self) -> &'static str {
        match self {
            Timer::Result => "result recorded",
            Timer::Deadline => "deadline passed",
            Timer::Flag => "pause or cancel requested",
        }
    }
}

/// `leadhand timer RUN_DIR --dispatch ID`: one line saying what ended the wait.
pub fn command(run_dir: &OsStr, dispatch_id: &OsStr) -> Result<ExitCode> {
    let seen = timer(Path::new(run_dir), &PyStr::from_os(dispatch_id))?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", seen.line())?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// Returns once the dispatch has a result, once its deadline passes, or as soon as the
/// operator asks to pause or cancel the run (`engine.timer`). It looks every
/// `TIMER_INTERVAL` seconds, and never sleeps past the worker's deadline.
pub fn timer(run_dir: &Path, dispatch_id: &PyStr) -> Result<Timer> {
    let paths = run_paths(&py_absolute(run_dir)?);
    if !paths.run_json.is_file() {
        return Err(Error::msg(format!(
            "not a run directory (no run.json): {}",
            paths.root.display()
        )));
    }
    let id = std::str::from_utf8(dispatch_id.as_wtf8()).ok();
    loop {
        let log = Log::read(&paths)?;
        let Some(d) = id.and_then(|id| log.dispatch(Some(id))) else {
            return Err(Error::msg(format!(
                "no dispatch {} was requested",
                dispatch_id.repr()
            )));
        };
        if d.result.is_some() {
            return Ok(Timer::Result);
        }
        if paths.cancel_flag.exists() || paths.pause_flag.exists() {
            return Ok(Timer::Flag);
        }
        let runner_done = d.vendor() == "codex"
            && d.started.is_some()
            && load_result(&paths.root.join("workers").join(d.id()))?.is_some();
        if runner_done {
            return Ok(Timer::Result);
        }
        let mut wait = TIMER_INTERVAL;
        if let Some(started) = d.started {
            let deadline = parse_time(started.deadline.as_deref().unwrap_or_default())?;
            let left = deadline - now();
            if left <= 0.0 {
                return Ok(Timer::Deadline);
            }
            wait = wait.min(left);
        }
        std::thread::sleep(Duration::from_secs_f64(wait));
    }
}
