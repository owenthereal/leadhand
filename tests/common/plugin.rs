//! Reading the plugin files: the agents, the supervisor and the run skill, as
//! the Python contract suite's `contract_plugin_files` reads them. `tests/plugin_files.rs` checks
//! the files themselves; `tests/cli.rs` checks the run skill's commands against the binary.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The package root (`harness.ROOT`).
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn agents() -> PathBuf {
    root().join("agents")
}

pub fn supervisor() -> PathBuf {
    agents().join("supervisor.md")
}

pub fn skill() -> PathBuf {
    root().join("skills").join("run").join("SKILL.md")
}

pub fn contract() -> PathBuf {
    root().join("templates/contract.md")
}

/// The commands the binary has (`harness.COMMANDS`).
pub const COMMANDS: [&str; 17] = [
    "cancel",
    "check",
    "dispatch",
    "dispatch-runner",
    "launch-line",
    "next",
    "pause",
    "record",
    "report",
    "route",
    "run",
    "start",
    "status",
    "stop-dispatch",
    "timer",
    "unpark",
    "version",
];

#[track_caller]
pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The frontmatter fields and the body of a plugin file (`split`): `key: value` lines between
/// `---` lines, a value's surrounding double quotes removed.
#[track_caller]
pub fn split(path: &Path) -> (BTreeMap<String, String>, String) {
    let text = read(path);
    let Some(rest) = text.strip_prefix("---\n") else {
        panic!("{}: no frontmatter", path.display());
    };
    let (head, body) = match rest.find("\n---\n") {
        Some(i) => (&rest[..i], &rest[i + "\n---\n".len()..]),
        None => (rest, ""),
    };
    let mut fields = BTreeMap::new();
    for line in head.lines() {
        let (key, value) = match line.split_once(':') {
            Some(kv) if !line.starts_with(' ') => kv,
            _ => panic!("{}: unexpected frontmatter line {line:?}", path.display()),
        };
        let mut value = value.trim();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = &value[1..value.len() - 1];
        }
        fields.insert(key.trim().to_string(), value.to_string());
    }
    (fields, body.to_string())
}

/// The text under a `### ` heading, up to the next heading of level 1 to 3 (`section`).
#[track_caller]
pub fn section(body: &str, heading: &str) -> String {
    let marker = format!("{heading}\n");
    let start = body
        .find(&marker)
        .unwrap_or_else(|| panic!("no heading {heading:?}"));
    let rest = &body[start + marker.len()..];
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        let hashes = line.bytes().take_while(|&b| b == b'#').count();
        if (1..=3).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
            return rest[..at].to_string();
        }
        at += line.len();
    }
    rest.to_string()
}

/// A word character as Python's `\w` (str patterns) has it.
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every `<cli> <command>` with its arguments, up to the end of the inline code span or the
/// line: `CLI_CALL.findall(text)`, for `<cli> ([a-z][a-z-]*)([^`\n]*)`.
pub fn cli_calls(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("<cli> ") {
        let start = from + i + "<cli> ".len();
        let rest = &text[start..];
        if !rest.starts_with(|c: char| c.is_ascii_lowercase()) {
            from = from + i + 1;
            continue;
        }
        let name_len = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
            .unwrap_or(rest.len());
        let after = &rest[name_len..];
        let args_len = after.find(['`', '\n']).unwrap_or(after.len());
        out.push((rest[..name_len].to_string(), after[..args_len].to_string()));
        from = start + name_len + args_len;
    }
    out
}

/// Every `--flag` not preceded by a word character or hyphen: `FLAG.findall(text)`, for
/// `(?<![\w-])(--[a-z][a-z-]*)`.
pub fn flags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();
        let free = prev.is_none_or(|p| !(is_word(p) || p == '-'));
        if free && rest.starts_with("--") && rest[2..].starts_with(|c: char| c.is_ascii_lowercase())
        {
            let len = 2 + rest[2..]
                .find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
                .unwrap_or(rest.len() - 2);
            out.push(rest[..len].to_string());
            prev = rest[..len].chars().last();
            i += len;
            continue;
        }
        prev = Some(c);
        i += c.len_utf8();
    }
    out
}

/// `word` appears in `text` with no word character or hyphen on either side (`has_token`).
pub fn has_token(text: &str, word: &str) -> bool {
    let edge = |c: Option<char>| c.is_none_or(|c| !(is_word(c) || c == '-'));
    text.match_indices(word)
        .any(|(i, _)| edge(text[..i].chars().last()) && edge(text[i + word.len()..].chars().next()))
}

/// `word` appears in `text` between word boundaries (`\bword\b`), `word` being word
/// characters.
pub fn has_word(text: &str, word: &str) -> bool {
    let edge = |c: Option<char>| c.is_none_or(|c| !is_word(c));
    text.match_indices(word)
        .any(|(i, _)| edge(text[..i].chars().last()) && edge(text[i + word.len()..].chars().next()))
}
