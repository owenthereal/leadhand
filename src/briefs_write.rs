//! Writing worker briefs (`briefs.write_brief`, `BRIEF_KINDS`, `REQUIRED`), the dispatch
//! message the supervisor hands a worker (`briefs.dispatch_message`), and the scratch files
//! every brief, report and reply lives in (`briefs.write_scratch_file`, `read_scratch_file`,
//! `remove_scratch_file`).
//!
//! A brief's text lines are contract output (ruling S1-R10): the templates are compiled in from
//! `scripts/lib/leadhand/templates/brief-<kind>.md` and filled as Python's `string.Template`
//! fills them. A brief is text as the Python holds it (`PyStr`, a path with an undecodable
//! byte carried as a surrogate escape) and is written as strict UTF-8, as the Python's
//! `open(..., "w", encoding="utf-8")` writes it, so such a path fails the write with the
//! Python's `UnicodeEncodeError` text.
//!
//! The scratch folder `<ws>/.leadhand/<card or run>/<dispatch id>/` is worker-writable, so it
//! is reached only through `SafeDir` (spec, bug class 5): a symlink or a non-folder on the way
//! is refused with the path, and nothing is read, written or removed through it.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::briefs::{CODEX_AGENT, SCRATCH, scratch_dir};
use crate::cli::human::shell_quote;
use crate::config::ROLE_AGENT;
use crate::deck::{Card, Deck};
use crate::error::{Error, Result};
use crate::pytext::{path_text, py_isspace, py_repr, py_splitlines_bytes, py_strip};
use crate::safedir::{FileProblem, SafeDir};
use crate::wire::{PyMap, PyStr};

/// The brief kinds, in the Python's order (`briefs.BRIEF_KINDS`).
pub const BRIEF_KINDS: [&str; 12] = [
    "implement",
    "fix",
    "answer",
    "continue",
    "integrate",
    "review",
    "rereview",
    "final_review",
    "final_fix",
    "security",
    "evidence",
    "verify",
];

/// The kinds about one card, which cannot be written without it (`briefs.CARD_KINDS`).
pub const CARD_KINDS: [&str; 7] = [
    "implement",
    "fix",
    "answer",
    "continue",
    "integrate",
    "review",
    "rereview",
];

/// The `extra` keys each kind needs (`briefs.REQUIRED`). Card kinds also need `CARD_REQUIRED`.
pub const REQUIRED: [(&str, &[&str]); 12] = [
    ("implement", &[]),
    ("fix", &["findings", "round"]),
    ("answer", &["ruling"]),
    ("continue", &[]),
    (
        "integrate",
        &["run_tip", "conflicts", "card_intent", "run_intent"],
    ),
    ("review", &["diff_range"]),
    ("rereview", &["diff_range", "findings"]),
    ("final_review", &["diff_range"]),
    ("final_fix", &["gate", "findings", "branch"]),
    ("security", &["diff_range"]),
    ("evidence", &["base_worktree", "cards"]),
    ("verify", &[]),
];

/// What every card kind needs besides its own keys (`briefs.CARD_REQUIRED`).
pub const CARD_REQUIRED: [&str; 2] = ["base", "branch"];

/// The kinds that need the implementer's report unless `test_first` is `off`
/// (`briefs.REPORT_KINDS`).
pub const REPORT_KINDS: [&str; 2] = ["review", "rereview"];

/// The line every brief carries in place of the worker contract (`briefs.CONTRACT_POINTER`).
pub const CONTRACT_POINTER: &str =
    "Your worker contract and report format are in your instructions.";

/// The `test_first` settings (`briefs.TEST_FIRST`).
pub const TEST_FIRST: [&str; 3] = ["when-cheap", "always", "off"];

/// Each kind's template (`templates/brief-<kind>.md`).
const TEMPLATES: [(&str, &str); 12] = [
    (
        "implement",
        include_str!("../scripts/lib/leadhand/templates/brief-implement.md"),
    ),
    (
        "fix",
        include_str!("../scripts/lib/leadhand/templates/brief-fix.md"),
    ),
    (
        "answer",
        include_str!("../scripts/lib/leadhand/templates/brief-answer.md"),
    ),
    (
        "continue",
        include_str!("../scripts/lib/leadhand/templates/brief-continue.md"),
    ),
    (
        "integrate",
        include_str!("../scripts/lib/leadhand/templates/brief-integrate.md"),
    ),
    (
        "review",
        include_str!("../scripts/lib/leadhand/templates/brief-review.md"),
    ),
    (
        "rereview",
        include_str!("../scripts/lib/leadhand/templates/brief-rereview.md"),
    ),
    (
        "final_review",
        include_str!("../scripts/lib/leadhand/templates/brief-final_review.md"),
    ),
    (
        "final_fix",
        include_str!("../scripts/lib/leadhand/templates/brief-final_fix.md"),
    ),
    (
        "security",
        include_str!("../scripts/lib/leadhand/templates/brief-security.md"),
    ),
    (
        "evidence",
        include_str!("../scripts/lib/leadhand/templates/brief-evidence.md"),
    ),
    (
        "verify",
        include_str!("../scripts/lib/leadhand/templates/brief-verify.md"),
    ),
];

// --------------------------------------------------------------------------
// Scratch files

/// Why a scratch file could not be reached (`briefs.ScratchError`, or the `OSError` the Python
/// raises for the file itself).
#[derive(Debug)]
pub enum ScratchError {
    /// A folder on the way is a symlink or not a folder: the refusal, naming its path.
    Refused(PyStr),
    /// The file, or a folder the walk could not open: the OS's reason, as `strerror`.
    Os(String, Error),
}

impl ScratchError {
    /// What the Python's note shows for it (`getattr(e, "strerror", None) or e`).
    pub fn reason(&self) -> PyStr {
        match self {
            ScratchError::Refused(text) => text.clone(),
            ScratchError::Os(reason, _) => PyStr::from(reason.as_str()),
        }
    }
}

impl From<ScratchError> for Error {
    fn from(e: ScratchError) -> Error {
        match e {
            ScratchError::Refused(text) => Error::msg(text.shown()),
            ScratchError::Os(_, e) => e,
        }
    }
}

/// `strerror` of an OS error: its text without Rust's ` (os error N)`.
pub(crate) fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(n) => text
            .strip_suffix(&format!(" (os error {n})"))
            .unwrap_or(&text)
            .to_string(),
        None => text,
    }
}

/// The scratch folder's components below the workspace, after `scratch_dir`'s check of the
/// dispatch ID.
fn scratch_parts(ws: &Path, card: Option<i64>, dispatch_id: &str) -> Result<[String; 3]> {
    scratch_dir(ws, card, dispatch_id)?;
    let folder = card.map_or_else(|| "run".to_string(), |n| n.to_string());
    Ok([SCRATCH.to_string(), folder, dispatch_id.to_string()])
}

/// The dispatch's scratch folder, reached from the workspace without following a symlink
/// (`briefs._open_scratch`): `None` when it is missing and not `create`. A component that is a
/// symlink or not a folder is refused with its path.
fn open_scratch(
    ws: &Path,
    card: Option<i64>,
    dispatch_id: &str,
    create: bool,
) -> std::result::Result<Option<SafeDir>, ScratchError> {
    let parts =
        scratch_parts(ws, card, dispatch_id).map_err(|e| ScratchError::Os(e.message.clone(), e))?;
    let rel: Vec<&OsStr> = parts.iter().map(OsStr::new).collect();
    let walked = if create {
        SafeDir::create_under(ws, &rel).map(Some)
    } else {
        SafeDir::open_under(ws, &rel)
    };
    walked.map_err(|e| diagnose(ws, &rel, e))
}

/// The Python's wording of a walk that failed: the first component that is a symlink or not a
/// folder is refused (`briefs._refusal`), kept lossless; otherwise the OS's reason.
fn diagnose(ws: &Path, rel: &[&OsStr], e: Error) -> ScratchError {
    if let Err(io) = std::fs::metadata(ws) {
        return ScratchError::Os(strerror(&io), e);
    }
    let mut shown = ws.to_path_buf();
    for name in rel {
        shown.push(name);
        let Ok(meta) = std::fs::symlink_metadata(&shown) else {
            break;
        };
        let what = if meta.file_type().is_symlink() {
            "a symlink"
        } else if !meta.is_dir() {
            "not a folder"
        } else {
            continue;
        };
        let mut text = PyStr::from("refused: the scratch path ");
        text.push(&path_text(&shown));
        text.push_str(&format!(
            " is {what}; remove it and run the command again (a worker may have planted it)"
        ));
        return ScratchError::Refused(text);
    }
    ScratchError::Os(e.message.clone(), e)
}

/// Atomically writes `name` in the dispatch's scratch folder, created as needed, never through
/// a symlink (`briefs.write_scratch_file`); the file's path. The folders are made before the
/// text is encoded, so text that is not UTF-8 fails with them made and no file written, as in
/// the Python.
pub fn write_scratch_file(
    ws: &Path,
    card: Option<i64>,
    dispatch_id: &str,
    name: &str,
    text: &PyStr,
) -> Result<PathBuf> {
    let dir = open_scratch(ws, card, dispatch_id, true)?.expect("a creating walk makes the folder");
    dir.write_file(OsStr::new(name), text.encode_utf8()?.as_bytes())?;
    Ok(scratch_dir(ws, card, dispatch_id)?.join(name))
}

/// The bytes of the regular file `name` in the dispatch's scratch folder, or `None` when it or
/// the folder is missing (`briefs.read_scratch_file`). Never through a symlink: a symlinked
/// folder on the way is refused; a symlink or anything but a regular file named `name` is the
/// OS's error (`Too many levels of symbolic links`) or `not a regular file`.
pub fn read_scratch_file(
    ws: &Path,
    card: Option<i64>,
    dispatch_id: &str,
    name: &str,
) -> std::result::Result<Option<Vec<u8>>, ScratchError> {
    let Some(dir) = open_scratch(ws, card, dispatch_id, false)? else {
        return Ok(None);
    };
    dir.read_file_checked(OsStr::new(name), u64::MAX)
        .map_err(|p| match p {
            FileProblem::Os(_, ref io) => ScratchError::Os(strerror(io), Error::from(p)),
            FileProblem::NotRegular(_) => {
                ScratchError::Os("not a regular file".into(), Error::from(p))
            }
            other => {
                let e = Error::from(other);
                ScratchError::Os(e.message.clone(), e)
            }
        })
}

/// Removes `name` from the dispatch's scratch folder if it is there; a symlink named `name` is
/// removed itself, never its target; a symlinked folder on the way is refused
/// (`briefs.remove_scratch_file`).
pub fn remove_scratch_file(
    ws: &Path,
    card: Option<i64>,
    dispatch_id: &str,
    name: &str,
) -> Result<()> {
    if let Some(dir) = open_scratch(ws, card, dispatch_id, false)? {
        dir.remove_file(OsStr::new(name))?;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Rendering

/// The text of `extra[key]` as `str(v)` gives it (`None` for a JSON `null`), when present.
fn value(extra: &PyMap<Option<PyStr>>, key: &str) -> Option<PyStr> {
    extra
        .get(&PyStr::from(key))
        .map(|v| v.clone().unwrap_or_else(|| PyStr::from("None")))
}

/// `extra.get(key) or default`: a missing, `null` or empty value gives `default`.
fn value_or(extra: &PyMap<Option<PyStr>>, key: &str, default: PyStr) -> PyStr {
    match extra.get(&PyStr::from(key)) {
        Some(Some(v)) if !v.is_empty() => v.clone(),
        _ => default,
    }
}

/// Whether a value counts as given (`str(extra.get(k, "")).strip()` is not empty).
fn given(extra: &PyMap<Option<PyStr>>, key: &str) -> bool {
    value(extra, key).is_some_and(|v| !strip_pystr(&v).is_empty())
}

/// `str.strip()` of carried text: whitespace is valid text, so a surrogate escape is kept.
pub(crate) fn strip_pystr(s: &PyStr) -> PyStr {
    trim_pystr(s, true)
}

/// `str.rstrip()` of carried text.
fn rstrip_pystr(s: &PyStr) -> PyStr {
    trim_pystr(s, false)
}

/// Python's whitespace cut from the end of `s`, and from its start when `left`.
fn trim_pystr(s: &PyStr, left: bool) -> PyStr {
    let bytes = s.as_wtf8();
    let is_space_at = |at: usize, len: usize| {
        bytes
            .get(at..at + len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|t| t.chars().next())
            .is_some_and(py_isspace)
    };
    let char_len = |lead: u8| match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    let mut start = 0;
    while left && start < bytes.len() {
        let len = char_len(bytes[start]);
        if !is_space_at(start, len) {
            break;
        }
        start += len;
    }
    let mut end = bytes.len();
    while end > start {
        // The last character starts at the last byte that is not a continuation byte.
        let at = (start..end)
            .rev()
            .find(|i| bytes[*i] & 0xC0 != 0x80)
            .unwrap_or(start);
        if !is_space_at(at, end - at) {
            break;
        }
        end = at;
    }
    PyStr::from_wtf8(bytes[start..end].to_vec()).unwrap_or_default()
}

/// `str.rstrip("\n")`.
fn rstrip_newlines(s: &PyStr) -> PyStr {
    s.strip_ascii(b"\n", false)
}

/// The card as the deck has it, field by field (`briefs._render_card`).
fn render_card(card: &Card) -> String {
    let mut lines = vec![format!("## Card {}: {}", card.number, card.title)];
    let mut field = |key: &str, value: &str| {
        if !value.is_empty() {
            lines.push(format!("- {key}: {value}"));
        }
    };
    field("Covers", &card.covers.join(", "));
    field("Role", &card.role);
    let hard = match (card.hard, card.hard_reason.is_empty()) {
        (false, _) => "no".to_string(),
        (true, true) => "yes".to_string(),
        (true, false) => format!("yes — {}", card.hard_reason),
    };
    field("Hard", &hard);
    field("Risk", &card.risk);
    let deps: Vec<String> = card.depends_on.iter().map(u32::to_string).collect();
    let deps = if deps.is_empty() {
        "none".to_string()
    } else {
        deps.join(", ")
    };
    field("Depends on", &deps);
    field("Source", &card.source);
    field("Allowed paths", &card.allowed_paths.join(", "));
    field("Out of scope", &card.out_of_scope);
    field("Done when", &card.done_when);
    field("Stop if", &card.stop_if);
    for item in &card.inferred {
        field("Inferred", item);
    }
    if let Some(r) = &card.route_pin {
        field("Route", &format!("{} {} {}", r.vendor, r.model, r.effort));
    }
    if !py_strip(&card.context).is_empty() {
        lines.push(String::new());
        lines.push(card.context.trim_matches('\n').to_string());
    }
    lines.join("\n")
}

/// The cards `spec` names (comma-separated numbers), else every deck card
/// (`briefs._selected_cards`).
fn selected_cards<'d>(deck: &'d Deck, spec: Option<&PyStr>) -> Result<Vec<&'d Card>> {
    let spec = match spec {
        Some(s) if !s.is_empty() => s.lossy().into_owned(),
        _ => return Ok(deck.cards.iter().collect()),
    };
    let mut out = Vec::new();
    for part in spec.split(',').map(py_strip) {
        if part.is_empty() {
            continue;
        }
        let card = part
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse::<u32>().ok())
            .flatten()
            .and_then(|n| deck.cards.iter().find(|c| c.number == n));
        match card {
            Some(c) => out.push(c),
            None => {
                return Err(Error::msg(format!(
                    "cards: no card {} in the deck",
                    py_repr(part)
                )));
            }
        }
    }
    Ok(out)
}

/// The cards' scope, as the run-level briefs list it (`briefs._card_list`).
fn card_list(cards: &[&Card]) -> String {
    let mut out = Vec::new();
    for c in cards {
        out.push(format!("- Card {}: {}", c.number, c.title));
        let allowed = if c.allowed_paths.is_empty() {
            "(none)".to_string()
        } else {
            c.allowed_paths.join(", ")
        };
        out.push(format!("  Allowed paths: {allowed}"));
        if !c.out_of_scope.is_empty() {
            out.push(format!("  Out of scope: {}", c.out_of_scope));
        }
        out.push(format!("  Done when: {}", c.done_when));
    }
    if out.is_empty() {
        "- (none)".to_string()
    } else {
        out.join("\n")
    }
}

/// The cards' allowed paths, each once, in order (`briefs._union_paths`).
fn union_paths(cards: &[&Card]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for c in cards {
        for p in &c.allowed_paths {
            if !seen.contains(&p.as_str()) {
                seen.push(p);
            }
        }
    }
    if seen.is_empty() {
        "(none)".to_string()
    } else {
        seen.join(", ")
    }
}

/// Where the worker's things are (`briefs._context`).
struct Context<'a> {
    deck: &'a Deck,
    card: Option<&'a Card>,
    attempt: i64,
    base: PyStr,
    branch: PyStr,
    brief: &'a Path,
    report: &'a Path,
    test_first: &'a str,
}

fn render_context(c: &Context) -> PyStr {
    let mut lines: Vec<PyStr> = vec![
        "## Where things are".into(),
        "".into(),
        CONTRACT_POINTER.into(),
        "".into(),
    ];
    let mut deck_line = PyStr::from("- Deck: ");
    deck_line.push(&path_text(&c.deck.path));
    lines.push(deck_line);
    if !c.deck.source.is_empty() {
        lines.push(format!("- Plan: {}", c.deck.source).into());
    }
    if let Some(card) = c.card {
        if !card.source.is_empty() {
            lines.push(format!("- Card source: {}", card.source).into());
        }
        if !card.allowed_paths.is_empty() {
            lines.push(format!("- Files: {}", card.allowed_paths.join(", ")).into());
        }
    }
    let rule = match c.test_first {
        "always" => {
            "always — write a failing test and see it fail before the code that makes it pass."
        }
        "when-cheap" => "when-cheap — write a failing test first whenever one is cheap to write.",
        _ => "off — test as you see fit.",
    };
    let line = |head: &str, value: &PyStr| {
        let mut out = PyStr::from(head);
        out.push(value);
        out
    };
    lines.push(line("- Base commit: ", &c.base));
    let branch_head = if c.card.is_some() {
        "- Card branch: "
    } else {
        "- Branch: "
    };
    lines.push(line(branch_head, &c.branch));
    lines.push(format!("- Attempt: {}", c.attempt).into());
    lines.push(line("- This brief: ", &path_text(c.brief)));
    lines.push(line("- Report file: ", &path_text(c.report)));
    lines.push(format!("- test_first: {rule}").into());
    if let Some(card) = c.card {
        lines.extend(["".into(), "## The card, verbatim".into(), "".into()]);
        lines.push(render_card(card).into());
    }
    PyStr::join(&lines, "\n")
}

/// `Template(template).substitute(values)`: each `$name` or `${name}` replaced by its value,
/// `$$` by `$`. A name with no value, or a `$` that starts none, is the Python's error.
fn substitute(kind: &str, template: &str, values: &[(String, PyStr)]) -> Result<PyStr> {
    let ident = |s: &str| {
        let mut end = 0;
        for (i, c) in s.char_indices() {
            let ok = c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit());
            if !ok {
                break;
            }
            end = i + c.len_utf8();
        }
        end
    };
    let lookup = |name: &str| {
        values
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| Error::msg(format!("brief-{kind}.md: cannot fill {}", py_repr(name))))
    };
    let mut out = PyStr::default();
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push_str("$");
            rest = tail;
        } else if let Some(braced) = after.strip_prefix('{')
            && let n = ident(braced)
            && n > 0
            && braced[n..].starts_with('}')
        {
            out.push(&lookup(&braced[..n])?);
            rest = &braced[n + 1..];
        } else if ident(after) > 0 {
            let n = ident(after);
            out.push(&lookup(&after[..n])?);
            rest = &after[n..];
        } else {
            return Err(Error::msg(format!(
                "brief-{kind}.md: cannot fill Invalid placeholder in string"
            )));
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// What a brief is written from (`write_brief`'s arguments).
pub struct BriefSpec<'a> {
    pub kind: &'a str,
    pub ws: &'a Path,
    pub deck: &'a Deck,
    pub card: Option<&'a Card>,
    pub attempt: i64,
    pub dispatch_id: &'a str,
    /// The keys `REQUIRED` names for the kind, and for card kinds `base` and `branch`; reviews
    /// also `implementer_report` unless `test_first` is `off`. Optional: `test_first` (default
    /// `when-cheap`), `cards` (accepted card numbers; required for evidence, default every deck
    /// card), `allowed_paths` for final_fix (default the union of those cards'), and for
    /// run-level kinds `base` and `branch` (default the deck's base commit and run branch).
    pub extra: &'a PyMap<Option<PyStr>>,
    /// Appended after the template, when not empty.
    pub notes: &'a PyStr,
}

/// Writes the brief for `spec.kind` (with its notes appended) in the dispatch's scratch
/// folder, and returns the brief's and the report's paths (`briefs.write_brief`). A symlink
/// or non-folder on the scratch folder's way is refused; a missing value or an unknown kind is
/// an error before anything is written.
pub fn write_brief(spec: &BriefSpec) -> Result<(PathBuf, PathBuf)> {
    let kind = spec.kind;
    let Some((_, template)) = TEMPLATES.iter().find(|(k, _)| *k == kind) else {
        return Err(Error::msg(format!("unknown brief kind {}", py_repr(kind))));
    };
    let card_kind = CARD_KINDS.contains(&kind);
    if card_kind && spec.card.is_none() {
        return Err(Error::msg(format!("a {kind} brief needs a card")));
    }
    let extra = spec.extra;
    let test_first = value_or(extra, "test_first", "when-cheap".into());
    let Some(test_first) = TEST_FIRST
        .into_iter()
        .find(|t| test_first.as_wtf8() == t.as_bytes())
    else {
        return Err(Error::msg(format!(
            "test_first must be one of {}, got {}",
            TEST_FIRST.join(", "),
            test_first.repr()
        )));
    };
    let mut needed: Vec<&str> = REQUIRED
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(&[][..], |(_, keys)| keys)
        .to_vec();
    if card_kind {
        needed.extend(CARD_REQUIRED);
    }
    if REPORT_KINDS.contains(&kind) && test_first != "off" {
        needed.push("implementer_report");
    }
    let mut missing: Vec<&str> = needed.into_iter().filter(|k| !given(extra, k)).collect();
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        return Err(Error::msg(format!(
            "{kind} brief needs {}",
            missing.join(", ")
        )));
    }

    let card_number = spec.card.map(|c| i64::from(c.number));
    let folder = scratch_dir(spec.ws, card_number, spec.dispatch_id)?;
    let (brief, report) = (folder.join("brief.md"), folder.join("report.md"));
    // `extra.get("cards")`: a `null` is no selection, so every card.
    let selection = extra.get(&PyStr::from("cards")).cloned().flatten();
    let cards = selected_cards(spec.deck, selection.as_ref())?;
    let mut values: Vec<(String, PyStr)> = extra
        .0
        .iter()
        .map(|(k, v)| {
            let v = v.clone().unwrap_or_else(|| PyStr::from("None"));
            (k.lossy().into_owned(), v)
        })
        .collect();
    let context = render_context(&Context {
        deck: spec.deck,
        card: spec.card,
        attempt: spec.attempt,
        base: value_or(extra, "base", spec.deck.base_sha.as_str().into()),
        branch: value_or(extra, "branch", spec.deck.branch.as_str().into()),
        brief: &brief,
        report: &report,
        test_first,
    });
    let ruling_quoted = quoted(&value(extra, "ruling").unwrap_or_default());
    values.extend([
        ("context".to_string(), context),
        ("card_list".to_string(), card_list(&cards).into()),
        (
            "allowed_paths".to_string(),
            value_or(extra, "allowed_paths", union_paths(&cards).into()),
        ),
        (
            "build".to_string(),
            non_empty_or(&spec.deck.build, "(none given; skip it)").into(),
        ),
        (
            "test".to_string(),
            non_empty_or(&spec.deck.test, "(none given; skip it)").into(),
        ),
        ("ruling_quoted".to_string(), ruling_quoted),
        (
            "implementer_report_line".to_string(),
            value_or(
                extra,
                "implementer_report",
                "(none; test_first is off)".into(),
            ),
        ),
    ]);
    let mut text = substitute(kind, template, &values)?;
    let common = crate::git::git_common_dir(spec.ws).map_err(Error::from)?;
    crate::git::workspace::ensure_excluded(&common, &format!("{SCRATCH}/"))?;
    text = rstrip_newlines(&text);
    text.push_str("\n");
    if !spec.notes.is_empty() {
        text.push_str("\n");
        text.push(&rstrip_newlines(spec.notes));
        text.push_str("\n");
    }
    write_scratch_file(spec.ws, card_number, spec.dispatch_id, "brief.md", &text)?;
    Ok((brief, report))
}

fn non_empty_or<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() { default } else { value }
}

/// Each line of a ruling quoted (`"\n".join(f"> {ln}".rstrip() for ln in text.splitlines())`).
/// A surrogate escape is never a line end (its WTF-8 form starts `0xED`), so the lines are
/// found in the WTF-8 bytes as in the text.
fn quoted(text: &PyStr) -> PyStr {
    let lines: Vec<PyStr> = py_splitlines_bytes(text.as_wtf8())
        .into_iter()
        .map(|line| {
            let mut out = PyStr::from("> ");
            out.push(&PyStr::from_wtf8(line.to_vec()).unwrap_or_default());
            rstrip_pystr(&out)
        })
        .collect();
    PyStr::join(&lines, "\n")
}

// --------------------------------------------------------------------------
// The dispatch message

/// The message for the dispatched agent (`briefs.dispatch_message`): for the Codex proxy
/// exactly one command line, every argument shell-quoted; for a Claude worker the brief, the
/// report file and the instruction.
pub fn dispatch_message(
    agent: &str,
    brief: &PyStr,
    report: &PyStr,
    codex_command: Option<&[OsString]>,
) -> Result<PyStr> {
    if agent == CODEX_AGENT {
        let Some(args) = codex_command.filter(|a| !a.is_empty()) else {
            return Err(Error::msg("the Codex proxy needs codex_command"));
        };
        let newline = |a: &OsString| a.as_bytes().iter().any(|b| matches!(b, b'\n' | b'\r'));
        if args.iter().any(newline) {
            return Err(Error::msg(
                "codex_command arguments must not contain newlines",
            ));
        }
        let mut line = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                line.push(b' ');
            }
            line.extend_from_slice(shell_quote(a).as_bytes());
        }
        return Ok(PyStr::from_os(OsStr::from_bytes(&line)));
    }
    if codex_command.is_some() {
        return Err(Error::msg(format!(
            "codex_command is only for {CODEX_AGENT}"
        )));
    }
    let known = ROLE_AGENT
        .iter()
        .any(|(_, name)| agent.strip_prefix("leadhand:") == Some(name));
    if !known {
        return Err(Error::msg(format!(
            "unknown worker agent {}",
            py_repr(agent)
        )));
    }
    let mut out = PyStr::from("Brief: ");
    out.push(brief);
    out.push_str("\nReport file: ");
    out.push(report);
    out.push_str(
        "\nRead the brief first and do the work it describes. Write your full report to the \
         report file, then reply in at most 15 lines, ending with `Report: <path>`.",
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::Route;
    use crate::git::fixture::Fixture;

    fn card() -> Card {
        Card {
            number: 3,
            title: "Parse the widget header".into(),
            role: "implementer".into(),
            hard: true,
            hard_reason: "touches the on-disk format".into(),
            risk: "high".into(),
            depends_on: vec![1, 2],
            covers: vec!["R1".into(), "R4".into()],
            source: "docs/plan.md#task-3".into(),
            allowed_paths: vec!["src/widget/**".into(), "tests/test_widget.py".into()],
            out_of_scope: "Rendering; the CLI flags.".into(),
            done_when: "`test_header_roundtrip` passes and `make check` is clean.".into(),
            stop_if: "the header needs a new version byte.".into(),
            inferred: vec!["\"Done when\" test name (plan named no test)".into()],
            route_pin: Some(Route {
                vendor: "codex".into(),
                model: "gpt-6.1-sol".into(),
                effort: "xhigh".into(),
            }),
            context: "The header is 16 bytes, little-endian.\nKeep the parser pure.".into(),
            missing: vec!["SENTINEL-missing".into()],
            line: 4242,
            unknown_keys: vec![(77, "SENTINEL-unknown".into())],
        }
    }

    fn deck(path: PathBuf) -> Deck {
        Deck {
            path,
            title: "Widget work".into(),
            source: "docs/plan.md".into(),
            base_ref: "main".into(),
            base_sha: "a".repeat(40),
            branch: "leadhand/widget".into(),
            build: "make build".into(),
            test: "make test".into(),
            profile: None,
            finish: None,
            requirements: Vec::new(),
            cards: vec![card()],
            digest: "0".repeat(64),
        }
    }

    /// The pairs as a Python dict holds them: a repeated key keeps its first place and its
    /// last value.
    fn extra(pairs: &[(&str, &str)]) -> PyMap<Option<PyStr>> {
        let mut map = PyMap::default();
        for (k, v) in pairs {
            *map.entry(&PyStr::from(*k), || None) = Some(PyStr::from(*v));
        }
        map
    }

    /// The Python fixture's extra for a kind (`test_briefs.EXTRA`), with a card's base and
    /// branch.
    fn kind_extra(kind: &str) -> Vec<(&'static str, &'static str)> {
        let mut out = vec![
            ("base", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ("branch", "leadhand/widget--card-3"),
        ];
        out.extend(match kind {
            "fix" => vec![("findings", "/x/findings.md"), ("round", "2")],
            "answer" => vec![("ruling", "Use option B: keep the old byte order.")],
            "integrate" => vec![
                ("run_tip", "cccccccccccccccccccccccccccccccccccccccc"),
                ("conflicts", "src/widget/header.py"),
                ("card_intent", "add a field"),
                ("run_intent", "rename the struct"),
            ],
            "review" => vec![
                ("diff_range", "abc123..def456"),
                ("implementer_report", "/x/impl report.md"),
            ],
            "rereview" => vec![
                ("diff_range", "abc123..def456"),
                ("findings", "/x/findings.md"),
                ("implementer_report", "/x/impl report.md"),
            ],
            "final_review" => vec![("diff_range", "base0..tip9"), ("cards", "3")],
            "final_fix" => vec![
                ("gate", "security"),
                ("findings", "/x/security.md"),
                ("branch", "leadhand/widget"),
            ],
            "security" => vec![("diff_range", "base0..tip9")],
            "evidence" => vec![("base_worktree", "/x/base tree"), ("cards", "3")],
            _ => vec![],
        });
        out
    }

    struct World {
        f: Fixture,
        ws: PathBuf,
        deck: Deck,
    }

    fn world() -> World {
        let f = Fixture::new();
        let ws = f.make_repo("work space");
        let deck = deck(ws.join("docs/my deck.md"));
        World { f, ws, deck }
    }

    impl World {
        fn brief(
            &self,
            kind: &str,
            with_card: bool,
            extra: &PyMap<Option<PyStr>>,
        ) -> Result<PathBuf> {
            let card = self.deck.cards[0].clone();
            write_brief(&BriefSpec {
                kind,
                ws: &self.ws,
                deck: &self.deck,
                card: with_card.then_some(&card),
                attempt: 1,
                dispatch_id: &format!("{kind}-1"),
                extra,
                notes: &PyStr::default(),
            })
            .map(|(brief, _)| brief)
        }
    }

    // CATALOG: test_briefs.py::BriefTests::test_evidence_requires_cards
    #[test]
    fn evidence_requires_cards() {
        let w = world();
        let e = w
            .brief("evidence", false, &extra(&[("base_worktree", "/x")]))
            .unwrap_err();
        assert!(e.message.contains("cards"), "{e}");
        assert_eq!(e.message, "evidence brief needs cards");
    }

    // `_selected_cards(deck, extra.get("cards"))`: a `null` selection is every card, not a
    // card named `None`.
    #[test]
    fn a_null_card_selection_is_every_card() {
        let mut w = world();
        let mut second = card();
        second.number = 4;
        second.title = "Render the widget".into();
        w.deck.cards.push(second);
        let brief = |kind: &str, cards: Option<&str>| {
            let mut map = extra(&kind_extra(kind));
            *map.entry(&PyStr::from("cards"), || None) = cards.map(PyStr::from);
            fs::read_to_string(w.brief(kind, false, &map).unwrap()).unwrap()
        };
        for kind in ["final_review", "evidence"] {
            let every = brief(kind, None);
            assert!(
                every.contains("- Card 4: Render the widget"),
                "{kind}: {every}"
            );
            assert_eq!(every, brief(kind, Some("3, 4")), "{kind}");
        }
    }

    // CATALOG: test_briefs.py::BriefTests::test_verify_says_skip_for_empty_commands
    #[test]
    fn verify_says_skip_for_empty_commands() {
        let mut w = world();
        w.deck.build = String::new();
        let brief = w
            .brief("verify", false, &extra(&kind_extra("verify")))
            .unwrap();
        let text = fs::read_to_string(brief).unwrap();
        assert!(text.contains("- Build: (none given; skip it)"), "{text}");
        assert!(text.contains("- Test: make test"), "{text}");
    }

    // CATALOG: test_briefs.py::BriefTests::test_missing_extra_and_unknown_kind_raise
    #[test]
    fn missing_extra_and_unknown_kind_raise() {
        let w = world();
        let e = w.brief("fix", true, &PyMap::default()).unwrap_err();
        assert_eq!(e.message, "fix brief needs base, branch, findings, round");
        let e = w
            .brief("polish", true, &extra(&kind_extra("implement")))
            .unwrap_err();
        assert_eq!(e.message, "unknown brief kind 'polish'");
        assert!(!w.ws.join(".leadhand").exists());
    }

    #[test]
    fn every_kind_matches_the_python_brief() {
        // Every kind, written by both from the same values, with notes and a path that has a
        // space: byte for byte the same file.
        let w = world();
        let code = "import json, sys\nfrom pathlib import Path\nfrom leadhand.briefs import write_brief\n\
            from leadhand.deck import Card, Deck\nfrom leadhand.config import Route\n\
            ws, deck_path, specs = Path(sys.argv[1]), Path(sys.argv[2]), json.loads(sys.argv[3])\n\
            card = Card(number=3, title='Parse the widget header', role='implementer', hard=True,\n\
            \x20   hard_reason='touches the on-disk format', risk='high', depends_on=(1, 2), covers=('R1', 'R4'),\n\
            \x20   source='docs/plan.md#task-3', allowed_paths=('src/widget/**', 'tests/test_widget.py'),\n\
            \x20   out_of_scope='Rendering; the CLI flags.',\n\
            \x20   done_when='`test_header_roundtrip` passes and `make check` is clean.',\n\
            \x20   stop_if='the header needs a new version byte.',\n\
            \x20   inferred=('\"Done when\" test name (plan named no test)',),\n\
            \x20   route_pin=Route('codex', 'gpt-6.1-sol', 'xhigh'),\n\
            \x20   context='The header is 16 bytes, little-endian.\\nKeep the parser pure.',\n\
            \x20   missing=('SENTINEL-missing',), line=4242, unknown_keys=((77, 'SENTINEL-unknown'),))\n\
            deck = Deck(path=deck_path, title='Widget work', source='docs/plan.md', base_ref='main',\n\
            \x20   base_sha='a' * 40, branch='leadhand/widget', build='make build', test='make test',\n\
            \x20   profile=None, finish=None, requirements=(), cards=(card,), digest='0' * 64)\n\
            out = []\n\
            for kind, with_card, extra, notes in specs:\n\
            \x20   brief, _ = write_brief(kind, ws=ws, deck=deck, card=card if with_card else None, attempt=2,\n\
            \x20                          dispatch_id=f'py-{kind}-1', extra=extra, notes=notes)\n\
            \x20   out.append(brief.read_text(encoding='utf-8'))\n\
            print(json.dumps(out))\n";
        let mut specs = Vec::new();
        for kind in BRIEF_KINDS {
            let with_card = CARD_KINDS.contains(&kind);
            let mut pairs = kind_extra(kind);
            if kind == "answer" {
                pairs.push(("ruling", "Use B.\n  Keep the old order.  \n\n"));
            }
            pairs.push((
                "test_first",
                if kind == "review" {
                    "always"
                } else {
                    "when-cheap"
                },
            ));
            let notes = if kind == "fix" {
                "## Previous attempt\n\nIt ended SPENT.\n\n\n"
            } else {
                ""
            };
            specs.push((kind, with_card, pairs, notes));
        }
        let json: Vec<serde_json::Value> = specs
            .iter()
            .map(|(k, c, pairs, notes)| {
                let map: serde_json::Map<String, serde_json::Value> = pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
                    .collect();
                serde_json::json!([k, c, map, notes])
            })
            .collect();
        let arg = serde_json::to_string(&json).unwrap();
        let theirs: Vec<String> = serde_json::from_slice(&crate::wire::tests::python(
            code,
            &[w.ws.as_os_str(), w.deck.path.as_os_str(), OsStr::new(&arg)],
        ))
        .unwrap();
        for ((kind, with_card, pairs, notes), want) in specs.iter().zip(theirs) {
            let card = w.deck.cards[0].clone();
            let (brief, report) = write_brief(&BriefSpec {
                kind,
                ws: &w.ws,
                deck: &w.deck,
                card: with_card.then_some(&card),
                attempt: 2,
                dispatch_id: &format!("rs-{kind}-1"),
                extra: &extra(pairs),
                notes: &PyStr::from(*notes),
            })
            .unwrap();
            assert_eq!(report, brief.with_file_name("report.md"));
            let ours = fs::read_to_string(&brief).unwrap();
            assert_eq!(
                ours,
                want.replace(&format!("py-{kind}-1"), &format!("rs-{kind}-1")),
                "{kind}"
            );
            assert!(!ours.contains('$'), "{kind}");
        }
        // The scratch folder is excluded, so the briefs leave the checkout clean.
        assert_eq!(
            w.f.git(&w.ws, &["status", "--porcelain", "--untracked-files=all"]),
            ""
        );
    }

    #[test]
    fn a_brief_is_never_written_through_a_symlink() {
        let w = world();
        let outside = w.f.root().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir_all(w.ws.join(".leadhand/3")).unwrap();
        std::os::unix::fs::symlink(&outside, w.ws.join(".leadhand/3/implement-1")).unwrap();
        let e = w
            .brief("implement", true, &extra(&kind_extra("implement")))
            .unwrap_err();
        let shown = w.ws.join(".leadhand/3/implement-1");
        assert_eq!(
            e.message,
            format!(
                "refused: the scratch path {} is a symlink; remove it and run the command again \
                 (a worker may have planted it)",
                shown.display()
            )
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        // Reading and removing are refused the same way; a missing folder reads as nothing.
        let read = read_scratch_file(&w.ws, Some(3), "implement-1", "report.md").unwrap_err();
        assert!(matches!(read, ScratchError::Refused(_)), "{read:?}");
        assert!(remove_scratch_file(&w.ws, Some(3), "implement-1", "reply.txt").is_err());
        assert_eq!(
            read_scratch_file(&w.ws, Some(3), "other-1", "report.md").unwrap(),
            None
        );
        // A symlinked file is the OS's error, worded as strerror.
        write_scratch_file(&w.ws, None, "d-1", "brief.md", &"x\n".into()).unwrap();
        let folder = w.ws.join(".leadhand/run/d-1");
        std::os::unix::fs::symlink(outside.join("x"), folder.join("report.md")).unwrap();
        let read = read_scratch_file(&w.ws, None, "d-1", "report.md").unwrap_err();
        assert_eq!(
            read.reason(),
            PyStr::from("Too many levels of symbolic links")
        );
        remove_scratch_file(&w.ws, None, "d-1", "report.md").unwrap();
        assert!(!folder.join("report.md").exists());
        assert_eq!(
            read_scratch_file(&w.ws, None, "d-1", "brief.md").unwrap(),
            Some(b"x\n".to_vec())
        );
    }

    #[test]
    fn dispatch_messages_match_python() {
        let brief = PyStr::from("/ws/.leadhand/1/c1-a1-implement-1/brief.md");
        let report = PyStr::from("/ws/.leadhand/1/c1-a1-implement-1/report.md");
        let claude = dispatch_message("leadhand:implementer", &brief, &report, None).unwrap();
        let argv: Vec<std::ffi::OsString> =
            ["/bin/lead hand", "dispatch", "--run", "it's $HOME; x"]
                .iter()
                .map(Into::into)
                .collect();
        let codex = dispatch_message(CODEX_AGENT, &brief, &report, Some(&argv)).unwrap();
        let code = "import json, sys\nfrom pathlib import Path\nfrom leadhand.briefs import dispatch_message\n\
                    b, r = Path(sys.argv[1]), Path(sys.argv[2])\n\
                    print(json.dumps([dispatch_message('leadhand:implementer', b, r),\n\
                    \x20   dispatch_message('leadhand:codex', b, r, codex_command=sys.argv[3:])]))";
        let mut args = vec![
            OsStr::new(brief.lossy().as_ref()).to_os_string(),
            report.lossy().as_ref().into(),
        ];
        args.extend(argv.iter().cloned());
        let args: Vec<&OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        let theirs: Vec<String> =
            serde_json::from_slice(&crate::wire::tests::python(code, &args)).unwrap();
        assert_eq!(
            [claude.lossy().into_owned(), codex.lossy().into_owned()].to_vec(),
            theirs
        );
        let bad: Vec<std::ffi::OsString> = vec!["a\nb".into()];
        assert_eq!(
            dispatch_message(CODEX_AGENT, &brief, &report, Some(&bad))
                .unwrap_err()
                .message,
            "codex_command arguments must not contain newlines"
        );
        assert_eq!(
            dispatch_message("leadhand:supervisor", &brief, &report, None)
                .unwrap_err()
                .message,
            "unknown worker agent 'leadhand:supervisor'"
        );
    }

    #[test]
    fn strip_matches_python_strip() {
        for (s, want) in [
            ("  a b \n", "a b"),
            ("\u{a0}\u{2003}x\u{3000}", "x"),
            ("\x1c y \x1f", "y"),
            ("", ""),
            ("   ", ""),
            ("caf\u{e9} ", "caf\u{e9}"),
        ] {
            assert_eq!(strip_pystr(&PyStr::from(s)), PyStr::from(want), "{s:?}");
        }
        let odd = PyStr::from_os(OsStr::from_bytes(b" a\xff "));
        assert_eq!(
            strip_pystr(&odd),
            PyStr::from_os(OsStr::from_bytes(b"a\xff"))
        );
    }
}
