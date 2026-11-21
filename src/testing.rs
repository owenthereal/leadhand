//! Test hooks. Each function returns the production value, or does nothing, unless the binary
//! was built with `--features testing` and `LEADHAND_TESTING` is `1`; the hook semantics are
//! the Python's (`engine._now`, `dispatch.cli_path`, `dispatch.minute_seconds`,
//! `dispatch.foreground_wait`, `dispatch.codex_executable`, `engine.installed_vendors`,
//! `state.crash_point`). A release build reads none of these variables (deliberate change 5).
//!
//! A malformed hook value fails loudly, as the Python's `ValueError` does: `error: …` on
//! stderr and exit 1, never a silent fallback to the production value.

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};

/// How long `dispatch` waits in the foreground before exiting 75 (the proxy's Bash call is
/// capped at 10 minutes).
pub const FOREGROUND_WAIT: Duration = Duration::from_secs(540);
/// The length of a "minute" in worker limits.
pub const MINUTE_SECONDS: f64 = 60.0;
/// The exit code of a crash point.
pub const CRASH_EXIT: i32 = 97;

const VENDORS: [&str; 2] = ["claude", "codex"];

/// Where a hook reads the environment from: the process's own, or a fixed one in unit tests
/// (setting the process environment is `unsafe` in edition 2024).
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// True when the test hooks are honoured: a `testing` build with `LEADHAND_TESTING=1`.
pub fn enabled() -> bool {
    enabled_in(&process_env)
}

/// The time, shifted by `LEADHAND_TEST_CLOCK_OFFSET` seconds.
pub fn now() -> SystemTime {
    now_in(&process_env).unwrap_or_else(|e| fail_loudly(e))
}

/// The path that names this binary wherever it names or runs itself: `LEADHAND_TEST_CLI`
/// (the contract router), else the running executable's resolved path.
pub fn self_path() -> PathBuf {
    self_path_in(&process_env)
}

/// The length of a worker-limit minute in seconds: `LEADHAND_MINUTE_SECONDS`, else 60.
pub fn minute_seconds() -> f64 {
    minute_seconds_in(&process_env).unwrap_or_else(|e| fail_loudly(e))
}

/// How long `dispatch` waits in the foreground: `LEADHAND_TEST_FOREGROUND_WAIT` seconds,
/// else 540 s.
pub fn foreground_wait() -> Duration {
    foreground_wait_in(&process_env).unwrap_or_else(|e| fail_loudly(e))
}

/// `LEADHAND_TEST_CARD_LOOP_ONLY=1`: `next` stops where the card loop ends.
pub fn card_loop_only() -> bool {
    card_loop_only_in(&process_env)
}

/// The vendors `LEADHAND_INSTALLED_VENDORS` names (a comma list; unknown names dropped), in
/// place of a PATH lookup; `None` when the hook is off or the variable is unset.
pub fn installed_vendors() -> Option<Vec<String>> {
    installed_vendors_in(&process_env)
}

/// The Codex program `LEADHAND_TEST_CODEX_BIN` names, in place of `codex` on PATH.
pub fn codex_bin() -> Option<PathBuf> {
    codex_bin_in(&process_env)
}

/// Fault injection, with `state.crash_point`'s protocol: exits at once with code 97 when
/// `LEADHAND_CRASH_AT` names this point.
/// - `LEADHAND_CRASH_LOG=<file>`: every point passed appends its name there, crash or not.
/// - `LEADHAND_CRASH_COUNTER=<file>` with `LEADHAND_CRASH_SKIP=<n>`: the named point counts
///   its passes in the file, under `flock`, and crashes only after n of them.
/// - `LEADHAND_CRASH_KILL_PGID=<pgid>`: the crash first SIGKILLs that process group.
pub fn crash_point(name: &str) {
    match crash_due(&process_env, name) {
        Ok(false) => {}
        Ok(true) => crash(&process_env),
        Err(e) => fail_loudly(e),
    }
}

// --------------------------------------------------------------------------
// The hooks, reading a given environment

#[cfg(feature = "testing")]
fn enabled_in(env: Env) -> bool {
    env("LEADHAND_TESTING").as_deref() == Some(OsStr::new("1"))
}

#[cfg(not(feature = "testing"))]
fn enabled_in(_env: Env) -> bool {
    false
}

/// Hook `name`'s value when the hooks are honoured and it is set.
#[cfg(feature = "testing")]
fn hook(env: Env, name: &str) -> Option<OsString> {
    if enabled_in(env) { env(name) } else { None }
}

#[cfg(not(feature = "testing"))]
fn hook(_env: Env, _name: &str) -> Option<OsString> {
    None
}

/// Hook `name`'s value when it is set and not empty (the Python tests the string's truth).
fn hook_set(env: Env, name: &str) -> Option<OsString> {
    hook(env, name).filter(|v| !v.is_empty())
}

/// A number of seconds, as Python's `float()` reads it (surrounding whitespace allowed).
fn float(name: &str, value: &OsStr) -> Result<f64> {
    value
        .to_str()
        .and_then(|text| text.trim().parse::<f64>().ok())
        .ok_or_else(|| {
            Error::msg(format!(
                "{name} must be a number, not {:?}",
                value.to_string_lossy()
            ))
        })
}

/// A whole number, as Python's `int()` reads it (surrounding whitespace allowed).
fn integer(name: &str, value: &OsStr) -> Result<i64> {
    value
        .to_str()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .ok_or_else(|| {
            Error::msg(format!(
                "{name} must be a whole number, not {:?}",
                value.to_string_lossy()
            ))
        })
}

fn now_in(env: Env) -> Result<SystemTime> {
    let now = SystemTime::now();
    let Some(value) = hook_set(env, "LEADHAND_TEST_CLOCK_OFFSET") else {
        return Ok(now);
    };
    let offset = float("LEADHAND_TEST_CLOCK_OFFSET", &value)?;
    let shifted = Duration::try_from_secs_f64(offset.abs())
        .ok()
        .and_then(|d| {
            if offset >= 0.0 {
                now.checked_add(d)
            } else {
                now.checked_sub(d)
            }
        });
    shifted.ok_or_else(|| {
        Error::msg(format!(
            "LEADHAND_TEST_CLOCK_OFFSET is out of range: {offset}"
        ))
    })
}

fn self_path_in(env: Env) -> PathBuf {
    if let Some(path) = hook_set(env, "LEADHAND_TEST_CLI") {
        return PathBuf::from(path);
    }
    // The Python names itself by its resolved path; so does this binary.
    let exe = std::env::current_exe().expect("the running executable's path");
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

fn minute_seconds_in(env: Env) -> Result<f64> {
    match hook_set(env, "LEADHAND_MINUTE_SECONDS") {
        Some(value) => float("LEADHAND_MINUTE_SECONDS", &value),
        None => Ok(MINUTE_SECONDS),
    }
}

fn foreground_wait_in(env: Env) -> Result<Duration> {
    let Some(value) = hook_set(env, "LEADHAND_TEST_FOREGROUND_WAIT") else {
        return Ok(FOREGROUND_WAIT);
    };
    let seconds = float("LEADHAND_TEST_FOREGROUND_WAIT", &value)?;
    if seconds.is_nan() {
        return Err(Error::msg(
            "LEADHAND_TEST_FOREGROUND_WAIT must be a number, not \"nan\"",
        ));
    }
    // As the Python's deadline arithmetic: a negative wait is over at once, a huge one never.
    Ok(Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::MAX))
}

fn card_loop_only_in(env: Env) -> bool {
    hook(env, "LEADHAND_TEST_CARD_LOOP_ONLY").as_deref() == Some(OsStr::new("1"))
}

fn installed_vendors_in(env: Env) -> Option<Vec<String>> {
    // Set but empty names no vendor, as in the Python.
    let value = hook(env, "LEADHAND_INSTALLED_VENDORS")?;
    let text = value.to_string_lossy();
    let named: Vec<&str> = text.split(',').map(str::trim).collect();
    Some(
        VENDORS
            .iter()
            .filter(|v| named.contains(v))
            .map(|v| v.to_string())
            .collect(),
    )
}

fn codex_bin_in(env: Env) -> Option<PathBuf> {
    hook_set(env, "LEADHAND_TEST_CODEX_BIN").map(PathBuf::from)
}

/// Logs the pass and counts it; true when the point should crash now.
fn crash_due(env: Env, name: &str) -> Result<bool> {
    if !enabled_in(env) {
        return Ok(false);
    }
    if let Some(log) = hook_set(env, "LEADHAND_CRASH_LOG") {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o644)
            .open(log)?;
        file.write_all(format!("{name}\n").as_bytes())?;
    }
    if hook(env, "LEADHAND_CRASH_AT").as_deref() != Some(OsStr::new(name)) {
        return Ok(false);
    }
    if let Some(counter) = hook_set(env, "LEADHAND_CRASH_COUNTER") {
        let seen = count_pass(&counter)?;
        let skip = match hook_set(env, "LEADHAND_CRASH_SKIP") {
            Some(value) => integer("LEADHAND_CRASH_SKIP", &value)?,
            None => 0,
        };
        if seen <= skip {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Adds one pass to the counter file, under an exclusive `flock`; returns the new count.
fn count_pass(counter: &OsStr) -> Result<i64> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(counter)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(std::io::Error::from)?;
    let mut buf = [0u8; 64];
    let n = file.read_at(&mut buf, 0)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let seen = match text.trim() {
        "" => 1,
        so_far => integer("the crash counter", OsStr::new(so_far))? + 1,
    };
    file.set_len(0)?;
    file.write_all_at(format!("{seen}\n").as_bytes(), 0)?;
    file.sync_all()?;
    Ok(seen)
}

/// Kills `LEADHAND_CRASH_KILL_PGID`'s group if named (errors ignored), then exits 97 with no
/// cleanup: no destructor runs.
fn crash(env: Env) -> ! {
    match crash_group(env) {
        Some(CrashGroup::Own) => {
            let _ = rustix::process::kill_current_process_group(rustix::process::Signal::KILL);
        }
        Some(CrashGroup::Of(pid)) => {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        None => {}
    }
    std::process::exit(CRASH_EXIT)
}

/// The process group a crash kills first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrashGroup {
    /// The crashing process's own (`LEADHAND_CRASH_KILL_PGID=0`, as `os.killpg(0, …)`).
    Own,
    /// The group this pid leads.
    Of(rustix::process::Pid),
}

/// The group `LEADHAND_CRASH_KILL_PGID` names, `None` when it names none: unset, empty, not a
/// whole number, out of a pid's range, negative, or 1, which `kill_process_group` would send
/// to every process the user owns (`kill(-1, …)`).
fn crash_group(env: Env) -> Option<CrashGroup> {
    let pgid = hook_set(env, "LEADHAND_CRASH_KILL_PGID")
        .and_then(|v| integer("LEADHAND_CRASH_KILL_PGID", &v).ok())?;
    match pgid {
        0 => Some(CrashGroup::Own),
        pgid if pgid > 1 => i32::try_from(pgid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .map(CrashGroup::Of),
        _ => None,
    }
}

fn fail_loudly(e: Error) -> ! {
    eprintln!("error: {e}");
    std::process::exit(i32::from(e.exit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An environment with LEADHAND_TESTING=1 and every hook set, plus `extra`.
    fn hooked(extra: &[(&str, &str)]) -> HashMap<String, OsString> {
        let mut env: HashMap<String, OsString> = [
            ("LEADHAND_TESTING", "1"),
            ("LEADHAND_TEST_CLOCK_OFFSET", "3600"),
            ("LEADHAND_TEST_CLI", "/x/router"),
            ("LEADHAND_MINUTE_SECONDS", "1"),
            ("LEADHAND_TEST_FOREGROUND_WAIT", "2.5"),
            ("LEADHAND_TEST_CARD_LOOP_ONLY", "1"),
            ("LEADHAND_INSTALLED_VENDORS", "codex"),
            ("LEADHAND_TEST_CODEX_BIN", "/x/fake codex"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), OsString::from(v)))
        .collect();
        for (k, v) in extra {
            env.insert(k.to_string(), OsString::from(v));
        }
        env
    }

    fn lookup(env: &HashMap<String, OsString>) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |name| env.get(name).cloned()
    }

    fn seconds_from_now(t: SystemTime) -> f64 {
        match t.duration_since(SystemTime::now()) {
            Ok(d) => d.as_secs_f64(),
            Err(e) => -e.duration().as_secs_f64(),
        }
    }

    fn real_exe() -> PathBuf {
        std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    }

    #[cfg(not(feature = "testing"))]
    #[test]
    fn hooks_compiled_out_without_feature() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crash log");
        let counter = dir.path().join("crash counter");
        let env = hooked(&[
            ("LEADHAND_CRASH_AT", "here"),
            ("LEADHAND_CRASH_LOG", log.to_str().unwrap()),
            ("LEADHAND_CRASH_COUNTER", counter.to_str().unwrap()),
            ("LEADHAND_CRASH_SKIP", "5"),
        ]);
        let env = lookup(&env);
        assert!(!enabled_in(&env));
        assert!(
            seconds_from_now(now_in(&env).unwrap()).abs() < 60.0,
            "now() honoured the offset"
        );
        assert_eq!(
            self_path_in(&env),
            real_exe(),
            "self_path() honoured LEADHAND_TEST_CLI"
        );
        assert_eq!(minute_seconds_in(&env).unwrap(), 60.0);
        assert_eq!(foreground_wait_in(&env).unwrap(), Duration::from_secs(540));
        assert!(!card_loop_only_in(&env));
        assert_eq!(installed_vendors_in(&env), None);
        assert_eq!(codex_bin_in(&env), None);
        assert!(!crash_due(&env, "here").unwrap());
        assert!(!log.exists() && !counter.exists());
        // Malformed values are never read either.
        let bad = hooked(&[
            ("LEADHAND_TEST_FOREGROUND_WAIT", "soon"),
            ("LEADHAND_TEST_CLOCK_OFFSET", "x"),
        ]);
        assert_eq!(foreground_wait_in(&lookup(&bad)).unwrap(), FOREGROUND_WAIT);
        assert!(now_in(&lookup(&bad)).is_ok());
    }

    #[test]
    fn hooks_ignored_without_leadhand_testing() {
        for testing in [None, Some("0"), Some(""), Some("true")] {
            let mut env = hooked(&[]);
            match testing {
                Some(v) => env.insert("LEADHAND_TESTING".into(), v.into()),
                None => env.remove("LEADHAND_TESTING"),
            };
            let env = lookup(&env);
            assert!(!enabled_in(&env));
            assert!(seconds_from_now(now_in(&env).unwrap()).abs() < 60.0);
            assert_eq!(self_path_in(&env), real_exe());
            assert_eq!(minute_seconds_in(&env).unwrap(), 60.0);
            assert_eq!(foreground_wait_in(&env).unwrap(), FOREGROUND_WAIT);
            assert!(!card_loop_only_in(&env));
            assert_eq!(installed_vendors_in(&env), None);
            assert_eq!(codex_bin_in(&env), None);
        }
    }

    #[test]
    fn production_values_without_hooks() {
        let env = HashMap::from([("LEADHAND_TESTING".to_string(), OsString::from("1"))]);
        let env = lookup(&env);
        assert_eq!(FOREGROUND_WAIT, Duration::from_secs(540));
        assert_eq!(foreground_wait_in(&env).unwrap(), Duration::from_secs(540));
        assert_eq!(minute_seconds_in(&env).unwrap(), 60.0);
        assert_eq!(self_path_in(&env), real_exe());
        assert!(self_path_in(&env).is_absolute());
        assert_eq!(installed_vendors_in(&env), None);
        assert_eq!(codex_bin_in(&env), None);
        assert!(!card_loop_only_in(&env));
        assert!(seconds_from_now(now_in(&env).unwrap()).abs() < 60.0);
    }

    #[cfg(feature = "testing")]
    #[test]
    fn hooks_honoured_under_leadhand_testing() {
        let env = hooked(&[]);
        let env = lookup(&env);
        assert!(enabled_in(&env));
        let ahead = seconds_from_now(now_in(&env).unwrap());
        assert!((3540.0..3660.0).contains(&ahead), "{ahead}");
        assert_eq!(self_path_in(&env), PathBuf::from("/x/router"));
        assert_eq!(minute_seconds_in(&env).unwrap(), 1.0);
        assert_eq!(
            foreground_wait_in(&env).unwrap(),
            Duration::from_millis(2500)
        );
        assert!(card_loop_only_in(&env));
        assert_eq!(installed_vendors_in(&env), Some(vec!["codex".to_string()]));
        assert_eq!(codex_bin_in(&env), Some(PathBuf::from("/x/fake codex")));
        let behind = hooked(&[("LEADHAND_TEST_CLOCK_OFFSET", " -7200 ")]);
        let behind = seconds_from_now(now_in(&lookup(&behind)).unwrap());
        assert!((-7260.0..-7140.0).contains(&behind), "{behind}");
    }

    #[cfg(feature = "testing")]
    #[test]
    fn empty_hook_values_mean_unset_except_installed_vendors() {
        let env = hooked(&[
            ("LEADHAND_TEST_CLOCK_OFFSET", ""),
            ("LEADHAND_TEST_CLI", ""),
            ("LEADHAND_MINUTE_SECONDS", ""),
            ("LEADHAND_TEST_FOREGROUND_WAIT", ""),
            ("LEADHAND_TEST_CARD_LOOP_ONLY", ""),
            ("LEADHAND_INSTALLED_VENDORS", ""),
            ("LEADHAND_TEST_CODEX_BIN", ""),
        ]);
        let env = lookup(&env);
        assert!(seconds_from_now(now_in(&env).unwrap()).abs() < 60.0);
        assert_eq!(self_path_in(&env), real_exe());
        assert_eq!(minute_seconds_in(&env).unwrap(), 60.0);
        assert_eq!(foreground_wait_in(&env).unwrap(), FOREGROUND_WAIT);
        assert!(!card_loop_only_in(&env));
        assert_eq!(installed_vendors_in(&env), Some(vec![]));
        assert_eq!(codex_bin_in(&env), None);
    }

    #[cfg(feature = "testing")]
    #[test]
    fn installed_vendors_keeps_known_names_only() {
        for (value, want) in [
            ("claude,codex", vec!["claude", "codex"]),
            (" codex , claude ", vec!["claude", "codex"]),
            ("codex,codex", vec!["codex"]),
            ("gemini,claude", vec!["claude"]),
            ("Claude", vec![]),
        ] {
            let env = hooked(&[("LEADHAND_INSTALLED_VENDORS", value)]);
            let got = installed_vendors_in(&lookup(&env)).unwrap();
            assert_eq!(got, want, "{value:?}");
        }
    }

    #[cfg(feature = "testing")]
    #[test]
    fn malformed_hook_values_fail_loudly() {
        let bad = |name: &str, value: &str| hooked(&[(name, value)]);
        let e =
            foreground_wait_in(&lookup(&bad("LEADHAND_TEST_FOREGROUND_WAIT", "soon"))).unwrap_err();
        assert_eq!(
            e.message,
            "LEADHAND_TEST_FOREGROUND_WAIT must be a number, not \"soon\""
        );
        assert_eq!(e.exit, 1);
        assert!(foreground_wait_in(&lookup(&bad("LEADHAND_TEST_FOREGROUND_WAIT", "nan"))).is_err());
        assert!(now_in(&lookup(&bad("LEADHAND_TEST_CLOCK_OFFSET", "an hour"))).is_err());
        assert!(now_in(&lookup(&bad("LEADHAND_TEST_CLOCK_OFFSET", "inf"))).is_err());
        assert!(minute_seconds_in(&lookup(&bad("LEADHAND_MINUTE_SECONDS", "1m"))).is_err());
        // Python's float() forms that are numbers.
        let wait = |v: &str| {
            foreground_wait_in(&lookup(&bad("LEADHAND_TEST_FOREGROUND_WAIT", v))).unwrap()
        };
        assert_eq!(wait(" 1.5\n"), Duration::from_millis(1500));
        assert_eq!(wait("1e1"), Duration::from_secs(10));
        assert_eq!(wait("-3"), Duration::ZERO);
        assert_eq!(wait("inf"), Duration::MAX);
    }

    #[cfg(feature = "testing")]
    #[test]
    fn crash_point_logs_every_pass_and_counts_the_named_one() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crash log");
        let counter = dir.path().join("crash counter");
        let env = hooked(&[
            ("LEADHAND_CRASH_AT", "after_rebase"),
            ("LEADHAND_CRASH_LOG", log.to_str().unwrap()),
            ("LEADHAND_CRASH_COUNTER", counter.to_str().unwrap()),
            ("LEADHAND_CRASH_SKIP", "2"),
        ]);
        let env = lookup(&env);
        assert!(!crash_due(&env, "before_result_record").unwrap());
        assert!(!counter.exists(), "only the named point counts");
        assert!(!crash_due(&env, "after_rebase").unwrap());
        assert!(!crash_due(&env, "after_rebase").unwrap());
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "2\n");
        assert!(
            crash_due(&env, "after_rebase").unwrap(),
            "the third pass crashes"
        );
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "3\n");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "before_result_record\nafter_rebase\nafter_rebase\nafter_rebase\n"
        );
    }

    #[cfg(feature = "testing")]
    #[test]
    fn crash_point_without_a_counter_crashes_at_the_first_pass() {
        let env = hooked(&[("LEADHAND_CRASH_AT", "after_start")]);
        let env = lookup(&env);
        assert!(!crash_due(&env, "after_rebase").unwrap());
        assert!(crash_due(&env, "after_start").unwrap());
        let off = hooked(&[
            ("LEADHAND_CRASH_AT", "after_start"),
            ("LEADHAND_TESTING", "0"),
        ]);
        assert!(!crash_due(&lookup(&off), "after_start").unwrap());
    }

    // `LEADHAND_CRASH_KILL_PGID=1` would be `kill(-1, SIGKILL)`, every process the user owns,
    // and a negative value no group at all: neither reaches `kill_process_group`. 0 keeps its
    // meaning, the crashing process's own group (`os.killpg(0, 9)`).
    #[cfg(feature = "testing")]
    #[test]
    fn crash_kill_pgid_never_names_every_process() {
        let group =
            |value: &str| crash_group(&lookup(&hooked(&[("LEADHAND_CRASH_KILL_PGID", value)])));
        let pid = |n: i32| rustix::process::Pid::from_raw(n).map(CrashGroup::Of);
        assert_eq!(group("0"), Some(CrashGroup::Own));
        assert_eq!(group("4242"), pid(4242));
        assert_eq!(group(" 4242\n"), pid(4242));
        assert_eq!(group("2"), pid(2));
        for none in ["1", " 1 ", "-1", "-4242", "", "x", "99999999999"] {
            assert_eq!(group(none), None, "{none:?}");
        }
        let off = hooked(&[
            ("LEADHAND_CRASH_KILL_PGID", "4242"),
            ("LEADHAND_TESTING", "0"),
        ]);
        assert_eq!(crash_group(&lookup(&off)), None);
    }

    #[cfg(feature = "testing")]
    #[test]
    fn crash_point_refuses_a_malformed_skip_or_counter() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("crash counter");
        let env = hooked(&[
            ("LEADHAND_CRASH_AT", "x"),
            ("LEADHAND_CRASH_COUNTER", counter.to_str().unwrap()),
            ("LEADHAND_CRASH_SKIP", "two"),
        ]);
        assert!(crash_due(&lookup(&env), "x").is_err());
        std::fs::write(&counter, "lots\n").unwrap();
        let env = hooked(&[
            ("LEADHAND_CRASH_AT", "x"),
            ("LEADHAND_CRASH_COUNTER", counter.to_str().unwrap()),
        ]);
        assert!(crash_due(&lookup(&env), "x").is_err());
    }
}
