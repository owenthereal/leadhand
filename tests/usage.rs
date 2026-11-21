//! Usage accounting: what `record result` (Claude workers) and `dispatch` (Codex workers)
//! charge, and how the run prices it.
//!
//! The Rust form of `tests/contract/contract_usage.py` (converted from tests/test_usage.py and
//! tests/test_usage_models.py). A Claude charge is read from charges.jsonl after `record
//! result`, with the worker's transcript written by the test under $CLAUDE_CONFIG_DIR/projects
//! (a FakeSupervisor step's `transcript`); one worker's later charges come from answer
//! dispatches that resume it. A Codex charge is read after a `dispatch` line, with the session
//! file written by the test under $CODEX_HOME/sessions and the `--json` stream played by the
//! fake Codex (`common::dispatch_world::World`). The attempt records (attempts.jsonl),
//! status.json and report.md show the totals and the cost estimates.
//!
//! Two invariants hold throughout: every token is charged exactly once (across resumed
//! workers, repeated charges, a crash between a charge and its attempt record, and concurrent
//! writers), and usage leadhand cannot read is recorded as unknown, never as zero.

#[macro_use]
mod common;

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use common::dispatch_world::{World, at, os_refs};
use common::json::{self, Json};
use common::supervisor::{CardOpts, CliOpts, FakeSupervisor, Options, Step, card_with, truthy};
use common::{Env, cli, jv, n_of, one, read_json};
use serde_json::{Value, json};

const OPUS: &str = "claude-opus-4-1";
const TOKENS: [&str; 5] = [
    "input",
    "cached",
    "cache_write_5m",
    "cache_write_1h",
    "output",
];

/// A Codex charge that reached no checkpoint.
fn nothing() -> Json {
    jv(r#"{"responses": [], "cumulative": {}}"#)
}

// --------------------------------------------------------------------------
// Source lines (field paths as Claude Code and Codex write them)

/// The input side of a Claude usage record (`claude_line`'s keyword arguments).
#[derive(Clone, Copy, Default)]
struct Tok {
    inp: i64,
    cached: i64,
    w5: i64,
    w1: i64,
}

const NONE: Tok = Tok {
    inp: 0,
    cached: 0,
    w5: 0,
    w1: 0,
};

/// An assistant message `mid` with `out` output tokens, of model OPUS.
fn claude_line(mid: &str, out: i64, t: Tok) -> String {
    claude_line_model(mid, out, t, OPUS)
}

fn claude_line_model(mid: &str, out: i64, t: Tok, model: &str) -> String {
    let usage = Json::object([
        ("input_tokens", t.inp.into()),
        ("cache_read_input_tokens", t.cached.into()),
        ("output_tokens", out.into()),
        ("cache_creation_input_tokens", (t.w5 + t.w1).into()),
        (
            "cache_creation",
            Json::object([
                ("ephemeral_5m_input_tokens", t.w5.into()),
                ("ephemeral_1h_input_tokens", t.w1.into()),
            ]),
        ),
    ]);
    json::dumps(&Json::object([
        ("type", "assistant".into()),
        (
            "message",
            Json::object([
                ("id", mid.into()),
                ("model", model.into()),
                ("role", "assistant".into()),
                ("usage", usage),
            ]),
        ),
    ]))
}

/// A rollout `token_count` event with these cumulative totals.
fn codex_line(inp: i64, cached: i64, out: i64, reasoning: i64) -> String {
    let total = Json::object([
        ("input_tokens", inp.into()),
        ("cached_input_tokens", cached.into()),
        ("output_tokens", out.into()),
        ("reasoning_output_tokens", reasoning.into()),
    ]);
    json::dumps(&Json::object([
        ("type", "event_msg".into()),
        (
            "payload",
            Json::object([
                ("type", "token_count".into()),
                (
                    "info",
                    Json::object([
                        ("total_token_usage", total.clone()),
                        ("last_token_usage", total),
                    ]),
                ),
            ]),
        ),
    ]))
}

/// Each line, then a newline.
fn text<S: AsRef<str>>(lines: &[S]) -> String {
    lines.iter().map(|l| format!("{}\n", l.as_ref())).collect()
}

fn fixture(name: &str) -> PathBuf {
    common::plugin::root().join(format!("tests/fixtures/{name}"))
}

fn fixture_text(name: &str) -> String {
    fs::read_to_string(fixture(name)).unwrap()
}

fn fixture_lines(name: &str) -> Vec<String> {
    fixture_text(name).lines().map(str::to_string).collect()
}

/// A charge's or attempt's usage fields.
#[track_caller]
fn tokens(c: &Json, fields: &[&str]) -> Vec<i64> {
    fields
        .iter()
        .map(|f| {
            c["usage"][*f]
                .as_i64()
                .unwrap_or_else(|| panic!("usage {f} is not an integer: {c}"))
        })
        .collect()
}

/// Every token field (`tokens(c)` with no fields).
#[track_caller]
fn all_tokens(c: &Json) -> Vec<i64> {
    tokens(c, &TOKENS)
}

#[track_caller]
fn summed(charges: &[&Json]) -> Vec<i64> {
    TOKENS
        .iter()
        .map(|f| charges.iter().map(|c| tokens(c, &[f])[0]).sum())
        .collect()
}

/// Python's `assertAlmostEqual(a, b)`: equal to 7 decimal places.
fn almost(a: &Json, b: f64) -> bool {
    a.as_f64().is_some_and(|a| (a - b).abs() < 5e-8)
}

fn os_string(s: impl AsRef<OsStr>) -> OsString {
    s.as_ref().to_os_string()
}

// --------------------------------------------------------------------------
// Claude workers: charged by `record result`

#[track_caller]
fn claude_run(route: Option<&str>, config: &str) -> FakeSupervisor {
    let mut w = FakeSupervisor::with_options(
        &[card_with(
            1,
            CardOpts {
                route,
                ..CardOpts::default()
            },
        )],
        Options {
            config,
            ..Options::default()
        },
    );
    w.start();
    w
}

fn run_dir(w: &FakeSupervisor) -> PathBuf {
    w.paths().root
}

/// A step with `status` (when given) and `transcript` (None: no transcript).
fn step(status: Option<&str>, transcript: Option<&str>) -> Step {
    Step {
        status: status.map(str::to_string),
        transcript: Some(transcript.map(str::to_string)),
        ..Step::default()
    }
}

/// Acts on `next` until `dispatches` more dispatches have run and been recorded.
#[track_caller]
fn drive(w: &mut FakeSupervisor, dispatches: usize) {
    let mut done = 0;
    while done < dispatches {
        let a = w.next();
        if a["action"] == "done" {
            panic!("the run finished after {done} of {dispatches} dispatches");
        }
        w.act(&a);
        done += usize::from(a["action"] == "dispatch");
    }
}

/// One Claude worker's dispatches in turn: card 1's implement, then answers that resume the
/// same worker (every dispatch but the last asks a question, which a ruling answers).
/// Dispatch i appends texts[i] to the worker's transcript (None: it has none yet). Returns the
/// worker's charges in dispatch order.
#[track_caller]
fn charge_worker(w: &mut FakeSupervisor, texts: &[Option<&str>]) -> Vec<Json> {
    let last = texts.len() - 1;
    let status = |i: usize| if i < last { "QUESTION" } else { "READY" };
    w.script(1, "implement", [step(Some(status(0)), texts[0])]);
    for (i, t) in texts.iter().enumerate().skip(1) {
        w.script(1, "answer", [step(Some(status(i)), *t)]);
    }
    w.rulings
        .entry(1)
        .or_default()
        .extend(vec!["A: JSON".to_string(); last]);
    drive(w, texts.len());
    let workers: BTreeSet<String> = w
        .events("worker_started")
        .iter()
        .map(|e| e["worker_id"].as_str().unwrap().to_string())
        .collect();
    if workers.len() != 1 {
        panic!("expected one resumed worker, got {workers:?}");
    }
    let charges: Vec<Json> = w
        .charges()
        .into_iter()
        .filter(|c| c["key"].as_str().is_some_and(|k| workers.contains(k)))
        .collect();
    let dids: Vec<String> = charges
        .iter()
        .map(|c| c["dispatch_id"].as_str().unwrap().to_string())
        .collect();
    if dids != w.dispatch_ids() {
        panic!("not one charge per dispatch: {charges:?}");
    }
    charges
}

/// The next dispatch, run by a worker the test names: `record started` with `worker`, then
/// `content` written to `transcript` (if given), then `record result` with a READY reply.
/// Returns the dispatch's charge.
#[track_caller]
fn record_as(
    w: &mut FakeSupervisor,
    worker: &str,
    transcript: Option<&Path>,
    content: &str,
) -> Json {
    let a = w.next();
    if a["action"] != "dispatch" {
        panic!("expected a dispatch, got {a}");
    }
    let did = a["dispatch_id"].as_str().unwrap().to_string();
    let rd = run_dir(w);
    w.cli(&os_args![
        "record",
        rd,
        "started",
        "--dispatch",
        did,
        "--worker",
        worker
    ])
    .ok();
    if let Some(t) = transcript {
        fs::create_dir_all(t.parent().unwrap()).unwrap();
        fs::write(t, content).unwrap();
    }
    let reply = PathBuf::from(a["reply_file"].as_os().unwrap());
    fs::create_dir_all(reply.parent().unwrap()).unwrap();
    fs::write(
        &reply,
        format!("Status: READY\nReport: {}\n", a["report"].as_str().unwrap()),
    )
    .unwrap();
    w.cli(&os_args![
        "record",
        rd,
        "result",
        "--dispatch",
        did,
        "--worker",
        worker,
        "--reply-file",
        reply
    ])
    .ok();
    one(w
        .charges()
        .into_iter()
        .filter(|c| c["dispatch_id"] == did.as_str())
        .collect())
}

/// status.json's usage, as the engine's next checkpoint (a `next`) writes it.
#[track_caller]
fn run_usage(run_dir: &Path, env: &Env, tmp: &Path) -> Json {
    cli(&os_args!["next", run_dir], env, tmp).ok();
    read_json(&run_dir.join("status.json"))["usage"].clone()
}

#[track_caller]
fn sup_usage(w: &FakeSupervisor) -> Json {
    run_usage(&run_dir(w), &w.env, &w.tmp)
}

#[track_caller]
fn world_usage(w: &World) -> Json {
    run_usage(&w.run_dir, &w.env, &w.tmp)
}

#[track_caller]
fn attempt_of(attempts: Vec<Json>, did: &str) -> Json {
    one(attempts
        .into_iter()
        .filter(|a| a["dispatch"] == did)
        .collect())
}

/// The implementer's input, cached and output totals in a run's usage.
#[track_caller]
fn implementer_totals(usage: &Json) -> Vec<i64> {
    let i = &usage["by_role"]["implementer"];
    ["input", "cached", "output"]
        .iter()
        .map(|f| i[*f].as_i64().unwrap())
        .collect()
}

/// A checkpoint's message keys.
fn message_keys(c: &Json) -> BTreeSet<String> {
    common::key_set(&c["checkpoint"]["messages"])
}

// --------------------------------------------------------------------------
// Codex workers: charged by the `dispatch` runner

fn session_file(w: &World, thread: &str) -> PathBuf {
    let path = PathBuf::from(w.env.get("CODEX_HOME").unwrap())
        .join("sessions/2026/11/10")
        .join(format!("rollout-2026-11-10T00-00-00-{thread}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

fn append<S: AsRef<str>>(path: &Path, lines: &[S]) {
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    f.write_all(text(lines).as_bytes()).unwrap();
}

fn turn_completed(inp: i64, cached: i64, out: i64, reasoning: i64) -> Value {
    json!({"type": "turn.completed", "usage": {"input_tokens": inp, "cached_input_tokens": cached,
        "output_tokens": out, "reasoning_output_tokens": reasoning}})
}

/// A Codex run that replies READY: the thread starts (unless `named` is false), then the
/// `turns` (turn.completed events) are streamed. `open_turn`: a turn starts and never
/// completes.
fn codex_steps(turns: &[Value], named: bool, open_turn: bool) -> Vec<Value> {
    let mut steps = Vec::new();
    if named {
        steps.push(json!({"type": "thread.started", "thread_id": "{thread}"}));
    }
    if open_turn {
        steps.push(json!({"type": "turn.started"}));
    }
    steps.push(json!({"action": "last_message", "text": "Status: READY\n"}));
    steps.extend(turns.iter().cloned());
    steps
}

/// Runs the `dispatch` line for `did` with the fake Codex on `thread`; returns its charge.
#[track_caller]
fn codex_charge_steps(w: &mut World, did: &str, thread: &str, steps: Vec<Value>) -> Json {
    w.scenario_steps(&steps, &[("thread", thread)]);
    w.dispatch(did).ok();
    one(w
        .charges()
        .into_iter()
        .filter(|c| c["dispatch_id"] == did)
        .collect())
}

/// `codex_charge(w, did, thread, *turns)`.
#[track_caller]
fn codex_charge(w: &mut World, did: &str, thread: &str, turns: &[Value]) -> Json {
    codex_charge_steps(w, did, thread, codex_steps(turns, true, false))
}

fn place_rollout<S: AsRef<str>>(w: &World, thread: &str, lines: &[S]) -> PathBuf {
    let path = session_file(w, thread);
    fs::write(&path, text(lines)).unwrap();
    path
}

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const ROLLOUT: &str = "codex-rollout-interrupted.jsonl";

/// A completed turn whose usage the session file overrides.
fn stream_turn() -> Value {
    turn_completed(1, 0, 1, 0)
}

// --------------------------------------------------------------------------
// Claude

mod claude_transcripts {
    //! How a worker's transcript is read: once per message ID, the record with the most
    //! output.
    use super::*;

    #[test]
    fn test_claude_usage_dedupes_by_message_id() {
        // Covers P13.
        let mut w = claude_run(None, "");
        let content = fixture_text("claude-subagent-duplicates.jsonl");
        let [c] = n_of(charge_worker(&mut w, &[Some(&content)]));
        assert_eq!(all_tokens(&c), [130, 1500, 250, 300, 130]);
        assert!(truthy(&c["usage"]["known"]), "{c}");
        assert_eq!(c["usage"]["model"], OPUS);
        assert_json_eq!(
            c["checkpoint"],
            jv(r#"{"messages": {"msg_A": 40, "msg_B": 60, "msg_C": 30}}"#)
        );
    }

    #[test]
    fn test_claude_usage_skips_messages_already_charged() {
        let mut w = claude_run(None, "");
        let lines = fixture_lines("claude-subagent-duplicates.jsonl");
        let (a, b) = (text(&lines[..4]), text(&lines[4..]));
        let [first, second] = n_of(charge_worker(&mut w, &[Some(&a), Some(&b)]));
        assert_json_eq!(first["checkpoint"], jv(r#"{"messages": {"msg_A": 40}}"#));
        assert_eq!(all_tokens(&second), [30, 500, 50, 0, 90]);
        assert_json_eq!(
            second["checkpoint"],
            jv(r#"{"messages": {"msg_B": 60, "msg_C": 30}}"#)
        );
    }

    #[test]
    fn test_claude_usage_ignores_partial_and_garbage_lines() {
        let mut w = claude_run(None, "");
        let partial: String = claude_line("m2", 9, NONE).chars().take(30).collect();
        let content = text(&[
            claude_line("m1", 4, Tok { inp: 1, ..NONE }),
            "not json".to_string(),
            "[1,2]".to_string(),
        ]) + &partial;
        let [c] = n_of(charge_worker(&mut w, &[Some(&content)])); // the partial final line has no newline
        assert_json_eq!(c["checkpoint"], jv(r#"{"messages": {"m1": 4}}"#));
        assert_eq!(tokens(&c, &["input", "output"]), [1, 4]);
        assert!(truthy(&c["usage"]["known"]), "{c}");
    }

    #[test]
    fn test_stopped_transcript_charges_its_one_message_once() {
        let mut w = claude_run(None, "");
        let content = fixture_text("claude-subagent-stopped.jsonl");
        let [c, again] = n_of(charge_worker(&mut w, &[Some(&content), Some("")]));
        assert_eq!(all_tokens(&c), [2, 2077, 5811, 0, 117]);
        assert_json_eq!(
            c["checkpoint"],
            jv(r#"{"messages": {"msg_fixture_1": 117}}"#)
        );
        assert_eq!(tokens(&again, &["input", "output"]), [0, 0]);
    }

    #[test]
    fn test_resumed_transcript_charges_each_message_once() {
        let mut w = claude_run(None, "");
        let content = fixture_text("claude-subagent-resumed.jsonl");
        let [c] = n_of(charge_worker(&mut w, &[Some(&content)]));
        assert_eq!(
            tokens(&c, &["input", "cached", "cache_write_5m", "output"]),
            [12, 27653, 6384, 734]
        ); // 734 per message ID, not 1468
    }

    #[test]
    fn test_resumed_file_growth_is_charged_once() {
        let mut w = claude_run(None, "");
        let lines = fixture_lines("claude-subagent-resumed.jsonl");
        let (a, b) = (text(&lines[..30]), text(&lines[30..]));
        let [c1, c2, c3] = n_of(charge_worker(&mut w, &[Some(&a), Some(&b), Some("")])); // the resume appends
        assert_eq!(tokens(&c1, &["input", "output"]), [4, 99 + 411]);
        assert_eq!(tokens(&c2, &["input", "output"]), [8, 111 + 113]);
        assert_eq!(tokens(&c3, &["input", "output"]), [0, 0]);
        // The whole transcript, charged at once
        // (test_resumed_transcript_charges_each_message_once).
        assert_eq!(summed(&[&c1, &c2, &c3]), [12, 27653, 6384, 0, 734]);
        assert_eq!(implementer_totals(&sup_usage(&w)), [12, 27653, 734]);
    }
}

mod claude_charges {
    //! charges.jsonl after `record result`: each message charged once, unknown never zero.
    use super::*;

    #[test]
    fn test_charge_claude_resumed_worker_counts_each_message_once() {
        let mut w = claude_run(None, "");
        let implementation = text(&[
            claude_line(
                "m1",
                10,
                Tok {
                    inp: 100,
                    cached: 1000,
                    w5: 50,
                    ..NONE
                },
            ),
            claude_line(
                "m1",
                20,
                Tok {
                    inp: 100,
                    cached: 1000,
                    w5: 50,
                    ..NONE
                },
            ), // repeated block
            claude_line(
                "m2",
                30,
                Tok {
                    inp: 5,
                    w1: 7,
                    ..NONE
                },
            ),
        ]);
        let resumed_fix = text(&[
            claude_line(
                "m3",
                40,
                Tok {
                    inp: 11,
                    cached: 2000,
                    ..NONE
                },
            ),
            claude_line(
                "m4",
                50,
                Tok {
                    inp: 12,
                    cached: 3000,
                    w5: 9,
                    ..NONE
                },
            ),
        ]);
        let interrupted = text(&[claude_line("m5", 60, Tok { inp: 13, ..NONE })]);
        let [c1, c2, c3, c4] = n_of(charge_worker(
            &mut w,
            &[
                Some(&implementation),
                Some(&resumed_fix),
                Some(&interrupted),
                Some(""),
            ],
        ));
        assert_eq!(
            [&c1, &c2, &c3].map(|c| tokens(c, &["output"])[0]),
            [50, 90, 60]
        );
        let total = [
            100 + 5 + 11 + 12 + 13,
            1000 + 2000 + 3000,
            50 + 9,
            7,
            20 + 30 + 40 + 50 + 60,
        ];
        assert_eq!(summed(&[&c1, &c2, &c3]), total);
        let keys: BTreeSet<String> = [&c1, &c2, &c3]
            .iter()
            .flat_map(|c| message_keys(c))
            .collect();
        assert_eq!(keys, common::set_of(&["m1", "m2", "m3", "m4", "m5"]));
        // a further charge with nothing new moves nothing
        assert_eq!(tokens(&c4, &["input", "output"]), [0, 0]);
        assert!(truthy(&c4["usage"]["known"]), "{c4}");
        assert_eq!(
            implementer_totals(&sup_usage(&w)),
            [total[0], total[1], total[4]]
        );
    }

    #[test]
    fn test_charge_record_is_durable_and_round_trips() {
        let mut w = claude_run(None, "");
        let content = text(&[claude_line("m1", 10, Tok { inp: 3, ..NONE })]);
        let [c] = n_of(charge_worker(&mut w, &[Some(&content)]));
        let raw: Vec<Json> = fs::read_to_string(&w.paths().charges)
            .unwrap()
            .lines()
            .map(jv)
            .collect();
        assert_json_eq!(Json::Arr(raw), Json::Arr(vec![c.clone()]));
        let dids = w.dispatch_ids();
        let [did] = <[String; 1]>::try_from(dids).unwrap();
        let worker = w.events("worker_started")[0]["worker_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(c["charge_id"], format!("{did}:claude:{worker}").as_str());
        assert_json_eq!(
            Json::from(vec![
                c["source"].clone(),
                c["key"].clone(),
                c["dispatch_id"].clone(),
                c["role"].clone()
            ]),
            Json::from(&["claude", worker.as_str(), did.as_str(), "implementer"][..])
        );
        assert_eq!(c["usage"]["known"], Json::Bool(true));
        assert_eq!(c["usage"]["output"], 10);
        assert_json_eq!(c["checkpoint"], jv(r#"{"messages": {"m1": 10}}"#));
        let attempt = attempt_of(w.attempts(), &did);
        assert_json_eq!(
            attempt["charge_ids"],
            Json::Arr(vec![c["charge_id"].clone()])
        );
        assert_json_eq!(attempt["usage"], c["usage"]);
    }

    #[test]
    fn test_charge_tops_up_message_charged_mid_stream() {
        // Covers R17.
        let mut w = claude_run(None, "");
        let m1 = |out| {
            claude_line(
                "m1",
                out,
                Tok {
                    inp: 100,
                    cached: 1000,
                    w5: 50,
                    ..NONE
                },
            )
        };
        let (a, b) = (
            text(&[m1(5)]),
            text(&[m1(40), claude_line("m2", 7, Tok { inp: 3, ..NONE })]),
        );
        let [c1, c2, c3] = n_of(charge_worker(&mut w, &[Some(&a), Some(&b), Some("")]));
        assert_eq!(tokens(&c1, &["input", "output"]), [100, 5]);
        // m1: only the 35 extra output (its input side was charged with its first record); m2
        // in full
        assert_eq!(
            tokens(&c2, &["input", "cached", "cache_write_5m", "output"]),
            [3, 0, 0, 35 + 7]
        );
        assert_json_eq!(c2["checkpoint"], jv(r#"{"messages": {"m1": 40, "m2": 7}}"#));
        assert_eq!(c3["usage"]["output"], 0); // nothing grew
        assert_json_eq!(c3["checkpoint"], jv(r#"{"messages": {}}"#));
        assert_eq!(summed(&[&c1, &c2, &c3]), [103, 1000, 50, 0, 47]);
        assert_eq!(implementer_totals(&sup_usage(&w)), [103, 1000, 47]);
    }

    #[test]
    fn test_charge_missing_file_is_unknown_and_moves_no_checkpoint() {
        let mut w = claude_run(None, "");
        let later = text(&[claude_line("m1", 10, Tok { inp: 2, ..NONE })]);
        let [c1, c2] = n_of(charge_worker(&mut w, &[None, Some(&later)]));
        assert!(!truthy(&c1["usage"]["known"]), "{c1}");
        assert_json_eq!(c1["checkpoint"], jv(r#"{"messages": {}}"#));
        let did1 = c1["dispatch_id"].as_str().unwrap();
        assert!(!truthy(&attempt_of(w.attempts(), did1)["usage_known"]));
        // the file appears later: everything is still chargeable
        assert!(truthy(&c2["usage"]["known"]), "{c2}");
        assert_eq!(tokens(&c2, &["input", "output"]), [2, 10]);
        // the totals say the run has an unknown charge
        let usage = sup_usage(&w);
        assert_eq!(usage["unknown_attempts"], 1);
        assert_eq!(usage["by_role"]["implementer"]["unknown_charges"], 1);
    }

    #[test]
    fn test_totals_group_by_role_dispatch_and_source() {
        let mut w = claude_run(None, "");
        let (impl_t, review_t) = (
            text(&[claude_line("m1", 10, NONE)]),
            text(&[claude_line("m9", 5, NONE)]),
        );
        w.script(1, "implement", [step(None, Some(&impl_t))]);
        w.script(1, "review", [step(None, Some(&review_t))]);
        drive(&mut w, 2);
        let [impl_did, review_did] = <[String; 2]>::try_from(w.dispatch_ids()).unwrap();
        assert_eq!(attempt_of(w.attempts(), &impl_did)["usage"]["output"], 10);
        assert_eq!(attempt_of(w.attempts(), &review_did)["usage"]["output"], 5);
        let usage = sup_usage(&w);
        let outputs: Vec<(String, Json)> = usage["by_role"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), v["output"].clone()))
            .collect();
        assert_json_eq!(
            Json::Obj(
                outputs
                    .into_iter()
                    .map(|(k, v)| (k.into_bytes(), v))
                    .collect()
            ),
            jv(r#"{"implementer": 10, "reviewer": 5}"#)
        );
        cli(&os_args!["report", run_dir(&w)], &w.env, &w.tmp).ok();
        let report = fs::read_to_string(run_dir(&w).join("report.md")).unwrap();
        let section = report
            .split("By vendor:\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no By vendor section in\n{report}"));
        let by_vendor: Vec<&str> = section.split("\n\n").next().unwrap().lines().collect();
        assert_eq!(by_vendor.len(), 1, "{by_vendor:?}");
        assert!(
            by_vendor[0].starts_with("- claude: ")
                && by_vendor[0].ends_with(" (0 input, 15 output tokens)"),
            "{by_vendor:?}"
        );
    }

    /// The reply file of `a`, with `reply` written to it.
    fn write_reply(a: &Json, reply: &str) -> PathBuf {
        let reply_file = PathBuf::from(a["reply_file"].as_os().unwrap());
        fs::create_dir_all(reply_file.parent().unwrap()).unwrap();
        fs::write(&reply_file, reply).unwrap();
        reply_file
    }

    fn result_args(w: &FakeSupervisor, a: &Json, worker: &str, reply_file: &Path) -> Vec<OsString> {
        vec![
            os_string("record"),
            os_string(run_dir(w)),
            os_string("result"),
            os_string("--dispatch"),
            os_string(a["dispatch_id"].as_str().unwrap()),
            os_string("--worker"),
            os_string(worker),
            os_string("--reply-file"),
            os_string(reply_file),
        ]
    }

    #[test]
    fn test_crash_after_charge_before_attempt_keeps_exact_total() {
        let mut w = claude_run(None, "");
        let t = text(&[
            claude_line("m1", 10, Tok { inp: 3, ..NONE }),
            claude_line("m2", 20, Tok { cached: 5, ..NONE }),
        ]);
        w.script(1, "implement", [step(None, Some(&t))]);
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.claude_worker(&a, &worker).expect("the worker's reply");
        let reply_file = write_reply(&a, &reply);
        let args = result_args(&w, &a, &worker, &reply_file);
        let crashed = w.cli_with(
            &os_refs(&args),
            CliOpts {
                crash: Some("after_charge_before_attempt"),
                ..CliOpts::default()
            },
        );
        assert_eq!(crashed.code, 97, "{crashed}");
        assert_eq!(w.charges().len(), 1);
        assert_eq!(w.attempts(), Vec::<Json>::new());
        w.cli(&os_refs(&args)).ok();
        let (charges, attempts) = (w.charges(), w.attempts());
        assert_eq!(charges.len(), 1);
        assert_eq!(attempts.len(), 1);
        assert_json_eq!(
            attempts[0]["charge_ids"],
            Json::Arr(vec![charges[0]["charge_id"].clone()])
        );
        assert_eq!(all_tokens(&charges[0]), [3, 5, 0, 0, 30]);
    }

    #[test]
    fn test_repeated_charge_id_is_harmless() {
        // The charge of a `record result` that crashed before its attempt record is the one
        // the retry finds, although the transcript has grown since; the growth is charged
        // later.
        let mut w = claude_run(None, "");
        let t = text(&[claude_line("m1", 10, NONE)]);
        w.script(1, "implement", [step(Some("QUESTION"), Some(&t))]);
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.claude_worker(&a, &worker).expect("the worker's reply");
        let reply_file = write_reply(&a, &reply);
        let args = result_args(&w, &a, &worker, &reply_file);
        assert_eq!(
            w.cli_with(
                &os_refs(&args),
                CliOpts {
                    crash: Some("after_charge_before_attempt"),
                    ..CliOpts::default()
                },
            )
            .code,
            97
        );
        let first = one(w.charges());
        append(
            &w.subagent_transcript(&worker),
            &[claude_line("m2", 99, NONE)],
        );
        w.cli(&os_refs(&args)).ok();
        assert_json_eq!(Json::Arr(w.charges()), Json::Arr(vec![first.clone()]));
        assert_eq!(first["usage"]["output"], 10);
        w.rulings.insert(1, vec!["A: JSON".to_string()]);
        w.script(1, "answer", [step(None, Some(""))]);
        drive(&mut w, 1);
        let [_, later] = n_of(w.charges());
        assert_json_eq!(later["checkpoint"], jv(r#"{"messages": {"m2": 99}}"#));
        assert_eq!(later["usage"]["output"], 99);
    }
}

mod locators {
    //! Where the transcripts are found: $CLAUDE_CONFIG_DIR/projects/<project>/<session>/
    //! subagents/agent-<worker id>.jsonl for a worker,
    //! $CLAUDE_CONFIG_DIR/projects/<project>/<session id>.jsonl for the supervisor.
    use super::*;

    fn session_env(sid: &str) -> CliOpts<'static> {
        CliOpts {
            env: vec![(os_string("CLAUDE_CODE_SESSION_ID"), os_string(sid))],
            ..CliOpts::default()
        }
    }

    #[test]
    fn test_claude_transcript_locators() {
        let mut w = claude_run(None, "");
        let projects = w.claude_home.join("projects");
        let sub = projects.join("-some-project/sess-1/subagents/agent-abc123.jsonl");
        let c = record_as(
            &mut w,
            "abc123",
            Some(&sub),
            &text(&[claude_line("w1", 7, Tok { inp: 1, ..NONE })]),
        );
        assert!(truthy(&c["usage"]["known"]), "{c}");
        assert_eq!(tokens(&c, &["input", "output"]), [1, 7]);
        // The supervisor's session transcript, charged by `next`.
        fs::write(
            projects.join("-some-project/sess-1.jsonl"),
            text(&[claude_line("s1", 3, Tok { inp: 2, ..NONE })]),
        )
        .unwrap();
        let rd = run_dir(&w);
        w.cli_with(&os_args!["next", rd], session_env("sess-1"))
            .ok();
        let s = one(w
            .charges()
            .into_iter()
            .filter(|c| c["key"] == "sess-1")
            .collect());
        assert_json_eq!(
            Json::from(vec![
                s["charge_id"].clone(),
                s["role"].clone(),
                s["dispatch_id"].clone()
            ]),
            jv(r#"["supervisor:sess-1:1", "supervisor", "supervisor"]"#)
        );
        assert_eq!(tokens(&s, &["input", "output"]), [2, 3]);
        // A session with no transcript is not charged.
        w.cli_with(&os_args!["next", rd], session_env("nope")).ok();
        let nope: Vec<Json> = w
            .charges()
            .into_iter()
            .filter(|c| c["key"] == "nope")
            .collect();
        assert_eq!(nope, Vec::<Json>::new());
    }

    #[test]
    fn test_wildcard_worker_id_finds_no_transcript() {
        let mut w = claude_run(None, "");
        let other = w
            .claude_home
            .join("projects/-some-project/sess-1/subagents/agent-abc123.jsonl");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, text(&[claude_line("w1", 7, NONE)])).unwrap();
        let c = record_as(&mut w, "*", None, "");
        assert!(!truthy(&c["usage"]["known"]), "{c}");
        assert_json_eq!(c["checkpoint"], jv(r#"{"messages": {}}"#));
    }

    #[test]
    fn test_relative_transcript_path_is_rejected() {
        // A relative CLAUDE_CONFIG_DIR locates the transcript by a relative path, which the
        // charge refuses (ruling S1-R13: pinned as Python behaves).
        let w = claude_run(None, "");
        let a = w.next();
        let did = a["dispatch_id"].as_str().unwrap();
        let rd = run_dir(&w);
        w.cli(&os_args![
            "record",
            rd,
            "started",
            "--dispatch",
            did,
            "--worker",
            "abc"
        ])
        .ok();
        let transcript = w.claude_home.join("projects/p/s/subagents/agent-abc.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, text(&[claude_line("m1", 5, NONE)])).unwrap();
        let reply = PathBuf::from(a["reply_file"].as_os().unwrap());
        fs::create_dir_all(reply.parent().unwrap()).unwrap();
        fs::write(
            &reply,
            format!("Status: READY\nReport: {}\n", a["report"].as_str().unwrap()),
        )
        .unwrap();
        let relative = w.claude_home.strip_prefix(&w.tmp).unwrap().to_path_buf(); // "claude home": the cli runs in w.tmp
        assert!(relative.to_str().unwrap().contains(' '), "{relative:?}");
        let r = w.cli_with(
            &os_args![
                "record",
                rd,
                "result",
                "--dispatch",
                did,
                "--worker",
                "abc",
                "--reply-file",
                reply
            ],
            CliOpts {
                env: vec![(os_string("CLAUDE_CONFIG_DIR"), os_string(&relative))],
                ..CliOpts::default()
            },
        );
        assert_eq!(r.code, 1, "{r}");
        let expected = format!(
            "error: transcript path must be absolute: {}/projects/p/s/subagents/agent-abc.jsonl",
            relative.display()
        );
        let stderr = r.stderr_text();
        assert!(stderr.lines().any(|l| l == expected), "{r}");
        let charges = w.paths().charges;
        assert!(
            !(charges.exists() && !fs::read_to_string(&charges).unwrap().trim().is_empty()),
            "a charge was appended"
        );
    }

    #[test]
    fn test_missing_projects_dir_is_none() {
        let mut w = claude_run(None, "");
        assert!(!w.claude_home.join("projects").exists());
        let c = record_as(&mut w, "abc", None, "");
        assert!(!truthy(&c["usage"]["known"]), "{c}");
        let did = c["dispatch_id"].as_str().unwrap();
        assert!(attempt_of(w.attempts(), did)["cost_estimate"].is_null());
    }
}

// --------------------------------------------------------------------------
// Prices

const PRICES_TOML: &str = "[prices.opus]\ninput = 15.0\noutput = 75.0\ncache_read = 1.5\n\
    cache_write_5m = 18.75\ncache_write_1h = 30.0\n\
    [prices.haiku]\ninput = 1.0\noutput = 5.0\ncache_read = 0.1\ncache_write_5m = 1.25\n\
    cache_write_1h = 2.0\n";
/// 1000 input and 1000 output tokens of each.
const ONE_OPUS: f64 = (1000.0 * 15.0 + 1000.0 * 75.0) / 1e6;
const ONE_HAIKU: f64 = (1000.0 * 1.0 + 1000.0 * 5.0) / 1e6;

fn model_line(mid: &str, model: &str, n: i64) -> String {
    claude_line_model(mid, n, Tok { inp: n, ..NONE }, model)
}

mod pricing {
    //! The cost estimate of a Claude attempt (attempts.jsonl) and of the run (status.json),
    //! from the prices in the run's configuration: per model, and unknown rather than low.
    use super::*;

    #[test]
    fn test_estimate_cost_opus_matches_recorded_session() {
        let mut w = claude_run(Some("claude opus high"), ""); // the presets' opus prices
        let t = text(&[claude_line(
            "m1",
            1_301_069,
            Tok {
                inp: 304_700,
                cached: 366_728_180,
                w5: 3_007_432,
                w1: 4_080_700,
            },
        )]);
        let [c] = n_of(charge_worker(&mut w, &[Some(&t)]));
        let attempt = attempt_of(w.attempts(), c["dispatch_id"].as_str().unwrap());
        assert_eq!(attempt["model"], "opus");
        let cost = attempt["cost_estimate"].as_f64().unwrap();
        assert!((cost - 148.27).abs() <= 0.01, "{cost}");
        let usd = sup_usage(&w)["estimated_usd"].as_f64().unwrap();
        assert!((usd - 148.27).abs() <= 0.01, "{usd}");
    }

    #[test]
    fn test_estimate_cost_unpriced_model_is_none() {
        let mut w = claude_run(None, ""); // claude sonnet: no sonnet price below
        common::dispatch_world::edit_json(&run_dir(&w).join("config.json"), |config| {
            let Json::Obj(prices) = at(config, &["prices"]) else {
                panic!("prices is not an object")
            };
            prices.retain(|(k, _)| k != b"sonnet");
        });
        let t = text(&[claude_line_model(
            "m1",
            1000,
            Tok { inp: 1000, ..NONE },
            "claude-sonnet-4",
        )]);
        w.script(1, "implement", [step(None, Some(&t))]);
        w.script(1, "review", [step(None, None)]);
        drive(&mut w, 2);
        let [impl_a, review] = <[String; 2]>::try_from(w.dispatch_ids())
            .unwrap()
            .map(|did| attempt_of(w.attempts(), &did));
        assert_eq!(impl_a["model"], "sonnet");
        assert!(truthy(&impl_a["usage_known"]), "{impl_a}");
        assert!(impl_a["cost_estimate"].is_null(), "{impl_a}");
        assert!(!truthy(&review["usage_known"]), "{review}"); // unknown usage: no estimate either
        assert!(review["cost_estimate"].is_null(), "{review}");
        let usage = sup_usage(&w);
        assert!(usage["estimated_usd"].is_null(), "{usage}");
        assert!(
            usage["by_role"]["implementer"]["estimated_usd"].is_null(),
            "{usage}"
        );
    }

    #[test]
    fn test_mixed_batch_is_priced_per_model() {
        let mut w = claude_run(Some("claude opus high"), PRICES_TOML);
        let implement = text(&[
            model_line("m0", "claude-opus-4", 1000),
            model_line("m1", "claude-haiku-4", 1000),
            model_line("m2", "claude-haiku-4", 1000),
        ]);
        // The order does not matter.
        let review = text(&[
            model_line("r0", "claude-haiku-4", 1000),
            model_line("r1", "claude-opus-4", 1000),
            model_line("r2", "claude-haiku-4", 1000),
        ]);
        w.script(1, "implement", [step(None, Some(&implement))]);
        w.script(1, "review", [step(None, Some(&review))]);
        drive(&mut w, 2);
        let expected = ONE_OPUS + 2.0 * ONE_HAIKU;
        for did in w.dispatch_ids() {
            let attempt = attempt_of(w.attempts(), &did);
            assert_eq!(
                tokens(&attempt, &["input", "output"]),
                [3000, 3000],
                "{did}"
            );
            // The route's model (opus for the implementer) does not price the haiku parts.
            assert!(almost(&attempt["cost_estimate"], expected), "{attempt}");
        }
        let first = w.dispatch_ids().remove(0);
        assert_eq!(attempt_of(w.attempts(), &first)["model"], "opus");
    }

    #[test]
    fn test_single_model_and_empty_synthetic_records_stay_single() {
        let mut w = claude_run(Some("claude opus high"), PRICES_TOML);
        let t = text(&[
            model_line("m0", "claude-opus-4", 1000),
            claude_line_model("m1", 0, NONE, "<synthetic>"),
            model_line("m2", "claude-opus-4", 1000),
        ]);
        let [c] = n_of(charge_worker(&mut w, &[Some(&t)]));
        assert_eq!(c["usage"]["model"], "claude-opus-4");
        assert!(c["usage"].get("by_model").is_none(), "{c}");
        let did = c["dispatch_id"].as_str().unwrap();
        assert!(almost(
            &attempt_of(w.attempts(), did)["cost_estimate"],
            2.0 * ONE_OPUS
        ));
        assert!(almost(&sup_usage(&w)["estimated_usd"], 2.0 * ONE_OPUS));
    }

    #[test]
    fn test_unpriced_part_makes_the_estimate_unknown() {
        let mut w = claude_run(Some("claude opus high"), PRICES_TOML);
        let t = text(&[
            model_line("m0", "claude-opus-4", 1000),
            model_line("m1", "some-other-model", 10),
        ]);
        let [c] = n_of(charge_worker(&mut w, &[Some(&t)]));
        let attempt = attempt_of(w.attempts(), c["dispatch_id"].as_str().unwrap());
        assert!(truthy(&attempt["usage_known"]), "{attempt}");
        assert!(attempt["cost_estimate"].is_null(), "{attempt}");
        assert!(sup_usage(&w)["estimated_usd"].is_null());
    }

    #[test]
    fn test_charge_keeps_the_per_model_parts() {
        let mut w = claude_run(Some("claude opus high"), PRICES_TOML);
        let lines: Vec<String> = ["claude-opus-4", "claude-haiku-4", "claude-haiku-4"]
            .iter()
            .enumerate()
            .map(|(i, model)| model_line(&format!("m{i}"), model, 1000))
            .collect();
        let t = text(&lines);
        let [c] = n_of(charge_worker(&mut w, &[Some(&t)]));
        assert!(c["usage"]["model"].is_null(), "{c}");
        let parts: Json = Json::Obj(
            c["usage"]["by_model"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(m, t)| {
                    (
                        m.clone(),
                        Json::Arr(vec![t["input"].clone(), t["output"].clone()]),
                    )
                })
                .collect(),
        );
        assert_json_eq!(
            parts,
            jv(r#"{"claude-opus-4": [1000, 1000], "claude-haiku-4": [2000, 2000]}"#)
        );
        assert!(almost(
            &sup_usage(&w)["estimated_usd"],
            ONE_OPUS + 2.0 * ONE_HAIKU
        ));
    }
}

// --------------------------------------------------------------------------
// Codex

mod codex_charges {
    //! charges.jsonl after `dispatch`: the session file first, then the `--json` stream, else
    //! unknown.
    use super::*;
    use common::cli_with;

    #[test]
    fn test_codex_cumulative_takes_last_token_count_and_maps_fields() {
        let mut w = World::new();
        append(
            &session_file(&w, THREAD),
            &[
                r#"{"type": "session_meta"}"#.to_string(),
                codex_line(100, 40, 10, 3),
                r#"{"type": "event_msg", "payload": {"type": "token_count", "info": null}}"#
                    .to_string(),
                codex_line(300, 100, 25, 8),
                r#"{"type": "event_msg", "payload": {"type": "agent_message"}}"#.to_string(),
            ],
        );
        let c = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        // input_tokens includes the cached tokens, so uncached input = 300 - 100
        assert_eq!(
            tokens(&c, &["input", "cached", "output", "reasoning"]),
            [200, 100, 25, 8]
        );
        assert!(truthy(&c["usage"]["known"]), "{c}");
        assert_json_eq!(
            Json::from(vec![
                c["charge_id"].clone(),
                c["source"].clone(),
                c["key"].clone()
            ]),
            Json::from(&[format!("d1:codex:{THREAD}").as_str(), "codex", THREAD][..])
        );
        let attempt = attempt_of(w.attempts(), "d1");
        assert_json_eq!(attempt["usage"], c["usage"]);
        assert!(truthy(&attempt["usage_known"]), "{attempt}");
        assert!(attempt["cost_estimate"].is_null(), "{attempt}"); // Codex models are unpriced
    }

    #[test]
    fn test_codex_cumulative_unknown_when_missing_or_without_token_count() {
        let mut w = World::new();
        let missing = codex_charge(&mut w, "d1", "22222222-2222-4222-8222-222222222222", &[]);
        assert!(!truthy(&missing["usage"]["known"]), "{missing}");
        assert_eq!(missing["source"], "codex");
        assert_json_eq!(missing["checkpoint"], nothing());
        let attempt = attempt_of(w.attempts(), "d1");
        assert_json_eq!(
            Json::from(vec![
                attempt["usage"].clone(),
                attempt["usage_known"].clone(),
                attempt["cost_estimate"].clone()
            ]),
            jv("[null, false, null]")
        );
        append(&session_file(&w, THREAD), &[r#"{"type": "session_meta"}"#]);
        assert!(!truthy(
            &codex_charge(&mut w, "d2", THREAD, &[])["usage"]["known"]
        ));
        assert_eq!(world_usage(&w)["unknown_attempts"], 2);
    }

    #[test]
    fn test_charge_none_thread_is_unknown() {
        let mut w = World::new();
        // the stream never names its thread
        let c = codex_charge_steps(&mut w, "d1", THREAD, codex_steps(&[], false, false));
        assert_json_eq!(
            Json::from(vec![c["charge_id"].clone(), c["key"].clone()]),
            jv(r#"["d1:codex:unknown", "dispatch:d1"]"#)
        );
        assert!(!truthy(&c["usage"]["known"]), "{c}");
    }

    #[test]
    fn test_charge_codex_cumulative_deltas() {
        let mut w = World::new();
        let f = session_file(&w, THREAD);
        append(&f, &[codex_line(100, 0, 0, 0)]);
        let c1 = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        append(&f, &[codex_line(250, 0, 0, 0)]);
        let c2 = codex_charge(&mut w, "d2", THREAD, &[stream_turn()]);
        assert_eq!(
            [tokens(&c1, &["input"])[0], tokens(&c2, &["input"])[0]],
            [100, 150]
        );
        assert_eq!(c2["checkpoint"]["cumulative"]["input"], 250);
        // interrupted: no new tokens -> zero delta
        let c3 = codex_charge(&mut w, "d3", THREAD, &[stream_turn()]);
        assert_eq!(c3["usage"]["input"], 0);
        assert!(truthy(&c3["usage"]["known"]), "{c3}");
        assert_eq!(summed(&[&c1, &c2, &c3])[0], 250);
    }

    #[test]
    fn test_codex_stream_fallback_charged_when_session_missing() {
        // Covers R22.
        let mut w = World::new();
        let c = codex_charge(
            &mut w,
            "d1",
            "99999999-9999-4999-8999-999999999999",
            &[turn_completed(100, 30, 25, 8)],
        );
        assert_eq!(c["source"], "codex-stream");
        assert_eq!(
            tokens(&c, &["input", "cached", "output", "reasoning"]),
            [70, 30, 25, 8]
        );
        assert!(truthy(&c["usage"]["known"]), "{c}");
        assert_json_eq!(attempt_of(w.attempts(), "d1")["usage"], c["usage"]);
        assert_eq!(world_usage(&w)["unknown_attempts"], 0);
        // A stream charge moves no Codex checkpoint: a later session file is charged from its
        // records.
        assert_json_eq!(c["checkpoint"], nothing());
        // No completed turn either: an unknown Codex charge.
        let u = codex_charge(&mut w, "d2", "88888888-8888-4888-8888-888888888888", &[]);
        assert_json_eq!(
            Json::from(vec![u["source"].clone(), u["usage"]["known"].clone()]),
            jv(r#"["codex", false]"#)
        );
        // With a session file the stream is ignored.
        append(&session_file(&w, THREAD), &[codex_line(100, 0, 0, 0)]);
        let s = codex_charge(&mut w, "d3", THREAD, &[turn_completed(100, 30, 25, 8)]);
        assert_json_eq!(
            Json::from(vec![s["source"].clone(), s["usage"]["input"].clone()]),
            jv(r#"["codex", 100]"#)
        );
    }

    #[test]
    fn test_codex_stream_usage_sums_turn_completed() {
        let mut w = World::new();
        let c = codex_charge(
            &mut w,
            "d1",
            THREAD,
            &[
                turn_completed(100, 30, 10, 4),
                json!({"type": "item.completed"}),
                turn_completed(50, 10, 5, 1),
            ],
        );
        assert_eq!(c["source"], "codex-stream");
        assert_eq!(
            tokens(&c, &["input", "cached", "output", "reasoning"]),
            [110, 40, 15, 5]
        );
        assert!(truthy(&c["usage"]["known"]), "{c}");
    }

    #[test]
    fn test_runner_crash_after_charge_keeps_the_charge() {
        // The runner dies between its charge and its attempt record; the session file grows
        // before `dispatch` runs again. The finished generation's charge is the one already
        // made, and the next generation charges only what is new.
        let mut w = World::new();
        let lines = fixture_lines(ROLLOUT);
        let f = place_rollout(&w, THREAD, &lines[..18]);
        w.scenario_steps(
            &codex_steps(&[stream_turn()], true, false),
            &[("thread", THREAD)],
        );
        let args = w.args("d1");
        let crashed = cli_with(
            &os_refs(&args),
            &w.env
                .with("LEADHAND_CRASH_AT", "after_charge_before_attempt"),
            &w.tmp,
            None,
            common::TIMEOUT,
        );
        assert_eq!(crashed.code, 1, "{crashed}");
        let first = one(w.charges());
        assert_eq!(w.attempts(), Vec::<Json>::new());
        fs::write(&f, text(&lines)).unwrap();
        w.dispatch("d1").ok();
        let [again, resumed] = n_of(w.charges());
        assert_json_eq!(again, first);
        assert_eq!(
            tokens(&first, &["input", "cached", "output"]),
            [19062 - 7168, 7168, 58]
        );
        assert_eq!(
            resumed["charge_id"],
            format!("d1:codex:{THREAD}:g2").as_str()
        );
        assert_eq!(
            tokens(&resumed, &["input", "cached", "output"]),
            [20812 - 18944, 18944, 6]
        );
        let ids: Vec<Json> = w
            .attempts()
            .iter()
            .map(|a| a["charge_ids"].clone())
            .collect();
        assert_json_eq!(
            Json::Arr(ids),
            Json::Arr(vec![
                Json::Arr(vec![first["charge_id"].clone()]),
                Json::Arr(vec![resumed["charge_id"].clone()]),
            ])
        );
        assert_eq!(
            summed(&[&first, &resumed]),
            [39874 - 26112, 26112, 0, 0, 64]
        );
    }
}

mod codex_rollout {
    //! The real (sanitized) post-resume rollout: a killed turn (response 1, 19120 tokens) then
    //! a resumed turn (response 2, 20818). Its token_count total is 20818 and omits the killed
    //! turn; the token_usage_records sum to 39938.
    use super::*;

    #[test]
    fn test_rollout_charges_every_response_not_the_token_count_total() {
        // Covers P12.
        let mut w = World::new();
        place_rollout(&w, THREAD, &fixture_lines(ROLLOUT));
        let c = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        assert!(truthy(&c["usage"]["known"]), "{c}");
        // 39874 input of which 26112 cached; 64 output: 39938 tokens, not 20818
        assert_eq!(
            tokens(&c, &["input", "cached", "output", "reasoning"]),
            [39874 - 26112, 26112, 64, 0]
        );
        assert_eq!(
            tokens(&c, &["input", "cached", "output"])
                .iter()
                .sum::<i64>(),
            39938
        );
        assert_json_eq!(
            c["checkpoint"]["responses"],
            jv(r#"["resp_fixture_1", "resp_fixture_2"]"#)
        );
        assert!(c.get("mismatch").is_none(), "{c}");
        assert!(c.get("possibly_incomplete").is_none(), "{c}");
    }

    #[test]
    fn test_rollout_charged_twice_charges_nothing_new() {
        let mut w = World::new();
        place_rollout(&w, THREAD, &fixture_lines(ROLLOUT));
        let first = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        let second = codex_charge(&mut w, "d2", THREAD, &[stream_turn()]);
        assert!(truthy(&second["usage"]["known"]), "{second}");
        assert_eq!(tokens(&second, &["input", "cached", "output"]), [0, 0, 0]);
        assert_eq!(summed(&[&first, &second]), summed(&[&first]));
    }

    #[test]
    fn test_rollout_truncated_before_resume_charges_only_the_killed_turn() {
        let mut w = World::new();
        let lines = fixture_lines(ROLLOUT);
        place_rollout(&w, THREAD, &lines[..18]); // the killed turn: no token_count yet
        let killed = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        assert!(truthy(&killed["usage"]["known"]), "{killed}");
        assert_eq!(
            tokens(&killed, &["input", "cached", "output"]),
            [19062 - 7168, 7168, 58]
        );
        assert_json_eq!(
            killed["checkpoint"]["responses"],
            jv(r#"["resp_fixture_1"]"#)
        );
        // the resume appends to the same file: only the resumed turn is new
        place_rollout(&w, THREAD, &lines);
        let resumed = codex_charge(&mut w, "d2", THREAD, &[stream_turn()]);
        assert_eq!(
            tokens(&resumed, &["input", "cached", "output"]),
            [20812 - 18944, 18944, 6]
        );
        let total = summed(&[&killed, &resumed]);
        assert_eq!([total[0], total[1], total[4]], [39874 - 26112, 26112, 64]);
    }

    #[test]
    fn test_rollout_repeated_response_id_counts_once() {
        let mut w = World::new();
        let mut lines = fixture_lines(ROLLOUT);
        lines.push(lines[16].clone()); // the first token_usage_record, repeated
        place_rollout(&w, THREAD, &lines);
        let c = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        assert_eq!(tokens(&c, &["cached", "output"]), [26112, 64]);
    }

    #[test]
    fn test_rollout_thread_total_mismatch_is_recorded_not_fatal() {
        let mut w = World::new();
        let mut lines = fixture_lines(ROLLOUT);
        let mut rec = jv(&lines[27]);
        *at(
            &mut rec,
            &["payload", "thread_token_usage", "output_tokens"],
        ) = 1000.into();
        lines[27] = json::dumps(&rec);
        place_rollout(&w, THREAD, &lines);
        let c = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        assert_eq!(c["usage"]["output"], 64); // still charged from the records
        assert_eq!(c["mismatch"]["thread_token_usage"]["output"], 1000);
        assert_eq!(c["mismatch"]["records"]["output"], 64);
        assert_eq!(attempt_of(w.attempts(), "d1")["status"], "READY");
    }

    #[test]
    fn test_possibly_incomplete_flag_is_recorded_on_the_charge() {
        let mut w = World::new();
        place_rollout(&w, THREAD, &fixture_lines(ROLLOUT)[..18]);
        // it ends inside a turn
        let c = codex_charge_steps(&mut w, "d1", THREAD, codex_steps(&[], true, true));
        assert_eq!(c["possibly_incomplete"], Json::Bool(true));
        assert!(truthy(&c["usage"]["known"]), "{c}");
        assert_eq!(
            attempt_of(w.attempts(), "d1")["possibly_incomplete"],
            Json::Bool(true)
        );
    }

    #[test]
    fn test_rollout_without_any_usable_record_is_unknown() {
        let mut w = World::new();
        place_rollout(&w, THREAD, &fixture_lines(ROLLOUT)[..3]);
        assert!(!truthy(
            &codex_charge(&mut w, "d1", THREAD, &[])["usage"]["known"]
        ));
    }

    #[test]
    fn test_token_count_is_only_a_fallback_for_a_rollout_without_records() {
        let mut w = World::new();
        let lines: Vec<String> = fixture_lines(ROLLOUT)
            .into_iter()
            .filter(|l| !l.contains("\"token_usage_record\""))
            .collect();
        place_rollout(&w, THREAD, &lines);
        let c = codex_charge(&mut w, "d1", THREAD, &[stream_turn()]);
        assert_eq!(
            tokens(&c, &["input", "cached", "output"]),
            [20812 - 18944, 18944, 6]
        );
    }
}

mod codex_stream {
    //! Usage from the `--json` stream (a session file is missing): turn.completed summed.
    use super::*;

    #[test]
    fn test_stream_usage_sums_turn_completed() {
        let mut w = World::new();
        let steps: Vec<Value> = fixture_lines("codex-exec-stream.jsonl")
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        w.scenario_steps(&steps, &[]);
        w.dispatch("d1").ok();
        let c = one(w.charges());
        assert_eq!(c["source"], "codex-stream");
        assert!(truthy(&c["usage"]["known"]), "{c}");
        assert_eq!(
            tokens(
                &c,
                &["input", "cached", "output", "reasoning", "cache_write_5m"]
            ),
            [137911 - 128640, 128640, 368, 0, 0]
        );
    }

    #[test]
    fn test_stream_cache_write_tokens_are_kept_as_five_minute_writes() {
        let mut w = World::new();
        let mut turn = turn_completed(100, 10, 3, 1);
        turn["usage"]["cache_write_input_tokens"] = json!(5);
        let c = codex_charge(&mut w, "d1", THREAD, &[turn]);
        assert_eq!(all_tokens(&c), [90, 10, 5, 0, 3]);
    }
}

// --------------------------------------------------------------------------
// Concurrency

mod concurrency {
    use super::*;
    use common::dispatch_world::{Req, args_in};
    use common::read_jsonl;
    use common::supervisor::PROJECT;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn test_concurrent_supervisor_and_runner_charges_keep_exact_totals() {
        // Two writers at once, each charging the supervisor's session (`next`) and a Codex
        // thread (`dispatch`): A adds the session's messages, B the thread's usage.
        const N: i64 = 12;
        let mut w = World::new();
        let sid = "concurrent-session";
        let session = w
            .sup
            .claude_home
            .join("projects")
            .join(PROJECT)
            .join(format!("{sid}.jsonl"));
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        append(&session, &[claude_line("seed", 3, Tok { inp: 1, ..NONE })]);
        let rollout = session_file(&w, THREAD);
        append(&rollout, &[codex_line(100, 20, 5, 0)]);
        w.scenario_steps(&codex_steps(&[], true, false), &[("thread", THREAD)]);
        let env = w.env.with("CLAUDE_CODE_SESSION_ID", sid);
        let (run_dir, ws, tmp) = (w.run_dir.clone(), w.ws.clone(), w.tmp.clone());

        let charge_session = {
            let (run_dir, env, tmp) = (run_dir.clone(), env.clone(), tmp.clone());
            move || {
                cli(&os_args!["next", run_dir], &env, &tmp).ok();
            }
        };
        let charge_thread = {
            let (run_dir, ws, env, tmp) = (run_dir.clone(), ws.clone(), env.clone(), tmp.clone());
            move |did: &str| {
                let args = args_in(&run_dir, &ws, did, &Req::default());
                cli(&os_refs(&args), &env, &tmp).ok();
            }
        };
        let writer = {
            let (session, rollout) = (session.clone(), rollout.clone());
            let (charge_session, charge_thread) = (charge_session.clone(), charge_thread.clone());
            move |who: &str| {
                for i in 0..N {
                    if who == "A" {
                        append(
                            &session,
                            &[claude_line(
                                &format!("A-{i}"),
                                i + 1,
                                Tok {
                                    inp: 2,
                                    cached: 7,
                                    ..NONE
                                },
                            )],
                        );
                        charge_session();
                        charge_thread(&format!("A{i}"));
                    } else {
                        append(
                            &rollout,
                            &[codex_line(100 + 10 * (i + 1), 20, 5 * (i + 2), 0)],
                        );
                        charge_thread(&format!("B{i}"));
                        charge_session();
                    }
                }
            }
        };
        let threads: Vec<thread::JoinHandle<()>> = ["A", "B"]
            .into_iter()
            .map(|who| {
                let writer = writer.clone();
                thread::spawn(move || writer(who))
            })
            .collect();
        let end = Instant::now() + Duration::from_secs(300);
        while !threads.iter().all(|t| t.is_finished()) && Instant::now() < end {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            threads.iter().all(|t| t.is_finished()),
            "a writer did not finish"
        );
        for t in threads {
            if let Err(failure) = t.join() {
                std::panic::resume_unwind(failure); // reported by the test thread
            }
        }
        // the final charges pick up whatever the last writer added after the last charge
        charge_session();
        charge_thread("final");

        let charges = w.charges();
        let ids: Vec<String> = charges
            .iter()
            .map(|c| c["charge_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), ids.iter().collect::<BTreeSet<_>>().len());
        let codex: Vec<&Json> = charges.iter().filter(|c| c["source"] == "codex").collect();
        let claude: Vec<&Json> = charges.iter().filter(|c| c["source"] == "claude").collect();
        assert_eq!(codex.len() as i64, 2 * N + 1);
        assert_eq!(claude.len() + codex.len(), charges.len());
        assert!(
            claude
                .iter()
                .all(|c| c["key"] == sid && c["role"] == "supervisor"),
            "{claude:?}"
        );
        let (claude_total, codex_total) = (summed(&claude), summed(&codex));
        // Claude: seed + A's n messages (output i+1, input 2, cached 7)
        assert_eq!(claude_total[4], 3 + (0..N).map(|i| i + 1).sum::<i64>());
        assert_eq!(claude_total[0], 1 + 2 * N);
        assert_eq!(claude_total[1], 7 * N);
        // Codex: B's last cumulative is (100 + 10n, 20 cached, 5(n+1) output)
        assert_eq!(codex_total[0], 100 + 10 * N - 20);
        assert_eq!(codex_total[1], 20);
        assert_eq!(codex_total[4], 5 * (N + 1));
        // the lock-protected file still parses cleanly line by line
        let raw = fs::read_to_string(w.run_dir.join("charges.jsonl")).unwrap();
        assert!(raw.ends_with('\n'));
        assert_eq!(
            raw.lines().count(),
            read_jsonl(&w.run_dir.join("charges.jsonl")).len()
        );
        // and so does events.jsonl, its seqs 1..N in file order: one counter, under the lock
        let raw = fs::read_to_string(w.run_dir.join("events.jsonl")).unwrap();
        assert!(raw.ends_with('\n'));
        let seqs: Vec<i64> = read_jsonl(&w.run_dir.join("events.jsonl"))
            .iter()
            .map(|e| e["seq"].as_i64().unwrap())
            .collect();
        assert_eq!(raw.lines().count(), seqs.len());
        assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>());
    }
}
