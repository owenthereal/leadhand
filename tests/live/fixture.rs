//! The live runs' fixture: a small Git repository, its bare origin, a deck and the config.
//! The Rust form of the Python live driver's fixture builder, which this replaced.
//!
//! The fixture repository is a tiny Python package: its deck's `Build:` and `Test:` lines
//! run `python3`. That is the fixture project's own toolchain, made in a temporary folder at
//! run time, never leadhand's (ruling S3-R12). Every such string lives in this file and in
//! the kept deck, `tests/e2e/fixture-deck.md`; the drive and the push probe take them from
//! here.
//!
//! `make_fixture(root, slow_card2)` builds, under `root` (which must be empty):
//!
//! ```text
//! root/fixture repo/          Git repo, branch main: package calc/ with add(), unittest
//!                             tests, .gitignore, and .leadhand.toml (committed)
//! root/origin.git/            bare clone; the repo's `origin` remote (a local path)
//! root/leadhand docs/         docs_dir (runs go to "root/leadhand docs/fixture repo/runs/")
//! root/plans/fixture deck.md  tests/e2e/fixture-deck.md with the real base commit
//! root/user config.toml       empty: passed as LEADHAND_CONFIG, so the operator's own
//!                             ~/.config/leadhand/config.toml is never read
//! root/elsewhere/             an unrelated directory to launch from
//! ```
//!
//! Several paths contain spaces on purpose. Routes are the cheapest ones (Claude haiku or
//! sonnet at low or medium, Codex gpt-6-luna only); the two cards pin their implementer routes
//! in the deck. Fixture commits are made with commit signing off, and the repo's local config
//! turns signing off for the run's commits too. The only binary used is `git`.
//!
//! `slow_card2` (the interrupted run) gives card 2 more work, so its Codex worker runs long
//! enough to be interrupted.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::common::json::{self, Json};
use crate::common::{self, Group, Spawn};

const BASE_TOKEN: &str = "@BASE_SHA@";
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The fixture's source file the cards change, read back from the run branch.
pub const SOURCE_FILE: &str = "calc/__init__.py";
/// The fixture's own test command, run in the run's workspace after the run.
pub const TEST_ARGV: [&str; 6] = ["python3", "-m", "unittest", "discover", "-s", "tests"];

const CALC_INIT: &str = r#""""A tiny calculator: the leadhand end-to-end fixture."""


def add(a, b):
    """Return a + b."""
    return a + b
"#;

const TEST_CALC: &str = r#"import unittest

from calc import add


class AddTests(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)


if __name__ == "__main__":
    unittest.main()
"#;

const README: &str = "# calc

A tiny calculator used as leadhand's end-to-end fixture.

- Build: `python3 -m compileall -q calc`
- Test: `python3 -m unittest discover -s tests`
";

const GITIGNORE: &str = "__pycache__/\n*.pyc\n";

// The interrupted run needs card 2's Codex worker to live long enough to be killed, so its
// variant asks for more work on card 2. The deck file itself is left unchanged.
const CARD2_CONTEXT_END: &str = "and a unittest test for it in tests/test_calc.py.";
const SLOW_CARD2_EXTRA: &str = " Write the tests first and run the suite after each one: \
separate test methods for positive numbers, negative numbers, zero, floats \
(assertAlmostEqual), large integers and the identity multiply(x, 1) == x; give multiply() a \
docstring with two examples; run `python3 -m unittest discover -s tests -v` at the end and \
quote its last line in your report.";

// Cheapest routes. Every Codex route is gpt-6-luna. The deck pins card 1 to
// `claude sonnet low` and card 2 to `codex gpt-6-luna medium`.
const ROLES: [(&str, &[(&str, &str)]); 8] = [
    ("supervisor", &[("claude", "sonnet low")]),
    (
        "implementer",
        &[
            ("claude", "sonnet low"),
            ("codex", "gpt-6-luna medium"),
            ("prefer", "codex"),
        ],
    ),
    (
        "implementer_hard",
        &[
            ("claude", "sonnet low"),
            ("codex", "gpt-6-luna medium"),
            ("prefer", "codex"),
        ],
    ),
    (
        "reviewer",
        &[
            ("claude", "haiku medium"),
            ("codex", "gpt-6-luna medium"),
            ("prefer", "other"),
        ],
    ),
    (
        "reviewer_hard",
        &[
            ("claude", "haiku medium"),
            ("codex", "gpt-6-luna medium"),
            ("prefer", "other"),
        ],
    ),
    (
        "final_reviewer",
        &[
            ("claude", "sonnet low"),
            ("codex", "gpt-6-luna medium"),
            ("prefer", "claude"),
        ],
    ),
    (
        "verifier",
        &[
            ("claude", "haiku low"),
            ("codex", "gpt-6-luna low"),
            ("prefer", "claude"),
        ],
    ),
    (
        "scout",
        &[
            ("claude", "haiku low"),
            ("codex", "gpt-6-luna low"),
            ("prefer", "claude"),
        ],
    ),
];

/// The fixture's paths and facts (the Python builder's returned dict).
#[derive(Clone, Debug)]
pub struct Fixture {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub origin: PathBuf,
    pub docs_dir: PathBuf,
    pub runs_dir: PathBuf,
    pub deck: PathBuf,
    pub user_config: PathBuf,
    pub elsewhere: PathBuf,
    pub base_sha: String,
    pub branch: String,
    pub slow_card2: bool,
}

impl Fixture {
    /// The fixture as the Python builder's dict, for `fixture.json`.
    pub fn to_json(&self) -> Json {
        let p = |p: &Path| Json::Str(json::from_os(p.as_os_str().as_encoded_bytes()));
        Json::object([
            ("root", p(&self.root)),
            ("repo", p(&self.repo)),
            ("origin", p(&self.origin)),
            ("docs_dir", p(&self.docs_dir)),
            ("runs_dir", p(&self.runs_dir)),
            ("deck", p(&self.deck)),
            ("user_config", p(&self.user_config)),
            ("elsewhere", p(&self.elsewhere)),
            ("base_sha", Json::from(self.base_sha.as_str())),
            ("branch", Json::from(self.branch.as_str())),
            ("slow_card2", Json::from(self.slow_card2)),
        ])
    }
}

/// The plan's fixture line, for the temp folder placeholder `t`.
pub fn plan_line(t: &str, slow_card2: bool) -> String {
    format!(
        "1. fixture: fixture::make_fixture('{t}'{})  (repo '{t}/fixture repo', bare \
         '{t}/origin.git', docs_dir '{t}/leadhand docs', deck '{t}/plans/fixture deck.md')",
        if slow_card2 { ", slow_card2" } else { "" }
    )
}

/// A JSON string without the ASCII escapes (`json.dumps(value, ensure_ascii=False)`), which
/// is a valid TOML basic string.
fn toml_str(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The fixture repo's `.leadhand.toml`.
fn leadhand_toml(docs_dir: &Path) -> String {
    let docs = format!("{}/{{repo}}", docs_dir.display());
    let mut lines = vec![
        "# leadhand end-to-end fixture: cheapest routes only.".to_string(),
        "profile = \"standard\"".to_string(),
        format!("docs_dir = {}", toml_str(&docs)),
        String::new(),
    ];
    for (role, routes) in ROLES {
        lines.push(format!("[roles.{role}]"));
        for (key, value) in routes {
            lines.push(format!("{key} = {}", toml_str(value)));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

/// `git <args>` in `cwd`, with the operator's environment; its stdout, stripped. Err when it
/// fails or runs past 60 s.
fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let mut argv: Vec<&OsStr> = vec![OsStr::new("git")];
    argv.extend(args.iter().map(OsStr::new));
    let spawn = Spawn {
        group: Group::Own,
        on_spawn: None,
    };
    let what = format!("git {} failed in {}", args.join(" "), cwd.display());
    match common::try_run_process(&argv, common::process_env(), cwd, None, GIT_TIMEOUT, spawn) {
        Err(e) => Err(format!("{what}: {e}")),
        Ok(Err(_)) => Err(format!("{what}: timed out after 60 s")),
        Ok(Ok(out)) if out.code != 0 => Err(format!("{what}: {}", out.stderr_text().trim())),
        Ok(Ok(out)) => Ok(out.stdout_text().trim().to_string()),
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Builds the fixture under `root`, which must not exist or be empty.
pub fn make_fixture(root: &Path, slow_card2: bool) -> Result<Fixture, String> {
    let root = std::path::absolute(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if root.exists()
        && fs::read_dir(&root)
            .map_err(|e| format!("reading {}: {e}", root.display()))?
            .next()
            .is_some()
    {
        return Err(format!("{} exists and is not empty", root.display()));
    }
    fs::create_dir_all(&root).map_err(|e| format!("making {}: {e}", root.display()))?;
    let repo = root.join("fixture repo");
    let origin = root.join("origin.git");
    let docs = root.join("leadhand docs");
    let plans = root.join("plans");
    let deck = plans.join("fixture deck.md");
    let user_config = root.join("user config.toml");
    let elsewhere = root.join("elsewhere");
    for d in [
        repo.join("calc"),
        repo.join("tests"),
        docs.clone(),
        plans.clone(),
        elsewhere.clone(),
    ] {
        fs::create_dir_all(&d).map_err(|e| format!("making {}: {e}", d.display()))?;
    }
    write(&user_config, "")?;

    write(&repo.join("calc/__init__.py"), CALC_INIT)?;
    write(&repo.join("tests/test_calc.py"), TEST_CALC)?;
    write(&repo.join("README.md"), README)?;
    write(&repo.join(".gitignore"), GITIGNORE)?;
    write(&repo.join(".leadhand.toml"), &leadhand_toml(&docs))?;

    git(&repo, &["init", "--quiet", "-b", "main"])?;
    for (key, value) in [
        ("user.name", "leadhand e2e"),
        ("user.email", "e2e@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ] {
        git(&repo, &["config", "--local", key, value])?;
    }
    git(&repo, &["add", "-A"])?;
    git(
        &repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "calc: add()",
        ],
    )?;
    let base_sha = git(&repo, &["rev-parse", "HEAD"])?;

    let (repo_s, origin_s) = (common::path_str(&repo), common::path_str(&origin));
    git(&root, &["clone", "--quiet", "--bare", repo_s, origin_s])?;
    git(&repo, &["remote", "add", "origin", origin_s])?;
    git(&repo, &["fetch", "--quiet", "origin"])?;
    git(
        &repo,
        &[
            "branch",
            "--quiet",
            "--set-upstream-to",
            "origin/main",
            "main",
        ],
    )?;
    git(&repo, &["remote", "set-head", "origin", "main"])?;

    let template = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/fixture-deck.md");
    let mut text = fs::read_to_string(&template)
        .map_err(|e| format!("reading {}: {e}", template.display()))?;
    if text.matches(BASE_TOKEN).count() != 1 {
        return Err(format!(
            "{} must contain {BASE_TOKEN} exactly once",
            template.display()
        ));
    }
    text = text.replace(BASE_TOKEN, &base_sha);
    if slow_card2 {
        // Card 1's context ends the same way; card 2's is last.
        if text.matches(CARD2_CONTEXT_END).count() != 2 {
            return Err(format!(
                "{}: card 2's context ending not found",
                template.display()
            ));
        }
        let at = text.rfind(CARD2_CONTEXT_END).unwrap() + CARD2_CONTEXT_END.len();
        text.insert_str(at, SLOW_CARD2_EXTRA);
    }
    write(&deck, &text)?;

    Ok(Fixture {
        runs_dir: docs.join("fixture repo/runs"),
        root,
        repo,
        origin,
        docs_dir: docs,
        deck,
        user_config,
        elsewhere,
        base_sha,
        branch: "leadhand/calc-e2e".to_string(),
        slow_card2,
    })
}
