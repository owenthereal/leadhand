//! The harness's own tests: what a failing or hung binary reports, the sealed environment,
//! the tests' isolation from the crate's library, the tags, the JSON decoder and the fake
//! `codex` and `claude`. Static: these never run LEADHAND_BIN (shell scripts stand in for it).

#[macro_use]
mod common;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::json::{self, Json};
use common::{Env, TempRoot, read_jsonl, run_argv};

const TIMEOUT: Duration = Duration::from_secs(120);

fn fake_binary(tmp: &Path, body: &str) -> PathBuf {
    let path = tmp.join("fake leadhand");
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

// --------------------------------------------------------------------------
// What a failing or hung binary reports

#[test]
fn failure_names_argv_code_and_stderr() {
    let tmp = TempRoot::new("lh self ");
    let env = Env::new(tmp.path());
    let binary = fake_binary(tmp.path(), "echo boom >&2\nexit 4\n");
    let r = run_argv(
        &os_args![binary, "first arg", "--flag"],
        &env,
        tmp.path(),
        None,
        TIMEOUT,
    );
    let want: Vec<OsString> = vec![binary.clone().into(), "first arg".into(), "--flag".into()];
    assert_eq!((r.argv.clone(), r.code), (want, 4));
    let text = assert_panics!(r.ok());
    for part in [
        binary.to_str().unwrap(),
        "first arg",
        "--flag",
        "exit code: 4",
        "boom",
    ] {
        assert!(text.contains(part), "{part:?} not in {text}");
    }
}

#[test]
fn output_that_is_not_json_names_argv_and_stdout() {
    let tmp = TempRoot::new("lh self ");
    let env = Env::new(tmp.path());
    let binary = fake_binary(tmp.path(), "echo not json\n");
    let r = run_argv(&os_args![binary, "next"], &env, tmp.path(), None, TIMEOUT);
    let text = assert_panics!(r.json());
    for part in [binary.to_str().unwrap(), "next", "not JSON", "not json"] {
        assert!(text.contains(part), "{part:?} not in {text}");
    }
}

#[test]
fn hung_binary_times_out() {
    // The sleeper is a child of the script, holding its stdout and stderr open: killing only
    // the script would leave the harness waiting on the pipes for 30 s.
    let tmp = TempRoot::new("lh self ");
    let env = Env::new(tmp.path());
    let binary = fake_binary(tmp.path(), "sleep 30 &\necho $! > \"$1\"\nwait\n");
    let pid_file = tmp.path().join("sleeper pid");
    let began = Instant::now();
    let text = assert_panics!(run_argv(
        &os_args![binary, pid_file],
        &env,
        tmp.path(),
        None,
        Duration::from_secs(1)
    ));
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "took {:?}",
        began.elapsed()
    );
    for part in [
        binary.to_str().unwrap(),
        pid_file.to_str().unwrap(),
        "timed out after 1",
    ] {
        assert!(text.contains(part), "{part:?} not in {text}");
    }
    let sleeper: i32 = text_of_pid(&pid_file);
    let deadline = Instant::now() + Duration::from_secs(2);
    while common::start_time(sleeper).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        common::start_time(sleeper).is_none(),
        "sleeper {sleeper} outlived the timeout"
    );
}

fn text_of_pid(path: &Path) -> i32 {
    text(path)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// --------------------------------------------------------------------------
// A path the binary prints for itself is compared by resolved file, not spelling (S1-R11)

struct SelfPath {
    _tmp: TempRoot,
    real: PathBuf,
    linked: PathBuf,
    other: PathBuf,
}

fn self_path() -> SelfPath {
    let tmp = TempRoot::new("lh self ");
    let real = fake_binary(tmp.path(), "exit 0\n");
    std::os::unix::fs::symlink(tmp.path(), tmp.path().join("link dir")).unwrap();
    let linked = tmp.path().join("link dir").join(real.file_name().unwrap());
    let other = tmp.path().join("other leadhand");
    fs::write(&other, "#!/bin/sh\n").unwrap();
    SelfPath {
        _tmp: tmp,
        real,
        linked,
        other,
    }
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn names_bin_accepts_any_spelling_of_bin_and_rejects_another_file() {
    let p = self_path();
    assert_ne!(p.linked, p.real);
    assert!(common::names_file(&p.real, s(&p.real)));
    assert!(common::names_file(&p.real, s(&p.linked)));
    assert!(!common::names_file(&p.real, s(&p.other)));
}

#[test]
fn bin_command_checks_the_rest_of_the_line_exactly() {
    let p = self_path();
    let (before, after) = ("next: ", " unpark '/a run' --card 1");
    let line = |path: &Path| format!("{before}{}{after}", common::shlex_quote(s(path)));
    let is = |text: &str| common::is_file_command(&p.real, text, before, after);
    assert!(is(&line(&p.linked)));
    assert!(!is(&line(&p.other)));
    assert!(!is(&format!("{} x", line(&p.real))));
    assert!(!is(&format!(
        "Next: {}{after}",
        common::shlex_quote(s(&p.real))
    )));
    assert!(!is(&format!(
        "{before} {}{after}",
        common::shlex_quote(s(&p.real))
    )));
    // an unquoted path with a space is two shell words, not one
    assert!(!is(&format!("{before}{}{after}", s(&p.real))));
}

#[test]
fn bin_between_takes_the_path_verbatim_and_checks_the_rest_exactly() {
    let p = self_path();
    let (before, after) = ("leadhand CLI: ", ". Then go.");
    let between = |text: &str| common::names_file_between(&p.real, text, before, after);
    assert!(between(&format!("{before}{}{after}", s(&p.linked))));
    assert!(!between(&format!("{before}{}{after}", s(&p.other))));
    assert!(!between(&format!("{before}{}{after} x", s(&p.real))));
    let quoted = common::shlex_quote(&format!("{} x", s(&p.real)));
    assert!(!between(&format!("{before}{quoted}{after}")));
    assert!(!between(&format!("{before}{after}")));
}

// --------------------------------------------------------------------------
// The sealed environment

#[test]
fn inherited_hooks_are_dropped() {
    let tmp = TempRoot::new("lh self ");
    let inherited = [
        ("LEADHAND_CRASH_AT", "after_start"),
        ("LEADHAND_TEST_CLI", "/elsewhere/leadhand"),
        ("LEADHAND_TEST_CLOCK_OFFSET", "3600"),
        ("LEADHAND_TEST_CARD_LOOP_ONLY", "1"),
        ("LEADHAND_INSTALLED_VENDORS", "claude,codex"),
        ("LEADHAND_MINUTE_SECONDS", "1"),
        ("LEADHAND_RUN_DIR", "/elsewhere/run"),
        ("CLAUDE_CODE_SESSION_ID", "abc"),
        ("FAKE_CODEX_SCENARIO", "hang"),
        ("GIT_DIR", "/elsewhere/.git"),
        ("LEADHAND_RUST_COMMANDS", "check"),
    ];
    // The test process's own environment with these on top, as the Python test patched
    // os.environ; nothing here changes the process environment (S3-R19).
    let base = std::env::vars_os().chain(inherited.iter().map(|(k, v)| (k.into(), v.into())));
    let env = Env::from_base(base, tmp.path(), &["codex"], "");
    for name in [
        "LEADHAND_CRASH_AT",
        "LEADHAND_TEST_CLI",
        "LEADHAND_TEST_CLOCK_OFFSET",
        "LEADHAND_TEST_CARD_LOOP_ONLY",
        "LEADHAND_MINUTE_SECONDS",
        "LEADHAND_RUN_DIR",
        "CLAUDE_CODE_SESSION_ID",
        "FAKE_CODEX_SCENARIO",
        "GIT_DIR",
    ] {
        assert!(env.get(name).is_none(), "{name} was not dropped");
    }
    assert_eq!(
        env.get("LEADHAND_INSTALLED_VENDORS"),
        Some(OsStr::new("codex"))
    );
    assert_eq!(env.get("LEADHAND_RUST_COMMANDS"), Some(OsStr::new("check")));
    assert_eq!(env.get("LEADHAND_TESTING"), Some(OsStr::new("1")));
    assert_eq!(env.get("GIT_CONFIG_GLOBAL"), Some(OsStr::new("/dev/null")));
    let config = PathBuf::from(env.get("LEADHAND_CONFIG").unwrap());
    assert_eq!(config, tmp.path().join("user config.toml"));
    let docs = format!("docs_dir = \"{}\"", tmp.path().join("docs dir").display());
    assert!(
        text(&config).contains(&docs),
        "{docs} not in {}",
        text(&config)
    );
}

// --------------------------------------------------------------------------
// The tests use the crate's library only as `leadhand::procs`

/// The text of Rust source with comments and string and character literals blanked out
/// (newlines kept), so only code is searched.
fn code_only(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let mut i = 0;
    while i < c.len() {
        let prev_ident = i > 0 && ident(c[i - 1]);
        if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
        } else if !prev_ident && raw_string_start(&c, i).is_some() {
            let (hashes, quote) = raw_string_start(&c, i).unwrap();
            let mut j = quote + 1;
            while j < c.len() && !(c[j] == '"' && (1..=hashes).all(|h| c.get(j + h) == Some(&'#')))
            {
                j += 1;
            }
            let end = (j + 1 + hashes).min(c.len());
            for &ch in &c[i..end] {
                blank(&mut out, ch);
            }
            i = end;
        } else if c[i] == '"' {
            blank(&mut out, c[i]);
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' {
                    blank(&mut out, c[i]);
                    i += 1;
                }
                if i < c.len() {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
            if i < c.len() {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '\'' && (c.get(i + 1) == Some(&'\\') || c.get(i + 2) == Some(&'\'')) {
            // A character literal ('x' or an escape); a lifetime has no closing quote.
            let mut j = i + 1;
            if c[j] == '\\' {
                j += 2;
            }
            while j < c.len() && c[j] != '\'' {
                j += 1;
            }
            let end = (j + 1).min(c.len());
            for &ch in &c[i..end] {
                blank(&mut out, ch);
            }
            i = end;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// For `r"`, `r#"`, `br"`, `cr#"` and the like at `i`: the number of `#`s and the index of
/// the opening quote.
fn raw_string_start(c: &[char], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if matches!(c.get(j), Some('b' | 'c')) {
        j += 1;
    }
    if c.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = c[j..].iter().take_while(|&&ch| ch == '#').count();
    (c.get(j + hashes) == Some(&'"')).then_some((hashes, j + hashes))
}

/// Every use of the crate's library in `src` other than `leadhand::procs`: each
/// `leadhand::<item>` path, each item of a `leadhand::{...}` group, and a `use` or
/// `extern crate` of the whole crate. Comments and literals are not code.
fn crate_uses(src: &str) -> Vec<String> {
    let code = code_only(src);
    let c: Vec<char> = code.chars().collect();
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    let word_at = |i: usize| -> String { c[i..].iter().take_while(|&&ch| ident(ch)).collect() };
    let skip_ws = |mut i: usize| {
        while i < c.len() && c[i].is_whitespace() {
            i += 1;
        }
        i
    };
    let mut found = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !ident(c[i]) {
            i += 1;
            continue;
        }
        let word = word_at(i);
        let start = i;
        i += word.chars().count();
        if word != "leadhand" || (start > 0 && ident(c[start - 1])) {
            continue;
        }
        let before: String = c[..start].iter().collect();
        let before = before.trim_end();
        let j = skip_ws(i);
        if c.get(j) == Some(&':') && c.get(j + 1) == Some(&':') {
            let k = skip_ws(j + 2);
            if c.get(k) == Some(&'{') {
                // The group's top-level items, each with any nested group of its own
                // (`procs::{identify}`), up to the brace that closes the group.
                let mut items = vec![String::new()];
                let mut depth = 0;
                for &ch in &c[k + 1..] {
                    match ch {
                        '{' => depth += 1,
                        '}' if depth == 0 => break,
                        '}' => depth -= 1,
                        ',' if depth == 0 => {
                            items.push(String::new());
                            continue;
                        }
                        _ => {}
                    }
                    items.last_mut().unwrap().push(ch);
                }
                for item in items.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    let head: String = item.chars().take_while(|&ch| ident(ch)).collect();
                    if head != "procs" {
                        found.push(format!("leadhand::{{{item}}}"));
                    }
                }
            } else {
                let item = if c.get(k) == Some(&'*') {
                    "*".to_string()
                } else {
                    word_at(k)
                };
                if item != "procs" {
                    found.push(format!("leadhand::{item}"));
                }
            }
        } else if before.ends_with("use") || before.ends_with("extern crate") {
            found.push("leadhand".to_string());
        }
    }
    found
}

/// Every `.rs` file under `dir`, recursively, sorted.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension() == Some(OsStr::new("rs")) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// True for the live tests' files: `tests/live*.rs` and everything under `tests/live*/`.
fn is_live(tests: &Path, path: &Path) -> bool {
    path.strip_prefix(tests)
        .ok()
        .and_then(|rel| rel.components().next())
        .is_some_and(|first| first.as_os_str().as_bytes().starts_with(b"live"))
}

fn uses_in(files: &[PathBuf], tests: &Path) -> Vec<String> {
    files
        .iter()
        .flat_map(|p| {
            let rel = p.strip_prefix(tests).unwrap().display().to_string();
            crate_uses(&text(p))
                .into_iter()
                .map(move |u| format!("{rel}: {u}"))
        })
        .collect()
}

#[test]
fn contract_tests_use_the_crate_only_for_procs() {
    let samples: [(&str, &[&str]); 11] = [
        (
            "use leadhand::{procs::{identify}, engine};",
            &["leadhand::{engine}"],
        ),
        (
            "use leadhand::{procs::{identify, same_process}, wire::{a, b}};",
            &["leadhand::{wire::{a, b}}"],
        ),
        ("use leadhand::engine;", &["leadhand::engine"]),
        ("use leadhand::procs;", &[]),
        ("    let x = leadhand::state::read();", &["leadhand::state"]),
        (
            "use leadhand::{procs, wire::Record};",
            &["leadhand::{wire::Record}"],
        ),
        ("use leadhand::procs::{identify, same_process};", &[]),
        ("use leadhand;", &["leadhand"]),
        ("// use leadhand::engine;", &[]),
        (
            "let s = \"leadhand::engine\"; let r = r#\"leadhand::git\"#;",
            &[],
        ),
        ("use leadhand_fake::engine; let my_leadhand = 1;", &[]),
    ];
    for (line, want) in samples {
        assert_eq!(crate_uses(line), want, "{line}");
    }
    let tests = manifest().join("tests");
    let files: Vec<PathBuf> = rust_files(&tests)
        .into_iter()
        .filter(|p| !is_live(&tests, p))
        .collect();
    assert!(
        files.contains(&tests.join("common").join("mod.rs")),
        "{files:?}"
    );
    assert!(files.contains(&tests.join("selfcheck.rs")), "{files:?}");
    assert_eq!(uses_in(&files, &tests), Vec::<String>::new());
}

#[test]
fn live_tests_use_the_crate_only_for_procs() {
    let tests = manifest().join("tests");
    for (rel, want) in [
        ("live.rs", true),
        ("live/fixture.rs", true),
        ("launch.rs", false),
    ] {
        assert_eq!(is_live(&tests, &tests.join(rel)), want, "{rel}");
    }
    assert_eq!(
        crate_uses("use leadhand::launch::run;"),
        ["leadhand::launch"]
    );
    let files: Vec<PathBuf> = rust_files(&tests)
        .into_iter()
        .filter(|p| is_live(&tests, p))
        .collect();
    assert_eq!(uses_in(&files, &tests), Vec::<String>::new());
}

// --------------------------------------------------------------------------
// Tags

/// "ran" or "skipped": what a rust-only test does under LEADHAND_IMPL=`value`.
fn outcome(value: Option<&str>) -> &'static str {
    if common::rust_only_under(value.map(OsStr::new)) {
        "skipped"
    } else {
        "ran"
    }
}

#[test]
fn rust_only_skips_under_python() {
    for (value, want) in [
        (None, "ran"),
        (Some(""), "ran"),
        (Some("rust"), "ran"),
        (Some("python"), "skipped"),
    ] {
        assert_eq!(outcome(value), want, "LEADHAND_IMPL={value:?}");
    }
    // rust_only() itself reads this process's LEADHAND_IMPL; calling it here would read to
    // check_conversion.py as a skip.
    let this_process = std::env::var_os("LEADHAND_IMPL");
    assert_eq!(
        common::rust_only_under(this_process.as_deref()),
        common::impl_is_python()
    );
}

// --------------------------------------------------------------------------
// The JSON decoder

#[test]
fn json_parse_keeps_lone_surrogate_as_wtf8() {
    let v = json::parse(br#""a\udcffb""#);
    assert_eq!(v, Json::Str(vec![0x61, 0xed, 0xb3, 0xbf, 0x62]));
    assert_eq!(
        json::to_os(v.as_bytes().unwrap()),
        Some(OsString::from_vec(vec![0x61, 0xff, 0x62]))
    );
    // Only U+DC80..U+DCFF stand for bytes; another lone surrogate is no file name.
    assert_eq!(
        json::to_os(json::parse(br#""\ud800""#).as_bytes().unwrap()),
        None
    );
    let obj = json::parse(br#"{"k\udcff": ["\u00e9", 1.50, -0, null, true]}"#);
    let want = Json::Obj(vec![(
        b"k\xed\xb3\xbf".to_vec(),
        Json::Arr(vec![
            Json::Str("é".into()),
            Json::Num("1.50".into()),
            Json::Num("-0".into()),
            Json::Null,
            Json::Bool(true),
        ]),
    )]);
    assert_eq!(obj, want);
}

#[test]
fn json_parse_keeps_distinct_strings_distinct() {
    assert_ne!(
        json::parse(br#""\u00e9""#),
        json::parse(br#""\udcc3\udca9""#)
    );
    assert_ne!(
        json::parse(br#""\ud800""#),
        json::parse(br#""\udced\udca0\udc80""#)
    );
    assert_eq!(json::parse(br#""\ud83d\ude00""#), Json::Str("😀".into()));
}

#[test]
fn json_python_equality_compares_dicts_and_numbers_as_python_does() {
    let p = |text: &str| json::parse(text.as_bytes());
    // Equal under Python's ==, though not under the derived (textual, ordered) ==.
    for (a, b) in [
        (r#"{"a": 1, "b": [2, "x"]}"#, r#"{"b": [2.0, "x"], "a": 1}"#),
        (r#"{"a": 1, "a": 2}"#, r#"{"a": 2}"#),
        ("1.5", "1.50"),
        ("1", "1.0"),
        ("-0", "0.0"),
        ("true", "1"),
        ("100", "1e2"),
        ("12345678901234567890", "12345678901234567890"),
    ] {
        assert!(json::py_eq(&p(a), &p(b)), "{a} == {b}");
        assert_json_eq!(p(a), &p(b), "{a} == {b}");
    }
    // Unequal under Python's == too.
    for (a, b) in [
        ("[1, 2]", "[2, 1]"),
        (r#"{"a": 1}"#, r#"{"a": 1, "b": 2}"#),
        (r#"{"a": 1, "a": 2}"#, r#"{"a": 1}"#),
        ("1.5", "1"),
        ("9007199254740993", "9007199254740992.0"),
        ("null", "0"),
        (r#""1""#, "1"),
        (r#""é""#, r#""\udcc3\udca9""#),
    ] {
        assert!(!json::py_eq(&p(a), &p(b)), "{a} != {b}");
    }
    assert_ne!(p("1.5"), p("1.50"), "the derived == stays textual");
    let text = assert_panics!(assert_json_eq!(p("[1, 2]"), p("[2, 1]")));
    assert!(
        text.contains("left: [1, 2]") && text.contains("right: [2, 1]"),
        "{text}"
    );
}

// --------------------------------------------------------------------------
// The fake binaries

#[test]
fn fake_codex_replays_scenario_and_logs_argv() {
    let tmp = TempRoot::new("lh self ");
    let base = Env::new(tmp.path());
    let work = tmp.path().join("work dir");
    fs::create_dir(&work).unwrap();
    let scenario = tmp.path().join("scenario.jsonl");
    fs::write(
        &scenario,
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"{thread}\"}\n",
            "\n",
            "{\"action\": \"stderr\", \"text\": \"warn \\u00e9\"}\n",
            "{\"action\": \"last_message\", \"text\": \"done\\n\"}\n",
            "{\"action\": \"write_file\", \"path\": \"sub/out.txt\", \"text\": \"hi\"}\n",
            "{\"type\":\"item\",\"n\":1.50,\"big\":12345678901234567890,\"z\":-0,\"e\":1e16,",
            "\"s\":\"\\u00e9\\udcff\\ud83d\\ude00\",\"l\":[true,null,{}],\"n\":2}\n",
            "{\"action\": \"exit\", \"code\": 3}\n",
            "{\"type\": \"never printed\"}\n",
        ),
    )
    .unwrap();
    let log = tmp.path().join("codex log.jsonl");
    let last = tmp.path().join("last message.md");
    let env = base
        .with("FAKE_CODEX_LOG", &log)
        .with("FAKE_CODEX_SCENARIO", &scenario)
        .with("FAKE_CODEX_THREAD", "not this one");
    let codex = base.fakes().join("codex");
    let policy = "shell_environment_policy.set.LEADHAND_X=\"a b\"";
    let args = os_args![
        codex,
        "exec",
        "--json",
        "-C",
        work,
        "-o",
        last,
        "-c",
        policy,
        "-c",
        "model=\"x\"",
        "resume",
        "thread-7",
        "-"
    ];
    let r = run_argv(
        &args,
        &env,
        tmp.path(),
        Some("caf\u{e9}\r\nline two\r"),
        TIMEOUT,
    );
    assert_eq!(r.code, 3, "{r}");
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        concat!(
            "{\"type\": \"thread.started\", \"thread_id\": \"thread-7\"}\n",
            "{\"type\": \"item\", \"n\": 2, \"big\": 12345678901234567890, \"z\": 0, \"e\": 1e+16, ",
            "\"s\": \"\\u00e9\\udcff\\ud83d\\ude00\", \"l\": [true, null, {}]}\n",
        )
    );
    assert_eq!(String::from_utf8_lossy(&r.stderr), "warn \u{e9}\n");
    assert_eq!(text(&last), "done\n");
    assert_eq!(text(&work.join("sub").join("out.txt")), "hi");

    let lines: Vec<String> = text(&log).lines().map(str::to_string).collect();
    let recs = read_jsonl(&log);
    assert_eq!(recs.len(), 2, "{lines:?}");
    let start = &recs[0];
    assert_eq!(
        start.keys(),
        [
            "time",
            "pid",
            "event",
            "argv",
            "cwd",
            "env",
            "policy_set",
            "prompt",
            "pgid"
        ]
    );
    let argv: Vec<String> = args[1..]
        .iter()
        .map(|a| a.to_str().unwrap().to_string())
        .collect();
    assert_eq!(start["argv"], Json::from(argv.as_slice()));
    let cwd = fs::canonicalize(tmp.path()).unwrap();
    assert_eq!(start["cwd"].as_str(), cwd.to_str());
    assert_eq!(start["env"]["FAKE_CODEX_LOG"].as_str(), log.to_str());
    assert_eq!(start["env"]["LEADHAND_TESTING"].as_str(), Some("1"));
    assert_eq!(
        start["pid"], start["pgid"],
        "the harness runs it in its own process group"
    );
    // Python's json.dumps, byte for byte: its separators and ASCII escapes; the prompt as
    // read, carriage returns kept (Python's sys.stdin.read() translates no newlines).
    let tail =
        "\"policy_set\": {\"LEADHAND_X\": \"a b\"}, \"prompt\": \"caf\\u00e9\\r\\nline two\\r\", ";
    assert!(lines[0].contains(tail), "{}", lines[0]);
    let argv_text = format!(
        "\"argv\": [\"exec\", \"--json\", \"-C\", \"{}\", ",
        work.display()
    );
    assert!(lines[0].contains(&argv_text), "{}", lines[0]);
    let pid = start["pid"].as_i64().unwrap();
    assert!(lines[1].ends_with(&format!(
        ", \"pid\": {pid}, \"event\": \"exit\", \"how\": \"scenario\", \"code\": 3}}"
    )));
    let time = &lines[1][..lines[1].find(',').unwrap()];
    assert!(
        time.starts_with("{\"time\": 1") && time.contains('.'),
        "{time}"
    );
}

#[test]
fn fake_claude_logs_argv_cwd_and_env() {
    let tmp = TempRoot::new("lh self ");
    let base = Env::new(tmp.path());
    let log = tmp.path().join("claude log.jsonl");
    let spawned = tmp.path().join("spawned pid");
    let spawn = format!(
        "[\"/bin/sh\", \"-c\", \"echo $$ > \\\"$0\\\"\", \"{}\"]",
        spawned.display()
    );
    let env = base
        .with("PATH", base.path_with_fakes())
        .with("FAKE_CLAUDE_LOG", &log)
        .with("FAKE_CLAUDE_EXIT", "5")
        .with("FAKE_CLAUDE_SPAWN", &spawn)
        .with("LEADHAND_RUN_ID", "run 1")
        .with("LEADHAND_CLI", "/x/lead\u{e9}hand");
    let cwd = tmp.path().join("some dir");
    fs::create_dir(&cwd).unwrap();
    // Found on PATH, as the launcher finds it: argv[0] is the path PATH found.
    let r = run_argv(
        &os_args!["claude", "--agent", "a b"],
        &env,
        &cwd,
        None,
        TIMEOUT,
    );
    assert_eq!(r.code, 5, "{r}");
    let lines: Vec<String> = text(&log).lines().map(str::to_string).collect();
    let recs = read_jsonl(&log);
    assert_eq!(recs.len(), 2, "{lines:?}");
    let start = &recs[0];
    assert_eq!(start.keys(), ["time", "pid", "ppid", "argv", "cwd", "env"]);
    let claude = base.fakes().join("claude");
    let want: [&str; 3] = [claude.to_str().unwrap(), "--agent", "a b"];
    assert_eq!(start["argv"], Json::from(&want[..]));
    assert_eq!(start["ppid"].as_i64(), Some(i64::from(std::process::id())));
    assert_eq!(
        start["cwd"].as_str(),
        fs::canonicalize(&cwd).unwrap().to_str()
    );
    assert!(
        lines[0].ends_with(concat!(
            "\"env\": {\"LEADHAND_RUN_ID\": \"run 1\", \"LEADHAND_RUN_DIR\": null, ",
            "\"LEADHAND_CLI\": \"/x/lead\\u00e9hand\"}}"
        )),
        "{}",
        lines[0]
    );
    let pid = start["pid"].as_i64().unwrap();
    assert_eq!(recs[1].keys(), ["time", "pid", "spawned"]);
    assert_eq!(recs[1]["pid"].as_i64(), Some(pid));
    let child = recs[1]["spawned"].as_i64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&spawned).is_ok_and(|t| t.ends_with('\n'))
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(i64::from(text_of_pid(&spawned)), child);
}
