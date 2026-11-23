//! The harness's own tests: what a failing or hung binary reports, the sealed environment,
//! the tests' isolation from the crate's library, the JSON decoder and the fake
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
use common::rust_src::code_only;
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
    //
    // The timeout is 10 s, not the Python's 1 s: on a loaded machine the shell may not reach
    // its `echo` within 1 s, and the sleeper's pid file must exist to check the kill. The
    // time bound keeps the Python's meaning, the timeout plus less than the harness's 5 s
    // grace for the pipes: it holds only if the group kill closed the pipes at once, and it
    // stays far below the sleeper's 30 s.
    const HANG_SECS: u64 = 10;
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
        Duration::from_secs(HANG_SECS)
    ));
    assert!(
        began.elapsed() < Duration::from_secs(HANG_SECS + 4),
        "took {:?}",
        began.elapsed()
    );
    let timed_out = format!("timed out after {HANG_SECS} s");
    for part in [
        binary.to_str().unwrap(),
        pid_file.to_str().unwrap(),
        timed_out.as_str(),
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
        // The Python-era router's settings, which nothing reads since the cut-over.
        ("LEADHAND_RUST_COMMANDS", "check"),
        ("LEADHAND_RUST_BIN", "/elsewhere/leadhand"),
        ("LEADHAND_PY_BIN", "/elsewhere/reference"),
    ];
    // The test process's own environment with these on top, as the Python test patched
    // os.environ; nothing here changes the process environment (S3-R19). Without
    // PYTHONDONTWRITEBYTECODE, so the check below sees only what the harness sets.
    let base = std::env::vars_os()
        .filter(|(k, _)| k != "PYTHONDONTWRITEBYTECODE")
        .chain(inherited.iter().map(|(k, v)| (k.into(), v.into())));
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
        "LEADHAND_RUST_COMMANDS",
        "LEADHAND_RUST_BIN",
        "LEADHAND_PY_BIN",
    ] {
        assert!(env.get(name).is_none(), "{name} was not dropped");
    }
    // The harness sets nothing for Python any more.
    assert!(env.get("PYTHONDONTWRITEBYTECODE").is_none());
    assert_eq!(
        env.get("LEADHAND_INSTALLED_VENDORS"),
        Some(OsStr::new("codex"))
    );
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
    // Not vacuous: the scan covers the live test, its three modules by name (as the Python
    // named its two drivers), and every other file under tests/live/ (hidden ones, such as
    // a Finder's .DS_Store, aside).
    for rel in [
        "live.rs",
        "live/drive.rs",
        "live/fixture.rs",
        "live/push_probe.rs",
    ] {
        assert!(
            files.contains(&tests.join(rel)),
            "tests/{rel} was not scanned: {files:?}"
        );
    }
    let live = tests.join("live");
    let under_live: Vec<PathBuf> = common::walk(&live)
        .into_iter()
        .filter(|p| !p.is_dir())
        .filter(|p| {
            !p.strip_prefix(&live)
                .unwrap()
                .components()
                .any(|c| c.as_os_str().as_bytes().starts_with(b"."))
        })
        .collect();
    assert!(!under_live.is_empty(), "no file under tests/live/");
    let unscanned: Vec<&PathBuf> = under_live.iter().filter(|p| !files.contains(p)).collect();
    assert!(
        unscanned.is_empty(),
        "files under tests/live/ the scan missed: {unscanned:?}"
    );
    assert_eq!(uses_in(&files, &tests), Vec::<String>::new());
}

// --------------------------------------------------------------------------
// Descriptor identity: the device number in one representation

/// A macOS device of 2^31 or more reads on the test's side as BSD `stat -f %d` prints it in
/// the shell stand-in: `dev()` sign-extends the signed 32-bit `dev_t` (-1590744321), and
/// `stat` prints the same bits unsigned (2704222975). Pure arithmetic, so it runs everywhere.
#[test]
fn macos_device_number_reads_as_bsd_stat_prints_it() {
    let sign_extended = (-1_590_744_321_i32) as i64 as u64;
    assert_eq!(
        common::launch_world::macos_stat_dev(sign_extended),
        2_704_222_975
    );
    assert_eq!(common::launch_world::macos_stat_dev(16_777_232), 16_777_232);
}

/// A real file's device reads as this platform's `stat` prints it (BSD `stat` on macOS, GNU
/// `stat` on Linux, the two `sh_open_fds!` uses).
#[test]
fn stat_dev_matches_the_platform_stat() {
    let tmp = TempRoot::new("lh self ");
    let path = tmp.path().join("f");
    fs::write(&path, "").unwrap();
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("/usr/bin/stat");
        c.args(["-f", "%d"]);
        c
    } else {
        let mut c = std::process::Command::new("stat");
        c.args(["-L", "-c", "%d"]);
        c
    };
    let out = cmd.arg(&path).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let printed: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let m = fs::metadata(&path).unwrap();
    assert_eq!(common::launch_world::stat_dev(&m), printed);
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

// --------------------------------------------------------------------------
// Forked drives: the test binary re-executed as a drive child (ruling S3-R23)

mod drive_child {
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::panic::{self, AssertUnwindSafe};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use rustix::process::{Pid, Signal, kill_process};

    use crate::common::drive;
    use crate::common::json::{self, Json};
    use crate::common::supervisor::{FakeSupervisor, Forked, Step, card, recorded_alive};
    use crate::common::{read_json, read_jsonl, start_time};

    fn steps() -> [Step; 2] {
        [
            Step {
                reply: Some("first".into()),
                ..Step::default()
            },
            Step {
                reply: Some("second".into()),
                ..Step::default()
            },
        ]
    }

    fn action() -> Json {
        json::parse(br#"{"card": 1, "kind": "implement"}"#)
    }

    /// The parent's world for drive `label`, or in its drive child the parent's world as
    /// attached; both with `steps` scripted. The parent takes the first step before the fork.
    fn world(label: &str) -> FakeSupervisor {
        if let Some(mut w) = FakeSupervisor::attach_drive(label) {
            w.script(1, "implement", steps());
            return w;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        w.script(1, "implement", steps());
        assert_eq!(w.take(&action()).reply.as_deref(), Some("first"));
        w.advance(90.5);
        w.parks.insert(1, "parked by the parent".into());
        w.env.set("FAKE_MARK", "the parent's");
        w
    }

    #[test]
    fn gets_the_parents_world_and_exits_97_at_a_crash_point() {
        let mut w = world("exit 97");
        let counter = w.tmp.join("crash counter");
        let saw = w.tmp.join("child saw.json");
        let forked = w.fork("exit 97", Duration::from_secs(60), |w, pid| {
            let seen = Json::object([
                ("pid", Json::from(pid)),
                ("step", Json::from(w.take(&action()).reply.unwrap())),
                ("offset", Json::from(w.offset)),
                ("park", Json::from(w.parks[&1].as_str())),
                ("mark", Json::from(w.env.get_str("FAKE_MARK"))),
            ]);
            fs::write(&saw, json::dumps(&seen)).unwrap();
            w.env
                .set("LEADHAND_CRASH_AT", "before_commit")
                .set("LEADHAND_CRASH_COUNTER", &counter);
            w.crash_point("after_commit"); // not the armed point: passes
            w.crash_point("before_commit");
            panic!("the crash point returned");
        });
        assert_eq!((forked.code, forked.hung), (97, false), "{}", forked.output);
        assert_eq!(forked.error, None);
        let seen = json::parse(&fs::read(&saw).unwrap());
        assert_eq!(seen["step"], "second", "{seen}");
        assert_eq!(seen["offset"].as_f64(), Some(90.5));
        assert_eq!(seen["park"], "parked by the parent");
        assert_eq!(seen["mark"], "the parent's");
        assert_ne!(seen["pid"].as_i64(), Some(i64::from(std::process::id())));
        assert_eq!(fs::read_to_string(&counter).unwrap(), "1\n");
        // Nothing the child did reached the parent's world.
        assert_eq!(w.take(&action()).reply.as_deref(), Some("second"));
        assert_eq!(w.env.get("LEADHAND_CRASH_AT"), None);
        // In the test process itself an armed crash point panics instead.
        w.env.set("LEADHAND_CRASH_AT", "before_commit");
        let message = assert_panics!(w.crash_point("before_commit"));
        assert!(
            message.contains("reached in the test process itself"),
            "{message}"
        );
    }

    #[test]
    fn dies_with_its_group_at_a_crash_point_that_kills_it() {
        let mut w = world("killed");
        let forked = w.fork("killed", Duration::from_secs(60), |w, pid| {
            w.env
                .set("LEADHAND_CRASH_AT", "after_commit")
                .set("LEADHAND_CRASH_KILL_PGID", pid.to_string());
            w.crash_point("after_commit");
            panic!("the crash point returned");
        });
        assert_eq!((forked.code, forked.hung), (-9, false), "{}", forked.output);
        assert_eq!(forked.error, None);
    }

    #[test]
    fn a_panic_exits_1_with_its_message() {
        let mut w = world("panics");
        let forked = w.fork("panics", Duration::from_secs(60), |_, _| {
            panic!("the drive broke");
        });
        assert_eq!((forked.code, forked.hung), (1, false), "{}", forked.output);
        assert_eq!(forked.error.as_deref(), Some("the drive broke"));
    }

    #[test]
    fn a_child_that_forks_its_own_drive_without_attaching_is_refused() {
        const LABEL: &str = "own label";
        // A second-level drive child (one that only a missing guard would start): it records
        // itself in the top parent's tmp, which the first-level child told it about, and
        // stops at once, so even without the guard no chain can form.
        if let Some(sel) = drive::selected_for(LABEL)
            && let Ok(top) = fs::read(sel.payload.parent().unwrap().join("top tmp"))
        {
            let top = PathBuf::from(OsString::from_vec(top));
            fs::write(top.join("second level started"), "").unwrap();
            std::process::exit(0);
        }
        // The mistake under test: the world does not come from attach_drive(LABEL).
        let mut w = FakeSupervisor::new(&[card(1)]);
        if let Some(sel) = drive::selected_for(LABEL) {
            // The first-level child: where a second level would report.
            let top = sel.payload.parent().unwrap().as_os_str().as_bytes();
            fs::write(w.tmp.join("top tmp"), top).unwrap();
        }
        let second = w.tmp.join("second level started");
        let message = assert_panics!(w.fork(LABEL, Duration::from_secs(30), |_, _| {}));
        assert!(
            message.contains("ended before reaching fork(\"own label\")"),
            "{message}"
        );
        assert!(
            message.contains("with a world not from FakeSupervisor::attach_drive(\"own label\")"),
            "{message}"
        );
        assert!(!second.exists(), "a second-level drive child started");
    }

    #[test]
    fn a_child_that_dies_before_its_drive_fails_the_parent_with_its_output() {
        let mut w = world("dies early");
        if drive::selected_for("dies early").is_some() {
            panic!("the child broke before its drive");
        }
        let message = assert_panics!(w.fork("dies early", Duration::from_secs(30), |_, _| {}));
        assert!(
            message.contains("ended before reaching fork(\"dies early\")"),
            "{message}"
        );
        assert!(
            message.contains("the child broke before its drive"),
            "{message}"
        );
    }

    #[test]
    fn a_drive_past_its_deadline_is_killed_with_its_group() {
        let mut w = world("hangs");
        let sibling = w.tmp.join("sibling.json");
        let staged = w.tmp.join("sibling.json.tmp");
        // 2 s is plenty for the child to start, attach and record its other process; on a
        // loaded machine it may not be, and the attempt is made again with twice the time.
        let mut deadline = Duration::from_secs(2);
        let (forked, rec) = loop {
            let _ = fs::remove_file(&sibling);
            let attempt = panic::catch_unwind(AssertUnwindSafe(|| {
                w.fork("hangs", deadline, |_, _| {
                    // A second process in the drive's group, which the deadline kill must
                    // take too.
                    let mut p = Command::new("sleep")
                        .arg("60")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    let pid = p.id() as i32;
                    let rec = Json::object([
                        ("pid", Json::from(pid)),
                        ("start", Json::from(start_time(pid).unwrap())),
                    ]);
                    fs::write(&staged, json::dumps(&rec)).unwrap();
                    fs::rename(&staged, &sibling).unwrap(); // whole, or not there
                    std::thread::sleep(Duration::from_secs(60));
                    let _ = p.kill();
                    let _ = p.wait();
                })
            }));
            match (attempt, fs::read(&sibling)) {
                (Ok(forked), Ok(bytes)) => break (forked, json::parse(&bytes)),
                _ if deadline < Duration::from_secs(32) => deadline *= 2,
                (Ok(forked), Err(_)) => panic!(
                    "the drive child never recorded its other process in {} s:\n{}",
                    deadline.as_secs(),
                    forked.output
                ),
                (Err(payload), _) => panic::resume_unwind(payload),
            }
        };
        assert!(forked.hung, "{}", forked.output);
        assert_eq!(forked.code, -9);
        let end = Instant::now() + Duration::from_secs(5);
        while recorded_alive(&rec) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let alive = recorded_alive(&rec);
        if alive && let Some(pid) = rec["pid"].as_i64().and_then(|p| Pid::from_raw(p as i32)) {
            let _ = kill_process(pid, Signal::KILL);
        }
        assert!(
            !alive,
            "the drive's other process outlived the deadline kill: {rec}"
        );
    }

    // -- the chain's bounds (drive::start's refusals) --------------------------------

    const LONG: Duration = Duration::from_secs(60);

    /// The top test's tmp, which every world of a chain test carries in its env (TOP_TMP).
    fn top_of(w: &FakeSupervisor) -> PathBuf {
        PathBuf::from(w.env.get("TOP_TMP").expect("TOP_TMP"))
    }

    /// The world for drive `label` in a chain test: the attached one in its own drive
    /// child, else a new one that knows the top test's tmp (`top`, or its own at the top).
    fn chain_world(label: &str, top: Option<&Path>) -> FakeSupervisor {
        if let Some(w) = FakeSupervisor::attach_drive(label) {
            return w;
        }
        let mut w = FakeSupervisor::new(&[card(1)]);
        let top = top.map_or_else(|| w.tmp.clone(), Path::to_path_buf);
        w.env.set("TOP_TMP", top);
        w
    }

    /// First thing in a chain test. In a drive child: record this process (pid, start and
    /// chain) in the top test's `children.jsonl`; and a child deeper than any rule allows
    /// (one only a missing rule could start) records `too deep` there and stops at once, so
    /// no chain can form even then.
    fn enter_chain() {
        let Some(sel) = drive::selected() else {
            return;
        };
        let state = read_json(&sel.payload);
        let top = state["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|kv| kv[0] == "TOP_TMP")
            .and_then(|kv| kv[1].as_os())
            .map(PathBuf::from)
            .expect("the parent's world names TOP_TMP");
        let pid = std::process::id() as i32;
        let rec = Json::object([
            ("pid", Json::from(pid)),
            ("start", Json::from(start_time(pid).unwrap())),
            ("chain", Json::from(&sel.chain[..])),
        ]);
        let mut log = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(top.join("children.jsonl"))
            .unwrap();
        log.write_all(format!("{}\n", json::dumps(&rec)).as_bytes())
            .unwrap();
        if sel.chain.len() > drive::MAX_DEPTH {
            fs::write(top.join("too deep"), sel.chain.join(" > ")).unwrap();
            std::process::exit(0);
        }
    }

    /// Every drive child the test started (as `enter_chain` recorded them), once all are
    /// gone; any still alive after 5 s is killed and fails the test.
    fn children_gone(top: &Path) -> Vec<Json> {
        let log = top.join("children.jsonl");
        let children = if log.exists() {
            read_jsonl(&log)
        } else {
            Vec::new()
        };
        let end = Instant::now() + Duration::from_secs(5);
        while children.iter().any(recorded_alive) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let alive: Vec<&Json> = children.iter().filter(|c| recorded_alive(c)).collect();
        for c in &alive {
            if let Some(pid) = c["pid"].as_i64().and_then(|p| Pid::from_raw(p as i32)) {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
        assert!(alive.is_empty(), "drive children still running: {alive:?}");
        children
    }

    /// The chains the recorded children ran under, as `a > b`.
    fn chains(children: &[Json]) -> Vec<String> {
        children
            .iter()
            .map(|c| {
                let labels: Vec<&str> = c["chain"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| l.as_str().unwrap())
                    .collect();
                labels.join(" > ")
            })
            .collect()
    }

    /// The crash child's drive: exit 97 at a harness crash point.
    fn crash(c: &mut FakeSupervisor) -> Forked {
        c.fork("crash", LONG, |c, _| {
            c.env.set("LEADHAND_CRASH_AT", "before_commit");
            c.crash_point("before_commit");
        })
    }

    #[test]
    fn a_worker_drive_forks_its_one_crash_child() {
        enter_chain();
        // The crash child goes straight to its own drive.
        if drive::selected_for("crash").is_some() {
            crash(&mut chain_world("crash", None));
        }
        let mut w = chain_world("worker", None);
        let top = top_of(&w);
        let forked = w.fork("worker", LONG, |w, _| {
            let top = top_of(w);
            let f = crash(&mut chain_world("crash", Some(&top)));
            fs::write(
                top.join("crash result"),
                format!("{} {:?}", f.code, f.error),
            )
            .unwrap();
        });
        assert_eq!((forked.code, forked.error), (0, None), "{}", forked.output);
        assert_eq!(
            fs::read_to_string(top.join("crash result")).unwrap(),
            "97 None"
        );
        let children = children_gone(&top);
        assert_eq!(chains(&children), ["worker", "worker > crash"]);
    }

    #[test]
    fn an_alternating_chain_is_refused_before_a_third_level() {
        enter_chain();
        // The mistake: the worker's fork runs again in the crash child (no branch on
        // drive::selected() first), so worker → crash → worker → … would never end.
        let mut w = chain_world("worker", None);
        let top = top_of(&w);
        let forked = w.fork("worker", LONG, |w, _| {
            let top = top_of(w);
            chain_world("crash", Some(&top)).fork("crash", LONG, |_, _| {});
        });
        assert_eq!(forked.code, 1, "{}", forked.output);
        let error = forked.error.unwrap_or_default();
        assert!(
            error.contains("drive refused (its label is already in the chain): fork(\"worker\")"),
            "{error}"
        );
        assert!(!top.join("too deep").exists(), "a third level started");
        let children = children_gone(&top);
        assert_eq!(chains(&children), ["worker", "worker > crash"]);
    }

    #[test]
    fn a_child_reaching_an_earlier_fork_than_its_own_fails_loudly() {
        enter_chain();
        // The mistake: two drives in a row with no branch on drive::selected() first, so
        // the child for "b" reaches fork("a") before its own.
        let mut a = chain_world("a", None);
        let top = top_of(&a);
        let fa = a.fork("a", LONG, |_, _| {});
        assert_eq!(fa.code, 0, "{}", fa.output);
        let mut b = chain_world("b", Some(&top));
        let message = assert_panics!(b.fork("b", LONG, |_, _| {}));
        assert!(
            message.contains("ended before reaching fork(\"b\")"),
            "{message}"
        );
        assert!(
            message.contains("drive refused (the child has not reached its own drive)"),
            "{message}"
        );
        let children = children_gone(&top);
        assert_eq!(chains(&children), ["a", "b"]);
    }

    #[test]
    fn a_third_nesting_level_is_refused() {
        enter_chain();
        // Each child goes straight to its own drive, but the crash child's drive forks a
        // third level.
        if drive::selected_for("deeper").is_some() {
            chain_world("deeper", None).fork("deeper", LONG, |_, _| {});
        }
        if drive::selected_for("crash").is_some() {
            chain_world("crash", None).fork("crash", LONG, |c, _| {
                let top = top_of(c);
                chain_world("deeper", Some(&top)).fork("deeper", LONG, |_, _| {});
            });
        }
        let mut w = chain_world("worker", None);
        let top = top_of(&w);
        let forked = w.fork("worker", LONG, |w, _| {
            let top = top_of(w);
            let f = chain_world("crash", Some(&top)).fork("crash", LONG, |_, _| {});
            fs::write(top.join("crash error"), f.error.unwrap_or_default()).unwrap();
        });
        assert_eq!((forked.code, forked.error), (0, None), "{}", forked.output);
        let error = fs::read_to_string(top.join("crash error")).unwrap();
        assert!(
            error.contains("drive refused (nesting past 2 levels): fork(\"deeper\")"),
            "{error}"
        );
        assert!(!top.join("too deep").exists(), "a third level started");
        let children = children_gone(&top);
        assert_eq!(chains(&children), ["worker", "worker > crash"]);
    }
}
