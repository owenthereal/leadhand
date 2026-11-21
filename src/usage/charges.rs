//! Charging (`usage.charge`): each worker's usage charged exactly once.
//!
//! `charges.jsonl` is the source of truth. Each charge is one durable record with a unique
//! `charge_id`; it holds the usage charged and the checkpoint reached (Claude: the output
//! charged so far for each message ID touched; Codex: the response IDs newly charged, or the
//! `token_count` total reached). The whole sequence (does the ID exist? derive the checkpoint
//! from earlier charges, read the source, compute only what is new, append) runs under the
//! run's state lock, the same `flock` the Python takes. So a repeated charge is harmless, a
//! crash between a charge and its attempt record loses nothing, and two writers (Rust or
//! Python) never double-count: each continues from the checkpoint the other reached.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Charge, Tokens, Usage, codex, new_claude_usage, read_charges, subtract};
use crate::error::{Error, Result};
use crate::pytext::pure_path;
use crate::state::{RunPaths, utc_now};
use crate::store;
use crate::wire::{PyMap, PyStr};

/// The stored source of a Codex charge taken from the `--json` stream because the session
/// file gave unknown usage (`usage.CODEX_STREAM_SOURCE`).
pub const CODEX_STREAM_SOURCE: &str = "codex-stream";

/// A charge's source, as `charged_checkpoint` keys it (`usage.SOURCES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Claude => "claude",
            Source::Codex => "codex",
        }
    }
}

/// What a charge reads.
#[derive(Clone, Copy, Debug)]
pub enum ChargeSource<'a> {
    /// A Claude transcript (source `claude`), which must be an absolute path; `None` when no
    /// transcript is known, which charges unknown usage.
    ClaudeTranscript(Option<&'a Path>),
    /// A Codex session (source `codex`), found by its thread under `sessions_dir` (`None`
    /// when no thread is known). When it gives unknown usage, `stream`'s usage is charged
    /// instead, if it has any, with source `codex-stream`; that moves no checkpoint.
    Codex {
        thread: Option<&'a PyStr>,
        sessions_dir: &'a Path,
        stream: Option<&'a Path>,
    },
}

/// What a charge reached (`checkpoint`): `messages` for a Claude charge; `responses` and
/// `cumulative` for a Codex one (both written, as the Python writes both).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Checkpoint {
    /// Each message ID charged, at the output charged so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<PyMap<u64>>,
    /// The response IDs charged, sorted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub responses: Option<Vec<PyStr>>,
    /// The `token_count` total reached (the fallback checkpoint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative: Option<Cumulative>,
}

/// A Codex fallback checkpoint: the `token_count` total reached, written as its token
/// fields, or `{}` for none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cumulative(pub Option<Tokens>);

impl Serialize for Cumulative {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match &self.0 {
            Some(t) => t.serialize(s),
            None => PyMap::<u64>::default().serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Cumulative {
    /// Each field given (missing ones 0); `{}` is none.
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let fields = PyMap::<u64>::deserialize(d)?;
        if fields.is_empty() {
            return Ok(Cumulative(None));
        }
        let get = |name: &str| fields.get(&PyStr::from(name)).copied().unwrap_or(0);
        Ok(Cumulative(Some(Tokens {
            input: get("input"),
            cached: get("cached"),
            cache_write_5m: get("cache_write_5m"),
            cache_write_1h: get("cache_write_1h"),
            output: get("output"),
            reasoning: get("reasoning"),
        })))
    }
}

/// A Codex charge's records disagreeing with the rollout's `thread_token_usage`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    pub records: Tokens,
    pub thread_token_usage: Tokens,
}

/// A field whose `null` reads as its default (`r.get(key) or {}`).
pub(crate) fn null_as_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    d: D,
) -> std::result::Result<T, D::Error> {
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

/// What earlier charges for (`source`, `key`) already cover (`usage.charged_checkpoint`).
/// Claude: `messages`, each ID's largest output charged over all charges (a message charged
/// mid-stream is topped up if it grows). Codex: `responses`, every response ID charged,
/// sorted; and `cumulative`, each field's largest fallback total (none for a thread charged
/// from `token_usage_record` events).
pub fn charged_checkpoint(charges: &[Charge], source: Source, key: &PyStr) -> Checkpoint {
    let mine = charges
        .iter()
        .filter(|c| c.source == source.as_str() && c.key == *key)
        .map(|c| &c.checkpoint);
    match source {
        Source::Claude => {
            let mut messages: PyMap<u64> = PyMap::default();
            for (id, n) in mine
                .filter_map(|cp| cp.messages.as_ref())
                .flat_map(|m| &m.0)
            {
                let most = messages.entry(id, || 0);
                *most = (*most).max(*n);
            }
            Checkpoint {
                messages: Some(messages),
                ..Checkpoint::default()
            }
        }
        Source::Codex => {
            let mut ids: Vec<PyStr> = Vec::new();
            let mut cumulative: Option<Tokens> = None;
            for cp in mine {
                ids.extend(cp.responses.iter().flatten().cloned());
                if let Some(Cumulative(Some(t))) = &cp.cumulative {
                    let m = cumulative.get_or_insert_with(Tokens::default);
                    *m = Tokens {
                        input: m.input.max(t.input),
                        cached: m.cached.max(t.cached),
                        cache_write_5m: m.cache_write_5m.max(t.cache_write_5m),
                        cache_write_1h: m.cache_write_1h.max(t.cache_write_1h),
                        output: m.output.max(t.output),
                        reasoning: m.reasoning.max(t.reasoning),
                    };
                }
            }
            ids.sort();
            ids.dedup();
            codex_checkpoint(ids, cumulative)
        }
    }
}

/// A Codex checkpoint: both fields, as the Python writes them.
fn codex_checkpoint(responses: Vec<PyStr>, cumulative: Option<Tokens>) -> Checkpoint {
    Checkpoint {
        messages: None,
        responses: Some(responses),
        cumulative: Some(Cumulative(cumulative)),
    }
}

/// What is new in a Codex session since `prior` (`usage._new_codex`): the responses not yet
/// charged; for a file with no `token_usage_record`, the `token_count` total's floored
/// difference from the largest total reached. With the checkpoint reached and any mismatch
/// with the thread's own total. Unknown, reaching nothing, when no session file is found or
/// it cannot be read or has neither.
fn new_codex_usage(
    thread: Option<&PyStr>,
    sessions_dir: &Path,
    prior: &Checkpoint,
) -> (Usage, Checkpoint, Option<Mismatch>) {
    let unknown = || (Usage::unknown(), codex_checkpoint(Vec::new(), None), None);
    let Some(path) = thread.and_then(|t| codex::codex_session_file(t, sessions_dir)) else {
        return unknown();
    };
    let Ok(rollout) = codex::codex_rollout(&path) else {
        return unknown();
    };
    if !rollout.responses.is_empty() {
        let charged: HashSet<&PyStr> = prior.responses.iter().flatten().collect();
        let new: Vec<&(PyStr, Usage)> = rollout
            .responses
            .iter()
            .filter(|(id, _)| !charged.contains(id))
            .collect();
        let records = codex::sum(rollout.responses.iter().map(|(_, u)| u)).tokens;
        let mismatch = rollout
            .thread_total
            .filter(|t| t.tokens != records)
            .map(|t| Mismatch {
                records,
                thread_token_usage: t.tokens,
            });
        let mut ids: Vec<PyStr> = new.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        let used = codex::sum(new.iter().map(|(_, u)| u));
        return (used, codex_checkpoint(ids, None), mismatch);
    }
    if let Some(total) = rollout.token_count_total {
        let reached = Usage {
            tokens: prior
                .cumulative
                .as_ref()
                .and_then(|c| c.0)
                .unwrap_or_default(),
            ..Usage::default()
        };
        let delta = subtract(&total, &reached);
        return (
            delta,
            codex_checkpoint(Vec::new(), Some(total.tokens)),
            None,
        );
    }
    unknown()
}

/// Charges what is new in `source` since the checkpoint earlier charges reached, exactly once,
/// entirely under the run's state lock (`usage.charge`); returns the charge.
///
/// A `charge_id` that already exists is returned unchanged, and nothing is appended. A
/// missing or unreadable source, or none known, appends an unknown charge that reaches no
/// checkpoint, so nothing is lost when the file appears later. A relative transcript path is
/// an error, and appends nothing. `possibly_incomplete` marks the charge of an attempt that
/// was stopped (a response in flight leaves no record anywhere).
pub fn charge(
    paths: &RunPaths,
    charge_id: &PyStr,
    key: &PyStr,
    dispatch_id: &str,
    role: &str,
    source: ChargeSource,
    possibly_incomplete: bool,
) -> Result<Charge> {
    if charge_id.is_empty() {
        return Err(Error::msg("charge_id is required"));
    }
    if let ChargeSource::ClaudeTranscript(Some(path)) = source
        && !path.is_absolute()
    {
        return Err(Error::msg(format!(
            "transcript path must be absolute: {}",
            pure_path(path.as_os_str()).display()
        )));
    }
    let _lock = store::state_lock(&paths.root)?;
    let earlier = read_charges(paths)?;
    if let Some(found) = earlier.iter().find(|c| c.charge_id == *charge_id) {
        return Ok(found.clone());
    }
    let (stored, usage, checkpoint, mismatch) = match source {
        ChargeSource::ClaudeTranscript(path) => {
            let prior = charged_checkpoint(&earlier, Source::Claude, key);
            let (used, reached) =
                new_claude_usage(path, prior.messages.as_ref().unwrap_or(&PyMap::default()));
            let checkpoint = Checkpoint {
                messages: Some(reached),
                ..Checkpoint::default()
            };
            (Source::Claude.as_str(), used, checkpoint, None)
        }
        ChargeSource::Codex {
            thread,
            sessions_dir,
            stream,
        } => {
            let prior = charged_checkpoint(&earlier, Source::Codex, key);
            let (used, checkpoint, mismatch) = new_codex_usage(thread, sessions_dir, &prior);
            let fallback = match stream {
                Some(stream) if !used.known => Some(codex::codex_stream_usage(stream)),
                _ => None,
            };
            match fallback {
                Some(streamed) if streamed.known => {
                    (CODEX_STREAM_SOURCE, streamed, checkpoint, mismatch)
                }
                _ => (Source::Codex.as_str(), used, checkpoint, mismatch),
            }
        }
    };
    let record = Charge {
        charge_id: charge_id.clone(),
        source: stored.to_string(),
        key: key.clone(),
        dispatch_id: dispatch_id.to_string(),
        role: role.to_string(),
        usage,
        checkpoint,
        ts: utc_now(),
        possibly_incomplete,
        mismatch,
    };
    store::append_jsonl(&paths.root, &paths.charges, &record)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::process::Command;

    use super::*;
    use crate::state::run_paths;
    use crate::usage::claude::tests::{OPUS, claude_line};
    use crate::usage::codex::tests::{
        ROLLOUT, append_lines, codex_line, fixture_lines, rollout_path,
    };
    use crate::usage::{Group, add, claude_usage, totals};
    use crate::wire::{self, tests::python};

    /// A run directory and the vendor folders beside it (the Python tests' `UsageCase`).
    struct Case {
        _tmp: tempfile::TempDir,
        tmp: std::path::PathBuf,
        paths: RunPaths,
        sessions: std::path::PathBuf,
    }

    impl Case {
        fn new() -> Case {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("run with spaces");
            fs::create_dir(&root).unwrap();
            Case {
                tmp: tmp.path().to_path_buf(),
                sessions: tmp.path().join("sessions"),
                paths: run_paths(&root),
                _tmp: tmp,
            }
        }

        fn claude(&self, n: u32, path: &Path) -> Charge {
            self.claude_as(&format!("d{n}:claude:agent1"), "agent1", n, path)
        }

        fn claude_as(&self, cid: &str, key: &str, n: u32, path: &Path) -> Charge {
            charge(
                &self.paths,
                &cid.into(),
                &key.into(),
                &format!("d{n}"),
                "implementer",
                ChargeSource::ClaudeTranscript(Some(path)),
                false,
            )
            .unwrap()
        }

        fn codex(&self, n: u32, thread: &str) -> Charge {
            self.codex_with(n, thread, None, false)
        }

        fn codex_with(
            &self,
            n: u32,
            thread: &str,
            stream: Option<&Path>,
            incomplete: bool,
        ) -> Charge {
            charge(
                &self.paths,
                &format!("d{n}:codex:{thread}").into(),
                &thread.into(),
                &format!("d{n}"),
                "implementer",
                ChargeSource::Codex {
                    thread: Some(&thread.into()),
                    sessions_dir: &self.sessions,
                    stream,
                },
                incomplete,
            )
            .unwrap()
        }

        fn charges(&self) -> Vec<Charge> {
            read_charges(&self.paths).unwrap()
        }

        fn implementer_total(&self) -> Usage {
            totals(&self.charges(), Group::Role)
                .get(&"implementer".into())
                .unwrap()
                .clone()
        }

        fn place_rollout(&self, count: Option<usize>) -> std::path::PathBuf {
            let f = rollout_path(&self.sessions, "thr1");
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            let lines = fixture_lines(ROLLOUT);
            let lines = &lines[..count.unwrap_or(lines.len())];
            fs::write(&f, lines.join("\n") + "\n").unwrap();
            f
        }

        fn place_transcript(&self, name: &str, count: Option<usize>) -> std::path::PathBuf {
            let f = self.tmp.join("projects/p/s/subagents/agent-agent1.jsonl");
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            let lines = fixture_lines(name);
            let lines = &lines[..count.unwrap_or(lines.len())];
            fs::write(&f, lines.join("\n") + "\n").unwrap();
            f
        }
    }

    fn line(mid: &str, out: u64) -> String {
        claude_line(mid, out, 0, 0, 0, 0)
    }

    fn io(u: &Usage) -> (u64, u64) {
        (u.tokens.input, u.tokens.output)
    }

    fn messages(c: &Charge) -> Vec<(String, u64)> {
        c.checkpoint
            .messages
            .as_ref()
            .unwrap()
            .0
            .iter()
            .map(|(k, v)| (k.lossy().into_owned(), *v))
            .collect()
    }

    fn ids(v: &[&str]) -> Vec<PyStr> {
        v.iter().map(|s| PyStr::from(*s)).collect()
    }

    // CATALOG: test_usage.py::ChargeClaudeTests::test_charge_claude_resumed_worker_counts_each_message_once
    #[test]
    fn charge_claude_resumed_worker_counts_each_message_once() {
        let c = Case::new();
        let f = c.tmp.join("projects/p/s/subagents/agent-agent1.jsonl");
        append_lines(
            &f,
            &[
                &claude_line("m1", 10, 100, 1000, 50, 0),
                &claude_line("m1", 20, 100, 1000, 50, 0), // a repeated block
                &claude_line("m2", 30, 5, 0, 0, 7),
            ],
        );
        let c1 = c.claude(1, &f);
        // A resumed fix appended to the same transcript.
        append_lines(
            &f,
            &[
                &claude_line("m3", 40, 11, 2000, 0, 0),
                &claude_line("m4", 50, 12, 3000, 9, 0),
            ],
        );
        let c2 = c.claude(2, &f);
        // An interrupted resume: one more message.
        append_lines(&f, &[&claude_line("m5", 60, 13, 0, 0, 0)]);
        let c3 = c.claude(3, &f);
        let outputs = [&c1, &c2, &c3].map(|x| x.usage.tokens.output);
        assert_eq!(outputs, [50, 90, 60]);
        let (total, counted) = claude_usage(&f, &[]).unwrap();
        assert_eq!(counted, ids(&["m1", "m2", "m3", "m4", "m5"]));
        assert_eq!(add(&add(&c1.usage, &c2.usage), &c3.usage), total);
        assert_eq!(c.implementer_total(), total);
        // A further charge with nothing new moves nothing.
        let c4 = c.claude(4, &f);
        assert_eq!(io(&c4.usage), (0, 0));
        assert!(c4.usage.known);
    }

    // CATALOG: test_usage.py::ChargeClaudeTests::test_charge_record_is_durable_and_round_trips
    #[test]
    fn charge_record_is_durable_and_round_trips() {
        let c = Case::new();
        let f = c.tmp.join("t.jsonl");
        append_lines(&f, &[&claude_line("m1", 10, 3, 0, 0, 0)]);
        let one = c.claude(1, &f);
        let text = fs::read_to_string(&c.paths.charges).unwrap();
        assert_eq!(text.lines().count(), 1);
        // The record as the Python writes it, field for field (its timestamp aside).
        let code = "import json, sys\nfrom pathlib import Path\nfrom leadhand import state, usage as u\n\
                    (c,) = u.read_charges(state.run_paths(Path(sys.argv[1])))\n\
                    print(json.dumps(u._charge_to_record(c), separators=(',', ':')))\n\
                    print(json.dumps(u.charged_checkpoint([c], 'claude', 'agent1')), \
                    json.dumps(u.charged_checkpoint([c], 'claude', 'other')))\n";
        let theirs = String::from_utf8(python(code, &[c.paths.root.as_os_str()])).unwrap();
        let mut theirs = theirs.lines();
        assert_eq!(theirs.next().unwrap(), text.trim_end());
        assert!(text.contains("\"charge_id\":\"d1:claude:agent1\""));
        let one = std::slice::from_ref(&one);
        assert_eq!(c.charges(), one);
        let mine = |key: &str| {
            wire::to_output(&charged_checkpoint(one, Source::Claude, &key.into())).unwrap()
        };
        assert_eq!(
            theirs.next().unwrap(),
            format!("{} {}", mine("agent1"), mine("other"))
        );
        assert_eq!(mine("agent1"), r#"{"messages": {"m1": 10}}"#);
    }

    // CATALOG: test_usage.py::ChargeClaudeTests::test_charge_tops_up_message_charged_mid_stream
    #[test]
    fn charge_tops_up_message_charged_mid_stream() {
        let c = Case::new();
        let f = c.tmp.join("live.jsonl");
        append_lines(&f, &[&claude_line("m1", 5, 100, 1000, 50, 0)]);
        let c1 = c.claude(1, &f);
        assert_eq!(io(&c1.usage), (100, 5));
        append_lines(
            &f,
            &[
                &claude_line("m1", 40, 100, 1000, 50, 0),
                &claude_line("m2", 7, 3, 0, 0, 0),
            ],
        );
        let c2 = c.claude(2, &f);
        // m1: only the 35 extra output (its input side was charged with its first record);
        // m2 in full.
        let t = c2.usage.tokens;
        assert_eq!(
            (t.input, t.cached, t.cache_write_5m, t.output),
            (3, 0, 0, 35 + 7)
        );
        assert_eq!(messages(&c2), [("m1".into(), 40), ("m2".into(), 7)]);
        let c3 = c.claude(3, &f); // nothing grew
        assert_eq!(c3.usage.tokens.output, 0);
        assert!(messages(&c3).is_empty());
        let (total, _) = claude_usage(&f, &[]).unwrap();
        assert_eq!(c.implementer_total(), total);
        let reached = charged_checkpoint(&c.charges(), Source::Claude, &"agent1".into());
        assert_eq!(
            wire::to_line(&reached).unwrap(),
            r#"{"messages":{"m1":40,"m2":7}}"#
        );
    }

    // CATALOG: test_usage.py::ChargeClaudeTests::test_charge_none_path_is_unknown_and_relative_path_is_rejected
    #[test]
    fn charge_none_path_is_unknown_and_relative_path_is_rejected() {
        let c = Case::new();
        let none = |cid: &str, source| {
            charge(
                &c.paths,
                &cid.into(),
                &"a".into(),
                "d1",
                "implementer",
                source,
                false,
            )
        };
        let claude = none("d1:claude:a", ChargeSource::ClaudeTranscript(None)).unwrap();
        assert!(!claude.usage.known);
        let codex = none(
            "d1:codex:t",
            ChargeSource::Codex {
                thread: None,
                sessions_dir: &c.sessions,
                stream: None,
            },
        )
        .unwrap();
        assert!(!codex.usage.known);
        assert_eq!(codex.checkpoint, nothing());
        let relative = Path::new("relative//agent.jsonl");
        let err = none(
            "d2:claude:a",
            ChargeSource::ClaudeTranscript(Some(relative)),
        )
        .unwrap_err();
        // The path as pathlib prints it.
        assert_eq!(
            err.message,
            "transcript path must be absolute: relative/agent.jsonl"
        );
        let err = none("", ChargeSource::ClaudeTranscript(None)).unwrap_err();
        assert_eq!(err.message, "charge_id is required");
        assert_eq!(c.charges().len(), 2);
    }

    fn nothing() -> Checkpoint {
        Checkpoint {
            messages: None,
            responses: Some(Vec::new()),
            cumulative: Some(Cumulative(None)),
        }
    }

    // CATALOG: test_usage.py::ChargeClaudeTests::test_repeated_charge_id_is_harmless
    #[test]
    fn repeated_charge_id_is_harmless() {
        let c = Case::new();
        let f = c.tmp.join("t.jsonl");
        append_lines(&f, &[&line("m1", 10)]);
        let first = c.claude(1, &f);
        append_lines(&f, &[&line("m2", 99)]);
        let again = c.claude(1, &f); // the same charge_id, the file has grown
        assert_eq!(again, first);
        assert_eq!(c.charges().len(), 1);
        // Codex too.
        append_lines(
            &rollout_path(&c.sessions, "thr1"),
            &[&codex_line(100, 0, 10, 0)],
        );
        let x = c.codex(1, "thr1");
        append_lines(
            &rollout_path(&c.sessions, "thr1"),
            &[&codex_line(500, 0, 50, 0)],
        );
        assert_eq!(c.codex(1, "thr1"), x);
        assert_eq!(c.charges().len(), 2);
    }

    // CATALOG: test_usage.py::ChargeClaudeTests::test_charge_missing_file_is_unknown_and_moves_no_checkpoint
    #[test]
    fn charge_missing_file_is_unknown_and_moves_no_checkpoint() {
        let c = Case::new();
        let f = c.tmp.join("later.jsonl");
        let one = c.claude(1, &f);
        assert!(!one.usage.known);
        let text = fs::read_to_string(&c.paths.charges).unwrap();
        assert!(text.contains("\"known\":false"), "{text}");
        let none = charged_checkpoint(std::slice::from_ref(&one), Source::Claude, &"agent1".into());
        assert_eq!(wire::to_line(&none).unwrap(), r#"{"messages":{}}"#);
        // The file appears later: everything is still chargeable.
        append_lines(&f, &[&claude_line("m1", 10, 2, 0, 0, 0)]);
        let two = c.claude(2, &f);
        assert!(two.usage.known);
        assert_eq!(io(&two.usage), (2, 10));
        // Codex: a missing session file.
        let ghost = c.codex(3, "ghost");
        assert!(!ghost.usage.known);
        let none = charged_checkpoint(&[ghost], Source::Codex, &"ghost".into());
        assert_eq!(
            wire::to_line(&none).unwrap(),
            r#"{"responses":[],"cumulative":{}}"#
        );
        // The totals say the run has an unknown charge.
        let all = c.charges();
        assert_eq!(crate::usage::unknown_count(&all), 2);
        assert!(!c.implementer_total().known);
    }

    // CATALOG: test_usage.py::CodexTests::test_charge_codex_cumulative_deltas
    #[test]
    fn charge_codex_cumulative_deltas() {
        let c = Case::new();
        let f = rollout_path(&c.sessions, "thr1");
        append_lines(&f, &[&codex_line(100, 0, 0, 0)]);
        let c1 = c.codex(1, "thr1");
        append_lines(&f, &[&codex_line(250, 0, 0, 0)]);
        let c2 = c.codex(2, "thr1");
        assert_eq!((c1.usage.tokens.input, c2.usage.tokens.input), (100, 150));
        let reached = charged_checkpoint(&[c1, c2], Source::Codex, &"thr1".into());
        assert_eq!(reached.cumulative.unwrap().0.unwrap().input, 250);
        // Interrupted: no new tokens, a zero delta, the checkpoint unchanged.
        let c3 = c.codex(3, "thr1");
        assert_eq!(c3.usage.tokens.input, 0);
        assert_eq!(c.implementer_total().tokens.input, 250);
    }

    // CATALOG: test_usage.py::CodexTests::test_codex_stream_fallback_charged_when_session_missing
    #[test]
    fn codex_stream_fallback_charged_when_session_missing() {
        let c = Case::new();
        let stream = c.tmp.join("stream.jsonl");
        fs::write(
            &stream,
            "{\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 100, \"cached_input_tokens\": 30, \
             \"output_tokens\": 25, \"reasoning_output_tokens\": 8}}\n",
        )
        .unwrap();
        let stream_usage = Usage {
            tokens: Tokens {
                input: 70,
                cached: 30,
                output: 25,
                reasoning: 8,
                ..Tokens::default()
            },
            ..Usage::default()
        };
        let s = c.codex_with(1, "thr9", Some(&stream), false);
        assert_eq!(s.source, CODEX_STREAM_SOURCE);
        assert_eq!(s.usage, stream_usage);
        assert_eq!(c.charges(), std::slice::from_ref(&s));
        assert_eq!(c.implementer_total(), stream_usage);
        assert_eq!(crate::usage::unknown_count(&c.charges()), 0);
        // A stream charge moves no Codex checkpoint: a later session file is charged from its
        // records.
        assert_eq!(
            charged_checkpoint(&[s], Source::Codex, &"thr9".into()),
            nothing()
        );
        // An unknown fallback (no turn completed) still gives an unknown Codex charge.
        let empty = c.tmp.join("empty.jsonl");
        fs::write(&empty, "{\"type\": \"thread.started\"}\n").unwrap();
        let u = c.codex_with(2, "thr8", Some(&empty), false);
        assert_eq!((u.source.as_str(), u.usage.known), ("codex", false));
        // With a session file, the fallback is ignored.
        append_lines(
            &rollout_path(&c.sessions, "thr1"),
            &[&codex_line(100, 0, 0, 0)],
        );
        let s = c.codex_with(3, "thr1", Some(&stream), false);
        assert_eq!((s.source.as_str(), s.usage.tokens.input), ("codex", 100));
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_rollout_charges_every_response_not_the_token_count_total
    #[test]
    fn rollout_charges_every_response_not_the_token_count_total() {
        let c = Case::new();
        c.place_rollout(None);
        let one = c.codex(1, "thr1");
        assert!(one.usage.known);
        // 39874 input of which 26112 cached; 64 output: 39938 tokens, not 20818.
        let t = one.usage.tokens;
        assert_eq!(
            (t.input, t.cached, t.output, t.reasoning),
            (39874 - 26112, 26112, 64, 0)
        );
        assert_eq!(t.input + t.cached + t.output, 39938);
        assert_eq!(
            one.checkpoint.responses,
            Some(ids(&["resp_fixture_1", "resp_fixture_2"]))
        );
        assert_eq!(one.mismatch, None);
        assert!(!one.possibly_incomplete);
        let cumulative = crate::usage::codex_cumulative(&"thr1".into(), &c.sessions);
        assert_eq!(cumulative.tokens.input, 39874 - 26112);
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_rollout_charged_twice_charges_nothing_new
    #[test]
    fn rollout_charged_twice_charges_nothing_new() {
        let c = Case::new();
        c.place_rollout(None);
        let first = c.codex(1, "thr1");
        let second = c.codex(2, "thr1");
        assert!(second.usage.known);
        let t = second.usage.tokens;
        assert_eq!((t.input, t.cached, t.output), (0, 0, 0));
        assert_eq!(c.implementer_total(), first.usage);
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_rollout_truncated_before_resume_charges_only_the_killed_turn
    #[test]
    fn rollout_truncated_before_resume_charges_only_the_killed_turn() {
        let c = Case::new();
        c.place_rollout(Some(18)); // the killed turn: no token_count was ever written
        let killed = c.codex(1, "thr1");
        assert!(killed.usage.known);
        let t = killed.usage.tokens;
        assert_eq!((t.input, t.cached, t.output), (19062 - 7168, 7168, 58));
        assert_eq!(killed.checkpoint.responses, Some(ids(&["resp_fixture_1"])));
        // The resume appends to the same file: only the resumed turn is new.
        c.place_rollout(None);
        let resumed = c.codex(2, "thr1");
        let t = resumed.usage.tokens;
        assert_eq!((t.input, t.cached, t.output), (20812 - 18944, 18944, 6));
        let t = c.implementer_total().tokens;
        assert_eq!((t.input, t.cached, t.output), (39874 - 26112, 26112, 64));
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_rollout_repeated_response_id_counts_once
    #[test]
    fn rollout_repeated_response_id_counts_once() {
        let c = Case::new();
        let f = c.place_rollout(None);
        append_lines(&f, &[&fixture_lines(ROLLOUT)[16]]); // the first record, repeated
        let one = c.codex(1, "thr1");
        assert_eq!(
            (one.usage.tokens.cached, one.usage.tokens.output),
            (26112, 64)
        );
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_rollout_thread_total_mismatch_is_recorded_not_fatal
    #[test]
    fn rollout_thread_total_mismatch_is_recorded_not_fatal() {
        let c = Case::new();
        let f = c.place_rollout(None);
        let mut lines = fixture_lines(ROLLOUT);
        lines[27] = lines[27].replace(
            "\"thread_token_usage\":{\"input_tokens\":39874,\"cached_input_tokens\":26112,\"cache_write_input_tokens\":0,\"output_tokens\":64",
            "\"thread_token_usage\":{\"input_tokens\":39874,\"cached_input_tokens\":26112,\"cache_write_input_tokens\":0,\"output_tokens\":1000",
        );
        assert!(
            lines[27].contains("\"output_tokens\":1000"),
            "{}",
            lines[27]
        );
        fs::write(&f, lines.join("\n") + "\n").unwrap();
        let one = c.codex(1, "thr1");
        assert_eq!(one.usage.tokens.output, 64); // still charged from the records
        let mismatch = one.mismatch.clone().unwrap();
        assert_eq!(
            (mismatch.thread_token_usage.output, mismatch.records.output),
            (1000, 64)
        );
        assert_eq!(c.charges()[0], one);
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_possibly_incomplete_flag_is_recorded_on_the_charge
    #[test]
    fn possibly_incomplete_flag_is_recorded_on_the_charge() {
        let c = Case::new();
        c.place_rollout(Some(18));
        let one = c.codex_with(1, "thr1", None, true);
        assert!(one.possibly_incomplete);
        let text = fs::read_to_string(&c.paths.charges).unwrap();
        assert!(text.contains("\"possibly_incomplete\":true"), "{text}");
        assert_eq!(c.charges()[0], one);
        // Not set, it is not written.
        let two = c.codex(2, "thr1");
        let text = fs::read_to_string(&c.paths.charges).unwrap();
        assert!(!two.possibly_incomplete);
        assert_eq!(text.matches("possibly_incomplete").count(), 1);
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_rollout_without_any_usable_record_is_unknown
    #[test]
    fn rollout_without_any_usable_record_is_unknown() {
        let c = Case::new();
        c.place_rollout(Some(3));
        assert!(!c.codex(1, "thr1").usage.known);
    }

    // CATALOG: test_usage.py::CodexRolloutFixtureTests::test_token_count_is_only_a_fallback_for_a_rollout_without_records
    #[test]
    fn token_count_is_only_a_fallback_for_a_rollout_without_records() {
        let c = Case::new();
        let f = c.place_rollout(None);
        let lines: Vec<String> = fixture_lines(ROLLOUT)
            .into_iter()
            .filter(|l| !l.contains("\"token_usage_record\""))
            .collect();
        fs::write(&f, lines.join("\n") + "\n").unwrap();
        let t = c.codex(1, "thr1").usage.tokens;
        assert_eq!((t.input, t.cached, t.output), (20812 - 18944, 18944, 6));
    }

    // CATALOG: test_usage.py::ClaudeFixtureTests::test_stopped_transcript_charges_its_one_message_once
    #[test]
    fn stopped_transcript_charges_its_one_message_once() {
        let c = Case::new();
        let f = c.place_transcript("claude-subagent-stopped.jsonl", None);
        let one = c.claude(1, &f);
        let t = one.usage.tokens;
        assert_eq!(
            (
                t.input,
                t.cached,
                t.cache_write_5m,
                t.cache_write_1h,
                t.output
            ),
            (2, 2077, 5811, 0, 117)
        );
        assert_eq!(messages(&one), [("msg_fixture_1".into(), 117)]);
        let again = c.claude(2, &f);
        assert_eq!(io(&again.usage), (0, 0));
    }

    // CATALOG: test_usage.py::ClaudeFixtureTests::test_resumed_transcript_charges_each_message_once
    #[test]
    fn resumed_transcript_charges_each_message_once() {
        let c = Case::new();
        let f = c.place_transcript("claude-subagent-resumed.jsonl", None);
        let t = c.claude(1, &f).usage.tokens;
        // 734 per message ID, not 1468.
        assert_eq!(
            (t.input, t.cached, t.cache_write_5m, t.output),
            (12, 27653, 6384, 734)
        );
    }

    // CATALOG: test_usage.py::ClaudeFixtureTests::test_resumed_file_growth_is_charged_once
    #[test]
    fn resumed_file_growth_is_charged_once() {
        let c = Case::new();
        let f = c.place_transcript("claude-subagent-resumed.jsonl", Some(30)); // before the resume
        assert_eq!(io(&c.claude(1, &f).usage), (4, 99 + 411));
        c.place_transcript("claude-subagent-resumed.jsonl", None); // the resume appended
        assert_eq!(io(&c.claude(2, &f).usage), (8, 111 + 113));
        assert_eq!(io(&c.claude(3, &f).usage), (0, 0));
        let (total, _) = claude_usage(&f, &[]).unwrap();
        assert_eq!(c.implementer_total(), total);
    }

    /// Charges a transcript and a thread from the Python reference.
    const PY_CHARGE: &str = "import sys\nfrom pathlib import Path\nfrom leadhand import state, usage as u\n\
        root, cid, transcript, sessions = sys.argv[1:5]\n\
        p = state.run_paths(Path(root))\n\
        u.charge(p, charge_id=cid + ':claude', source='claude', key='agent1', dispatch_id='d1',\n\
        \x20        role='implementer', path_or_thread=Path(transcript))\n\
        u.charge(p, charge_id=cid + ':codex', source='codex', key='thr1', dispatch_id='d1',\n\
        \x20        role='implementer', path_or_thread='thr1', sessions_dir=Path(sessions))\n";

    #[test]
    fn charge_after_python_charge_continues_checkpoint() {
        let c = Case::new();
        let f = c.tmp.join("projects/p/s/subagents/agent-agent1.jsonl");
        // Python charges the start: m1 mid-stream, m2 whole; and the killed Codex turn.
        append_lines(
            &f,
            &[
                &claude_line("m1", 5, 100, 1000, 50, 0),
                &claude_line("m2", 7, 3, 0, 0, 4),
            ],
        );
        c.place_rollout(Some(18));
        let args = |cid: &str| -> Vec<std::ffi::OsString> {
            vec![
                c.paths.root.clone().into(),
                cid.into(),
                f.clone().into(),
                c.sessions.clone().into(),
            ]
        };
        let a = args("py1");
        python(
            PY_CHARGE,
            &a.iter().map(|s| s.as_os_str()).collect::<Vec<_>>(),
        );
        // The transcript grows (m1 tops up, m3 is new) and the resumed turn lands; Rust
        // charges the rest.
        append_lines(
            &f,
            &[
                &claude_line("m1", 40, 100, 1000, 50, 0),
                &claude_line("m3", 9, 1, 2, 0, 0),
            ],
        );
        c.place_rollout(None);
        let claude = c.claude_as("rs1:claude", "agent1", 2, &f);
        let codex = charge(
            &c.paths,
            &"rs1:codex".into(),
            &"thr1".into(),
            "d2",
            "implementer",
            ChargeSource::Codex {
                thread: Some(&"thr1".into()),
                sessions_dir: &c.sessions,
                stream: None,
            },
            false,
        )
        .unwrap();
        // Only what Python had not charged: m1's extra 35 output, m3 whole; the resumed turn.
        let t = claude.usage.tokens;
        assert_eq!((t.input, t.cached, t.output), (1, 2, 35 + 9));
        assert_eq!(codex.checkpoint.responses, Some(ids(&["resp_fixture_2"])));
        // Rust's charges and Python's sum to the whole transcript and the whole rollout.
        let all = c.charges();
        assert_eq!(all.len(), 4);
        let by_source = totals(&all, Group::Source);
        let (whole, _) = claude_usage(&f, &[]).unwrap();
        assert_eq!(by_source.get(&"claude".into()).unwrap(), &whole);
        let t = by_source.get(&"codex".into()).unwrap().tokens;
        assert_eq!((t.input, t.cached, t.output), (39874 - 26112, 26112, 64));
        // And Python, charging again after Rust, finds nothing new: it reads Rust's records
        // and continues from their checkpoints.
        let b = args("py2");
        python(
            PY_CHARGE,
            &b.iter().map(|s| s.as_os_str()).collect::<Vec<_>>(),
        );
        let last = c.charges();
        let after: Vec<(u64, u64)> = last[4..].iter().map(|x| io(&x.usage)).collect();
        assert_eq!(after, [(0, 0), (0, 0)]);
        let code = "import json, sys\nfrom pathlib import Path\nfrom leadhand import state, usage as u\n\
                    cs = u.read_charges(state.run_paths(Path(sys.argv[1])))\n\
                    t = u.totals(cs, by='source')\n\
                    print(json.dumps({k: u.usage_to_dict(v) for k, v in t.items()}, separators=(',', ':')))\n";
        let theirs = python(code, &[c.paths.root.as_os_str()]);
        let ours = format!(
            "{}\n",
            wire::to_line(&totals(&last, Group::Source)).unwrap()
        );
        assert_eq!(String::from_utf8(theirs).unwrap(), ours);
    }

    #[test]
    fn mixed_model_usage_keeps_by_model_and_prices_each() {
        let c = Case::new();
        let opus_file = c.tmp.join("opus.jsonl");
        let sonnet_file = c.tmp.join("sonnet.jsonl");
        append_lines(&opus_file, &[&claude_line("m1", 1000, 1000, 0, 0, 0)]);
        append_lines(
            &sonnet_file,
            &[&claude_line("m2", 2000, 2000, 0, 0, 0).replace(OPUS, "claude-sonnet-4-5")],
        );
        let opus = c.claude_as("c1", "a", 1, &opus_file);
        let sonnet = c.claude_as("c2", "b", 2, &sonnet_file);
        let total = totals(&c.charges(), Group::Role)
            .get(&"implementer".into())
            .unwrap()
            .clone();
        assert_eq!(total.model, None);
        let parts = total.by_model.as_ref().unwrap();
        assert_eq!(parts.get(&OPUS.into()), Some(&opus.usage.tokens));
        assert_eq!(
            parts.get(&"claude-sonnet-4-5".into()),
            Some(&sonnet.usage.tokens)
        );
        let prices = |input: f64, output: f64| crate::config::Prices {
            input,
            output,
            cache_read: input / 10.0,
            cache_write_5m: input * 1.25,
            cache_write_1h: input * 2.0,
        };
        let table = vec![
            ("opus".to_string(), prices(15.0, 75.0)),
            ("sonnet".into(), prices(3.0, 15.0)),
        ];
        let cost = crate::usage::estimate_cost(&total, None, &table).unwrap();
        let each = |u: &Usage| crate::usage::estimate_cost(u, u.model.as_ref(), &table).unwrap();
        assert_eq!(cost, each(&opus.usage) + each(&sonnet.usage));
        // The serialized record is the Python's for the same sum, and so is its price.
        let code = "import json\nfrom leadhand import state, usage as u\n\
                    a = u.Usage(input=1000, output=1000, model='claude-opus-4-1')\n\
                    b = u.Usage(input=2000, output=2000, model='claude-sonnet-4-5')\n\
                    p = lambda i, o: {'input': i, 'output': o, 'cache_read': i / 10, 'cache_write_5m': i * 1.25, 'cache_write_1h': i * 2.0}\n\
                    s = u.add(a, b)\n\
                    print(json.dumps(u.usage_to_dict(s), separators=(',', ':')))\n\
                    print(repr(u.estimate_cost(s, None, {'opus': p(15.0, 75.0), 'sonnet': p(3.0, 15.0)})))\n";
        let theirs = String::from_utf8(python(code, &[])).unwrap();
        let mut theirs = theirs.lines();
        assert_eq!(theirs.next().unwrap(), wire::to_line(&total).unwrap());
        assert_eq!(theirs.next().unwrap(), crate::pytext::py_float(cost));
        // A charge reading one transcript of both models keeps the split itself.
        let both = c.tmp.join("both.jsonl");
        let (opus_text, sonnet_text) = (
            fs::read_to_string(&opus_file).unwrap(),
            fs::read_to_string(&sonnet_file).unwrap(),
        );
        append_lines(&both, &[opus_text.trim_end(), sonnet_text.trim_end()]);
        let mixed = c.claude_as("c3", "c", 3, &both);
        assert_eq!(mixed.usage, total);
        let text = fs::read_to_string(&c.paths.charges).unwrap();
        assert!(
            text.lines()
                .last()
                .unwrap()
                .contains("\"model\":null,\"by_model\":{"),
            "{text}"
        );
    }

    #[test]
    fn single_model_usage_serializes_without_by_model() {
        let c = Case::new();
        let f = c.tmp.join("t.jsonl");
        // Two messages of one model, and a synthetic record with no tokens: no part.
        append_lines(
            &f,
            &[
                &claude_line("m1", 10, 1, 0, 0, 0),
                &claude_line("m2", 20, 2, 0, 0, 0),
                &claude_line("m3", 0, 0, 0, 0, 0).replace(OPUS, "<synthetic>"),
            ],
        );
        let one = c.claude(1, &f);
        assert_eq!(
            (one.usage.model.clone(), one.usage.by_model.clone()),
            (Some(OPUS.into()), None)
        );
        let line = fs::read_to_string(&c.paths.charges).unwrap();
        assert!(!line.contains("by_model"), "{line}");
        let code = "import json\nfrom leadhand import state, usage as u\n\
                    s = u.Usage()\n\
                    for x in (u.Usage(input=1, output=10, model='claude-opus-4-1'), u.Usage(input=2, output=20, model='claude-opus-4-1'), u.Usage(model='<synthetic>')):\n\
                    \x20   s = u.add(s, x)\n\
                    print(json.dumps(u.usage_to_dict(s), separators=(',', ':')))\n";
        let theirs = String::from_utf8(python(code, &[])).unwrap();
        assert_eq!(theirs.trim_end(), wire::to_line(&one.usage).unwrap());
    }

    /// Charges from Rust and from the Python reference at once, each appending to the source
    /// files as it goes (`test_concurrent_supervisor_and_runner_charges_keep_exact_totals`, one
    /// writer in each implementation).
    #[test]
    fn concurrent_rust_and_python_charges_keep_exact_totals() {
        let c = Case::new();
        let n: u64 = 12;
        let claude_file = c.tmp.join("projects/p/S.jsonl");
        append_lines(&claude_file, &[&claude_line("seed", 3, 1, 0, 0, 0)]);
        append_lines(
            &rollout_path(&c.sessions, "thr1"),
            &[&codex_line(100, 20, 5, 0)],
        );
        // Python is writer A: Claude messages; Rust is writer B: Codex totals. Each charges
        // both sources after each line.
        let code = "import sys, json\nfrom pathlib import Path\nfrom leadhand import state, usage as u\n\
            root, claude_file, sessions, n = sys.argv[1:5]\n\
            p = state.run_paths(Path(root))\n\
            print('ready', flush=True)\n\
            sys.stdin.readline()\n\
            for i in range(int(n)):\n\
            \x20   line = {'type': 'assistant', 'message': {'id': f'A-{i}', 'model': 'claude-opus-4-1', 'usage': {\n\
            \x20       'input_tokens': 2, 'cache_read_input_tokens': 7, 'output_tokens': i + 1}}}\n\
            \x20   with open(claude_file, 'a', encoding='utf-8') as f:\n\
            \x20       f.write(json.dumps(line) + '\\n')\n\
            \x20   u.charge(p, charge_id=f'A{i}:claude:S', source='claude', key='S', dispatch_id=f'A{i}',\n\
            \x20            role='supervisor', path_or_thread=Path(claude_file))\n\
            \x20   u.charge(p, charge_id=f'A{i}:codex:T', source='codex', key='thr1', dispatch_id=f'A{i}',\n\
            \x20            role='supervisor', path_or_thread='thr1', sessions_dir=Path(sessions))\n";
        let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/lib");
        let mut child = Command::new("python3")
            .args(["-B", "-c", code])
            .arg(&c.paths.root)
            .arg(&claude_file)
            .arg(&c.sessions)
            .arg(n.to_string())
            .env("PYTHONPATH", &lib)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run python3");
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.as_mut().unwrap()),
            &mut ready,
        )
        .unwrap();
        assert_eq!(ready, "ready\n");
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"go\n").unwrap();
        let claude_source = ChargeSource::ClaudeTranscript(Some(&claude_file));
        let thread: PyStr = "thr1".into();
        for i in 0..n {
            append_lines(
                &rollout_path(&c.sessions, "thr1"),
                &[&codex_line(100 + 10 * (i + 1), 20, 5 * (i + 2), 0)],
            );
            let did = format!("B{i}");
            charge(
                &c.paths,
                &format!("{did}:claude:S").into(),
                &"S".into(),
                &did,
                "implementer",
                claude_source,
                false,
            )
            .unwrap();
            let codex = ChargeSource::Codex {
                thread: Some(&thread),
                sessions_dir: &c.sessions,
                stream: None,
            };
            charge(
                &c.paths,
                &format!("{did}:codex:T").into(),
                &thread,
                &did,
                "implementer",
                codex,
                false,
            )
            .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The final charges pick up whatever the last writer added after the last charge.
        c.claude_as("final:claude:S", "S", 99, &claude_file);
        c.codex(99, "thr1");
        let charges = c.charges();
        let mut seen: Vec<&PyStr> = charges.iter().map(|x| &x.charge_id).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), charges.len());
        assert_eq!(charges.len() as u64, 2 * 2 * n + 2);
        let by = totals(&charges, Group::Source);
        let claude = by.get(&"claude".into()).unwrap().tokens;
        // Claude: the seed and A's n messages (output i+1, input 2, cached 7).
        assert_eq!(claude.output, 3 + (1..=n).sum::<u64>());
        assert_eq!(claude.input, 1 + 2 * n);
        assert_eq!(claude.cached, 7 * n);
        // Codex: B's last total is (100 + 10n input, 20 cached, 5(n+1) output).
        let codex = by.get(&"codex".into()).unwrap().tokens;
        assert_eq!(
            (codex.input, codex.cached, codex.output),
            (100 + 10 * n - 20, 20, 5 * (n + 1))
        );
        // Every line is whole, and the Python reads them all.
        let text = fs::read_to_string(&c.paths.charges).unwrap();
        assert_eq!(text.lines().count(), charges.len());
        let count = python(
            "import sys\nfrom pathlib import Path\nfrom leadhand import state, usage as u\n\
             print(len(u.read_charges(state.run_paths(Path(sys.argv[1])))))\n",
            &[OsStr::new(&c.paths.root)],
        );
        assert_eq!(count, format!("{}\n", charges.len()).into_bytes());
    }
}
