//! The dispatch runner's pure core (`dispatch.run_runner`'s watch loop, `_Tracker.stop`,
//! `_Stream`, `_classify`): `Runner::step(event, now) -> actions`, with no I/O and no clock
//! of its own. The shell (`shell.rs`) feeds it events and carries out its actions.
//!
//! While Codex runs the runner counts its steps, enforces the step and time limits, touches
//! the heartbeat and snapshots Codex's descendants. A limit, a cancel (SIGTERM or the `cancel`
//! file), a fault in the shell, or Codex's own exit starts the stop: the acknowledgement
//! (`stopping.json`), then the ladder (`Ladder`): a snapshot with the tag sweep before each
//! signal, SIGINT to Codex's group, SIGTERM to Codex's and every recorded group after
//! `SIGNAL_STEP`, then SIGKILL every `KILL_RETRY` until nothing recorded is alive. After a
//! normal exit the ladder clears what Codex left (it skips SIGINT, Codex being gone). Then the
//! shell finalizes: `classify`, the commit, the charge, the attempt and `result.json`.
//!
//! Also here, pure: the stream's typed view (`StreamView`, `StreamSummary`), `classify` and
//! `has_status_line`.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use rustix::process::Signal;
use serde::de::{IgnoredAny, MapAccess};

use super::{BlockedBy, DispatchRequest, ExitReason, STEP_ITEM_TYPES, Status, thread_id};
use crate::briefs::{UnknownHeadKey, after_folded, parse_report_head};
use crate::pytext::{py_isspace, py_splitlines, py_strip};
use crate::usage::meter::{Lenient, VendorStr, deserialize_leniently};
use crate::wire::PyStr;

// --------------------------------------------------------------------------
// Timing (`dispatch.py:89-100`; `docs/cli-contract.md`, "Timing that parity tests depend on")

/// Wall-clock time between heartbeat touches (S2-R31: every second).
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);
/// How long a starting runner retries `runner.lock` before concluding another runner owns it.
pub const RUNNER_LOCK_RETRY: Duration = Duration::from_secs(2);
/// The stop ladder's spacing: SIGINT, SIGTERM 5 s later, SIGKILL 5 s after that.
pub const SIGNAL_STEP: Duration = Duration::from_secs(5);
/// After SIGKILL, it is sent again this often until the processes are confirmed gone.
pub const KILL_RETRY: Duration = Duration::from_secs(5);
/// Kept back from a cancel's deadline for charging and writing the result.
pub const CANCEL_FINISH_MARGIN: Duration = Duration::from_secs(2);
/// How often a watching runner looks again (for the `cancel` file, among the rest).
pub const POLL: Duration = Duration::from_millis(100);
/// How often a watching runner snapshots Codex's descendants.
pub const SNAPSHOT_EVERY: Duration = Duration::from_secs(1);
/// In a wait for the recorded processes to be gone (`_Tracker.wait_gone`): how often the
/// recorded set is snapshotted again, with the tag sweep...
pub const WAIT_SNAPSHOT: Duration = Duration::from_millis(250);
/// ...and how often their liveness is checked in between.
pub const WAIT_POLL: Duration = Duration::from_millis(50);

// --------------------------------------------------------------------------
// The stream (`dispatch._Stream`)

/// One line of Codex's `--json` stream, as the runner reads it: only the fields it uses, each
/// only when it has the JSON type the Python's `isinstance` checks want (a field of another
/// type is no value, never an error). A repeated key keeps its last value, as `json.loads`
/// does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamView {
    /// `type`.
    pub kind: Option<PyStr>,
    /// `thread_id` (`thread.started`).
    pub thread_id: Option<PyStr>,
    /// `item`, when it is an object (`item.completed`).
    pub item: Option<ItemView>,
    /// `message` (`error`).
    pub message: Option<PyStr>,
    /// `error.message`, when `error` is an object (`turn.failed`).
    pub error_message: Option<PyStr>,
}

/// A stream event's `item` object: its `type` and `text`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemView {
    pub kind: Option<PyStr>,
    pub text: Option<PyStr>,
}

impl StreamView {
    /// One stream line without its newline, as `_Stream.poll` reads it: decoded as UTF-8
    /// with replacement, then JSON; `None` when that fails or the value is not an object.
    pub fn parse(line: &[u8]) -> Option<StreamView> {
        serde_json::from_str::<Top>(&String::from_utf8_lossy(line))
            .ok()
            .and_then(|top| top.0)
    }

    fn is(&self, kind: &str) -> bool {
        is_text(self.kind.as_ref(), kind)
    }

    fn item_is(&self, kind: &str) -> bool {
        self.item
            .as_ref()
            .is_some_and(|item| is_text(item.kind.as_ref(), kind))
    }

    /// Whether this is a worker step: an `item.completed` whose item type is one of
    /// `STEP_ITEM_TYPES` (probes.md, Tasks 3 and 4).
    pub fn is_step(&self) -> bool {
        self.is("item.completed") && STEP_ITEM_TYPES.iter().any(|t| self.item_is(t))
    }
}

fn is_text(value: Option<&PyStr>, text: &str) -> bool {
    value.is_some_and(|v| v.as_wtf8() == text.as_bytes())
}

deserialize_leniently!(Top, ItemField, ErrorField);

/// A stream line's value: its view when it is an object.
#[derive(Default)]
struct Top(Option<StreamView>);

impl Lenient for Top {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut v = StreamView::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"type" => v.kind = map.next_value::<VendorStr>()?.0,
                b"thread_id" => v.thread_id = map.next_value::<VendorStr>()?.0,
                b"item" => v.item = map.next_value::<ItemField>()?.0,
                b"message" => v.message = map.next_value::<VendorStr>()?.0,
                b"error" => v.error_message = map.next_value::<ErrorField>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Top(Some(v)))
    }
}

/// `item`: its view when it is an object.
#[derive(Default)]
struct ItemField(Option<ItemView>);

impl Lenient for ItemField {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut item = ItemView::default();
        while let Some(key) = map.next_key::<PyStr>()? {
            match key.as_wtf8() {
                b"type" => item.kind = map.next_value::<VendorStr>()?.0,
                b"text" => item.text = map.next_value::<VendorStr>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(ItemField(Some(item)))
    }
}

/// `error`: its `message`, when it is an object with a string there.
#[derive(Default)]
struct ErrorField(Option<PyStr>);

impl Lenient for ErrorField {
    fn object<'de, A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut message = None;
        while let Some(key) = map.next_key::<PyStr>()? {
            if key.as_wtf8() == b"message" {
                message = map.next_value::<VendorStr>()?.0;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(ErrorField(message))
    }
}

/// What a stream says, folded line by line as `_Stream._add` folds it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamSummary {
    /// The worker steps (`StreamView::is_step`).
    pub steps: u32,
    /// The first `thread.started`'s `thread_id`. One that a `String` cannot hold (a lone
    /// surrogate, which no Codex thread ID has) counts as none, as one that is not a string
    /// does (`thread_id`).
    pub thread_id: Option<String>,
    /// A `turn.started` not (yet) followed by its `turn.completed` (R41b).
    pub turn_open: bool,
    /// The last `agent_message` item's text (`_Stream.last_agent_message`).
    pub last_agent_message: Option<PyStr>,
    /// The `error` events' messages and the `turn.failed` events' `error.message`s, in order
    /// (`_Stream.error_texts`).
    pub errors: Vec<PyStr>,
}

impl StreamSummary {
    /// Folds one stream event in.
    pub fn add(&mut self, view: &StreamView) {
        if view.is("turn.started") {
            self.turn_open = true;
        } else if view.is("turn.completed") {
            self.turn_open = false;
        }
        if view.is("thread.started") {
            if self.thread_id.is_none() {
                self.thread_id = view.thread_id.as_ref().and_then(thread_id);
            }
        } else if view.is_step() {
            self.steps = self.steps.saturating_add(1);
        }
        if view.is("item.completed")
            && view.item_is("agent_message")
            && let Some(text) = view.item.as_ref().and_then(|i| i.text.clone())
        {
            self.last_agent_message = Some(text);
        }
        if view.is("error") {
            self.errors.extend(view.message.clone());
        }
        if view.is("turn.failed") {
            self.errors.extend(view.error_message.clone());
        }
    }

    /// A whole stream file's contents: every complete line (a last line without its newline
    /// is still being written, and is left out).
    pub fn of(stream: &[u8]) -> StreamSummary {
        let mut summary = StreamSummary::default();
        let mut lines: Vec<&[u8]> = stream.split(|b| *b == b'\n').collect();
        lines.pop();
        for view in lines.into_iter().filter_map(StreamView::parse) {
            summary.add(&view);
        }
        summary
    }
}

// --------------------------------------------------------------------------
// The state machine

/// Why the runner stopped Codex (`stopped_for`). `Interrupted` is a dead runner's dispatch,
/// finalized by whoever finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Cancelled,
    StepLimit,
    TimeLimit,
    /// The runner itself failed while watching (the Python's `except Exception`).
    Error,
    Interrupted,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        self.exit_reason().as_str()
    }

    /// The result's `exit_reason` for a dispatch stopped for this reason.
    pub fn exit_reason(self) -> ExitReason {
        match self {
            StopReason::Cancelled => ExitReason::Cancelled,
            StopReason::StepLimit => ExitReason::StepLimit,
            StopReason::TimeLimit => ExitReason::TimeLimit,
            StopReason::Error => ExitReason::Error,
            StopReason::Interrupted => ExitReason::Interrupted,
        }
    }
}

/// A rung of the stop ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    /// SIGINT, to Codex's group only.
    Int,
    /// SIGTERM, to Codex's group and every recorded group.
    Term,
    /// SIGKILL, to the same.
    Kill,
}

impl Sig {
    pub fn signal(self) -> Signal {
        match self {
            Sig::Int => Signal::INT,
            Sig::Term => Signal::TERM,
            Sig::Kill => Signal::KILL,
        }
    }

    /// Whether only Codex's own group gets it (`_Tracker.signal(codex_only=…)`).
    pub fn codex_only(self) -> bool {
        self == Sig::Int
    }
}

/// What the recorded set looks like now: whether Codex is alive, and whether anything
/// recorded is (Codex included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    pub codex: bool,
    pub any: bool,
}

/// One request for the recorded set's liveness, carried back by its answer: the ladder takes
/// only the answer to the request it has outstanding, so a late or stray answer can never
/// stand for a later look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask(u64);

/// What the shell tells the core.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A complete line of Codex's stream.
    Line(StreamView),
    /// Codex exited (and was reaped).
    Exited(ExitStatus),
    /// A signal the runner caught: SIGTERM is a cancel request; any other is ignored.
    Signal(i32),
    /// A cancel request: the worker folder's `cancel` file is there, or SIGTERM came.
    CancelRequested,
    /// The answer to the `Snapshot` or `CheckAlive` that asked `Ask`.
    Procs(Ask, Liveness),
    /// The shell failed while Codex ran (a heartbeat it could not touch, a stream it could
    /// not read).
    Fault,
    /// Time passed.
    Tick,
}

/// What the core asks the shell to do, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    TouchHeartbeat,
    /// Record Codex's live descendants (and, with `tagged`, every process carrying the
    /// dispatch tag), drop the recorded ones that are gone, and write `procs.json` if it
    /// changed. With an `ask`, answered with `Event::Procs`; a watching runner's own refresh
    /// asks nothing.
    Snapshot {
        tagged: bool,
        ask: Option<Ask>,
    },
    /// Answered with `Event::Procs`, the recorded set unchanged.
    CheckAlive(Ask),
    /// Signal Codex's group, and with `Term` and `Kill` every recorded group (a recorded
    /// process that leads no recorded group, itself), each only while the recorded process
    /// still has its recorded identity.
    SignalGroups(Sig),
    /// Write `stopping.json`: the reason, or `finished` (`None`) after Codex's own exit.
    Acknowledge(Option<StopReason>),
    /// Everything recorded is gone: finalize (classify, commit, charge, attempt, result).
    ChargeAndFinish(Finish),
}

/// How the watch ended, for finalizing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finish {
    pub stopped_for: Option<StopReason>,
    /// Codex's return code as Python's `Popen.returncode` gives it (a signal is negative).
    pub codex_exit: Option<i32>,
}

/// A dispatch's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Worker steps (`request.json`'s `steps`).
    pub steps: i64,
    /// Time awake since Codex was launched: `minutes` × the minute's length.
    pub time: Duration,
}

impl Limits {
    /// The request's limits, a minute being `minute_seconds` long (60, or
    /// `LEADHAND_MINUTE_SECONDS` in tests). A time that is not a number never runs out; one
    /// that is not positive is out at once, as the Python's `now - t0 >= limit` is.
    pub fn of(req: &DispatchRequest, minute_seconds: f64) -> Limits {
        let seconds = req.minutes as f64 * minute_seconds;
        let time = if seconds.is_nan() {
            Duration::MAX
        } else if seconds <= 0.0 {
            Duration::ZERO
        } else {
            Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
        };
        Limits {
            steps: req.steps,
            time,
        }
    }
}

/// The runner: watching Codex, then stopping it, then finished.
#[derive(Clone, Debug)]
pub struct Runner {
    limits: Limits,
    started: Instant,
    steps: u32,
    last_beat: Instant,
    last_snapshot: Instant,
    cancel: bool,
    codex_exit: Option<i32>,
    phase: Phase,
}

#[derive(Clone, Debug)]
enum Phase {
    Watching,
    Stopping {
        stopped_for: Option<StopReason>,
        ladder: Ladder,
    },
    Finished,
}

impl Runner {
    /// The runner for `req`, Codex launched at `started`: a minute is
    /// `testing::minute_seconds()` long.
    pub fn new(req: &DispatchRequest, started: Instant) -> Runner {
        Runner::with_limits(Limits::of(req, crate::testing::minute_seconds()), started)
    }

    pub fn with_limits(limits: Limits, started: Instant) -> Runner {
        Runner {
            limits,
            started,
            steps: 0,
            last_beat: started,
            last_snapshot: started,
            cancel: false,
            codex_exit: None,
            phase: Phase::Watching,
        }
    }

    /// One event at `now`: what to do about it.
    pub fn step(&mut self, ev: Event, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if matches!(self.phase, Phase::Finished) {
            return out;
        }
        if now.saturating_duration_since(self.last_beat) >= HEARTBEAT_EVERY {
            self.last_beat = now;
            out.push(Action::TouchHeartbeat);
        }
        match &ev {
            Event::Line(view) => {
                if view.is_step() {
                    self.steps = self.steps.saturating_add(1);
                }
            }
            Event::Signal(sig) => self.cancel |= *sig == Signal::TERM.as_raw(),
            Event::CancelRequested => self.cancel = true,
            Event::Exited(status) => {
                self.codex_exit = Some(return_code(*status));
                if self.watching() {
                    self.begin_stop(None, now, &mut out);
                }
            }
            Event::Fault => {
                if self.watching() {
                    self.begin_stop(Some(StopReason::Error), now, &mut out);
                }
            }
            Event::Procs(..) | Event::Tick => {}
        }
        if self.watching() {
            if let Some(reason) = self.limit_reached(now) {
                self.begin_stop(Some(reason), now, &mut out);
            } else if now.saturating_duration_since(self.last_snapshot) >= SNAPSHOT_EVERY {
                self.last_snapshot = now;
                out.push(Action::Snapshot {
                    tagged: false,
                    ask: None,
                });
            }
            return out;
        }
        let mut finished = None;
        if let Phase::Stopping {
            stopped_for,
            ladder,
        } = &mut self.phase
        {
            out.extend(ladder.step(&ev, now));
            if ladder.done().is_some() {
                finished = Some(Finish {
                    stopped_for: *stopped_for,
                    codex_exit: self.codex_exit,
                });
            }
        }
        if let Some(finish) = finished {
            self.phase = Phase::Finished;
            out.push(Action::ChargeAndFinish(finish));
        }
        out
    }

    /// How long the shell may wait for an event before it ticks.
    pub fn next_wakeup(&self, now: Instant) -> Duration {
        let beat = (self.last_beat + HEARTBEAT_EVERY).saturating_duration_since(now);
        match &self.phase {
            Phase::Watching => {
                let snapshot = (self.last_snapshot + SNAPSHOT_EVERY).saturating_duration_since(now);
                let limit = self
                    .started
                    .checked_add(self.limits.time)
                    .map_or(Duration::MAX, |at| at.saturating_duration_since(now));
                POLL.min(beat).min(snapshot).min(limit)
            }
            Phase::Stopping { ladder, .. } => ladder.next_wakeup(now).min(beat),
            Phase::Finished => Duration::MAX,
        }
    }

    /// The `_Tracker` loop's checks, in its order: a cancel, then the steps, then the time.
    fn limit_reached(&self, now: Instant) -> Option<StopReason> {
        if self.cancel {
            Some(StopReason::Cancelled)
        } else if i64::from(self.steps) >= self.limits.steps {
            Some(StopReason::StepLimit)
        } else if now.saturating_duration_since(self.started) >= self.limits.time {
            Some(StopReason::TimeLimit)
        } else {
            None
        }
    }

    /// The acknowledgement, then the ladder's first snapshot.
    fn begin_stop(&mut self, reason: Option<StopReason>, now: Instant, out: &mut Vec<Action>) {
        let mut ladder = Ladder::new(None);
        out.push(Action::Acknowledge(reason));
        out.extend(ladder.start(now));
        self.phase = Phase::Stopping {
            stopped_for: reason,
            ladder,
        };
    }

    /// Whether Codex is still being watched (no stop has begun).
    pub fn watching(&self) -> bool {
        matches!(self.phase, Phase::Watching)
    }

    /// The steps counted so far.
    pub fn steps(&self) -> u32 {
        self.steps
    }
}

// --------------------------------------------------------------------------
// The stop ladder (`_Tracker.stop`, `_Tracker.wait_gone`)

/// The stop ladder, with or without a deadline (a cancel has one; the runner's own stop
/// does not, and sends SIGKILL until everything is gone). It asks for a snapshot with the tag
/// sweep before each signal, and while it waits for the recorded processes to go, it checks
/// them every `WAIT_POLL` and snapshots them every `WAIT_SNAPSHOT`, the first time at once.
///
/// It has at most one request out at a time, and takes only that request's answer: an answer
/// to anything else (the watching runner's own refresh, an earlier look) is ignored.
#[derive(Clone, Debug)]
pub struct Ladder {
    deadline: Option<Instant>,
    state: LadderState,
    /// The request whose answer the ladder waits for.
    asked: Option<Ask>,
    /// The next request's number.
    asks: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum LadderState {
    Idle,
    /// A snapshot was asked for; its answer decides the next rung.
    Probing(Probe),
    /// `sig` was sent; waiting until `until` for everything to be gone.
    Waiting {
        sig: Sig,
        until: Instant,
        last_snapshot: Instant,
        next_check: Instant,
    },
    /// Over: everything is gone (`true`), or the deadline passed with something alive.
    Done(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    First,
    BeforeTerm,
    BeforeKill,
}

impl Ladder {
    pub fn new(deadline: Option<Instant>) -> Ladder {
        Ladder {
            deadline,
            state: LadderState::Idle,
            asked: None,
            asks: 0,
        }
    }

    /// The first snapshot.
    pub fn start(&mut self, _now: Instant) -> Vec<Action> {
        self.state = LadderState::Probing(Probe::First);
        vec![self.snapshot()]
    }

    /// `Event::Procs` answering the outstanding request, and `Event::Tick`, drive it; anything
    /// else is ignored.
    pub fn step(&mut self, ev: &Event, now: Instant) -> Vec<Action> {
        match ev {
            Event::Procs(ask, procs) if self.asked == Some(*ask) => {
                self.asked = None;
                self.answered(*procs, now)
            }
            Event::Tick => self.tick(now),
            _ => Vec::new(),
        }
    }

    /// A snapshot with the tag sweep, asked for.
    fn snapshot(&mut self) -> Action {
        Action::Snapshot {
            tagged: true,
            ask: Some(self.ask()),
        }
    }

    fn ask(&mut self) -> Ask {
        let ask = Ask(self.asks);
        self.asks += 1;
        self.asked = Some(ask);
        ask
    }

    /// The answer to the outstanding request.
    fn answered(&mut self, procs: Liveness, now: Instant) -> Vec<Action> {
        match self.state.clone() {
            LadderState::Probing(probe) => self.probed(probe, procs, now),
            LadderState::Waiting {
                sig,
                until,
                last_snapshot,
                ..
            } => {
                // `wait_gone`: gone first, then the deadline.
                if !procs.any {
                    self.state = LadderState::Done(true);
                    Vec::new()
                } else if now >= until {
                    self.next_rung(sig, now)
                } else {
                    self.state = LadderState::Waiting {
                        sig,
                        until,
                        last_snapshot,
                        next_check: now + WAIT_POLL,
                    };
                    Vec::new()
                }
            }
            LadderState::Idle | LadderState::Done(_) => Vec::new(),
        }
    }

    /// A wait's next look, when it is due and no answer is outstanding: a snapshot every
    /// `WAIT_SNAPSHOT`, a check in between.
    fn tick(&mut self, now: Instant) -> Vec<Action> {
        let LadderState::Waiting {
            sig,
            until,
            last_snapshot,
            next_check,
        } = self.state
        else {
            return Vec::new();
        };
        if self.asked.is_some() || now < next_check {
            return Vec::new();
        }
        if now.saturating_duration_since(last_snapshot) >= WAIT_SNAPSHOT {
            self.state = LadderState::Waiting {
                sig,
                until,
                last_snapshot: now,
                next_check,
            };
            vec![self.snapshot()]
        } else {
            vec![Action::CheckAlive(self.ask())]
        }
    }

    /// How long until the next check.
    pub fn next_wakeup(&self, now: Instant) -> Duration {
        match &self.state {
            _ if self.asked.is_some() => Duration::ZERO,
            LadderState::Waiting { next_check, .. } => next_check.saturating_duration_since(now),
            LadderState::Probing(_) => Duration::ZERO,
            LadderState::Idle | LadderState::Done(_) => Duration::MAX,
        }
    }

    /// A probe's answer: the next signal, or done when nothing recorded is alive. Codex gets
    /// SIGINT only while it is alive.
    fn probed(&mut self, probe: Probe, procs: Liveness, now: Instant) -> Vec<Action> {
        if !procs.any {
            self.state = LadderState::Done(true);
            return Vec::new();
        }
        match probe {
            Probe::First if procs.codex => self.signal(Sig::Int, now),
            Probe::First => {
                self.state = LadderState::Probing(Probe::BeforeTerm);
                vec![self.snapshot()]
            }
            Probe::BeforeTerm => self.signal(Sig::Term, now),
            Probe::BeforeKill => self.signal(Sig::Kill, now),
        }
    }

    /// `sig`, then the wait for everything to be gone, which snapshots at once.
    fn signal(&mut self, sig: Sig, now: Instant) -> Vec<Action> {
        let span = match (sig, self.deadline) {
            (Sig::Kill, None) => KILL_RETRY,
            _ => self.left(now),
        };
        self.state = LadderState::Waiting {
            sig,
            until: now + span,
            last_snapshot: now,
            next_check: now,
        };
        vec![Action::SignalGroups(sig), self.snapshot()]
    }

    /// `left()`: a signal step, cut to what is left of the deadline.
    fn left(&self, now: Instant) -> Duration {
        match self.deadline {
            None => SIGNAL_STEP,
            Some(deadline) => SIGNAL_STEP.min(deadline.saturating_duration_since(now)),
        }
    }

    /// The wait after `sig` ran out with something alive: the next rung's probe, or, after
    /// SIGKILL past the deadline, the end.
    fn next_rung(&mut self, sig: Sig, now: Instant) -> Vec<Action> {
        let next = match sig {
            Sig::Int => Probe::BeforeTerm,
            Sig::Term => Probe::BeforeKill,
            Sig::Kill if self.deadline.is_some_and(|d| now >= d) => {
                self.state = LadderState::Done(false);
                return Vec::new();
            }
            Sig::Kill => Probe::BeforeKill,
        };
        self.state = LadderState::Probing(next);
        vec![self.snapshot()]
    }

    /// `Some(true)` once everything recorded is gone, `Some(false)` once the deadline passed
    /// with something still alive; `None` while it runs.
    pub fn done(&self) -> Option<bool> {
        match self.state {
            LadderState::Done(gone) => Some(gone),
            _ => None,
        }
    }
}

// --------------------------------------------------------------------------
// Classifying the result (`_classify`, `has_status_line`)

/// What a dispatch's result is (`_classify`'s tuple).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classified {
    pub status: Status,
    pub blocked_by: BlockedBy,
    pub exit_reason: ExitReason,
    /// The reply has no parseable status line: keep it as `reply.raw.md`.
    pub keep_raw: bool,
}

/// `_classify`: the status, blocker and exit reason of a dispatch from why it stopped,
/// Codex's return code, its reply, the stream's error texts and its stderr. A usage-limit
/// message counts only when Codex did not finish cleanly with a status. `UnknownHeadKey`
/// where the Python's `parse_report_head` raises `KeyError` (S2-R32), in the same order of
/// evaluation, so the same replies fail.
pub fn classify(
    stopped_for: Option<StopReason>,
    codex_exit: Option<i32>,
    reply: &str,
    errors: &[PyStr],
    stderr: &str,
) -> Result<Classified, UnknownHeadKey> {
    let result = |status, blocked_by, exit_reason, keep_raw| Classified {
        status,
        blocked_by,
        exit_reason,
        keep_raw,
    };
    match stopped_for {
        Some(StopReason::Error) => {
            // The runner itself failed.
            return Ok(result(
                Status::Blocked,
                BlockedBy::Environment,
                ExitReason::Error,
                false,
            ));
        }
        Some(reason) => {
            return Ok(result(
                Status::Spent,
                BlockedBy::Nothing,
                reason.exit_reason(),
                false,
            ));
        }
        None => {}
    }
    let has_reply = !py_strip(reply).is_empty();
    let clean = codex_exit == Some(0) && has_reply && has_status_line(reply)?;
    if !clean
        && (errors.iter().any(|t| mentions_usage_limit(&t.lossy())) || mentions_usage_limit(stderr))
    {
        return Ok(result(
            Status::Blocked,
            BlockedBy::UsageLimit,
            ExitReason::UsageLimit,
            false,
        ));
    }
    if codex_exit.is_some_and(|c| c < 0) && !has_reply {
        // Killed by a signal leadhand did not send.
        return Ok(result(
            Status::Spent,
            BlockedBy::Nothing,
            ExitReason::Interrupted,
            false,
        ));
    }
    if has_reply {
        if !has_status_line(reply)? {
            return Ok(result(
                Status::Blocked,
                BlockedBy::Nothing,
                ExitReason::Completed,
                true,
            ));
        }
        // A reviewer's or verifier's reply leads with its Verdict or Result: the work finished.
        let (key, value, blocked_by) = parse_report_head(reply)?;
        let status = match (key, value) {
            ("Status", "READY") | ("Verdict" | "Result", _) => Status::Ready,
            ("Status", "QUESTION") => Status::Question,
            ("Status", "SPENT") => Status::Spent,
            _ => Status::Blocked,
        };
        let blocked_by = match blocked_by {
            "environment" => BlockedBy::Environment,
            "usage limit" => BlockedBy::UsageLimit,
            _ => BlockedBy::Nothing,
        };
        return Ok(result(status, blocked_by, ExitReason::Completed, false));
    }
    let exit_reason = if codex_exit == Some(0) {
        ExitReason::Completed
    } else {
        ExitReason::Error
    };
    Ok(result(
        Status::Blocked,
        BlockedBy::Nothing,
        exit_reason,
        true,
    ))
}

/// `has_status_line`: the reply's head parses, or its first non-blank line is a
/// `Status: BLOCKED` the parse does not take (markup around it).
pub fn has_status_line(reply: &str) -> Result<bool, UnknownHeadKey> {
    if parse_report_head(reply)? != ("Status", "BLOCKED", "") {
        return Ok(true);
    }
    let first = py_splitlines(reply)
        .into_iter()
        .find(|line| !py_strip(line).is_empty())
        .unwrap_or("");
    Ok(status_blocked(first))
}

/// `_STATUS_BLOCKED.match(line)`: `^[\s*_`#>]*status[\s*_`]*:[\s*_`]*blocked\b`, case-folded.
/// No class can take the letter after it, so each is matched greedily, without backtracking.
fn status_blocked(line: &str) -> bool {
    let marks = |c: char| py_isspace(c) || matches!(c, '*' | '_' | '`');
    let rest = line.trim_start_matches(|c: char| marks(c) || matches!(c, '#' | '>'));
    let Some((_, rest)) = after_folded(rest, "status") else {
        return false;
    };
    let Some(rest) = rest.trim_start_matches(marks).strip_prefix(':') else {
        return false;
    };
    let Some((_, rest)) = after_folded(rest.trim_start_matches(marks), "blocked") else {
        return false;
    };
    !rest.chars().next().is_some_and(is_word)
}

/// `\w` of Python's Unicode `re`: a letter, a digit or other number, or `_`.
fn is_word(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// `_USAGE_LIMIT.search(text)`: `usage[\s_-]*limit`, case-folded as Python's Unicode
/// `IGNORECASE` folds.
pub fn mentions_usage_limit(text: &str) -> bool {
    text.char_indices().any(|(at, _)| {
        after_folded(&text[at..], "usage").is_some_and(|(_, rest)| {
            let rest = rest.trim_start_matches(|c: char| py_isspace(c) || matches!(c, '_' | '-'));
            after_folded(rest, "limit").is_some()
        })
    })
}

/// Whether a dispatch's usage may be missing a response (R41b): Codex ran, and was stopped,
/// killed by a signal, or ended inside a turn.
pub fn possibly_incomplete(
    launched: bool,
    stopped_for: Option<StopReason>,
    codex_exit: Option<i32>,
    turn_open: bool,
) -> bool {
    launched && (stopped_for.is_some() || codex_exit.is_some_and(|c| c < 0) || turn_open)
}

/// `Popen.returncode`: the exit code, or minus the signal that killed it.
pub fn return_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::wire::tests::{hex, reference_outputs};

    fn term() -> i32 {
        Signal::TERM.as_raw()
    }

    fn req(steps: i64, minutes: i64) -> DispatchRequest {
        DispatchRequest {
            model: "gpt-6-luna".into(),
            effort: "low".into(),
            role: "implementer".into(),
            brief: PathBuf::from("/ws/.leadhand/1/d1/brief.md"),
            report: PathBuf::from("/ws/.leadhand/1/d1/report.md"),
            cwd: PathBuf::from("/ws"),
            run_dir: PathBuf::from("/runs/r1"),
            card: Some(1),
            attempt: 1,
            dispatch_id: "d1".into(),
            steps,
            minutes,
            network: false,
            writable_dirs: Vec::new(),
            resume: None,
            kind: None,
            branch: None,
        }
    }

    /// A runner for `steps` steps and `minutes` sixty-second minutes, Codex launched at `t0`
    /// (explicit limits: `Runner::new` would read the minute's length from the test hooks).
    fn runner(steps: i64, minutes: i64, t0: Instant) -> Runner {
        Runner::with_limits(Limits::of(&req(steps, minutes), 60.0), t0)
    }

    #[test]
    fn new_takes_the_request_limits_and_the_minute_hook() {
        let t0 = Instant::now();
        let r = Runner::new(&req(3, 2), t0);
        let minute = crate::testing::minute_seconds();
        assert_eq!(r.limits, Limits::of(&req(3, 2), minute));
        assert_eq!((r.started, r.steps()), (t0, 0));
    }

    fn line(json: &str) -> Event {
        Event::Line(StreamView::parse(json.as_bytes()).expect("a JSON object"))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    const ALIVE: Liveness = Liveness {
        codex: true,
        any: true,
    };
    const LEFT: Liveness = Liveness {
        codex: false,
        any: true,
    };
    const GONE: Liveness = Liveness {
        codex: false,
        any: false,
    };

    /// Any request's token, in a logged action: the logs compare actions without their tokens.
    const ANY: Ask = Ask(u64::MAX);
    /// A tagged snapshot that asks for an answer, as logged.
    const SNAP: Action = Action::Snapshot {
        tagged: true,
        ask: Some(ANY),
    };
    /// A watching runner's own refresh, which asks nothing.
    const WATCH: Action = Action::Snapshot {
        tagged: false,
        ask: None,
    };
    /// A liveness check, as logged.
    const CHECK: Action = Action::CheckAlive(ANY);

    /// The request an action makes, if it asks for an answer.
    fn ask_of(a: &Action) -> Option<Ask> {
        match a {
            Action::Snapshot { ask, .. } => *ask,
            Action::CheckAlive(ask) => Some(*ask),
            _ => None,
        }
    }

    /// `a` with its token replaced by `ANY`.
    fn logged(a: Action) -> Action {
        match a {
            Action::Snapshot {
                tagged,
                ask: Some(_),
            } => Action::Snapshot {
                tagged,
                ask: Some(ANY),
            },
            Action::CheckAlive(_) => CHECK,
            other => other,
        }
    }

    fn all_logged(actions: Vec<Action>) -> Vec<Action> {
        actions.into_iter().map(logged).collect()
    }

    /// The shell's part in a test: each request is answered at once with `procs`, under its
    /// own token; every action is recorded (tokens aside), the answers' too.
    fn act(r: &mut Runner, ev: Event, now: Instant, procs: Liveness, log: &mut Vec<Action>) {
        let mut todo = std::collections::VecDeque::from(r.step(ev, now));
        while let Some(a) = todo.pop_front() {
            let ask = ask_of(&a);
            log.push(logged(a));
            if let Some(ask) = ask {
                todo.extend(r.step(Event::Procs(ask, procs), now));
            }
        }
    }

    /// Ticks `r` every `WAIT_POLL` from `from` up to `to` (inclusive), answering with `procs`.
    fn tick_until(
        r: &mut Runner,
        from: Instant,
        to: Instant,
        procs: Liveness,
        log: &mut Vec<Action>,
    ) {
        let mut t = from;
        while t <= to {
            act(r, Event::Tick, t, procs, log);
            t += WAIT_POLL;
        }
    }

    fn signals(log: &[Action]) -> Vec<Sig> {
        log.iter()
            .filter_map(|a| match a {
                Action::SignalGroups(s) => Some(*s),
                _ => None,
            })
            .collect()
    }

    fn stops(log: &[Action]) -> Vec<Option<StopReason>> {
        log.iter()
            .filter_map(|a| match a {
                Action::Acknowledge(r) => Some(*r),
                _ => None,
            })
            .collect()
    }

    fn finish(log: &[Action]) -> Option<Finish> {
        log.iter().find_map(|a| match a {
            Action::ChargeAndFinish(f) => Some(*f),
            _ => None,
        })
    }

    #[test]
    fn step_limit_starts_ladder_after_nth_counted_item() {
        let t0 = Instant::now();
        let mut r = runner(3, 60, t0);
        let mut log = Vec::new();
        for json in [
            r#"{"type": "thread.started", "thread_id": "t"}"#,
            r#"{"type": "turn.started"}"#,
            r#"{"type": "item.completed", "item": {"type": "agent_message", "text": "hi"}}"#,
            r#"{"type": "item.completed", "item": {"type": "command_execution"}}"#,
            r#"{"type": "item.started", "item": {"type": "command_execution"}}"#,
            r#"{"type": "item.completed", "item": {"type": "file_change"}}"#,
            r#"{"type": "item.completed", "item": "command_execution"}"#,
            r#"{"type": "item.updated", "item": {"type": "file_change"}}"#,
        ] {
            act(&mut r, line(json), t0 + ms(10), ALIVE, &mut log);
        }
        assert_eq!(r.steps(), 2);
        assert!(r.watching(), "{log:?}");
        assert_eq!(stops(&log), []);
        act(
            &mut r,
            line(r#"{"type": "item.completed", "item": {"type": "command_execution"}}"#),
            t0 + ms(20),
            ALIVE,
            &mut log,
        );
        assert_eq!(r.steps(), 3);
        assert!(!r.watching());
        // The acknowledgement, then the ladder's first snapshot (with the tag sweep), then
        // SIGINT to Codex's group.
        assert_eq!(
            log,
            [
                Action::Acknowledge(Some(StopReason::StepLimit)),
                SNAP,
                Action::SignalGroups(Sig::Int),
                SNAP,
            ]
        );
        // More steps change nothing.
        let mut more = Vec::new();
        act(
            &mut r,
            line(r#"{"type": "item.completed", "item": {"type": "file_change"}}"#),
            t0 + ms(30),
            ALIVE,
            &mut more,
        );
        assert_eq!(more, []);
    }

    #[test]
    fn step_limit_of_zero_or_less_stops_at_the_first_look() {
        for steps in [0, -1] {
            let t0 = Instant::now();
            let mut r = runner(steps, 60, t0);
            let mut log = Vec::new();
            act(&mut r, Event::Tick, t0, ALIVE, &mut log);
            assert_eq!(stops(&log), [Some(StopReason::StepLimit)], "{steps}");
        }
    }

    #[test]
    fn time_limit_uses_minute_seconds() {
        // Two "minutes" of one second each.
        let t0 = Instant::now();
        let mut r = Runner::with_limits(Limits::of(&req(50, 2), 1.0), t0);
        let mut log = Vec::new();
        act(&mut r, Event::Tick, t0 + ms(1999), ALIVE, &mut log);
        assert!(r.watching());
        act(&mut r, Event::Tick, t0 + ms(2000), ALIVE, &mut log);
        assert_eq!(stops(&log), [Some(StopReason::TimeLimit)]);
        // The wake-up never sleeps past the limit (here 1.8 s; the beat is due at 2 s).
        let mut r = Runner::with_limits(Limits::of(&req(50, 2), 0.9), t0);
        act(&mut r, Event::Tick, t0 + ms(1000), ALIVE, &mut Vec::new());
        assert_eq!(r.next_wakeup(t0 + ms(1750)), ms(50));
        // Sixty-second minutes: `minutes` × 60.
        assert_eq!(
            Limits::of(&req(50, 3), 60.0),
            Limits {
                steps: 50,
                time: Duration::from_secs(180)
            }
        );
        assert_eq!(
            Limits::of(&req(50, 3), 0.5).time,
            Duration::from_millis(1500)
        );
        // Not positive: out at once; not a number or too long: never.
        assert_eq!(Limits::of(&req(50, 0), 60.0).time, Duration::ZERO);
        assert_eq!(Limits::of(&req(50, -2), 60.0).time, Duration::ZERO);
        assert_eq!(Limits::of(&req(50, 2), f64::NAN).time, Duration::MAX);
        assert_eq!(Limits::of(&req(50, 2), f64::INFINITY).time, Duration::MAX);
        let mut r = Runner::with_limits(Limits::of(&req(50, 0), 60.0), t0);
        let mut log = Vec::new();
        act(&mut r, Event::Tick, t0, ALIVE, &mut log);
        assert_eq!(stops(&log), [Some(StopReason::TimeLimit)]);
    }

    #[test]
    fn cancel_comes_before_the_step_and_time_limits() {
        // The Python's order: cancelled, then steps, then time.
        let t0 = Instant::now();
        let mut r = Runner::with_limits(Limits::of(&req(0, 0), 60.0), t0);
        let mut log = Vec::new();
        act(&mut r, Event::CancelRequested, t0, ALIVE, &mut log);
        assert_eq!(stops(&log), [Some(StopReason::Cancelled)]);
        let mut r = Runner::with_limits(Limits::of(&req(0, 0), 60.0), t0);
        let mut log = Vec::new();
        act(&mut r, Event::Tick, t0, ALIVE, &mut log);
        assert_eq!(stops(&log), [Some(StopReason::StepLimit)]);
    }

    #[test]
    fn ladder_is_int_then_term_after_5s_then_kill_after_5s() {
        let t0 = Instant::now();
        let mut r = runner(1, 60, t0);
        let mut log = Vec::new();
        // SIGINT at once.
        act(
            &mut r,
            line(r#"{"type": "item.completed", "item": {"type": "command_execution"}}"#),
            t0,
            ALIVE,
            &mut log,
        );
        assert_eq!(signals(&log), [Sig::Int]);
        // Codex ignores it: SIGTERM 5 s later, not before.
        tick_until(&mut r, t0 + WAIT_POLL, t0 + ms(4990), ALIVE, &mut log);
        assert_eq!(signals(&log), [Sig::Int]);
        tick_until(&mut r, t0 + ms(5000), t0 + ms(5000), ALIVE, &mut log);
        assert_eq!(signals(&log), [Sig::Int, Sig::Term]);
        // Then SIGKILL 5 s after that.
        let term_at = t0 + ms(5000);
        tick_until(
            &mut r,
            term_at + WAIT_POLL,
            term_at + ms(4990),
            ALIVE,
            &mut log,
        );
        assert_eq!(signals(&log), [Sig::Int, Sig::Term]);
        tick_until(
            &mut r,
            term_at + ms(5000),
            term_at + ms(5000),
            ALIVE,
            &mut log,
        );
        assert_eq!(signals(&log), [Sig::Int, Sig::Term, Sig::Kill]);
        // Without a deadline SIGKILL is sent again every KILL_RETRY until all is gone.
        let kill_at = term_at + ms(5000);
        tick_until(
            &mut r,
            kill_at + WAIT_POLL,
            kill_at + ms(5000),
            ALIVE,
            &mut log,
        );
        assert_eq!(signals(&log), [Sig::Int, Sig::Term, Sig::Kill, Sig::Kill]);
        assert_eq!(finish(&log), None);
        // Gone: finalize, once.
        let gone_at = kill_at + ms(5050);
        act(&mut r, Event::Tick, gone_at, GONE, &mut log);
        assert_eq!(
            finish(&log),
            Some(Finish {
                stopped_for: Some(StopReason::StepLimit),
                codex_exit: None,
            })
        );
        let mut after = Vec::new();
        act(&mut r, Event::Tick, gone_at + ms(5000), GONE, &mut after);
        assert_eq!(after, []);
        assert_eq!(r.next_wakeup(gone_at), Duration::MAX);
    }

    #[test]
    fn ladder_stops_as_soon_as_everything_is_gone() {
        // Codex leaves at SIGINT: no SIGTERM, and the exit status is kept.
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(&mut r, Event::Signal(term()), t0, ALIVE, &mut log);
        act(
            &mut r,
            Event::Exited(ExitStatus::from_raw(2)),
            t0 + ms(30),
            ALIVE,
            &mut log,
        );
        act(&mut r, Event::Tick, t0 + ms(50), GONE, &mut log);
        assert_eq!(signals(&log), [Sig::Int]);
        assert_eq!(
            finish(&log),
            Some(Finish {
                stopped_for: Some(StopReason::Cancelled),
                codex_exit: Some(-2),
            })
        );
        // Codex leaves its tool child behind: SIGTERM for it after 5 s.
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(&mut r, Event::Signal(term()), t0, ALIVE, &mut log);
        act(
            &mut r,
            Event::Exited(ExitStatus::from_raw(1 << 8)),
            t0 + ms(30),
            LEFT,
            &mut log,
        );
        tick_until(&mut r, t0 + WAIT_POLL, t0 + ms(5000), LEFT, &mut log);
        assert_eq!(signals(&log), [Sig::Int, Sig::Term]);
        act(&mut r, Event::Tick, t0 + ms(5100), GONE, &mut log);
        assert_eq!(finish(&log).map(|f| f.codex_exit), Some(Some(1)));
    }

    #[test]
    fn snapshot_before_each_signal() {
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(&mut r, Event::Signal(term()), t0, ALIVE, &mut log);
        tick_until(&mut r, t0 + WAIT_POLL, t0 + ms(15_000), ALIVE, &mut log);
        assert_eq!(signals(&log), [Sig::Int, Sig::Term, Sig::Kill, Sig::Kill]);
        for (i, a) in log.iter().enumerate() {
            if matches!(a, Action::SignalGroups(_)) {
                assert_eq!(log[i - 1], SNAP, "action {i} of {log:?}");
            }
        }
        // While it waits, liveness is checked every WAIT_POLL and the set re-snapshotted
        // every WAIT_SNAPSHOT, the first time at once.
        let first_wait: Vec<&Action> = log
            .iter()
            .skip_while(|a| **a != Action::SignalGroups(Sig::Int))
            .skip(1)
            .take(6)
            .collect();
        assert_eq!(first_wait, [&SNAP, &CHECK, &CHECK, &CHECK, &CHECK, &SNAP,]);
        // Watching, the snapshots have no tag sweep, every second.
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(&mut r, Event::Tick, t0 + ms(999), ALIVE, &mut log);
        assert!(!log.contains(&WATCH));
        act(&mut r, Event::Tick, t0 + ms(1000), ALIVE, &mut log);
        act(&mut r, Event::Tick, t0 + ms(1500), ALIVE, &mut log);
        act(&mut r, Event::Tick, t0 + ms(2000), ALIVE, &mut log);
        let snaps = log.iter().filter(|a| **a == WATCH).count();
        assert_eq!(snaps, 2);
    }

    #[test]
    fn exit_clears_leftovers_without_sigint() {
        // Codex's own exit: acknowledged as finished, then the ladder from SIGTERM for what it
        // left; nothing left means nothing signalled.
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(
            &mut r,
            Event::Exited(ExitStatus::from_raw(0)),
            t0,
            GONE,
            &mut log,
        );
        assert_eq!(
            log,
            [
                Action::Acknowledge(None),
                SNAP,
                Action::ChargeAndFinish(Finish {
                    stopped_for: None,
                    codex_exit: Some(0),
                }),
            ]
        );
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(
            &mut r,
            Event::Exited(ExitStatus::from_raw(0)),
            t0,
            LEFT,
            &mut log,
        );
        assert_eq!(signals(&log), [Sig::Term]);
        // The probe before SIGTERM is its own snapshot, as the Python's second one is.
        assert_eq!(
            log[..4],
            [
                Action::Acknowledge(None),
                SNAP,
                SNAP,
                Action::SignalGroups(Sig::Term),
            ]
        );
    }

    #[test]
    fn usage_limit_error_gives_blocked_usage_limit() {
        let stream = b"{\"type\": \"thread.started\", \"thread_id\": \"t\"}\n\
            {\"type\": \"turn.started\"}\n\
            {\"type\": \"error\", \"message\": \"You've hit your usage limit. Try again later.\"}\n\
            {\"type\": \"turn.failed\", \"error\": {\"message\": \"You've hit your Usage_Limit.\"}}\n";
        let summary = StreamSummary::of(stream);
        assert_eq!(
            summary.errors,
            [
                PyStr::from("You've hit your usage limit. Try again later."),
                PyStr::from("You've hit your Usage_Limit."),
            ]
        );
        let limited = Classified {
            status: Status::Blocked,
            blocked_by: BlockedBy::UsageLimit,
            exit_reason: ExitReason::UsageLimit,
            keep_raw: false,
        };
        assert_eq!(
            classify(None, Some(1), "", &summary.errors, ""),
            Ok(limited)
        );
        // From stderr alone, too.
        assert_eq!(
            classify(
                None,
                Some(1),
                "",
                &[],
                "ERROR: You've hit your usage-limit."
            ),
            Ok(limited)
        );
        // A clean READY after a warning stands.
        let warned = [PyStr::from("Heads up: you are close to your usage limit.")];
        assert_eq!(
            classify(
                None,
                Some(0),
                "Status: READY\nReport: report.md\n",
                &warned,
                ""
            ),
            Ok(Classified {
                status: Status::Ready,
                blocked_by: BlockedBy::Nothing,
                exit_reason: ExitReason::Completed,
                keep_raw: false,
            })
        );
        // A limit stop wins over any message.
        assert_eq!(
            classify(Some(StopReason::StepLimit), Some(-2), "", &warned, ""),
            Ok(Classified {
                status: Status::Spent,
                blocked_by: BlockedBy::Nothing,
                exit_reason: ExitReason::StepLimit,
                keep_raw: false,
            })
        );
    }

    // CATALOG: test_dispatch.py::FixRoundTests::test_runner_exception_is_blocked_environment
    #[test]
    fn runner_exception_is_blocked_environment() {
        assert_eq!(
            classify(Some(StopReason::Error), None, "", &[], ""),
            Ok(Classified {
                status: Status::Blocked,
                blocked_by: BlockedBy::Environment,
                exit_reason: ExitReason::Error,
                keep_raw: false,
            })
        );
        // The core: a fault while watching stops Codex for "error".
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(&mut r, Event::Fault, t0, ALIVE, &mut log);
        assert_eq!(stops(&log), [Some(StopReason::Error)]);
        // A fault once stopping changes nothing.
        let mut more = Vec::new();
        act(&mut r, Event::Fault, t0 + ms(10), ALIVE, &mut more);
        assert_eq!(more, []);
    }

    #[test]
    fn cancel_signal_starts_ladder() {
        let t0 = Instant::now();
        for cancel in [Event::Signal(term()), Event::CancelRequested] {
            let mut r = runner(50, 60, t0);
            let mut log = Vec::new();
            act(&mut r, cancel.clone(), t0 + ms(300), ALIVE, &mut log);
            assert_eq!(
                log,
                [
                    Action::Acknowledge(Some(StopReason::Cancelled)),
                    SNAP,
                    Action::SignalGroups(Sig::Int),
                    SNAP,
                ],
                "{cancel:?}"
            );
        }
        // SIGHUP is ignored; a second cancel changes nothing.
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(&mut r, Event::Signal(1), t0, ALIVE, &mut log);
        assert_eq!(log, []);
        act(&mut r, Event::Signal(term()), t0, ALIVE, &mut log);
        let before = log.len();
        act(&mut r, Event::CancelRequested, t0 + ms(10), ALIVE, &mut log);
        assert_eq!(log.len(), before);
    }

    #[test]
    fn exit_before_turn_completed_marks_possibly_incomplete() {
        // R41b: the last turn started and never completed.
        let open = StreamSummary::of(
            b"{\"type\": \"thread.started\", \"thread_id\": \"t\"}\n\
              {\"type\": \"turn.started\"}\n\
              {\"type\": \"turn.completed\", \"usage\": {\"input_tokens\": 5}}\n\
              {\"type\": \"turn.started\"}\n",
        );
        assert!(open.turn_open);
        assert!(possibly_incomplete(true, None, Some(0), open.turn_open));
        let closed =
            StreamSummary::of(b"{\"type\": \"turn.started\"}\n{\"type\": \"turn.completed\"}\n");
        assert!(!closed.turn_open);
        assert!(!possibly_incomplete(true, None, Some(0), closed.turn_open));
        // Stopped, or killed by a signal: possibly incomplete too; never when Codex never ran.
        assert!(possibly_incomplete(
            true,
            Some(StopReason::Cancelled),
            Some(0),
            false
        ));
        assert!(possibly_incomplete(true, None, Some(-9), false));
        assert!(!possibly_incomplete(
            false,
            Some(StopReason::Cancelled),
            None,
            true
        ));
        // The core reports Codex's return code for it.
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let mut log = Vec::new();
        act(
            &mut r,
            Event::Exited(ExitStatus::from_raw(9)),
            t0,
            GONE,
            &mut log,
        );
        assert_eq!(finish(&log).map(|f| f.codex_exit), Some(Some(-9)));
    }

    #[test]
    fn heartbeat_every_second() {
        // S2-R31: every second (`HEARTBEAT_EVERY = 1.0`), not every 15.
        assert_eq!(HEARTBEAT_EVERY, Duration::from_secs(1));
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let beats = |log: &[Action]| log.iter().filter(|a| **a == Action::TouchHeartbeat).count();
        let mut log = Vec::new();
        for t in [500, 999] {
            act(&mut r, Event::Tick, t0 + ms(t), ALIVE, &mut log);
        }
        assert_eq!(beats(&log), 0);
        act(&mut r, Event::Tick, t0 + ms(1000), ALIVE, &mut log);
        assert_eq!(beats(&log), 1);
        for t in [1100, 1500, 1999] {
            act(&mut r, Event::Tick, t0 + ms(t), ALIVE, &mut log);
        }
        assert_eq!(beats(&log), 1);
        act(&mut r, Event::Tick, t0 + ms(2000), ALIVE, &mut log);
        assert_eq!(beats(&log), 2);
        // The wake-up never sleeps past the next beat, nor longer than POLL.
        assert_eq!(r.next_wakeup(t0 + ms(2950)), ms(50));
        assert_eq!(r.next_wakeup(t0 + ms(2000)), POLL);
        // The heartbeat goes on while the ladder runs.
        let mut log = Vec::new();
        act(
            &mut r,
            Event::Signal(term()),
            t0 + ms(2500),
            ALIVE,
            &mut log,
        );
        tick_until(&mut r, t0 + ms(2550), t0 + ms(6000), ALIVE, &mut log);
        assert_eq!(beats(&log), 4);
    }

    #[test]
    fn ladder_with_a_deadline_gives_up_there() {
        // A cancel's ladder: each wait is cut to what is left, and after SIGKILL it ends at
        // the deadline with something still alive.
        let t0 = Instant::now();
        let mut ladder = Ladder::new(Some(t0 + ms(7000)));
        let mut log = Vec::new();
        let feed =
            |ladder: &mut Ladder, actions: Vec<Action>, now: Instant, log: &mut Vec<Action>| {
                let mut todo = std::collections::VecDeque::from(actions);
                while let Some(a) = todo.pop_front() {
                    let ask = ask_of(&a);
                    log.push(logged(a));
                    if let Some(ask) = ask {
                        todo.extend(ladder.step(&Event::Procs(ask, ALIVE), now));
                    }
                }
            };
        let first = ladder.start(t0);
        feed(&mut ladder, first, t0, &mut log);
        let mut t = t0;
        while ladder.done().is_none() && t < t0 + ms(20_000) {
            t += WAIT_POLL;
            let actions = ladder.step(&Event::Tick, t);
            feed(&mut ladder, actions, t, &mut log);
        }
        assert_eq!(signals(&log), [Sig::Int, Sig::Term, Sig::Kill]);
        assert_eq!(ladder.done(), Some(false));
        assert!(t >= t0 + ms(7000) && t < t0 + ms(7100), "{:?}", t - t0);
        // Nothing alive at the first look: done at once, nothing signalled.
        let mut ladder = Ladder::new(Some(t0 + ms(7000)));
        let first = ladder.start(t0);
        assert_eq!(all_logged(first.clone()), [SNAP]);
        let ask = ask_of(&first[0]).unwrap();
        assert_eq!(ladder.step(&Event::Procs(ask, GONE), t0), []);
        assert_eq!(ladder.done(), Some(true));
    }

    #[test]
    fn ladder_takes_only_the_answer_to_its_own_request() {
        // The review's case: Codex exits while the runner watches, so the ladder starts; a
        // stale answer (to any other request) arrives before the ladder's own, and says nothing
        // is alive. It must not end the ladder: the tagged snapshot decides, and here a tagged
        // process is still alive, so it gets SIGTERM.
        let t0 = Instant::now();
        let mut r = runner(50, 60, t0);
        let actions = r.step(Event::Exited(ExitStatus::from_raw(0)), t0);
        assert_eq!(
            all_logged(actions.clone()),
            [Action::Acknowledge(None), SNAP]
        );
        let ask = ask_of(&actions[1]).unwrap();
        for stale in [Ask(ask.0 + 1), Ask(ask.0.wrapping_sub(1)), ANY] {
            assert_eq!(r.step(Event::Procs(stale, GONE), t0), [], "{stale:?}");
        }
        let probe = r.step(Event::Procs(ask, LEFT), t0);
        assert_eq!(all_logged(probe.clone()), [SNAP]);
        // An answer already taken is stale too.
        assert_eq!(r.step(Event::Procs(ask, GONE), t0), []);
        let before_term = ask_of(&probe[0]).unwrap();
        let term = r.step(Event::Procs(before_term, LEFT), t0);
        assert_eq!(all_logged(term), [Action::SignalGroups(Sig::Term), SNAP]);
        // A tick while an answer is outstanding asks nothing more.
        assert_eq!(r.step(Event::Tick, t0 + ms(100)), []);
    }

    // ----------------------------------------------------------------------
    // Parity with the Python reference

    /// The reference fixture of the parity tests.
    const FIXTURE: &str = "dispatch/runner.json";

    /// The recorded `out` of each row of `section`, as text, once its inputs are checked to be
    /// `inputs`.
    fn reference_lines(section: &str, inputs: &[serde_json::Value]) -> Vec<String> {
        reference_outputs(FIXTURE, section, inputs)
            .into_iter()
            .map(|o| o.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn stream_summary_reads_as_python_stream_reads() {
        let streams: [&[u8]; 7] = [
            b"{\"type\": \"thread.started\", \"thread_id\": \"a\"}\n\
              {\"type\": \"thread.started\", \"thread_id\": \"b\"}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"command_execution\"}}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"agent_message\", \"text\": \"one\"}}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"agent_message\", \"text\": \"two\"}}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"agent_message\", \"text\": 3}}\n",
            // Not JSON, not an object, the wrong types, a line still being written.
            b"not json\n[1, 2]\n\"text\"\n{\"type\": 5}\n{\"type\": \"thread.started\", \"thread_id\": 7}\n\
              {\"type\": \"thread.started\", \"thread_id\": \"late\"}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"file_change\"}}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"file_change\"}",
            // Repeated keys: the last wins.
            b"{\"type\": \"item.completed\", \"type\": \"turn.started\", \"item\": {\"type\": \"file_change\"}}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"file_change\"}, \"item\": 5}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"x\", \"type\": \"command_execution\"}}\n",
            // Errors in order; turn.failed needs an object; bad UTF-8 is replaced.
            b"{\"type\": \"error\", \"message\": \"one \xff\"}\n\
              {\"type\": \"turn.failed\", \"error\": \"flat\"}\n\
              {\"type\": \"turn.failed\", \"error\": {\"message\": \"two\"}}\n\
              {\"type\": \"error\", \"message\": {\"text\": \"x\"}}\n\
              {\"type\": \"turn.failed\", \"message\": \"not here\", \"error\": {\"message\": 5}}\n\
              {\"type\": \"turn.started\"}\n",
            // A lone surrogate in a message is kept.
            b"{\"type\": \"error\", \"message\": \"lone \\ud800 one\"}\n\
              {\"type\": \"item.completed\", \"item\": {\"type\": \"agent_message\", \"text\": \"caf\\u00e9 \\ud83d\\ude00\"}}\n",
            b"",
            b"\n\n   \n{\"type\": \"turn.started\"}\r\n{\"type\": \"turn.completed\"}\n",
        ];
        // Python's `_Stream` over each (a stream file), as JSON: steps, thread_id, turn_open,
        // last_agent_message, error_texts. The inputs are the streams' bytes in hex.
        let hex: Vec<serde_json::Value> = streams.iter().map(|b| hex(b).into()).collect();
        let theirs = reference_lines("stream", &hex);
        for (bytes, want) in streams.into_iter().zip(theirs) {
            let ours = StreamSummary::of(bytes);
            let got = crate::wire::to_output(&(
                ours.steps,
                &ours.thread_id,
                ours.turn_open,
                &ours.last_agent_message,
                &ours.errors,
            ))
            .unwrap();
            assert_eq!(got, want.trim_end(), "{}", String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn thread_id_that_is_not_text_is_no_thread_id() {
        // A lone surrogate (which no Codex thread ID has) is no thread ID, as a non-string one
        // is; a later one that is text is taken.
        let summary = StreamSummary::of(
            b"{\"type\": \"thread.started\", \"thread_id\": \"t\\ud800\"}\n\
              {\"type\": \"thread.started\", \"thread_id\": \"t\\udcff\"}\n\
              {\"type\": \"thread.started\", \"thread_id\": \"0f0f-thread\"}\n",
        );
        assert_eq!(summary.thread_id.as_deref(), Some("0f0f-thread"));
        let none =
            StreamSummary::of(b"{\"type\": \"thread.started\", \"thread_id\": \"\\ud800\"}\n");
        assert_eq!(none.thread_id, None);
        // An empty one is a thread ID, as the Python keeps it (and charges it as unknown).
        let empty = StreamSummary::of(
            b"{\"type\": \"thread.started\", \"thread_id\": \"\"}\n\
              {\"type\": \"thread.started\", \"thread_id\": \"later\"}\n",
        );
        assert_eq!(empty.thread_id.as_deref(), Some(""));
    }

    /// Python's `dispatch.has_status_line`, `_USAGE_LIMIT.search` and `_classify` over
    /// `texts`: one JSON line each, `[has_status_line or "KeyError", usage, classify]`, where
    /// `classify` is `_classify(None, 0, text, [], "")[:4]` or `"KeyError"`: recorded for
    /// `texts`.
    fn reference_reply_checks(texts: &[&str]) -> Vec<String> {
        let inputs: Vec<serde_json::Value> = texts.iter().map(|t| (*t).into()).collect();
        reference_lines("reply_checks", &inputs)
    }

    #[test]
    fn reply_checks_match_python() {
        let texts = [
            "",
            "   \n\t\n",
            "Status: READY\nCommits: none\n",
            "**Status:** BLOCKED\nBlocked by: usage limit\n",
            "Status: blocked (no reason)\n",
            "> ## *Status*:`blocked`\nmore\n",
            "Status : Blocked\n",
            "Status: blockedx\n",
            "Status: blocked_\n",
            "Status: blocked2\n",
            "Status: blocked\u{e9}\n",
            "Status: blocked-now\n",
            "statuſ: blocked\n",
            "STATUS:BLOCKED",
            "Status: \u{212a}\n",
            "Status: bloc\u{212a}ed\n",
            "\n\nStatus: QUESTION\n",
            "Verdict: clean\n",
            "Result: fail\n",
            "re\u{17f}ult: pass\n",
            "verd\u{130}ct: clean\n",
            "I looked around and stopped.\nNothing else to say.\n",
            "\u{2028}Status: READY",
            "usage limit",
            "USAGE_LIMIT reached",
            "usage - _ limit",
            "u\u{17f}age\u{a0}lim\u{131}t",
            "usagelimit",
            "usage. limit",
            "usage\nlimit",
            "the usage\u{2003}LIMIT",
        ];
        let theirs = reference_reply_checks(&texts);
        assert_eq!(theirs.len(), texts.len());
        for (text, want) in texts.iter().zip(&theirs) {
            let has = match has_status_line(text) {
                Ok(b) => serde_json::to_string(&b).unwrap(),
                Err(_) => "\"KeyError\"".into(),
            };
            let classified = match classify(None, Some(0), text, &[], "") {
                Ok(c) => format!(
                    "[\"{}\", \"{}\", \"{}\", {}]",
                    c.status.as_str(),
                    c.blocked_by.as_str(),
                    c.exit_reason.as_str(),
                    c.keep_raw
                ),
                Err(_) => "\"KeyError\"".into(),
            };
            let ours = format!("[{has}, {}, {classified}]", mentions_usage_limit(text));
            assert_eq!(&ours, want, "{text:?}");
        }
    }

    /// `_classify`'s arguments: stopped_for, codex_exit, reply, errors, stderr.
    type ClassifyCase<'a> = (Option<&'a str>, Option<i32>, &'a str, Vec<&'a str>, &'a str);

    #[test]
    fn classify_matches_python_across_exits_and_stops() {
        let cases: Vec<ClassifyCase> = vec![
            (None, Some(0), "", vec![], ""),
            (None, Some(1), "", vec![], ""),
            (None, None, "", vec![], ""),
            (None, Some(-9), "", vec![], ""),
            (None, Some(-9), "Status: READY\n", vec![], ""),
            (None, Some(1), "Status: QUESTION\n", vec![], ""),
            (None, Some(1), "no status\n", vec![], ""),
            (None, Some(1), "Status: READY\n", vec!["usage limit"], ""),
            (None, Some(0), "no status\n", vec!["usage limit"], ""),
            (
                None,
                Some(0),
                "Status: READY\n",
                vec!["usage limit"],
                "usage limit",
            ),
            (
                None,
                Some(1),
                "re\u{17f}ult: pass\n",
                vec!["usage limit"],
                "",
            ),
            (None, Some(1), "re\u{17f}ult: pass\n", vec![], ""),
            (
                Some("cancelled"),
                Some(-2),
                "re\u{17f}ult: pass\n",
                vec![],
                "",
            ),
            (Some("time_limit"), None, "", vec![], ""),
            (Some("interrupted"), None, "", vec![], ""),
            (Some("error"), Some(0), "Status: READY\n", vec![], ""),
            (
                None,
                Some(0),
                "Status: BLOCKED\nBlocked by: environment\n",
                vec![],
                "",
            ),
            (None, Some(0), "Verdict: defects\n", vec![], ""),
        ];
        // The reference's `_classify(*case)` of each, as JSON (or `"KeyError"`).
        let inputs: Vec<serde_json::Value> = cases
            .iter()
            .map(|c| serde_json::to_value(c).unwrap())
            .collect();
        let theirs = reference_lines("classify", &inputs);
        let reason = |s: &str| match s {
            "cancelled" => StopReason::Cancelled,
            "step_limit" => StopReason::StepLimit,
            "time_limit" => StopReason::TimeLimit,
            "error" => StopReason::Error,
            "interrupted" => StopReason::Interrupted,
            other => panic!("{other}"),
        };
        for ((stopped, exit, reply, errors, stderr), want) in cases.iter().zip(&theirs) {
            let errors: Vec<PyStr> = errors.iter().map(|e| PyStr::from(*e)).collect();
            let ours = match classify(stopped.map(reason), *exit, reply, &errors, stderr) {
                Ok(c) => format!(
                    "[\"{}\", \"{}\", \"{}\", {}]",
                    c.status.as_str(),
                    c.blocked_by.as_str(),
                    c.exit_reason.as_str(),
                    c.keep_raw
                ),
                Err(_) => "\"KeyError\"".into(),
            };
            assert_eq!(&ours, want, "{stopped:?} {exit:?} {reply:?}");
        }
    }

    #[test]
    fn return_code_is_popen_returncode() {
        assert_eq!(return_code(ExitStatus::from_raw(0)), 0);
        assert_eq!(return_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(return_code(ExitStatus::from_raw(9)), -9);
        assert_eq!(return_code(ExitStatus::from_raw(15)), -15);
    }
}
