//! Route resolution, as `route` prints it: the route line, then one reason per line.
//!
//! The Rust form of `tests/contract/contract_route.py` (converted from tests/test_route.py).
//! The card comes from tests/fixtures/deck-basic.md (card 1: Hard: no, Risk: low), changed
//! where a test changes the card. The installed vendors are passed as `--installed`, as the
//! reference test does.

#[macro_use]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::Out;

const BOTH: &str = "claude,codex";

fn fixture() -> PathBuf {
    common::plugin::root()
        .join("tests")
        .join("fixtures")
        .join("deck-basic.md")
}

/// `fixture_text`'s keyword arguments.
#[derive(Default)]
struct CardChange<'a> {
    hard: bool,
    risk: Option<&'a str>,
    pin: Option<&'a str>,
    profile: Option<&'a str>,
}

/// deck-basic.md with card 1 changed.
fn fixture_text(c: &CardChange) -> String {
    let text = fs::read_to_string(fixture()).unwrap();
    let card1 = "- Hard: no\n- Risk: low\n";
    assert_eq!(text.matches(card1).count(), 1);
    let mut text = text.replace(
        card1,
        &format!(
            "- Hard: {}\n- Risk: {}\n",
            if c.hard { "yes — x" } else { "no" },
            c.risk.unwrap_or("low")
        ),
    );
    if let Some(pin) = c.pin {
        let stop = "- Stop if: the schema has to change\n";
        text = text.replace(stop, &format!("{stop}- Route: {pin}\n"));
    }
    if let Some(profile) = c.profile {
        text = text.replace(
            "Test: make test\n",
            &format!("Test: make test\nProfile: {profile}\n"),
        );
    }
    text
}

/// `run_route`'s keyword arguments.
struct Args<'a> {
    attempt: Option<i64>,
    last: Option<&'a str>,
    installed: &'a str,
}

impl Default for Args<'_> {
    fn default() -> Self {
        Args {
            attempt: None,
            last: None,
            installed: BOTH,
        }
    }
}

/// A route as (vendor, model, effort).
type Key = (String, String, String);

fn key(v: &str, m: &str, e: &str) -> Key {
    (v.to_string(), m.to_string(), e.to_string())
}

struct RouteCase {
    tmp: common::TempRoot,
    env: common::Env,
    decks: usize,
}

impl RouteCase {
    fn new() -> RouteCase {
        let tmp = common::TempRoot::new("lh route ");
        let env = common::Env::new(&tmp);
        RouteCase { tmp, env, decks: 0 }
    }

    /// Replaces the user configuration file (LEADHAND_CONFIG) with `text`.
    fn cfg(&self, text: &str) {
        fs::write(self.env.get("LEADHAND_CONFIG").unwrap(), text).unwrap();
    }

    fn deck(&mut self, c: CardChange) -> PathBuf {
        self.decks += 1;
        let p = self.tmp.join(format!("deck {}.md", self.decks));
        fs::write(&p, fixture_text(&c)).unwrap();
        p
    }

    fn run_route(&self, role: &str, deck: Option<&Path>, a: Args) -> Out {
        let fixture = fixture();
        let deck = deck.unwrap_or(&fixture);
        let mut args: Vec<std::ffi::OsString> = os_args![
            "route",
            deck,
            "1",
            role,
            "--repo",
            self.tmp.path(),
            "--installed",
            a.installed
        ]
        .iter()
        .map(|s| s.to_os_string())
        .collect();
        if let Some(n) = a.attempt {
            args.extend(["--attempt".into(), n.to_string().into()]);
        }
        if let Some(last) = a.last {
            args.extend(["--last-vendor".into(), last.into()]);
        }
        let args: Vec<&std::ffi::OsStr> = args.iter().map(|s| s.as_os_str()).collect();
        common::cli(&args, &self.env, &self.tmp)
    }

    /// The route as (vendor, model, effort), and the reasons (`r`).
    #[track_caller]
    fn r(&self, role: &str, deck: Option<&Path>, a: Args) -> (Key, Vec<String>) {
        let res = self.run_route(role, deck, a).ok();
        let stdout = res.stdout_text();
        let lines: Vec<String> = stdout.lines().map(str::to_string).collect();
        assert!(!lines.is_empty(), "{res}");
        let words: Vec<&str> = lines[0].split(' ').collect();
        assert_eq!(words.len(), 3, "{res}");
        (key(words[0], words[1], words[2]), lines[1..].to_vec())
    }

    fn r1(&self) -> (Key, Vec<String>) {
        self.r("implementer", None, Args::default())
    }
}

/// `exhausted`: exit 1, nothing on stdout, and stderr names the exhausted ladder.
macro_rules! assert_exhausted {
    ($res:expr) => {{
        let res: &Out = &$res;
        assert_eq!(res.code, 1, "{res}");
        assert_eq!(res.stdout_text(), "", "{res}");
        assert!(res.stderr_text().contains("exhaust"), "{res}");
    }};
}

fn has(reasons: &[String], reason: &str) -> bool {
    reasons.iter().any(|r| r == reason)
}

fn attempt(n: i64) -> Args<'static> {
    Args {
        attempt: Some(n),
        ..Args::default()
    }
}

mod route {
    use super::*;

    #[test]
    fn test_standard_implementer_uses_codex_terra_high() {
        let t = RouteCase::new();
        let (key, reasons) = t.r1();
        assert_eq!(key, super::key("codex", "gpt-6.1-terra", "high"));
        assert!(has(&reasons, "vendor codex: prefer"), "{reasons:?}");
    }

    #[test]
    fn test_hard_card_uses_implementer_hard() {
        let mut t = RouteCase::new();
        let d = t.deck(CardChange {
            hard: true,
            ..CardChange::default()
        });
        let (key, reasons) = t.r("implementer", Some(&d), Args::default());
        assert_eq!(key, super::key("codex", "gpt-6.1-sol", "high"));
        assert!(
            has(&reasons, "role implementer_hard: card is Hard"),
            "{reasons:?}"
        );
    }

    #[test]
    fn test_reviewer_other_vendor_than_implementer() {
        let t = RouteCase::new();
        let last = |v| Args {
            last: Some(v),
            ..Args::default()
        };
        let (key, reasons) = t.r("reviewer", None, last("codex"));
        assert_eq!(key, super::key("claude", "sonnet", "medium"));
        assert!(
            has(&reasons, "vendor claude: other than codex"),
            "{reasons:?}"
        );
        let (key, _) = t.r("reviewer", None, last("claude"));
        assert_eq!(key.0, "codex");
        // only one vendor installed: the same vendor is used
        let (key, _) = t.r(
            "reviewer",
            None,
            Args {
                last: Some("codex"),
                installed: "codex",
                ..Args::default()
            },
        );
        assert_eq!(key.0, "codex");
    }

    #[test]
    fn test_high_risk_reviewer_uses_reviewer_hard_plus_one() {
        let mut t = RouteCase::new();
        let d = t.deck(CardChange {
            risk: Some("high"),
            ..CardChange::default()
        });
        let (key, reasons) = t.r(
            "reviewer",
            Some(&d),
            Args {
                last: Some("codex"),
                ..Args::default()
            },
        );
        assert_eq!(key, super::key("claude", "opus", "xhigh"));
        assert!(has(&reasons, "risk high: +1"), "{reasons:?}");
    }

    #[test]
    fn test_profile_lean_lowers_every_role() {
        let mut t = RouteCase::new();
        t.cfg("profile = \"lean\"\n");
        let (key, reasons) = t.r1();
        assert_eq!(key.2, "medium");
        assert!(has(&reasons, "profile lean: -1"), "{reasons:?}");
        let (key, _) = t.r("verifier", None, Args::default());
        assert_eq!(key.2, "low");
        // the deck's profile overrides the config's
        let d = t.deck(CardChange {
            profile: Some("thorough"),
            ..CardChange::default()
        });
        let (key, reasons) = t.r("implementer", Some(&d), Args::default());
        assert_eq!(key.2, "xhigh");
        assert!(has(&reasons, "profile thorough: +1"), "{reasons:?}");
    }

    #[test]
    fn test_pin_wins_for_first_two_attempts() {
        // Covers R11: a pinned xhigh stays xhigh at attempt 2; a pinned max is kept, never
        // created.
        let mut t = RouteCase::new();
        let pinned = t.deck(CardChange {
            pin: Some("claude opus xhigh"),
            ..CardChange::default()
        });
        let (key, reasons) = t.r("implementer", Some(&pinned), Args::default());
        assert_eq!(key, super::key("claude", "opus", "xhigh"));
        assert!(has(&reasons, "pinned: claude opus xhigh"), "{reasons:?}");
        let (key, reasons) = t.r("implementer", Some(&pinned), attempt(2));
        assert_eq!(key, super::key("claude", "opus", "xhigh"));
        assert!(has(&reasons, "clamped to xhigh"), "{reasons:?}");
        let maxed = t.deck(CardChange {
            pin: Some("claude opus max"),
            ..CardChange::default()
        });
        assert_eq!(t.r("implementer", Some(&maxed), attempt(2)).0.2, "max");
        let (key, _) = t.r("implementer", Some(&pinned), attempt(3));
        assert_eq!(key, super::key("codex", "gpt-6.1-sol", "xhigh")); // high +1 (pinned)
    }

    #[test]
    fn test_ladder_attempts_1_to_4_then_exhausted() {
        let mut t = RouteCase::new();
        let [a1, a2, a3, a4] = [1, 2, 3, 4].map(|n| t.r("implementer", None, attempt(n)).0);
        assert_eq!(a1, super::key("codex", "gpt-6.1-terra", "high"));
        assert_eq!(a2, super::key("codex", "gpt-6.1-terra", "xhigh"));
        assert_eq!(a3, super::key("codex", "gpt-6.1-sol", "high"));
        assert_eq!(a4, super::key("claude", "opus", "high"));
        assert!(has(
            &t.r("implementer", None, attempt(2)).1,
            "attempt 2: +1"
        ));
        assert!(has(
            &t.r("implementer", None, attempt(3)).1,
            "role implementer_hard: attempt 3"
        ));
        assert_exhausted!(t.run_route("implementer", None, attempt(5)));
        // a Hard card gets one more step at attempt 3
        let hard = t.deck(CardChange {
            hard: true,
            ..CardChange::default()
        });
        assert_eq!(t.r("implementer", Some(&hard), attempt(3)).0.2, "xhigh");
        assert_eq!(t.r("implementer", Some(&hard), attempt(4)).0.2, "xhigh");
    }

    #[test]
    fn test_attempt_4_with_prefer_other_switches_from_attempt_3s_actual_vendor() {
        // implementer_hard prefers "other": attempt 4 must leave the vendor attempt 3 really
        // ran on (passed as --last-vendor), not a recomputed guess.
        let t = RouteCase::new();
        t.cfg("[roles.implementer_hard]\nprefer = \"other\"\n");
        for (third, fourth) in [("claude", "codex"), ("codex", "claude")] {
            let (key, reasons) = t.r(
                "implementer",
                None,
                Args {
                    attempt: Some(4),
                    last: Some(third),
                    ..Args::default()
                },
            );
            assert_eq!(key.0, fourth, "attempt 3 on {third}");
            assert!(
                has(&reasons, &format!("vendor {fourth}: other than {third}")),
                "attempt 3 on {third}: {reasons:?}"
            );
        }
    }

    #[test]
    fn test_claude_only_install_exhausts_at_attempt_4() {
        let t = RouteCase::new();
        let claude = |n| Args {
            attempt: n,
            installed: "claude",
            ..Args::default()
        };
        let (key, reasons) = t.r("implementer", None, claude(Some(1)));
        assert_eq!(key, super::key("claude", "sonnet", "medium"));
        assert!(
            has(&reasons, "vendor claude: codex not installed"),
            "{reasons:?}"
        );
        assert_eq!(t.r("implementer", None, claude(Some(3))).0.0, "claude");
        assert_exhausted!(t.run_route("implementer", None, claude(Some(4))));
        // no vendor installed: a configuration error
        let res = t.run_route(
            "implementer",
            None,
            Args {
                installed: "",
                ..Args::default()
            },
        );
        assert_eq!(res.code, 1, "{res}");
        assert_eq!(res.stdout_text(), "", "{res}");
        assert!(res.stderr_text().starts_with("error: "), "{res}");
    }

    #[test]
    fn test_effort_clamped_to_xhigh_unless_pinned_max() {
        // shift_effort's own cases are a Rust unit test; these are the resolutions.
        let mut t = RouteCase::new();
        t.cfg("profile = \"thorough\"\n");
        let d = t.deck(CardChange {
            risk: Some("high"),
            ..CardChange::default()
        });
        let (key, reasons) = t.r("implementer_hard", Some(&d), Args::default()); // high +1 +1 = max -> xhigh
        assert_eq!(key.2, "xhigh");
        assert!(has(&reasons, "clamped to xhigh"), "{reasons:?}");
        let d = t.deck(CardChange {
            risk: Some("high"),
            pin: Some("codex gpt-6.1-sol max"),
            ..CardChange::default()
        });
        let (key, reasons) = t.r("implementer", Some(&d), Args::default());
        assert_eq!(key.2, "max");
        assert!(!has(&reasons, "clamped to xhigh"), "{reasons:?}");
    }

    #[test]
    fn test_pin_requires_installed_vendor() {
        let mut t = RouteCase::new();
        let d = t.deck(CardChange {
            pin: Some("codex gpt-6.1-sol high"),
            ..CardChange::default()
        });
        let res = t.run_route(
            "implementer",
            Some(&d),
            Args {
                installed: "claude",
                ..Args::default()
            },
        );
        assert_eq!(res.code, 1, "{res}");
        // `^error: .*pinned to codex, which is not installed`: on stderr's first line.
        let stderr = res.stderr_text();
        let first = stderr.split('\n').next().unwrap_or("");
        assert!(
            first
                .strip_prefix("error: ")
                .is_some_and(|rest| rest.contains("pinned to codex, which is not installed")),
            "{res}"
        );
    }

    #[test]
    fn test_invalid_profile_is_config_error() {
        let mut t = RouteCase::new();
        let d = t.deck(CardChange {
            profile: Some("Thorough"),
            ..CardChange::default()
        });
        let res = t.run_route("implementer", Some(&d), Args::default());
        assert_eq!(res.code, 1, "{res}");
        assert!(res.stderr_text().starts_with("error: "), "{res}");
        assert!(res.stderr_text().contains("Thorough"), "{res}");
    }

    #[test]
    fn test_low_pin_not_shifted_by_profile_or_risk() {
        let mut t = RouteCase::new();
        t.cfg("profile = \"thorough\"\n");
        let d = t.deck(CardChange {
            risk: Some("high"),
            pin: Some("claude sonnet low"),
            ..CardChange::default()
        });
        let (key, _) = t.r("implementer", Some(&d), Args::default());
        assert_eq!(key, super::key("claude", "sonnet", "low"));
    }

    #[test]
    fn test_cli_route_prints_route_and_reasons() {
        let t = RouteCase::new();
        let r = t.run_route("implementer", None, Args::default()).ok();
        let stdout = r.stdout_text();
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], "codex gpt-6.1-terra high", "{r}");
        assert!(lines[1..].contains(&"vendor codex: prefer"), "{r}");
        let r = t.run_route("implementer", None, attempt(5));
        assert_eq!(r.code, 1, "{r}");
        assert_eq!(r.stdout_text(), "", "{r}");
        assert!(r.stderr_text().contains("exhaust"), "{r}");
        let r = common::cli(
            &os_args![
                "route",
                fixture(),
                "99",
                "implementer",
                "--repo",
                t.tmp.path(),
                "--installed",
                BOTH
            ],
            &t.env,
            &t.tmp,
        );
        assert_eq!(r.code, 1, "{r}");
        assert!(r.stderr_text().contains("no card 99"), "{r}");
    }
}

mod security_gate {
    use crate::common::json::{self, Json};
    use crate::common::supervisor::{CardOpts, FakeSupervisor, Options, card_with};

    #[test]
    fn test_security_uses_reviewer_hard() {
        // `route` has no security input; the end-of-run security gate resolves the reviewer
        // with security set. A medium-risk card makes the gate run and adds no risk step.
        let mut w = FakeSupervisor::with_options(
            &[card_with(
                1,
                CardOpts {
                    risk: "medium",
                    ..CardOpts::default()
                },
            )],
            Options {
                installed: &["claude", "codex"],
                gates: true,
                ..Options::default()
            },
        );
        w.start();
        let actions = w.run();
        assert_json_eq!(actions.last().unwrap()["action"], Json::from("done"));
        let gates: Vec<&Json> = w
            .run_started
            .iter()
            .filter(|a| a["kind"] == "security")
            .collect();
        assert_eq!(gates.len(), 1, "{:?}", w.run_started);
        assert_json_eq!(gates[0]["role"], Json::from("reviewer_hard"));
        assert_json_eq!(
            gates[0]["route"],
            json::parse(br#"{"vendor": "claude", "model": "opus", "effort": "high"}"#)
        );
    }
}
