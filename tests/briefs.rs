//! Worker briefs: the brief and report files `next` writes for every dispatch, under
//! <workspace>/.leadhand/<card or run>/<dispatch id>/, and the lines they render.
//!
//! The Rust form of `tests/contract/contract_briefs.py` (converted from tests/test_briefs.py).
//! A brief's text lines are contract output (ruling S1-R10), so each test runs a deck to the
//! dispatch and reads the brief that `next` wrote, asserting the rendered lines. The card
//! section is read with `common::engine::card_section`.
//!
//! `BriefWorld` is Python's FakeSupervisor subclass of that name: its `launch` override is the
//! world's `before_launch` hook, keeping every dispatch's brief as it was when the worker
//! started, and the workspace's `git status` at that moment.

#[macro_use]
mod common;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::rc::Rc;

use common::engine::{CARD1, RUN, recorded, with_status, with_verdict};
use common::git::run_git;
use common::json::Json;
use common::supervisor::{
    CardOpts, FakeSupervisor, Options, Step, card, card_with, os_path, run_dir, text,
};
use common::{jv, lines, path_str as s, read_text};

const CONTRACT_POINTER: &str = "Your worker contract and report format are in your instructions.";
const WHEN_CHEAP: &str =
    "- test_first: when-cheap — write a failing test first whenever one is cheap to write.";
const ALWAYS: &str = "- test_first: always — write a failing test and see it fail before the code that makes it pass.";
const RULING: &str = "Use B.\nKeep the old order.";

const RICH_CARD: &str = "## Card 3: Parse the widget header
- Covers: R1, R4
- Role: implementer
- Hard: yes — touches the on-disk format
- Risk: high
- Depends on: 1, 2
- Source: docs/plan.md#task-3
- Allowed paths: src/widget/**, tests/test_widget.py
- Out of scope: Rendering; the CLI flags.
- Done when: `test_header_roundtrip` passes and `make check` is clean.
- Stop if: the header needs a new version byte.
- Inferred: \"Done when\" test name (plan named no test)
- Route: codex gpt-6.1-sol xhigh

Context: The header is 16 bytes, little-endian.
Keep the parser pure.
";

/// A worker agent file's body (agents/<name>.md without its front matter), as a Codex
/// worker's prompt carries it.
fn agent_body(name: &str) -> String {
    let mut text = read_text(&common::plugin::agents().join(format!("{name}.md")));
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some((_, body)) = rest.split_once("\n---\n")
    {
        text = body.to_string();
    }
    text.trim().to_string()
}

fn has(lines: &[String], line: &str) -> bool {
    lines.iter().any(|l| l == line)
}

/// Each dispatch's action and brief text as the worker started, in launch order (a dict:
/// a dispatch launched again keeps its first place, with the later brief).
type Briefs = Rc<RefCell<Vec<(String, Json, String)>>>;

/// A FakeSupervisor that keeps every dispatch's brief as it was when the worker started, and
/// the workspace's `git status` at that moment.
struct BriefWorld {
    w: FakeSupervisor,
    briefs: Briefs,
    status_at_launch: Rc<RefCell<HashMap<String, String>>>,
}

impl Deref for BriefWorld {
    type Target = FakeSupervisor;

    fn deref(&self) -> &FakeSupervisor {
        &self.w
    }
}

impl DerefMut for BriefWorld {
    fn deref_mut(&mut self) -> &mut FakeSupervisor {
        &mut self.w
    }
}

impl BriefWorld {
    #[track_caller]
    fn new(cards: &[String], o: Options) -> BriefWorld {
        let mut w = FakeSupervisor::with_options(cards, o);
        let briefs: Briefs = Rc::default();
        let status_at_launch: Rc<RefCell<HashMap<String, String>>> = Rc::default();
        let (b, st) = (briefs.clone(), status_at_launch.clone());
        w.before_launch = Some(Box::new(move |w: &FakeSupervisor, a: &Json| {
            let did = text(&a["dispatch_id"]).to_string();
            let brief = read_text(&os_path(&a["brief"]));
            let mut b = b.borrow_mut();
            match b.iter_mut().find(|(d, _, _)| *d == did) {
                Some(entry) => *entry = (did.clone(), a.clone(), brief),
                None => b.push((did.clone(), a.clone(), brief)),
            }
            let status = run_git(
                &w.workspace(),
                &["status", "--porcelain", "--untracked-files=all"],
            );
            st.borrow_mut().insert(did, status);
        }));
        BriefWorld {
            w,
            briefs,
            status_at_launch,
        }
    }

    /// Every kept (dispatch ID, action, brief text), in launch order.
    fn briefs(&self) -> Vec<(String, Json, String)> {
        self.briefs.borrow().clone()
    }

    /// The dispatch's action and brief text (Python's `w.briefs[dispatch_id]`).
    #[track_caller]
    fn entry(&self, dispatch_id: &str) -> (Json, String) {
        self.briefs
            .borrow()
            .iter()
            .find(|(d, _, _)| d == dispatch_id)
            .map(|(_, a, t)| (a.clone(), t.clone()))
            .unwrap_or_else(|| panic!("no brief kept for {dispatch_id}"))
    }

    /// The dispatch's action and brief lines.
    #[track_caller]
    fn brief(&self, dispatch_id: &str) -> (Json, Vec<String>) {
        let (a, text) = self.entry(dispatch_id);
        (a, lines(&text))
    }

    #[track_caller]
    fn status_at_launch(&self, dispatch_id: &str) -> String {
        self.status_at_launch
            .borrow()
            .get(dispatch_id)
            .cloned()
            .unwrap_or_else(|| panic!("no status kept for {dispatch_id}"))
    }
}

/// A finished run with the end-of-run gates, reaching every brief kind but integrate and
/// continue: card 1 (medium risk) asks a question and gets a two-line ruling, then a review
/// with defects, a fix and a re-review; card 2 is Hard; the security gate finds defects, so a
/// final fix wave runs. The deck's Build and Test are `make build` and `make test`.
#[track_caller]
fn gates_world() -> BriefWorld {
    let one = card_with(
        1,
        CardOpts {
            risk: "medium",
            title: Some("Parse the widget header"),
            ..CardOpts::default()
        },
    )
    .replace("- Done when", "- Out of scope: Rendering.\n- Done when");
    let two = card_with(
        2,
        CardOpts {
            title: Some("Second"),
            hard: true,
            allowed: "lib/b.py, src/",
            ..CardOpts::default()
        },
    )
    .replace("- Done when", "- Out of scope: Docs.\n- Done when");
    let mut w = BriefWorld::new(
        &[one, two],
        Options {
            gates: true,
            ..Options::default()
        },
    );
    let deck = read_text(&w.deck_path).replace(
        "Build: true\nTest: true\n",
        "Build: make build\nTest: make test\n",
    );
    fs::write(&w.deck_path, deck).unwrap();
    w.start();
    w.script(1, "implement", [with_status("QUESTION")]);
    w.rulings.insert(1, vec![RULING.to_string()]);
    w.script(1, "review", [with_verdict("defects")]);
    w.script(
        1,
        "fix",
        [Step {
            files: Some(vec![("src/widget.py".into(), "fixed\n".into())]),
            ..Step::default()
        }],
    );
    w.script(None, "security", [with_verdict("defects")]);
    let done = w.run().pop().unwrap();
    assert_eq!(done["outcome"], "complete", "{done}");
    w
}

fn kinds(w: &BriefWorld) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (did, a, _) in w.briefs() {
        out.entry(text(&a["kind"]).to_string())
            .or_default()
            .push(did);
    }
    out
}

/// Every placeholder filled, the contract pointer present, the brief and report named
/// (`assert_filled`).
macro_rules! assert_filled {
    ($a:expr, $lines:expr) => {{
        let a: &Json = &$a;
        let lines: &[String] = &$lines;
        let did = text(&a["dispatch_id"]);
        let joined = lines.join("\n");
        assert!(!joined.contains('$'), "{did}");
        assert!(has(lines, CONTRACT_POINTER), "{did}");
        assert!(
            has(lines, &format!("- This brief: {}", text(&a["brief"]))),
            "{did}"
        );
        assert!(
            has(lines, &format!("- Report file: {}", text(&a["report"]))),
            "{did}"
        );
    }};
}

mod briefs {
    use super::*;
    use crate::common::dispatch_world::{World as DispatchWorld, implementer_prompt};
    use crate::common::engine::{card_section, relaunch_env};
    use crate::common::supervisor::CliOpts;
    use crate::common::{index, names_bin, pick, shlex_split};

    // -- the card, verbatim -------------------------------------------------------

    #[test]
    fn test_implement_brief_contains_contract_fields_verbatim() {
        let cfg = "[run]\ntest_first = \"always\"\n[roles.implementer]\nprefer = \"claude\"\n\
                   [roles.reviewer]\nprefer = \"claude\"\n";
        let mut w = BriefWorld::new(
            &[card(1), card(2), RICH_CARD.to_string()],
            Options {
                installed: &["claude", "codex"],
                config: cfg,
                ..Options::default()
            },
        );
        w.start();
        let mut a = Json::Null;
        for _ in 0..20 {
            a = w.next();
            if a["action"] == "dispatch" && a["card"] == 3 {
                break;
            }
            w.act(&a);
        }
        assert_json_eq!(
            pick(&a, &["kind", "agent"]),
            jv(r#"["implement", "leadhand:codex"]"#),
            "{a}"
        );
        let lines = lines(&read_text(&os_path(&a["brief"])));
        let run_tip = w.branch_tip(RUN);
        for line in [
            format!("- Deck: {}", s(&run_dir(&w).join("deck.md"))),
            "- Plan: docs/plan.md".to_string(),
            "- Card source: docs/plan.md#task-3".to_string(),
            "- Files: src/widget/**, tests/test_widget.py".to_string(),
            format!("- Base commit: {run_tip}"),
            "- Card branch: leadhand/demo--card-3".to_string(),
            "- Attempt: 1".to_string(),
            format!("- This brief: {}", text(&a["brief"])),
            format!("- Report file: {}", text(&a["report"])),
            ALWAYS.to_string(),
        ] {
            assert!(has(&lines, &line), "{line:?} not in {lines:?}");
        }
        assert_eq!(
            card_section(&lines.join("\n"), 3),
            [
                "## Card 3: Parse the widget header",
                "- Covers: R1, R4",
                "- Role: implementer",
                "- Hard: yes — touches the on-disk format",
                "- Risk: high",
                "- Depends on: 1, 2",
                "- Source: docs/plan.md#task-3",
                "- Allowed paths: src/widget/**, tests/test_widget.py",
                "- Out of scope: Rendering; the CLI flags.",
                "- Done when: `test_header_roundtrip` passes and `make check` is clean.",
                "- Stop if: the header needs a new version byte.",
                "- Inferred: \"Done when\" test name (plan named no test)",
                "- Route: codex gpt-6.1-sol xhigh",
                "",
                "The header is 16 bytes, little-endian.",
                "Keep the parser pure.",
            ]
        );
        assert_filled!(a, lines);
    }

    #[test]
    fn test_test_first_defaults_to_when_cheap() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        let lines = lines(&read_text(&os_path(&a["brief"])));
        assert!(has(&lines, WHEN_CHEAP), "{lines:?}");
    }

    // -- every kind -----------------------------------------------------------------

    #[test]
    fn test_every_brief_kind_fills_its_values() {
        let w = gates_world();
        let seen = kinds(&w);
        assert_eq!(
            seen.keys().collect::<Vec<_>>(),
            [
                "answer",
                "evidence",
                "final_fix",
                "final_review",
                "fix",
                "implement",
                "rereview",
                "review",
                "security",
                "verify"
            ]
        );
        for (did, _, _) in w.briefs() {
            let (a, lines) = w.brief(&did);
            assert_filled!(a, lines);
        }
        let base = w.base.clone();
        let reports = run_dir(&w).join("cards");
        let (_, fix) = w.brief("c1-a1-fix-1");
        assert!(
            has(
                &fix,
                &format!(
                    "- Findings: {}",
                    s(&reports.join("1/c1-a1-review-1/report.md"))
                )
            ),
            "{fix:?}"
        );
        assert!(
            has(
                &fix,
                "A review of this card found defects. This is fix round 1. Read the"
            ),
            "{fix:?}"
        );
        let (_, answer) = w.brief("c1-a1-answer-1");
        assert!(has(&answer, "> Use B."), "{answer:?}");
        let (_, review) = w.brief("c1-a1-review-1");
        let tip = w
            .events("result_recorded")
            .into_iter()
            .find(|e| e["dispatch_id"] == "c1-a1-answer-1")
            .map(|e| text(&e["tip"]).to_string())
            .expect("the answer's result");
        assert!(
            has(&review, &format!("- Diff range: {base}..{tip}")),
            "{review:?}"
        );
        let (_, rereview) = w.brief("c1-a1-rereview-1");
        assert!(
            has(
                &rereview,
                &format!(
                    "- Findings to recheck: {}",
                    s(&reports.join("1/c1-a1-review-1/report.md"))
                )
            ),
            "{rereview:?}"
        );
        let accepted: BTreeMap<String, String> = w
            .events("accepted")
            .iter()
            .map(|e| (common::json::dumps(&e["card"]), text(&e["sha"]).to_string()))
            .collect();
        let (_, final_review) = w.brief("run-final_review-1");
        assert!(
            has(
                &final_review,
                &format!("- Diff range: {base}..{}", accepted["2"])
            ),
            "{final_review:?}"
        );
        let (_, security) = w.brief("run-security-1");
        assert!(
            has(
                &security,
                &format!("- Diff range: {base}..{}", accepted["2"])
            ),
            "{security:?}"
        );
        let (_, evidence) = w.brief("run-evidence-1");
        let ws = w.workspace();
        let base_ws = ws.parent().unwrap().join(format!(
            "{}--base",
            ws.file_name().unwrap().to_str().unwrap()
        ));
        assert!(
            has(&evidence, &format!("- Base worktree: {}", s(&base_ws))),
            "{evidence:?}"
        );
        let (_, final_fix) = w.brief("run-w1-final_fix-1");
        assert!(
            has(
                &final_fix,
                &format!(
                    "- Findings: {}",
                    s(&reports.join("run/run-security-1/report.md"))
                )
            ),
            "{final_fix:?}"
        );
        assert!(
            has(
                &final_fix,
                "The run's final gate \"security\" failed. Its findings:"
            ),
            "{final_fix:?}"
        );
    }

    #[test]
    fn test_kind_specific_content() {
        let w = gates_world();
        let (_, security) = w.brief("run-security-1");
        assert!(
            has(
                &security,
                "Use the runtime's security-review skill when one is installed. Look for"
            ),
            "{security:?}"
        );
        let (_, evidence) = w.brief("run-evidence-1");
        let joined = evidence.join("\n");
        assert!(
            has(
                &evidence,
                "  Done when: the card's file exists and the tests pass"
            ),
            "{evidence:?}"
        );
        assert!(joined.contains("by an assertion"), "{joined}"); // R46: compile or import errors do not count
        assert!(joined.contains("import"), "{joined}");
        assert!(
            !joined.contains("fail (or do not\nexist) on the base"),
            "{joined}"
        );
        let (_, final_review) = w.brief("run-final_review-1");
        assert!(
            has(&final_review, "- Card 1: Parse the widget header"),
            "{final_review:?}"
        );
        let (_, verify) = w.brief("run-verify-1");
        assert!(has(&verify, "- Build: make build"), "{verify:?}");
        assert!(has(&verify, "- Test: make test"), "{verify:?}");
    }

    #[test]
    fn test_continue_brief_carries_on_from_the_branch() {
        // A worker that committed and died with its supervisor session: the relaunch
        // continues it.
        let mut w = BriefWorld::new(&[card(1)], Options::default());
        w.session("supervisor-1");
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                crash: Some("after_commit".into()),
                ..Step::default()
            }],
        );
        let a = w.next();
        let worker = w.launch(&a);
        let reply = w.perform(&a, &worker);
        assert!(reply.is_none(), "{reply:?}");
        let tip = w.branch_tip(CARD1);
        assert_ne!(tip, w.base);
        w.session("supervisor-2");
        w.cli_with(
            &os_args!["run", run_dir(&w)],
            CliOpts {
                env: relaunch_env(&w),
                ..CliOpts::default()
            },
        )
        .ok();
        let nxt = w.next();
        assert_json_eq!(
            pick(&nxt, &["action", "kind", "dispatch_id"]),
            jv(r#"["dispatch", "continue", "c1-a1-continue-1"]"#)
        );
        let lines = lines(&read_text(&os_path(&nxt["brief"])));
        assert!(
            has(
                &lines,
                "continue from the branch as it stands and write your report."
            ),
            "{lines:?}"
        );
        assert!(has(&lines, "## The interrupted step"), "{lines:?}");
        assert!(lines.join("\n").contains("c1-a1-implement-1"), "{lines:?}");
        assert!(
            has(&lines, "- Card branch: leadhand/demo--card-1"),
            "{lines:?}"
        );
        assert_filled!(nxt, lines);
        assert_eq!(w.branch_tip(CARD1), tip);
    }

    // -- card briefs ------------------------------------------------------------------

    #[test]
    fn test_card_briefs_render_card_base_and_branch() {
        let w = gates_world();
        // Keyed as Python keys it: by the card value (a number, or "final" for a fix wave).
        let cut: BTreeMap<String, String> = w
            .events("card_cut")
            .iter()
            .map(|e| {
                (
                    common::json::dumps(&e["card"]),
                    text(&e["base"]).to_string(),
                )
            })
            .collect();
        for did in [
            "c1-a1-implement-1",
            "c1-a1-answer-1",
            "c1-a1-review-1",
            "c1-a1-fix-1",
            "c1-a1-rereview-1",
        ] {
            let (_, lines) = w.brief(did);
            assert!(
                has(&lines, &format!("- Base commit: {}", cut["1"])),
                "{did}"
            );
            assert!(has(&lines, "- Card branch: leadhand/demo--card-1"), "{did}");
            assert!(has(&lines, "## The card, verbatim"), "{did}");
        }
        assert_eq!(cut["1"], w.base);
    }

    #[test]
    fn test_review_names_the_implementer_report() {
        let w = gates_world();
        let reports = run_dir(&w).join("cards/1");
        let (_, review) = w.brief("c1-a1-review-1");
        assert!(
            has(
                &review,
                &format!(
                    "- Implementer's report: {}",
                    s(&reports.join("c1-a1-answer-1/report.md"))
                )
            ),
            "{review:?}"
        );
        assert!(
            has(
                &review,
                "correctness, test evidence (start from the `## Tests` section of the"
            ),
            "{review:?}"
        );
        let (_, rereview) = w.brief("c1-a1-rereview-1");
        assert!(
            has(
                &rereview,
                &format!(
                    "- Implementer's report: {}",
                    s(&reports.join("c1-a1-fix-1/report.md"))
                )
            ),
            "{rereview:?}"
        );
    }

    #[test]
    fn test_integrate_permits_rebase_onto_run_tip() {
        // Covers R15: integrate is the one permitted rewrite. Card 1 asks a question and is
        // parked; card 2 lands a conflicting change; the unparked card's rebase conflicts, so
        // an integrate attempt is requested.
        let mut w = BriefWorld::new(&[card(1), card(2)], Options::default());
        w.session("supervisor-1");
        w.start();
        w.script(
            1,
            "implement",
            [Step {
                files: Some(vec![("src/shared.txt".into(), "card one\n".into())]),
                ..with_status("QUESTION")
            }],
        );
        w.script(
            2,
            "implement",
            [Step {
                files: Some(vec![("src/shared.txt".into(), "card two\n".into())]),
                ..Step::default()
            }],
        );
        w.parks
            .insert(1, "product decision: storage format".to_string());
        w.run();
        w.parks.remove(&1).expect("card 1's park"); // Python's `del`: a KeyError when missing
        let run_tip = w.branch_tip(RUN);
        w.cli(&os_args![
            "unpark",
            run_dir(&w),
            "--card",
            "1",
            "--note",
            "Use JSON; it is simpler."
        ])
        .ok();
        let mut a = Json::Null;
        for _ in 0..40 {
            a = w.next();
            if a["action"] == "dispatch" && a["kind"] == "integrate" {
                break;
            }
            w.act(&a);
        }
        assert_eq!(a["dispatch_id"], "c1-a2-integrate-1", "{a}");
        let lines = lines(&read_text(&os_path(&a["brief"])));
        assert!(
            has(&lines, &format!("- Run tip to rebase onto: {run_tip}")),
            "{lines:?}"
        );
        assert!(
            has(&lines, "- Conflicting paths: src/shared.txt"),
            "{lines:?}"
        );
        assert!(
            has(
                &lines,
                "- The card's intent: card 1 (Card number 1): the card's file exists and the tests pass"
            ),
            "{lines:?}"
        );
        assert!(
            has(
                &lines,
                "- The run branch's intent: card 2 (Card number 2): the card's file exists and the tests pass"
            ),
            "{lines:?}"
        );
        assert!(
            lines.join(" ").contains("rebase the card branch"),
            "{lines:?}"
        );
        assert!(
            has(&lines, "- Card branch: leadhand/demo--card-1"),
            "{lines:?}"
        );
        assert_filled!(a, lines);
        // The worker contract the agents carry permits exactly that rewrite.
        let contract = read_text(&common::plugin::root().join("agents/implementer.md"));
        let contract = contract.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            contract.contains("rebase the card branch onto a named commit"),
            "{contract}"
        );
    }

    #[test]
    fn test_answer_quotes_every_ruling_line() {
        let w = gates_world();
        let (a, brief_text) = w.entry("c1-a1-answer-1");
        assert!(
            brief_text.contains("> Use B.\n> Keep the old order.\n"),
            "{brief_text}"
        );
        let ruling = common::one(w.events("ruling"));
        assert_eq!(ruling["text"], RULING, "{ruling}");
        assert_json_eq!(
            a["resume"],
            Json::object([(
                "worker_id",
                w.events("worker_started")[0]["worker_id"].clone()
            )])
        );
    }

    // -- run-level briefs -------------------------------------------------------------

    #[test]
    fn test_final_briefs_list_scope() {
        let w = gates_world();
        let (_, review) = w.brief("run-final_review-1");
        for line in [
            "- Card 1: Parse the widget header",
            "  Allowed paths: src/, tests/",
            "  Out of scope: Rendering.",
            "- Card 2: Second",
            "  Allowed paths: lib/b.py, src/",
            "  Out of scope: Docs.",
        ] {
            assert!(has(&review, line), "{line:?} not in {review:?}");
        }
        assert!(has(&review, "- Branch: leadhand/demo"), "{review:?}");
        let (_, fix) = w.brief("run-w1-final_fix-1");
        assert!(
            has(&fix, "- Commit on: leadhand/demo--card-final"),
            "{fix:?}"
        );
        assert!(
            has(&fix, "- Allowed paths: src/, tests/, lib/b.py"),
            "{fix:?}"
        );
        assert!(has(&fix, "- Branch: leadhand/demo--card-final"), "{fix:?}");
    }

    #[test]
    fn test_final_briefs_cover_only_the_accepted_cards() {
        // Card 1 is parked, card 2 (medium risk) is accepted, and the security gate finds
        // defects: the run-level briefs and the fix wave's allowed paths cover card 2 only.
        let one = card_with(
            1,
            CardOpts {
                title: Some("Parse the widget header"),
                ..CardOpts::default()
            },
        );
        let two = card_with(
            2,
            CardOpts {
                title: Some("Second"),
                risk: "medium",
                allowed: "lib/b.py, src/",
                ..CardOpts::default()
            },
        );
        let mut w = BriefWorld::new(
            &[one, two],
            Options {
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        w.script(1, "implement", [with_status("QUESTION")]);
        w.parks
            .insert(1, "product decision: storage format".to_string());
        w.script(None, "security", [with_verdict("defects")]);
        w.run();
        let accepted: Vec<Json> = w
            .events("accepted")
            .iter()
            .map(|e| e["card"].clone())
            .collect();
        assert_json_eq!(Json::Arr(accepted), jv(r#"[2, "final"]"#)); // card 2, then the fix wave
        let (_, fix) = w.brief("run-w1-final_fix-1");
        assert!(has(&fix, "- Allowed paths: lib/b.py, src/"), "{fix:?}");
        for did in ["run-w1-final_fix-1", "run-final_review-1", "run-evidence-1"] {
            let (_, lines) = w.brief(did);
            assert!(has(&lines, "- Card 2: Second"), "{did}");
            let card1: Vec<&String> = lines
                .iter()
                .filter(|l| l.starts_with("- Card 1:"))
                .collect();
            assert!(card1.is_empty(), "{did}: {card1:?}");
        }
    }

    // -- where briefs live ---------------------------------------------------------------

    #[test]
    fn test_brief_and_report_paths_are_inside_workspace_scratch() {
        // Covers C6.
        let w = gates_world();
        let ws = w.workspace();
        let briefs = w.briefs();
        for (did, a, _) in &briefs {
            let scope = if a["card"].is_null() {
                "run".to_string()
            } else {
                common::json::dumps(&a["card"])
            };
            let folder = ws.join(".leadhand").join(scope).join(did);
            assert_eq!(os_path(&a["brief"]), folder.join("brief.md"), "{did}");
            assert_eq!(os_path(&a["report"]), folder.join("report.md"), "{did}");
            assert_eq!(os_path(&a["reply_file"]), folder.join("reply.txt"), "{did}");
        }
        assert!(briefs.iter().any(|(d, _, _)| d == "run-verify-1"));
        assert!(briefs.iter().any(|(d, _, _)| d == "c1-a1-implement-1"));
    }

    #[test]
    fn test_scratch_dir_is_ignored_by_git() {
        // Covers C1.
        let w = gates_world();
        let ws = w.workspace();
        for did in [
            "c1-a1-implement-1",
            "c2-a1-implement-1",
            "run-final_review-1",
            "run-verify-1",
        ] {
            assert_eq!(w.status_at_launch(did), "", "{did}");
            let (a, _) = w.entry(did);
            assert!(os_path(&a["brief"]).starts_with(&ws), "{did}");
        }
        assert_eq!(
            run_git(
                &ws,
                &["check-ignore", ".leadhand/1/c1-a1-implement-1/brief.md"]
            ),
            ".leadhand/1/c1-a1-implement-1/brief.md"
        );
    }

    // -- agents and messages ---------------------------------------------------------------

    #[test]
    fn test_agent_for_maps_every_role_and_vendor() {
        let w = gates_world();
        let seen: BTreeSet<(String, String)> = w
            .briefs()
            .iter()
            .map(|(_, a, _)| (text(&a["role"]).to_string(), text(&a["agent"]).to_string()))
            .collect();
        let want: BTreeSet<(String, String)> = [
            ("implementer", "leadhand:implementer"),
            ("implementer_hard", "leadhand:implementer"),
            ("reviewer", "leadhand:reviewer"),
            ("reviewer_hard", "leadhand:reviewer"),
            ("final_reviewer", "leadhand:reviewer"),
            ("verifier", "leadhand:verifier"),
        ]
        .iter()
        .map(|(r, a)| (r.to_string(), a.to_string()))
        .collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn test_claude_dispatch_message_names_paths() {
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.start();
        let a = w.next();
        assert_eq!(a["agent"], "leadhand:implementer", "{a}");
        let lines = lines(text(&a["message"]));
        assert!(
            has(&lines, &format!("Brief: {}", text(&a["brief"]))),
            "{lines:?}"
        );
        assert!(
            has(&lines, &format!("Report file: {}", text(&a["report"]))),
            "{lines:?}"
        );
        assert!(lines.len() <= 15, "{lines:?}");
    }

    #[test]
    fn test_codex_dispatch_message_is_one_quoted_command() {
        let mut w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                installed: &["claude", "codex"],
                ..Options::default()
            },
        );
        w.start();
        let a = w.next();
        assert_eq!(a["agent"], "leadhand:codex", "{a}");
        assert!(text(&a["brief"]).contains(' '), "{a}");
        assert_eq!(text(&a["message"]).lines().count(), 1, "{a}");
        let argv = shlex_split(text(&a["message"])).expect("shell words");
        assert!(names_bin(&argv[0]), "{argv:?}"); // S1-R11: by file
        assert_eq!(argv[1], "dispatch");
        assert_eq!(argv[index(&argv, "--brief") + 1], text(&a["brief"]));
        assert_eq!(argv[index(&argv, "--report") + 1], text(&a["report"]));
        assert_eq!(argv[index(&argv, "--cwd") + 1], s(&w.workspace()));
        assert_eq!(argv[index(&argv, "--run") + 1], s(&run_dir(&w)));
    }

    #[test]
    fn test_codex_dispatch_message_quotes_shell_metacharacters() {
        // The argv carries user-controlled paths: a docs_dir with a quote, a dollar sign and a
        // semicolon must come back from shlex.split exactly.
        let mut w = FakeSupervisor::with_options(
            &[card(1)],
            Options {
                installed: &["claude", "codex"],
                ..Options::default()
            },
        );
        let docs = w.tmp.join("it's $HOME; rm -rf");
        fs::write(
            &w.config_file,
            format!(
                "docs_dir = {}\n",
                common::json::dumps(&Json::from(s(&docs)))
            ),
        )
        .unwrap();
        w.start();
        let rd = run_dir(&w);
        assert!(s(&rd).starts_with(s(&docs)), "{}", rd.display());
        let a = w.next();
        assert_eq!(a["agent"], "leadhand:codex", "{a}");
        assert_eq!(text(&a["message"]).lines().count(), 1, "{a}");
        let argv = shlex_split(text(&a["message"])).expect("shell words");
        assert!(names_bin(&argv[0]), "{argv:?}");
        assert_eq!(argv[index(&argv, "--run") + 1], s(&rd));
        assert_eq!(
            argv[index(&argv, "--dispatch-id") + 1],
            text(&a["dispatch_id"])
        );
    }

    /// The prompt Codex got for an implementer, a Hard implementer and a reviewer dispatch,
    /// from the fake Codex's log; with each dispatch's brief and report.
    #[track_caller]
    fn prompts() -> BTreeMap<String, (String, PathBuf, PathBuf)> {
        let mut w = DispatchWorld::new();
        w.scenario("ready", &[]);
        let mut out = BTreeMap::new();
        for (did, role) in [
            ("d1", "implementer"),
            ("d2", "implementer_hard"),
            ("d3", "reviewer"),
        ] {
            w.dispatch_with(
                did,
                &crate::common::dispatch_world::Req {
                    role,
                    ..Default::default()
                },
            )
            .ok();
            let starts = w.records("start");
            let prompt = text(&starts.last().expect("a start record")["prompt"]).to_string();
            let (brief, report) = w.scratch(did);
            out.insert(role.to_string(), (prompt, brief, report));
        }
        out
    }

    #[test]
    fn test_worker_prompt_for_codex_workers() {
        let prompts = prompts();
        let (prompt, brief, report) = &prompts["implementer_hard"];
        assert_eq!(*prompt, implementer_prompt((brief.clone(), report.clone())));
        assert!(prompt.contains(&agent_body("implementer")));
        let lines = lines(prompt);
        assert!(has(&lines, &format!("- Brief: {}", s(brief))), "{prompt}");
        assert!(
            has(&lines, &format!("- Report file: {}", s(report))),
            "{prompt}"
        );
        assert!(!prompt.contains("---\nname:"), "{prompt}");
        let (review, brief, report) = &prompts["reviewer"];
        assert_eq!(
            *review,
            format!(
                "{}\n\n## Your files\n\n- Brief: {}\n- Report file: {}\n\n\
                 Read the brief now, do the work, write the report, then reply.\n",
                agent_body("reviewer"),
                s(brief),
                s(report)
            )
        );
        assert!(review.contains("Verdict: clean"), "{review}");
        assert!(!review.contains(&agent_body("implementer")), "{review}");
    }

    #[test]
    fn test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not() {
        let no_commit = "Do not run git commit, git add, or any command that writes under .git; \
                         leave your changes in the working tree and list the changed files in \
                         your report. leadhand commits them on your branch after you finish.";
        let prompts = prompts();
        for role in ["implementer", "implementer_hard"] {
            let words = prompts[role]
                .0
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(words.contains(no_commit), "{role}");
        }
        assert!(
            !prompts["reviewer"].0.contains("leadhand commits them"),
            "{}",
            prompts["reviewer"].0
        );
        let mut agents: Vec<PathBuf> = fs::read_dir(common::plugin::agents())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        agents.sort();
        for agent in agents {
            assert!(
                !read_text(&agent).contains("leadhand commits them"),
                "{}",
                agent.display()
            );
        }
    }

    // -- the report head ---------------------------------------------------------------

    #[test]
    fn test_parse_report_head_variants() {
        // A reply with no report file: the result event carries the head parsed from the
        // reply.
        let cases: [(&str, [&str; 3]); 15] = [
            ("Status: READY\n## Summary\n", ["Status", "READY", ""]),
            (
                "\n\n**Status:** QUESTION\n## Question\n",
                ["Status", "QUESTION", ""],
            ),
            (
                "Status: BLOCKED\nBlocked by: usage limit\n",
                ["Status", "BLOCKED", "usage limit"],
            ),
            (
                "**Status: BLOCKED**\n**Blocked by:** environment\n",
                ["Status", "BLOCKED", "environment"],
            ),
            ("Status: spent\n", ["Status", "SPENT", ""]),
            (
                "Status: BLOCKED\nBlocked by: Environment — no network\n",
                ["Status", "BLOCKED", "environment"],
            ),
            (
                "Status: BLOCKED\nBlocked by: usage limits hit\n",
                ["Status", "BLOCKED", "usage limit"],
            ),
            (
                "Status: BLOCKED\nBlocked by: a hard bug\n",
                ["Status", "BLOCKED", ""],
            ),
            (
                "Verdict: defects\n## Findings\n1. [high] a.py:3 — x — y\n",
                ["Verdict", "defects", ""],
            ),
            ("Verdict: Clean\n", ["Verdict", "clean", ""]),
            ("Result: pass\n## Checks\n", ["Result", "pass", ""]),
            ("", ["Status", "BLOCKED", ""]),
            (
                "I finished the work, all good.\n",
                ["Status", "BLOCKED", ""],
            ),
            ("Status: DONE\n", ["Status", "BLOCKED", ""]),
            ("Verdict: maybe\n", ["Status", "BLOCKED", ""]),
        ];
        for (reply, want) in cases {
            let mut w = FakeSupervisor::new(&[card(1)]);
            w.start();
            let a = w.next();
            let worker = w.launch(&a);
            let event = recorded(
                &w,
                "result",
                &os_args![
                    "--dispatch",
                    text(&a["dispatch_id"]),
                    "--worker",
                    worker,
                    "--reply",
                    reply
                ],
            );
            let key = text(&event["head"]).to_string();
            // Python's dict of all three: each must be there, whichever the head names.
            let heads = [
                ("Status", event["status"].clone()),
                ("Verdict", event["verdict"].clone()),
                ("Result", event["result"].clone()),
            ];
            let value = heads
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("an unknown head {key:?}: {event}"));
            assert_json_eq!(
                Json::from(vec![
                    key.as_str().into(),
                    value,
                    event["blocked_by"].clone()
                ]),
                Json::from(&want[..]),
                "reply={reply:?}: {event}"
            );
        }
    }
}
