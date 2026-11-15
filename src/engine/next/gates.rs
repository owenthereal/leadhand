//! The end of the card loop (`engine._end_of_run`): the commit-bound gates, the fix waves a
//! failing gate gets, and how the run ends. Pure, as the rest of `decide`.
//!
//! A gate passes only for the commit it examined: each `gate_result` records `gate`,
//! `examined_sha` and `passed`, and a gate is current only when its last result on the run head
//! passed. The loop runs the first required gate that is not current. A failing gate gets a fix
//! wave while fewer than `MAX_FINAL_FIX_WAVES` have run: an implementer (`final_fix`) on the
//! final branch cut from the run head, a review of its diff, and, when clean, a fast-forward of
//! the run branch to the fix, which makes every gate stale. Once the waves are spent, a failing
//! gate is settled: a review gate leaves open findings, verify fails the run, and an evidence
//! gap is a hold (`evidence = "record"`) or ends the run blocked for a decision
//! (`evidence = "block"`). When every required gate has a result on the head, the outcome is
//! decided. With no card accepted, no gate runs.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{Decision, Ending, Need, Observations, RunRequest, RunState, Step, fail, need};
use crate::deck::Card;
use crate::engine::cards::{
    Log, Run, accepted_cards, card_summary, final_branch, is_final, short_sha,
};
use crate::engine::protocol::{
    Action, DoneAction, GATES, MAX_FINAL_FIX_WAVES, REVIEW_GATES, RISKS, gate_role, run_dispatch_id,
};
use crate::error::Result;
use crate::pytext::path_text;
use crate::route::{RouteRequest, resolve};
use crate::state::events::{self, given};
use crate::state::{Blocked, CardStage, CardState, Event};
use crate::wire::{PyMap, PyStr};

/// Every card is accepted, parked, or waits on a parked card (`engine._end_of_run`): run the
/// gates, then end. The card-loop-only test mode stops here, with no gates and no outcome.
pub(super) fn end_of_run(st: &RunState, obs: &Observations) -> Step<Decision> {
    if obs.card_loop_only {
        return card_loop_done(st);
    }
    let accepted = accepted_cards(st.run, &st.cards)?;
    if accepted.is_empty() {
        return Ok(Decision::End(Ending::default()));
    }
    let head = run_head(st, obs)?;
    let results = gate_results(&st.log, &head, st.log.reopened_since());
    let waves = st
        .log
        .run_dispatches()
        .iter()
        .filter(|d| d.kind() == "final_fix")
        .map(|d| given(&d.req.wave).copied())
        .collect::<HashSet<_>>()
        .len();
    let mut settled: Vec<(&str, &events::GateResult)> = Vec::new();
    for gate in required_gates(st, obs, &accepted, &head)? {
        let Some(r) = results.get(gate) else {
            return request_gate(st, obs, gate, &head, &accepted);
        };
        if r.passed == Some(true) {
            continue;
        }
        if waves < MAX_FINAL_FIX_WAVES {
            return start_wave(st, obs, waves as i64 + 1, r, &head, &accepted);
        }
        settled.push((gate, r));
        if gate == "evidence" && st.run.config.run.evidence == "block" {
            let mut why = PyStr::from(format!(
                "evidence gaps on {} after {waves} fix waves: ",
                short_sha(&head)
            ));
            why.push(&r.findings.clone().unwrap_or_default());
            return Ok(Decision::End(Ending {
                blocked: Some(Blocked::DecisionNeeded),
                why: vec![why],
                ..Ending::default()
            }));
        }
    }
    let settled_gate = |g: &str| settled.iter().find(|(name, _)| *name == g).map(|(_, r)| *r);
    let holds = settled_gate("evidence")
        .map(|r| {
            let mut hold = PyStr::from(format!("evidence gaps on {}: ", short_sha(&head)));
            hold.push(&r.findings.clone().unwrap_or_default());
            vec![hold]
        })
        .unwrap_or_default();
    Ok(Decision::End(Ending {
        gates_current: settled.is_empty(),
        open_findings: REVIEW_GATES.iter().any(|g| settled_gate(g).is_some()),
        holds,
        verification_failed: settled_gate("verify").is_some(),
        ..Ending::default()
    }))
}

/// The card-loop-only test mode's `done`: where the card loop ends, no outcome event.
fn card_loop_done(st: &RunState) -> Step<Decision> {
    let cards = card_summary(st.run, &st.cards)?;
    let reasons = cards
        .iter()
        .filter(|row| !row.reason.is_empty())
        .map(|row| {
            let mut text = PyStr::from(format!("card {} {}: ", row.card, row.state));
            text.push(&row.reason);
            text
        })
        .collect();
    Ok(Decision::Act(Action::Done(DoneAction {
        outcome: None,
        qualifier: String::new(),
        reasons,
        report: None,
        cards,
        card_loop_only: true,
    })))
}

/// The run branch's tip (`_run_head`).
pub(super) fn run_head(st: &RunState, obs: &Observations) -> Step<String> {
    match &obs.run_tip {
        Some(tip) => Ok(tip.clone()),
        None => fail(format!(
            "the run branch {} is missing",
            st.run.branch.shown()
        )),
    }
}

/// Every gate, with security only when the highest accepted risk is medium or high (an
/// unknown risk counts as medium, as in `risk_card`) or a path changed since the base is
/// sensitive (`_required_gates`).
fn required_gates(
    st: &RunState,
    obs: &Observations,
    accepted: &[&Card],
    head: &str,
) -> Step<Vec<&'static str>> {
    let risky = accepted.iter().any(|c| c.risk != "low");
    let security = risky || {
        let key = (st.run.base_sha.clone(), head.to_string());
        let Some(paths) = obs.changed.get(&key) else {
            return need(Need::Changed(key.0, key.1));
        };
        paths.iter().any(is_sensitive_path)
    };
    Ok(GATES
        .into_iter()
        .filter(|g| *g != "security" || security)
        .collect())
}

/// Each gate's last result on `head` (`_gate_results`). A failure recorded before the ending a
/// resume followed (`since`) is not counted (the gate runs again); a pass stays current while
/// the head stays.
fn gate_results<'l>(
    log: &'l Log,
    head: &str,
    since: u64,
) -> HashMap<String, &'l events::GateResult> {
    let mut out = HashMap::new();
    for e in &log.events {
        if let Event::GateResult(g) = e
            && g.examined_sha.as_deref() == Some(head)
            && (g.passed == Some(true) || g.seq > since)
        {
            out.insert(g.gate.clone().unwrap_or_default(), g);
        }
    }
    out
}

/// The accepted card with the highest risk (the first of them), stripped of Hard and route
/// pins: run-level routes follow the run's highest risk only (`_risk_card`).
fn risk_card(accepted: &[&Card]) -> Card {
    let rank = |c: &Card| RISKS.iter().position(|r| *r == c.risk).unwrap_or(1);
    let mut top = accepted[0];
    for c in &accepted[1..] {
        if rank(c) > rank(top) {
            top = c;
        }
    }
    let mut card = top.clone();
    card.hard = false;
    card.hard_reason = String::new();
    card.route_pin = None;
    card
}

/// The route for a run-level role (`_resolve_run`); no route is an error, not a park: there
/// is no card to park.
pub(super) fn resolve_run(
    st: &RunState,
    obs: &Observations,
    role: &str,
    accepted: &[&Card],
    security: bool,
) -> Step<(String, crate::config::Route)> {
    let card = risk_card(accepted);
    let found = resolve(
        &st.run.config,
        &RouteRequest {
            role,
            card: Some(&card),
            attempt: 1,
            last_implementer_vendor: None,
            installed: &obs.installed,
            security,
            profile: st.run.deck.profile.as_deref(),
        },
    );
    match found {
        Ok(res) => Ok((res.role, res.route)),
        Err(e) => fail(format!("no route for the run's {role}: {e}")),
    }
}

/// A run-level request with its deterministic dispatch ID (`_request_run`'s `did`): `n`
/// counts the kind's run-level requests within the wave.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_request(
    st: &RunState,
    kind: &str,
    gate: Option<String>,
    wave: Option<i64>,
    attempt: i64,
    role: String,
    route: crate::config::Route,
    extra: PyMap<Option<PyStr>>,
    notes: PyStr,
    accepted: &[&Card],
) -> Decision {
    let n = 1 + st
        .log
        .events
        .iter()
        .filter(|e| {
            matches!(e, Event::DispatchRequested(d)
                if d.card.is_none()
                    && d.dispatch_kind.as_deref() == Some(kind)
                    && given(&d.wave).copied() == wave)
        })
        .count();
    Decision::RunRequest(Box::new(RunRequest {
        dispatch_id: run_dispatch_id(kind, wave, n),
        kind: kind.to_string(),
        gate,
        wave,
        attempt,
        role,
        route,
        extra,
        notes,
        cards: accepted.iter().map(|c| c.number).collect(),
    }))
}

/// A gate's first dispatch on `head` (`_request_gate`): a review gate reviews the run's whole
/// diff; the evidence gate runs in the evidence base worktree, which the shell prepares.
fn request_gate(
    st: &RunState,
    obs: &Observations,
    gate: &str,
    head: &str,
    accepted: &[&Card],
) -> Step<Decision> {
    let (role, security) = gate_role(gate);
    let (role, route) = resolve_run(st, obs, role, accepted, security)?;
    let mut extra: PyMap<Option<PyStr>> = PyMap::default();
    if REVIEW_GATES.contains(&gate) {
        let range = format!("{}..{head}", st.run.base_sha);
        extra.0.push(("diff_range".into(), Some(range.into())));
    }
    if gate == "evidence" {
        let path = crate::engine::gates::base_worktree_path(st.run);
        extra
            .0
            .push(("base_worktree".into(), Some(path_text(&path))));
    }
    Ok(run_request(
        st,
        gate,
        Some(gate.to_string()),
        None,
        1,
        role,
        route,
        extra,
        PyStr::default(),
        accepted,
    ))
}

/// Fix wave `wave` for the failing gate result `failing` (`_start_wave`): the final branch is
/// cut at the run head first, then the wave's implementer is requested on it.
fn start_wave(
    st: &RunState,
    obs: &Observations,
    wave: i64,
    failing: &events::GateResult,
    head: &str,
    accepted: &[&Card],
) -> Step<Decision> {
    let cut = st.log.events.iter().any(|e| {
        matches!(e, Event::CardCut(c) if is_final(c.card.as_ref()) && given(&c.wave) == Some(&wave))
    });
    if !cut {
        return Ok(Decision::CutFinal {
            wave,
            head: head.to_string(),
        });
    }
    let (role, route) = resolve_run(st, obs, "implementer", accepted, false)?;
    let findings = failing
        .findings
        .clone()
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "(no report kept)".into());
    let extra = PyMap(vec![
        ("gate".into(), failing.gate.clone().map(PyStr::from)),
        ("findings".into(), Some(findings)),
        ("branch".into(), Some(final_branch(st.run))),
        ("base".into(), Some(head.into())),
    ]);
    Ok(run_request(
        st,
        "final_fix",
        None,
        Some(wave),
        wave,
        role,
        route,
        extra,
        PyStr::default(),
        accepted,
    ))
}

// --------------------------------------------------------------------------
// The outcome's decisions

/// Whether the card's latest park waits on the operator's decision: the supervisor parked it
/// on a worker's question (`decision`), or the violation cap parked it (R32)
/// (`_decision_park`).
fn decision_park(log: &Log, card: u32) -> bool {
    let last = log.events.iter().rev().find_map(|e| match e {
        Event::Parked(p) if crate::engine::cards::is_card(p.card.as_ref(), card) => Some(p),
        _ => None,
    });
    last.is_some_and(|p| {
        given(&p.decision).copied().unwrap_or(false)
            || given(&p.cap_dispatch).is_some_and(|d| !d.is_empty())
    })
}

/// When every card not accepted is parked on a decision, or waits only on such cards, one
/// reason per parked card; else none, also when every card is accepted
/// (`_waiting_on_decisions`, R44).
pub(crate) fn waiting_on_decisions(
    run: &Run,
    cards: &BTreeMap<u32, CardState>,
    log: &Log,
) -> Result<Vec<PyStr>> {
    fn blockers(
        run: &Run,
        cards: &BTreeMap<u32, CardState>,
        n: u32,
        seen: &[u32],
    ) -> Result<HashSet<u32>> {
        let mut out = HashSet::new();
        for d in &run.card(n)?.depends_on {
            let Some(c) = cards.get(d) else { continue };
            if seen.contains(d) || c.state == CardStage::Accepted {
                continue;
            }
            if c.state == CardStage::Parked {
                out.insert(*d);
            } else {
                let mut deeper = seen.to_vec();
                deeper.push(n);
                out.extend(blockers(run, cards, *d, &deeper)?);
            }
        }
        Ok(out)
    }

    let left: Vec<u32> = cards
        .iter()
        .filter(|(_, c)| c.state != CardStage::Accepted)
        .map(|(n, _)| *n)
        .collect();
    if left.is_empty() {
        return Ok(Vec::new());
    }
    let parked: Vec<u32> = left
        .iter()
        .copied()
        .filter(|n| cards[n].state == CardStage::Parked)
        .collect();
    for n in &left {
        if parked.contains(n) {
            if !decision_park(log, *n) {
                return Ok(Vec::new());
            }
        } else {
            let waits = blockers(run, cards, *n, &[])?;
            if waits.is_empty() || !waits.iter().all(|b| decision_park(log, *b)) {
                return Ok(Vec::new());
            }
        }
    }
    Ok(parked
        .iter()
        .map(|n| {
            let mut reason = PyStr::from(format!("card {n} parked: "));
            match cards[n].park_reason.as_ref().filter(|r| !r.is_empty()) {
                Some(why) => reason.push(why),
                None => reason.push_str("no reason recorded"),
            }
            reason
        })
        .collect())
}

// --------------------------------------------------------------------------
// Sensitive paths

/// Whether a changed path makes the security gate required whatever the cards' risk
/// (`engine.is_sensitive_path`, `SENSITIVE_PATHS`): the families of the deck's risk rule (auth,
/// secrets, CI, release or install scripts, data migrations) plus infrastructure. Matched
/// against repo-relative POSIX paths, case-insensitively as Python's `re.IGNORECASE` matches
/// (so `İ` and `ı` match `i`, the Kelvin sign `k`, and `ſ` `s`); `$` also matches before a
/// final newline, as Python's does. A false match only costs one review.
pub fn is_sensitive_path(path: &PyStr) -> bool {
    let s: Vec<u32> = path.code_points().map(fold).collect();
    SENSITIVE.iter().any(|m| m(&s))
}

/// A code point as an ASCII-lowercase pattern sees it under `re.IGNORECASE`.
fn fold(cp: u32) -> u32 {
    match cp {
        0x41..=0x5A => cp + 0x20,
        0x130 | 0x131 => u32::from(b'i'),
        0x212A => u32::from(b'k'),
        0x17F => u32::from(b's'),
        _ => cp,
    }
}

const SLASH: u32 = b'/' as u32;
const NEWLINE: u32 = b'\n' as u32;

/// Whether `s[at..]` starts with the ASCII `word`.
fn has(s: &[u32], at: usize, word: &str) -> bool {
    let w = word.as_bytes();
    s.len() >= at + w.len()
        && s[at..at + w.len()]
            .iter()
            .zip(w)
            .all(|(c, b)| *c == u32::from(*b))
}

/// Python's `$` at `at`: the end, or right before a final newline.
fn end(s: &[u32], at: usize) -> bool {
    at == s.len() || (at + 1 == s.len() && s[at] == NEWLINE)
}

/// The positions `(^|/)` matches at: the start, and after each `/`.
fn starts(s: &[u32]) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain((0..s.len()).filter(|i| s[*i] == SLASH).map(|i| i + 1))
}

/// `(^|/)(word…)` followed by what `after` accepts at the position after the word.
fn component(s: &[u32], words: &[&str], after: impl Fn(&[u32], usize) -> bool) -> bool {
    starts(s).any(|p| words.iter().any(|w| has(s, p, w) && after(s, p + w.len())))
}

/// `^<word>`.
fn prefix(s: &[u32], words: &[&str]) -> bool {
    words.iter().any(|w| has(s, 0, w))
}

/// `^<word>$`.
fn whole(s: &[u32], words: &[&str]) -> bool {
    words.iter().any(|w| has(s, 0, w) && end(s, w.len()))
}

/// `<suffix>$`, anywhere.
fn suffix(s: &[u32], words: &[&str]) -> bool {
    words.iter().any(|w| {
        let n = w.len();
        (s.len() >= n && has(s, s.len() - n, w))
            || (s.len() > n && s[s.len() - 1] == NEWLINE && has(s, s.len() - 1 - n, w))
    })
}

/// Whether `s[at]` is one of the ASCII `chars` (a character class).
fn one_of(s: &[u32], at: usize, chars: &[u8]) -> bool {
    s.get(at)
        .is_some_and(|c| chars.iter().any(|b| u32::from(*b) == *c))
}

/// `[^/]*$` from `at`: no `/` up to the end (a newline included, as `[^/]` takes it).
fn rest_has_no_slash(s: &[u32], at: usize) -> bool {
    !s[at.min(s.len())..].contains(&SLASH)
}

type Matcher = fn(&[u32]) -> bool;

/// `SENSITIVE_PATHS`, pattern by pattern.
const SENSITIVE: [Matcher; 19] = [
    // CI
    |s| prefix(s, &[".github/workflows/", ".github/actions/"]),
    |s| whole(s, &[".gitlab-ci.yml", ".gitlab-ci.yaml"]),
    |s| prefix(s, &[".circleci/"]),
    |s| prefix(s, &[".buildkite/"]),
    |s| component(s, &["jenkinsfile"], end),
    |s| whole(s, &["azure-pipelines.yml", "azure-pipelines.yaml"]),
    |s| whole(s, &[".travis.yml", ".travis.yaml"]),
    // release and install scripts: `(^|/)(install|…)([._-][^/]*)?$`
    |s| {
        let words = [
            "install",
            "installer",
            "release",
            "publish",
            "deploy",
            "bootstrap",
        ];
        component(s, &words, |s, at| {
            end(s, at) || (at < s.len() && one_of(s, at, b"._-") && rest_has_no_slash(s, at + 1))
        })
    },
    |s| component(s, &["release/", "releases/", "packaging/"], |_, _| true),
    // infrastructure
    |s| {
        let words = [
            "infra/",
            "infrastructure/",
            "terraform/",
            "k8s/",
            "kubernetes/",
            "helm/",
            "ansible/",
            "deploy/",
            "deployment/",
            "deployments/",
        ];
        component(s, &words, |_, _| true)
    },
    |s| suffix(s, &[".tf", ".tfvars"]),
    |s| component(s, &["dockerfile"], rest_has_no_slash),
    // `(^|/)docker-compose[^/]*\.ya?ml$`
    |s| {
        component(s, &["docker-compose"], |s, at| {
            let tail = &s[at..];
            !tail.contains(&SLASH) && suffix(tail, &[".yml", ".yaml"])
        })
    },
    // auth and secrets: `(^|/)(auth|…)([/._-]|$)`
    |s| {
        let words = [
            "auth",
            "authn",
            "authz",
            "oauth",
            "login",
            "session",
            "sessions",
            "permission",
            "permissions",
            "secret",
            "secrets",
            "credential",
            "credentials",
            "crypto",
            "security",
        ];
        component(s, &words, |s, at| end(s, at) || one_of(s, at, b"/._-"))
    },
    // `(^|/)\.env(\.[^/]*)?$`
    |s| {
        component(s, &[".env"], |s, at| {
            end(s, at) || (at < s.len() && s[at] == u32::from(b'.') && rest_has_no_slash(s, at + 1))
        })
    },
    |s| suffix(s, &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"]),
    // migrations
    |s| component(s, &["migration/", "migrations/"], |_, _| true),
    |s| component(s, &["db/migrate/", "database/migrate/"], |_, _| true),
    |s| component(s, &["alembic/"], |_, _| true),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_paths_match_as_the_python_matches() {
        let paths = [
            ".github/workflows/ci.yml",
            ".GitHub/Actions/x",
            "a/.github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".gitlab-ci.yaml\n",
            ".gitlab-ci.yml.bak",
            ".circleci/config.yml",
            ".buildkite/p.yml",
            "ci/Jenkinsfile",
            "Jenkinsfile.old",
            "azure-pipelines.yml",
            ".travis.yaml",
            "scripts/install.sh",
            "scripts/installer",
            "scripts/install-deps/x",
            "scripts/installs",
            "release",
            "x/release_notes.md",
            "releases/v1/notes",
            "src/packaging/x",
            "infra/main.go",
            "x/k8s/y",
            "deploy/x",
            "deployments/y",
            "main.tf",
            "vars.tfvars",
            "x.tf.json",
            "Dockerfile",
            "docker/Dockerfile.dev",
            "Dockerfile/x",
            "docker-compose.yml",
            "ops/docker-compose.prod.yaml",
            "docker-compose.yml/x",
            "src/auth/session.py",
            "src/auth.py",
            "src/author.py",
            "src/oauthlib/x",
            "src/sessions",
            "src/Session-store.js",
            "app/login.html",
            "app/loginpage.html",
            "x/crypto",
            ".env",
            "config/.env.local",
            ".envrc",
            "certs/server.pem",
            "a.KEY",
            "store.keystore",
            "db/migrations/0001_init.sql",
            "migration/x",
            "db/migrate/1.rb",
            "database/migrate/2.rb",
            "alembic/versions/x.py",
            "src/widget.py",
            "docs/authoring.md",
            "src/\u{130}nstall.sh",
            "\u{212a}8s/x",
            "src/\u{17f}ecret",
            "src/secr\u{e9}t",
            "",
            "/",
            "src/permissions\n",
        ];
        let code = "import json, sys\nfrom leadhand import engine\n\
                    print(json.dumps([engine.is_sensitive_path(p) for p in sys.argv[1:]]))";
        let args: Vec<&std::ffi::OsStr> = paths.iter().map(std::ffi::OsStr::new).collect();
        let theirs: Vec<bool> =
            serde_json::from_slice(&crate::wire::tests::python(code, &args)).unwrap();
        let ours: Vec<bool> = paths
            .iter()
            .map(|p| is_sensitive_path(&PyStr::from(*p)))
            .collect();
        for ((p, o), t) in paths.iter().zip(&ours).zip(&theirs) {
            assert_eq!(o, t, "{p:?}");
        }
        // An undecodable byte is a character outside every pattern.
        let odd = PyStr::from_os(std::os::unix::ffi::OsStrExt::from_bytes(b"src/\xffauth/x"));
        assert!(!is_sensitive_path(&odd));
        assert!(is_sensitive_path(&PyStr::from_os(
            std::os::unix::ffi::OsStrExt::from_bytes(b"\xff/auth/x")
        )));
    }

    #[test]
    fn the_risk_card_is_the_first_of_the_highest_risk_stripped_of_pins() {
        let card = |n: u32, risk: &str| Card {
            number: n,
            title: String::new(),
            role: "implementer".into(),
            hard: true,
            hard_reason: "core".into(),
            risk: risk.into(),
            depends_on: Vec::new(),
            covers: Vec::new(),
            source: String::new(),
            allowed_paths: Vec::new(),
            out_of_scope: String::new(),
            done_when: String::new(),
            stop_if: String::new(),
            inferred: Vec::new(),
            route_pin: Some(crate::config::Route {
                vendor: "codex".into(),
                model: "m".into(),
                effort: "low".into(),
            }),
            context: String::new(),
            missing: Vec::new(),
            line: 0,
            unknown_keys: Vec::new(),
        };
        let cards = [
            card(1, "low"),
            card(2, "high"),
            card(3, "high"),
            card(4, "odd"),
        ];
        let refs: Vec<&Card> = cards.iter().collect();
        let top = risk_card(&refs);
        assert_eq!(
            (top.number, top.hard, top.hard_reason.as_str()),
            (2, false, "")
        );
        assert_eq!(top.route_pin, None);
        // An unknown risk counts as medium.
        let refs: Vec<&Card> = vec![&cards[0], &cards[3]];
        assert_eq!(risk_card(&refs).number, 4);
    }
}
