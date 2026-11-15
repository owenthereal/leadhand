//! The operator's cancel and pause, and what follows a card worker that died with an earlier
//! supervisor session (`engine.py`: `_cancel_step`, `_pause_step`, `_after_interrupted`,
//! `_branch_progress`, `_tip_before`, `_continue_notes`). Pure, as the rest of `decide`.
//! Reconciling a relaunched run, which records the interrupted results these act on, is the
//! shell's (`engine::recover`); it judges an interrupted worker's branch with the same
//! `branch_progress`.

use super::{
    Decision, Ending, Need, Observations, Request, RunState, Step, in_flight, integrate_origin,
    need, request,
};
use crate::engine::cards::{
    Dispatch, card_branch, card_state, final_branch, heads, is_card, short_sha,
};
use crate::engine::protocol::{Action, IMPLEMENTER_KINDS, StopWorkerAction, continue_notes, tag};
use crate::state::events::given;
use crate::state::{Blocked, CardState, Event, fold_cards};
use crate::wire::PyStr;

/// The cancel's step (`_cancel_step`): every running worker is stopped, then the run ends
/// incomplete (cancelled). In request order, each dispatch without a result: a Codex one is
/// cancelled through the dispatcher (termination confirmed), its result recorded when it had a
/// worker; a Claude worker of this session gets `stop_worker`; one that died with an earlier
/// session needs no stop and is recorded as interrupted. The outcome comes only after every
/// worker is stopped. Partial work stays on the card branches: a Codex runner commits it
/// (R39), and a stopped or dead Claude worker's is committed as its result is recorded (R42).
pub(super) fn cancel_step(st: &RunState) -> Step<Decision> {
    let mut dispatches: Vec<Dispatch> = st.log.dispatches().collect();
    dispatches.sort_by_key(Dispatch::seq);
    // Codex dispatches that never started a worker: cancelled too (a late proxy then starts
    // nothing), and the loop goes on.
    let mut codex = Vec::new();
    let mut then = None;
    for d in &dispatches {
        if d.result.is_some() {
            continue;
        }
        let id = d.id().to_string();
        if d.vendor() == "codex" {
            if d.started.is_none() {
                codex.push(id);
                continue;
            }
            then = Some(Decision::StopCodex {
                dispatch_id: id,
                then: None,
            });
            break;
        }
        let Some(started) = d.started else {
            continue;
        };
        if st.log.gone(d) {
            let reconciled = given(&started.reconciled).copied().unwrap_or(false);
            then = Some(Decision::StoreInterrupted {
                dispatch_id: id,
                charge: !reconciled,
            });
            break;
        }
        then = Some(Decision::Act(Action::StopWorker(StopWorkerAction {
            action: tag::StopWorker,
            dispatch_id: id,
            worker_id: started.worker_id.clone(),
            vendor: "claude",
            record_result: true,
            reason: Some("the run is cancelled".to_string()),
        })));
        break;
    }
    let then = then.unwrap_or_else(|| {
        Decision::End(Ending {
            cancelled: true,
            ..Ending::default()
        })
    });
    Ok(if codex.is_empty() {
        then
    } else {
        Decision::Cancel {
            codex,
            then: Box::new(then),
        }
    })
}

/// The pause's step (`_pause_step`): running workers finish (`wait`, or `stop_worker` at
/// their deadline) and nothing new starts; then the run ends blocked (paused), with the
/// handoff. A dispatch whose worker never started holds nothing up.
pub(super) fn pause_step(st: &RunState, obs: &Observations) -> Step<Decision> {
    if let Some(d) = st
        .log
        .open_dispatches(&st.cards)
        .into_iter()
        .find(|d| d.started.is_some())
    {
        return in_flight(st, obs, &d);
    }
    Ok(Decision::End(Ending {
        blocked: Some(Blocked::Paused),
        ..Ending::default()
    }))
}

/// A card dispatch whose worker died with an earlier session and left no report
/// (`_after_interrupted`): a check runs again with a fresh worker; implementer-side work
/// continues from its commits (resuming the Codex session when its ID is known) or, when the
/// branch did not move, is requested again as the same attempt under a new dispatch ID. A
/// branch that matches neither fails the run: nothing is guessed.
pub(super) fn after_interrupted(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    d: &Dispatch,
) -> Step<Decision> {
    let again = || {
        let mut r = Request::new(c.number, d.kind(), d.attempt(), d.role(), d.route());
        r.extra = d.extra();
        r.notes = d.notes();
        r
    };
    if !IMPLEMENTER_KINDS.contains(&d.kind()) {
        return request(st, obs, c, again());
    }
    let mut ref_sha = |name: PyStr| match obs.ref_shas.get(&name) {
        Some(sha) => Ok(sha.clone()),
        None => need(Need::RefSha(name)),
    };
    let mut contains = |start: String, tip: String| {
        let key = (PyStr::from(start), tip);
        match obs.ancestors.get(&key) {
            Some(yes) => Ok(*yes),
            None => need(Need::Ancestor(key.0, key.1)),
        }
    };
    match branch_progress(st, d, &mut ref_sha, &mut contains)? {
        Progress::Commits => {
            let mut r = Request::new(c.number, "continue", d.attempt(), d.role(), d.route());
            r.resume = Some(d.id().to_string());
            r.notes = interrupted_notes(&PyStr::default(), d);
            r.continues = Some(d.id().to_string());
            request(st, obs, c, r)
        }
        Progress::Nothing => {
            let mut r = again();
            // `index.get(req.get("resumes") or "")`: the shell resumes only a dispatch the
            // log has.
            r.resume = d.req.resumes.clone();
            r.continues = given(&d.req.continues).cloned();
            request(st, obs, c, r)
        }
        Progress::Mismatch(why) => Ok(Decision::End(Ending {
            unreconciled: Some(why),
            stop_codex: true,
            ..Ending::default()
        })),
    }
}

/// What an interrupted implementer-side dispatch left on its branch (`_branch_progress`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The branch moved on from where the dispatch started (an integrate step may also have
    /// rewritten it).
    Commits,
    /// It did not move.
    Nothing,
    /// Why the branch cannot be matched: missing, or rewritten.
    Mismatch(String),
}

/// `_branch_progress` for the dispatch `d` of the run `st` folds: the card's branch (a fix
/// wave's, the final branch), its tip from `ref_sha` (`None` when the ref is missing) and,
/// only when the tip moved, whether the tip the dispatch started from is in its history
/// (`contains(start, tip)`). The two look-ups are the caller's: observations in `decide`, Git
/// in reconciliation.
pub fn branch_progress(
    st: &RunState,
    d: &Dispatch,
    ref_sha: &mut dyn FnMut(PyStr) -> Step<Option<String>>,
    contains: &mut dyn FnMut(String, String) -> Step<bool>,
) -> Step<Progress> {
    let (branch, who) = match &d.req.card {
        None => (final_branch(st.run), "the final fix".to_string()),
        Some(card) => {
            let c = card_state(&st.cards, Some(card))?;
            (
                card_branch(st.run, c),
                format!("card {}", card.text().lossy()),
            )
        }
    };
    let Some(tip) = ref_sha(heads(&branch))? else {
        return Ok(Progress::Mismatch(format!(
            "{who}'s branch {} is missing",
            branch.shown()
        )));
    };
    let Some(start) = tip_before(st, d).filter(|s| !s.is_empty() && *s != tip) else {
        return Ok(Progress::Nothing);
    };
    if contains(start.clone(), tip.clone())? || integrate_origin(st, Some(*d)).is_some() {
        return Ok(Progress::Commits);
    }
    Ok(Progress::Mismatch(format!(
        "{who}'s branch {} was rewritten while {} was interrupted (from {} to {})",
        branch.shown(),
        d.id(),
        short_sha(&start),
        short_sha(&tip)
    )))
}

/// The branch tip a dispatch started from (`_tip_before`): recorded as `start_tip`, else (an
/// older log) the card's last known tip or base before the dispatch, or a fix wave's base.
fn tip_before(st: &RunState, d: &Dispatch) -> Option<String> {
    if let Some(tip) = given(&d.req.start_tip).filter(|t| !t.is_empty()) {
        return Some(tip.clone());
    }
    let Some(card) = &d.req.card else {
        return d
            .extra()
            .get(&PyStr::from("base"))
            .cloned()
            .flatten()
            .map(|b| b.lossy().into_owned());
    };
    let before: Vec<Event> = st
        .log
        .events
        .iter()
        .filter(|e| e.seq() < d.seq())
        .cloned()
        .collect();
    let folded = fold_cards(&before, &st.run.numbers());
    let c = folded
        .iter()
        .find(|(n, _)| is_card(Some(card), **n))
        .map(|(_, c)| c)?;
    c.tip
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| c.base.clone())
}

/// The notes for the step that carries on from the interrupted dispatch `d` (`_continue_notes`,
/// added once), naming the run-folder copy of its brief, else the brief it was given.
pub fn interrupted_notes(notes: &PyStr, d: &Dispatch) -> PyStr {
    let copy = d
        .result
        .and_then(|r| r.files.as_ref())
        .and_then(|f| f.get(&PyStr::from("brief")))
        .filter(|b| !b.is_empty())
        .cloned()
        .or_else(|| d.req.brief.clone())
        .unwrap_or_default();
    continue_notes(notes, d.kind(), d.id(), &copy)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{log, obs, world};
    use super::super::{Decision, Ending, Need, Observations, Request, RunState, decide};
    use crate::engine::protocol::{Action, CONTINUE_NOTE};
    use crate::state::Blocked;
    use crate::wire::PyStr;

    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const CARD_REF: &str = "refs/heads/leadhand/demo--card-1";

    fn cut() -> (&'static str, String) {
        (
            "card_cut",
            format!(r#""card":1,"branch":"leadhand/demo--card-1","base":"{B}""#),
        )
    }

    /// A card 1 `dispatch_requested`; `start_tip` names where an implementer started.
    fn req(id: &str, kind: &str, vendor: &str, start_tip: Option<&str>) -> (&'static str, String) {
        let tip = start_tip.map_or(String::new(), |t| format!(r#","start_tip":"{t}""#));
        (
            "dispatch_requested",
            format!(
                r#""dispatch_id":"{id}","card":1,"attempt":1,"dispatch_kind":"{kind}","role":"implementer","vendor":"{vendor}","model":"sonnet","effort":"medium","brief":"/ws/.leadhand/1/{id}/brief.md","report":"/ws/.leadhand/1/{id}/report.md","extra":{{"diff_range":"{B}..{C}"}},"notes":"keep it small","resumes":null,"resume_worker":null,"resume_session":null,"minutes":60{tip}"#
            ),
        )
    }

    fn started(id: &str) -> (&'static str, String) {
        (
            "worker_started",
            format!(
                r#""dispatch_id":"{id}","worker_id":"agent0001","card":1,"deadline":"2099-01-01T00:00:00Z","session":null"#
            ),
        )
    }

    /// The interrupted result reconciliation records for a worker that left no report.
    fn interrupted(id: &str, kind: &str) -> (&'static str, String) {
        (
            "result_recorded",
            format!(
                r#""dispatch_id":"{id}","card":1,"attempt":1,"dispatch_kind":"{kind}","worker_id":"agent0001","tip":"{C}","session_id":null,"files":{{"brief":"/run/cards/1/{id}/brief.md","reply":"/run/cards/1/{id}/reply.md"}},"stopped":false,"charge_ids":[],"reconciled":true,"status":"SPENT","blocked_by":"","head":"Status","verdict":null,"result":null,"source":"interrupted","raw_kept":false,"interrupted":true"#
            ),
        )
    }

    fn state<'r>(run: &'r crate::engine::cards::Run, lines: &[(&str, String)]) -> RunState<'r> {
        let lines: Vec<(&str, &str)> = lines.iter().map(|(k, f)| (*k, f.as_str())).collect();
        RunState::new(run, log(&lines))
    }

    /// Decides, answering the branch observations: the card branch at `tip` (`None`: missing),
    /// and whether the start tip is in its history.
    fn decide_with(st: &RunState, tip: Option<&str>, contains: bool) -> Decision {
        let mut o = obs(&["claude"]);
        loop {
            match decide(st, &o) {
                Decision::Need(Need::RefSha(name)) => {
                    assert_eq!(name, PyStr::from(CARD_REF));
                    o.ref_shas.insert(name, tip.map(str::to_string));
                }
                Decision::Need(Need::Ancestor(start, at)) => {
                    o.ancestors.insert((start, at), contains);
                }
                d => return d,
            }
        }
    }

    fn requested(d: Decision) -> Box<Request> {
        match d {
            Decision::Request(r) => r,
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn the_cancel_stops_every_worker_before_the_outcome() {
        let w = world("");
        let mut lines = vec![
            ("run_started", String::new()),
            cut(),
            req("c1-a1-implement-1", "implement", "claude", Some(B)),
            started("c1-a1-implement-1"),
        ];
        // The flag is recorded first, and removed; also once `cancelled` is recorded.
        let mut o = obs(&["claude"]);
        o.cancel_flag = true;
        assert_eq!(decide(&state(&w.run, &lines), &o), Decision::RecordCancel);
        lines.push(("cancelled", r#""by":"operator""#.into()));
        assert_eq!(decide(&state(&w.run, &lines), &o), Decision::RecordCancel);
        // A worker of this session is stopped, its result recorded with --stopped.
        let o = obs(&["claude"]);
        match decide(&state(&w.run, &lines), &o) {
            Decision::Act(Action::StopWorker(s)) => assert_eq!(
                (
                    s.dispatch_id.as_str(),
                    s.vendor,
                    s.record_result,
                    s.reason.as_deref()
                ),
                (
                    "c1-a1-implement-1",
                    "claude",
                    true,
                    Some("the run is cancelled")
                )
            ),
            d => panic!("{d:?}"),
        }
        // A Codex dispatch requested before it, never started, is cancelled first.
        let mut with_codex = lines.clone();
        with_codex.insert(2, req("c1-a1-implement-0", "implement", "codex", Some(B)));
        match decide(&state(&w.run, &with_codex), &o) {
            Decision::Cancel { codex, then } => {
                assert_eq!(codex, ["c1-a1-implement-0"]);
                assert!(matches!(*then, Decision::Act(Action::StopWorker(_))));
            }
            d => panic!("{d:?}"),
        }
        // One that died with an earlier session needs no stop: it is recorded as interrupted,
        // and charged (its start was recorded).
        let mut relaunched = lines.clone();
        relaunched.insert(4, ("resumed", r#""run_id":"abc123abc123""#.into()));
        relaunched.insert(
            5,
            (
                "reconciled",
                r#""resumed_seq":5,"found":[],"headline":"Resumed""#.into(),
            ),
        );
        assert_eq!(
            decide(&state(&w.run, &relaunched), &o),
            Decision::StoreInterrupted {
                dispatch_id: "c1-a1-implement-1".into(),
                charge: true
            }
        );
        // Every worker stopped: the run ends cancelled.
        lines.push(interrupted("c1-a1-implement-1", "implement"));
        assert_eq!(
            decide(&state(&w.run, &lines), &o),
            Decision::End(Ending {
                cancelled: true,
                ..Ending::default()
            })
        );
    }

    #[test]
    fn the_pause_lets_running_workers_finish_then_ends_blocked() {
        let w = world("");
        let mut lines = vec![
            ("run_started", String::new()),
            cut(),
            req("c1-a1-implement-1", "implement", "claude", Some(B)),
        ];
        let mut o = obs(&["claude"]);
        o.pause_flag = true;
        assert_eq!(decide(&state(&w.run, &lines), &o), Decision::RecordPause);
        lines.push(("paused", r#""by":"operator""#.into()));
        let o = obs(&["claude"]);
        // A dispatch whose worker never started holds nothing up; nothing new starts.
        let paused = Decision::End(Ending {
            blocked: Some(Blocked::Paused),
            ..Ending::default()
        });
        assert_eq!(decide(&state(&w.run, &lines), &o), paused);
        // A running worker is waited for.
        let mut running = lines.clone();
        running.insert(3, started("c1-a1-implement-1"));
        assert!(matches!(
            decide(&state(&w.run, &running), &o),
            Decision::Act(Action::Wait(_))
        ));
        // A relaunch ends the pause: the run goes on, from its reconciliation.
        running.push(("resumed", r#""run_id":"abc123abc123""#.into()));
        assert_eq!(decide(&state(&w.run, &running), &o), Decision::Reconcile);
    }

    #[test]
    fn an_interrupted_check_runs_again_with_a_fresh_worker() {
        let w = world("");
        let lines = vec![
            ("run_started", String::new()),
            cut(),
            req("c1-a1-review-1", "review", "claude", None),
            started("c1-a1-review-1"),
            interrupted("c1-a1-review-1", "review"),
        ];
        let r = requested(decide_with(&state(&w.run, &lines), Some(C), true));
        assert_eq!(
            (r.dispatch_id.as_str(), r.kind.as_str(), r.attempt, r.resume),
            ("c1-a1-review-2", "review", 1, None)
        );
        assert_eq!(r.notes, PyStr::from("keep it small"));
        assert_eq!(
            r.extra.get(&"diff_range".into()).cloned().flatten(),
            Some(PyStr::from(format!("{B}..{C}")))
        );
    }

    #[test]
    fn an_interrupted_implementer_continues_from_its_commits_or_runs_again() {
        let w = world("");
        let lines = vec![
            ("run_started", String::new()),
            cut(),
            req("c1-a1-implement-1", "implement", "claude", Some(B)),
            started("c1-a1-implement-1"),
            interrupted("c1-a1-implement-1", "implement"),
        ];
        let st = state(&w.run, &lines);
        // Its branch moved on from where it started: a `continue` carries on from it.
        let r = requested(decide_with(&st, Some(C), true));
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.resume.as_deref(),
                r.continues.as_deref()
            ),
            (
                "c1-a1-continue-1",
                "continue",
                Some("c1-a1-implement-1"),
                Some("c1-a1-implement-1")
            )
        );
        assert!(r.extra.0.is_empty());
        let notes = r.notes.lossy().into_owned();
        assert!(notes.starts_with(CONTINUE_NOTE), "{notes}");
        assert!(
            notes.contains("Its brief: /run/cards/1/c1-a1-implement-1/brief.md."),
            "{notes}"
        );
        // It did not move: the same attempt again, under a new dispatch ID.
        let r = requested(decide_with(&st, Some(B), true));
        assert_eq!(
            (r.dispatch_id.as_str(), r.kind.as_str(), r.attempt),
            ("c1-a1-implement-2", "implement", 1)
        );
        assert_eq!(r.notes, PyStr::from("keep it small"));
        // A branch that matches neither fails the run, after the Codex runners are stopped.
        let mismatch = |why: &str| {
            Decision::End(Ending {
                unreconciled: Some(why.to_string()),
                stop_codex: true,
                ..Ending::default()
            })
        };
        assert_eq!(
            decide_with(&st, None, true),
            mismatch("card 1's branch leadhand/demo--card-1 is missing")
        );
        assert_eq!(
            decide_with(&st, Some(C), false),
            mismatch(
                "card 1's branch leadhand/demo--card-1 was rewritten while c1-a1-implement-1 \
                 was interrupted (from bbbbbbbbbbbb to cccccccccccc)"
            )
        );
    }

    #[test]
    fn an_older_log_without_a_start_tip_starts_from_the_cards_base() {
        let w = world("");
        let lines = vec![
            ("run_started", String::new()),
            cut(),
            req("c1-a1-implement-1", "implement", "claude", None),
            started("c1-a1-implement-1"),
            interrupted("c1-a1-implement-1", "implement"),
        ];
        let st = state(&w.run, &lines);
        let mut o: Observations = obs(&["claude"]);
        o.ref_shas.insert(CARD_REF.into(), Some(C.into()));
        assert_eq!(
            decide(&st, &o),
            Decision::Need(Need::Ancestor(B.into(), C.into()))
        );
        // At the base: nothing moved.
        o.ref_shas.insert(CARD_REF.into(), Some(B.into()));
        assert_eq!(requested(decide(&st, &o)).dispatch_id, "c1-a1-implement-2");
    }
}
