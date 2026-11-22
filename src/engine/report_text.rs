//! What the engine reads in a worker's report or reply (`engine.py`'s report-text rules): the
//! head of a result (`_head`: the status, verdict or result line, the turn-limit note), the
//! verdict cross-check (`_FINDINGS_SECTION`, `_SEVERE_FINDING`), and the question a worker
//! asked (`_QUESTION`), with the report reader they all start from (`_read_text`).
//!
//! The Python matches these with `re` (Unicode, `IGNORECASE`, `MULTILINE`, some `DOTALL`);
//! there is no regex crate, so each is matched by hand, as the same lines would match, and
//! the tests check them against the Python's own patterns.

use std::path::PathBuf;

use crate::briefs::{after_folded, parse_report_head};
use crate::dispatch::DispatchResult;
use crate::dispatch::commit::is_decimal;
use crate::dispatch::runner::has_status_line;
use crate::error::{Error, Result};
use crate::pytext::{py_isspace, py_strip};
use crate::wire::PyStr;

/// `Path(path).read_text(encoding="utf-8", errors="replace")` (`engine._read_text`): `None`
/// for an empty name, a missing file or a folder; line ends translated, as text mode does.
pub(crate) fn read_text(path: &PyStr) -> Result<Option<String>> {
    if path.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(path.to_os()?);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(Error::io(&path, e)),
    };
    Ok(Some(
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n"),
    ))
}

/// `_QUESTION.search(text).group(1).strip()`, else `""`: the section under the first
/// `# Question` heading (any level, any case), up to the next heading.
pub fn question(text: &str) -> String {
    for start in line_starts(text) {
        let rest = &text[start..];
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        if hashes == 0 {
            continue;
        }
        let rest = rest[hashes..].trim_start_matches(py_isspace);
        let Some((_, after)) = after_folded(rest, "question") else {
            continue;
        };
        // `\s*$`: the longest run of whitespace that ends at a line end or the text's end.
        let run: usize = after
            .chars()
            .take_while(|c| py_isspace(*c))
            .map(char::len_utf8)
            .sum();
        let end_of_head = (0..=run)
            .rev()
            .filter(|i| after.is_char_boundary(*i))
            .find(|i| after[*i..].is_empty() || after[*i..].starts_with('\n'));
        let Some(end_of_head) = end_of_head else {
            continue;
        };
        let group_start = text.len() - after.len() + end_of_head;
        let group_end = (group_start..=text.len())
            .filter(|i| text.is_char_boundary(*i))
            .find(|i| *i == text.len() || at_heading(text, *i))
            .unwrap_or(text.len());
        return py_strip(&text[group_start..group_end]).to_string();
    }
    String::new()
}

/// The starts of `text`'s lines: 0 and every position after a `\n` (`^` under `MULTILINE`).
pub(crate) fn line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        text.bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i + 1),
    )
}

/// `^#+\s` at `at`.
fn at_heading(text: &str, at: usize) -> bool {
    if at != 0 && text.as_bytes()[at - 1] != b'\n' {
        return false;
    }
    let rest = &text[at..];
    let hashes = rest.bytes().take_while(|b| *b == b'#').count();
    hashes > 0 && rest[hashes..].chars().next().is_some_and(py_isspace)
}

/// The head of a result (`_head`): the report's head line; else, for an interrupted worker,
/// SPENT marked interrupted; for a stopped one, SPENT; then the reply's head, the Codex
/// runner's status, a Claude turn-limit note (SPENT), else BLOCKED with the raw reply kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub status: String,
    pub blocked_by: String,
    pub head: String,
    pub verdict: Option<String>,
    pub result: Option<String>,
    pub source: &'static str,
    pub raw_kept: bool,
    pub interrupted: bool,
    pub cross_checked: bool,
}

impl Head {
    fn status(status: &str, blocked_by: &str, source: &'static str, raw_kept: bool) -> Head {
        Head {
            status: status.to_string(),
            blocked_by: blocked_by.to_string(),
            head: "Status".into(),
            verdict: None,
            result: None,
            source,
            raw_kept,
            interrupted: false,
            cross_checked: false,
        }
    }
}

/// `_has_status`: text that is not blank and has a status line.
fn has_status(text: Option<&str>) -> Result<bool> {
    match text {
        Some(t) if !t.trim_matches(py_isspace).is_empty() => Ok(has_status_line(t)?),
        _ => Ok(false),
    }
}

/// The head a worker's result is recorded with (`_head`): the report's status line, else
/// the interruption or stop, else the reply's status line, else the Codex runner's result,
/// else the turn limit, else blocked and unparsed.
pub(crate) fn head_of(
    report: Option<&str>,
    reply: &str,
    codex: Option<&DispatchResult>,
    stopped: bool,
    interrupted: bool,
) -> Result<Head> {
    let from_text = |text: &str, source: &'static str| -> Result<Head> {
        let (key, value, blocked_by) = parse_report_head(text)?;
        let mut out = Head {
            status: if key == "Status" { value } else { "READY" }.to_string(),
            blocked_by: blocked_by.to_string(),
            head: key.to_string(),
            verdict: (key == "Verdict").then(|| value.to_string()),
            result: (key == "Result").then(|| value.to_string()),
            source,
            raw_kept: false,
            interrupted: false,
            cross_checked: false,
        };
        if out.verdict.as_deref() == Some("clean")
            && findings_sections(text).iter().any(|s| severe_finding(s))
        {
            out.verdict = Some("defects".into());
            out.cross_checked = true;
        }
        Ok(out)
    };
    if has_status(report)? {
        return from_text(report.unwrap_or_default(), "report");
    }
    if interrupted {
        let mut out = Head::status("SPENT", "", "interrupted", false);
        out.interrupted = true;
        return Ok(out);
    }
    if stopped {
        return Ok(Head::status("SPENT", "", "stopped", false));
    }
    if has_status(Some(reply))? {
        return from_text(reply, "reply");
    }
    if let Some(codex) = codex {
        return Ok(Head::status(
            codex.status.as_str(),
            codex.blocked_by.as_str(),
            "runner",
            true,
        ));
    }
    if turn_limit(reply) {
        return Ok(Head::status("SPENT", "", "turn limit", true));
    }
    Ok(Head::status("BLOCKED", "", "unparsed", true))
}

/// The `## Findings` sections of a reviewer's report (`_FINDINGS_SECTION.finditer`): from a
/// `## Findings` line to the next `## ` heading.
fn findings_sections(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) =
        line_starts(text).find(|s| *s >= from && findings_head(&text[*s..]).is_some())
    {
        let group = start + findings_head(&text[start..]).expect("found above");
        let end = (group..=text.len())
            .filter(|i| text.is_char_boundary(*i))
            .find(|i| *i == text.len() || at_h2(text, *i))
            .unwrap_or(text.len());
        out.push(&text[group..end]);
        if end <= from {
            break;
        }
        from = end;
    }
    out
}

/// `^##[ \t]*Findings[ \t\r]*$` at the start of `s`: where the heading's line ends.
fn findings_head(s: &str) -> Option<usize> {
    let rest = s.strip_prefix("##")?.trim_start_matches([' ', '\t']);
    let (_, after) = after_folded(rest, "findings")?;
    let after_marks = after.trim_start_matches([' ', '\t', '\r']);
    (after_marks.is_empty() || after_marks.starts_with('\n')).then(|| s.len() - after_marks.len())
}

/// `^##\s` at `at`.
fn at_h2(text: &str, at: usize) -> bool {
    (at == 0 || text.as_bytes()[at - 1] == b'\n')
        && text[at..]
            .strip_prefix("##")
            .and_then(|r| r.chars().next())
            .is_some_and(py_isspace)
}

/// `_SEVERE_FINDING.search(section)`: a numbered or bulleted line whose severity tag is high or
/// medium (`1. [high] …`, `- [Medium] …`).
fn severe_finding(section: &str) -> bool {
    line_starts(section).any(|at| {
        let rest = section[at..].trim_start_matches(py_isspace);
        let digits: usize = rest
            .chars()
            .take_while(|c| is_decimal(*c))
            .map(char::len_utf8)
            .sum();
        let rest = if digits > 0 {
            match rest[digits..].strip_prefix(['.', ')']) {
                Some(r) => r,
                None => return false,
            }
        } else if let Some(r) = rest.strip_prefix(['-', '*']) {
            r
        } else {
            return false;
        };
        let Some(rest) = rest.trim_start_matches(py_isspace).strip_prefix('[') else {
            return false;
        };
        ["high", "medium"]
            .iter()
            .any(|level| after_folded(rest, level).is_some_and(|(_, after)| after.starts_with(']')))
    })
}

/// `_TURN_LIMIT.search(reply)`: `stopped at its <n>-turn limit`, any case.
pub(crate) fn turn_limit(reply: &str) -> bool {
    reply.char_indices().any(|(at, _)| {
        let Some((_, rest)) = after_folded(&reply[at..], "stopped at its ") else {
            return false;
        };
        let digits: usize = rest
            .chars()
            .take_while(|c| is_decimal(*c))
            .map(char::len_utf8)
            .sum();
        digits > 0 && after_folded(&rest[digits..], "-turn limit").is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::tests::reference_for;

    /// The reference fixture of the tests here.
    const FIXTURE: &str = "engine.json";

    #[test]
    fn questions_are_read_as_the_python_reads_them() {
        let texts = [
            "Status: QUESTION\n## Summary\nx\n## Question\nWhich storage format should widgets use?\nOptions:\n- A: JSON\n## Concerns\nNone\n",
            "# question  \n\n  Which one?  \n",
            "### QUESTION\nA?\n#Not a heading\n## Next\nB",
            "## Questions\nno\n",
            "## Queſtion\nfolded\n",
            "##Question\r\nCRLF?\r\n## End\n",
            "no question here",
            "## Question\n## Empty\n",
        ];
        // `engine.json`: `_QUESTION.search(t)`'s group 1, stripped, or "".
        let theirs: Vec<String> = reference_for(FIXTURE, "question", &texts);
        let ours: Vec<String> = texts.iter().map(|t| question(t)).collect();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn heads_cross_check_findings_as_the_python_does() {
        let texts = [
            "Verdict: clean\n## Findings\n2. [High] src/a.py:3 — leaks — close it\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\n  1) [medium] src/a.py:3 — x — y\n",
            "Verdict: clean\n## Findings\n1. [low] src/a.py:3 — nit — rename\n## Notes\n- [high] earlier round: fixed\n1. [medium] fixed\n",
            "Verdict: clean\n[high] src/a.py:3 outside any section\n## Findings\nNone\n",
            "Verdict: clean\n## Findings\n- [high] src/a.py:3 — leaks — close it\n",
            "Verdict: clean\n## Findings\n  * [Medium] src/a.py:3 — x — y\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\nNone\n## Notes\n1. [high] src/widget.py:1 — fixed\n",
            "Verdict: clean\n##findings \r\n- [mediuM] a\n",
            "Verdict: clean\n## Findings\n\n\n   \n١٢. [hıgh] unicode digits\n",
            "Verdict: clean\n## Findings x\n- [high] not the heading\n",
            "Verdict: clean\n## Findings\nNone\n## Findings\n- [high] the second one\n",
            "Verdict: defects\n## Findings\n- [high] a\n",
            "Status: READY\n## Findings\n- [high] a\n",
            "Verdict: clean\n## Findings\n-[high]\n#### Findings\n",
            "NOTE: this agent stopped at its 150-turn limit before finishing.\n",
            "stopped at ITS 7-TURN LIMIT",
            "stopped at its -turn limit",
        ];
        // `engine.json`: `_head(t, '', None, False)`'s status, verdict and `cross_checked`,
        // and whether `_TURN_LIMIT` finds the text.
        let theirs: Vec<(String, Option<String>, bool, bool)> =
            reference_for(FIXTURE, "heads_cross_check", &texts);
        let ours: Vec<(String, Option<String>, bool, bool)> = texts
            .iter()
            .map(|t| {
                let h = head_of(Some(t), "", None, false, false).unwrap();
                (h.status, h.verdict, h.cross_checked, turn_limit(t))
            })
            .collect();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn heads_fall_back_as_the_python_does() {
        let cases: [(Option<&str>, &str, bool, bool); 7] = [
            (None, "Status: READY\nReport: x", false, false),
            (Some("   \n"), "I changed some things.\n", false, false),
            (
                None,
                "NOTE: this agent stopped at its 150-turn limit.\n",
                false,
                false,
            ),
            (Some("Status: QUESTION\n"), "Status: READY", true, false),
            (None, "Status: READY", true, false),
            (None, "", false, true),
            (Some("Result: pass\n"), "", false, true),
        ];
        // `engine.json`: `_head(report, reply, None, stopped, interrupted)`'s status, head,
        // source, `raw_kept` and `interrupted`.
        let theirs: Vec<(String, String, String, bool, bool)> =
            reference_for(FIXTURE, "heads_fall_back", &cases);
        let ours: Vec<(String, String, String, bool, bool)> = cases
            .iter()
            .map(|(report, reply, stopped, interrupted)| {
                let h = head_of(*report, reply, None, *stopped, *interrupted).unwrap();
                (
                    h.status,
                    h.head,
                    h.source.to_string(),
                    h.raw_kept,
                    h.interrupted,
                )
            })
            .collect();
        assert_eq!(ours, theirs);
        // A crafted head the Python's parser raises KeyError on fails the same way (S2-R32).
        let e = head_of(Some("re\u{17f}ult: pass\n"), "", None, false, false).unwrap_err();
        assert_eq!(e.message, "KeyError: 'Re\u{17f}ult'");
    }
}
