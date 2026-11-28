//! Deck parsing and validation (the Python `deck` module; docs/deck-format.md).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::{
    Config, Route, in_file, parse_route, unsupported_in_repo, unsupported_settings,
    unsupported_warnings,
};
use crate::error::{Error, Failure};
use crate::git::Git;
use crate::pytext::{py_isspace, py_repr, py_splitlines, py_strip};
use crate::wire::PyStr;

/// Loading and checking keep a failure's text lossless (`Failure`); `?` turns it into an
/// `Error`.
type Result<T, E = Failure> = std::result::Result<T, E>;

pub const HEADER_KEYS: [&str; 7] = [
    "Source", "Base", "Branch", "Build", "Test", "Profile", "Finish",
];
pub const CARD_KEYS: [&str; 12] = [
    "Covers",
    "Role",
    "Hard",
    "Risk",
    "Depends on",
    "Source",
    "Allowed paths",
    "Out of scope",
    "Done when",
    "Stop if",
    "Inferred",
    "Route",
];
pub const REQUIRED: [&str; 6] = [
    "Role",
    "Hard",
    "Risk",
    "Allowed paths",
    "Done when",
    "Stop if",
];
pub const ROLES: [&str; 2] = ["implementer", "scout"];
pub const RISKS: [&str; 3] = ["low", "medium", "high"];
/// How long `check`'s base-commit lookup may take (`deck.GIT_TIMEOUT`).
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub number: u32,
    pub title: String,
    pub role: String,
    pub hard: bool,
    pub hard_reason: String,
    pub risk: String,
    pub depends_on: Vec<u32>,
    pub covers: Vec<String>,
    pub source: String,
    pub allowed_paths: Vec<String>,
    pub out_of_scope: String,
    pub done_when: String,
    pub stop_if: String,
    pub inferred: Vec<String>,
    pub route_pin: Option<Route>,
    pub context: String,
    /// Required fields absent or empty.
    pub missing: Vec<String>,
    /// The line of the card's heading.
    pub line: usize,
    /// (line, key) of bullets that look like fields but name none: treated as context.
    pub unknown_keys: Vec<(usize, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    pub id: String,
    pub text: String,
    pub cards: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deck {
    pub path: PathBuf,
    pub title: String,
    pub source: String,
    pub base_ref: String,
    pub base_sha: String,
    pub branch: String,
    pub build: String,
    pub test: String,
    pub profile: Option<String>,
    pub finish: Option<String>,
    pub requirements: Vec<Requirement>,
    pub cards: Vec<Card>,
    /// sha256 of the file's bytes, in hex.
    pub digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub level: Level,
    pub card: Option<u32>,
    pub message: PyStr,
    pub fix: Option<Vec<OsString>>,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
        }
    }
}

/// A deck that does not parse (`deck.DeckError`): `line <n>: <message>`, or the message alone
/// for line 0.
fn deck_error(message: impl fmt::Display, line: usize) -> Failure {
    if line == 0 {
        Failure::msg(&message.to_string())
    } else {
        Failure::msg(&format!("line {line}: {message}"))
    }
}

/// The deck at `path`. The file must be UTF-8; its digest is sha256 of its bytes, in hex.
pub fn parse_deck(path: &Path) -> Result<Deck> {
    let raw = std::fs::read(path).map_err(|e| Failure::io(path, e))?;
    let digest: String = Sha256::digest(&raw)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let text = std::str::from_utf8(&raw).map_err(|e| in_file(path, &format!("not UTF-8: {e}")))?;
    let lines = py_splitlines(text);
    let n = lines.len();
    let mut i = 0;
    while i < n && py_strip(lines[i]).is_empty() {
        i += 1;
    }
    let Some(title) = lines.get(i).and_then(|l| l.strip_prefix("# Deck:")) else {
        return Err(deck_error(
            "expected '# Deck: <title>' first",
            if i < n { i + 1 } else { 1 },
        ));
    };
    let title = py_strip(title).to_string();
    i += 1;

    let mut header: Vec<(&str, String)> = Vec::new();
    while i < n && !lines[i].starts_with("## ") {
        let text = py_strip(lines[i]);
        i += 1;
        if text.is_empty() {
            continue;
        }
        for part in split_header(text) {
            let Some((key, value)) = header_field(py_strip(part)) else {
                return Err(deck_error(
                    format!("unrecognized header line {}", py_repr(text)),
                    i,
                ));
            };
            if header.iter().any(|(k, _)| *k == key) {
                return Err(deck_error(format!("duplicate header {key}"), i));
            }
            header.push((key, py_strip(value).to_string()));
        }
    }
    let get = |key: &str| {
        header
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, v)| v.as_str())
    };
    for key in ["Source", "Base", "Branch"] {
        if get(key).is_empty() {
            return Err(deck_error(format!("missing header {key}"), 1));
        }
    }
    let (base_ref, base_sha) = match get("Base").split_once(" @ ") {
        Some((r, s)) if !py_strip(r).is_empty() && !py_strip(s).is_empty() => {
            (py_strip(r), py_strip(s))
        }
        _ => return Err(deck_error("Base must be '<ref> @ <sha>'", 1)),
    };

    let mut requirements = Vec::new();
    let mut cards = Vec::new();
    while i < n {
        let line = lines[i];
        let ln = i + 1;
        if py_strip(line) == "## Requirements" {
            i += 1;
            while i < n && !lines[i].starts_with("## ") {
                let text = py_strip(lines[i]);
                i += 1;
                if text.is_empty() {
                    continue;
                }
                let Some((id, req_text, numbers)) = requirement(text) else {
                    return Err(deck_error(
                        "requirement must be '- R<n> <text> \u{2192} cards <n>[, <n>]'",
                        i,
                    ));
                };
                requirements.push(Requirement {
                    id: id.to_string(),
                    text: py_strip(req_text).to_string(),
                    cards: ints(numbers, i, "requirement cards")?,
                });
            }
            continue;
        }
        if let Some((number, title)) = card_heading(line) {
            let number = card_number(number, ln)?;
            i += 1;
            let mut fields = Vec::new();
            let mut ctx = Vec::new();
            let mut unknown = Vec::new();
            let mut in_ctx = false;
            while i < n && !lines[i].starts_with("## ") {
                let text = lines[i];
                let fm = field_line(text);
                if let Some((key, value)) = fm
                    && CARD_KEYS.contains(&key)
                {
                    fields.push((key, py_strip(value), i + 1));
                    i += 1;
                    continue;
                }
                if let Some((key, _)) = fm {
                    unknown.push((i + 1, key.to_string()));
                }
                if !py_strip(text).is_empty() {
                    in_ctx = true;
                }
                if in_ctx {
                    ctx.push(text);
                }
                i += 1;
            }
            let joined = ctx.join("\n");
            let mut context = py_strip(&joined);
            if let Some(rest) = context.strip_prefix("Context:") {
                context = py_strip(rest);
            }
            let mut card = parse_card(number, title, ln, &fields, context)?;
            card.unknown_keys = unknown;
            cards.push(card);
            continue;
        }
        if !py_strip(line).is_empty() {
            return Err(deck_error(
                format!("unexpected text {}", py_repr(py_strip(line))),
                ln,
            ));
        }
        i += 1;
    }

    Ok(Deck {
        path: path.to_path_buf(),
        title,
        source: get("Source").to_string(),
        base_ref: base_ref.to_string(),
        base_sha: base_sha.to_string(),
        branch: get("Branch").to_string(),
        build: get("Build").to_string(),
        test: get("Test").to_string(),
        profile: Some(get("Profile").to_string()).filter(|p| !p.is_empty()),
        finish: Some(get("Finish").to_string()).filter(|f| !f.is_empty()),
        requirements,
        cards,
        digest,
    })
}

/// A header line split where two or more whitespace characters come before a known key and
/// its colon (`deck._HEADER_SPLIT`): the spec's compact header puts several fields on a line.
fn split_header(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !py_isspace(c) {
            continue;
        }
        // The whole run of whitespace from i.
        let mut end = i + c.len_utf8();
        let mut count = 1;
        while let Some(&(j, d)) = chars.peek() {
            if !py_isspace(d) {
                break;
            }
            end = j + d.len_utf8();
            count += 1;
            chars.next();
        }
        if count >= 2 && header_field(&text[end..]).is_some() {
            parts.push(&text[start..i]);
            start = end;
        }
    }
    parts.push(&text[start..]);
    parts
}

/// `<Key>:<value>` for a header key (`deck._HEADER_KV`).
fn header_field(part: &str) -> Option<(&'static str, &str)> {
    HEADER_KEYS.iter().find_map(|k| {
        let value = part.strip_prefix(k)?.strip_prefix(':')?;
        Some((*k, value))
    })
}

/// `- <Key>: <value>`, where the key is a capital letter then letters and spaces, up to the
/// first colon (`deck._FIELD`).
fn field_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("- ")?;
    let (key, value) = rest.split_once(':')?;
    let mut chars = key.chars();
    let first = chars.next()?;
    let ok = first.is_ascii_uppercase() && chars.all(|c| c.is_ascii_alphabetic() || c == ' ');
    ok.then(|| (key, value.trim_start_matches(py_isspace)))
}

/// `## Card <n>: <title>` (`deck._CARD`): the number's digits and the title.
fn card_heading(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("## Card ")?;
    let digits = leading_digits(rest);
    if digits == 0 {
        return None;
    }
    let title = rest[digits..].strip_prefix(':')?;
    Some((&rest[..digits], py_strip(title)))
}

fn leading_digits(s: &str) -> usize {
    s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len()
}

fn card_number(digits: &str, line: usize) -> Result<u32> {
    digits
        .parse()
        .map_err(|_| deck_error(format!("card number {digits} is too large"), line))
}

/// `- R<n> <text> → cards <n>[, <n>]` (`deck._REQ`): the ID, the text and the card list. The
/// arrow is `→` or `->`; the text runs to the last one that a `card`/`cards` list follows.
fn requirement(text: &str) -> Option<(&str, &str, &str)> {
    let rest = text.strip_prefix("- R")?;
    let digits = leading_digits(rest);
    if digits == 0 {
        return None;
    }
    let id = &text[2..3 + digits];
    let after_id = &rest[digits..];
    let body = after_id.trim_start_matches(py_isspace);
    if body.len() == after_id.len() {
        return None;
    }
    let arrows: Vec<(usize, usize)> = body
        .char_indices()
        .filter_map(|(i, _)| {
            ["\u{2192}", "->"]
                .iter()
                .find(|a| body[i..].starts_with(**a))
                .map(|a| (i, a.len()))
        })
        .collect();
    arrows.iter().rev().find_map(|&(at, len)| {
        let after = body[at + len..].trim_start_matches(py_isspace);
        let after = after.strip_prefix("card")?;
        // `cards?\s+(.+?)\s*$`: `card` or `cards`, whitespace, then the list.
        let list = [after.strip_prefix('s'), Some(after)]
            .into_iter()
            .flatten()
            .find_map(|a| {
                let list = a.trim_start_matches(py_isspace);
                (list.len() < a.len() && !list.is_empty()).then_some(list)
            })?;
        Some((id, &body[..at], list.trim_end_matches(py_isspace)))
    })
}

/// `deck._csv`.
fn csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(py_strip)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Card numbers, or none (`deck._ints`).
fn ints(text: &str, line: usize, what: &str) -> Result<Vec<u32>> {
    if py_strip(text).to_lowercase() == "none" {
        return Ok(Vec::new());
    }
    csv(text)
        .into_iter()
        .map(|p| {
            if !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(deck_error(
                    format!("{what}: {} is not a card number", py_repr(&p)),
                    line,
                ));
            }
            card_number(&p, line)
        })
        .collect()
}

/// `yes`, or `yes` then a dash or a colon and the reason (`deck._HARD`): the reason.
fn hard_yes(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("yes")?;
    if rest.is_empty() {
        return Some("");
    }
    let rest = rest.trim_start_matches(py_isspace);
    ["\u{2014}", "\u{2013}", "-", ":"]
        .iter()
        .find_map(|sep| rest.strip_prefix(sep))
}

fn parse_card(
    number: u32,
    title: &str,
    line: usize,
    fields: &[(&str, &str, usize)],
    context: &str,
) -> Result<Card> {
    let mut seen: Vec<(&str, &str, usize)> = Vec::new();
    let mut inferred = Vec::new();
    for &(key, value, ln) in fields {
        if key == "Inferred" {
            inferred.push(value.to_string());
            continue;
        }
        if seen.iter().any(|(k, _, _)| *k == key) {
            return Err(deck_error(format!("duplicate field {key}"), ln));
        }
        seen.push((key, value, ln));
    }
    let val = |key: &str| seen.iter().find(|f| f.0 == key).map_or("", |f| f.1);
    let at = |key: &str| seen.iter().find(|f| f.0 == key).map_or(line, |f| f.2);

    let role = val("Role");
    if !role.is_empty() && !ROLES.contains(&role) {
        return Err(deck_error(
            format!(
                "Role must be one of {}, got {}",
                ROLES.join(", "),
                py_repr(role)
            ),
            at("Role"),
        ));
    }
    let risk = val("Risk");
    if !risk.is_empty() && !RISKS.contains(&risk) {
        return Err(deck_error(
            format!(
                "Risk must be one of {}, got {}",
                RISKS.join(", "),
                py_repr(risk)
            ),
            at("Risk"),
        ));
    }
    let (mut hard, mut reason) = (false, "");
    let hv = val("Hard");
    if !hv.is_empty() && hv != "no" {
        let Some(why) = hard_yes(hv) else {
            return Err(deck_error(
                format!(
                    "Hard must be 'no' or 'yes \u{2014} <reason>', got {}",
                    py_repr(hv)
                ),
                at("Hard"),
            ));
        };
        (hard, reason) = (true, py_strip(why));
    }
    let route_pin = match val("Route") {
        "" => None,
        pin => {
            let (vendor, rest) = pin.split_once(' ').unwrap_or((pin, ""));
            let route = parse_route(vendor, rest)
                .map_err(|e| deck_error(format!("Route: {e}"), at("Route")))?;
            Some(route)
        }
    };
    let missing = REQUIRED
        .iter()
        .filter(|k| val(k).is_empty())
        .map(|k| k.to_string())
        .collect();
    Ok(Card {
        number,
        title: title.to_string(),
        role: role.to_string(),
        hard,
        hard_reason: reason.to_string(),
        risk: risk.to_string(),
        depends_on: ints(val("Depends on"), at("Depends on"), "Depends on")?,
        covers: csv(val("Covers")),
        source: val("Source").to_string(),
        allowed_paths: csv(val("Allowed paths")),
        out_of_scope: val("Out of scope").to_string(),
        done_when: val("Done when").to_string(),
        stop_if: val("Stop if").to_string(),
        inferred,
        route_pin,
        context: context.to_string(),
        missing,
        line,
        unknown_keys: Vec::new(),
    })
}

/// Kahn's algorithm, smallest ready number first: the order, and the cards left in a cycle
/// (`deck._cycle_free_order`).
fn cycle_free_order(deck: &Deck) -> (Vec<u32>, Vec<u32>) {
    // A repeated number keeps its last card's dependencies, as the Python's dict does.
    let mut deps: BTreeMap<u32, &[u32]> = BTreeMap::new();
    for c in &deck.cards {
        deps.insert(c.number, &c.depends_on);
    }
    let mut remaining: BTreeMap<u32, BTreeSet<u32>> = deps
        .iter()
        .map(|(c, d)| {
            let known = d.iter().copied().filter(|x| deps.contains_key(x));
            (*c, known.collect())
        })
        .collect();
    let mut ready: BinaryHeap<Reverse<u32>> = remaining
        .iter()
        .filter(|(_, d)| d.is_empty())
        .map(|(c, _)| Reverse(*c))
        .collect();
    let mut queued: BTreeSet<u32> = ready.iter().map(|r| r.0).collect();
    let mut order = Vec::new();
    while let Some(Reverse(c)) = ready.pop() {
        order.push(c);
        for (o, d) in remaining.iter_mut() {
            if d.remove(&c) && d.is_empty() && queued.insert(*o) {
                ready.push(Reverse(*o));
            }
        }
    }
    let left = remaining
        .keys()
        .copied()
        .filter(|c| !order.contains(c))
        .collect();
    (order, left)
}

fn cycle_message(left: &[u32]) -> String {
    let shown: Vec<String> = left.iter().map(u32::to_string).collect();
    format!("dependency cycle among cards {}", shown.join(", "))
}

/// The cards in dependency order, smallest ready number first; an error for a cycle.
pub fn dependency_order(deck: &Deck) -> Result<Vec<u32>, Error> {
    let (order, left) = cycle_free_order(deck);
    if left.is_empty() {
        Ok(order)
    } else {
        Err(Error::msg(cycle_message(&left)))
    }
}

/// The deck's problems for a run in the repository at `repo_top` with `config`, in the
/// Python's order (`deck.check_deck`). An error only when Git cannot be asked.
pub fn check_deck(deck: &Deck, repo_top: &Path, config: &Config) -> Result<Vec<Problem>> {
    let mut out: Vec<Problem> = unsupported_in_repo(config, repo_top)?
        .into_iter()
        .map(|u| Problem {
            level: Level::Error,
            card: None,
            message: u.message,
            fix: u.fix,
        })
        .collect();
    // Every message is the deck's or the configuration's text (UTF-8) or leadhand's own,
    // except the one naming the repository, which keeps its bytes.
    fn add(out: &mut Vec<Problem>, level: Level, card: Option<u32>, message: impl Into<PyStr>) {
        out.push(Problem {
            level,
            card,
            message: message.into(),
            fix: None,
        });
    }
    for msg in unsupported_settings(config) {
        add(&mut out, Level::Error, None, msg);
    }
    for msg in unsupported_warnings(config) {
        add(&mut out, Level::Warning, None, msg);
    }
    if let Some(finish) = deck.finish.as_deref().filter(|f| *f != "none") {
        add(
            &mut out,
            Level::Error,
            None,
            format!("not supported in this version: Finish: {finish}"),
        );
    }
    if py_strip(&deck.build).is_empty() {
        add(&mut out, Level::Error, None, "Build is empty");
    }
    if py_strip(&deck.test).is_empty() {
        add(&mut out, Level::Error, None, "Test is empty");
    }
    let sha = &deck.base_sha;
    let hex = (4..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex {
        add(
            &mut out,
            Level::Error,
            None,
            format!("base commit {} is not a commit hash", py_repr(sha)),
        );
    } else {
        let object = OsString::from(format!("{sha}^{{commit}}"));
        let git = Git::try_at(repo_top)?.with_deadline(GIT_TIMEOUT);
        if !git.ok(&[OsStr::new("cat-file"), OsStr::new("-e"), &object]) {
            add(
                &mut out,
                Level::Error,
                None,
                PyStr::from_os_parts(&[
                    OsStr::new(&format!("base commit {sha} not found in ")),
                    repo_top.as_os_str(),
                ]),
            );
        }
    }

    let mut numbers = BTreeSet::new();
    for c in &deck.cards {
        if !numbers.insert(c.number) {
            add(
                &mut out,
                Level::Error,
                Some(c.number),
                format!("duplicate card number {}", c.number),
            );
        }
    }
    for c in &deck.cards {
        let p = format!("card {}: ", c.number);
        let card = Some(c.number);
        for key in &c.missing {
            add(&mut out, Level::Error, card, format!("{p}missing {key}"));
        }
        if c.role == "scout" {
            add(
                &mut out,
                Level::Error,
                card,
                format!("{p}not supported in this version: scout cards"),
            );
        }
        if c.hard && c.hard_reason.is_empty() {
            add(
                &mut out,
                Level::Error,
                card,
                format!("{p}Hard: yes needs a reason"),
            );
        }
        for d in &c.depends_on {
            if !numbers.contains(d) {
                add(
                    &mut out,
                    Level::Error,
                    card,
                    format!("{p}depends on unknown card {d}"),
                );
            }
        }
        for (ln, key) in &c.unknown_keys {
            add(
                &mut out,
                Level::Warning,
                card,
                format!("{p}line {ln}: \"{key}\" is not a card field; treated as context"),
            );
        }
        for path in &c.allowed_paths {
            // As pathlib joins it: `src/` exists when `src` does, file or folder.
            let rel: PathBuf = Path::new(path).components().collect();
            if !repo_top.join(rel).exists() {
                add(
                    &mut out,
                    Level::Warning,
                    card,
                    format!("{p}allowed path {path} does not exist yet"),
                );
            }
        }
    }

    let (_, left) = cycle_free_order(deck);
    if !left.is_empty() {
        add(&mut out, Level::Error, None, cycle_message(&left));
    }

    for r in &deck.requirements {
        if r.cards.is_empty() {
            add(
                &mut out,
                Level::Warning,
                None,
                format!("requirement {} is uncovered (cards none)", r.id),
            );
        }
        for cn in &r.cards {
            if !numbers.contains(cn) {
                add(
                    &mut out,
                    Level::Error,
                    None,
                    format!(
                        "requirement {} maps to card {cn}, which does not exist",
                        r.id
                    ),
                );
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = "# Deck: Retry uploads
Source: docs/plans/2026-11-09-retry.md
Base: origin/main @ 3f2a9c1      Branch: leadhand/retry-uploads
Build: make build                 Test: make test
Profile: standard                 Finish: none

## Requirements
- R1 Uploads retry on transient errors (plan: Goal) \u{2192} cards 2, 3
- R2 Retries are bounded and logged (plan: Task 3) \u{2192} card 3

## Card 3: Retry uploads on transient errors
- Covers: R1, R2
- Role: implementer
- Hard: no
- Risk: medium
- Depends on: 2
- Source: plan Task 3
- Allowed paths: src/sync/, tests/sync/
- Out of scope: the upload queue
- Done when: tests/sync/retry.rs passes; full test suite passes
- Stop if: the client API has to change; two approaches fail
- Inferred: \"Done when\" test name (plan named no test)

Context: pointers to the plan section and files, not copies.
";

    /// Python's `test_deck`'s `deck_text`: cards are (number, role, hard, depends on).
    fn deck_text(cards: &[(u32, &str, &str, &str)]) -> String {
        let mut text = String::from(
            "# Deck: T\nSource: s\nBase: main @ abc1234\nBranch: b\nBuild: make build\n\
             Test: make test\n\n## Requirements\n- R1 Thing \u{2192} cards 1\n",
        );
        for (n, role, hard, deps) in cards {
            text += &format!(
                "\n## Card {n}: Title {n}\n- Covers: R1\n- Role: {role}\n- Hard: {hard}\n\
                 - Risk: low\n- Depends on: {deps}\n- Source: plan\n- Allowed paths: src/\n\
                 - Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n"
            );
        }
        text
    }

    struct Dir(tempfile::TempDir);

    impl Dir {
        fn new() -> Dir {
            Dir(tempfile::tempdir().unwrap())
        }

        fn write(&self, text: &str) -> PathBuf {
            let p = self.0.path().join("deck.md");
            std::fs::write(&p, text).unwrap();
            p
        }
    }

    /// What the reference's deck pattern of `section` made of each of `inputs`: its groups (or
    /// `null` for no match), or its split.
    fn reference(section: &str, inputs: &[&str]) -> Vec<serde_json::Value> {
        let inputs: Vec<serde_json::Value> = inputs.iter().map(|s| serde_json::json!(s)).collect();
        crate::wire::tests::reference_outputs("deck.json", section, &inputs)
    }

    #[test]
    fn requirement_matches_the_python_regex() {
        let cases = [
            "- R1 Thing \u{2192} cards 1",
            "- R1 Thing -> card 1",
            "- R12 a \u{2192} b \u{2192} cards 1, 2",
            "- R1 a -> b \u{2192} cards 3 -> card 4",
            "- R1 a \u{2192} cards 1 \u{2192} nothing",
            "- R1 a \u{2192} cards none",
            "- R1 a \u{2192}cards 1",
            "- R1 \u{2192} cards 1",
            "- R1\t\tx\u{2192}  card\t7",
            "- R1 a cards 1",
            "- R1\u{2192} cards 1",
            "- R1 a \u{2192} cardsx 1",
            "- R1 a \u{2192} cards",
            "- R1 a --> cards 2",
            "- Rx a \u{2192} cards 1",
            "- R a \u{2192} cards 1",
            "-R1 a \u{2192} cards 1",
            "- R1 caf\u{e9} \u{2192} cards 1,2 ,3",
        ];
        // `_REQ.match`: groups 1, 2 (stripped) and 3.
        let theirs = reference("requirement", &cases);
        for (case, want) in cases.iter().zip(theirs) {
            let ours = requirement(case).map(|(id, text, list)| [id, py_strip(text), list]);
            assert_eq!(serde_json::json!(ours), want, "{case:?}");
        }
    }

    #[test]
    fn header_split_matches_the_python_regex() {
        let cases = [
            "Base: main @ abc  Branch: b",
            "Base: main @ abc      Branch: leadhand/x",
            "Base: main @ abc Branch: b",
            "Base: main @ abc\t\tBranch: b",
            "Build: make build                 Test: make test",
            "Profile: standard                 Finish: none",
            "Source: a  b  c",
            "Source: a  Nope: b",
            "Source: a  Base:x  Branch: y",
            "Source: a \u{a0}Branch: y",
            "Source: a   Branch : y",
        ];
        let theirs = reference("header_split", &cases);
        for (case, want) in cases.iter().zip(theirs) {
            assert_eq!(serde_json::json!(split_header(case)), want, "{case:?}");
        }
    }

    #[test]
    fn field_card_and_hard_match_the_python_regexes() {
        let fields = [
            "- Role: implementer",
            "- Done when:   tests pass  ",
            "- Done when : x",
            "- Depends on:1",
            "- lower: x",
            "- Files: src/a.py",
            "- X1: y",
            "-  Role: x",
            "- Role x",
            "- A:b:c",
            "- R\u{e9}le: x",
            "- : x",
        ];
        // `_FIELD.match`: groups 1 and 2.
        let theirs = reference("field", &fields);
        for (case, want) in fields.iter().zip(theirs) {
            assert_eq!(serde_json::json!(field_line(case)), want, "{case:?}");
        }
        let cards = [
            "## Card 3: Retry uploads",
            "## Card 3:   spaced title   ",
            "## Card 3:",
            "## Card 12:x",
            "## Card 3 : x",
            "## Card x: y",
            "##  Card 3: y",
            "## Card 007: y",
        ];
        // `_CARD.match`: groups 1 and 2.
        let theirs = reference("card", &cards);
        for (case, want) in cards.iter().zip(theirs) {
            assert_eq!(serde_json::json!(card_heading(case)), want, "{case:?}");
        }
        let hards = [
            "yes",
            "yes \u{2014} no test can check it",
            "yes \u{2013} x",
            "yes - x",
            "yes: x",
            "yes:x",
            "yes -",
            "yes because",
            "yess",
            "yes \u{2014}   spaced  ",
            "Yes - x",
        ];
        // `_HARD.match`: group 1 (or `''`), stripped.
        let theirs = reference("hard", &hards);
        for (case, want) in hards.iter().zip(theirs) {
            assert_eq!(
                serde_json::json!(hard_yes(case).map(py_strip)),
                want,
                "{case:?}"
            );
        }
    }

    #[test]
    fn a_blank_deck_has_no_title() {
        let dir = Dir::new();
        for text in ["", "\n  \n"] {
            let path = dir.write(text);
            // Caught, so a reader that panics on a blank deck fails this assertion.
            let got =
                std::panic::catch_unwind(|| parse_deck(&path).map(drop).map_err(|e| e.to_string()));
            assert_eq!(
                got.map_err(|_| "parse_deck panicked"),
                Ok(Err("line 1: expected '# Deck: <title>' first".to_string())),
                "the deck {text:?}"
            );
        }
    }

    // CATALOG: test_deck::DeckTests::test_spec_example_header_parses
    #[test]
    fn spec_example_header_parses() {
        let dir = Dir::new();
        let d = parse_deck(&dir.write(SPEC_EXAMPLE)).unwrap();
        assert_eq!(
            (&*d.base_ref, &*d.base_sha, &*d.branch),
            ("origin/main", "3f2a9c1", "leadhand/retry-uploads")
        );
        assert_eq!(
            (
                &*d.build,
                &*d.test,
                d.profile.as_deref(),
                d.finish.as_deref()
            ),
            ("make build", "make test", Some("standard"), Some("none"))
        );
        let cards: Vec<&[u32]> = d.requirements.iter().map(|r| &r.cards[..]).collect();
        assert_eq!(cards, [&[2, 3][..], &[3][..]]);
        assert_eq!(d.cards[0].number, 3);
        assert_eq!(d.cards[0].covers, ["R1", "R2"]);
        assert_eq!(
            d.cards[0].stop_if,
            "the client API has to change; two approaches fail"
        );
    }

    // CATALOG: test_deck::DeckTests::test_dependency_order_is_stable_topological
    #[test]
    fn dependency_order_is_stable_topological() {
        let dir = Dir::new();
        let d = parse_deck(&dir.write(&deck_text(&[
            (3, "implementer", "no", "1"),
            (1, "implementer", "no", "none"),
            (2, "implementer", "no", "none"),
            (4, "implementer", "no", "3, 2"),
        ])))
        .unwrap();
        assert_eq!(dependency_order(&d).unwrap(), [1, 2, 3, 4]);
    }

    // CATALOG: test_deck::DeckTests::test_digest_changes_when_file_changes
    #[test]
    fn digest_changes_when_file_changes() {
        let dir = Dir::new();
        let text = deck_text(&[(1, "implementer", "no", "none")]);
        let p = dir.write(&text);
        let a = parse_deck(&p).unwrap().digest;
        std::fs::write(&p, text + "\nmore\n").unwrap();
        let b = parse_deck(&p).unwrap().digest;
        assert_ne!(a, b);
        // sha256 of the file's bytes, in hex, as hashlib computes it for these two files.
        assert_eq!(
            a,
            "c016f460eb992d63296724f7192bb94e680ae7e12f87359300f8c5c410e1e554"
        );
        assert_eq!(
            b,
            "3c43356c78f45b25157a12d339fbf98b207d969048af7a2303e06cd1df478168"
        );
    }
}
