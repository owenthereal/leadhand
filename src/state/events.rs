//! The event log, `events.jsonl`: one typed struct per event kind (`state.EVENT_KINDS`, with
//! the fields `docs/cli-contract.md` lists), plus the branch release's two kinds.
//!
//! Events are decoded tag-first (Global Constraints, "The wire representation"): each line is
//! read once for its `kind` alone (`wire::event_kind`), then deserialized directly into that
//! kind's own struct. No `#[serde(tag)]`, `#[serde(flatten)]` or untagged enum is involved, and
//! no `Value`: a tagged enum would refuse a lone surrogate (`\udcff`, an undecodable path
//! byte) in any field before the field's own `PyStr` decoder ran. Each struct carries its own
//! `seq`, `ts` and `kind` (a marker type that writes the kind's name), so an event is written
//! by serializing its struct, in the Python's field order: `seq`, `ts`, `kind`, then the rest.
//!
//! Reading is tolerant as the Python's `dict.get` is: every field may be missing (a struct's
//! `Default` fills it in), and fields this version does not know are skipped unvalidated.
//! Paths, ref and branch names, worker and supervisor text are `PyStr`; identifiers (dispatch
//! IDs, SHAs, run IDs), timestamps and closed enumerations are `String`. A field the Python
//! writes only in some cases is left out when it is `None`; a field it always writes is
//! written `null`.

use std::fmt;

use serde::de::{self, IgnoredAny, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::paths::RunPaths;
use crate::error::{Error, Result};
use crate::pytext::py_isspace;
use crate::store;
use crate::wire::{self, PyMap, PyStr, RawLine};

/// A card as an event names it: a deck card's number, or text (`final`, the card of the
/// final fix waves). Written back as it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardRef {
    Number(i64),
    Text(PyStr),
}

impl CardRef {
    /// The card number, as `state._card_number` reads it (`int(value)`): a number as it is,
    /// text that is a decimal integer (Python's whitespace around it, a sign, `_` between
    /// digits), else `None`.
    pub fn number(&self) -> Option<i64> {
        match self {
            CardRef::Number(n) => Some(*n),
            CardRef::Text(text) => std::str::from_utf8(text.as_wtf8()).ok().and_then(py_int),
        }
    }

    /// `str(card)`, as an f-string shows it.
    pub fn text(&self) -> PyStr {
        match self {
            CardRef::Number(n) => PyStr::from(n.to_string()),
            CardRef::Text(text) => text.clone(),
        }
    }
}

/// `int(text)` for ASCII decimal text; `None` where Python raises.
fn py_int(text: &str) -> Option<i64> {
    let text = text.trim_matches(py_isspace);
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    let well_formed = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'_');
    if !well_formed {
        return None;
    }
    text.replace('_', "").parse().ok()
}

impl Serialize for CardRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            CardRef::Number(n) => serializer.serialize_i64(*n),
            CardRef::Text(text) => text.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CardRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct CardVisitor;

        impl Visitor<'_> for CardVisitor {
            type Value = CardRef;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a card number or name")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<CardRef, E> {
                Ok(CardRef::Number(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<CardRef, E> {
                i64::try_from(v)
                    .map(CardRef::Number)
                    .map_err(|_| E::custom("card number out of range"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<CardRef, E> {
                Ok(CardRef::Text(PyStr::from(v)))
            }
        }

        deserializer.deserialize_any(CardVisitor)
    }
}

/// A ref a worker moved: its tips before and after (`null` for a ref that did not exist).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefMove {
    pub old: Option<String>,
    pub new: Option<String>,
}

/// One struct per event kind, the `kind` marker types, the `Event` enum over them and the
/// tag-first decoder, from one list.
macro_rules! event_kinds {
    ($($variant:ident = $kind:literal,)*) => {
        /// The event kinds, in the Python's order (`state.EVENT_KINDS`), then the branch
        /// release's two (spec, "The branch release").
        pub const EVENT_KINDS: &[&str] = &[$($kind),*];

        /// Each event kind's `kind` field: written as the kind's name, and read only when it
        /// is that name.
        pub mod tag {
            use serde::{Deserialize, Deserializer, Serialize, Serializer};
            $(
                #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
                pub struct $variant;

                impl Serialize for $variant {
                    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                        s.serialize_str($kind)
                    }
                }

                impl<'de> Deserialize<'de> for $variant {
                    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                        let kind = String::deserialize(d)?;
                        if kind == $kind {
                            Ok($variant)
                        } else {
                            Err(serde::de::Error::custom(format!(
                                "kind {kind:?} is not {:?}", $kind
                            )))
                        }
                    }
                }
            )*
        }

        /// One event, of any kind.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Event {
            $($variant($variant),)*
        }

        impl Event {
            /// The event's kind, as written in its `kind` field.
            pub fn kind(&self) -> &'static str {
                match self {
                    $(Event::$variant(_) => $kind,)*
                }
            }

            /// Its `seq` (0 when the line had none, as `e.get("seq", 0)`).
            pub fn seq(&self) -> u64 {
                match self {
                    $(Event::$variant(e) => e.seq,)*
                }
            }

            pub fn ts(&self) -> &str {
                match self {
                    $(Event::$variant(e) => &e.ts,)*
                }
            }

            /// Sets its `seq` and `ts`: `append_event` assigns both under the state lock.
            pub(crate) fn stamp(&mut self, seq: u64, ts: String) {
                match self {
                    $(Event::$variant(e) => {
                        e.seq = seq;
                        e.ts = ts;
                    })*
                }
            }
        }

        impl Serialize for Event {
            /// The variant's own struct: the event as the Python writes it.
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                match self {
                    $(Event::$variant(e) => e.serialize(s),)*
                }
            }
        }

        /// The line `text` as the event `kind` names; `Ok(None)` for a kind that is not one.
        fn decode_as(kind: &str, text: &str) -> serde_json::Result<Option<Event>> {
            match kind {
                $($kind => serde_json::from_str::<$variant>(text).map(|e| Some(Event::$variant(e))),)*
                _ => Ok(None),
            }
        }
    };
}

event_kinds! {
    RunStarted = "run_started",
    CardCut = "card_cut",
    BriefWritten = "brief_written",
    DispatchRequested = "dispatch_requested",
    WorkerStarted = "worker_started",
    ResultRecorded = "result_recorded",
    VerdictRecorded = "verdict_recorded",
    FixRound = "fix_round",
    DiffChecked = "diff_checked",
    IntegrationStarted = "integration_started",
    Rebased = "rebased",
    Conflict = "conflict",
    AcceptStarted = "accept_started",
    Accepted = "accepted",
    Parked = "parked",
    Unparked = "unparked",
    Ruling = "ruling",
    Violation = "violation",
    GateStarted = "gate_started",
    GateResult = "gate_result",
    Outcome = "outcome",
    ReportWritten = "report_written",
    Paused = "paused",
    Cancelled = "cancelled",
    Resumed = "resumed",
    Reconciled = "reconciled",
    BaseWorktreeAdded = "base_worktree_added",
    BaseWorktreeReady = "base_worktree_ready",
    BranchReleaseStarted = "branch_release_started",
    BranchReleased = "branch_released",
}

/// An event struct: `seq`, `ts` and its `kind` marker, then its own fields.
macro_rules! event {
    ($(#[$meta:meta])* $name:ident { $($(#[$fmeta:meta])* $field:ident: $ty:ty,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        pub struct $name {
            pub seq: u64,
            pub ts: String,
            pub kind: tag::$name,
            $($(#[$fmeta])* pub $field: $ty,)*
        }
    };
}

/// A field the Python writes only in some cases: `None` when the line has no such field,
/// `Some(None)` for `null`. Read with `wire::present`, and left out when `None`, so the field's
/// presence survives a round trip (the Python tests it: `"refs" in d.req`).
pub type Opt<T> = Option<Option<T>>;

/// The value of an `Opt` field, whether it was missing or `null`.
pub fn given<T>(field: &Opt<T>) -> Option<&T> {
    field.as_ref().and_then(Option::as_ref)
}

event!(RunStarted {
    run_id: Option<String>,
    base_sha: Option<String>,
    branch: Option<PyStr>,
});

event!(CardCut {
    card: Option<CardRef>,
    branch: Option<PyStr>,
    base: Option<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    wave: Opt<i64>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    recut_after: Opt<String>,
});

event!(
    /// Declared in the Python's kind list; the reference never writes it.
    BriefWritten {}
);

event!(
    /// A worker about to be launched. The fields after `minutes` depend on the dispatch: a
    /// check's guard (`expected_branch`, `expected_head`, `refs`), an implementer's (`refs`,
    /// `ref_scope`, `start_tip`, `untracked`, `continues`), a run-level dispatch's (`gate`,
    /// `wave`).
    DispatchRequested {
        dispatch_id: Option<String>,
        card: Option<CardRef>,
        attempt: Option<i64>,
        dispatch_kind: Option<String>,
        role: Option<String>,
        vendor: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        brief: Option<PyStr>,
        report: Option<PyStr>,
        extra: Option<PyMap<Option<PyStr>>>,
        notes: Option<PyStr>,
        resumes: Option<String>,
        resume_worker: Option<PyStr>,
        resume_session: Option<PyStr>,
        minutes: Option<i64>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        gate: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        wave: Opt<i64>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        expected_branch: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        expected_head: Opt<String>,
        /// The refs the dispatch may not move, and their tips at the request.
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        refs: Opt<PyMap<Option<String>>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        ref_scope: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        start_tip: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        untracked: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        continues: Opt<String>,
    }
);

event!(WorkerStarted {
    dispatch_id: Option<String>,
    worker_id: Option<PyStr>,
    card: Option<CardRef>,
    deadline: Option<String>,
    session: Option<PyStr>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    reconciled: Opt<bool>,
});

event!(
    /// A dispatch's result: the fields every result has, the report head (`status` to
    /// `raw_kept`, then `interrupted` or `cross_checked`), the partial-work fields, and a
    /// Codex runner's commit fields.
    ResultRecorded {
        dispatch_id: Option<String>,
        card: Option<CardRef>,
        attempt: Option<i64>,
        dispatch_kind: Option<String>,
        worker_id: Option<PyStr>,
        tip: Option<String>,
        session_id: Option<PyStr>,
        /// The card folder's copies: `brief`, `report`, `reply` -> path.
        files: Option<PyMap<PyStr>>,
        stopped: Option<bool>,
        charge_ids: Option<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        reconciled: Opt<bool>,
        status: Option<String>,
        blocked_by: Option<String>,
        head: Option<String>,
        verdict: Option<String>,
        result: Option<String>,
        source: Option<String>,
        raw_kept: Option<bool>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        interrupted: Opt<bool>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        cross_checked: Opt<bool>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        files_skipped: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        partial_commit: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        partial_commit_error: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        partial_gitlinks_skipped: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        committed: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        commit_error: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        commit_skipped: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        gitlinks_skipped: Opt<Vec<PyStr>>,
    }
);

event!(VerdictRecorded {
    card: Option<CardRef>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    wave: Opt<i64>,
    verdict: Option<String>,
    findings: Option<PyStr>,
    dispatch_id: Option<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    reason: Opt<PyStr>,
});

event!(FixRound {
    card: Option<CardRef>,
    n: Option<i64>,
    attempt: Option<i64>,
    pre_tip: Option<String>,
    findings: Option<PyStr>,
    review: Option<String>,
});

event!(DiffChecked {
    card: Option<CardRef>,
    dispatch_id: Option<String>,
    ok: Option<bool>,
    note: Option<PyStr>,
});

event!(IntegrationStarted {
    card: Option<CardRef>,
    onto: Option<String>,
    tip: Option<String>,
    /// The `integration/<card>-<k>` ref that keeps the card's tip.
    keep: Option<PyStr>,
});

event!(Rebased {
    card: Option<CardRef>,
    onto: Option<String>,
    tip: Option<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    dispatch_id: Opt<String>,
});

event!(Conflict {
    card: Option<CardRef>,
    onto: Option<String>,
    paths: Option<Vec<PyStr>>,
});

event!(AcceptStarted {
    card: Option<CardRef>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    wave: Opt<i64>,
    target: Option<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    dispatch_id: Opt<String>,
});

event!(Accepted {
    card: Option<CardRef>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    wave: Opt<i64>,
    sha: Option<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    dispatch_id: Opt<String>,
});

event!(Parked {
    card: Option<CardRef>,
    reason: Option<PyStr>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    cap_dispatch: Opt<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    cap_kind: Opt<String>,
    #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
    decision: Opt<bool>,
});

event!(Unparked {
    card: Option<CardRef>,
    note: Option<PyStr>,
});

event!(Ruling {
    card: Option<CardRef>,
    dispatch_id: Option<String>,
    text: Option<PyStr>,
});

event!(
    /// A violation `found`, then `restored`. Which fields a record has depends on its stage
    /// and on who it was by: every field but `stage`, `by`, `n` and `ref` is an `Opt`.
    Violation {
        stage: Option<String>,
        by: Option<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        dispatch_id: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        card: Opt<CardRef>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        dispatch_kind: Opt<String>,
        n: Option<i64>,
        r#ref: Option<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        base_worktree: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        base_head: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        paths: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        operation: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        head_moved: Opt<bool>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        index_dirty: Opt<bool>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        expected_head: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        expected_branch: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        after_dispatch: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        why: Opt<PyStr>,
        /// Each ref the worker moved, with its tips before and after.
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        refs: Opt<PyMap<RefMove>>,
        /// Each moved ref -> the ref that keeps the worker's tip.
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        preserved: Opt<PyMap<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        untracked: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        hidden: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        saved: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        siblings: Opt<Vec<PyStr>>,
    }
);

event!(GateStarted {
    gate: Option<String>,
    examined_sha: Option<String>,
    dispatch_id: Option<String>,
});

event!(GateResult {
    gate: Option<String>,
    examined_sha: Option<String>,
    passed: Option<bool>,
    findings: Option<PyStr>,
    dispatch_id: Option<String>,
});

event!(Outcome {
    outcome: Option<String>,
    qualifier: Option<String>,
    reasons: Option<Vec<PyStr>>,
    next_step: Option<String>,
    head: Option<String>,
});

event!(ReportWritten {
    report: Option<PyStr>,
    outcome: Option<String>,
    outcome_seq: Option<i64>,
});

event!(Paused { by: Option<String>, });

event!(Cancelled { by: Option<String>, });

event!(Resumed { run_id: Option<String>, });

event!(
    /// What reconciliation did, by `stage` (`cancel`, `partial_work`), or what a relaunch
    /// found (`resumed_seq`, `found`, `headline`): every field is an `Opt`.
    Reconciled {
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        stage: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        resumed_seq: Opt<i64>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        found: Opt<Vec<PyStr>>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        headline: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        dispatch_id: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        runner: Opt<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        branch: Opt<PyStr>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        head_before: Opt<String>,
    }
);

event!(BaseWorktreeAdded { path: Option<PyStr>, });

event!(BaseWorktreeReady { path: Option<PyStr>, });

event!(
    /// The branch release's intent (spec, "The branch release"). Task 16 writes it.
    BranchReleaseStarted {
        worktree: Option<PyStr>,
        branch: Option<PyStr>,
        sha: Option<String>,
    }
);

event!(
    /// The branch release's completion, or why it was skipped. Task 16 writes it.
    BranchReleased {
        worktree: Option<PyStr>,
        branch: Option<PyStr>,
        sha: Option<String>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        skipped: Opt<bool>,
        #[serde(deserialize_with = "wire::present", skip_serializing_if = "Option::is_none")]
        reason: Opt<PyStr>,
    }
);

impl Event {
    /// The event's `card`, for the kinds that name one.
    pub fn card(&self) -> Option<&CardRef> {
        match self {
            Event::CardCut(e) => e.card.as_ref(),
            Event::DispatchRequested(e) => e.card.as_ref(),
            Event::WorkerStarted(e) => e.card.as_ref(),
            Event::ResultRecorded(e) => e.card.as_ref(),
            Event::VerdictRecorded(e) => e.card.as_ref(),
            Event::FixRound(e) => e.card.as_ref(),
            Event::DiffChecked(e) => e.card.as_ref(),
            Event::IntegrationStarted(e) => e.card.as_ref(),
            Event::Rebased(e) => e.card.as_ref(),
            Event::Conflict(e) => e.card.as_ref(),
            Event::AcceptStarted(e) => e.card.as_ref(),
            Event::Accepted(e) => e.card.as_ref(),
            Event::Parked(e) => e.card.as_ref(),
            Event::Unparked(e) => e.card.as_ref(),
            Event::Ruling(e) => e.card.as_ref(),
            Event::Violation(e) => given(&e.card),
            _ => None,
        }
    }
}

/// One whole line of the event log, decoded tag-first: its `kind` alone, then the line as
/// that kind's struct. The line is kept as read.
pub fn decode_event(raw: RawLine) -> Result<(Event, RawLine)> {
    let kind = wire::event_kind(&raw).map_err(|e| corrupt(&e.message))?;
    match decode_as(&kind, raw.get()) {
        Ok(Some(event)) => Ok((event, raw)),
        Ok(None) => Err(corrupt(&format!(
            "unknown event kind: {}",
            crate::pytext::py_repr(&kind)
        ))),
        Err(e) => Err(corrupt(&format!("{kind} event: {e}"))),
    }
}

fn corrupt(why: &str) -> Error {
    Error::msg(format!("corrupt record in events.jsonl: {why}"))
}

/// Every whole event in the run's log, each with its line as read (`state.read_events`):
/// none when the run directory or the log is missing; an error for a line that is not an
/// event. Read under the state lock.
pub fn read_events(paths: &RunPaths) -> Result<Vec<(Event, RawLine)>> {
    let lines: Vec<(IgnoredAny, RawLine)> = store::read_jsonl(&paths.root, &paths.events)?;
    lines
        .into_iter()
        .map(|(_, raw)| decode_event(raw))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use crate::state::run_paths;
    use crate::wire::tests::python;

    fn run_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("run with spaces")
            .tempdir()
            .unwrap()
    }

    fn read(lines: &str) -> Result<Vec<(Event, RawLine)>> {
        let dir = run_dir();
        let paths = run_paths(dir.path());
        fs::write(&paths.events, lines).unwrap();
        read_events(&paths)
    }

    fn card_cut(event: &Event) -> &CardCut {
        match event {
            Event::CardCut(e) => e,
            other => panic!("not a card_cut: {other:?}"),
        }
    }

    // CATALOG: test_state.py::CrashPointTests::test_event_kinds_listed
    #[test]
    fn event_kinds_listed() {
        let python_kinds = [
            "run_started",
            "card_cut",
            "brief_written",
            "dispatch_requested",
            "worker_started",
            "result_recorded",
            "verdict_recorded",
            "fix_round",
            "diff_checked",
            "integration_started",
            "rebased",
            "conflict",
            "accept_started",
            "accepted",
            "parked",
            "unparked",
            "ruling",
            "violation",
            "gate_started",
            "gate_result",
            "outcome",
            "report_written",
            "paused",
            "cancelled",
            "resumed",
            "reconciled",
            "base_worktree_added",
            "base_worktree_ready",
        ];
        // The Python's kinds, in its order, then the branch release's two.
        assert_eq!(&EVENT_KINDS[..28], python_kinds);
        assert_eq!(
            &EVENT_KINDS[28..],
            ["branch_release_started", "branch_released"]
        );
        let mut unique = EVENT_KINDS.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), EVENT_KINDS.len());
        // And the reference's own list, read from it.
        let listed = python(
            "from leadhand import state\nprint('\\n'.join(state.EVENT_KINDS))\n",
            &[],
        );
        assert_eq!(
            String::from_utf8(listed)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            python_kinds
        );
        // Every kind decodes as itself, from a bare line.
        for kind in EVENT_KINDS {
            let line = format!(r#"{{"seq":1,"ts":"2026-11-10T00:00:00.000Z","kind":"{kind}"}}"#);
            let (event, raw) = decode_event(serde_json::from_str(&line).unwrap()).unwrap();
            assert_eq!(
                (event.kind(), event.seq(), raw.get()),
                (*kind, 1, line.as_str())
            );
        }
    }

    #[test]
    fn event_reader_accepts_surrogate_escape_with_kind_last() {
        let events = read(
            "{\"seq\":1,\"ts\":\"2026-11-10T00:00:00.000Z\",\"branch\":\"a\\udcffb\",\
             \"base\":\"0123\",\"card\":1,\"kind\":\"card_cut\"}\n",
        )
        .unwrap();
        let cut = card_cut(&events[0].0);
        assert_eq!(
            cut.branch.as_ref().unwrap().as_wtf8(),
            b"\x61\xed\xb3\xbf\x62"
        );
        assert_eq!(
            (cut.card.as_ref(), cut.base.as_deref()),
            (Some(&CardRef::Number(1)), Some("0123"))
        );
    }

    #[test]
    fn event_reader_accepts_surrogate_escape_with_kind_first() {
        let events = read(
            "{\"kind\":\"card_cut\",\"seq\":1,\"ts\":\"2026-11-10T00:00:00.000Z\",\
             \"card\":1,\"branch\":\"a\\udcffb\",\"base\":\"0123\"}\n",
        )
        .unwrap();
        let cut = card_cut(&events[0].0);
        assert_eq!(
            cut.branch.as_ref().unwrap().as_wtf8(),
            b"\x61\xed\xb3\xbf\x62"
        );
        // Written back in the Python's order: seq, ts, kind, then the fields.
        assert_eq!(
            wire::to_line(&events[0].0).unwrap(),
            "{\"seq\":1,\"ts\":\"2026-11-10T00:00:00.000Z\",\"kind\":\"card_cut\",\"card\":1,\
             \"branch\":\"a\\udcffb\",\"base\":\"0123\"}"
        );
    }

    #[test]
    fn event_reader_round_trips_python_written_events() {
        let dir = run_dir();
        let paths = run_paths(dir.path());
        // The Python reference appends events carrying a non-UTF-8 path, a non-ASCII one and
        // an astral one, in fields of several kinds (a list, a map key, a map value).
        let write = "import sys\nfrom pathlib import Path\nfrom leadhand import state\n\
                     paths = state.run_paths(Path(sys.argv[1]))\n\
                     odd = b'src/a\\xffb.txt'.decode('utf-8', 'surrogateescape')\n\
                     wide = 'src/caf\\u00e9 \\U0001f600.txt'\n\
                     state.append_event(paths, 'card_cut', card=1, branch='leadhand/r\\u00e9', base='b' * 40)\n\
                     state.append_event(paths, 'conflict', card=1, onto='c' * 40, paths=[odd, wide])\n\
                     state.append_event(paths, 'violation', stage='found', by='check', dispatch_id='c1-a1-review-1',\n\
                     \x20   card=1, dispatch_kind='review', n=1, ref='refs/leadhand/abc/violations/1',\n\
                     \x20   paths=[odd], head_moved=False, index_dirty=False, expected_head=None,\n\
                     \x20   expected_branch='leadhand/r', refs={'refs/heads/' + odd: {'old': None, 'new': 'd' * 40}},\n\
                     \x20   preserved={'refs/heads/' + odd: 'refs/leadhand/abc/violations/1-' + wide})\n\
                     state.append_event(paths, 'parked', card='final', reason=wide)\n";
        python(write, &[dir.path().as_os_str()]);
        let file = fs::read(&paths.events).unwrap();
        let events = read_events(&paths).unwrap();
        assert_eq!(
            events.iter().map(|(e, _)| e.kind()).collect::<Vec<_>>(),
            ["card_cut", "conflict", "violation", "parked"]
        );
        let lines: Vec<&[u8]> = file
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        for ((event, raw), line) in events.iter().zip(&lines) {
            // Kept as read, and written back byte for byte.
            assert_eq!(raw.get().as_bytes(), *line);
            assert_eq!(wire::to_line(event).unwrap().as_bytes(), *line, "{event:?}");
        }
        let odd = PyStr::from_os(OsStr::from_bytes(b"src/a\xffb.txt"));
        match &events[1].0 {
            Event::Conflict(e) => assert_eq!(e.paths.as_ref().unwrap()[0], odd),
            other => panic!("{other:?}"),
        }
        match &events[2].0 {
            Event::Violation(v) => {
                let mut key = PyStr::from("refs/heads/");
                key.push(&odd);
                let moved = given(&v.refs).unwrap().get(&key).unwrap();
                assert_eq!(
                    (moved.old.as_deref(), moved.new.as_deref()),
                    (None, Some(&*"d".repeat(40)))
                );
                assert!(given(&v.preserved).unwrap().get(&key).is_some());
                // `expected_head=None` is a field written `null`, not a missing one.
                assert_eq!(v.expected_head, Some(None));
                assert_eq!(v.after_dispatch, None);
            }
            other => panic!("{other:?}"),
        }
        match &events[3].0 {
            Event::Parked(p) => {
                assert_eq!(p.card, Some(CardRef::Text(PyStr::from("final"))));
                assert_eq!(p.card.as_ref().unwrap().number(), None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn event_reader_rejects_unknown_kind() {
        let e = read(
            "{\"seq\":1,\"ts\":\"t\",\"kind\":\"card_cut\",\"card\":1}\n\
                      {\"seq\":2,\"ts\":\"t\",\"kind\":\"card_teleported\",\"card\":1}\n",
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "corrupt record in events.jsonl: unknown event kind: 'card_teleported'"
        );
        // A line with no kind is not an event either; a missing log is no events.
        assert!(read("{\"seq\":1,\"ts\":\"t\"}\n").is_err());
        let dir = run_dir();
        assert!(read_events(&run_paths(dir.path())).unwrap().is_empty());
    }

    #[test]
    fn card_numbers_read_as_python_int_reads_them() {
        let text = |s: &str| CardRef::Text(PyStr::from(s)).number();
        assert_eq!(CardRef::Number(3).number(), Some(3));
        for (s, want) in [
            ("3", Some(3)),
            (" 3\n", Some(3)),
            ("+3", Some(3)),
            ("-2", Some(-2)),
            ("1_0", Some(10)),
            ("final", None),
            ("", None),
            ("_1", None),
            ("1__0", None),
            ("3.0", None),
        ] {
            assert_eq!(text(s), want, "{s:?}");
        }
        assert_eq!(CardRef::Number(7).text(), PyStr::from("7"));
    }
}
