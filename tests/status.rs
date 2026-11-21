//! What `status [RUN_DIR]` prints about a run directory, and the status.json and
//! meter-start.json that the engine's checkpoints write.
//!
//! The Rust form of `tests/contract/contract_status.py` (converted from tests/test_status.py).
//! Run directories are built with `RunDir`; the checkpoint files come from `FakeSupervisor`
//! runs, since only `next` and `record` write them. `status` with no argument is
//! reference-only (deliberate change 3) and is not converted; `tests/rust_status.rs` has the
//! Rust behaviour.

#[macro_use]
mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::json::Json;
use common::rundir::{RunDir, outcome};
use common::{Env, STATUS_KEYS, is_bin_command, key_set, set_of, shlex_quote};

/// The newest rollout files the Codex meter reads.
const METER_SCAN_FILES: usize = 20;

/// `status <run dir>`'s lines, in `env` (default the run directory's); it must succeed.
#[track_caller]
fn status(d: &RunDir, env: Option<&Env>) -> Vec<String> {
    let parent = d.root.parent().unwrap();
    let r = common::cli(&os_args!["status", d.root], env.unwrap_or(&d.env), parent).ok();
    r.stdout_text().lines().map(str::to_string).collect()
}

#[track_caller]
fn usage_line(lines: &[String]) -> String {
    lines
        .iter()
        .find(|line| line.starts_with("Usage: "))
        .unwrap_or_else(|| panic!("no Usage line in {lines:?}"))
        .clone()
}

/// The parked card's attention line, its next step naming the binary (by file: S1-R11).
fn is_unpark_line(line: &str, n: i64, root: &Path, reason: &str) -> bool {
    is_bin_command(
        line,
        &format!("Card {n} parked: {reason} — next: "),
        &format!(
            " unpark {} --card {n} --note '…'",
            shlex_quote(root.to_str().unwrap())
        ),
    )
}

mod status {
    use super::*;
    use crate::common::rundir::{AttemptOpts, ChargeOpts, DispatchOpts, RATE_LIMIT_NAME};
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    #[test]
    fn test_status_headline_while_implementing() {
        let w = RunDir::with_cards(5);
        w.accept(1);
        w.accept(2);
        w.dispatch_with(
            3,
            DispatchOpts {
                effort: "high",
                ..DispatchOpts::default()
            },
        );
        let lines = status(&w, None);
        assert_eq!(
            lines[0],
            "Running · Implementing card 3 (attempt 1, codex gpt-6.1-terra high) · 2 of 5 accepted"
        );
        assert!(
            lines.contains(
                &"Card 3: Card number 3 · started · attempt 1 · codex gpt-6.1-terra high"
                    .to_string()
            ),
            "{lines:?}"
        );
    }

    #[test]
    fn test_status_headline_while_reviewing() {
        let w = RunDir::with_cards(5);
        w.accept(1);
        w.accept(2);
        w.dispatch_with(
            3,
            DispatchOpts {
                kind: "review",
                role: "reviewer",
                vendor: "claude",
                model: "opus",
                effort: "high",
                ..DispatchOpts::default()
            },
        );
        assert_eq!(
            status(&w, None)[0],
            "Running · Reviewing card 3 · 2 of 5 accepted"
        );
    }

    #[test]
    fn test_status_attention_lists_parked_cards_and_decisions() {
        let w = RunDir::with_cards(5);
        w.accept(1);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(5)),
                ("branch", Json::from("b5")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "parked",
            [
                ("card", Json::from(5)),
                ("reason", Json::from("needs a design decision")),
            ],
        );
        w.dispatch(2);
        let lines = status(&w, None);
        assert!(w.root.to_str().unwrap().contains(' '));
        assert_eq!(
            lines[0],
            "Needs your decision on card 5; independent work continues"
        );
        assert!(
            is_unpark_line(&lines[1], 5, &w.root, "needs a design decision"),
            "{lines:?}"
        );
        assert!(
            lines[2..]
                .iter()
                .any(|l| l.starts_with("Card 5") || l.starts_with("5 ") || l.contains(" 5 ")),
            "{lines:?}"
        );
        assert!(lines.last().unwrap().contains("Usage"), "{lines:?}");
    }

    /// Covers R12: `accepted` is terminal. Once the fast-forward is recorded the card is in,
    /// whatever park or unpark comes after it.
    #[test]
    fn test_accepted_card_stays_accepted_after_a_late_park() {
        let w = RunDir::with_cards(1);
        let sha = "a".repeat(40);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(1)),
                ("branch", Json::from("leadhand/demo-2/c1")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "accept_started",
            [
                ("card", Json::from(1)),
                ("target", Json::from(sha.as_str())),
            ],
        );
        w.ev(
            "parked",
            [("card", Json::from(1)), ("reason", Json::from("operator"))],
        );
        w.ev(
            "accepted",
            [("card", Json::from(1)), ("sha", Json::from(sha.as_str()))],
        );
        w.ev(
            "unparked",
            [("card", Json::from(1)), ("note", Json::from("late"))],
        );
        let lines = status(&w, None);
        assert_eq!(
            lines[0], "Running · Waiting for the supervisor · 1 of 1 accepted",
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("Card 1: Card number 1 · accepted"),
            "{lines:?}"
        );
        let parked: Vec<&String> = lines.iter().filter(|l| l.contains("parked")).collect();
        assert!(parked.is_empty(), "{lines:?}");
    }

    /// Covers limit L7 (terminal escapes): worker text reaches `status` with no
    /// control-character filtering, so a park reason's escape sequence and bell are printed
    /// as they are, in the attention line and the card line.
    #[test]
    fn test_park_reason_control_characters_reach_status_unfiltered() {
        let reason = "\x1b[31mred\x1b[0m and a bell\x07";
        let w = RunDir::with_cards(1);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(1)),
                ("branch", Json::from("leadhand/demo-2/c1")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "parked",
            [("card", Json::from(1)), ("reason", Json::from(reason))],
        );
        let lines = status(&w, None);
        assert_eq!(lines[0], "Needs your decision on card 1", "{lines:?}");
        assert!(is_unpark_line(&lines[1], 1, &w.root, reason), "{lines:?}");
        assert_eq!(
            lines[2],
            format!("Card 1: Card number 1 · parked · {reason}"),
            "{lines:?}"
        );
    }

    #[test]
    fn test_ended_run_states() {
        let w = RunDir::with_cards(1);
        w.accept(1);
        w.ev("outcome", outcome("complete", "", &[]));
        assert_eq!(status(&w, None)[0], "Complete · 1 of 1 accepted");
        let w2 = RunDir::with_cards(2);
        w2.ev(
            "outcome",
            outcome("blocked", "paused", &["blocked: paused"]),
        );
        assert_eq!(status(&w2, None)[0], "Blocked (paused) · 0 of 2 accepted");
    }

    #[test]
    fn test_status_cli_without_status_json() {
        let w = RunDir::with_cards(3);
        w.dispatch(1);
        assert!(!w.paths.status.exists());
        let r = common::cli(
            &os_args!["status", w.root],
            &w.env,
            w.root.parent().unwrap(),
        );
        assert_eq!(r.code, 0, "{r}");
        assert!(
            r.stdout_text().starts_with("Running · Implementing card 1"),
            "{r}"
        );
    }

    #[test]
    fn test_corrupt_event_line_is_an_attention_line_not_a_failure() {
        let w = RunDir::with_cards(2);
        w.accept(1);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(2)),
                ("branch", Json::from("b2")),
                ("base", Json::from("b")),
            ],
        );
        let data = fs::read(&w.paths.events).unwrap();
        let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
        lines.insert(3, b"{not json}");
        fs::write(&w.paths.events, lines.join(&b'\n')).unwrap();
        let r = common::cli(
            &os_args!["status", w.root],
            &w.env,
            w.root.parent().unwrap(),
        );
        assert_eq!(r.code, 0, "{r}");
        let stdout = r.stdout_text();
        let out: Vec<&str> = stdout.lines().collect();
        assert!(out[0].ends_with(" · 1 of 2 accepted"), "{r}");
        assert!(
            out.contains(&"event log has an unreadable line at seq 4/line 4"),
            "{out:?}"
        );
    }

    #[test]
    fn test_partial_last_line_is_ignored() {
        let w = RunDir::with_cards(1);
        let mut f = OpenOptions::new()
            .append(true)
            .open(&w.paths.events)
            .unwrap();
        f.write_all(br#"{"seq": 9, "kind": "acc"#).unwrap();
        drop(f);
        let lines = status(&w, None);
        // no attention lines: the card rows follow the headline
        assert!(lines[0].starts_with("Running · "), "{lines:?}");
        assert!(lines[1].starts_with("Card 1: "), "{lines:?}");
    }

    #[test]
    fn test_complete_json_without_a_newline_is_ignored() {
        // A record is a line: a last record without its newline is an interrupted append,
        // unread even when its JSON is whole (test_state.py).
        let w = RunDir::with_cards(2);
        w.accept(1);
        let mut f = OpenOptions::new()
            .append(true)
            .open(&w.paths.events)
            .unwrap();
        let record = format!(
            r#"{{"seq": 99, "ts": "2026-01-01T00:00:00Z", "kind": "accepted", "card": 2, "sha": "{}"}}"#,
            "b".repeat(40)
        );
        f.write_all(record.as_bytes()).unwrap();
        drop(f);
        let lines = status(&w, None);
        assert!(lines[0].ends_with(" · 1 of 2 accepted"), "{lines:?}");
        // no attention lines: the card rows follow the headline
        assert!(lines[1].starts_with("Card 1: "), "{lines:?}");
    }

    #[test]
    fn test_usage_comes_from_charges_and_excludes_nothing_by_role() {
        let w = RunDir::with_cards(1);
        w.charge_with(
            "c1",
            "implementer",
            ChargeOpts {
                tokens: (1000, 100),
                ..ChargeOpts::default()
            },
        );
        w.charge_with(
            "c2",
            "proxy",
            ChargeOpts {
                tokens: (10, 1),
                ..ChargeOpts::default()
            },
        );
        w.charge_with(
            "c3",
            "supervisor",
            ChargeOpts {
                known: false,
                ..ChargeOpts::default()
            },
        );
        let d = w.dispatch(1);
        w.attempt_with(
            &d,
            1,
            AttemptOpts {
                usage_known: false,
                ..AttemptOpts::default()
            },
        );
        let line = usage_line(&status(&w, None));
        let head = line.split(" attempts with unknown usage").next().unwrap();
        let unknown = head
            .rsplit_once(" · ")
            .unwrap_or_else(|| panic!("no ` · ` in {line:?}"))
            .1;
        let unknown: i64 = unknown
            .parse()
            .unwrap_or_else(|e| panic!("{unknown:?} in {line:?}: {e}"));
        assert!(unknown >= 1, "{line:?}");
        common::cli(
            &os_args!["report", w.root],
            &w.env,
            w.root.parent().unwrap(),
        )
        .ok();
        let report = fs::read_to_string(&w.paths.report).unwrap();
        let usage = report
            .split("By role:\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no `By role:` in {report}"))
            .split("\n\n")
            .next()
            .unwrap();
        let roles: BTreeSet<String> = usage
            .lines()
            .map(|line| {
                line.get(2..)
                    .unwrap_or("")
                    .split(':')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(roles, set_of(&["implementer", "proxy", "supervisor"]));
    }

    #[test]
    fn test_codex_meter_reads_latest_record() {
        let w = RunDir::with_cards(1);
        let line = usage_line(&status(&w, None));
        assert!(line.contains("· Codex meter unknown ·"), "{line:?}");
        w.rate_limit_with(12.5, "2026-01-02T03:04:05.000Z", RATE_LIMIT_NAME);
        w.rate_limit_with(15.0, "2026-01-02T05:00:00.000Z", RATE_LIMIT_NAME);
        let line = usage_line(&status(&w, None));
        assert!(
            line.contains("· Codex meter 15.0% as of 2026-01-02T05:00:00.000Z ·"),
            "{line:?}"
        );
        let missing = w
            .env
            .with("CODEX_HOME", w.root.parent().unwrap().join("missing"));
        let line = usage_line(&status(&w, Some(&missing)));
        assert!(line.contains("· Codex meter unknown ·"), "{line:?}");
    }
}

mod fix_round {
    use super::*;
    use crate::common::rundir::{RATE_LIMIT_TS, write_rate_limit_with};
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn test_attention_lists_violations_with_preserved_ref_and_next_action() {
        let w = RunDir::with_cards(2);
        w.ev(
            "violation",
            [
                ("stage", Json::from("found")),
                ("by", Json::from("check")),
                ("dispatch_id", Json::from("c1-a1-review-1")),
                ("card", Json::from(1)),
                ("dispatch_kind", Json::from("review")),
                ("n", Json::from(1)),
                ("ref", Json::from("refs/leadhand/abc123/violations/1")),
                ("preserved", Json::object([])),
            ],
        );
        w.ev(
            "violation",
            [
                ("stage", Json::from("found")),
                ("by", Json::from("implementer")),
                ("n", Json::from(1)),
                ("ref", Json::from("refs/leadhand/abc123/leftovers/1")),
                ("after_dispatch", Json::from("c2-a1-implement-1")),
                ("card", Json::from(2)),
            ],
        );
        let att = status(&w, None);
        assert!(
            att.iter().any(|a| a.contains("review worker on card 1")
                && a.contains("refs/leadhand/abc123/violations/1")
                && a.contains("next: inspect refs/leadhand/abc123/violations/1")),
            "{att:?}"
        );
        assert!(
            att.iter()
                .any(|a| a.contains("refs/leadhand/abc123/leftovers/1")),
            "{att:?}"
        );
    }

    #[test]
    fn test_meter_scan_is_capped_to_newest_files() {
        let w = RunDir::with_cards(1);
        let old = write_rate_limit_with(&w.sessions, 5.0, RATE_LIMIT_TS, "rollout-0-old.jsonl");
        common::set_mtime(&old, UNIX_EPOCH + Duration::from_secs(1));
        let line = usage_line(&status(&w, None));
        assert!(line.contains("· Codex meter 5.0% as of "), "{line:?}");
        for i in 0..METER_SCAN_FILES {
            let d = w.sessions.join("x");
            if !d.is_dir() {
                fs::create_dir(&d).unwrap();
            }
            fs::write(d.join(format!("rollout-{i}.jsonl")), "{}\n").unwrap();
        }
        let line = usage_line(&status(&w, None));
        assert!(line.contains("· Codex meter unknown ·"), "{line:?}");
    }

    #[test]
    fn test_unreadable_log_is_an_attention_line() {
        let w = RunDir::with_cards(1);
        // A directory where the event log should be: opening it for reading fails with
        // EISDIR for every user, root included, where chmod(0) would not stop root.
        fs::remove_file(&w.paths.events).unwrap();
        fs::create_dir(&w.paths.events).unwrap();
        let att = status(&w, None);
        assert!(
            att.iter()
                .any(|a| a.starts_with("event log could not be read")),
            "{att:?}"
        );
    }
}

mod checkpoints {
    //! status.json and meter-start.json, written at the engine's checkpoints.
    use super::*;
    use crate::common::json;
    use crate::common::read_json;
    use crate::common::rundir::{RATE_LIMIT_NAME, write_rate_limit, write_rate_limit_with};
    use crate::common::supervisor::{FakeSupervisor, Options, card};
    use std::fs;

    #[test]
    fn test_checkpoint_writes_status_json() {
        let mut w = FakeSupervisor::new(&[card(1), card(2)]);
        w.start();
        w.next();
        let s = read_json(&w.paths().status);
        assert_eq!(key_set(&s), set_of(&STATUS_KEYS), "{s}");
        assert_json_eq!(s["run_id"], w.meta()["run_id"]);
        assert_json_eq!(s["deck"], Json::from("Demo deck"));
        assert_json_eq!(s["state"], Json::from("running"));
        assert_json_eq!(
            s["progress"],
            json::parse(br#"{"accepted": 0, "total": 2}"#)
        );
        assert_eq!(
            key_set(&s["usage"]),
            set_of(&[
                "estimated_usd",
                "unknown_attempts",
                "by_role",
                "codex_meter"
            ]),
            "{s}"
        );
        assert_eq!(
            key_set(&s["cards"][1]),
            set_of(&["number", "title", "state", "attempt", "route", "note"]),
            "{s}"
        );
    }

    #[test]
    fn test_engine_checkpoints_write_status_json_and_survive_status_errors() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.next();
        let status = w.paths().status;
        assert_json_eq!(
            read_json(&status)["progress"],
            json::parse(br#"{"accepted": 0, "total": 1}"#)
        );
        fs::remove_file(&status).unwrap();
        fs::create_dir(&status).unwrap(); // status.json can no longer be written
        let run_dir = w.run_dir.clone().unwrap();
        let r = w.cli(&os_args!["next", run_dir]);
        assert_eq!(r.code, 0, "{r}");
        assert!(r.json().get("action").is_some(), "{r}");
        assert!(
            r.stderr_text().contains("could not write status.json: "),
            "{r}"
        );
    }

    #[test]
    fn test_meter_before_is_taken_once_even_when_null() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        w.next();
        let meter_start = w.run_dir.as_ref().unwrap().join("meter-start.json");
        let null = json::parse(br#"{"percent": null, "observed_at": null}"#);
        assert_json_eq!(read_json(&meter_start), null);
        write_rate_limit(&w.codex_home.join("sessions"), 40.0);
        w.next();
        assert_json_eq!(read_json(&meter_start), null);
    }

    #[test]
    fn test_meter_is_not_scanned_at_every_checkpoint() {
        let mut w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        let sessions = w.codex_home.join("sessions");
        write_rate_limit_with(&sessions, 10.0, "2026-01-02T03:04:05.000Z", RATE_LIMIT_NAME);
        w.start();
        let a = w.next();
        let status = w.paths().status;
        let meter = || read_json(&status)["usage"]["codex_meter"]["percent"].clone();
        assert_json_eq!(meter(), Json::from(10.0));
        write_rate_limit_with(&sessions, 11.0, "2026-01-02T05:00:00.000Z", RATE_LIMIT_NAME);
        w.act(&a); // `record started` and `record result` are checkpoints too
        assert_json_eq!(meter(), Json::from(10.0));
        let mut actions = vec![a];
        actions.extend(w.run());
        let last = actions.last().unwrap();
        assert_json_eq!(last["action"], Json::from("done"), "{last}");
        assert_json_eq!(meter(), Json::from(11.0)); // read again once the run has ended
    }
}

mod status_during_run {
    use super::*;
    use crate::common::supervisor::{FakeSupervisor, Options, card};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const FINAL: &str = "Complete · 3 of 3 accepted";
    const TORN: [&str; 4] = [
        "unreadable line",
        "could not be read",
        "is missing or unreadable",
        "status rebuilt from status.json",
    ];

    #[derive(Default)]
    struct Seen {
        polls: usize,
        running: usize,
        dead: bool,
    }

    #[derive(Default)]
    struct Shared {
        stop: AtomicBool,
        seen: Mutex<Seen>,
        cond: Condvar,
        failures: Mutex<Vec<String>>,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The poller thread; dropping it stops it (the Python's `addCleanup(stop_poller)`).
    struct Poller {
        shared: Arc<Shared>,
        thread: Option<JoinHandle<()>>,
    }

    impl Poller {
        /// `stop_poller`: tells it to stop and waits up to 150 s for it (one status call is
        /// bounded by the harness's timeout). A thread still running then is left behind,
        /// as the Python's daemon thread is.
        fn stop(&mut self) {
            self.shared.stop.store(true, Ordering::SeqCst);
            let end = Instant::now() + Duration::from_secs(150);
            while self.is_alive() && Instant::now() < end {
                thread::sleep(Duration::from_millis(20));
            }
            if !self.is_alive()
                && let Some(t) = self.thread.take()
            {
                let _ = t.join();
            }
        }

        fn is_alive(&self) -> bool {
            self.thread.as_ref().is_some_and(|t| !t.is_finished())
        }
    }

    impl Drop for Poller {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Polls `status <run_dir>` as `w.cli` does (the world's environment, from its tmp)
    /// until told to stop, recording every failure.
    fn start_poller(env: common::Env, tmp: PathBuf, run_dir: PathBuf) -> Poller {
        let shared = Arc::new(Shared::default());
        let s = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("status poller".to_string())
            .spawn(move || {
                let died = common::panic_message(|| {
                    while !s.stop.load(Ordering::SeqCst) {
                        let r = common::cli(&os_args!["status", run_dir], &env, &tmp);
                        let stdout = r.stdout_text();
                        let first = stdout.split('\n').next().unwrap_or("");
                        let failure = if r.code != 0 {
                            Some(format!("status exited {}\n{r}", r.code))
                        } else if !(first.starts_with("Running") || first == FINAL) {
                            Some(format!("unexpected headline {first:?}\n{r}"))
                        } else if TORN.iter().any(|m| stdout.contains(m)) {
                            Some(format!("status saw a torn or unreadable file\n{r}"))
                        } else {
                            None
                        };
                        if let Some(f) = failure {
                            lock(&s.failures).push(f);
                        }
                        let mut seen = lock(&s.seen);
                        seen.polls += 1;
                        seen.running += usize::from(first.starts_with("Running"));
                        s.cond.notify_all();
                    }
                });
                if let Some(message) = died {
                    lock(&s.failures).push(message);
                }
                lock(&s.seen).dead = true;
                s.cond.notify_all();
            })
            .expect("starting the status poller");
        Poller {
            shared,
            thread: Some(thread),
        }
    }

    /// Polling `status` during a run never exits non-zero and never sees a torn line.
    #[test]
    fn test_status_polled_during_a_run_never_fails() {
        let mut w = FakeSupervisor::with_options(
            &[card(1), card(2), card(3)],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        let run_dir = w.start();
        let mut poller = start_poller(w.clock_env(), w.tmp.clone(), run_dir.clone());
        let shared = Arc::clone(&poller.shared);

        // Between engine steps, wait (bounded) for at least one more completed poll, so the
        // polls spread over the whole run rather than bunching where the scheduler puts them.
        let mut actions: Vec<Json> = Vec::new();
        let mut finished = false;
        for _ in 0..80 {
            {
                let seen = lock(&shared.seen);
                let target = seen.polls + 1;
                let (seen, _) = shared
                    .cond
                    .wait_timeout_while(seen, Duration::from_secs(60), |s| {
                        !(s.polls >= target || s.dead)
                    })
                    .unwrap_or_else(|e| e.into_inner());
                assert!(
                    seen.polls >= target || seen.dead,
                    "the status poller made no progress in 60 s"
                );
                assert!(
                    !seen.dead,
                    "the status poller died:\n{}",
                    lock(&shared.failures).join("\n")
                );
            }
            let a = w.next();
            actions.push(a.clone());
            if a["action"] == "done" {
                finished = true;
                break;
            }
            w.act(&a);
        }
        let tail = &actions[actions.len().saturating_sub(3)..];
        assert!(finished, "the run did not finish in 80 actions: {tail:?}");
        let running_polls = lock(&shared.seen).running;
        poller.stop();
        assert!(!poller.is_alive(), "the status poller did not stop");
        let failures = lock(&shared.failures).clone();
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));

        let a = actions.last().unwrap();
        assert_json_eq!(
            a.get("outcome").cloned().unwrap_or(Json::Null),
            Json::from("complete"),
            "{a}"
        );
        let out = w.cli(&os_args!["status", run_dir]).ok();
        assert_eq!(out.stdout_text().lines().next().unwrap_or(""), FINAL);
        // Every poll that finished before the last `next` saw the run in progress; there was
        // at least one per engine step before it.
        assert!(
            running_polls >= actions.len() - 1,
            "{running_polls} < {} - 1",
            actions.len()
        );
        assert!(actions.len() >= 9, "{actions:?}");
    }
}

mod state_lock {
    //! Contract item 12: <run>/state.lock is an exclusive flock(2) that another process
    //! contends on. While the test holds it, as another leadhand process would, the writers
    //! (`next`, `record started`) and the reader (`status`) wait and write nothing; once it
    //! is released they finish.
    use super::*;
    use crate::common::supervisor::{FakeSupervisor, Proc, card};
    use crate::common::{Out, hold_lock, holds, read_jsonl};
    use rustix::process::{Pid, Signal, kill_process_group};
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    struct Setup {
        w: FakeSupervisor,
        run_dir: PathBuf,
        events: PathBuf,
    }

    /// `setUp`: a world with one card, started.
    fn set_up() -> Setup {
        let mut w = FakeSupervisor::new(&[card(1)]);
        let run_dir = w.start();
        let events = run_dir.join("events.jsonl");
        Setup { w, run_dir, events }
    }

    /// The binary in the background in its own process group; dropping it kills the group.
    struct Background {
        argv: Vec<OsString>,
        proc: Proc,
    }

    impl Drop for Background {
        fn drop(&mut self) {
            if self.proc.poll().is_none()
                && let Some(pid) = Pid::from_raw(self.proc.pid())
            {
                let _ = kill_process_group(pid, Signal::KILL);
            }
            let _ = self.proc.communicate(Duration::from_secs(10));
        }
    }

    impl Setup {
        #[track_caller]
        fn background(&self, args: &[OsString]) -> Background {
            let mut argv = vec![common::bin().into_os_string()];
            argv.extend(args.iter().cloned());
            let child = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(&self.w.tmp)
                .env_clear()
                .envs(self.w.env.vars())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap_or_else(|e| panic!("could not start {argv:?}: {e}"));
            Background {
                argv,
                proc: Proc::new(child),
            }
        }

        /// Holds state.lock, starts `commands` (one command each) and `status`, checks they
        /// neither exit nor append for a second, releases, and returns how they finished.
        #[track_caller]
        fn wait_while_held(&self, commands: &[Vec<OsString>]) -> Vec<Out> {
            let before = fs::read(&self.events).unwrap();
            let held = hold_lock(&self.run_dir.join("state.lock"));
            let mut all: Vec<Vec<OsString>> = commands.to_vec();
            all.push(vec!["status".into(), self.run_dir.clone().into()]);
            let procs: Vec<Background> = all.iter().map(|a| self.background(a)).collect();
            holds(
                || {
                    procs.iter().all(|p| p.proc.poll().is_none())
                        && fs::read(&self.events).unwrap() == before
                },
                1.0,
                "the commands wait while another process holds state.lock",
            );
            drop(held);
            procs.iter().map(finish).collect()
        }
    }

    #[track_caller]
    fn finish(p: &Background) -> Out {
        let (stdout, stderr) = p
            .proc
            .communicate(Duration::from_secs(60))
            .unwrap_or_else(|| panic!("{:?} did not finish in 60 s", p.argv));
        Out {
            argv: p.argv.clone(),
            code: p.proc.returncode().unwrap(),
            stdout,
            stderr,
        }
    }

    #[test]
    fn test_writers_and_readers_wait_while_another_process_holds_state_lock() {
        let s = set_up();
        let outs = s.wait_while_held(&[vec!["next".into(), s.run_dir.clone().into()]]);
        let [nxt, st] = <[Out; 2]>::try_from(outs).unwrap();
        let a = nxt.ok().json();
        assert_json_eq!(a["action"], Json::from("dispatch"), "{a}");
        assert_eq!(st.code, 0, "{st}");
        assert!(st.stdout_text().starts_with("Running · "), "{st}");
        let kinds: Vec<Json> = read_jsonl(&s.events)
            .iter()
            .map(|e| e["kind"].clone())
            .collect();
        assert!(kinds.iter().any(|k| k == "card_cut"), "{kinds:?}");

        let did = a["dispatch_id"].as_os().unwrap();
        let outs = s.wait_while_held(&[vec![
            "record".into(),
            s.run_dir.clone().into(),
            "started".into(),
            "--dispatch".into(),
            did,
            "--worker".into(),
            "agent0001".into(),
        ]]);
        let [rec, st] = <[Out; 2]>::try_from(outs).unwrap();
        let rec_text = rec.to_string();
        assert_json_eq!(
            rec.ok().json()["kind"],
            Json::from("worker_started"),
            "{rec_text}"
        );
        assert_eq!(st.code, 0, "{st}");
        let events = read_jsonl(&s.events);
        assert_json_eq!(events.last().unwrap()["kind"], Json::from("worker_started"));
    }

    /// Holding run.lock, as a live launcher does, holds up neither a writer nor a reader.
    #[test]
    fn test_state_lock_is_not_the_run_lock() {
        let s = set_up();
        let _held = hold_lock(&s.run_dir.join("run.lock"));
        let timeout = Duration::from_secs(60);
        let a = common::cli_with(
            &os_args!["next", s.run_dir],
            &s.w.env,
            &s.w.tmp,
            None,
            timeout,
        )
        .ok()
        .json();
        assert_json_eq!(a["action"], Json::from("dispatch"), "{a}");
        common::cli_with(
            &os_args!["status", s.run_dir],
            &s.w.env,
            &s.w.tmp,
            None,
            timeout,
        )
        .ok();
    }
}
