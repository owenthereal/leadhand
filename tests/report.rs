//! `report RUN_DIR`: it writes report.md, the run's account of itself, and prints its path.
//!
//! The Rust form of the Python contract suite's `contract_report` (converted from Python's
//! `test_report`), with run directories built by `RunDir`. The report's text is read from the
//! report.md that `report` writes.

#[macro_use]
mod common;

use std::fs;

use common::json::Json;
use common::rundir::{AttemptOpts, DispatchOpts, RATE_LIMIT_NAME, RATE_LIMIT_TS, RunDir, outcome};
use common::{is_bin_command, shlex_quote};

const SECTIONS: [&str; 8] = [
    "## Needs your attention",
    "## Result",
    "## Evidence",
    "## Unresolved",
    "## Cards",
    "## Rulings",
    "## Usage",
    "## Routing suggestions",
];

fn finished() -> RunDir {
    let w = RunDir::with_cards(2);
    let d1 = w.dispatch(1);
    w.attempt(&d1, 1);
    w.accept_with(1, &"1".repeat(40));
    let d2 = w.dispatch_with(
        2,
        DispatchOpts {
            kind: "implement",
            vendor: "claude",
            model: "sonnet",
            effort: "medium",
            ..DispatchOpts::default()
        },
    );
    w.attempt_with(
        &d2,
        2,
        AttemptOpts {
            vendor: "claude",
            model: "sonnet",
            effort: "medium",
            status: "SPENT",
            ..AttemptOpts::default()
        },
    );
    w.accept_with(2, &"2".repeat(40));
    w.ev(
        "ruling",
        [
            ("card", Json::from(2)),
            ("dispatch_id", Json::from(d2.as_str())),
            ("text", Json::from("use the small API")),
        ],
    );
    w.ev(
        "gate_result",
        [
            ("gate", Json::from("tests")),
            ("examined_sha", Json::from("2".repeat(40))),
            ("passed", Json::from(true)),
        ],
    );
    w.ev("outcome", outcome("complete", "", &[]));
    w
}

/// Runs `report` (it must succeed) and returns the report.md it wrote.
#[track_caller]
fn report(w: &RunDir) -> String {
    common::cli(
        &os_args!["report", w.root],
        &w.env,
        w.root.parent().unwrap(),
    )
    .ok();
    fs::read_to_string(&w.paths.report).unwrap()
}

/// The text after `## <title>`, up to `## <until>` (to the end when None).
#[track_caller]
fn section(text: &str, title: &str, until: Option<&str>) -> String {
    let rest = text
        .split(&format!("## {title}"))
        .nth(1)
        .unwrap_or_else(|| panic!("no `## {title}` in {text}"));
    match until {
        Some(until) => rest.split(&format!("## {until}")).next().unwrap(),
        None => rest,
    }
    .to_string()
}

mod report {
    use super::*;
    use crate::common::rundir::ChargeOpts;

    #[test]
    fn test_report_outcome_is_line_3_after_title() {
        let text = report(&finished());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# Run report: Demo deck");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Outcome: complete");
    }

    #[test]
    fn test_outcome_qualifier() {
        let w = RunDir::with_cards(1);
        w.ev(
            "outcome",
            outcome("blocked", "paused", &["blocked: paused"]),
        );
        assert_eq!(
            report(&w).lines().nth(2).unwrap(),
            "Outcome: blocked (paused)"
        );
    }

    #[test]
    fn test_report_sections_in_order() {
        let w = finished();
        let text = report(&w);
        let headings: Vec<&str> = text.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(headings, SECTIONS);
        assert!(text.contains("leadhand/demo-2"), "{text}");
        let workspace = w.root.parent().unwrap().join("work space");
        assert!(text.contains(workspace.to_str().unwrap()), "{text}");
        assert!(text.contains("use the small API"), "{text}");
        let evidence = section(&text, "Evidence", Some("Unresolved"));
        assert!(evidence.contains("tests"), "{evidence}");
        assert!(evidence.contains(&"2".repeat(12)), "{evidence}");
    }

    #[test]
    fn test_incomplete_report_puts_parked_cards_under_attention() {
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
        w.ev(
            "parked",
            [
                ("card", Json::from(2)),
                ("reason", Json::from("needs a decision")),
            ],
        );
        w.ev(
            "outcome",
            outcome("incomplete", "", &["card 2 parked: needs a decision"]),
        );
        let text = report(&w);
        let attention = section(&text, "Needs your attention", Some("Result"));
        assert!(
            attention.contains("Card 2 parked: needs a decision"),
            "{attention}"
        );
        assert_eq!(text.lines().nth(2).unwrap(), "Outcome: incomplete");
    }

    #[test]
    fn test_report_usage_includes_supervisor_and_proxy_and_unknown_count() {
        let w = finished();
        w.charge_with(
            "s1",
            "supervisor",
            ChargeOpts {
                tokens: (5000, 500),
                ..ChargeOpts::default()
            },
        );
        w.charge_with(
            "p1",
            "proxy",
            ChargeOpts {
                tokens: (200, 20),
                ..ChargeOpts::default()
            },
        );
        w.charge_with(
            "i1",
            "implementer",
            ChargeOpts {
                source: "codex-stream",
                model: "gpt-6.1-terra",
                possibly_incomplete: true,
                ..ChargeOpts::default()
            },
        );
        w.attempt_with(
            "dx",
            2,
            AttemptOpts {
                usage_known: false,
                ..AttemptOpts::default()
            },
        );
        w.rate_limit_with(20.0, RATE_LIMIT_TS, RATE_LIMIT_NAME);
        w.rate_limit_with(
            31.0,
            "2026-01-03T03:04:05.000Z",
            "rollout-2026-01-03-abc.jsonl",
        );
        // the meter at the start of the run was recorded by an earlier status write
        fs::write(
            w.root.join("meter-start.json"),
            r#"{"percent": 20.0, "observed_at": "2026-01-02T03:04:05.000Z"}"#,
        )
        .unwrap();
        let usage = section(&report(&w), "Usage", Some("Routing suggestions"));
        for needle in ["supervisor", "proxy", "implementer", "claude", "codex"] {
            assert!(usage.contains(needle), "{needle:?} in {usage}");
        }
        // assertRegex(usage, r"unknown usage: 1"): the pattern is literal text
        assert!(usage.contains("unknown usage: 1"), "{usage}");
        assert!(usage.contains("may be undercounted"), "{usage}");
        assert!(
            usage.contains("Claude subscription meter: unknown"),
            "{usage}"
        );
        assert!(usage.contains("20.0%"), "{usage}");
        assert!(usage.contains("31.0%"), "{usage}");
    }

    #[test]
    fn test_routing_counts_worker_attempts_not_proxy() {
        let w = RunDir::with_cards(1);
        let d = w.dispatch(1);
        w.attempt_with(
            &d,
            1,
            AttemptOpts {
                role: "implementer",
                status: "READY",
                ..AttemptOpts::default()
            },
        );
        w.attempt_with(
            &d,
            1,
            AttemptOpts {
                role: "proxy",
                vendor: "claude",
                model: "haiku",
                effort: "low",
                status: "READY",
                extra: vec![("recorded_by", Json::from("engine"))],
                ..AttemptOpts::default()
            },
        );
        let routing = section(&report(&w), "Routing suggestions", None);
        assert!(
            routing.contains("implementer · codex gpt-6.1-terra high: 1 READY"),
            "{routing}"
        );
        assert!(!routing.contains("proxy"), "{routing}");
    }

    #[test]
    fn test_write_report_and_cli() {
        let w = finished();
        let r = common::cli(
            &os_args!["report", w.root],
            &w.env,
            w.root.parent().unwrap(),
        );
        assert_eq!(r.code, 0, "{r}");
        assert_eq!(
            r.stdout_text(),
            format!("{}\n", w.paths.report.display()),
            "{r}"
        );
        assert!(
            fs::read_to_string(&w.paths.report)
                .unwrap()
                .starts_with("# Run report: Demo deck\n\nOutcome: complete\n")
        );
    }
}

mod fix_round {
    use super::*;

    #[test]
    fn test_unresolved_lists_parked_cards_with_branches_and_open_findings() {
        let w = RunDir::with_cards(3);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(2)),
                ("branch", Json::from("leadhand/demo-2/c2")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "parked",
            [
                ("card", Json::from(2)),
                ("reason", Json::from("needs a decision")),
            ],
        );
        w.ev(
            "card_cut",
            [
                ("card", Json::from(3)),
                ("branch", Json::from("leadhand/demo-2/c3")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "verdict_recorded",
            [
                ("card", Json::from(3)),
                ("verdict", Json::from("defects")),
                ("findings", Json::from("/runs/x/findings.md")),
            ],
        );
        let unresolved = section(&report(&w), "Unresolved", Some("Cards"));
        assert!(
            unresolved.contains("Card 2 (branch leadhand/demo-2/c2) parked"),
            "{unresolved}"
        );
        assert!(unresolved.contains("needs a decision"), "{unresolved}");
        assert!(
            unresolved.contains(
                "Card 3 (branch leadhand/demo-2/c3) has open review findings: /runs/x/findings.md"
            ),
            "{unresolved}"
        );
    }

    #[test]
    fn test_attention_has_next_action_for_parked_cards() {
        let w = RunDir::with_cards(1);
        w.ev(
            "card_cut",
            [
                ("card", Json::from(1)),
                ("branch", Json::from("b")),
                ("base", Json::from("b")),
            ],
        );
        w.ev(
            "parked",
            [("card", Json::from(1)), ("reason", Json::from("why"))],
        );
        let attention = section(&report(&w), "Needs your attention", Some("Result"));
        let lines: Vec<&str> = attention.lines().collect();
        // the next step names the binary; it is compared by file, not spelling (S1-R11)
        let after = format!(
            " unpark {} --card 1 --note '…'",
            shlex_quote(w.root.to_str().unwrap())
        );
        assert!(
            lines
                .iter()
                .any(|l| is_bin_command(l, "- Card 1 parked: why — next: ", &after)),
            "{lines:?}"
        );
    }

    #[test]
    fn test_evidence_shows_last_result_per_gate_and_marks_stale() {
        let w = RunDir::with_cards(2);
        let (one, two) = ("1".repeat(40), "2".repeat(40));
        let gate = |name: &str, sha: &str, passed: bool| {
            [
                ("gate", Json::from(name)),
                ("examined_sha", Json::from(sha)),
                ("passed", Json::from(passed)),
            ]
        };
        w.accept_with(1, &one);
        w.ev("gate_result", gate("tests", &one, false));
        w.ev("gate_result", gate("tests", &one, true));
        w.ev("gate_result", gate("lint", &one, true));
        w.accept_with(2, &two);
        let ev = section(&report(&w), "Evidence", Some("Unresolved"));
        assert_eq!(ev.matches("- tests:").count(), 1, "{ev}");
        let stale = format!(
            "- tests: passed on commit {} (stale: the run head is {})",
            "1".repeat(12),
            "2".repeat(12)
        );
        assert!(ev.contains(&stale), "{ev}");
        w.ev("gate_result", gate("lint", &two, true));
        let ev = section(&report(&w), "Evidence", Some("Unresolved"));
        let fresh = format!("- lint: passed on commit {}\n", "2".repeat(12));
        assert!(ev.contains(&fresh), "{ev}");
    }

    #[test]
    fn test_report_is_written_atomically_without_leftover_temp_files() {
        let w = finished();
        report(&w);
        report(&w);
        assert!(w.paths.report.is_file());
        let temps: Vec<String> = fs::read_dir(&w.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert_eq!(temps, Vec::<String>::new());
    }

    #[test]
    fn test_world_is_hermetic_about_codex_home() {
        let w = RunDir::with_cards(1);
        let parent = w.root.parent().unwrap().to_str().unwrap();
        assert!(w.env.get_str("CODEX_HOME").starts_with(parent));
        // the binary reads the Codex meter from that CODEX_HOME, not the operator's
        w.rate_limit_with(42.0, RATE_LIMIT_TS, RATE_LIMIT_NAME);
        let usage = section(&report(&w), "Usage", Some("Routing suggestions"));
        assert!(
            usage
                .lines()
                .any(|l| l == "Codex meter after: 42.0% (observed 2026-01-02T03:04:05.000Z)"),
            "{usage}"
        );
    }
}
