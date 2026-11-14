//! The conventions shared by the commands a person types (spec, "Commands you type"):
//! - `--json` prints the same information as the text output, as one JSON object;
//! - an error's first line is `error: <what happened>`;
//! - when one command fixes it, the last line is that command alone, ready to copy.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::process::ExitCode;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::wire;

/// Prints `text` as it is, or with `json`, `value` as one JSON object on a line of its own
/// (ASCII, as Python's `json.dumps` writes it).
pub fn print_or_json<T: Serialize>(text: &str, value: &T, json: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if json {
        writeln!(out, "{}", wire::to_output(value)?)?;
    } else {
        out.write_all(text.as_bytes())?;
    }
    out.flush()?;
    Ok(())
}

/// Prints `e` on stderr (`error: <message>`, then the fix command alone, when there is one)
/// and gives its exit code.
pub fn fail_with_fix(e: Error) -> ExitCode {
    let _ = render_error(&e, &mut std::io::stderr().lock());
    ExitCode::from(e.exit)
}

/// `e` as the commands a person types print it.
pub fn render_error(e: &Error, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "error: {}", e.message)?;
    if let Some(fix) = &e.fix {
        writeln!(out, "{fix}")?;
    }
    out.flush()
}

/// `word` quoted for a POSIX shell, as Python's `shlex.quote` quotes it: unchanged when every
/// byte is in `[A-Za-z0-9_@%+=:,./-]`, else in single quotes.
pub fn shell_quote(word: &OsStr) -> OsString {
    let bytes = word.as_bytes();
    let safe = |b: &u8| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(b);
    if !bytes.is_empty() && bytes.iter().all(safe) {
        return word.to_os_string();
    }
    let mut out = vec![b'\''];
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(b"'\"'\"'");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    OsString::from_vec(out)
}

/// `argv` as one shell command line, each word quoted; shown lossily (a fix line is display
/// text, as an error message is).
pub fn command_line(argv: &[OsString]) -> String {
    let words: Vec<Vec<u8>> = argv.iter().map(|w| shell_quote(w).into_vec()).collect();
    String::from_utf8_lossy(&words.join(&b' ')).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(e: &Error) -> String {
        let mut out = Vec::new();
        render_error(e, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn an_error_is_one_error_line() {
        assert_eq!(
            rendered(&Error::msg("dependency cycle among cards 1, 2")),
            "error: dependency cycle among cards 1, 2\n"
        );
    }

    #[test]
    fn the_fix_command_is_alone_on_the_last_line() {
        let e = Error::msg("/r is a sparse checkout")
            .with_fix("git -C /r sparse-checkout disable".into());
        let text = rendered(&e);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "error: /r is a sparse checkout",
                "git -C /r sparse-checkout disable"
            ]
        );
        assert!(text.ends_with("disable\n"));
    }

    #[test]
    fn fail_with_fix_exits_with_the_error_code() {
        assert_eq!(fail_with_fix(Error::msg("x")), ExitCode::from(1));
        assert_eq!(fail_with_fix(Error::refused("x")), ExitCode::from(3));
    }

    #[test]
    fn shell_quote_is_shlex_quote() {
        for (word, want) in [
            ("/plain/path-1.2_x@%+=:,", "/plain/path-1.2_x@%+=:,"),
            ("", "''"),
            ("/tmp/my repo", "'/tmp/my repo'"),
            ("it's", "'it'\"'\"'s'"),
            ("caf\u{e9}", "'caf\u{e9}'"),
            ("$HOME", "'$HOME'"),
        ] {
            assert_eq!(
                shell_quote(OsStr::new(word)),
                OsString::from(want),
                "{word:?}"
            );
        }
        let raw = OsStr::from_bytes(b"/r/\xff");
        assert_eq!(shell_quote(raw).as_bytes(), b"'/r/\xff'");
        let argv: Vec<OsString> = ["git", "-C", "/tmp/my repo", "sparse-checkout", "disable"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(
            command_line(&argv),
            "git -C '/tmp/my repo' sparse-checkout disable"
        );
    }
}
