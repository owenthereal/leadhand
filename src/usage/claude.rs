//! Claude transcripts (`usage.py`): where a worker's or a session's transcript is, and what
//! its assistant messages used, once per message ID.
//!
//! Each content block of one assistant message is its own line, all with the same
//! `message.id`. Per message ID the record with the largest `output_tokens` is kept (ties keep
//! the first), so a message charged mid-stream is topped up when it grows (R17). A message
//! with no ID cannot be deduplicated or checkpointed and is skipped. When `cache_creation` is
//! absent, `cache_creation_input_tokens` counts as 5-minute writes.
//!
//! Transcripts are vendor files: only the fields used are read, leniently, as the Python's
//! `isinstance` checks read them (`meter::Lenient`), and a repeated key keeps its last value,
//! as `json.loads` does. Message IDs and models are kept as `PyStr`.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::de::{IgnoredAny, MapAccess};

use super::meter::{Count, Lenient, VendorStr, deserialize_leniently};
use super::{Locate, Tokens, Usage, add, json_lines, newest, safe_id};
use crate::error::{Error, Result};
use crate::wire::{PyMap, PyStr};

/// `<projects_dir>/*/*/subagents/agent-<id>.jsonl`, the newest when several match; `None`
/// for an ID that cannot name a file (`usage.claude_subagent_transcript`).
pub fn claude_subagent_transcript(agent_id: &PyStr, projects_dir: &Path) -> Option<PathBuf> {
    let file = named("agent-", agent_id)?;
    newest(
        projects_dir,
        2,
        Locate::Path(&[OsStr::new("subagents"), &file]),
    )
}

/// `<projects_dir>/*/<session_id>.jsonl`, the newest when several match
/// (`usage.claude_session_transcript`).
pub fn claude_session_transcript(session_id: &PyStr, projects_dir: &Path) -> Option<PathBuf> {
    let file = named("", session_id)?;
    newest(projects_dir, 1, Locate::Path(&[&file]))
}

/// `<prefix><id>.jsonl`, for an ID that can name a file. An ID whose text has no OS bytes
/// (a lone surrogate `surrogateescape` cannot encode) names none, as Python's `lexists`
/// finds nothing for it.
fn named(prefix: &str, id: &PyStr) -> Option<OsString> {
    if !safe_id(id) {
        return None;
    }
    let mut name = OsString::from(prefix);
    name.push(id.to_os().ok()?);
    name.push(".jsonl");
    Some(name)
}

/// One message's usage, from the record kept for it.
pub(crate) type Messages = Vec<(PyStr, Usage)>;

/// The record kept per message ID, in first-seen order (`usage._claude_messages`).
pub(crate) fn claude_messages(path: &Path) -> std::io::Result<Messages> {
    let mut best: Messages = Vec::new();
    let mut at: HashMap<PyStr, usize> = HashMap::new();
    for record in json_lines::<Record>(path, &["assistant"])? {
        let Some((usage, id)) = record.usage() else {
            continue;
        };
        match at.get(&id) {
            Some(&i) => {
                if usage.tokens.output > best[i].1.tokens.output {
                    best[i].1 = usage;
                }
            }
            None => {
                at.insert(id.clone(), best.len());
                best.push((id, usage));
            }
        }
    }
    Ok(best)
}

/// The transcript's assistant messages summed once per message ID, skipping
/// `exclude_ids`; with the IDs counted, in first-seen order (`usage.claude_usage`).
pub fn claude_usage(path: &Path, exclude_ids: &[PyStr]) -> Result<(Usage, Vec<PyStr>)> {
    let messages = claude_messages(path).map_err(|e| Error::io(path, e))?;
    let excluded: HashSet<&PyStr> = exclude_ids.iter().collect();
    let mut total = Usage::default();
    let mut counted = Vec::new();
    for (id, usage) in messages {
        if !excluded.contains(&id) {
            total = add(&total, &usage);
            counted.push(id);
        }
    }
    Ok((total, counted))
}

/// What is new in a transcript since `charged` (each message ID's output already charged)
/// (`usage.new_claude_usage`): a new message is charged in full; a message that has grown is
/// charged only its extra output (its input side was charged with its first record). With
/// the new or grown IDs at their output so far. Unknown, reaching nothing, when there is no
/// transcript or it cannot be read.
pub fn new_claude_usage(path: Option<&Path>, charged: &PyMap<u64>) -> (Usage, PyMap<u64>) {
    let Some(Ok(messages)) = path.map(claude_messages) else {
        return (Usage::unknown(), PyMap::default());
    };
    let charged: HashMap<&PyStr, u64> = charged.0.iter().map(|(id, n)| (id, *n)).collect();
    let mut used = Usage::default();
    let mut reached = PyMap::default();
    for (id, usage) in messages {
        let output = usage.tokens.output;
        match charged.get(&id) {
            None => used = add(&used, &usage),
            Some(&so_far) if output > so_far => {
                let extra = Usage {
                    tokens: Tokens {
                        output: output - so_far,
                        ..Tokens::default()
                    },
                    model: usage.model,
                    ..Usage::default()
                };
                used = add(&used, &extra);
            }
            Some(_) => continue,
        }
        reached.0.push((id, output));
    }
    (used, reached)
}

// --------------------------------------------------------------------------
// The records read

deserialize_leniently!(Record, Message, MessageUsage, Creation);

/// A transcript line: whether its `type` is `assistant`, and its message.
#[derive(Default)]
struct Record {
    assistant: bool,
    message: Message,
}

impl Lenient for Record {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> std::result::Result<Self, A::Error> {
        let mut r = Record::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"type" => r.assistant = map.next_value::<VendorStr>()?.is("assistant"),
                b"message" => r.message = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(r)
    }
}

/// `message`: its ID and model when they are strings, and its usage when it is an object.
#[derive(Default)]
struct Message {
    id: Option<PyStr>,
    model: Option<PyStr>,
    usage: MessageUsage,
}

impl Lenient for Message {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> std::result::Result<Self, A::Error> {
        let mut m = Message::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"id" => m.id = map.next_value::<VendorStr>()?.0,
                b"model" => m.model = map.next_value::<VendorStr>()?.0,
                b"usage" => m.usage = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(m)
    }
}

/// `message.usage`; `object` is false when it is not a JSON object.
#[derive(Default)]
struct MessageUsage {
    object: bool,
    input: Count,
    cache_read: Count,
    output: Count,
    creation: Creation,
    creation_total: Count,
}

impl Lenient for MessageUsage {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> std::result::Result<Self, A::Error> {
        let mut u = MessageUsage {
            object: true,
            ..MessageUsage::default()
        };
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"input_tokens" => u.input = map.next_value()?,
                b"cache_read_input_tokens" => u.cache_read = map.next_value()?,
                b"output_tokens" => u.output = map.next_value()?,
                b"cache_creation" => u.creation = map.next_value()?,
                b"cache_creation_input_tokens" => u.creation_total = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(u)
    }
}

/// `cache_creation`, the per-TTL breakdown; `object` is false when it is not a JSON object.
#[derive(Default)]
struct Creation {
    object: bool,
    five_minutes: Count,
    one_hour: Count,
}

impl Lenient for Creation {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> std::result::Result<Self, A::Error> {
        let mut c = Creation {
            object: true,
            ..Creation::default()
        };
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"ephemeral_5m_input_tokens" => c.five_minutes = map.next_value()?,
                b"ephemeral_1h_input_tokens" => c.one_hour = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(c)
    }
}

impl Record {
    /// The message's usage and ID (`usage._claude_record_usage`): `None` unless the record is
    /// an assistant message with a non-empty string ID and an object usage.
    fn usage(self) -> Option<(Usage, PyStr)> {
        let m = self.message;
        let id = m.id.filter(|id| !id.is_empty())?;
        if !self.assistant || !m.usage.object {
            return None;
        }
        let u = &m.usage;
        let (w5, w1) = if u.creation.object {
            (u.creation.five_minutes.0, u.creation.one_hour.0)
        } else {
            (u.creation_total.0, 0)
        };
        let tokens = Tokens {
            input: u.input.0,
            cached: u.cache_read.0,
            cache_write_5m: w5,
            cache_write_1h: w1,
            output: u.output.0,
            reasoning: 0,
        };
        let usage = Usage {
            tokens,
            model: m.model,
            ..Usage::default()
        };
        Some((usage, id))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    pub(crate) const OPUS: &str = "claude-opus-4-1";

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// A transcript line, as the Python tests' `claude_line` writes it.
    pub(crate) fn claude_line(
        mid: &str,
        out: u64,
        inp: u64,
        cached: u64,
        w5: u64,
        w1: u64,
    ) -> String {
        format!(
            "{{\"type\": \"assistant\", \"message\": {{\"id\": \"{mid}\", \"model\": \"{OPUS}\", \
             \"role\": \"assistant\", \"usage\": {{\"input_tokens\": {inp}, \
             \"cache_read_input_tokens\": {cached}, \"output_tokens\": {out}, \
             \"cache_creation_input_tokens\": {}, \"cache_creation\": \
             {{\"ephemeral_5m_input_tokens\": {w5}, \"ephemeral_1h_input_tokens\": {w1}}}}}}}}}",
            w5 + w1
        )
    }

    /// Whether the file system under `dir` takes a file name that is not UTF-8 (APFS
    /// refuses one with EILSEQ).
    pub(crate) fn odd_names_allowed(dir: &Path) -> bool {
        let probe = dir.join(OsStr::from_bytes(b"probe-\xff"));
        let ok = fs::write(&probe, "").is_ok();
        let _ = fs::remove_file(&probe);
        ok
    }

    fn five(u: &Usage) -> (u64, u64, u64, u64, u64) {
        let t = u.tokens;
        (
            t.input,
            t.cached,
            t.cache_write_5m,
            t.cache_write_1h,
            t.output,
        )
    }

    fn ids(v: &[&str]) -> Vec<PyStr> {
        v.iter().map(|s| PyStr::from(*s)).collect()
    }

    // CATALOG: test_usage.py::ClaudeUsageTests::test_claude_usage_dedupes_by_message_id
    #[test]
    fn claude_usage_dedupes_by_message_id() {
        let (u, counted) = claude_usage(&fixture("claude-subagent-duplicates.jsonl"), &[]).unwrap();
        assert_eq!(counted, ids(&["msg_A", "msg_B", "msg_C"]));
        assert_eq!(five(&u), (130, 1500, 250, 300, 130));
        assert!(u.known);
        assert_eq!(u.model, Some(PyStr::from(OPUS)));
    }

    // CATALOG: test_usage.py::ClaudeUsageTests::test_claude_usage_skips_excluded_ids
    #[test]
    fn claude_usage_skips_excluded_ids() {
        let path = fixture("claude-subagent-duplicates.jsonl");
        let (u, counted) = claude_usage(&path, &ids(&["msg_A"])).unwrap();
        assert_eq!(counted, ids(&["msg_B", "msg_C"]));
        assert_eq!(five(&u), (30, 500, 50, 0, 90));
    }

    // CATALOG: test_usage.py::ClaudeUsageTests::test_claude_usage_ignores_partial_and_garbage_lines
    #[test]
    fn claude_usage_ignores_partial_and_garbage_lines() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("t.jsonl");
        let partial = claude_line("m2", 9, 0, 0, 0, 0);
        fs::write(
            &f,
            format!(
                "{}\nnot json\n[1,2]\n{}",
                claude_line("m1", 4, 1, 0, 0, 0),
                &partial[..30]
            ),
        )
        .unwrap();
        let (u, counted) = claude_usage(&f, &[]).unwrap();
        assert_eq!(counted, ids(&["m1"]));
        assert_eq!(u.tokens.output, 4);
        // A missing transcript is an error here, and unknown usage for a charge.
        assert!(claude_usage(&dir.path().join("gone.jsonl"), &[]).is_err());
        let (gone, reached) =
            new_claude_usage(Some(&dir.path().join("gone.jsonl")), &PyMap::default());
        assert!(!gone.known);
        assert!(reached.is_empty());
        let (none, reached) = new_claude_usage(None, &PyMap::default());
        assert!(!none.known && reached.is_empty());
    }

    #[test]
    fn records_read_as_the_python_reads_them() {
        // Each line is read by the Rust reader and by `usage._claude_messages`; both must
        // agree on every message's usage.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("odd.jsonl");
        let lines = [
            // The legacy shape: no breakdown, all writes are 5-minute writes.
            r#"{"type":"assistant","message":{"id":"legacy","usage":{"input_tokens":1,"cache_creation_input_tokens":7,"output_tokens":2}}}"#,
            // Counts of other types: a bool and a string are 0, a float truncates, a
            // negative floors at 0.
            r#"{"type":"assistant","message":{"id":"types","model":7,"usage":{"input_tokens":true,"cache_read_input_tokens":"9","output_tokens":3.9,"cache_creation":{"ephemeral_5m_input_tokens":-4,"ephemeral_1h_input_tokens":2}}}}"#,
            // A repeated key keeps its last value; a lone surrogate in an ID is kept.
            r#"{"type":"user","type":"assistant","message":{"id":"a\udcffb","usage":{"output_tokens":1,"output_tokens":5}}}"#,
            // Skipped: an empty ID, a usage that is not an object, a non-assistant record,
            // and a record whose `type` is not a string.
            r#"{"type":"assistant","message":{"id":"","usage":{"output_tokens":1}}}"#,
            r#"{"type":"assistant","message":{"id":"nousage","usage":[1]}}"#,
            r#"{"type":"system","message":{"id":"sys","usage":{"output_tokens":1}}}"#,
            r#"{"type":["assistant"],"message":{"id":"list","usage":{"output_tokens":1}}}"#,
            // A record split by a lone CR, and one ending in CRLF; a stripped form feed.
            "\x0c{\"type\":\"assistant\",\"message\":{\"id\":\"cr\",\"usage\":{\"output_tokens\":6}}}\r{\"type\":\"assistant\",\"message\":{\"id\":\"crlf\",\"usage\":{\"output_tokens\":8}}}\r",
            // Invalid UTF-8 elsewhere on the line is replaced, the record still read.
            "{\"type\":\"assistant\",\"x\":\"\u{fffd}\",\"message\":{\"id\":\"bad\",\"usage\":{\"output_tokens\":2}}}",
        ];
        let mut bytes = lines.join("\n").into_bytes();
        let at = bytes
            .windows(3)
            .position(|w| w == "\u{fffd}".as_bytes())
            .unwrap();
        bytes.splice(at..at + 3, [0xffu8]);
        fs::write(&f, &bytes).unwrap();
        let ours: Vec<(Vec<u8>, String)> = claude_messages(&f)
            .unwrap()
            .into_iter()
            .map(|(id, u)| (id.as_wtf8().to_vec(), crate::wire::to_line(&u).unwrap()))
            .collect();
        let code = "import json, sys\nfrom leadhand import usage as u\n\
                    for mid, x in u._claude_messages(sys.argv[1]).items():\n\
                    \x20   print(json.dumps(mid), json.dumps(u.usage_to_dict(x), separators=(',', ':')))\n";
        let theirs = crate::wire::tests::python(code, &[f.as_os_str()]);
        let theirs: Vec<(Vec<u8>, String)> = String::from_utf8(theirs)
            .unwrap()
            .lines()
            .map(|line| {
                let (id, usage) = line.split_once(' ').unwrap();
                let id: PyStr = serde_json::from_str(id).unwrap();
                (id.as_wtf8().to_vec(), usage.to_string())
            })
            .collect();
        assert_eq!(ours, theirs);
        assert_eq!(ours.len(), 6, "{ours:?}");
    }

    // CATALOG: test_usage.py::LocatorTests::test_claude_transcript_locators
    #[test]
    fn claude_transcript_locators() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let sub = projects.join("-some-project/sess-1/subagents/agent-abc123.jsonl");
        let sess = projects.join("-some-project/sess-1.jsonl");
        fs::create_dir_all(sub.parent().unwrap()).unwrap();
        fs::write(&sub, "{}\n").unwrap();
        fs::write(&sess, "{}\n").unwrap();
        let id = |s: &str| PyStr::from(s);
        assert_eq!(
            claude_subagent_transcript(&id("abc123"), &projects),
            Some(sub.clone())
        );
        assert_eq!(
            claude_session_transcript(&id("sess-1"), &projects),
            Some(sess)
        );
        assert_eq!(claude_subagent_transcript(&id("nope"), &projects), None);
        assert_eq!(claude_session_transcript(&id("nope"), &projects), None);
        assert_eq!(claude_subagent_transcript(&id("*"), &projects), None);
        for unsafe_id in ["", "a/b", "a\0b"] {
            assert_eq!(claude_subagent_transcript(&id(unsafe_id), &projects), None);
        }
        // A missing projects folder finds nothing (test_missing_projects_dir_is_none).
        assert_eq!(
            claude_subagent_transcript(&id("abc"), &dir.path().join("absent")),
            None
        );
        // A hidden project folder is not searched, as glob's `*` skips it.
        let hidden = projects.join(".hidden/sess-2/subagents/agent-h.jsonl");
        fs::create_dir_all(hidden.parent().unwrap()).unwrap();
        fs::write(&hidden, "{}\n").unwrap();
        assert_eq!(claude_subagent_transcript(&id("h"), &projects), None);
        // Of several matches, the newest; an odd ID names its file's bytes: a non-UTF-8 one
        // where the file system allows such a name (not APFS), else a non-ASCII one.
        let odd: &[u8] = if odd_names_allowed(dir.path()) {
            b"x\xff"
        } else {
            "x\u{e9}".as_bytes()
        };
        let name = [b"agent-", odd, b".jsonl"].concat();
        let older = projects
            .join("p1/s/subagents")
            .join(OsStr::from_bytes(&name));
        let newer = projects
            .join("p2/s/subagents")
            .join(OsStr::from_bytes(&name));
        let odd = PyStr::from_os(OsStr::from_bytes(odd));
        for (f, secs) in [(&older, 1_000_000), (&newer, 2_000_000)] {
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, "{}\n").unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        assert_eq!(claude_subagent_transcript(&odd, &projects), Some(newer));
    }
}
