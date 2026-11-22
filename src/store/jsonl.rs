//! Append-only JSONL files (events, attempts, charges): `state._append` and
//! `state._read_jsonl`.
//!
//! Every append runs under the state lock: it drops a partial final line (a write in progress
//! when a process died), appends one whole line, flushes and fsyncs. Every read holds the lock
//! too, so a writer's tail repair cannot splice into it, and keeps whole lines only: a final
//! line with no newline is skipped, a whole line that does not parse is corruption.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::error::{Error, Result};
use crate::store::{fsync, fsync_dir, parent, state_lock};
use crate::wire::{self, RawLine};

/// How far back each read goes looking for a newline.
const CHUNK: u64 = 4096;

/// Appends `record` as one line of `file` in the run `run_dir`, under its state lock;
/// returns the line written (`state.append_jsonl`).
pub fn append_jsonl<T: Serialize + ?Sized>(
    run_dir: &Path,
    file: &Path,
    record: &T,
) -> Result<RawLine> {
    append_jsonl_with(run_dir, file, |_last| Ok(record))
}

/// Appends the record `build` makes from the file's last whole line (`None` for an empty
/// file), all under the state lock (`state._append`): how a record numbered from the one
/// before it, such as an event's `seq`, is appended.
pub fn append_jsonl_with<T: Serialize>(
    run_dir: &Path,
    file: &Path,
    build: impl FnOnce(Option<&RawValue>) -> Result<T>,
) -> Result<RawLine> {
    let _lock = state_lock(run_dir)?;
    let io = |e| Error::io(file, e);
    let created = !file.exists();
    let f = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o644)
        .open(file)
        .map_err(io)?;
    let mut size = f.metadata().map_err(io)?.len();
    if size > 0 {
        let mut final_byte = [0u8];
        f.read_exact_at(&mut final_byte, size - 1).map_err(io)?;
        if final_byte != *b"\n" {
            // A partial final line: a write in progress when its process died.
            let good = rfind_newline(&f, size, file)?.map_or(0, |i| i + 1);
            f.set_len(good).map_err(io)?;
            size = good;
        }
    }
    let last = if size > 0 {
        let start = rfind_newline(&f, size - 1, file)?.map_or(0, |i| i + 1);
        let mut line = vec![0u8; (size - 1 - start) as usize];
        f.read_exact_at(&mut line, start).map_err(io)?;
        Some(parse_line(line, file)?)
    } else {
        None
    };
    let line = wire::to_line(&build(last.as_deref())?)?;
    let mut data = Vec::with_capacity(line.len() + 1);
    data.extend_from_slice(line.as_bytes());
    data.push(b'\n');
    // O_APPEND: the kernel writes at the end of the file.
    (&f).write_all(&data).map_err(io)?;
    fsync(&f, file)?;
    drop(f);
    if created {
        fsync_dir(parent(file))?;
    }
    RawValue::from_string(line)
        .map_err(|e| Error::msg(format!("cannot keep the line written: {e}")))
}

/// Every whole line of `file`, read typed and kept raw; empty when the run directory or the
/// file is missing (`state._read_jsonl`).
pub fn read_jsonl<T: DeserializeOwned>(run_dir: &Path, file: &Path) -> Result<Vec<(T, RawLine)>> {
    if !run_dir.is_dir() {
        return Ok(Vec::new());
    }
    let raw = {
        let _lock = state_lock(run_dir)?;
        match fs::read(file) {
            Ok(raw) => raw,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(file, e)),
        }
    };
    let mut lines: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
    lines.pop(); // after the last newline: empty, or a line still being written
    lines
        .into_iter()
        .filter(|line| !line.iter().all(|&b| is_python_space(b)))
        .map(|line| {
            let raw = parse_line(line.to_vec(), file)?;
            let record = serde_json::from_str(raw.get()).map_err(|e| corrupt(file, &e))?;
            Ok((record, raw))
        })
        .collect()
}

/// The whitespace `bytes.strip()` removes.
fn is_python_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// One whole line as a raw JSON object (`state._parse_line`).
fn parse_line(line: Vec<u8>, file: &Path) -> Result<RawLine> {
    let text = String::from_utf8(line).map_err(|e| corrupt(file, &e))?;
    // JSON whitespace around the object, as json.loads allows it.
    let text = text.trim_matches([' ', '\t', '\n', '\r']).to_owned();
    let raw = RawValue::from_string(text).map_err(|e| corrupt(file, &e))?;
    if !raw.get().starts_with('{') {
        return Err(corrupt(file, &"not a JSON object"));
    }
    Ok(raw)
}

fn corrupt(file: &Path, why: &dyn std::fmt::Display) -> Error {
    let name = file
        .file_name()
        .unwrap_or(file.as_os_str())
        .to_string_lossy();
    Error::msg(format!("corrupt record in {name}: {why}"))
}

/// The offset of the last newline before offset `before`, if there is one.
fn rfind_newline(f: &File, before: u64, file: &Path) -> Result<Option<u64>> {
    let mut block = [0u8; CHUNK as usize];
    let mut end = before;
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let block = &mut block[..(end - start) as usize];
        f.read_exact_at(block, start)
            .map_err(|e| Error::io(file, e))?;
        if let Some(i) = block.iter().rposition(|&b| b == b'\n') {
            return Ok(Some(start + i as u64));
        }
        end = start;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::{Command, Stdio};

    use serde::{Deserialize, Serialize, Serializer};

    use super::*;
    use crate::state::{Event, append_event, events, run_paths};

    fn run_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("run with spaces")
            .tempdir()
            .unwrap()
    }

    #[derive(Deserialize)]
    struct Seq {
        seq: u64,
    }

    /// Appends a `ruling` event through `state::append_event`, which numbers it from the last
    /// one under the lock (S2-R11); its text names the writer and the line.
    fn append_ruling(run: &Path, who: &str, i: u32) -> u64 {
        let ruling = events::Ruling {
            text: Some(format!("{who} {i}").into()),
            ..events::Ruling::default()
        };
        let (event, _) = append_event(&run_paths(run), Event::Ruling(ruling)).unwrap();
        event.seq()
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Any {}

    #[test]
    fn read_missing_files_are_empty() {
        let tmp = run_dir();
        let run = tmp.path();
        for name in ["events.jsonl", "attempts.jsonl"] {
            assert!(read_jsonl::<Any>(run, &run.join(name)).unwrap().is_empty());
        }
    }

    #[test]
    fn read_of_missing_run_dir_is_empty() {
        let tmp = run_dir();
        let gone = tmp.path().join("nope");
        let read = read_jsonl::<Any>(&gone, &gone.join("events.jsonl")).unwrap();
        assert!(read.is_empty());
        assert!(!gone.exists());
    }

    #[test]
    fn failed_serialisation_leaves_file_intact() {
        struct Unserialisable;
        impl Serialize for Unserialisable {
            fn serialize<S: Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom(
                    "Object of type object is not JSON serializable",
                ))
            }
        }
        #[derive(Serialize)]
        struct Bad {
            kind: &'static str,
            bad: Unserialisable,
        }
        let tmp = run_dir();
        let run = tmp.path();
        append_ruling(run, "a", 0);
        let events = run.join("events.jsonl");
        let before = fs::read(&events).unwrap();
        let bad = Bad {
            kind: "ruling",
            bad: Unserialisable,
        };
        assert!(append_jsonl(run, &events, &bad).is_err());
        assert_eq!(fs::read(&events).unwrap(), before);
    }

    #[test]
    fn concurrent_appends_from_threads() {
        let tmp = run_dir();
        let run = tmp.path();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for i in 0..50 {
                        append_ruling(run, "t", i);
                    }
                });
            }
        });
        let events: Vec<(Seq, RawLine)> = read_jsonl(run, &run.join("events.jsonl")).unwrap();
        let mut seqs: Vec<u64> = events.iter().map(|(e, _)| e.seq).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=200).collect::<Vec<u64>>());
    }

    /// The run directory `two_process_append_child` appends to; unset in an ordinary run.
    const APPEND_CHILD: &str = "LEADHAND_STORE_TEST_APPEND_CHILD";

    /// The second writer's record: a `ruling` event numbered from the last line under the
    /// lock, naming its writer and number in `who` and `i`, padded long.
    #[derive(Serialize)]
    struct ChildLine {
        seq: u64,
        ts: &'static str,
        kind: &'static str,
        who: &'static str,
        i: u32,
        pad: String,
    }

    /// Not a test of its own: the second writer `concurrent_appends_from_two_processes_keep_
    /// unique_seq` starts (this test binary, running this test alone). It says it is ready,
    /// waits for the word to go, then appends 200 events to the run directory it was given.
    /// Does nothing in an ordinary run.
    #[test]
    fn two_process_append_child() {
        let Some(run) = std::env::var_os(APPEND_CHILD) else {
            return;
        };
        let run = Path::new(&run);
        // On a line of its own: libtest has printed `test <name> ... ` without a newline.
        println!("\nready");
        let mut go = String::new();
        std::io::stdin().read_line(&mut go).unwrap();
        let events = run.join("events.jsonl");
        for i in 0..200 {
            append_jsonl_with(run, &events, |last| {
                let seq = last.map_or(1, |l| serde_json::from_str::<Seq>(l.get()).unwrap().seq + 1);
                Ok(ChildLine {
                    seq,
                    ts: "2026-01-01T00:00:00.000Z",
                    kind: "ruling",
                    who: "child",
                    i,
                    pad: "x".repeat(300),
                })
            })
            .unwrap();
        }
    }

    /// Two processes append at once under `state.lock` (the stage-2 test raced the Python
    /// reference's writer; here the second writer is this test binary again): every line is
    /// whole, the seqs are 1..=400 in order, and each writer's lines are all there, in order.
    #[test]
    fn concurrent_appends_from_two_processes_keep_unique_seq() {
        let tmp = run_dir();
        let run = tmp.path();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "store::jsonl::tests::two_process_append_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(APPEND_CHILD, run)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the test binary");
        // Both writers start together: the child once it is running its test.
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        loop {
            ready.clear();
            if std::io::BufRead::read_line(&mut stdout, &mut ready).unwrap() == 0
                || ready == "ready\n"
            {
                break;
            }
        }
        assert_eq!(ready, "ready\n");
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"go\n").unwrap();
        for i in 0..200 {
            append_ruling(run, "rust", i);
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let raw = fs::read(run.join("events.jsonl")).unwrap();
        assert!(raw.ends_with(b"\n"));
        let lines: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
        let lines = &lines[..lines.len() - 1];
        assert_eq!(lines.len(), 400);
        // The child's lines name their writer and number in `who` and `i`; the parent's, in
        // the ruling's text.
        #[derive(Deserialize)]
        struct Line {
            seq: u64,
            who: Option<String>,
            i: Option<u32>,
            text: Option<String>,
        }
        let parsed: Vec<Line> = lines
            .iter()
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(
            parsed.iter().map(|e| e.seq).collect::<Vec<u64>>(),
            (1..=400).collect::<Vec<u64>>()
        );
        let writer = |e: &Line| -> (String, u32) {
            match (&e.who, e.i, &e.text) {
                (Some(who), Some(i), None) => (who.clone(), i),
                (None, None, Some(text)) => {
                    let (who, i) = text.split_once(' ').unwrap();
                    (who.to_string(), i.parse().unwrap())
                }
                _ => panic!("a line of neither writer (seq {})", e.seq),
            }
        };
        for who in ["child", "rust"] {
            let mine: Vec<u32> = parsed
                .iter()
                .map(writer)
                .filter(|(w, _)| w == who)
                .map(|(_, i)| i)
                .collect();
            assert_eq!(mine, (0..200).collect::<Vec<u32>>(), "{who}");
        }
        // The event reader reads every line either writer wrote.
        let n = crate::state::read_events(&run_paths(run)).unwrap().len();
        assert_eq!(n, 400);
    }

    #[test]
    fn appends_repair_a_partial_line_and_reads_skip_one() {
        let tmp = run_dir();
        let run = tmp.path();
        let charges = run.join("charges.jsonl");
        fs::write(&charges, b"{\"n\": 1}\n{\"n\": 2").unwrap();
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct N {
            n: u32,
        }
        let read = |run| -> Vec<u32> {
            read_jsonl::<N>(run, &charges)
                .unwrap()
                .into_iter()
                .map(|(r, _)| r.n)
                .collect()
        };
        assert_eq!(read(run), [1]);
        let line = append_jsonl(run, &charges, &N { n: 3 }).unwrap();
        assert_eq!(line.get(), r#"{"n":3}"#);
        assert_eq!(fs::read(&charges).unwrap(), b"{\"n\": 1}\n{\"n\":3}\n");
        assert_eq!(read(run), [1, 3]);

        // A whole line that does not parse is corruption, for a reader and an appender.
        fs::write(&charges, b"{\"n\": 1}\nnot json\n").unwrap();
        let err = read_jsonl::<N>(run, &charges).unwrap_err();
        assert!(
            err.message.starts_with("corrupt record in charges.jsonl: "),
            "{err}"
        );
        assert!(append_jsonl(run, &charges, &N { n: 4 }).is_err());
        fs::write(&charges, b"[1]\n").unwrap();
        let err = read_jsonl::<N>(run, &charges).unwrap_err();
        assert_eq!(
            err.message,
            "corrupt record in charges.jsonl: not a JSON object"
        );

        // Blank lines are skipped, as Python skips them.
        fs::write(&charges, b"{\"n\": 5}\n \x0b\n\n").unwrap();
        assert_eq!(read(run), [5]);
    }
}
