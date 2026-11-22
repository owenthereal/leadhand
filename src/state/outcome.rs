//! Run outcomes (`state.Outcome`, `state.decide_outcome`, `state.allowed_next_step`).

use std::collections::{BTreeMap, BTreeSet};

use super::fold::{CardStage, CardState};
use crate::config::AfterRun;
use crate::error::{Error, Result};
use crate::pytext::py_repr;
use crate::wire::PyStr;

/// How a run ended (contract item 8, "Outcomes").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    Incomplete,
    Blocked,
    Failed,
}

impl Outcome {
    pub const ALL: [Outcome; 4] = [
        Outcome::Complete,
        Outcome::Incomplete,
        Outcome::Blocked,
        Outcome::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Complete => "complete",
            Outcome::Incomplete => "incomplete",
            Outcome::Blocked => "blocked",
            Outcome::Failed => "failed",
        }
    }
}

/// The blocked qualifiers (`state.BLOCKED_QUALIFIERS`).
pub const BLOCKED_QUALIFIERS: [&str; 4] = [
    "paused",
    "provider capacity",
    "decision needed",
    "environment",
];

/// Why a run ends blocked: one of `BLOCKED_QUALIFIERS`, so no other reason can be given
/// (where the Python raises `ValueError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    Paused,
    ProviderCapacity,
    DecisionNeeded,
    Environment,
}

impl Blocked {
    pub fn as_str(self) -> &'static str {
        match self {
            Blocked::Paused => BLOCKED_QUALIFIERS[0],
            Blocked::ProviderCapacity => BLOCKED_QUALIFIERS[1],
            Blocked::DecisionNeeded => BLOCKED_QUALIFIERS[2],
            Blocked::Environment => BLOCKED_QUALIFIERS[3],
        }
    }

    /// The qualifier named `name`; an error, worded as the Python's, for any other.
    pub fn parse(name: &str) -> Result<Blocked> {
        [
            Blocked::Paused,
            Blocked::ProviderCapacity,
            Blocked::DecisionNeeded,
            Blocked::Environment,
        ]
        .into_iter()
        .find(|b| b.as_str() == name)
        .ok_or_else(|| Error::msg(format!("unknown blocked reason: {}", py_repr(name))))
    }
}

/// The outcome, its qualifier (`""` when there is none) and its reasons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeDecision {
    pub outcome: Outcome,
    pub qualifier: String,
    /// Each reason as text; a parked card's reason quotes its park reason, worker text.
    pub reasons: Vec<PyStr>,
}

/// What `decide_outcome` decides from (the Python's keyword arguments).
#[derive(Clone, Debug)]
pub struct OutcomeInput<'a> {
    pub cards: &'a BTreeMap<u32, CardState>,
    pub removed: BTreeSet<u32>,
    pub open_findings: bool,
    pub holds: Vec<PyStr>,
    pub gates_current: bool,
    pub verification_failed: bool,
    pub cancelled: bool,
    pub blocked_reason: Option<Blocked>,
    /// What could not be reconciled; empty text counts as nothing, as in the Python.
    pub unreconciled: Option<PyStr>,
}

/// The run's outcome; the first match wins (`state.decide_outcome`):
///
/// 1. unreconciled -> failed (`state could not be reconciled: ...`);
/// 2. verification failed -> failed;
/// 3. a blocked reason -> blocked, qualified by it;
/// 4. cancelled -> incomplete, qualifier `cancelled`;
/// 5. every card accepted or removed, no open findings, no holds, gates current -> complete;
/// 6. otherwise incomplete, with one reason per card not done, per hold, one for open
///    findings and one for stale gates.
pub fn decide_outcome(input: &OutcomeInput) -> OutcomeDecision {
    let decision = |outcome, qualifier: &str, reasons: Vec<PyStr>| OutcomeDecision {
        outcome,
        qualifier: qualifier.to_string(),
        reasons,
    };
    if let Some(problem) = input.unreconciled.as_ref().filter(|p| !p.is_empty()) {
        let mut reason = PyStr::from("state could not be reconciled: ");
        reason.push(problem);
        return decision(Outcome::Failed, "", vec![reason]);
    }
    if input.verification_failed {
        return decision(
            Outcome::Failed,
            "",
            vec!["verification of the final commit failed".into()],
        );
    }
    if let Some(blocked) = input.blocked_reason {
        let reason = format!("blocked: {}", blocked.as_str());
        return decision(Outcome::Blocked, blocked.as_str(), vec![reason.into()]);
    }

    let mut reasons: Vec<PyStr> = Vec::new();
    for (number, c) in input.cards {
        if c.state == CardStage::Accepted || input.removed.contains(number) {
            continue;
        }
        if c.state == CardStage::Parked {
            let mut reason = PyStr::from(format!("card {number} parked: "));
            match c.park_reason.as_ref().filter(|r| !r.is_empty()) {
                Some(why) => reason.push(why),
                None => reason.push_str("no reason recorded"),
            }
            reasons.push(reason);
        } else {
            reasons
                .push(format!("card {number} not accepted (state: {})", c.state.as_str()).into());
        }
    }
    if input.open_findings {
        reasons.push("unresolved review findings".into());
    }
    for hold in &input.holds {
        let mut reason = PyStr::from("hold: ");
        reason.push(hold);
        reasons.push(reason);
    }
    if !input.gates_current {
        reasons.push("required gates have not all passed on the run head".into());
    }

    if input.cancelled {
        let mut all = vec![PyStr::from("run cancelled")];
        all.extend(reasons);
        return decision(Outcome::Incomplete, "cancelled", all);
    }
    if reasons.is_empty() {
        return decision(Outcome::Complete, "", Vec::new());
    }
    decision(Outcome::Incomplete, "", reasons)
}

/// The next step the run may take (`state.allowed_next_step`): complete -> `after_run.step`;
/// incomplete -> `draft-pr` when `on_incomplete` is `draft-pr`, else `none`; blocked and
/// failed -> `none`.
pub fn allowed_next_step(decision: &OutcomeDecision, after_run: &AfterRun) -> String {
    match decision.outcome {
        Outcome::Complete => after_run.step.clone(),
        Outcome::Incomplete if after_run.on_incomplete == "draft-pr" => "draft-pr".into(),
        _ => "none".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(number: u32, state: CardStage) -> (u32, CardState) {
        let mut c = CardState::blank(number);
        c.state = state;
        c.attempt = 1;
        (number, c)
    }

    fn parked(number: u32, reason: &str) -> (u32, CardState) {
        let (n, mut c) = card(number, CardStage::Parked);
        c.park_reason = Some(reason.into());
        (n, c)
    }

    fn cards(list: Vec<(u32, CardState)>) -> BTreeMap<u32, CardState> {
        list.into_iter().collect()
    }

    /// The Python tests' `decide` defaults: nothing removed, no findings, no holds, gates
    /// current, nothing failed, cancelled, blocked or unreconciled.
    fn input(cards: &BTreeMap<u32, CardState>) -> OutcomeInput<'_> {
        OutcomeInput {
            cards,
            removed: BTreeSet::new(),
            open_findings: false,
            holds: Vec::new(),
            gates_current: true,
            verification_failed: false,
            cancelled: false,
            blocked_reason: None,
            unreconciled: None,
        }
    }

    fn texts(d: &OutcomeDecision) -> Vec<String> {
        d.reasons.iter().map(|r| r.lossy().into_owned()).collect()
    }

    fn accepted(numbers: &[u32]) -> BTreeMap<u32, CardState> {
        cards(
            numbers
                .iter()
                .map(|&n| card(n, CardStage::Accepted))
                .collect(),
        )
    }

    // CATALOG: test_state::OutcomeTests::test_all_accepted_and_current_is_complete
    #[test]
    fn all_accepted_and_current_is_complete() {
        let c = accepted(&[1, 2]);
        let d = decide_outcome(&input(&c));
        assert_eq!(
            (d.outcome, d.qualifier.as_str(), d.reasons.len()),
            (Outcome::Complete, "", 0)
        );
    }

    // CATALOG: test_state::OutcomeTests::test_removed_cards_count_as_done
    #[test]
    fn removed_cards_count_as_done() {
        let c = cards(vec![card(1, CardStage::Accepted), parked(2, "x")]);
        let d = decide_outcome(&OutcomeInput {
            removed: [2].into(),
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Complete);
    }

    // CATALOG: test_state::OutcomeTests::test_complete_requires_current_gates
    #[test]
    fn complete_requires_current_gates() {
        let c = accepted(&[1]);
        let d = decide_outcome(&OutcomeInput {
            gates_current: false,
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Incomplete);
        assert_eq!(d.reasons.len(), 1);
        assert!(texts(&d)[0].contains("gate"));
    }

    // CATALOG: test_state::OutcomeTests::test_parked_card_makes_incomplete
    #[test]
    fn parked_card_makes_incomplete() {
        let c = cards(vec![
            card(1, CardStage::Accepted),
            parked(2, "stuck"),
            parked(3, "spent"),
        ]);
        let d = decide_outcome(&input(&c));
        assert_eq!((d.outcome, d.qualifier.as_str()), (Outcome::Incomplete, ""));
        let reasons = texts(&d);
        assert_eq!(reasons.len(), 2);
        assert!(reasons[0].contains("card 2") && reasons[0].contains("stuck"));
        assert!(reasons[1].contains("card 3"));
        assert_eq!(reasons[0], "card 2 parked: stuck");
    }

    // CATALOG: test_state::OutcomeTests::test_unfinished_card_makes_incomplete
    #[test]
    fn unfinished_card_makes_incomplete() {
        let c = cards(vec![card(1, CardStage::Reviewing)]);
        let d = decide_outcome(&input(&c));
        assert_eq!(d.outcome, Outcome::Incomplete);
        assert!(texts(&d)[0].contains("reviewing"));
        assert_eq!(texts(&d)[0], "card 1 not accepted (state: reviewing)");
    }

    // CATALOG: test_state::OutcomeTests::test_open_findings_make_incomplete
    #[test]
    fn open_findings_make_incomplete() {
        let c = accepted(&[1]);
        let d = decide_outcome(&OutcomeInput {
            open_findings: true,
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Incomplete);
        assert_eq!(d.reasons.len(), 1);
    }

    // CATALOG: test_state::OutcomeTests::test_holds_make_incomplete
    #[test]
    fn holds_make_incomplete() {
        let c = accepted(&[1]);
        let d = decide_outcome(&OutcomeInput {
            holds: vec![
                "card 1: no evidence".into(),
                "card 1: unverified claim".into(),
            ],
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Incomplete);
        assert_eq!(d.reasons.len(), 2);
        assert!(texts(&d)[0].contains("no evidence"));
    }

    // CATALOG: test_state::OutcomeTests::test_reasons_accumulate
    #[test]
    fn reasons_accumulate() {
        let c = cards(vec![parked(1, "p")]);
        let d = decide_outcome(&OutcomeInput {
            open_findings: true,
            holds: vec!["h".into()],
            gates_current: false,
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Incomplete);
        assert_eq!(d.reasons.len(), 4);
    }

    // CATALOG: test_state::OutcomeTests::test_cancel_is_incomplete_cancelled
    #[test]
    fn cancel_is_incomplete_cancelled() {
        let c = accepted(&[1]);
        let d = decide_outcome(&OutcomeInput {
            cancelled: true,
            ..input(&c)
        });
        assert_eq!(
            (d.outcome, d.qualifier.as_str()),
            (Outcome::Incomplete, "cancelled")
        );
        assert!(!d.reasons.is_empty());
    }

    // CATALOG: test_state::OutcomeTests::test_pause_is_blocked_paused
    #[test]
    fn pause_is_blocked_paused() {
        let c = cards(vec![card(1, CardStage::Pending)]);
        let d = decide_outcome(&OutcomeInput {
            blocked_reason: Some(Blocked::Paused),
            ..input(&c)
        });
        assert_eq!(
            (d.outcome, d.qualifier.as_str()),
            (Outcome::Blocked, "paused")
        );
    }

    // CATALOG: test_state::OutcomeTests::test_blocked_qualifiers
    #[test]
    fn blocked_qualifiers() {
        let none = BTreeMap::new();
        for q in [
            "paused",
            "provider capacity",
            "decision needed",
            "environment",
        ] {
            let d = decide_outcome(&OutcomeInput {
                blocked_reason: Some(Blocked::parse(q).unwrap()),
                ..input(&none)
            });
            assert_eq!((d.outcome, d.qualifier.as_str()), (Outcome::Blocked, q));
        }
        assert_eq!(
            Blocked::parse("because").unwrap_err().message,
            "unknown blocked reason: 'because'"
        );
    }

    // CATALOG: test_state::OutcomeTests::test_unreconciled_is_failed
    #[test]
    fn unreconciled_is_failed() {
        let c = accepted(&[1]);
        let d = decide_outcome(&OutcomeInput {
            unreconciled: Some("card 2 has a worker with no dispatch".into()),
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Failed);
        assert_eq!(d.qualifier, "");
        assert_eq!(
            texts(&d),
            ["state could not be reconciled: card 2 has a worker with no dispatch"]
        );
    }

    // CATALOG: test_state::OutcomeTests::test_verification_failed_is_failed
    #[test]
    fn verification_failed_is_failed() {
        let c = accepted(&[1]);
        let d = decide_outcome(&OutcomeInput {
            verification_failed: true,
            ..input(&c)
        });
        assert_eq!(d.outcome, Outcome::Failed);
    }

    // CATALOG: test_state::OutcomeTests::test_first_match_wins
    #[test]
    fn first_match_wins() {
        let one = accepted(&[1]);
        let none = BTreeMap::new();
        let everything = |cards| OutcomeInput {
            unreconciled: Some("x".into()),
            verification_failed: true,
            blocked_reason: Some(Blocked::Paused),
            cancelled: true,
            ..input(cards)
        };
        assert_eq!(decide_outcome(&everything(&one)).outcome, Outcome::Failed);
        assert!(texts(&decide_outcome(&everything(&none)))[0].contains("reconciled"));
        let no_unreconciled = |cards| OutcomeInput {
            unreconciled: None,
            ..everything(cards)
        };
        let d = decide_outcome(&no_unreconciled(&none));
        assert_eq!(d.outcome, Outcome::Failed);
        assert!(texts(&d)[0].contains("verification"));
        let verified = |cards| OutcomeInput {
            verification_failed: false,
            ..no_unreconciled(cards)
        };
        assert_eq!(decide_outcome(&verified(&none)).outcome, Outcome::Blocked);
        let d = decide_outcome(&OutcomeInput {
            blocked_reason: None,
            ..verified(&one)
        });
        assert_eq!(
            (d.outcome, d.qualifier.as_str()),
            (Outcome::Incomplete, "cancelled")
        );
    }

    // CATALOG: test_state::OutcomeTests::test_outcome_values
    #[test]
    fn outcome_values() {
        let values: Vec<&str> = Outcome::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(values, ["complete", "incomplete", "blocked", "failed"]);
        assert_eq!(Outcome::Complete.as_str(), "complete");
    }

    // CATALOG: test_state::OutcomeTests::test_allowed_next_step_matrix
    #[test]
    fn allowed_next_step_matrix() {
        let decision = |outcome| OutcomeDecision {
            outcome,
            qualifier: String::new(),
            reasons: Vec::new(),
        };
        for step in ["none", "pr", "skill:x"] {
            for on_incomplete in ["none", "draft-pr"] {
                let after = AfterRun {
                    step: step.into(),
                    on_incomplete: on_incomplete.into(),
                    repeat_safe: false,
                    check: String::new(),
                };
                let next = |o| allowed_next_step(&decision(o), &after);
                assert_eq!(next(Outcome::Complete), step);
                assert_eq!(
                    next(Outcome::Incomplete),
                    if on_incomplete == "draft-pr" {
                        "draft-pr"
                    } else {
                        "none"
                    }
                );
                assert_eq!(next(Outcome::Blocked), "none");
                assert_eq!(next(Outcome::Failed), "none");
            }
        }
    }

    #[test]
    fn a_park_reason_is_quoted_losslessly() {
        let odd: PyStr = serde_json::from_str(r#""a\udcffb""#).unwrap();
        let mut c = CardState::blank(1);
        c.state = CardStage::Parked;
        c.park_reason = Some(odd);
        let c = cards(vec![(1, c), parked(2, "")]);
        let d = decide_outcome(&input(&c));
        assert_eq!(
            crate::wire::ascii_json(&d.reasons[0]),
            r#""card 1 parked: a\udcffb""#
        );
        assert_eq!(texts(&d)[1], "card 2 parked: no reason recorded");
    }
}
