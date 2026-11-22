//! A run directory's layout, and a synthetic one for status and report tests: the Rust form
//! of `tests/contract/rundir.py`.
//!
//! `RunDir` is written from the file formats, not from the leadhand library: run.json,
//! deck.md, and whatever events, attempts and charges a test appends. No engine, Git or
//! workers. `env` is the environment to run `status` and `report` in; Codex rate limits
//! written by `rate_limit` land in `sessions`, which is $CODEX_HOME/sessions there.
//!
//! Python's keyword arguments are option structs with `Default` (`DispatchOpts`,
//! `AttemptOpts`, `ChargeOpts`), and its `**fields` are `(name, Json)` pairs.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};

use super::json::{self, Json};
use super::{Env, TempRoot, read_jsonl};

/// The files of a run directory, named as the CLI names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPaths {
    pub root: PathBuf,
    pub deck: PathBuf,
    pub run_json: PathBuf,
    pub events: PathBuf,
    pub status: PathBuf,
    pub attempts: PathBuf,
    pub charges: PathBuf,
    pub state_lock: PathBuf,
    pub report: PathBuf,
    pub handoff: PathBuf,
    pub lock: PathBuf,
    pub holder: PathBuf,
    pub pause_flag: PathBuf,
    pub cancel_flag: PathBuf,
    pub cards_dir: PathBuf,
    pub workers_dir: PathBuf,
}

/// The files of the run directory `root`, named as the CLI names them.
pub fn run_paths(root: impl AsRef<Path>) -> RunPaths {
    let root = root.as_ref().to_path_buf();
    let j = |name: &str| root.join(name);
    RunPaths {
        deck: j("deck.md"),
        run_json: j("run.json"),
        events: j("events.jsonl"),
        status: j("status.json"),
        attempts: j("attempts.jsonl"),
        charges: j("charges.jsonl"),
        state_lock: j("state.lock"),
        report: j("report.md"),
        handoff: j("handoff.md"),
        lock: j("run.lock"),
        holder: j("holder.json"),
        pause_flag: j("pause"),
        cancel_flag: j("cancel"),
        cards_dir: j("cards"),
        workers_dir: j("workers"),
        root,
    }
}

/// A JSON-lines file's complete records; none when the file does not exist yet.
#[track_caller]
pub fn read_records(path: &Path) -> Vec<Json> {
    if path.exists() {
        read_jsonl(path)
    } else {
        Vec::new()
    }
}

/// The time now as the CLI writes it: UTC, to the millisecond, `2026-01-01T00:00:00.000Z`.
pub fn utc_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    let (secs, millis) = (now.as_secs() as i64, now.subsec_millis());
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The seconds since the epoch of an ISO 8601 time with a zone, as Python's
/// `datetime.fromisoformat(ts).timestamp()`: `2026-01-01T00:00:00Z`, with optional fractional
/// seconds and `Z` or `±HH:MM`. Panics on anything else (a time without a zone, which Python
/// would read as local time, included).
#[track_caller]
pub fn epoch_seconds(ts: &str) -> f64 {
    try_epoch_seconds(ts).unwrap_or_else(|| panic!("not an ISO 8601 time with a zone: {ts:?}"))
}

/// `epoch_seconds`, None where it panics (where Python's `fromisoformat` raises ValueError,
/// and for a time without a zone).
pub fn try_epoch_seconds(ts: &str) -> Option<f64> {
    let num = |s: &str| -> Option<i64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let (date, rest) = ts.split_once('T')?;
    let (clock, offset) = if let Some(c) = rest.strip_suffix('Z') {
        (c, 0)
    } else if rest.len() > 6 && matches!(rest.as_bytes()[rest.len() - 6], b'+' | b'-') {
        let (c, z) = rest.split_at(rest.len() - 6);
        let (h, m) = z[1..].split_once(':')?;
        let minutes = num(h)? * 60 + num(m)?;
        (
            c,
            if z.starts_with('-') {
                -minutes
            } else {
                minutes
            } * 60,
        )
    } else {
        return None;
    };
    let d: Vec<i64> = date.split('-').map(num).collect::<Option<_>>()?;
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let t: Vec<i64> = hms.split(':').map(num).collect::<Option<_>>()?;
    let ([year, month, day], [h, m, s]) =
        (<[i64; 3]>::try_from(d).ok()?, <[i64; 3]>::try_from(t).ok()?);
    let frac = if frac.is_empty() {
        0.0
    } else {
        num(frac)? as f64 / 10f64.powi(frac.len() as i32)
    };
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + m * 60 + s - offset) as f64 + frac)
}

/// Appends one record as one line under `state_lock`, as the CLI does; `build` gets the
/// file's last record (or None). Returns the record.
#[track_caller]
pub fn append_record(
    state_lock: &Path,
    file: &Path,
    build: impl FnOnce(Option<&Json>) -> Json,
) -> Json {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(state_lock)
        .unwrap_or_else(|e| panic!("opening {}: {e}", state_lock.display()));
    flock(&lock, FlockOperation::LockExclusive)
        .unwrap_or_else(|e| panic!("locking {}: {e}", state_lock.display()));
    let records = read_records(file);
    let record = build(records.last());
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(file)
        .unwrap_or_else(|e| panic!("opening {}: {e}", file.display()));
    f.write_all(format!("{}\n", json::dumps_compact(&record)).as_bytes())
        .unwrap_or_else(|e| panic!("writing {}: {e}", file.display()));
    drop(lock); // closing it releases the lock
    record
}

/// Appends event `kind` with `fields` to run directory `run_dir`'s events.jsonl, with the
/// next seq. Returns the event.
#[track_caller]
pub fn append_event<'a>(
    run_dir: &Path,
    kind: &str,
    fields: impl IntoIterator<Item = (&'a str, Json)>,
) -> Json {
    let paths = run_paths(run_dir);
    let fields: Vec<(&str, Json)> = fields.into_iter().collect();
    append_record(&paths.state_lock, &paths.events, |last| {
        let seq = match last {
            None => 1,
            Some(last) => last["seq"].as_i64().expect("an integer seq") + 1,
        };
        let mut pairs = vec![
            ("seq", Json::from(seq)),
            ("ts", Json::from(utc_now())),
            ("kind", Json::from(kind)),
        ];
        pairs.extend(fields);
        Json::object(pairs)
    })
}

/// An `outcome` event's fields: `ev("outcome", outcome=…, qualifier=…, reasons=[…])`.
pub fn outcome(outcome: &str, qualifier: &str, reasons: &[&str]) -> [(&'static str, Json); 3] {
    [
        ("outcome", Json::from(outcome)),
        ("qualifier", Json::from(qualifier)),
        ("reasons", Json::from(reasons)),
    ]
}

pub const DECK: &str = "# Deck: Demo deck
Source: docs/plan.md
Base: main @ 0123456789abcdef0123456789abcdef01234567
Branch: leadhand/demo
Build: true
Test: true

";

/// Card `n` of `DECK`, depending on nothing.
pub fn deck_card(n: i64) -> String {
    deck_card_with(n, "none")
}

/// Card `n` of `DECK`, with its `Depends on:` line.
pub fn deck_card_with(n: i64, depends: &str) -> String {
    format!(
        "## Card {n}: Card number {n}\n- Role: implementer\n- Hard: no\n- Risk: low\n\
         - Depends on: {depends}\n- Allowed paths: src/\n- Done when: it works\n- Stop if: never\n"
    )
}

/// `RunDir.dispatch`'s keyword arguments.
#[derive(Clone, Debug)]
pub struct DispatchOpts<'a> {
    pub kind: &'a str,
    pub attempt: i64,
    pub vendor: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub role: &'a str,
    /// The dispatch id; None: `c<n>-a<attempt>-<kind>-1`.
    pub did: Option<&'a str>,
    pub start: bool,
}

impl Default for DispatchOpts<'_> {
    fn default() -> Self {
        DispatchOpts {
            kind: "implement",
            attempt: 1,
            vendor: "codex",
            model: "gpt-6.1-terra",
            effort: "high",
            role: "implementer",
            did: None,
            start: true,
        }
    }
}

/// `RunDir.attempt`'s keyword arguments; `extra` is its `**extra`.
#[derive(Clone, Debug)]
pub struct AttemptOpts<'a> {
    pub role: &'a str,
    pub vendor: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub attempt: i64,
    pub status: &'a str,
    pub usage_known: bool,
    pub extra: Vec<(&'a str, Json)>,
}

impl Default for AttemptOpts<'_> {
    fn default() -> Self {
        AttemptOpts {
            role: "implementer",
            vendor: "codex",
            model: "gpt-6.1-terra",
            effort: "high",
            attempt: 1,
            status: "READY",
            usage_known: true,
            extra: Vec::new(),
        }
    }
}

/// `RunDir.charge`'s keyword arguments.
#[derive(Clone, Debug)]
pub struct ChargeOpts<'a> {
    /// (input, output) tokens.
    pub tokens: (i64, i64),
    pub known: bool,
    pub source: &'a str,
    pub model: &'a str,
    pub possibly_incomplete: bool,
    pub dispatch: &'a str,
}

impl Default for ChargeOpts<'_> {
    fn default() -> Self {
        ChargeOpts {
            tokens: (100, 10),
            known: true,
            source: "claude",
            model: "claude-sonnet-4-5",
            possibly_incomplete: false,
            dispatch: "d1",
        }
    }
}

/// The default timestamp and rollout file name of a rate-limit record.
pub const RATE_LIMIT_TS: &str = "2026-01-02T03:04:05.000Z";
pub const RATE_LIMIT_NAME: &str = "rollout-2026-01-02-abc.jsonl";

/// A synthetic run directory: `<tmp>/run dir`, with run.json, a deck of `cards` cards and a
/// `run_started` event. The temp root is removed when it is dropped.
pub struct RunDir {
    pub root: PathBuf,
    pub env: Env,
    pub paths: RunPaths,
    pub sessions: PathBuf,
    _tmp: TempRoot,
}

impl RunDir {
    /// A run directory with five cards (`RunDir(test)`).
    #[track_caller]
    pub fn new() -> RunDir {
        RunDir::with_cards(5)
    }

    /// A run directory with `cards` cards (`RunDir(test, cards=n)`).
    #[track_caller]
    pub fn with_cards(cards: i64) -> RunDir {
        let tmp = TempRoot::new("lh status ");
        let root = tmp.join("run dir");
        fs::create_dir(&root).unwrap_or_else(|e| panic!("making {}: {e}", root.display()));
        let env = Env::new(&tmp);
        let paths = run_paths(&root);
        let deck: Vec<String> = (1..=cards).map(deck_card).collect();
        fs::write(&paths.deck, format!("{DECK}{}", deck.join("\n")))
            .unwrap_or_else(|e| panic!("writing {}: {e}", paths.deck.display()));
        let text = |p: PathBuf| Json::from(p.to_str().expect("a UTF-8 temp root"));
        let meta = Json::object([
            ("run_id", Json::from("abc123")),
            ("repo", text(tmp.join("repo"))),
            ("deck_source", Json::from("deck.md")),
            ("deck_digest", Json::from("x")),
            ("base_ref", Json::from("main")),
            (
                "base_sha",
                Json::from("0123456789abcdef0123456789abcdef01234567"),
            ),
            ("branch", Json::from("leadhand/demo-2")),
            ("branch_requested", Json::from("leadhand/demo")),
            ("workspace", text(tmp.join("work space"))),
            ("slug", Json::from("demo-deck")),
            ("created_at", Json::from("2026-01-01T00:00:00.000Z")),
            ("supervisor_sessions", Json::Arr(Vec::new())),
        ]);
        fs::write(
            &paths.run_json,
            format!("{}\n", json::dumps_indent(&meta, 2)),
        )
        .unwrap_or_else(|e| panic!("writing {}: {e}", paths.run_json.display()));
        let sessions = PathBuf::from(env.get("CODEX_HOME").unwrap()).join("sessions");
        fs::create_dir_all(&sessions)
            .unwrap_or_else(|e| panic!("making {}: {e}", sessions.display()));
        let r = RunDir {
            root,
            env,
            paths,
            sessions,
            _tmp: tmp,
        };
        r.ev(
            "run_started",
            [
                ("run_id", Json::from("abc123")),
                ("base_sha", Json::from("0123456789abcdef")),
                ("branch", Json::from("leadhand/demo-2")),
            ],
        );
        r
    }

    /// Appends one record to `file` under the run's state lock (`_append`).
    #[track_caller]
    pub fn append(&self, file: &Path, build: impl FnOnce(Option<&Json>) -> Json) -> Json {
        append_record(&self.paths.state_lock, file, build)
    }

    /// Appends event `kind` with `fields`.
    #[track_caller]
    pub fn ev<'a>(&self, kind: &str, fields: impl IntoIterator<Item = (&'a str, Json)>) -> Json {
        append_event(&self.root, kind, fields)
    }

    /// Card `n` cut and accepted at the default sha, `"a" * 40`.
    #[track_caller]
    pub fn accept(&self, n: i64) {
        self.accept_with(n, &"a".repeat(40));
    }

    /// Card `n` cut and accepted at `sha`.
    #[track_caller]
    pub fn accept_with(&self, n: i64, sha: &str) {
        self.ev(
            "card_cut",
            [
                ("card", Json::from(n)),
                ("branch", Json::from(format!("leadhand/demo-2/c{n}"))),
                ("base", Json::from("b")),
            ],
        );
        self.ev(
            "accepted",
            [("card", Json::from(n)), ("sha", Json::from(sha))],
        );
    }

    /// A dispatch of card `n` with the default options; returns its id.
    #[track_caller]
    pub fn dispatch(&self, n: i64) -> String {
        self.dispatch_with(n, DispatchOpts::default())
    }

    /// A dispatch of card `n` (the card is cut first if it is not yet): `dispatch_requested`,
    /// then `worker_started` unless `o.start` is false. Returns its id.
    #[track_caller]
    pub fn dispatch_with(&self, n: i64, o: DispatchOpts) -> String {
        let did = match o.did {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => format!("c{n}-a{}-{}-1", o.attempt, o.kind),
        };
        let cut = read_records(&self.paths.events)
            .iter()
            .any(|e| e["kind"] == "card_cut" && json::py_eq(&e["card"], &Json::from(n)));
        if !cut {
            self.ev(
                "card_cut",
                [
                    ("card", Json::from(n)),
                    ("branch", Json::from(format!("leadhand/demo-2/c{n}"))),
                    ("base", Json::from("b")),
                ],
            );
        }
        self.ev(
            "dispatch_requested",
            [
                ("dispatch_id", Json::from(did.as_str())),
                ("card", Json::from(n)),
                ("attempt", Json::from(o.attempt)),
                ("dispatch_kind", Json::from(o.kind)),
                ("role", Json::from(o.role)),
                ("vendor", Json::from(o.vendor)),
                ("model", Json::from(o.model)),
                ("effort", Json::from(o.effort)),
            ],
        );
        if o.start {
            self.ev(
                "worker_started",
                [
                    ("dispatch_id", Json::from(did.as_str())),
                    ("worker_id", Json::from(format!("w-{did}"))),
                ],
            );
        }
        did
    }

    /// An attempt record for dispatch `did` of card `n`, with the default options.
    #[track_caller]
    pub fn attempt(&self, did: &str, n: i64) {
        self.attempt_with(did, n, AttemptOpts::default());
    }

    /// An attempt record for dispatch `did` of card `n`.
    #[track_caller]
    pub fn attempt_with(&self, did: &str, n: i64, o: AttemptOpts) {
        let mut pairs = vec![
            ("dispatch", Json::from(did)),
            ("run", Json::from("abc123")),
            ("card", Json::from(n)),
            ("attempt", Json::from(o.attempt)),
            ("role", Json::from(o.role)),
            ("vendor", Json::from(o.vendor)),
            ("model", Json::from(o.model)),
            ("effort", Json::from(o.effort)),
            ("status", Json::from(o.status)),
            ("usage_known", Json::from(o.usage_known)),
            ("cost_estimate", Json::Null),
        ];
        pairs.extend(o.extra);
        let record = Json::object(pairs);
        self.append(&self.paths.attempts, |_last| record);
    }

    /// A charge `cid` under `role`, with the default options.
    #[track_caller]
    pub fn charge(&self, cid: &str, role: &str) {
        self.charge_with(cid, role, ChargeOpts::default());
    }

    /// A charge `cid` under `role`.
    #[track_caller]
    pub fn charge_with(&self, cid: &str, role: &str, o: ChargeOpts) {
        let usage = Json::object([
            ("input", Json::from(o.tokens.0)),
            ("cached", Json::from(0)),
            ("cache_write_5m", Json::from(0)),
            ("cache_write_1h", Json::from(0)),
            ("output", Json::from(o.tokens.1)),
            ("reasoning", Json::from(0)),
            ("known", Json::from(o.known)),
            ("model", Json::from(o.model)),
        ]);
        let mut pairs = vec![
            ("charge_id", Json::from(cid)),
            ("source", Json::from(o.source)),
            ("key", Json::from(cid)),
            ("dispatch_id", Json::from(o.dispatch)),
            ("role", Json::from(role)),
            ("usage", usage),
            ("checkpoint", Json::object([])),
            ("ts", Json::from("2026-01-01T00:00:00.000Z")),
        ];
        if o.possibly_incomplete {
            pairs.push(("possibly_incomplete", Json::from(true)));
        }
        let record = Json::object(pairs);
        self.append(&self.paths.charges, |_last| record);
    }

    /// A Codex rate-limit record at `percent` in `sessions` (`write_rate_limit`), at the
    /// default time and file.
    #[track_caller]
    pub fn rate_limit(&self, percent: impl Into<Json>) {
        self.rate_limit_with(percent, RATE_LIMIT_TS, RATE_LIMIT_NAME);
    }

    /// A Codex rate-limit record at `percent`, time `ts`, in rollout file `name`.
    #[track_caller]
    pub fn rate_limit_with(&self, percent: impl Into<Json>, ts: &str, name: &str) {
        write_rate_limit_with(&self.sessions, percent, ts, name);
    }
}

/// `write_rate_limit_with` at the default time and file.
#[track_caller]
pub fn write_rate_limit(sessions: &Path, percent: impl Into<Json>) -> PathBuf {
    write_rate_limit_with(sessions, percent, RATE_LIMIT_TS, RATE_LIMIT_NAME)
}

/// Appends a Codex rate-limit record (a weekly window at `percent`) to the rollout file
/// `name` under `sessions`/2026/01/02; returns the file.
#[track_caller]
pub fn write_rate_limit_with(
    sessions: &Path,
    percent: impl Into<Json>,
    ts: &str,
    name: &str,
) -> PathBuf {
    let d = sessions.join("2026").join("01").join("02");
    fs::create_dir_all(&d).unwrap_or_else(|e| panic!("making {}: {e}", d.display()));
    let primary = Json::object([
        ("used_percent", percent.into()),
        ("window_minutes", Json::from(10080)),
        ("resets_at", Json::from(1_767_830_400_i64)),
    ]);
    let line = Json::object([
        ("timestamp", Json::from(ts)),
        ("type", Json::from("event_msg")),
        (
            "payload",
            Json::object([
                ("type", Json::from("token_count")),
                ("info", Json::Null),
                (
                    "rate_limits",
                    Json::object([("primary", primary), ("secondary", Json::Null)]),
                ),
            ]),
        ),
    ]);
    let path = d.join(name);
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
    f.write_all(format!("{}\n", json::dumps(&line)).as_bytes())
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    path
}

/// `shutil.copytree(src, dest)`: `dest` and its parents made, files copied.
#[track_caller]
pub fn copy_tree(src: &Path, dest: &Path) {
    fs::create_dir_all(dest).unwrap_or_else(|e| panic!("making {}: {e}", dest.display()));
    for entry in fs::read_dir(src).unwrap_or_else(|e| panic!("reading {}: {e}", src.display())) {
        let entry = entry.unwrap();
        let to = dest.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), &to)
                .unwrap_or_else(|e| panic!("copying {}: {e}", entry.path().display()));
        }
    }
}

/// An unfinished run under `docs`/runs that belongs to another repository (RunDir's own,
/// which does not exist), updated after everything else there: the run the Python's old
/// choice would show (`contract_rust_status.other_repos_run`). The RunDir is returned to
/// keep its temp root for the test.
#[track_caller]
pub fn other_repos_run(docs: &Path) -> (RunDir, PathBuf) {
    let other = RunDir::with_cards(3);
    let dest = docs.join("runs").join("2099-01-01-elsewhere");
    copy_tree(&other.root, &dest);
    super::set_mtime(&dest.join("events.jsonl"), SystemTime::now());
    (other, dest)
}
