//! Usage accounting (the Python `usage` module): the token counts a charge carries, their sums,
//! totals per group and the estimated cost; `charges.jsonl` read typed; and the Codex meter
//! (`meter`).
//!
//! - `claude`: Claude transcripts, located, and read once per message ID;
//! - `codex`: Codex rollouts (once per `response_id`) and `--json` streams;
//! - `charges`: charging what is new exactly once, from the checkpoint earlier charges reached.
//!
//! A `Usage` mixing models keeps each model's tokens in `by_model` (with `model` `None`), so
//! a total is priced per model; the key `""` stands for records without a model.

pub mod charges;
pub mod claude;
pub mod codex;
pub mod meter;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub use charges::{
    CODEX_STREAM_SOURCE, ChargeSource, Checkpoint, Cumulative, Mismatch, Source, charge,
    charged_checkpoint,
};
pub use claude::{
    claude_session_transcript, claude_subagent_transcript, claude_usage, new_claude_usage,
    projects_dir,
};
pub use codex::{codex_cumulative, codex_session_file, codex_stream_usage};
pub use meter::{METER_SCAN_FILES, Meter, codex_meter};

use serde::{Deserialize, Serialize};

use crate::config::Prices;
use crate::error::Result;
use crate::state::RunPaths;
use crate::store;
use crate::wire::{PyMap, PyStr, RawLine};

/// The token fields, in the Python's order (`usage._TOKEN_FIELDS`).
pub const TOKEN_FIELDS: [&str; 6] = [
    "input",
    "cached",
    "cache_write_5m",
    "cache_write_1h",
    "output",
    "reasoning",
];

/// The model families that have prices (`usage.PRICE_FAMILIES`).
pub const PRICE_FAMILIES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// One usage's token counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tokens {
    pub input: u64,
    pub cached: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Tokens {
    /// Whether any count is not zero.
    pub fn any(&self) -> bool {
        *self != Tokens::default()
    }

    /// Field-wise sum, saturating: a vendor's count can be as large as `u64` holds.
    fn plus(&self, o: &Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_add(o.input),
            cached: self.cached.saturating_add(o.cached),
            cache_write_5m: self.cache_write_5m.saturating_add(o.cache_write_5m),
            cache_write_1h: self.cache_write_1h.saturating_add(o.cache_write_1h),
            output: self.output.saturating_add(o.output),
            reasoning: self.reasoning.saturating_add(o.reasoning),
        }
    }

    /// Field-wise difference, floored at 0.
    fn minus(&self, o: &Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(o.input),
            cached: self.cached.saturating_sub(o.cached),
            cache_write_5m: self.cache_write_5m.saturating_sub(o.cache_write_5m),
            cache_write_1h: self.cache_write_1h.saturating_sub(o.cache_write_1h),
            output: self.output.saturating_sub(o.output),
            reasoning: self.reasoning.saturating_sub(o.reasoning),
        }
    }
}

/// Usage (`usage.Usage`): token counts, whether they are known, and the model, or per-model
/// counts when the usage mixes models. Written with the token fields flat, as the Python's
/// `usage_to_dict` writes it; `by_model` is left out when there is none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "UsageRecord", into = "UsageRecord")]
pub struct Usage {
    pub tokens: Tokens,
    pub known: bool,
    pub model: Option<PyStr>,
    pub by_model: Option<PyMap<Tokens>>,
}

impl Default for Usage {
    /// Nothing used, known (the Python's `Usage()`).
    fn default() -> Usage {
        Usage {
            tokens: Tokens::default(),
            known: true,
            model: None,
            by_model: None,
        }
    }
}

impl Usage {
    /// Usage that could not be read (`usage.UNKNOWN`).
    pub fn unknown() -> Usage {
        Usage {
            known: false,
            ..Usage::default()
        }
    }
}

/// `Usage` as a record holds it: the token fields flat.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct UsageRecord {
    input: u64,
    cached: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    output: u64,
    reasoning: u64,
    known: bool,
    model: Option<PyStr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    by_model: Option<PyMap<Tokens>>,
}

impl Default for UsageRecord {
    fn default() -> UsageRecord {
        Usage::default().into()
    }
}

impl From<UsageRecord> for Usage {
    fn from(r: UsageRecord) -> Usage {
        Usage {
            tokens: Tokens {
                input: r.input,
                cached: r.cached,
                cache_write_5m: r.cache_write_5m,
                cache_write_1h: r.cache_write_1h,
                output: r.output,
                reasoning: r.reasoning,
            },
            known: r.known,
            model: r.model,
            by_model: r.by_model,
        }
    }
}

impl From<Usage> for UsageRecord {
    fn from(u: Usage) -> UsageRecord {
        let t = u.tokens;
        UsageRecord {
            input: t.input,
            cached: t.cached,
            cache_write_5m: t.cache_write_5m,
            cache_write_1h: t.cache_write_1h,
            output: t.output,
            reasoning: t.reasoning,
            known: u.known,
            model: u.model,
            by_model: u.by_model,
        }
    }
}

/// The usage's tokens per model (`usage._parts`): its `by_model` when it has one, else its
/// own tokens under its model (`""` for none). Usage with no tokens at all is no part.
fn parts(u: &Usage) -> PyMap<Tokens> {
    match &u.by_model {
        Some(by_model) if !by_model.is_empty() => by_model.clone(),
        _ if u.tokens.any() => PyMap(vec![(u.model.clone().unwrap_or_default(), u.tokens)]),
        _ => PyMap::default(),
    }
}

/// Field-wise sum (`usage.add`). The result is known only if both are: a total that includes
/// an unknown part must not pass for a complete one. Tokens of different models are kept per
/// model too (`by_model`, `model` `None`), so the total is priced per model.
pub fn add(a: &Usage, b: &Usage) -> Usage {
    let mut sum = parts(a);
    for (model, tokens) in parts(b).0 {
        let mine = sum.entry(&model, Tokens::default);
        *mine = mine.plus(&tokens);
    }
    let (model, by_model) = match sum.len() {
        2.. => (None, Some(sum)),
        1 => (Some(sum.0[0].0.clone()).filter(|m| !m.is_empty()), None),
        _ if a.model == b.model || b.model.is_none() => (a.model.clone(), None),
        _ if a.model.is_none() => (b.model.clone(), None),
        _ => (None, None),
    };
    Usage {
        tokens: a.tokens.plus(&b.tokens),
        known: a.known && b.known,
        model,
        by_model,
    }
}

/// Field-wise `a - b`, floored at 0 (`usage.subtract`): keeps `a`'s `known` and `model`, and
/// no `by_model`.
pub fn subtract(a: &Usage, b: &Usage) -> Usage {
    Usage {
        tokens: a.tokens.minus(&b.tokens),
        known: a.known,
        model: a.model.clone(),
        by_model: None,
    }
}

/// `"opus"` for `claude-opus-*`, and so on, by substring of the lowercased model; `None` for
/// any other model (all of Codex's) (`usage.price_family`).
pub fn price_family(model: Option<&PyStr>) -> Option<&'static str> {
    let low = model.map(|m| m.lossy().to_lowercase()).unwrap_or_default();
    PRICE_FAMILIES.into_iter().find(|f| low.contains(f))
}

/// The estimated API-equivalent cost in dollars: tokens times $/MTok (`usage.estimate_cost`).
/// `None` for unknown usage or an unpriced model. Usage mixing models is priced per model
/// (a part without a model takes `model`), and an unpriced part makes the whole estimate
/// unknown rather than silently low. Reasoning tokens are not priced on their own.
pub fn estimate_cost(u: &Usage, model: Option<&PyStr>, prices: &[(String, Prices)]) -> Option<f64> {
    if !u.known {
        return None;
    }
    match &u.by_model {
        Some(by_model) if !by_model.is_empty() => {
            let mut total = 0.0;
            for (m, tokens) in &by_model.0 {
                let m = if m.is_empty() { model } else { Some(m) };
                total += price(tokens, m, prices)?;
            }
            Some(total)
        }
        _ => price(&u.tokens, model, prices),
    }
}

fn price(t: &Tokens, model: Option<&PyStr>, prices: &[(String, Prices)]) -> Option<f64> {
    let family = price_family(model)?;
    let (_, p) = prices.iter().find(|(name, _)| name == family)?;
    // The Python's order of operations, so the float result is the same.
    Some(
        (t.input as f64 * p.input
            + t.output as f64 * p.output
            + t.cached as f64 * p.cache_read
            + t.cache_write_5m as f64 * p.cache_write_5m
            + t.cache_write_1h as f64 * p.cache_write_1h)
            / 1_000_000.0,
    )
}

/// One charge (`charges.jsonl`, contract item 4), in the Python's field order
/// (`usage._charge_to_record`). The fields the Python's `_charge_from_record` requires are
/// required here; a missing or `null` checkpoint is an empty one. `possibly_incomplete` is
/// written only when set, `mismatch` only when there is one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Charge {
    pub charge_id: PyStr,
    pub source: String,
    pub key: PyStr,
    pub dispatch_id: String,
    pub role: String,
    pub usage: Usage,
    #[serde(default, deserialize_with = "charges::null_as_default")]
    pub checkpoint: Checkpoint,
    pub ts: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub possibly_incomplete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatch: Option<Mismatch>,
}

/// Every charge in the run's log, typed (`usage.read_charges`).
pub fn read_charges(paths: &RunPaths) -> Result<Vec<Charge>> {
    let lines: Vec<(Charge, RawLine)> = store::read_jsonl(&paths.root, &paths.charges)?;
    Ok(lines.into_iter().map(|(c, _)| c).collect())
}

/// What `totals` sums charges by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Role,
    DispatchId,
    Source,
    Key,
}

/// Charged usage summed per group, in the order groups first appear (`usage.totals`). A group
/// containing any unknown charge is itself not known.
pub fn totals(charges: &[Charge], by: Group) -> PyMap<Usage> {
    let mut out: PyMap<Usage> = PyMap::default();
    for c in charges {
        let group = match by {
            Group::Role => PyStr::from(c.role.as_str()),
            Group::DispatchId => PyStr::from(c.dispatch_id.as_str()),
            Group::Source => PyStr::from(c.source.as_str()),
            Group::Key => c.key.clone(),
        };
        let total = out.entry(&group, Usage::default);
        *total = add(total, &c.usage);
    }
    out
}

/// How many charges have unknown usage (`usage.unknown_count`).
pub fn unknown_count(charges: &[Charge]) -> usize {
    charges.iter().filter(|c| !c.usage.known).count()
}

/// `usage._safe_id`: an ID that can be part of a file name (not empty, no `/`, no NUL).
pub(crate) fn safe_id(id: &PyStr) -> bool {
    let b = id.as_wtf8();
    !b.is_empty() && !b.contains(&b'/') && !b.contains(&0)
}

/// What a locator's pattern ends with, after its `*` folders.
pub(crate) enum Locate<'a> {
    /// These literal components, matched when the path exists (`os.path.lexists`).
    Path(&'a [&'a OsStr]),
    /// Any entry whose name, decoded as `surrogateescape` decodes it, ends with these WTF-8
    /// bytes (`*<suffix>`).
    Suffix(&'a [u8]),
}

/// The newest match of `<base>/*/…/*/<last>`, with `wild` `*` folders, as `glob.glob` and
/// `usage._newest` find it: `*` matches no hidden name (one starting with `.`) and, before the
/// last component, only folders (followed through links). The newest is the greatest
/// (`st_mtime`, 0 when it cannot be read; then the path's text). The path is spelled as
/// `pathlib` spells it.
pub(crate) fn newest(base: &Path, wild: usize, last: Locate) -> Option<PathBuf> {
    let visible = |name: &OsStr| !name.as_bytes().starts_with(b".");
    let mut dirs = vec![crate::pytext::pure_path(base.as_os_str())];
    for _ in 0..wild {
        let mut next = Vec::new();
        for dir in &dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = dir.join(entry.file_name());
                if visible(&entry.file_name()) && path.is_dir() {
                    next.push(path);
                }
            }
        }
        dirs = next;
    }
    let mut found = Vec::new();
    for dir in dirs {
        match last {
            Locate::Path(parts) => {
                let path = parts.iter().fold(dir, |p, part| p.join(part));
                if std::fs::symlink_metadata(&path).is_ok() {
                    found.push(path);
                }
            }
            Locate::Suffix(suffix) => {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if visible(&name) && PyStr::from_os(&name).as_wtf8().ends_with(suffix) {
                        found.push(dir.join(name));
                    }
                }
            }
        }
    }
    found
        .into_iter()
        .map(|path| {
            let path = crate::pytext::pure_path(path.as_os_str());
            let mtime = std::fs::metadata(&path)
                .map_or(0.0, |m| m.mtime() as f64 + m.mtime_nsec() as f64 * 1e-9);
            (mtime, PyStr::from_os(path.as_os_str()), path)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, _, path)| path)
}

/// The parsed lines of a vendor file (`usage._json_lines`): the text decoded as UTF-8 with
/// replacement and split at universal newlines (`\n`, `\r\n`, `\r`, as Python's text mode
/// reads it; a final line without one counts too). A line holding none of `needles` (when
/// there are any) is skipped unparsed; the rest are stripped as `str.strip` strips them and
/// parsed, and a line that does not parse (a partial write, foreign noise) is skipped. `T`
/// reads leniently (`meter::Lenient`): only the fields it uses, a field of another type
/// being no value.
pub(crate) fn json_lines<T: serde::de::DeserializeOwned>(
    path: &Path,
    needles: &[&str],
) -> std::io::Result<Vec<T>> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    let mut rest: &str = &text;
    while !rest.is_empty() {
        let (line, next) = match rest.find(['\n', '\r']) {
            Some(at) if rest[at..].starts_with("\r\n") => (&rest[..at], &rest[at + 2..]),
            Some(at) => (&rest[..at], &rest[at + 1..]),
            None => (rest, ""),
        };
        rest = next;
        if !needles.is_empty() && !needles.iter().any(|n| line.contains(n)) {
            continue;
        }
        let line = crate::pytext::py_strip(line);
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str(line) {
            out.push(record);
        }
    }
    Ok(out)
}

/// `$CODEX_HOME/sessions`, else `~/.codex/sessions` (`usage.default_sessions_dir`).
pub fn default_sessions_dir() -> std::path::PathBuf {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(home) if !home.is_empty() => home.into(),
        _ => Path::new(&crate::pytext::expand_user("~".as_ref())).join(".codex"),
    };
    home.join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{self, tests::read_reference_text};

    fn tokens(input: u64, output: u64) -> Tokens {
        Tokens {
            input,
            output,
            ..Tokens::default()
        }
    }

    fn usage(t: Tokens, model: Option<&str>) -> Usage {
        Usage {
            tokens: t,
            model: model.map(PyStr::from),
            ..Usage::default()
        }
    }

    fn prices() -> Vec<(String, Prices)> {
        let p = |input, output| Prices {
            input,
            output,
            cache_read: input / 10.0,
            cache_write_5m: input * 1.25,
            cache_write_1h: input * 2.0,
        };
        vec![
            ("opus".into(), p(15.0, 75.0)),
            ("sonnet".into(), p(3.0, 15.0)),
        ]
    }

    // CATALOG: test_usage::ArithmeticTests::test_add_sums_fields_and_known_is_conjunction
    #[test]
    fn add_sums_fields_and_known_is_conjunction() {
        let a = Usage {
            tokens: Tokens {
                input: 1,
                cached: 2,
                cache_write_5m: 3,
                cache_write_1h: 4,
                output: 5,
                reasoning: 6,
            },
            ..Usage::default()
        };
        let b = Usage {
            tokens: Tokens {
                input: 10,
                cached: 20,
                cache_write_5m: 30,
                cache_write_1h: 40,
                output: 50,
                reasoning: 60,
            },
            ..Usage::default()
        };
        let s = add(&a, &b);
        assert_eq!(
            s.tokens,
            Tokens {
                input: 11,
                cached: 22,
                cache_write_5m: 33,
                cache_write_1h: 44,
                output: 55,
                reasoning: 66
            }
        );
        assert!(s.known);
        assert!(!add(&a, &Usage::unknown()).known);
        assert!(!add(&Usage::unknown(), &a).known);
    }

    // CATALOG: test_usage::ArithmeticTests::test_subtract_is_field_wise_and_floored_at_zero
    #[test]
    fn subtract_is_field_wise_and_floored_at_zero() {
        let a = Usage {
            tokens: Tokens {
                input: 5,
                cached: 1,
                output: 100,
                ..Tokens::default()
            },
            model: Some("m".into()),
            ..Usage::default()
        };
        let b = Usage {
            tokens: Tokens {
                input: 3,
                cached: 9,
                output: 40,
                ..Tokens::default()
            },
            known: false,
            ..Usage::default()
        };
        let d = subtract(&a, &b);
        assert_eq!(
            (d.tokens.input, d.tokens.cached, d.tokens.output),
            (2, 0, 60)
        );
        // a's `known` and model are kept; a per-model split is not.
        assert!(d.known);
        assert_eq!(d.model, Some(PyStr::from("m")));
        let mixed = add(
            &usage(tokens(1, 1), Some("x")),
            &usage(tokens(1, 1), Some("y")),
        );
        assert!(mixed.by_model.is_some());
        assert_eq!(subtract(&mixed, &Usage::default()).by_model, None);
    }

    #[test]
    fn mixed_models_keep_by_model_and_each_is_priced_by_its_own() {
        let opus = usage(tokens(1000, 100), Some("claude-opus-4"));
        let sonnet = usage(tokens(2000, 200), Some("claude-sonnet-4-5"));
        let sum = add(&add(&opus, &sonnet), &opus);
        assert_eq!(sum.model, None);
        let by_model = sum.by_model.as_ref().unwrap();
        assert_eq!(
            by_model.0,
            [
                (PyStr::from("claude-opus-4"), tokens(2000, 200)),
                (PyStr::from("claude-sonnet-4-5"), tokens(2000, 200)),
            ]
        );
        let cost = estimate_cost(&sum, None, &prices()).unwrap();
        let opus_cost = estimate_cost(&opus, opus.model.as_ref(), &prices()).unwrap();
        let sonnet_cost = estimate_cost(&sonnet, sonnet.model.as_ref(), &prices()).unwrap();
        assert!((cost - (2.0 * opus_cost + sonnet_cost)).abs() < 1e-12);
        // An unpriced part makes the whole estimate unknown.
        let codex = usage(tokens(5, 5), Some("gpt-6.1-terra"));
        assert_eq!(estimate_cost(&add(&sum, &codex), None, &prices()), None);
        // Records with no tokens are no part; one model keeps `model`.
        let empty = usage(Tokens::default(), Some("claude-sonnet-4-5"));
        let one = add(&opus, &empty);
        assert_eq!(
            (one.model.clone(), one.by_model.clone()),
            (opus.model.clone(), None)
        );
        // With no tokens on either side, the model is the one both, or either, name.
        let (none_a, none_b) = (
            usage(Tokens::default(), Some("m")),
            usage(Tokens::default(), None),
        );
        assert_eq!(add(&none_a, &none_b).model, Some(PyStr::from("m")));
        assert_eq!(add(&none_b, &none_a).model, Some(PyStr::from("m")));
        let other = usage(Tokens::default(), Some("n"));
        assert_eq!(add(&none_a, &other).model, None);
    }

    #[test]
    fn usage_serializes_as_the_python_writes_it() {
        let opus = usage(tokens(1000, 100), Some("claude-opus-4"));
        let sonnet = usage(tokens(2000, 200), Some("claude-sonnet-4-5"));
        let sum = add(&opus, &sonnet);
        // The reference's lines of the same opus usage, of its sum with the sonnet one, and of
        // its unknown usage.
        let want = read_reference_text("usage/serialized.jsonl");
        let got = [&opus, &sum, &Usage::unknown()]
            .map(|u| wire::to_line(u).unwrap())
            .join("\n");
        assert_eq!(format!("{got}\n"), want);
        // And reads back as the same usage.
        for line in want.lines() {
            let u: Usage = serde_json::from_str(line).unwrap();
            assert_eq!(wire::to_line(&u).unwrap(), line);
        }
        // A missing field reads as the Python's default.
        let bare: Usage = serde_json::from_str("{}").unwrap();
        assert_eq!(bare, Usage::default());
    }

    #[test]
    fn estimate_cost_matches_python() {
        let u = Usage {
            tokens: Tokens {
                input: 1234,
                cached: 56789,
                cache_write_5m: 1011,
                cache_write_1h: 1213,
                output: 1415,
                reasoning: 99,
            },
            ..Usage::default()
        };
        // The reference's estimates of the same usage at the same opus prices, for
        // `claude-Opus-4` (its `repr`) and for `gpt-5`.
        let want = read_reference_text("usage/estimate-cost.txt");
        let opus = Some(PyStr::from("claude-Opus-4"));
        let cost = estimate_cost(&u, opus.as_ref(), &prices()).unwrap();
        let gpt = estimate_cost(&u, Some(&PyStr::from("gpt-5")), &prices());
        assert_eq!(gpt, None);
        assert_eq!(format!("{} None\n", crate::pytext::py_float(cost)), want);
        assert_eq!(
            estimate_cost(&Usage::unknown(), opus.as_ref(), &prices()),
            None
        );
        assert_eq!(price_family(None), None);
    }

    #[test]
    fn totals_group_in_first_seen_order() {
        let charge = |role: &str, u: Usage| Charge {
            charge_id: "c".into(),
            source: "claude".into(),
            key: "k".into(),
            dispatch_id: "d1".into(),
            role: role.into(),
            usage: u,
            checkpoint: Checkpoint::default(),
            ts: "t".into(),
            possibly_incomplete: false,
            mismatch: None,
        };
        let charges = [
            charge("supervisor", usage(tokens(1, 1), None)),
            charge("implementer", usage(tokens(2, 2), None)),
            charge("supervisor", Usage::unknown()),
        ];
        let t = totals(&charges, Group::Role);
        let names: Vec<&[u8]> = t.0.iter().map(|(k, _)| k.as_wtf8()).collect();
        assert_eq!(names, [&b"supervisor"[..], b"implementer"]);
        assert!(!t.0[0].1.known && t.0[1].1.known);
        assert_eq!(t.0[0].1.tokens, tokens(1, 1));
        assert_eq!(unknown_count(&charges), 1);
        assert_eq!(totals(&charges, Group::DispatchId).len(), 1);
    }
}
