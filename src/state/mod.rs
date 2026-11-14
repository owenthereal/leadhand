//! Run state (`state.py`): the run directory's files, the event log read into typed events,
//! each card's state folded from them, and the run's outcome.
//!
//! - `paths`: the run directory's file names (`RunPaths`);
//! - `events`: one typed struct per event kind, decoded tag-first (`read_events`);
//! - `fold`: each card's state (`fold_cards`);
//! - `outcome`: how a run ends (`decide_outcome`, `allowed_next_step`).
//!
//! Here too: the typed views of `run.json` and of `attempts.jsonl` that `status` and `report`
//! read, and the timestamp every record carries.

pub mod events;
pub mod fold;
pub mod outcome;
pub mod paths;

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use events::{CardRef, EVENT_KINDS, Event, decode_event, read_events};
pub use fold::{CARD_STATES, CardStage, CardState, fold_cards};
pub use outcome::{
    BLOCKED_QUALIFIERS, Blocked, Outcome, OutcomeDecision, OutcomeInput, allowed_next_step,
    decide_outcome,
};
pub use paths::{RunPaths, run_paths};

use crate::error::{Error, Result};
use crate::wire::{self, PyStr};

/// `run.json` (contract item 4), as `status`, `report` and run selection read it. Fields
/// whose absence and `null` the Python shows differently (`meta.get(key, default)`) keep
/// which it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunMeta {
    pub run_id: Option<String>,
    pub repo: Option<PyStr>,
    pub deck_source: Option<PyStr>,
    pub deck_digest: Option<String>,
    #[serde(deserialize_with = "wire::present")]
    pub base_ref: Option<Option<PyStr>>,
    pub base_sha: Option<String>,
    #[serde(deserialize_with = "wire::present")]
    pub branch: Option<Option<PyStr>>,
    pub branch_requested: Option<PyStr>,
    #[serde(deserialize_with = "wire::present")]
    pub workspace: Option<Option<PyStr>>,
    pub slug: Option<String>,
    pub created_at: Option<String>,
    pub supervisor_sessions: Option<Vec<PyStr>>,
}

/// The run's `run.json`: an error when it is missing, not UTF-8 or not a JSON object.
pub fn read_run_meta(paths: &RunPaths) -> Result<RunMeta> {
    let bytes = std::fs::read(&paths.run_json).map_err(|e| Error::io(&paths.run_json, e))?;
    let text =
        String::from_utf8(bytes).map_err(|e| Error::msg(format!("run.json is not UTF-8: {e}")))?;
    serde_json::from_str(&text).map_err(|e| Error::msg(format!("run.json: {e}")))
}

/// One attempt record (`attempts.jsonl`), with the fields `status` and `report` read. Task 8
/// adds the rest of the record when it writes them.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AttemptRecord {
    pub dispatch: Option<String>,
    pub card: Option<CardRef>,
    pub attempt: Option<i64>,
    pub role: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: Option<String>,
    pub usage_known: Option<bool>,
}

/// The time now, UTC, as every record carries it: `YYYY-MM-DDTHH:MM:SS.mmmZ`
/// (`state.utc_now`: the real clock, milliseconds truncated).
pub fn utc_now() -> String {
    iso_millis(SystemTime::now())
}

fn iso_millis(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn utc_now_is_python_utc_now_format() {
        let at = |secs: u64, micros: u64| {
            iso_millis(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_micros(micros))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        // Milliseconds are truncated, as isoformat(timespec="milliseconds") truncates.
        assert_eq!(at(1_767_225_599, 999_999), "2025-12-31T23:59:59.999Z");
        assert_eq!(at(951_782_400, 1_500), "2000-02-29T00:00:00.001Z");
        assert_eq!(at(1_794_268_800, 0), "2026-11-10T00:00:00.000Z");
        let now = utc_now();
        assert_eq!((now.len(), &now[10..11], &now[23..]), (24, "T", "Z"));
    }

    #[test]
    fn run_meta_keeps_null_apart_from_missing() {
        let meta: RunMeta =
            serde_json::from_str(r#"{"run_id": "abc123", "branch": null, "workspace": "/w"}"#)
                .unwrap();
        assert_eq!(meta.run_id.as_deref(), Some("abc123"));
        assert_eq!(meta.branch, Some(None));
        assert_eq!(meta.base_ref, None);
        assert_eq!(meta.workspace, Some(Some(PyStr::from("/w"))));
    }
}
