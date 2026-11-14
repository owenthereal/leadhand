//! Folding the event log into each card's state (`state.fold_cards`, `state.CardState`).

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use super::events::{CardRef, Event};
use crate::wire::PyStr;

/// The card states, in the Python's order (`state.CARD_STATES`).
pub const CARD_STATES: [&str; 12] = [
    "pending",
    "cut",
    "requested",
    "started",
    "reported",
    "reviewing",
    "reviewed",
    "fixing",
    "integrating",
    "accepting",
    "accepted",
    "parked",
];

/// A card's state (contract item 8, "Card states").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardStage {
    Pending,
    Cut,
    Requested,
    Started,
    Reported,
    Reviewing,
    Reviewed,
    Fixing,
    Integrating,
    Accepting,
    Accepted,
    Parked,
}

impl CardStage {
    pub const ALL: [CardStage; 12] = [
        CardStage::Pending,
        CardStage::Cut,
        CardStage::Requested,
        CardStage::Started,
        CardStage::Reported,
        CardStage::Reviewing,
        CardStage::Reviewed,
        CardStage::Fixing,
        CardStage::Integrating,
        CardStage::Accepting,
        CardStage::Accepted,
        CardStage::Parked,
    ];

    pub fn as_str(self) -> &'static str {
        CARD_STATES[self as usize]
    }
}

impl Serialize for CardStage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// One card's state, derived from the events (`state.CardState`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardState {
    pub number: u32,
    pub state: CardStage,
    pub attempt: i64,
    pub branch: Option<PyStr>,
    pub base: Option<String>,
    pub tip: Option<String>,
    pub dispatch_id: Option<String>,
    /// The kind of the card's current dispatch (implement, fix, review, ...): `kind` is the
    /// event kind in every record.
    pub dispatch_kind: Option<String>,
    pub worker_id: Option<PyStr>,
    pub vendor: Option<String>,
    pub status: Option<String>,
    pub verdict: Option<String>,
    pub findings: Option<PyStr>,
    pub fix_rounds: i64,
    pub kind_counts: BTreeMap<String, u32>,
    pub accept_target: Option<String>,
    pub accepted_sha: Option<String>,
    pub park_reason: Option<PyStr>,
}

impl CardState {
    /// A card no event has touched: `pending`.
    pub fn blank(number: u32) -> CardState {
        CardState {
            number,
            state: CardStage::Pending,
            attempt: 0,
            branch: None,
            base: None,
            tip: None,
            dispatch_id: None,
            dispatch_kind: None,
            worker_id: None,
            vendor: None,
            status: None,
            verdict: None,
            findings: None,
            fix_rounds: 0,
            kind_counts: BTreeMap::new(),
            accept_target: None,
            accepted_sha: None,
            park_reason: None,
        }
    }
}

/// Each card's state, derived from the event stream in order (`state.fold_cards`; its
/// docstring has the full table). Every card in `cards` starts `pending`; events for other
/// cards are ignored.
///
/// - `worker_started` and `result_recorded` name only a dispatch: they apply to the card that
///   dispatch belongs to, and only while it is that card's current dispatch.
/// - `accepted` is terminal: later events for an accepted card are ignored.
/// - A parked card records the fields of later events, but its state stays `parked`; only
///   `unparked` leaves it, and `accepted`, which overrides it (the run branch is the truth).
pub fn fold_cards(events: &[Event], cards: &[u32]) -> BTreeMap<u32, CardState> {
    let mut out: BTreeMap<u32, CardState> =
        cards.iter().map(|&n| (n, CardState::blank(n))).collect();
    // dispatch_id -> (card, dispatch_kind). The Python keys by `e.get("dispatch_id")`, so a
    // request without an ID is the dispatch a result without one belongs to.
    let mut dispatches: HashMap<Option<String>, (i64, String)> = HashMap::new();

    for e in events {
        let mut dispatch_kind = String::new();
        let number = match e {
            Event::WorkerStarted(w) => match dispatches.get(&w.dispatch_id) {
                Some((n, kind)) => {
                    dispatch_kind = kind.clone();
                    Some(*n)
                }
                None => continue,
            },
            Event::ResultRecorded(r) => match dispatches.get(&r.dispatch_id) {
                Some((n, kind)) => {
                    dispatch_kind = kind.clone();
                    Some(*n)
                }
                None => continue,
            },
            other => {
                let number = other.card().and_then(CardRef::number);
                if let (Event::DispatchRequested(d), Some(n)) = (other, number) {
                    dispatch_kind = d
                        .dispatch_kind
                        .clone()
                        .filter(|k| !k.is_empty())
                        .unwrap_or_else(|| "implement".into());
                    dispatches.insert(d.dispatch_id.clone(), (n, dispatch_kind.clone()));
                }
                number
            }
        };
        let Some(card) = number
            .and_then(|n| u32::try_from(n).ok())
            .and_then(|n| out.get_mut(&n))
        else {
            continue;
        };
        if card.state == CardStage::Accepted {
            continue;
        }
        let event_dispatch = match e {
            Event::WorkerStarted(w) => Some(&w.dispatch_id),
            Event::ResultRecorded(r) => Some(&r.dispatch_id),
            _ => None,
        };
        if event_dispatch.is_some_and(|id| *id != card.dispatch_id) {
            continue;
        }
        let held = card.state == CardStage::Parked
            && !matches!(e, Event::Unparked(_) | Event::Accepted(_));
        let before = card.state;

        match e {
            Event::CardCut(c) => {
                card.state = CardStage::Cut;
                card.branch = c.branch.clone();
                card.base = c.base.clone();
                card.tip = None;
                card.verdict = None;
                card.findings = None;
                card.status = None;
                card.dispatch_id = None;
                card.dispatch_kind = None;
                card.worker_id = None;
                card.accept_target = None;
                card.fix_rounds = 0;
            }
            Event::DispatchRequested(d) => {
                card.state = match dispatch_kind.as_str() {
                    "review" => CardStage::Reviewing,
                    "fix" => CardStage::Fixing,
                    _ => CardStage::Requested,
                };
                card.dispatch_id = d.dispatch_id.clone();
                card.attempt = d.attempt.unwrap_or(card.attempt);
                card.vendor = d.vendor.clone();
                card.worker_id = None;
                card.status = None;
                *card.kind_counts.entry(dispatch_kind.clone()).or_default() += 1;
                card.dispatch_kind = Some(dispatch_kind);
            }
            Event::WorkerStarted(w) => {
                card.worker_id = w.worker_id.clone();
                if card.state == CardStage::Requested {
                    card.state = CardStage::Started;
                }
            }
            Event::ResultRecorded(r) => {
                card.status = r.status.clone();
                if r.tip.is_some() {
                    card.tip = r.tip.clone();
                }
                if dispatch_kind != "review" {
                    card.state = CardStage::Reported;
                }
            }
            Event::VerdictRecorded(v) => {
                card.state = CardStage::Reviewed;
                card.verdict = v.verdict.clone();
                card.findings = v.findings.clone();
            }
            Event::FixRound(f) => {
                card.state = CardStage::Fixing;
                card.fix_rounds = f.n.unwrap_or(card.fix_rounds + 1);
            }
            Event::IntegrationStarted(_) | Event::Conflict(_) => {
                card.state = CardStage::Integrating;
            }
            Event::Rebased(r) => {
                card.state = CardStage::Integrating;
                if r.tip.is_some() {
                    card.tip = r.tip.clone();
                }
                card.verdict = None;
                card.findings = None;
            }
            Event::AcceptStarted(a) => {
                card.state = CardStage::Accepting;
                card.accept_target = a.target.clone();
            }
            Event::Accepted(a) => {
                card.state = CardStage::Accepted;
                card.accepted_sha = a.sha.clone();
            }
            Event::Parked(p) => {
                card.state = CardStage::Parked;
                card.park_reason = p.reason.clone();
            }
            Event::Unparked(_) => {
                card.state = if card.branch.is_some() {
                    CardStage::Cut
                } else {
                    CardStage::Pending
                };
                card.park_reason = None;
            }
            _ => {}
        }
        if held {
            card.state = before;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::events::decode_event;

    /// An event from its fields, as the Python tests' `ev(seq, kind, **fields)` builds it.
    fn ev(seq: u64, kind: &str, fields: &str) -> Event {
        let comma = if fields.is_empty() { "" } else { "," };
        let line = format!(
            r#"{{"seq":{seq},"ts":"2026-11-10T00:00:00Z","kind":"{kind}"{comma}{fields}}}"#
        );
        decode_event(serde_json::from_str(&line).unwrap())
            .unwrap()
            .0
    }

    fn fold(events: &[Event], cards: &[u32]) -> BTreeMap<u32, CardState> {
        fold_cards(events, cards)
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    fn py(s: &str) -> Option<PyStr> {
        Some(PyStr::from(s))
    }

    fn counts(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
        pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_tracks_requested_started_and_accepting
    #[test]
    fn fold_cards_tracks_requested_started_and_accepting() {
        let mut events = vec![
            ev(1, "run_started", ""),
            ev(
                2,
                "card_cut",
                r#""card":1,"branch":"leadhand/c1","base":"aaa""#,
            ),
            ev(
                3,
                "dispatch_requested",
                r#""dispatch_id":"d1","card":1,"attempt":1,"dispatch_kind":"implement","vendor":"codex""#,
            ),
        ];
        let cards = fold(&events, &[1, 2]);
        let c = &cards[&1];
        assert_eq!(c.state, CardStage::Requested);
        assert_eq!(c.attempt, 1);
        assert_eq!(c.dispatch_id, some("d1"));
        assert_eq!(c.vendor, some("codex"));
        assert_eq!(c.branch, py("leadhand/c1"));
        assert_eq!(c.base, some("aaa"));
        assert_eq!(c.kind_counts, counts(&[("implement", 1)]));
        assert_eq!(cards[&2].state, CardStage::Pending);
        assert_eq!(cards[&2].attempt, 0);

        events.push(ev(
            4,
            "worker_started",
            r#""dispatch_id":"d1","worker_id":"w1""#,
        ));
        let c = &fold(&events, &[1, 2])[&1];
        assert_eq!(
            (c.state, c.worker_id.clone()),
            (CardStage::Started, py("w1"))
        );

        events.push(ev(
            5,
            "result_recorded",
            r#""dispatch_id":"d1","status":"READY","blocked_by":null,"tip":"bbb""#,
        ));
        let c = &fold(&events, &[1, 2])[&1];
        assert_eq!(
            (c.state, c.status.clone(), c.tip.clone()),
            (CardStage::Reported, some("READY"), some("bbb"))
        );

        events.push(ev(
            6,
            "dispatch_requested",
            r#""dispatch_id":"d2","card":1,"attempt":1,"dispatch_kind":"review","vendor":"claude""#,
        ));
        let c = &fold(&events, &[1, 2])[&1];
        assert_eq!(c.state, CardStage::Reviewing);
        assert_eq!(c.kind_counts, counts(&[("implement", 1), ("review", 1)]));

        events.push(ev(
            7,
            "verdict_recorded",
            r#""card":1,"verdict":"approve","findings":null"#,
        ));
        let c = &fold(&events, &[1, 2])[&1];
        assert_eq!(
            (c.state, c.verdict.clone()),
            (CardStage::Reviewed, some("approve"))
        );

        events.push(ev(8, "accept_started", r#""card":1,"target":"bbb""#));
        let c = &fold(&events, &[1, 2])[&1];
        assert_eq!(
            (c.state, c.accept_target.clone()),
            (CardStage::Accepting, some("bbb"))
        );
        assert_eq!(c.accepted_sha, None);

        events.push(ev(9, "accepted", r#""card":1,"sha":"bbb""#));
        let c = &fold(&events, &[1, 2])[&1];
        assert_eq!(
            (c.state, c.accepted_sha.clone()),
            (CardStage::Accepted, some("bbb"))
        );
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_fix_round
    #[test]
    fn fold_cards_fix_round() {
        let mut events = vec![
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(
                2,
                "dispatch_requested",
                r#""dispatch_id":"d1","card":1,"attempt":1,"dispatch_kind":"implement","vendor":"codex""#,
            ),
            ev(
                3,
                "result_recorded",
                r#""dispatch_id":"d1","status":"READY","tip":"t1""#,
            ),
            ev(
                4,
                "verdict_recorded",
                r#""card":1,"verdict":"changes","findings":"F1: fix it""#,
            ),
            ev(5, "fix_round", r#""card":1,"n":1"#),
        ];
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.fix_rounds, c.findings.clone()),
            (CardStage::Fixing, 1, py("F1: fix it"))
        );
        events.push(ev(
            6,
            "dispatch_requested",
            r#""dispatch_id":"d2","card":1,"attempt":2,"dispatch_kind":"fix","vendor":"codex""#,
        ));
        events.push(ev(
            7,
            "worker_started",
            r#""dispatch_id":"d2","worker_id":"w2""#,
        ));
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.attempt, c.kind_counts.clone()),
            (
                CardStage::Fixing,
                2,
                counts(&[("implement", 1), ("fix", 1)])
            )
        );
        events.push(ev(
            8,
            "result_recorded",
            r#""dispatch_id":"d2","status":"READY","tip":"t2""#,
        ));
        let c = &fold(&events, &[1])[&1];
        assert_eq!((c.state, c.tip.clone()), (CardStage::Reported, some("t2")));
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_integration_resets_verdict
    #[test]
    fn fold_cards_integration_resets_verdict() {
        let mut events = vec![
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(
                2,
                "verdict_recorded",
                r#""card":1,"verdict":"approve","findings":null"#,
            ),
            ev(3, "integration_started", r#""card":1"#),
        ];
        assert_eq!(fold(&events, &[1])[&1].state, CardStage::Integrating);
        events.push(ev(4, "rebased", r#""card":1,"tip":"newtip""#));
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.tip.clone(), c.verdict.clone()),
            (CardStage::Integrating, some("newtip"), None)
        );
        events.push(ev(5, "conflict", r#""card":1"#));
        assert_eq!(fold(&events, &[1])[&1].state, CardStage::Integrating);
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_park_and_unpark
    #[test]
    fn fold_cards_park_and_unpark() {
        let mut events = vec![
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(2, "parked", r#""card":1,"reason":"two approaches failed""#),
        ];
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.park_reason.clone()),
            (CardStage::Parked, py("two approaches failed"))
        );
        events.push(ev(3, "unparked", r#""card":1,"note":"try again""#));
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.park_reason.clone(), c.branch.clone()),
            (CardStage::Cut, None, py("b"))
        );
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_unparked_without_branch_is_pending
    #[test]
    fn fold_cards_unparked_without_branch_is_pending() {
        let events = [
            ev(1, "parked", r#""card":1,"reason":"x""#),
            ev(2, "unparked", r#""card":1,"note":"n""#),
        ];
        assert_eq!(fold(&events, &[1])[&1].state, CardStage::Pending);
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_accepted_is_terminal_and_unknown_cards_ignored
    #[test]
    fn fold_cards_accepted_is_terminal_and_unknown_cards_ignored() {
        let events = [
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(2, "accepted", r#""card":1,"sha":"s""#),
            ev(3, "parked", r#""card":1,"reason":"late""#),
            ev(4, "card_cut", r#""card":99,"branch":"z","base":"a""#),
        ];
        let cards = fold(&events, &[1]);
        assert_eq!(cards.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(cards[&1].state, CardStage::Accepted);
        assert_eq!(cards[&1].park_reason, None);
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_ignores_stale_dispatch_events
    #[test]
    fn fold_cards_ignores_stale_dispatch_events() {
        let events = [
            ev(
                1,
                "dispatch_requested",
                r#""dispatch_id":"d1","card":1,"attempt":1,"dispatch_kind":"implement","vendor":"codex""#,
            ),
            ev(
                2,
                "dispatch_requested",
                r#""dispatch_id":"d2","card":1,"attempt":1,"dispatch_kind":"implement","vendor":"codex""#,
            ),
            ev(
                3,
                "worker_started",
                r#""dispatch_id":"d1","worker_id":"old""#,
            ),
            ev(
                4,
                "worker_started",
                r#""dispatch_id":"nope","worker_id":"ghost""#,
            ),
        ];
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.worker_id.clone(), c.dispatch_id.clone()),
            (CardStage::Requested, None, some("d2"))
        );
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_parked_stays_parked_on_late_events
    #[test]
    fn fold_cards_parked_stays_parked_on_late_events() {
        let mut events = vec![
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(
                2,
                "dispatch_requested",
                r#""dispatch_id":"d1","card":1,"attempt":1,"dispatch_kind":"implement","vendor":"codex""#,
            ),
            ev(3, "parked", r#""card":1,"reason":"operator""#),
            ev(
                4,
                "worker_started",
                r#""dispatch_id":"d1","worker_id":"w1""#,
            ),
            ev(
                5,
                "result_recorded",
                r#""dispatch_id":"d1","status":"READY","tip":"t""#,
            ),
            ev(
                6,
                "verdict_recorded",
                r#""card":1,"verdict":"approve","findings":null"#,
            ),
            ev(7, "fix_round", r#""card":1,"n":1"#),
            ev(8, "accept_started", r#""card":1,"target":"t""#),
        ];
        let c = &fold(&events, &[1])[&1];
        assert_eq!(c.state, CardStage::Parked);
        assert_eq!(
            (
                c.status.clone(),
                c.tip.clone(),
                c.verdict.clone(),
                c.worker_id.clone()
            ),
            (some("READY"), some("t"), some("approve"), py("w1"))
        );
        assert_eq!(c.park_reason, py("operator"));
        events.push(ev(9, "unparked", r#""card":1,"note":"go""#));
        assert_eq!(fold(&events, &[1])[&1].state, CardStage::Cut);
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_accepted_overrides_parked
    #[test]
    fn fold_cards_accepted_overrides_parked() {
        // R12: the run branch is the truth; once the fast-forward is recorded the card is in.
        let events = [
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(2, "accept_started", r#""card":1,"target":"t""#),
            ev(3, "parked", r#""card":1,"reason":"operator""#),
            ev(4, "accepted", r#""card":1,"sha":"t""#),
            ev(5, "unparked", r#""card":1,"note":"late""#),
        ];
        let c = &fold(&events, &[1])[&1];
        assert_eq!(
            (c.state, c.accepted_sha.clone()),
            (CardStage::Accepted, some("t"))
        );
    }

    // CATALOG: test_state.py::FoldCardsTests::test_fold_cards_exposes_dispatch_kind_for_phase_disambiguation
    #[test]
    fn fold_cards_exposes_dispatch_kind_for_phase_disambiguation() {
        let mut events = vec![
            ev(1, "card_cut", r#""card":1,"branch":"b","base":"a""#),
            ev(
                2,
                "dispatch_requested",
                r#""dispatch_id":"d1","card":1,"attempt":1,"dispatch_kind":"implement","vendor":"codex""#,
            ),
        ];
        let phase = |events: &[Event]| {
            let c = &fold(events, &[1])[&1];
            (c.state, c.dispatch_kind.clone())
        };
        assert_eq!(phase(&events), (CardStage::Requested, some("implement")));
        events.push(ev(
            3,
            "dispatch_requested",
            r#""dispatch_id":"d2","card":1,"attempt":1,"dispatch_kind":"review","vendor":"claude""#,
        ));
        assert_eq!(phase(&events), (CardStage::Reviewing, some("review")));
        // fix round recorded, no fix dispatch yet: fixing, but dispatch_kind is not "fix"
        events.push(ev(
            4,
            "verdict_recorded",
            r#""card":1,"verdict":"changes","findings":"f""#,
        ));
        events.push(ev(5, "fix_round", r#""card":1,"n":1"#));
        assert_eq!(phase(&events), (CardStage::Fixing, some("review")));
        events.push(ev(
            6,
            "dispatch_requested",
            r#""dispatch_id":"d3","card":1,"attempt":2,"dispatch_kind":"fix","vendor":"codex""#,
        ));
        assert_eq!(phase(&events), (CardStage::Fixing, some("fix")));
        events.push(ev(7, "card_cut", r#""card":1,"branch":"b2","base":"a""#));
        assert_eq!(fold(&events, &[1])[&1].dispatch_kind, None);
        assert_eq!(fold(&[], &[1])[&1].dispatch_kind, None);
    }

    // CATALOG: test_state.py::FoldCardsTests::test_card_states_constant
    #[test]
    fn card_states_constant() {
        assert_eq!(
            CARD_STATES,
            [
                "pending",
                "cut",
                "requested",
                "started",
                "reported",
                "reviewing",
                "reviewed",
                "fixing",
                "integrating",
                "accepting",
                "accepted",
                "parked"
            ]
        );
        let names: Vec<&str> = CardStage::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, CARD_STATES);
    }

    #[test]
    fn card_named_as_text_folds_as_its_number() {
        // `_card_number` is `int(value)`: "1" is card 1, "final" no deck card.
        let events = [
            ev(1, "card_cut", r#""card":"1","branch":"b","base":"a""#),
            ev(2, "card_cut", r#""card":"final","branch":"f","base":"a""#),
        ];
        let cards = fold(&events, &[1]);
        assert_eq!(
            (cards[&1].state, cards[&1].branch.clone()),
            (CardStage::Cut, py("b"))
        );
    }
}
