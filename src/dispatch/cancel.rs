//! Cancelling a dispatch (`dispatch.cancel`, `_cancel_locked`): request the stop, let the runner
//! stop Codex, or stop what a gone runner left, confirm everything is gone, then finalize.
//!
//! The request is the `cancel` file (with the requester's token and deadline), and SIGTERM to
//! the runner, sent only while its pid and start time match. A runner that does not acknowledge
//! it (`stopping.json`) within `CANCEL_ACK` is hung, and is killed, by identity too.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Signal, getpid};
use serde::Serialize;

use super::caller::py_name;
use super::commit::RunnerCommits;
use super::finalize::finalize;
use super::own::{owner_lock, runner_alive_in, runner_target};
use super::runner::{CANCEL_FINISH_MARGIN, Finish, POLL, StopReason};
use super::tracker::{Tracker, stop};
use super::{
    CANCEL, DispatchResult, RUNNER, STOPPING, load_result, read_json_object, read_request, wall_now,
};
use crate::error::{Error, Result};
use crate::procs;
use crate::state::utc_now;
use crate::store::write_json_atomic;
use crate::testing::crash_point;
use crate::wire::PyStr;

/// How long a cancel waits for everything to be gone (`CANCEL_TIMEOUT`).
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(30);
/// A runner that does not acknowledge a cancel this fast is hung.
pub const CANCEL_ACK: Duration = Duration::from_secs(5);

/// The `cancel` file, in the Python's key order.
#[derive(Serialize)]
struct CancelRequest {
    requested_at: String,
    by_pid: i32,
    token: Option<String>,
    deadline_wall: f64,
}

/// `cancel`: stop a dispatch, confirm that Codex and its processes are gone, finalize. A result
/// already there is returned as it is. An error after `timeout` naming what is still alive.
pub fn cancel(worker_dir: &Path, timeout: Duration) -> Result<DispatchResult> {
    let wd = worker_dir;
    let deadline = Instant::now() + timeout;
    if let Some(done) = load_result(wd)? {
        return Ok(done);
    }
    let _owner = owner_lock(wd, Some(deadline))?;
    cancel_locked(wd, deadline, Some(new_token()?))
}

/// `_cancel_locked`, `cancel`'s body; the caller holds `owner.lock`. Without a token, a cancel
/// request already there is kept.
pub(crate) fn cancel_locked(
    wd: &Path,
    deadline: Instant,
    token: Option<String>,
) -> Result<DispatchResult> {
    if let Some(done) = load_result(wd)? {
        return Ok(done);
    }
    let req = read_request(wd)?
        .ok_or_else(|| Error::msg(format!("no request.json in {}", wd.display())))?;
    if token.is_some() || !wd.join(CANCEL).exists() {
        let request = CancelRequest {
            requested_at: utc_now(),
            by_pid: getpid().as_raw_pid(),
            token,
            deadline_wall: wall_now() + seconds_until(deadline),
        };
        write_json_atomic(&wd.join(CANCEL), &request)?;
    }
    let mut target = runner_target(wd)?;
    if let Some(runner) = &target
        && runner_alive_in(wd)?
    {
        procs::signal_if_same(runner, Signal::TERM);
    }
    crash_point("after_cancel_request");
    let ack_by = Instant::now() + CANCEL_ACK;
    loop {
        if let Some(done) = load_result(wd)? {
            return Ok(done);
        }
        if !runner_alive_in(wd)? {
            break;
        }
        if !wd.join(STOPPING).exists() && Instant::now() >= ack_by {
            // Alive but not acknowledging: hung. Kill it (verified), then stop Codex here.
            target = runner_target(wd)?;
            if let Some(runner) = &target {
                procs::signal_if_same(runner, Signal::KILL);
            }
        }
        if Instant::now() >= deadline {
            let pid = target
                .as_ref()
                .map_or(String::new(), |t| format!(" (pid {})", t.pid));
            return Err(Error::msg(format!(
                "cancel timed out; the runner of {} is still alive{pid}",
                py_name(wd).to_string_lossy()
            )));
        }
        thread::sleep(POLL);
    }
    // The runner is gone: stop what it left, then finalize.
    let mut tracker = Tracker::load(wd)?;
    let left = stop(&mut tracker, Some(deadline));
    if !left.is_empty() {
        let named: Vec<String> = left
            .iter()
            .map(|p| format!("pid {} (group {})", p.pid, p.pgid))
            .collect();
        return Err(Error::msg(format!(
            "cancel timed out; still alive: {}",
            named.join(", ")
        )));
    }
    let started_at = read_json_object(&wd.join(RUNNER))?.and_then(|runner| {
        runner
            .get(&PyStr::from("started_at"))
            .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
    });
    let finish = Finish {
        stopped_for: Some(StopReason::Cancelled),
        codex_exit: None,
    };
    let margin = deadline
        .checked_sub(CANCEL_FINISH_MARGIN)
        .unwrap_or_else(Instant::now);
    finalize(
        wd,
        &req,
        finish,
        &tracker.stopped(),
        started_at.as_deref(),
        Some(margin),
        None,
        &RunnerCommits,
    )
}

/// Seconds from now until `deadline`, negative once it has passed.
fn seconds_until(deadline: Instant) -> f64 {
    let now = Instant::now();
    if deadline >= now {
        (deadline - now).as_secs_f64()
    } else {
        -(now - deadline).as_secs_f64()
    }
}

/// A fresh cancel token, as `uuid.uuid4().hex` makes one: 32 lowercase hex digits of a random
/// version-4 UUID.
pub(crate) fn new_token() -> Result<String> {
    let path = Path::new("/dev/urandom");
    let mut bytes = [0u8; 16];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| Error::io(path, e))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_uuid4_hex() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(t.len(), 32);
            assert!(
                t.bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
            assert_eq!(&t[12..13], "4");
            assert!("89ab".contains(&t[16..17]));
        }
    }

    #[test]
    fn seconds_until_is_negative_once_passed() {
        let now = Instant::now();
        assert!(seconds_until(now + Duration::from_secs(30)) > 29.0);
        assert!(seconds_until(now - Duration::from_secs(1)) < -0.9);
    }
}
