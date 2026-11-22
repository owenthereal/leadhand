//! Codex usage (`usage.py`): the session rollout and the `--json` stream.
//!
//! The rollout (`<sessions>/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`) is append-only across
//! resumes. Its source is the top-level `token_usage_record` events written after every model
//! response (P12): `payload.usage` is that response's usage, and the sum over distinct
//! `payload.response_id` (a record repeated after a crash counts once) is the thread's. The
//! last `payload.thread_token_usage` is a cross-check, recorded on a charge when it disagrees
//! and never failing it. A `token_count` event's `total_token_usage` omits a turn killed
//! before its `token_count` was written, so it is a fallback only, for a file with no
//! `token_usage_record`. A file with neither is unknown, not zero.
//!
//! The field mapping, shared by rollout records, `token_count` totals and the stream's
//! `turn.completed.usage`: `input_tokens` includes `cached_input_tokens`
//! (`CODEX_INPUT_INCLUDES_CACHED`), so input is the difference; `reasoning_output_tokens` is
//! carried for information; `cache_write_input_tokens` has no TTL and is kept as 5-minute
//! writes.
//!
//! These are vendor files: only the fields used are read, leniently (`meter::Lenient`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::de::{IgnoredAny, MapAccess};

use super::meter::{Count, Lenient, VendorStr, deserialize_leniently};
use super::{Locate, Tokens, Usage, add, json_lines, newest, safe_id};
use crate::wire::PyStr;

/// `<sessions_dir>/*/*/*/*<thread>.jsonl`, the newest when several match; `None` for a thread
/// ID that cannot name a file (`usage.codex_session_file`).
pub fn codex_session_file(thread: &PyStr, sessions_dir: &Path) -> Option<PathBuf> {
    if !safe_id(thread) {
        return None;
    }
    let mut suffix = thread.as_wtf8().to_vec();
    suffix.extend_from_slice(b".jsonl");
    newest(sessions_dir, 3, Locate::Suffix(&suffix))
}

/// A rollout's usage (`usage._Rollout`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Rollout {
    /// Each distinct `response_id`'s usage, in first-seen order.
    pub responses: Vec<(PyStr, Usage)>,
    /// The last record's `thread_token_usage`.
    pub thread_total: Option<Usage>,
    /// The last `token_count` total (the fallback).
    pub token_count_total: Option<Usage>,
}

/// The rollout at `path`, read (`usage._codex_rollout`).
pub(crate) fn codex_rollout(path: &Path) -> std::io::Result<Rollout> {
    let mut out = Rollout::default();
    let mut seen: HashSet<PyStr> = HashSet::new();
    for rec in json_lines::<RolloutRecord>(path, &["token_usage_record", "token_count"])? {
        let p = rec.payload;
        if rec.usage_record {
            if let (Some(id), Some(usage)) = (p.response_id, p.usage.usage())
                && !id.is_empty()
                && seen.insert(id.clone())
            {
                out.responses.push((id, usage));
            }
            if let Some(total) = p.thread_token_usage.usage() {
                out.thread_total = Some(total);
            }
        } else if p.token_count
            && let Some(total) = p.info.total_token_usage.usage()
        {
            out.token_count_total = Some(total);
        }
    }
    Ok(out)
}

/// The sum of `usages` (`usage._sum`).
pub(crate) fn sum<'a>(usages: impl IntoIterator<Item = &'a Usage>) -> Usage {
    usages
        .into_iter()
        .fold(Usage::default(), |total, u| add(&total, u))
}

/// A Codex session's total usage (`usage.codex_cumulative`): the sum over its distinct
/// responses, else the last `token_count` total; unknown when the file is missing, unreadable
/// or has neither.
pub fn codex_cumulative(thread: &PyStr, sessions_dir: &Path) -> Usage {
    let Some(Ok(rollout)) = codex_session_file(thread, sessions_dir).map(|f| codex_rollout(&f))
    else {
        return Usage::unknown();
    };
    if !rollout.responses.is_empty() {
        return sum(rollout.responses.iter().map(|(_, u)| u));
    }
    rollout.token_count_total.unwrap_or_else(Usage::unknown)
}

/// The usage a `codex exec --json` stream file reports: the sum of its `turn.completed`
/// usages (`usage.codex_usage_from_stream` over `dispatch._Stream`'s events: whole lines only,
/// each decoded with replacement). Unknown when no turn completed, or the file cannot be read.
pub fn codex_stream_usage(stream: &Path) -> Usage {
    let Ok(bytes) = std::fs::read(stream) else {
        return Usage::unknown();
    };
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    lines.pop(); // after the last newline: empty, or a line still being written
    let mut total = Usage::default();
    let mut seen = false;
    for line in lines {
        let Ok(event) = serde_json::from_str::<StreamEvent>(&String::from_utf8_lossy(line)) else {
            continue;
        };
        if let (true, Some(usage)) = (event.turn_completed, event.usage.usage()) {
            total = add(&total, &usage);
            seen = true;
        }
    }
    if seen { total } else { Usage::unknown() }
}

// --------------------------------------------------------------------------
// The records read

deserialize_leniently!(RolloutRecord, Payload, Info, StreamEvent, CodexTokens);

/// A rollout line: its `type`, and its payload.
#[derive(Default)]
struct RolloutRecord {
    usage_record: bool,
    payload: Payload,
}

impl Lenient for RolloutRecord {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut r = RolloutRecord::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"type" => {
                    r.usage_record = map.next_value::<VendorStr>()?.is("token_usage_record");
                }
                b"payload" => r.payload = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(r)
    }
}

/// `payload`: a usage record's fields and a `token_count` event's.
#[derive(Default)]
struct Payload {
    response_id: Option<PyStr>,
    usage: CodexTokens,
    thread_token_usage: CodexTokens,
    token_count: bool,
    info: Info,
}

impl Lenient for Payload {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut p = Payload::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"response_id" => p.response_id = map.next_value::<VendorStr>()?.0,
                b"usage" => p.usage = map.next_value()?,
                b"thread_token_usage" => p.thread_token_usage = map.next_value()?,
                b"type" => p.token_count = map.next_value::<VendorStr>()?.is("token_count"),
                b"info" => p.info = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(p)
    }
}

/// `payload.info`.
#[derive(Default)]
struct Info {
    total_token_usage: CodexTokens,
}

impl Lenient for Info {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut i = Info::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"total_token_usage" => i.total_token_usage = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(i)
    }
}

/// A stream event: whether it is `turn.completed`, and its usage.
#[derive(Default)]
struct StreamEvent {
    turn_completed: bool,
    usage: CodexTokens,
}

impl Lenient for StreamEvent {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut e = StreamEvent::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"type" => e.turn_completed = map.next_value::<VendorStr>()?.is("turn.completed"),
                b"usage" => e.usage = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(e)
    }
}

/// Codex's token fields; `object` is false when the value is not a JSON object.
#[derive(Default)]
struct CodexTokens {
    object: bool,
    input: Count,
    cached: Count,
    output: Count,
    reasoning: Count,
    cache_write: Count,
}

impl Lenient for CodexTokens {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut t = CodexTokens {
            object: true,
            ..CodexTokens::default()
        };
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"input_tokens" => t.input = map.next_value()?,
                b"cached_input_tokens" => t.cached = map.next_value()?,
                b"output_tokens" => t.output = map.next_value()?,
                b"reasoning_output_tokens" => t.reasoning = map.next_value()?,
                b"cache_write_input_tokens" => t.cache_write = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(t)
    }
}

impl CodexTokens {
    /// The usage these fields give (`usage._codex_usage_from`); `None` for a value that is not
    /// an object.
    fn usage(&self) -> Option<Usage> {
        if !self.object {
            return None;
        }
        let tokens = Tokens {
            input: self.input.0.saturating_sub(self.cached.0),
            cached: self.cached.0,
            cache_write_5m: self.cache_write.0,
            cache_write_1h: 0,
            output: self.output.0,
            reasoning: self.reasoning.0,
        };
        Some(Usage {
            tokens,
            ..Usage::default()
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use crate::wire::tests::read_reference_text;

    pub(crate) const ROLLOUT: &str = "codex-rollout-interrupted.jsonl";

    pub(crate) fn fixture_lines(name: &str) -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The lines of the frozen copy of `ROLLOUT` that the reference fixtures were recorded
    /// from (S3-R30): a later edit of the live fixture changes neither side.
    pub(crate) fn frozen_rollout_lines() -> Vec<String> {
        read_reference_text("usage/codex-rollout/codex-rollout-interrupted.jsonl")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// `<sessions>/2026/11/10/rollout-…-<thread>.jsonl` (the Python tests' `rollout_path`).
    pub(crate) fn rollout_path(sessions: &Path, thread: &str) -> PathBuf {
        sessions
            .join("2026/11/10")
            .join(format!("rollout-2026-11-10T00-00-00-{thread}.jsonl"))
    }

    /// A rollout `token_count` event with these cumulative totals (`codex_line`).
    pub(crate) fn codex_line(inp: u64, cached: u64, out: u64, reasoning: u64) -> String {
        let total = format!(
            "{{\"input_tokens\": {inp}, \"cached_input_tokens\": {cached}, \
             \"output_tokens\": {out}, \"reasoning_output_tokens\": {reasoning}}}"
        );
        format!(
            "{{\"type\": \"event_msg\", \"payload\": {{\"type\": \"token_count\", \"info\": \
             {{\"total_token_usage\": {total}, \"last_token_usage\": {total}}}}}}}"
        )
    }

    pub(crate) fn append_lines(path: &Path, lines: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        let mut f = fs::File::options()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
    }

    fn four(u: &Usage) -> (u64, u64, u64, u64) {
        let t = u.tokens;
        (t.input, t.cached, t.output, t.reasoning)
    }

    // CATALOG: test_usage.py::CodexTests::test_codex_cumulative_takes_last_token_count_and_maps_fields
    #[test]
    fn codex_cumulative_takes_last_token_count_and_maps_fields() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        append_lines(
            &rollout_path(&sessions, "thr1"),
            &[
                r#"{"type": "session_meta"}"#,
                &codex_line(100, 40, 10, 3),
                r#"{"type": "event_msg", "payload": {"type": "token_count", "info": null}}"#,
                &codex_line(300, 100, 25, 8),
                r#"{"type": "event_msg", "payload": {"type": "agent_message"}}"#,
            ],
        );
        let u = codex_cumulative(&"thr1".into(), &sessions);
        // input_tokens includes the cached tokens, so uncached input = 300 - 100.
        assert_eq!(four(&u), (200, 100, 25, 8));
        assert!(u.known);
    }

    // CATALOG: test_usage.py::CodexTests::test_codex_cumulative_unknown_when_missing_or_without_token_count
    #[test]
    fn codex_cumulative_unknown_when_missing_or_without_token_count() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        assert!(!codex_cumulative(&"thr1".into(), &sessions).known);
        append_lines(
            &rollout_path(&sessions, "thr2"),
            &[r#"{"type": "session_meta"}"#],
        );
        assert!(!codex_cumulative(&"thr2".into(), &sessions).known);
        // An unsafe thread ID names no file.
        assert!(!codex_cumulative(&"../thr2".into(), &sessions).known);
    }

    // CATALOG: test_usage.py::CodexTests::test_codex_stream_usage_sums_turn_completed
    #[test]
    fn codex_stream_usage_sums_turn_completed() {
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("stream.jsonl");
        fs::write(
            &stream,
            "{\"type\": \"thread.started\", \"thread_id\": \"thr1\"}\n\
             {\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 100, \"cached_input_tokens\": 30, \
             \"output_tokens\": 10, \"reasoning_output_tokens\": 4}}\n\
             {\"type\": \"item.completed\"}\n\
             {\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 50, \"cached_input_tokens\": 10, \
             \"output_tokens\": 5, \"reasoning_output_tokens\": 1}}\n\
             {\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 1000}}",
        )
        .unwrap();
        // The last line has no newline: the stream reader has not taken it yet.
        let u = codex_stream_usage(&stream);
        assert_eq!(four(&u), (110, 40, 15, 5));
        assert!(u.known);
        fs::write(&stream, "{\"type\": \"thread.started\"}\n").unwrap();
        assert!(!codex_stream_usage(&stream).known);
        assert!(!codex_stream_usage(&dir.path().join("missing.jsonl")).known);
    }

    // CATALOG: test_usage.py::CodexStreamFixtureTests::test_stream_usage_sums_turn_completed
    #[test]
    fn stream_fixture_usage_sums_turn_completed() {
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("stream.jsonl");
        fs::write(
            &stream,
            fixture_lines("codex-exec-stream.jsonl").join("\n") + "\n",
        )
        .unwrap();
        let u = codex_stream_usage(&stream);
        assert!(u.known);
        assert_eq!(four(&u), (137911 - 128640, 128640, 368, 0));
        assert_eq!(u.tokens.cache_write_5m, 0);
    }

    // CATALOG: test_usage.py::CodexStreamFixtureTests::test_stream_cache_write_tokens_are_kept_as_five_minute_writes
    #[test]
    fn stream_cache_write_tokens_are_kept_as_five_minute_writes() {
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("stream.jsonl");
        fs::write(
            &stream,
            "{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 100, \"cached_input_tokens\": 10, \
             \"cache_write_input_tokens\": 5, \"output_tokens\": 3, \"reasoning_output_tokens\": 1}}\n",
        )
        .unwrap();
        let t = codex_stream_usage(&stream).tokens;
        assert_eq!(
            (
                t.input,
                t.cached,
                t.cache_write_5m,
                t.cache_write_1h,
                t.output
            ),
            (90, 10, 5, 0, 3)
        );
    }

    #[test]
    fn rollout_reads_as_the_python_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let f = rollout_path(&sessions, "thr1");
        // The rollout fixture as the reference read it: a frozen copy, so a later edit of the
        // fixture changes neither side.
        let mut lines = frozen_rollout_lines();
        // A repeated response, a record whose response_id is not a string, a payload that is
        // not an object, and a response ID with a lone surrogate.
        lines.push(lines[16].clone());
        lines.push(r#"{"type":"token_usage_record","payload":{"response_id":7,"usage":{"input_tokens":5}}}"#.into());
        lines.push(r#"{"type":"token_usage_record","payload":[1]}"#.into());
        lines.push(r#"{"type":"token_usage_record","payload":{"response_id":"r\udcff","usage":{"input_tokens":5,"cached_input_tokens":9}}}"#.into());
        append_lines(&f, &lines.iter().map(String::as_str).collect::<Vec<_>>());
        let r = codex_rollout(&f).unwrap();
        let ours = format!(
            "{}\n{}\n{}\n",
            r.responses
                .iter()
                .map(|(id, u)| format!(
                    "{} {}",
                    crate::wire::to_line(id).unwrap(),
                    crate::wire::to_line(u).unwrap()
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            crate::wire::to_line(&r.thread_total).unwrap(),
            crate::wire::to_line(&r.token_count_total).unwrap(),
        );
        // The reference read the same lines as these responses (ID and usage), then its thread
        // total and its last token count's total.
        assert_eq!(ours, read_reference_text("usage/codex-rollout/read.txt"));
        assert_eq!(r.responses.len(), 3);
        // The session file is found by its thread ID, as the Python finds it.
        assert_eq!(
            codex_session_file(&"thr1".into(), &sessions),
            Some(f.clone())
        );
        assert_eq!(codex_session_file(&"hr1".into(), &sessions), Some(f));
        assert_eq!(codex_session_file(&"thr".into(), &sessions), None);
        // A thread ID whose file name is not UTF-8 where the file system allows one, else
        // not ASCII.
        let odd: &[u8] = if crate::usage::claude::tests::odd_names_allowed(dir.path()) {
            b"t\xff"
        } else {
            "t\u{e9}".as_bytes()
        };
        let name = [b"rollout-", odd, b".jsonl"].concat();
        let odd_file = sessions
            .join("2026/11/11")
            .join(std::ffi::OsStr::from_bytes(&name));
        let odd = PyStr::from_os(std::ffi::OsStr::from_bytes(odd));
        append_lines(&odd_file, &["{}"]);
        assert_eq!(codex_session_file(&odd, &sessions), Some(odd_file));
        // A hidden file is not matched by glob's `*`.
        append_lines(&sessions.join("2026/11/12/.thr9.jsonl"), &["{}"]);
        assert_eq!(codex_session_file(&"thr9".into(), &sessions), None);
    }
}
