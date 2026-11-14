//! The error every command returns, and the exit code it maps to.
//!
//! Exit codes (spec, contract item 3): 0 success; 1 an error, with `error: …` on stderr;
//! 2 a usage error (clap's); 3 a launch refused because the run is owned, or a second runner
//! for a dispatch that already has one; 75 `dispatch` still running; 97 a test crash point
//! (`testing::crash_point`, testing builds only).

use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

use crate::wire::PyStr;

/// An error with its display text and exit code. The message is display text: paths in it
/// are converted lossily, so no JSON a command prints ever takes text from an `Error`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    /// The one command that fixes it, for the commands a person types: printed alone as the
    /// last line, ready to copy.
    pub fix: Option<String>,
    pub exit: u8,
}

pub type Result<T> = std::result::Result<T, Error>;

pub const EXIT_ERROR: u8 = 1;
pub const EXIT_REFUSED: u8 = 3;
pub const EXIT_STILL_RUNNING: u8 = 75;

impl Error {
    fn new(message: impl Into<String>, exit: u8) -> Self {
        Error {
            message: message.into(),
            fix: None,
            exit,
        }
    }

    /// An error: `error: <m>` on stderr, exit 1.
    pub fn msg(m: impl Into<String>) -> Self {
        Self::new(m, EXIT_ERROR)
    }

    /// A launch refused because the run is owned (or a second runner for a dispatch): exit 3.
    pub fn refused(m: impl Into<String>) -> Self {
        Self::new(m, EXIT_REFUSED)
    }

    /// `dispatch` is still running: `m` is the whole line, printed on stdout; exit 75.
    pub fn still_running(m: impl Into<String>) -> Self {
        Self::new(m, EXIT_STILL_RUNNING)
    }

    /// An I/O error on `path`, worded as Python's `OSError` words it, so the file is named:
    /// `[Errno 2] No such file or directory: '<path>'`, the path shown lossily.
    pub fn io(path: &Path, err: std::io::Error) -> Self {
        Failure::io(path, err).into()
    }

    /// The command that fixes this error; the commands a person types print it as the last line.
    pub fn with_fix(self, command: String) -> Self {
        Error {
            fix: Some(command),
            ..self
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::msg(e.to_string())
    }
}

/// A failure whose text a command prints as data (`check --json`), so it is kept lossless:
/// paths and Git's output in it are decoded as `PyStr::from_os` decodes them. For display it
/// converts into an `Error` (exit 1) whose message is the same text, shown lossily. It carries
/// no fix command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub text: PyStr,
}

impl Failure {
    /// Text with no path in it.
    pub fn msg(text: &str) -> Failure {
        Failure { text: text.into() }
    }

    /// The text made of `parts`, generated words and OS bytes alike.
    pub fn of(parts: &[&OsStr]) -> Failure {
        Failure {
            text: PyStr::from_os_parts(parts),
        }
    }

    /// An I/O error on `path`, as `Error::io` words it, with the path kept.
    pub fn io(path: &Path, err: std::io::Error) -> Failure {
        let head = match err.raw_os_error() {
            Some(n) => {
                let text = err.to_string();
                let text = text
                    .strip_suffix(&format!(" (os error {n})"))
                    .unwrap_or(&text)
                    .to_string();
                format!("[Errno {n}] {text}: '")
            }
            None => format!("{err}: '"),
        };
        Failure::of(&[OsStr::new(&head), path.as_os_str(), OsStr::new("'")])
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text.shown())
    }
}

impl From<Failure> for Error {
    fn from(f: Failure) -> Error {
        Error::msg(f.text.shown())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(Error::msg("bad deck").exit, 1);
        assert_eq!(Error::refused("run owned by pid 42").exit, 3);
        assert_eq!(
            Error::still_running("still running: run the same command again").exit,
            75
        );
        let e =
            Error::msg("no run here").with_fix("leadhand status 2026-11-10-retry-uploads".into());
        assert_eq!(e.exit, 1);
        assert_eq!(e.message, "no run here");
        assert_eq!(
            e.fix.as_deref(),
            Some("leadhand status 2026-11-10-retry-uploads")
        );
        assert_eq!(Error::refused("x").fix, None);
        assert_eq!(Error::refused("x").with_fix("y".into()).exit, 3);
        assert_eq!(
            Error::from(std::io::Error::other("disk full")),
            Error::msg("disk full")
        );
    }

    #[test]
    fn io_errors_name_the_path() {
        let missing = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            Error::io(Path::new("/runs/r 1/events.jsonl"), missing).message,
            "[Errno 2] No such file or directory: '/runs/r 1/events.jsonl'"
        );
        let e = Error::io(Path::new("/x"), std::io::Error::other("short write"));
        assert_eq!((e.message.as_str(), e.exit), ("short write: '/x'", 1));
    }
}
