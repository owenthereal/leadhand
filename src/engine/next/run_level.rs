//! What follows the latest run-level dispatch's result (`engine._after_run_result`): a gate's
//! result recorded, a fix wave's implementer reviewed or its wave spent, a fix review's verdict
//! recorded and a clean fix accepted onto the run branch. A run-level check that changed what
//! it guards is voided and run again with a fresh worker (R33), its fix wave re-cut from the run
//! head first (R36), or ends the run blocked past the violation cap; a worker that ended without
//! a usable report is dispatched again, once. Pure, as the rest of `decide`.

use super::gates::{resolve_run, run_head, run_request};
use super::{Decision, Ending, Need, Observations, RunState, Step, fail, need};
use crate::deck::Card;
use crate::engine::cards::{
    Dispatch, accepted_cards, final_branch, final_card, is_final, report_copy,
};
use crate::engine::protocol::{VIOLATION_CAP, continue_notes, voided_notes};
use crate::state::events::{self, given};
use crate::state::{Blocked, Event};
use crate::wire::{PyMap, PyStr};

/// Acts on the run-level dispatch `d`'s result: the step, or `None` when the result is
/// settled and the gate loop decides what is next.
pub(super) fn after_run_result(
    st: &RunState,
    obs: &Observations,
    d: &Dispatch,
) -> Step<Option<Decision>> {
    let r = d.result.expect("after_run_result is called with a result");
    let req = d.req;
    let accepted = accepted_cards(st.run, &st.cards)?;
    let interrupted = given(&r.interrupted).copied().unwrap_or(false);
    if given(&req.gate).is_some_and(|g| !g.is_empty()) {
        let recorded =
            st.log.events.iter().any(
                |e| matches!(e, Event::GateResult(g) if g.dispatch_id.as_deref() == Some(d.id())),
            );
        if recorded {
            return Ok(None);
        }
        if req.refs.is_some() && voided(st, obs, d)? {
            return after_void(st, d, &accepted).map(Some);
        }
        if interrupted {
            // The gate runs again.
            return Ok(Some(rerun(st, d, d.notes(), &accepted)));
        }
        if !usable(d) {
            return worker_failed(st, d, &accepted).map(Some);
        }
        let passed = r.verdict.as_deref() == Some("clean") || r.result.as_deref() == Some("pass");
        let Some(examined) = given(&req.expected_head).cloned() else {
            return fail("KeyError: 'expected_head'".into());
        };
        return Ok(Some(Decision::RecordGate(Box::new(Event::GateResult(
            events::GateResult {
                gate: given(&req.gate).cloned(),
                examined_sha: Some(examined),
                passed: Some(passed),
                dispatch_id: Some(d.id().to_string()),
                findings: Some(report_copy(d)),
                ..events::GateResult::default()
            },
        )))));
    }
    let wave = given(&req.wave).copied();
    if d.kind() == "final_fix" {
        if verdict_of(st, d).is_some() {
            // Settled as spent: never checked again.
            return Ok(None);
        }
        if req.refs.is_some() && voided(st, obs, d)? {
            return after_void(st, d, &accepted).map(Some);
        }
        if interrupted {
            // Its worker died with an earlier session: the wave's implementer runs again on
            // the final branch as it stands (the fix review covers every commit since the
            // wave's base).
            let copy = r
                .files
                .as_ref()
                .and_then(|f| f.get(&PyStr::from("brief")))
                .filter(|b| !b.is_empty())
                .cloned()
                .or_else(|| req.brief.clone())
                .unwrap_or_default();
            let notes = continue_notes(&d.notes(), d.kind(), d.id(), &copy);
            return Ok(Some(rerun(st, d, notes, &accepted)));
        }
        return match r.status.as_deref() {
            Some("READY") => fix_ready(st, obs, d, wave, &accepted).map(Some),
            Some("BLOCKED") => worker_failed(st, d, &accepted).map(Some),
            status => Ok(Some(settle_spent(
                d,
                format!("ended {}", status.unwrap_or("None")).into(),
            ))),
        };
    }
    // The review of a fix wave.
    let Some(verdict) = verdict_of(st, d) else {
        if req.refs.is_some() && voided(st, obs, d)? {
            return after_void(st, d, &accepted).map(Some);
        }
        if interrupted {
            return Ok(Some(rerun(st, d, d.notes(), &accepted)));
        }
        if !usable(d) {
            return worker_failed(st, d, &accepted).map(Some);
        }
        return Ok(Some(super::append(Event::VerdictRecorded(
            events::VerdictRecorded {
                card: Some(final_card()),
                wave: Some(wave),
                verdict: r.verdict.clone(),
                findings: Some(report_copy(d)),
                dispatch_id: Some(d.id().to_string()),
                ..events::VerdictRecorded::default()
            },
        ))));
    };
    if verdict.verdict.as_deref() != Some("clean") {
        // Rejected: the wave is spent, the run head stays.
        return Ok(None);
    }
    let accepted_fix = st.log.events.iter().any(|e| {
        matches!(e, Event::Accepted(a)
            if is_final(a.card.as_ref()) && given(&a.dispatch_id).map(String::as_str) == Some(d.id()))
    });
    if accepted_fix {
        return Ok(None);
    }
    let Some(target) = given(&req.expected_head).cloned() else {
        return fail("KeyError: 'expected_head'".into());
    };
    let range = d
        .extra()
        .get(&PyStr::from("diff_range"))
        .cloned()
        .flatten()
        .unwrap_or_default();
    // `.partition("..")[2]`.
    let bytes = range.as_wtf8();
    let reviewed = bytes
        .windows(2)
        .position(|w| w == b"..")
        .map(|at| PyStr::from_wtf8(bytes[at + 2..].to_vec()).unwrap_or_default())
        .unwrap_or_default();
    if reviewed.as_wtf8() != target.as_bytes() {
        let mut why = PyStr::from(format!(
            "fix review {} examined {target}, not the fix tip ",
            d.id()
        ));
        why.push(&reviewed);
        return fail(why.shown());
    }
    let started = st.log.events.iter().any(|e| {
        matches!(e, Event::AcceptStarted(a) if given(&a.dispatch_id).map(String::as_str) == Some(d.id()))
    });
    if !started {
        let key = (st.run.run_ref(), target.clone());
        let Some(contains) = obs.ancestors.get(&key) else {
            return need(Need::Ancestor(key.0, key.1));
        };
        if !contains {
            // A card was accepted meanwhile: the wave is stale.
            return Ok(None);
        }
    }
    Ok(Some(Decision::AcceptFix {
        dispatch_id: d.id().to_string(),
        wave,
        target,
        started,
    }))
}

/// A fix wave's implementer reported READY: its fix is reviewed, unless it changed nothing or
/// the run head moved since its wave began, which spends the wave.
fn fix_ready(
    st: &RunState,
    obs: &Observations,
    d: &Dispatch,
    wave: Option<i64>,
    accepted: &[&Card],
) -> Step<Decision> {
    let extra = d.extra();
    let value = |key: &str| match extra.get(&PyStr::from(key)) {
        Some(v) => Ok(v.clone().unwrap_or_else(|| "None".into())),
        None => fail(format!("KeyError: '{key}'")),
    };
    let base = value("base")?;
    let tip = d
        .result
        .and_then(|r| r.tip.clone())
        .filter(|t| !t.is_empty());
    let Some(tip) = tip.filter(|t| t.as_bytes() != base.as_wtf8()) else {
        return Ok(settle_spent(d, "changed nothing".into()));
    };
    if base.as_wtf8() != run_head(st, obs)?.as_bytes() {
        return Ok(settle_spent(d, "the run head moved".into()));
    }
    let (role, route) = resolve_run(st, obs, "final_reviewer", accepted, false)?;
    let wave_text = wave.map_or_else(|| "None".to_string(), |w| w.to_string());
    let mut notes = PyStr::from(format!(
        "## Fix wave\n\nThis reviews fix wave {wave_text} for the failed \""
    ));
    notes.push(&value("gate")?);
    notes.push_str("\" gate, whose findings are in ");
    notes.push(&value("findings")?);
    notes.push_str(". The fix's report: ");
    notes.push(&report_copy(d));
    notes.push_str(". Review only the fix's diff ");
    notes.push(&base);
    notes.push_str(&format!(
        "..{tip}: it must fix the findings without undoing any card's \"Done when\" or reaching \
         outside the cards' allowed paths. `Verdict: clean` lets it onto the run branch."
    ));
    let mut range = base.clone();
    range.push_str(&format!("..{tip}"));
    let extra = PyMap(vec![
        ("diff_range".into(), Some(range)),
        ("base".into(), Some(base)),
        ("branch".into(), Some(final_branch(st.run))),
    ]);
    Ok(run_request(
        st,
        "final_review",
        None,
        wave,
        wave.unwrap_or_default(),
        role,
        route,
        extra,
        notes,
        accepted,
    ))
}

/// The `verdict_recorded` of dispatch `d`, the first.
fn verdict_of<'l>(st: &'l RunState, d: &Dispatch) -> Option<&'l events::VerdictRecorded> {
    st.log.events.iter().find_map(|e| match e {
        Event::VerdictRecorded(v) if v.dispatch_id.as_deref() == Some(d.id()) => Some(v),
        _ => None,
    })
}

/// Records that a fix wave's implementer result spent the wave (its guard was checked first),
/// as `verdict_recorded{card: "final", verdict: "spent"}`, so the result is acted on once:
/// later steps never check its guard again, whatever moves afterwards (`_settle_spent`, R37).
fn settle_spent(d: &Dispatch, why: PyStr) -> Decision {
    super::append(Event::VerdictRecorded(events::VerdictRecorded {
        card: Some(final_card()),
        wave: Some(given(&d.req.wave).copied()),
        verdict: Some("spent".into()),
        reason: Some(Some(why)),
        findings: Some(report_copy(d)),
        dispatch_id: Some(d.id().to_string()),
        ..events::VerdictRecorded::default()
    }))
}

/// Whether a run-level result can be acted on: a verdict, a pass or fail, or an implementer
/// status (`_usable`).
fn usable(d: &Dispatch) -> bool {
    let Some(r) = d.result else {
        return false;
    };
    match d.kind() {
        "final_review" | "security" => matches!(r.verdict.as_deref(), Some("clean" | "defects")),
        "evidence" | "verify" => matches!(r.result.as_deref(), Some("pass" | "fail")),
        _ => matches!(r.status.as_deref(), Some("READY" | "QUESTION" | "SPENT")),
    }
}

/// Whether the run-level check `d` changed what it guards (`_run_check_voided`'s `_void`, and
/// for evidence `_base_violation`): a change found now, or one recorded and not restored yet,
/// is recorded, kept and undone first (`Decision::Void`); then the result is void, `true`.
fn voided(st: &RunState, obs: &Observations, d: &Dispatch) -> Step<bool> {
    let mine: Vec<&events::Violation> = st
        .log
        .check_violations()
        .filter(|v| given(&v.dispatch_id).map(String::as_str) == Some(d.id()))
        .collect();
    let stage = |s: &str| mine.iter().any(|v| v.stage.as_deref() == Some(s));
    let void = |found: &events::Violation| {
        Box::new(Decision::Void {
            dispatch_id: d.id().to_string(),
            found: Some(Box::new(found.clone())),
        })
    };
    if mine.is_empty() {
        match obs.guards.get(d.id()) {
            None => return need(Need::Guard(d.id().to_string())),
            Some(Some(found)) => return Err(void(found)),
            Some(None) => {}
        }
        if d.kind() != "evidence" {
            return Ok(false);
        }
        return match obs.base_guards.get(d.id()) {
            None => need(Need::BaseGuard(d.id().to_string())),
            Some(Some(found)) => Err(void(found)),
            Some(None) => Ok(false),
        };
    }
    if !stage("found") {
        // `next(e for e in mine if e.get("stage") == "found")`.
        return fail("StopIteration".into());
    }
    if !stage("restored") {
        return Err(Box::new(Decision::Void {
            dispatch_id: d.id().to_string(),
            found: None,
        }));
    }
    Ok(true)
}

/// What follows a voided run-level result (`_run_check_voided`): the same dispatch again with a
/// fresh worker (R33), a fix wave's implementer from the run head (R36); past `VIOLATION_CAP`
/// voided runs of the kind since the run was last resumed, the run ends blocked for a decision
/// instead (there is no card to park).
fn after_void(st: &RunState, d: &Dispatch, accepted: &[&Card]) -> Step<Decision> {
    let since = st.log.reopened_since();
    let count: Vec<&events::Violation> = st
        .log
        .check_violations()
        .filter(|v| {
            v.stage.as_deref() == Some("found")
                && given(&v.card).is_none()
                && given(&v.dispatch_kind).map(String::as_str) == Some(d.kind())
                && v.seq > since
        })
        .collect();
    if count.len() > VIOLATION_CAP {
        // `sorted({...})`: WTF-8 order is code point order.
        let mut paths: Vec<PyStr> = count
            .iter()
            .flat_map(|v| given(&v.paths).into_iter().flatten())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        let mut refs: Vec<PyStr> = count
            .iter()
            .flat_map(|v| given(&v.refs).into_iter().flat_map(|r| r.0.iter()))
            .map(|(name, _)| name.clone())
            .collect();
        refs.sort();
        refs.dedup();
        let mut why = PyStr::from(format!("{} workers ", d.kind()));
        if paths.is_empty() {
            why.push_str("move refs: ");
            why.push(&PyStr::join(&refs, ", "));
        } else {
            why.push_str("change tracked files: ");
            why.push(&PyStr::join(&paths, ", "));
        }
        return Ok(Decision::End(Ending {
            blocked: Some(Blocked::DecisionNeeded),
            why: vec![why],
            ..Ending::default()
        }));
    }
    if d.kind() == "final_fix" {
        let recut = st.log.events.iter().any(|e| {
            matches!(e, Event::CardCut(c) if given(&c.recut_after).map(String::as_str) == Some(d.id()))
        });
        if !recut {
            return Ok(Decision::RecutFinal {
                dispatch_id: d.id().to_string(),
            });
        }
    }
    Ok(rerun(st, d, voided_notes(&d.notes()), accepted))
}

/// The same run-level dispatch again, on the same route with a fresh worker (`_rerun`).
fn rerun(st: &RunState, d: &Dispatch, notes: PyStr, accepted: &[&Card]) -> Decision {
    run_request(
        st,
        d.kind(),
        given(&d.req.gate).cloned(),
        given(&d.req.wave).copied(),
        d.attempt(),
        d.role().to_string(),
        d.route(),
        d.extra(),
        notes,
        accepted,
    )
}

/// A run-level worker ended BLOCKED or without a verdict or result (`_run_worker_failed`):
/// provider capacity ends the run blocked at once; otherwise it is dispatched again, and a
/// second such ending for the same step (since the run was last resumed) ends the run blocked
/// (environment).
fn worker_failed(st: &RunState, d: &Dispatch, accepted: &[&Card]) -> Step<Decision> {
    let r = d.result.expect("worker_failed is called with a result");
    let req = d.req;
    let ended_after = st
        .log
        .events
        .iter()
        .any(|e| matches!(e, Event::Outcome(o) if o.seq > r.seq));
    if !ended_after {
        if r.status.as_deref() == Some("BLOCKED") && r.blocked_by.as_deref() == Some("usage limit")
        {
            return Ok(Decision::End(Ending {
                blocked: Some(Blocked::ProviderCapacity),
                ..Ending::default()
            }));
        }
        let since = st.log.reopened_since();
        let voided = st.log.voided();
        let same = st
            .log
            .run_dispatches()
            .into_iter()
            .filter(|x| {
                x.kind() == d.kind()
                    && given(&x.req.wave) == given(&req.wave)
                    && given(&x.req.gate) == given(&req.gate)
                    && given(&x.req.expected_head) == given(&req.expected_head)
                    && x.result.is_some_and(|xr| {
                        xr.seq > since && !given(&xr.interrupted).copied().unwrap_or(false)
                    })
                    && !voided.contains(x.id())
                    && !usable(x)
            })
            .count();
        if same >= 2 {
            let status = r.status.as_deref().unwrap_or("None");
            return Ok(Decision::End(Ending {
                blocked: Some(Blocked::Environment),
                why: vec![
                    format!("{} ended {status} twice without a usable report", d.kind()).into(),
                ],
                ..Ending::default()
            }));
        }
    }
    Ok(rerun(st, d, d.notes(), accepted))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{log, obs, world};
    use super::super::{Decision, Ending, Need, Observations, RunState, decide};
    use crate::engine::cards::{Run, work_branch};
    use crate::state::{Blocked, Event};
    use crate::wire::PyStr;

    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddd";

    /// Card 1 cut and accepted at `C`.
    fn accepted() -> Vec<(&'static str, String)> {
        vec![
            ("run_started", String::new()),
            (
                "card_cut",
                format!(r#""card":1,"branch":"leadhand/demo--card-1","base":"{B}""#),
            ),
            ("accepted", format!(r#""card":1,"sha":"{C}""#)),
        ]
    }

    /// A run-level `dispatch_requested`, guarded as a check on the run branch at `C`.
    fn run_req(id: &str, kind: &str, gate: Option<&str>, wave: Option<i64>, extra: &str) -> String {
        let gate = gate.map_or("null".to_string(), |g| format!("\"{g}\""));
        let wave = wave.map_or("null".to_string(), |w| w.to_string());
        format!(
            r#""dispatch_id":"{id}","card":null,"attempt":1,"dispatch_kind":"{kind}","role":"verifier","vendor":"claude","model":"sonnet","effort":"medium","brief":"/ws/.leadhand/run/{id}/brief.md","report":"/ws/.leadhand/run/{id}/report.md","extra":{{{extra}}},"notes":"","resumes":null,"resume_worker":null,"resume_session":null,"minutes":60,"gate":{gate},"wave":{wave},"expected_branch":"leadhand/demo","expected_head":"{C}""#
        )
    }

    fn run_result(id: &str, kind: &str, fields: &str) -> String {
        format!(
            r#""dispatch_id":"{id}","card":null,"attempt":1,"dispatch_kind":"{kind}","worker_id":"w","tip":"{D}","session_id":null,"files":{{"report":"/run/cards/run/{id}/report.md"}},"stopped":false,"charge_ids":[],{fields},"blocked_by":"","source":"report","raw_kept":false"#
        )
    }

    fn gate_result(gate: &str, passed: bool, id: &str) -> (&'static str, String) {
        (
            "gate_result",
            format!(
                r#""gate":"{gate}","examined_sha":"{C}","passed":{passed},"dispatch_id":"{id}","findings":"/r/{gate}.md""#
            ),
        )
    }

    fn state<'r>(run: &'r Run, lines: &[(&str, String)]) -> RunState<'r> {
        let lines: Vec<(&str, &str)> = lines.iter().map(|(k, f)| (*k, f.as_str())).collect();
        RunState::new(run, log(&lines))
    }

    /// The gates' observations: the run head at `C`, one ordinary path changed since the base.
    fn end_obs() -> Observations {
        let mut o = obs(&["claude"]);
        o.card_loop_only = false;
        o.run_tip = Some(C.into());
        o.changed
            .insert((B.into(), C.into()), vec![PyStr::from("src/widget.py")]);
        o
    }

    fn requested(st: &RunState, o: &Observations) -> Box<super::super::RunRequest> {
        match decide(st, o) {
            Decision::RunRequest(r) => r,
            d => panic!("{d:?}"),
        }
    }

    fn ended(st: &RunState, o: &Observations) -> Ending {
        match decide(st, o) {
            Decision::End(e) => e,
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn the_gates_run_in_order_on_the_run_head_then_the_run_ends_with_them_current() {
        let w = world("");
        let mut lines = accepted();
        let st = state(&w.run, &lines);
        // Which paths changed decides the security gate, for a low-risk card.
        let mut o = end_obs();
        o.changed.clear();
        assert_eq!(
            decide(&st, &o),
            Decision::Need(Need::Changed(B.into(), C.into()))
        );
        let r = requested(&st, &end_obs());
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.gate.as_deref(),
                r.role.as_str()
            ),
            (
                "run-final_review-1",
                "final_review",
                Some("final_review"),
                "final_reviewer"
            )
        );
        assert_eq!(r.cards, [1]);
        assert_eq!(
            r.extra.get(&"diff_range".into()).cloned().flatten(),
            Some(PyStr::from(format!("{B}..{C}")))
        );
        lines.push(gate_result("final_review", true, "run-final_review-1"));
        let st = state(&w.run, &lines);
        let r = requested(&st, &end_obs());
        assert_eq!(
            (r.dispatch_id.as_str(), r.role.as_str()),
            ("run-evidence-1", "verifier")
        );
        let base = w.run.repo.join(".worktrees").join("run--base");
        assert_eq!(
            r.extra.get(&"base_worktree".into()).cloned().flatten(),
            Some(crate::pytext::path_text(&base))
        );
        lines.push(gate_result("evidence", true, "run-evidence-1"));
        lines.push(gate_result("verify", true, "run-verify-1"));
        let st = state(&w.run, &lines);
        let ending = ended(&st, &end_obs());
        assert!(ending.gates_current && !ending.open_findings && !ending.verification_failed);
        // A sensitive path makes the security gate required, before verify.
        let mut o = end_obs();
        o.changed
            .insert((B.into(), C.into()), vec![PyStr::from("src/auth/x.py")]);
        let r = requested(&st, &o);
        assert_eq!(
            (r.kind.as_str(), r.role.as_str()),
            ("security", "reviewer_hard")
        );
    }

    #[test]
    fn a_failing_gate_gets_a_fix_wave_on_the_final_branch() {
        let w = world("");
        let mut lines = accepted();
        lines.push(gate_result("final_review", false, "run-final_review-1"));
        let st = state(&w.run, &lines);
        assert_eq!(
            decide(&st, &end_obs()),
            Decision::CutFinal {
                wave: 1,
                head: C.into()
            }
        );
        lines.push((
            "card_cut",
            format!(r#""card":"final","branch":"leadhand/demo--card-final","base":"{C}","wave":1"#),
        ));
        let st = state(&w.run, &lines);
        let r = requested(&st, &end_obs());
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.wave,
                r.attempt,
                r.gate.as_deref()
            ),
            ("run-w1-final_fix-1", "final_fix", Some(1), 1, None)
        );
        let extra: Vec<(String, String)> = r
            .extra
            .0
            .iter()
            .map(|(k, v)| {
                let v = v.clone().unwrap_or_default();
                (k.lossy().into_owned(), v.lossy().into_owned())
            })
            .collect();
        assert_eq!(
            extra,
            [
                ("gate".to_string(), "final_review".to_string()),
                ("findings".into(), "/r/final_review.md".into()),
                ("branch".into(), "leadhand/demo--card-final".into()),
                ("base".into(), C.into()),
            ]
        );
    }

    #[test]
    fn an_evidence_gap_past_the_waves_ends_blocked_when_evidence_blocks() {
        let w = world("[run]\nevidence = \"block\"\n");
        let mut lines = accepted();
        lines.push(gate_result("final_review", true, "run-final_review-1"));
        for wave in [1, 2] {
            let id = format!("run-w{wave}-final_fix-1");
            lines.push((
                "dispatch_requested",
                run_req(&id, "final_fix", None, Some(wave), ""),
            ));
            lines.push((
                "result_recorded",
                run_result(&id, "final_fix", r#""status":"SPENT""#),
            ));
            lines.push((
                "verdict_recorded",
                format!(
                    r#""card":"final","wave":{wave},"verdict":"spent","reason":"ended SPENT","findings":"x","dispatch_id":"{id}""#
                ),
            ));
        }
        let ev = run_req("run-evidence-3", "evidence", Some("evidence"), None, "");
        lines.push(("dispatch_requested", ev));
        lines.push((
            "result_recorded",
            run_result("run-evidence-3", "evidence", r#""result":"fail""#),
        ));
        lines.push(gate_result("evidence", false, "run-evidence-3"));
        let st = state(&w.run, &lines);
        let ending = ended(&st, &end_obs());
        assert_eq!(ending.blocked, Some(Blocked::DecisionNeeded));
        assert_eq!(
            ending.why,
            [PyStr::from(
                "evidence gaps on cccccccccccc after 2 fix waves: /r/evidence.md"
            )]
        );
        // With `evidence = "record"`, the gap is a hold and the gates run on.
        let w = world("");
        let st = state(&w.run, &lines);
        assert_eq!(requested(&st, &end_obs()).kind, "verify");
    }

    #[test]
    fn a_gate_result_is_recorded_on_the_commit_it_examined() {
        let w = world("");
        let mut lines = accepted();
        let review = |id: &str| run_req(id, "final_review", Some("final_review"), None, "");
        lines.push(("dispatch_requested", review("run-final_review-1")));
        lines.push((
            "result_recorded",
            run_result(
                "run-final_review-1",
                "final_review",
                r#""verdict":"defects""#,
            ),
        ));
        let st = state(&w.run, &lines);
        let Decision::RecordGate(event) = decide(&st, &end_obs()) else {
            panic!("{:?}", decide(&st, &end_obs()));
        };
        let Event::GateResult(g) = *event else {
            panic!("{event:?}");
        };
        assert_eq!(
            (
                g.gate.as_deref(),
                g.examined_sha.as_deref(),
                g.passed,
                g.dispatch_id.as_deref()
            ),
            (
                Some("final_review"),
                Some(C),
                Some(false),
                Some("run-final_review-1")
            )
        );
        assert_eq!(
            g.findings,
            Some("/run/cards/run/run-final_review-1/report.md".into())
        );
        // A result without a verdict is dispatched again, once; the second ends the run.
        lines.truncate(3);
        let ready = |id: &str| run_result(id, "final_review", r#""status":"READY""#);
        lines.push(("dispatch_requested", review("run-final_review-1")));
        lines.push(("result_recorded", ready("run-final_review-1")));
        let st = state(&w.run, &lines);
        assert_eq!(requested(&st, &end_obs()).dispatch_id, "run-final_review-2");
        lines.push(("dispatch_requested", review("run-final_review-2")));
        lines.push(("result_recorded", ready("run-final_review-2")));
        let st = state(&w.run, &lines);
        let ending = ended(&st, &end_obs());
        assert_eq!(ending.blocked, Some(Blocked::Environment));
        assert_eq!(
            ending.why,
            [PyStr::from(
                "final_review ended READY twice without a usable report"
            )]
        );
    }

    #[test]
    fn a_fix_that_changed_nothing_spends_its_wave_and_a_clean_fix_is_accepted() {
        let w = world("");
        let fix = |base: &str| {
            let extra = format!(
                r#""gate":"final_review","findings":"/r/f.md","branch":"leadhand/demo--card-final","base":"{base}""#
            );
            run_req("run-w1-final_fix-1", "final_fix", None, Some(1), &extra)
        };
        let ready = run_result("run-w1-final_fix-1", "final_fix", r#""status":"READY""#);
        let mut lines = accepted();
        lines.push(("dispatch_requested", fix(D)));
        lines.push(("result_recorded", ready.clone()));
        let st = state(&w.run, &lines);
        let Decision::Append(event) = decide(&st, &end_obs()) else {
            panic!("{:?}", decide(&st, &end_obs()));
        };
        let Event::VerdictRecorded(v) = *event else {
            panic!("{event:?}");
        };
        assert_eq!(
            (v.verdict.as_deref(), v.reason.clone().flatten(), v.wave),
            (
                Some("spent"),
                Some(PyStr::from("changed nothing")),
                Some(Some(1))
            )
        );
        // A fix that moved its branch past its base, still the run head, is reviewed.
        let mut lines = accepted();
        lines.push(("dispatch_requested", fix(C)));
        lines.push(("result_recorded", ready));
        let st = state(&w.run, &lines);
        let r = requested(&st, &end_obs());
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.wave,
                r.role.as_str()
            ),
            (
                "run-w1-final_review-1",
                "final_review",
                Some(1),
                "final_reviewer"
            )
        );
        assert!(r.notes.lossy().starts_with(
            "## Fix wave\n\nThis reviews fix wave 1 for the failed \"final_review\" gate, \
             whose findings are in /r/f.md."
        ));
        assert_eq!(
            r.extra.get(&"diff_range".into()).cloned().flatten(),
            Some(PyStr::from(format!("{C}..{D}")))
        );
        // The review's clean verdict: the run branch moves to the fix tip.
        let review = run_req("run-w1-final_review-1", "final_review", None, Some(1), &{
            format!(r#""diff_range":"{C}..{D}","base":"{C}","branch":"leadhand/demo--card-final""#)
        })
        .replace(
            &format!(r#""expected_head":"{C}""#),
            &format!(r#""expected_head":"{D}""#),
        );
        lines.push(("dispatch_requested", review));
        lines.push((
            "result_recorded",
            run_result(
                "run-w1-final_review-1",
                "final_review",
                r#""verdict":"clean""#,
            ),
        ));
        lines.push((
            "verdict_recorded",
            r#""card":"final","wave":1,"verdict":"clean","findings":"x","dispatch_id":"run-w1-final_review-1""#
                .to_string(),
        ));
        let st = state(&w.run, &lines);
        let mut o = end_obs();
        let key = (PyStr::from("refs/heads/leadhand/demo"), D.to_string());
        assert_eq!(
            decide(&st, &o),
            Decision::Need(Need::Ancestor(key.0.clone(), key.1.clone()))
        );
        o.ancestors.insert(key.clone(), true);
        assert_eq!(
            decide(&st, &o),
            Decision::AcceptFix {
                dispatch_id: "run-w1-final_review-1".into(),
                wave: Some(1),
                target: D.into(),
                started: false,
            }
        );
        // A card accepted meanwhile makes the wave stale: the gate loop goes on.
        o.ancestors.insert(key, false);
        assert!(!matches!(decide(&st, &o), Decision::AcceptFix { .. }));
    }

    #[test]
    fn a_run_level_check_voided_past_the_cap_ends_the_run_blocked() {
        let w = world("");
        let guarded = |id: &str, kind: &str| {
            run_req(id, kind, Some(kind), None, "")
                + &format!(r#","refs":{{"refs/heads/leadhand/demo":"{C}"}}"#)
        };
        let mut lines = accepted();
        for n in 1..=3 {
            let id = format!("run-verify-{n}");
            lines.push(("dispatch_requested", guarded(&id, "verify")));
            lines.push((
                "result_recorded",
                run_result(&id, "verify", r#""result":"pass""#),
            ));
            lines.push((
                "violation",
                format!(
                    r#""stage":"found","by":"check","dispatch_id":"{id}","card":null,"dispatch_kind":"verify","n":{n},"ref":null,"paths":["README.md"],"refs":{{}},"preserved":{{}}"#
                ),
            ));
            lines.push((
                "violation",
                format!(
                    r#""stage":"restored","by":"check","dispatch_id":"{id}","card":null,"n":{n},"ref":null,"saved":null,"siblings":[],"preserved":{{}}"#
                ),
            ));
        }
        let st = state(&w.run, &lines);
        let ending = ended(&st, &end_obs());
        assert_eq!(ending.blocked, Some(Blocked::DecisionNeeded));
        assert_eq!(
            ending.why,
            [PyStr::from(
                "verify workers change tracked files: README.md"
            )]
        );
        // Below the cap, the check runs again with a fresh worker and the warning.
        let st = state(&w.run, &lines[..11]);
        let r = requested(&st, &end_obs());
        assert_eq!(r.dispatch_id, "run-verify-3");
        assert!(r.notes.contains(crate::engine::protocol::VOIDED_NOTE));
        // An evidence result is checked against the base worktree too.
        let mut ev = accepted();
        ev.push(("dispatch_requested", guarded("run-evidence-1", "evidence")));
        ev.push((
            "result_recorded",
            run_result("run-evidence-1", "evidence", r#""result":"pass""#),
        ));
        let st = state(&w.run, &ev);
        let mut o = end_obs();
        o.guards.insert("run-evidence-1".into(), None);
        assert_eq!(
            decide(&st, &o),
            Decision::Need(Need::BaseGuard("run-evidence-1".into()))
        );
        o.base_guards.insert("run-evidence-1".into(), None);
        assert!(matches!(decide(&st, &o), Decision::RecordGate(_)));
    }

    // Carried from Task 14's review: `_work_branch` gives a fix wave's implementer the final
    // branch (`engine.py:910`), which a run-level Codex dispatch's line and its cancel name.
    #[test]
    fn a_fix_waves_implementer_works_on_the_final_branch() {
        let w = world("");
        let mut lines = accepted();
        lines.push((
            "dispatch_requested",
            run_req("run-w1-final_fix-1", "final_fix", None, Some(1), ""),
        ));
        lines.push((
            "dispatch_requested",
            run_req("run-evidence-1", "evidence", Some("evidence"), None, ""),
        ));
        let st = state(&w.run, &lines);
        let fix = st.log.dispatch(Some("run-w1-final_fix-1")).unwrap();
        assert_eq!(
            work_branch(&w.run, &st.cards, &fix).unwrap(),
            PyStr::from("leadhand/demo--card-final")
        );
        let gate = st.log.dispatch(Some("run-evidence-1")).unwrap();
        assert_eq!(
            work_branch(&w.run, &st.cards, &gate).unwrap(),
            PyStr::from("leadhand/demo")
        );
    }

    #[test]
    fn nothing_accepted_ends_the_run_without_gates_and_decisions_block_it() {
        let w = world("");
        let lines = vec![
            ("run_started", String::new()),
            (
                "card_cut",
                format!(r#""card":1,"branch":"leadhand/demo--card-1","base":"{B}""#),
            ),
            (
                "parked",
                r#""card":1,"reason":"product decision","decision":true"#.to_string(),
            ),
        ];
        let st = state(&w.run, &lines);
        let mut o = end_obs();
        o.run_tip = Some(B.into());
        assert_eq!(decide(&st, &o), Decision::End(Ending::default()));
        let waiting = super::super::waiting_on_decisions(&w.run, &st.cards, &st.log).unwrap();
        assert_eq!(waiting, [PyStr::from("card 1 parked: product decision")]);
    }
}
