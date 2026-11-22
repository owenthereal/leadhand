//! Linux: the process table from `/proc`, read as the Python `procs` module reads it, and this
//! process's thread count. No `unsafe` here.
//!
//! The start time is field 22 of `/proc/<pid>/stat` (clock ticks since boot), written
//! `"ticks:<n>"` as the Python writes it; the owner is the owner of `/proc/<pid>`, as
//! `procs._linux_tagged` checks it.

use std::fs;
use std::os::unix::fs::MetadataExt;

use super::ProcInfo;

/// `procs._proc_stat`: `None` when the process is gone or its `stat` cannot be read.
pub(super) fn proc_info(pid: i32) -> Option<ProcInfo> {
    let raw = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let uid = fs::metadata(format!("/proc/{pid}")).ok()?.uid();
    parse_stat(pid, &raw, uid)
}

/// The fields of a `/proc/<pid>/stat` line. The command name (field 2) may hold spaces and
/// parentheses, so the fields are read after its last `)`.
fn parse_stat(pid: i32, raw: &[u8], uid: u32) -> Option<ProcInfo> {
    let close = raw.iter().rposition(|&b| b == b')')?;
    let rest: Vec<&[u8]> = raw
        .get(close + 2..)?
        .split(u8::is_ascii_whitespace)
        .filter(|f| !f.is_empty())
        .collect();
    if rest.len() < 20 {
        return None;
    }
    let number = |f: &[u8]| std::str::from_utf8(f).ok()?.parse::<i32>().ok();
    let state = rest[0];
    Some(ProcInfo {
        pid,
        ppid: number(rest[1])?,
        pgid: number(rest[2])?,
        start: format!("ticks:{}", std::str::from_utf8(rest[19]).ok()?),
        zombie: state == b"Z" || state == b"X",
        uid,
    })
}

/// A `/proc/<pid>/stat` line, read as `procs._proc_stat` reads it; the owner, which the line
/// does not hold, is 0.
#[cfg(test)]
pub(super) fn parse_record(pid: i32, raw: &[u8]) -> Option<ProcInfo> {
    parse_stat(pid, raw, 0)
}

/// `procs._proc_all`: every numeric entry of `/proc`.
pub(super) fn all_procs() -> Option<Vec<ProcInfo>> {
    let entries = fs::read_dir("/proc").ok()?;
    Some(
        entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter_map(proc_info)
            .collect(),
    )
}

/// Whether `/proc/<pid>/environ` holds `entry`; `None` when it cannot be read.
pub(super) fn env_has(pid: i32, entry: &[u8]) -> Option<bool> {
    let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(raw.split(|&b| b == 0).any(|e| e == entry))
}

/// The number of threads in this process, from the `Threads:` line of `/proc/self/status`;
/// `None` when it cannot be read.
pub(super) fn thread_count() -> Option<usize> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[u8] = b"S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 50\n";

    #[test]
    fn stat_fields_are_read_after_the_last_parenthesis() {
        let mut raw = b"1234 (a) (b c) ".to_vec();
        raw.extend_from_slice(FIELDS);
        let info = parse_stat(1234, &raw, 1000).unwrap();
        assert_eq!(
            info,
            ProcInfo {
                pid: 1234,
                ppid: 1,
                pgid: 1234,
                start: "ticks:98765".into(),
                zombie: false,
                uid: 1000
            }
        );
    }

    #[test]
    fn zombie_and_dead_states() {
        for (state, zombie) in [(b'Z', true), (b'X', true), (b'R', false)] {
            let mut raw = b"7 (sleep) ".to_vec();
            raw.extend_from_slice(FIELDS);
            raw[10] = state;
            assert_eq!(parse_stat(7, &raw, 0).unwrap().zombie, zombie);
        }
    }

    #[test]
    fn short_or_malformed_stat_is_none() {
        assert_eq!(parse_stat(7, b"7 (sleep) S 1 7", 0), None);
        assert_eq!(parse_stat(7, b"no parenthesis", 0), None);
    }

    #[test]
    fn thread_count_counts_this_process() {
        assert!(thread_count().expect("readable") >= 1);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _ = rx.recv();
        });
        started_rx.recv().unwrap();
        assert!(thread_count().unwrap() > 1);
        drop(tx);
        t.join().unwrap();
    }

    #[test]
    fn own_stat_matches_proc_self() {
        let me = std::process::id().try_into().unwrap();
        let raw = fs::read("/proc/self/stat").unwrap();
        let text = String::from_utf8_lossy(&raw);
        let field22 = text[text.rfind(')').unwrap() + 2..]
            .split_whitespace()
            .nth(19)
            .unwrap()
            .to_owned();
        assert_eq!(proc_info(me).unwrap().start, format!("ticks:{field22}"));
        assert_eq!(env_has(me, b"LEADHAND_SURELY_NOT_SET=1"), Some(false));
    }
}
