//! Worker briefs (the Python `briefs` module): where a dispatch's scratch folder is, the prompt a
//! Codex worker gets, and the head of a worker's report.
//!
//! A brief is a Markdown file in the attempt's scratch folder inside the workspace
//! (`<ws>/.leadhand/<card or "run">/<dispatch_id>/brief.md`); the worker writes its report
//! beside it. The worker contract and the role rules are the agent files under `agents/`,
//! compiled in; Codex workers get the same text through `worker_prompt`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::{AGENTS, ROLE_AGENT, VENDORS};
use crate::error::{Error, Result};
use crate::pytext::{pure_path, py_isspace, py_repr, py_splitlines, py_strip};

/// The scratch folder's name in the workspace (`briefs.SCRATCH`).
pub const SCRATCH: &str = ".leadhand";
/// The agent a Codex worker's proxy runs as (`briefs.CODEX_AGENT`).
pub const CODEX_AGENT: &str = "leadhand:codex";
/// A report head's status values (`briefs.STATUS_VALUES`).
pub const STATUS_VALUES: [&str; 4] = ["READY", "QUESTION", "BLOCKED", "SPENT"];
/// What `Blocked by:` may name (`briefs.BLOCKED_REASONS`).
pub const BLOCKED_REASONS: [&str; 2] = ["environment", "usage limit"];

/// Codex's workspace-write sandbox keeps the Git directory read-only (ruling R39), so a
/// Codex implementer never commits; the runner commits its work afterwards
/// (`briefs.CODEX_NO_COMMIT`).
pub const CODEX_NO_COMMIT: &str = "## Commits (Codex workers)\n\n\
    This overrides the contract's rule to commit on the card branch. Do not run git commit,\n\
    git add, or any command that writes under .git; leave your changes in the working tree\n\
    and list the changed files in your report. leadhand commits them on your branch after\n\
    you finish.";

/// The agent a role's worker runs as on `vendor` (`briefs.agent_for`).
pub fn agent_for(role: &str, vendor: &str) -> Result<&'static str> {
    if !VENDORS.contains(&vendor) {
        return Err(Error::msg(format!("unknown vendor {}", py_repr(vendor))));
    }
    let Some((_, agent)) = ROLE_AGENT.iter().find(|(r, _)| *r == role) else {
        return Err(Error::msg(format!(
            "no worker agent for role {}",
            py_repr(role)
        )));
    };
    if vendor == "codex" {
        return Ok(CODEX_AGENT);
    }
    Ok(match *agent {
        "implementer" => "leadhand:implementer",
        "reviewer" => "leadhand:reviewer",
        "verifier" => "leadhand:verifier",
        _ => "leadhand:scout",
    })
}

/// `<ws>/.leadhand/<card or "run">/<dispatch_id>`, spelled as `pathlib` spells it
/// (`briefs.scratch_dir`); a dispatch ID that is empty, `.`, `..` or holds a `/` is an error.
pub fn scratch_dir(ws: &Path, card: Option<i64>, dispatch_id: &str) -> Result<PathBuf> {
    if dispatch_id.is_empty() || dispatch_id.contains('/') || matches!(dispatch_id, "." | "..") {
        return Err(Error::msg(format!(
            "bad dispatch id {}",
            py_repr(dispatch_id)
        )));
    }
    let ws = pure_path(ws.as_os_str());
    let scratch = if ws == Path::new(".") {
        PathBuf::from(SCRATCH)
    } else {
        ws.join(SCRATCH)
    };
    let folder = card.map_or_else(|| "run".to_string(), |n| n.to_string());
    Ok(scratch.join(folder).join(dispatch_id))
}

/// The instructions a Codex worker gets (`briefs.worker_prompt`): the role's Claude agent
/// body (the contract and the role's rules), for implementers the no-commit rule, then the
/// brief and report paths, spelled as `pathlib` spells them. The paths are kept as their
/// bytes, so the prompt is OS text: it goes to Codex as an argument or on its stdin.
pub fn worker_prompt(role: &str, brief: &Path, report: &Path) -> Result<OsString> {
    let agent = agent_for(role, "claude")?;
    let name = agent.trim_start_matches("leadhand:");
    let file = AGENTS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, text)| *text);
    Ok(worker_prompt_from(file, agent, brief, report))
}

/// `worker_prompt` for the worker agent `agent` (`leadhand:<name>`), whose agent file's text is
/// `file`.
fn worker_prompt_from(file: &str, agent: &str, brief: &Path, report: &Path) -> OsString {
    let body = match file
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
    {
        Some((_, body)) => body,
        None => file,
    };
    let mut body = py_strip(body).to_string();
    if agent == "leadhand:implementer" {
        body.push_str("\n\n");
        body.push_str(CODEX_NO_COMMIT);
    }
    let mut prompt = OsString::from(format!("{body}\n\n## Your files\n\n- Brief: "));
    prompt.push(pure_path(brief.as_os_str()));
    prompt.push("\n- Report file: ");
    prompt.push(pure_path(report.as_os_str()));
    prompt.push("\n\nRead the brief now, do the work, write the report, then reply.\n");
    prompt
}

/// What a report head is when it cannot be parsed (`briefs._UNPARSED`).
const UNPARSED: (&str, &str, &str) = ("Status", "BLOCKED", "");

/// `(key, value, blocked_by)` from a report's or reply's head (`briefs.parse_report_head`):
/// the first non-blank line, Markdown emphasis and quote marks aside, must be `Status:`,
/// `Verdict:` or `Result:` and one of its values, in any case; a `BLOCKED` status takes its
/// reason from a `Blocked by:` line among the next four. Anything else is
/// `("Status", "BLOCKED", "")`.
///
/// The Python matches with `re.IGNORECASE` over Unicode, so `ſ` matches `s`, `ı` and `İ`
/// match `i`, and the Kelvin sign matches `k`; this matches the same lines. Where its
/// `capitalize()` then makes a key it has no values for (`reſult`, `verdİct`), the Python
/// raises `KeyError`, and so does this: `UnknownHeadKey` (S2-R32), which a caller
/// propagates as the Python's exception propagates.
pub fn parse_report_head(
    text: &str,
) -> std::result::Result<(&'static str, &'static str, &'static str), UnknownHeadKey> {
    let lines: Vec<String> = py_splitlines(text)
        .into_iter()
        .map(clean)
        .filter(|line| !line.is_empty())
        .collect();
    let Some((key, word)) = (match lines.first() {
        Some(first) => head(first)?,
        None => None,
    }) else {
        return Ok(UNPARSED);
    };
    let allowed: &[&'static str] = match key {
        "Status" => &STATUS_VALUES,
        "Verdict" => &["clean", "defects"],
        _ => &["pass", "fail"],
    };
    let word = folded_lower(word);
    let Some(value) = allowed
        .iter()
        .find(|v| Some(v.to_ascii_lowercase()) == word)
    else {
        return Ok(UNPARSED);
    };
    let mut blocked_by = "";
    if key == "Status" && *value == "BLOCKED" {
        for line in lines.iter().skip(1).take(4) {
            if let Some(reason) = blocked_reason(line) {
                let reason = reason.to_lowercase();
                blocked_by = BLOCKED_REASONS
                    .into_iter()
                    .find(|r| reason.starts_with(r))
                    .unwrap_or("");
                break;
            }
        }
    }
    Ok((key, value, blocked_by))
}

/// The Python's `KeyError` from `parse_report_head`: a head whose case-folded key
/// `capitalize()`s to a name with no values (`reſult` → `Reſult`). `key` is that name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownHeadKey {
    pub key: String,
}

impl std::fmt::Display for UnknownHeadKey {
    /// As the Python's uncaught exception names it: `KeyError: 'Reſult'`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyError: {}", py_repr(&self.key))
    }
}

impl From<UnknownHeadKey> for Error {
    fn from(e: UnknownHeadKey) -> Error {
        Error::msg(e.to_string())
    }
}

/// `str.capitalize()`: the first character in title case, the rest lowercased.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// `briefs._clean`: emphasis marks removed, stripped, leading `#` and `>` removed, stripped.
fn clean(line: &str) -> String {
    let unmarked: String = line
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`'))
        .collect();
    py_strip(py_strip(&unmarked).trim_start_matches(['#', '>'])).to_string()
}

/// Whether `c` matches the lowercase ASCII letter `p` under Python's Unicode `IGNORECASE`.
fn folds_to(c: char, p: char) -> bool {
    c.to_ascii_lowercase() == p && c.is_ascii()
        || matches!((p, c), ('s', 'ſ') | ('i', 'İ' | 'ı') | ('k', '\u{212a}'))
}

/// The text of `line` that matched the case-folded `word`, and the rest after it.
pub(crate) fn after_folded<'a>(line: &'a str, word: &str) -> Option<(&'a str, &'a str)> {
    let mut at = 0;
    for p in word.chars() {
        let c = line[at..].chars().next()?;
        if !folds_to(c, p) {
            return None;
        }
        at += c.len_utf8();
    }
    Some(line.split_at(at))
}

/// `_HEAD`: `(status|verdict|result)\s*:\s*([A-Za-z]+)\s*$`, case-folded; the key as the
/// Python names it, and the value word. `None` when the line does not match; the Python's
/// `KeyError` when it matches but `capitalize()` does not give the key back.
fn head(line: &str) -> std::result::Result<Option<(&'static str, &str)>, UnknownHeadKey> {
    for (word, key) in [
        ("status", "Status"),
        ("verdict", "Verdict"),
        ("result", "Result"),
    ] {
        let Some((matched, rest)) = after_folded(line, word) else {
            continue;
        };
        let Some(rest) = rest.trim_start_matches(py_isspace).strip_prefix(':') else {
            return Ok(None);
        };
        let rest = rest.trim_start_matches(py_isspace);
        let letters = rest
            .find(|c: char| !is_head_letter(c))
            .unwrap_or(rest.len());
        let (value, tail) = rest.split_at(letters);
        if value.is_empty() || !tail.chars().all(py_isspace) {
            return Ok(None);
        }
        let named = capitalize(matched);
        if named != key {
            return Err(UnknownHeadKey { key: named });
        }
        return Ok(Some((key, value)));
    }
    Ok(None)
}

/// `[A-Za-z]` under Unicode `IGNORECASE`.
fn is_head_letter(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, 'İ' | 'ı' | 'ſ' | '\u{212a}')
}

/// `word.lower()` when it is ASCII once lowered (the Kelvin sign lowers to `k`); `None` when
/// it is not, so it equals no value.
fn folded_lower(word: &str) -> Option<String> {
    word.chars()
        .map(|c| match c {
            '\u{212a}' => Some('k'),
            c if c.is_ascii() => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// `_BLOCKED_BY`: `blocked by\s*:\s*(.+?)\s*$`, case-folded; the reason.
fn blocked_reason(line: &str) -> Option<&str> {
    let (_, rest) = after_folded(line, "blocked")?;
    let rest = rest.strip_prefix(' ')?;
    let (_, rest) = after_folded(rest, "by")?;
    let rest = rest.trim_start_matches(py_isspace).strip_prefix(':')?;
    let reason = py_strip(rest);
    (!reason.is_empty()).then_some(reason)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    use super::*;
    use crate::wire::tests::{read_reference, reference_for};

    /// The reference fixture of the tests here (besides the worker prompts).
    pub(crate) const FIXTURE: &str = "briefs.json";

    /// `worker-prompts/<role>.txt`: the reference's `worker_prompt` for each role, for each of
    /// these brief and report paths in turn, as the OS gets it (`os.fsencode`), the prompts
    /// joined by NUL. The reference read frozen copies of the agent files
    /// (`worker-prompts/agents/<name>.md`), and Rust builds each prompt from the same copy, so
    /// the check stays the reference's when an agent file changes. `worker_prompt` itself is
    /// `worker_prompt_from` on the live agent file.
    #[test]
    fn worker_prompts_match_reference() {
        let paths: [(&[u8], &[u8]); 4] = [
            (
                b"/ws/.leadhand/1/c1-a1-implement-1/brief.md",
                b"/ws/.leadhand/1/c1-a1-implement-1/report.md",
            ),
            // A non-UTF-8 byte, kept as it is.
            (
                b"/ws/caf\xff/.leadhand/run/d/brief.md",
                b"/ws/caf\xff/.leadhand/run/d/report.md",
            ),
            // Non-ASCII text, and a space.
            (
                b"/ws/caf\xc3\xa9 \xe2\x98\x95/brief.md",
                b"/ws/caf\xc3\xa9 \xe2\x98\x95/report.md",
            ),
            // Spelled as pathlib spells it.
            (b"rel//./ws/brief.md", b"./rel/report.md/"),
        ];
        let roles: Vec<&str> = ROLE_AGENT.iter().map(|(role, _)| *role).collect();
        let mut ours: Vec<Vec<u8>> = Vec::new();
        for role in &roles {
            let agent = agent_for(role, "claude").unwrap();
            let name = agent.trim_start_matches("leadhand:");
            let frozen =
                String::from_utf8(read_reference(&format!("worker-prompts/agents/{name}.md")))
                    .unwrap();
            let live = AGENTS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, text)| *text)
                .expect("the role's agent file");
            let mut mine: Vec<Vec<u8>> = Vec::new();
            for (b, r) in &paths {
                let (b, r) = (
                    Path::new(OsStr::from_bytes(b)),
                    Path::new(OsStr::from_bytes(r)),
                );
                let prompt = worker_prompt(role, b, r).unwrap();
                assert_eq!(prompt, worker_prompt_from(live, agent, b, r), "{role}");
                mine.push(worker_prompt_from(&frozen, agent, b, r).into_vec());
            }
            let theirs = read_reference(&format!("worker-prompts/{role}.txt"));
            assert_eq!(mine.join(&b"\0"[..]), theirs, "{role}");
            ours.extend(mine);
        }
        assert_eq!(ours.len(), 7 * 4);
        // The implementer's prompt carries the no-commit rule; the reviewer's does not.
        let has = |p: &[u8], s: &[u8]| p.windows(s.len()).any(|w| w == s);
        assert!(has(&ours[0], CODEX_NO_COMMIT.as_bytes()));
        assert!(!has(&ours[2 * 4], CODEX_NO_COMMIT.as_bytes()));
        // The non-UTF-8 byte is in the prompt as itself.
        assert!(has(
            &ours[1],
            b"\n- Brief: /ws/caf\xff/.leadhand/run/d/brief.md\n"
        ));
        let err = worker_prompt("supervisor", Path::new("/b"), Path::new("/r")).unwrap_err();
        assert_eq!(err.message, "no worker agent for role 'supervisor'");
    }

    #[test]
    fn agent_for_maps_every_role_and_vendor() {
        for (role, agent) in ROLE_AGENT {
            assert_eq!(
                agent_for(role, "claude").unwrap(),
                format!("leadhand:{agent}")
            );
            assert_eq!(agent_for(role, "codex").unwrap(), CODEX_AGENT);
        }
        assert_eq!(
            agent_for("implementer", "gemini").unwrap_err().message,
            "unknown vendor 'gemini'"
        );
        assert_eq!(
            agent_for("proxy", "claude").unwrap_err().message,
            "no worker agent for role 'proxy'"
        );
    }

    #[test]
    fn scratch_dir_matches_python() {
        let cases: [(&str, Option<i64>, &str); 4] = [
            ("/ws", Some(1), "c1-a1-implement-1"),
            ("/ws/", None, "run-a1-final_review-1"),
            ("ws//a/.", Some(-2), "x"),
            (".", None, "d"),
        ];
        // `briefs.json`: the reference's `scratch_dir` of each case.
        let theirs: String = reference_for::<String>(FIXTURE, "scratch_dir", &cases)
            .iter()
            .map(|out| format!("{out}\n"))
            .collect();
        let ours: String = cases
            .iter()
            .map(|(ws, card, did)| {
                format!(
                    "{}\n",
                    scratch_dir(Path::new(ws), *card, did).unwrap().display()
                )
            })
            .collect();
        assert_eq!(ours, theirs);
        for bad in ["", ".", "..", "a/b"] {
            let err = scratch_dir(Path::new("/ws"), Some(1), bad).unwrap_err();
            assert_eq!(err.message, format!("bad dispatch id {}", py_repr(bad)));
        }
    }

    #[test]
    fn parse_report_head_matches_python() {
        let cases = [
            "Status: READY",
            "  **Status**: ready  ",
            "\n\n_Status_ :  `Blocked`\nBlocked by: environment (no network)",
            "Status: BLOCKED\nBlocked by: Usage limit reached",
            "Status: BLOCKED\nBlocked by:\nBlocked by: usage limit",
            "Status: BLOCKED\n\n\n\n\nBlocked by: environment",
            "Status: BLOCKED\na\nb\nc\nd\nBlocked by: environment",
            "Status: BLOCKED\nblocked  by : environment",
            "Status: BLOCKED\nBlocked by: something else",
            "Status: BLOCKED\nBloc\u{212a}ed by: environment",
            "Status: QUESTION\nBlocked by: environment",
            "# Verdict: clean",
            "> Result: FAIL",
            "# > Verdict : Clean",
            "Verdict: dirty",
            "Result: pass extra",
            "Status: SPENT\r\nmore",
            "\u{17f}tatus: READY",
            "Status: B\u{212a}OCKED",
            "Status: BLO\u{212a}ED",
            "Status: \u{17f}pent",
            "Status\u{a0}:\u{2003}READY\u{3000}",
            "Statusx: READY",
            "Status READY",
            "",
            "   \n\t",
            "**",
            "status:ready\u{2028}Blocked by: environment",
        ];
        let cases: Vec<String> = cases.iter().map(|s| s.to_string()).collect();
        let theirs = reference_heads("report_head", &cases);
        assert!(theirs.iter().all(|r| r.is_ok()), "{theirs:?}");
        assert_same_heads(&cases, theirs);
    }

    /// The reference's `briefs.parse_report_head` of each text, from section `section` of
    /// `briefs.json`: the triple, or the key its `KeyError` names.
    fn reference_heads(
        section: &str,
        texts: &[String],
    ) -> Vec<std::result::Result<Vec<String>, String>> {
        let out: Vec<Vec<String>> = reference_for(FIXTURE, section, texts);
        out.into_iter()
            .map(|mut r| match r[0].as_str() {
                "ok" => Ok(r.split_off(1)),
                _ => Err(r.remove(1)),
            })
            .collect()
    }

    fn assert_same_heads(texts: &[String], theirs: Vec<std::result::Result<Vec<String>, String>>) {
        assert_eq!(texts.len(), theirs.len());
        for (text, want) in texts.iter().zip(theirs) {
            let ours = parse_report_head(text)
                .map(|(k, v, b)| vec![k.to_string(), v.to_string(), b.to_string()])
                .map_err(|e| e.key);
            assert_eq!(ours, want, "{text:?}");
        }
    }

    #[test]
    fn parse_report_head_fails_where_python_raises() {
        // Each of the four folding letters (ſ, ı, İ, the Kelvin sign) in every position of
        // every key, with a value that matches: the Python raises KeyError where
        // `capitalize()` gives a key with no values, and matches or not otherwise (S2-R32).
        let mut texts = Vec::new();
        for (word, value) in [
            ("status", "READY"),
            ("verdict", "clean"),
            ("result", "pass"),
        ] {
            for at in 0..word.len() {
                for fold in ['\u{17f}', '\u{131}', '\u{130}', '\u{212a}'] {
                    let mut key: Vec<char> = word.chars().collect();
                    key[at] = fold;
                    let key: String = key.into_iter().collect();
                    texts.push(format!("{key}: {value}"));
                    texts.push(format!("**{key}**: {value}\nBlocked by: environment"));
                }
            }
        }
        let theirs = reference_heads("report_head_folded", &texts);
        let raised = theirs.iter().filter(|r| r.is_err()).count();
        // `statuſ`, `verdıct`, `verdİct` and `reſult` raise (two texts each); `ſtatus` parses,
        // and a letter that folds to none of the key's is no match.
        assert_eq!(raised, 2 * 4, "{theirs:?}");
        assert_same_heads(&texts, theirs);
        let err = parse_report_head("re\u{17f}ult: pass").unwrap_err();
        assert_eq!(err.key, "Re\u{17f}ult");
        assert_eq!(err.to_string(), "KeyError: 'Re\u{17f}ult'");
        assert_eq!(Error::from(err).message, "KeyError: 'Re\u{17f}ult'");
        assert_eq!(
            parse_report_head("\u{17f}tatus: ready"),
            Ok(("Status", "READY", ""))
        );
    }

    #[test]
    fn parse_report_head_reads_the_kelvin_sign_in_a_value() {
        // The Kelvin sign lowers to `k`, so `BLOC\u{212a}ED` is BLOCKED.
        let texts: Vec<String> = [
            "Status: BLOC\u{212a}ED",
            "status: bloc\u{212a}ed\nBlocked by: usage limit",
            "Status: BLO\u{212a}\u{212a}ED",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(parse_report_head(&texts[0]), Ok(("Status", "BLOCKED", "")));
        assert_eq!(
            parse_report_head(&texts[1]),
            Ok(("Status", "BLOCKED", "usage limit"))
        );
        assert_same_heads(&texts, reference_heads("report_head_kelvin", &texts));
    }
}
