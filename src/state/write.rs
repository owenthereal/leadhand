//! Writing run state (`state.append_event`, `state.append_attempt`), and the two-phase step
//! every side effect is wrapped in.
//!
//! An event's `seq` (the last event's plus one) and `ts` are assigned under the state lock,
//! from the log's last whole line, so a Rust and a Python writer never share a number. The
//! caller chooses neither, nor the `kind`: an `Event` is always one of `EVENT_KINDS`, and its
//! kind is its variant's.
//!
//! Side effects are written as intent and completion events around the act (spec, bug
//! class 7): `two_phase` appends the intent, passes the crash point before the act, acts,
//! passes the crash point after, then appends the completion. A crash between the two is
//! reconciled from Git and the run directory; the crash points are where the tests put it.

use serde::de::{IgnoredAny, MapAccess};
use serde_json::value::RawValue;

use super::events::Event;
use super::{AttemptRecord, RunPaths, utc_now};
use crate::error::{Error, Result};
use crate::store;
use crate::testing::crash_point;
use crate::usage::meter::{Lenient, deserialize_leniently};
use crate::wire::{PyStr, RawLine};

/// Appends `event` to the run's log with its `seq` and `ts` assigned under the state lock
/// (`state.append_event`); returns the event as written, and its line.
pub fn append_event(paths: &RunPaths, event: Event) -> Result<(Event, RawLine)> {
    let mut event = event;
    let line = store::append_jsonl_with(&paths.root, &paths.events, |last| {
        event.stamp(next_seq(last)?, utc_now());
        Ok(event.clone())
    })?;
    Ok((event, line))
}

/// Appends one attempt record to `attempts.jsonl` (`state.append_attempt`).
pub fn append_attempt(paths: &RunPaths, record: &AttemptRecord) -> Result<()> {
    store::append_jsonl(&paths.root, &paths.attempts, record).map(drop)
}

/// Every attempt record, in order (`state.read_attempts`); none when the run directory or
/// the file is missing.
pub fn read_attempts(paths: &RunPaths) -> Result<Vec<AttemptRecord>> {
    let lines: Vec<(AttemptRecord, RawLine)> = store::read_jsonl(&paths.root, &paths.attempts)?;
    Ok(lines.into_iter().map(|(a, _)| a).collect())
}

/// One two-phase step: appends `intent`, passes the crash point `point_before`, runs `act`,
/// passes `point_after`, then appends the completion `done` makes from what `act` returned.
/// An `act` that fails appends no completion. Returns what `act` returned.
pub fn two_phase<T>(
    paths: &RunPaths,
    intent: Event,
    point_before: &str,
    act: impl FnOnce() -> Result<T>,
    point_after: &str,
    done: impl FnOnce(&T) -> Event,
) -> Result<T> {
    append_event(paths, intent)?;
    crash_point(point_before);
    let out = act()?;
    crash_point(point_after);
    append_event(paths, done(&out))?;
    Ok(out)
}

/// The resume form of `two_phase`, for a step whose intent is already recorded (a crash came
/// between it and the completion): runs `act` again (it must be idempotent), passes
/// `point_after`, then appends the completion. The intent and the crash point before the act
/// are not repeated, as the Python's resume paths skip them (`_finish_accept` passes only
/// `after_fast_forward`).
pub fn two_phase_resume<T>(
    paths: &RunPaths,
    act: impl FnOnce() -> Result<T>,
    point_after: &str,
    done: impl FnOnce(&T) -> Event,
) -> Result<T> {
    let out = act()?;
    crash_point(point_after);
    append_event(paths, done(&out))?;
    Ok(out)
}

/// The `seq` after the log's last line `last` (1 for an empty log): it must have an integer
/// `seq`, as the Python requires (`isinstance(prev, int)`, a `bool` not counting).
fn next_seq(last: Option<&RawValue>) -> Result<u64> {
    let Some(last) = last else {
        return Ok(1);
    };
    let no_seq = || Error::msg("last event in events.jsonl has no integer seq");
    let LastSeq(SeqValue(prev)) = serde_json::from_str(last.get()).map_err(|_| no_seq())?;
    let prev = prev.ok_or_else(no_seq)?;
    u64::try_from(prev + 1)
        .map_err(|_| Error::msg(format!("last event in events.jsonl has seq {prev}")))
}

deserialize_leniently!(LastSeq, SeqValue);

/// The last line's `seq`, and only that: no other field is read (or validated). A repeated
/// key keeps its last value, as `json.loads` does.
#[derive(Default)]
struct LastSeq(SeqValue);

impl Lenient for LastSeq {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> std::result::Result<Self, A::Error> {
        let mut seq = SeqValue::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            if key.as_wtf8() == b"seq" {
                seq = map.next_value()?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(LastSeq(seq))
    }
}

/// A JSON integer (not a `bool` or a float), else `None`.
#[derive(Default)]
struct SeqValue(Option<i128>);

impl Lenient for SeqValue {
    fn int(v: i64) -> Self {
        SeqValue(Some(v.into()))
    }
    fn uint(v: u64) -> Self {
        SeqValue(Some(v.into()))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::state::events::{self, CardRef, EVENT_KINDS};
    use crate::state::{decode_event, read_events, run_paths};
    use crate::usage::{Tokens, Usage};
    use crate::wire::{self, tests::python};

    fn run_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("run with spaces")
            .tempdir()
            .unwrap()
    }

    fn ruling(text: &str) -> Event {
        Event::Ruling(events::Ruling {
            text: Some(text.into()),
            ..events::Ruling::default()
        })
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn append_event_assigns_seq_and_ts() {
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        let (first, raw) = append_event(&paths, ruling("one")).unwrap();
        assert_eq!(first.seq(), 1);
        let ts = first.ts().to_string();
        assert_eq!((ts.len(), &ts[10..11], &ts[23..]), (24, "T", "Z"));
        assert_eq!(
            raw.get(),
            format!(
                r#"{{"seq":1,"ts":"{ts}","kind":"ruling","card":null,"dispatch_id":null,"text":"one"}}"#
            )
        );
        let (second, _) = append_event(&paths, ruling("two")).unwrap();
        assert_eq!(second.seq(), 2);
        assert_eq!(lines(&paths.events)[0], raw.get());
        // After a Python writer, the next number is the Python's plus one.
        let code = "import sys\nfrom pathlib import Path\nfrom leadhand import state\n\
                    p = state.run_paths(Path(sys.argv[1]))\n\
                    state.append_event(p, 'paused', by='operator')\n\
                    print(len(state.read_events(p)))\n";
        assert_eq!(python(code, &[tmp.path().as_os_str()]), b"3\n");
        let (fourth, _) = append_event(&paths, ruling("four")).unwrap();
        assert_eq!(fourth.seq(), 4);
        let read: Vec<u64> = read_events(&paths)
            .unwrap()
            .iter()
            .map(|(e, _)| e.seq())
            .collect();
        assert_eq!(read, [1, 2, 3, 4]);
    }

    #[test]
    fn append_event_needs_an_integer_seq_on_the_last_line() {
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        for last in [
            r#"{"kind":"ruling"}"#,
            r#"{"seq":true,"kind":"ruling"}"#,
            r#"{"seq":2.0,"kind":"ruling"}"#,
            r#"{"seq":"2","kind":"ruling"}"#,
        ] {
            let before = format!("{last}\n");
            fs::write(&paths.events, &before).unwrap();
            let err = append_event(&paths, ruling("x")).unwrap_err();
            assert_eq!(
                err.message, "last event in events.jsonl has no integer seq",
                "{last}"
            );
            assert_eq!(fs::read_to_string(&paths.events).unwrap(), before);
        }
        // Only `seq` is read: a lone surrogate elsewhere, and a repeated `seq` (its last value
        // counts), are fine.
        fs::write(
            &paths.events,
            "{\"seq\":1,\"kind\":\"ruling\",\"text\":\"a\\udcffb\",\"seq\":41}\n",
        )
        .unwrap();
        assert_eq!(append_event(&paths, ruling("x")).unwrap().0.seq(), 42);
    }

    // CATALOG: test_state.py::AppendTests::test_unknown_event_kind_rejected
    #[test]
    fn unknown_event_kind_rejected() {
        // An unknown kind cannot be built: an `Event` is one of `EVENT_KINDS`, read only from a
        // line of a known kind, so nothing reaches `append_event` with another. Every kind it
        // can write, it writes as that kind, and the log reads it back as that kind.
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        let unknown = serde_json::from_str(r#"{"seq":1,"ts":"t","kind":"not_a_kind"}"#).unwrap();
        assert!(decode_event(unknown).is_err());
        assert!(!paths.events.exists());
        for (i, kind) in EVENT_KINDS.iter().enumerate() {
            let line = format!(r#"{{"kind":"{kind}"}}"#);
            let (event, _) = decode_event(serde_json::from_str(&line).unwrap()).unwrap();
            let (written, raw) = append_event(&paths, event).unwrap();
            assert_eq!((written.kind(), written.seq()), (*kind, i as u64 + 1));
            assert_eq!(wire::event_kind(&raw).unwrap(), *kind);
        }
        let read = read_events(&paths).unwrap();
        let kinds: Vec<&str> = read.iter().map(|(e, _)| e.kind()).collect();
        assert_eq!(kinds, EVENT_KINDS);
    }

    // CATALOG: test_state.py::AppendTests::test_reserved_field_names_rejected
    #[test]
    fn reserved_field_names_rejected() {
        // `seq`, `ts` and `kind` are never the caller's: an event's own `seq` and `ts` are
        // replaced, and its kind is its variant's (the `kind` marker writes only that name).
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        let mut given = events::Ruling {
            seq: 99,
            ts: "1999-01-01T00:00:00.000Z".into(),
            text: Some("t".into()),
            ..events::Ruling::default()
        };
        given.card = Some(CardRef::Number(1));
        let (written, raw) = append_event(&paths, Event::Ruling(given)).unwrap();
        assert_eq!(written.seq(), 1);
        assert_ne!(written.ts(), "1999-01-01T00:00:00.000Z");
        assert!(raw.get().starts_with(&format!(
            r#"{{"seq":1,"ts":"{}","kind":"ruling","#,
            written.ts()
        )));
        assert_eq!(raw.get().matches("\"seq\"").count(), 1);
        assert_eq!(raw.get().matches("\"ts\"").count(), 1);
        assert_eq!(raw.get().matches("\"kind\"").count(), 1);
    }

    // CATALOG: test_state.py::AppendTests::test_append_attempt_and_read_attempts
    #[test]
    fn append_attempt_and_read_attempts() {
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        assert!(read_attempts(&paths).unwrap().is_empty());
        let attempt = |n| AttemptRecord {
            card: Some(CardRef::Number(1)),
            attempt: Some(n),
            ..AttemptRecord::default()
        };
        append_attempt(&paths, &attempt(1)).unwrap();
        append_attempt(&paths, &attempt(2)).unwrap();
        assert_eq!(read_attempts(&paths).unwrap(), [attempt(1), attempt(2)]);
        // Python's records read as theirs, with every field it did not write missing.
        let code = "import sys\nfrom pathlib import Path\nfrom leadhand import state\n\
                    state.append_attempt(state.run_paths(Path(sys.argv[1])), {'card': 1, 'attempt': 3})\n";
        python(code, &[tmp.path().as_os_str()]);
        assert_eq!(read_attempts(&paths).unwrap()[2], attempt(3));
    }

    #[test]
    fn attempt_records_are_the_python_writers_records() {
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        let usage = Usage {
            tokens: Tokens {
                input: 3,
                output: 10,
                ..Tokens::default()
            },
            model: Some("claude-opus-4-1".into()),
            ..Usage::default()
        };
        // The engine's record of a Claude worker, then a runner's of a Codex one.
        let claude = AttemptRecord {
            dispatch: Some("c1-a1-implement-1".into()),
            run: Some("abc123".into()),
            card: Some(CardRef::Number(1)),
            attempt: Some(1),
            role: Some("implementer".into()),
            vendor: Some("claude".into()),
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            agent: Some("leadhand:implementer".into()),
            worker_id: Some(PyStr::from_os(OsStr::new("w\u{e9}"))),
            started_at: Some("2026-11-14T00:00:00.000Z".into()),
            ended_at: Some("2026-11-14T00:01:00.000Z".into()),
            steps: None,
            charge_ids: vec!["c1-a1-implement-1:claude:w\u{e9}".into()],
            usage: Some(usage),
            usage_known: Some(true),
            cost_estimate: Some(1e-05),
            exit_reason: Some("completed".into()),
            status: Some("READY".into()),
            blocked_by: Some(String::new()),
            recorded_by: Some("engine".into()),
            ..AttemptRecord::default()
        };
        let codex = AttemptRecord {
            dispatch: Some("c1-a2-implement-1".into()),
            run: None,
            card: None,
            attempt: Some(2),
            role: Some("implementer".into()),
            vendor: Some("codex".into()),
            model: Some("gpt-6.1-terra".into()),
            effort: Some("high".into()),
            worker_id: None,
            started_at: Some("2026-11-14T00:00:00.000Z".into()),
            ended_at: Some("2026-11-14T00:01:00.000Z".into()),
            steps: Some(4),
            charge_ids: vec!["c1-a2-implement-1:codex:unknown".into()],
            usage: None,
            usage_known: Some(false),
            usage_source: Some("unknown".into()),
            cost_estimate: None,
            exit_reason: Some("interrupted".into()),
            status: Some("SPENT".into()),
            blocked_by: Some(String::new()),
            generation: Some(1),
            possibly_incomplete: Some(true),
            ..AttemptRecord::default()
        };
        append_attempt(&paths, &claude).unwrap();
        append_attempt(&paths, &codex).unwrap();
        let code = "import json, sys\nfrom pathlib import Path\nfrom leadhand import state\n\
            claude = {'dispatch': 'c1-a1-implement-1', 'run': 'abc123', 'card': 1, 'attempt': 1,\n\
            \x20   'role': 'implementer', 'vendor': 'claude', 'model': 'sonnet', 'effort': 'high',\n\
            \x20   'agent': 'leadhand:implementer', 'worker_id': 'w\\u00e9', 'started_at': '2026-11-14T00:00:00.000Z',\n\
            \x20   'ended_at': '2026-11-14T00:01:00.000Z', 'steps': None, 'charge_ids': ['c1-a1-implement-1:claude:w\\u00e9'],\n\
            \x20   'usage': {'input': 3, 'cached': 0, 'cache_write_5m': 0, 'cache_write_1h': 0, 'output': 10,\n\
            \x20             'reasoning': 0, 'known': True, 'model': 'claude-opus-4-1'},\n\
            \x20   'usage_known': True, 'cost_estimate': 1e-05, 'exit_reason': 'completed', 'status': 'READY',\n\
            \x20   'blocked_by': '', 'recorded_by': 'engine'}\n\
            codex = {'dispatch': 'c1-a2-implement-1', 'run': None, 'card': None, 'attempt': 2,\n\
            \x20   'role': 'implementer', 'vendor': 'codex', 'model': 'gpt-6.1-terra', 'effort': 'high',\n\
            \x20   'worker_id': None, 'started_at': '2026-11-14T00:00:00.000Z', 'ended_at': '2026-11-14T00:01:00.000Z',\n\
            \x20   'steps': 4, 'charge_ids': ['c1-a2-implement-1:codex:unknown'], 'usage': None, 'usage_known': False,\n\
            \x20   'usage_source': 'unknown', 'cost_estimate': None, 'exit_reason': 'interrupted', 'status': 'SPENT',\n\
            \x20   'blocked_by': '', 'generation': 1, 'possibly_incomplete': True}\n\
            got = state.read_attempts(state.run_paths(Path(sys.argv[1])))\n\
            print(got == [claude, codex], [list(a) for a in got] == [list(claude), list(codex)])\n";
        assert_eq!(python(code, &[tmp.path().as_os_str()]), b"True True\n");
        assert_eq!(read_attempts(&paths).unwrap(), [claude, codex]);
    }

    #[test]
    fn two_phase_appends_intent_then_completion() {
        let tmp = run_dir();
        let paths = run_paths(tmp.path());
        let out = two_phase(
            &paths,
            ruling("intent"),
            "before_rebase",
            || Ok(7),
            "after_rebase",
            |n| ruling(&format!("done {n}")),
        )
        .unwrap();
        assert_eq!(out, 7);
        let texts: Vec<String> = read_events(&paths)
            .unwrap()
            .into_iter()
            .map(|(e, _)| match e {
                Event::Ruling(r) => r.text.unwrap().lossy().into_owned(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(texts, ["intent", "done 7"]);
        // An act that fails appends no completion.
        let err = two_phase(
            &paths,
            ruling("again"),
            "before_rebase",
            || Err::<u8, _>(Error::msg("rebase failed")),
            "after_rebase",
            |_| ruling("never"),
        )
        .unwrap_err();
        assert_eq!(err.message, "rebase failed");
        assert_eq!(read_events(&paths).unwrap().len(), 3);
    }

    /// The run directory `two_phase_child` acts in.
    const CHILD_RUN: &str = "LEADHAND_TEST_TWO_PHASE_RUN";

    /// Runs one two-phase step in the run directory `CHILD_RUN` names, when it names one:
    /// this test binary run again as a child, with the crash hooks set.
    #[test]
    fn two_phase_child() {
        let Some(run) = std::env::var_os(CHILD_RUN) else {
            return;
        };
        let paths = run_paths(Path::new(&run));
        let acted = paths.root.join("acted");
        let act = || fs::write(&acted, "").map_err(|e| Error::io(&acted, e));
        if std::env::var_os(CHILD_RESUME).is_some() {
            two_phase_resume(&paths, act, "after_rebase", |_| ruling("done")).unwrap();
            return;
        }
        two_phase(
            &paths,
            ruling("intent"),
            "before_rebase",
            act,
            "after_rebase",
            |_| ruling("done"),
        )
        .unwrap();
    }

    /// Set: `two_phase_child` takes the resume form.
    const CHILD_RESUME: &str = "LEADHAND_TEST_TWO_PHASE_RESUME";

    /// `two_phase_child` with the crash hooks crashing at `at`; returns its exit code, the
    /// events' texts, whether the act ran, and the crash points passed.
    fn crash_child(at: &str) -> (Option<i32>, Vec<String>, bool, Vec<String>) {
        crash_child_in(at, false)
    }

    fn crash_child_in(at: &str, resume: bool) -> (Option<i32>, Vec<String>, bool, Vec<String>) {
        let tmp = run_dir();
        let log = tmp.path().join("crash.log");
        let mut child = Command::new(std::env::current_exe().unwrap());
        if resume {
            child.env(CHILD_RESUME, "1");
        }
        let out = child
            .args([
                "state::write::tests::two_phase_child",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD_RUN, tmp.path())
            .env("LEADHAND_TESTING", "1")
            .env("LEADHAND_CRASH_AT", at)
            .env("LEADHAND_CRASH_LOG", &log)
            .output()
            .unwrap();
        let paths = run_paths(tmp.path());
        let texts = read_events(&paths)
            .unwrap()
            .into_iter()
            .map(|(e, _)| match e {
                Event::Ruling(r) => r.text.unwrap().lossy().into_owned(),
                other => panic!("{other:?}"),
            })
            .collect();
        let passed = fs::read_to_string(&log)
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default();
        (
            out.status.code(),
            texts,
            tmp.path().join("acted").exists(),
            passed,
        )
    }

    #[cfg(feature = "testing")]
    #[test]
    fn two_phase_crash_points_bracket_the_act() {
        // Crashed before the act: the intent is recorded, nothing was done.
        let (code, texts, acted, passed) = crash_child("before_rebase");
        assert_eq!(
            (code, texts, acted),
            (Some(97), vec!["intent".into()], false)
        );
        assert_eq!(passed, ["before_rebase"]);
        // Crashed after it: done, but not yet recorded.
        let (code, texts, acted, passed) = crash_child("after_rebase");
        assert_eq!(
            (code, texts, acted),
            (Some(97), vec!["intent".into()], true)
        );
        assert_eq!(passed, ["before_rebase", "after_rebase"]);
        // No crash: both recorded, and both points passed in order.
        let (code, texts, acted, passed) = crash_child("elsewhere");
        assert_eq!(
            (code, texts, acted),
            (Some(0), vec!["intent".into(), "done".into()], true)
        );
        assert_eq!(passed, ["before_rebase", "after_rebase"]);
    }

    #[cfg(feature = "testing")]
    #[test]
    fn two_phase_resume_passes_only_the_point_after() {
        // The intent is already recorded: the act runs again, then only the point after it.
        let (code, texts, acted, passed) = crash_child_in("after_rebase", true);
        assert_eq!((code, texts, acted), (Some(97), Vec::new(), true));
        assert_eq!(passed, ["after_rebase"]);
        let (code, texts, acted, passed) = crash_child_in("before_rebase", true);
        assert_eq!((code, texts, acted), (Some(0), vec!["done".into()], true));
        assert_eq!(passed, ["after_rebase"]);
    }

    #[cfg(not(feature = "testing"))]
    #[test]
    fn two_phase_crash_points_are_inert_without_the_hooks() {
        let (code, texts, acted, passed) = crash_child("before_rebase");
        assert_eq!(
            (code, texts, acted),
            (Some(0), vec!["intent".into(), "done".into()], true)
        );
        assert!(passed.is_empty());
    }
}
