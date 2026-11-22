//! The `codex exec` command line (`dispatch.codex_argv`; contract item 10), byte for byte.
//!
//! The prompt goes on stdin (`-`). Resume is `codex exec <the same options> resume <thread
//! id> -`. The program is always `codex` as found on PATH (ruling R49): `request.json` never
//! names an executable; a testing build takes `LEADHAND_TEST_CODEX_BIN` instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::pytext::pure_path;
use crate::wire::PyStr;

use super::DispatchRequest;

/// The `codex exec` argv for `req`. `env_overrides` are passed on to Codex's tool shells
/// (`codex_overrides`), in order; `extra_dirs` are writable folders the dispatch itself
/// needs (an evidence worker's base worktree). Errors where the Python raises
/// `ValueError`: a name that is not an environment variable name, a resume ID that starts
/// with `-`; and where it fails to encode an argument (a lone surrogate that is not a
/// surrogate escape).
pub fn codex_argv(
    req: &DispatchRequest,
    last_message: &Path,
    git_common: &Path,
    env_overrides: &[(OsString, OsString)],
    extra_dirs: &[PathBuf],
) -> Result<Vec<OsString>> {
    codex_argv_parts(req, last_message, git_common, env_overrides, extra_dirs)?
}

/// `codex_argv`, with its two failures kept apart, as the Python's runner meets them at two
/// different times: the outer error is a refusal, raised by the Python's `codex_argv` itself
/// (`ValueError`, before anything starts); the inner one an argument the OS cannot be given
/// (a lone surrogate that is not a surrogate escape), which the Python's `Popen` raises at the
/// launch.
pub fn codex_argv_parts(
    req: &DispatchRequest,
    last_message: &Path,
    git_common: &Path,
    env_overrides: &[(OsString, OsString)],
    extra_dirs: &[PathBuf],
) -> Result<Result<Vec<OsString>>> {
    let program = crate::testing::codex_bin()
        .map_or_else(|| OsString::from("codex"), PathBuf::into_os_string);
    parts_for(
        program,
        req,
        last_message,
        git_common,
        env_overrides,
        extra_dirs,
    )
}

/// `codex_argv` with the program given.
#[cfg(test)]
fn argv_for(
    program: OsString,
    req: &DispatchRequest,
    last_message: &Path,
    git_common: &Path,
    env_overrides: &[(OsString, OsString)],
    extra_dirs: &[PathBuf],
) -> Result<Vec<OsString>> {
    parts_for(
        program,
        req,
        last_message,
        git_common,
        env_overrides,
        extra_dirs,
    )?
}

/// `codex_argv_parts` with the program given.
fn parts_for(
    program: OsString,
    req: &DispatchRequest,
    last_message: &Path,
    git_common: &Path,
    env_overrides: &[(OsString, OsString)],
    extra_dirs: &[PathBuf],
) -> Result<Result<Vec<OsString>>> {
    // Built as the Python builds it, every refusal first; encoded for the OS last, as
    // `subprocess` encodes the Python's list.
    let path = |p: &Path| Arg::Os(pure_path(p.as_os_str()).into_os_string());
    let text = |s: &str| Arg::Os(OsString::from(s));
    let setting = |key: &str, value: &PyStr| Arg::Text(setting_of(&PyStr::from(key), value));
    let mut argv = vec![
        Arg::Os(program),
        text("exec"),
        text("-m"),
        Arg::Text(req.model.clone()),
        text("-c"),
        setting("model_reasoning_effort", &req.effort),
        text("-c"),
        text("approval_policy=\"never\""),
        text("-c"),
        text(if req.network {
            "sandbox_workspace_write.network_access=true"
        } else {
            "sandbox_workspace_write.network_access=false"
        }),
        text("-C"),
        path(&req.cwd),
        text("-s"),
        text(if req.role == "scout" {
            "read-only"
        } else {
            "workspace-write"
        }),
        text("--add-dir"),
        path(git_common),
    ];
    for dir in req.writable_dirs.iter().chain(extra_dirs) {
        argv.push(text("--add-dir"));
        argv.push(path(dir));
    }
    for (name, value) in env_overrides {
        let name = PyStr::from_os(name);
        if !is_env_name(name.as_wtf8()) {
            return Err(Error::msg(format!(
                "not an environment variable name: {}",
                name.repr()
            )));
        }
        let mut key = PyStr::from("shell_environment_policy.set.");
        key.push(&name);
        argv.push(text("-c"));
        argv.push(Arg::Text(setting_of(&key, &PyStr::from_os(value))));
    }
    argv.extend([text("--json"), text("-o"), path(last_message)]);
    if let Some(resume) = req.resume.as_ref().filter(|r| !r.is_empty()) {
        if resume.starts_with("-") {
            return Err(Error::msg(format!("bad session id {}", resume.repr())));
        }
        argv.push(text("resume"));
        argv.push(Arg::Text(resume.clone()));
    }
    argv.push(text("-"));
    Ok(argv
        .into_iter()
        .map(|arg| match arg {
            Arg::Os(os) => Ok(os),
            Arg::Text(text) => text.to_os(),
        })
        .collect())
}

/// A `-c` setting: `<key>=<value as a TOML string>`.
fn setting_of(key: &PyStr, value: &PyStr) -> PyStr {
    let mut out = key.clone();
    out.push_str("=");
    out.push(&toml_string(value));
    out
}

/// One argument: OS bytes as they are, or text the OS gets surrogate-escaped.
enum Arg {
    Os(OsString),
    Text(PyStr),
}

/// `_ENV_NAME.match(name)`: `^[A-Za-z_][A-Za-z0-9_]*$`, where `$` also matches before a
/// final newline.
fn is_env_name(name: &[u8]) -> bool {
    let name = name.strip_suffix(b"\n").unwrap_or(name);
    match name.split_first() {
        Some((first, rest)) => {
            (first.is_ascii_alphabetic() || *first == b'_')
                && rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        }
        None => false,
    }
}

/// `dispatch._toml_string`: `json.dumps(value, ensure_ascii=False)`, a TOML basic string.
/// Only `"`, `\` and the C0 controls are escaped; everything else, a lone surrogate
/// included, is kept as it is.
pub fn toml_string(value: &PyStr) -> PyStr {
    let mut out = vec![b'"'];
    for &byte in value.as_wtf8() {
        // The bytes of a multi-byte character are never ASCII, so no character is cut.
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x00..=0x1f => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
    // ASCII added between whole characters keeps the bytes well-formed.
    PyStr::from_wtf8(out).expect("escaping ASCII keeps WTF-8 well-formed")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::wire::tests::{python, read_reference};
    use crate::wire::to_line;

    fn os(b: &[u8]) -> OsString {
        OsString::from_vec(b.to_vec())
    }

    fn p(b: &[u8]) -> PathBuf {
        PathBuf::from(os(b))
    }

    fn base(role: &str) -> DispatchRequest {
        DispatchRequest {
            model: PyStr::from("gpt-6-luna"),
            effort: PyStr::from("medium"),
            role: role.to_string(),
            brief: p(b"/ws/.leadhand/1/c1-a1/brief.md"),
            report: p(b"/ws/.leadhand/1/c1-a1/report.md"),
            cwd: p(b"/ws"),
            run_dir: p(b"/runs/r1"),
            card: Some(1),
            attempt: 1,
            dispatch_id: "c1-a1".to_string(),
            steps: 40,
            minutes: 20,
            network: false,
            writable_dirs: Vec::new(),
            resume: None,
            kind: Some("implement".to_string()),
            branch: Some(PyStr::from("lh/r1")),
        }
    }

    /// What `run_runner` passes, for a gh-config folder with a space and a non-UTF-8 byte,
    /// and a parent `GIT_CONFIG_VALUE_0` that needs TOML escapes.
    fn overrides() -> Vec<(OsString, OsString)> {
        [
            (&b"GIT_SSH_COMMAND"[..], &b"false"[..]),
            (b"GIT_TERMINAL_PROMPT", b"0"),
            (b"GH_CONFIG_DIR", b"/runs/r 1/workers/c1-a1/gh\xff-config"),
            (b"GIT_CONFIG_KEY_0", b"http.extraHeader"),
            (
                b"GIT_CONFIG_VALUE_0",
                b"say \"hi\" \\ \n\t\x01\x1f\x7f caf\xc3\xa9 \xfe",
            ),
            (b"GIT_CONFIG_KEY_1", b"credential.helper"),
            (b"GIT_CONFIG_VALUE_1", b""),
            (b"GIT_CONFIG_COUNT", b"2"),
            (b"GIT_CONFIG_PARAMETERS", b"'a.b=c' 'core.askPass='"),
            (b"LEADHAND_RUN_ID", b"r1"),
            (b"LEADHAND_DISPATCH_TAG", b"r1:c1-a1:1"),
        ]
        .iter()
        .map(|(k, v)| (os(k), os(v)))
        .collect()
    }

    struct Case {
        req: DispatchRequest,
        last: PathBuf,
        git_common: PathBuf,
        overrides: Vec<(OsString, OsString)>,
        extra: Vec<PathBuf>,
    }

    fn case(req: DispatchRequest) -> Case {
        Case {
            req,
            last: p(b"/runs/r1/workers/c1-a1/last-message.md"),
            git_common: p(b"/repo/.git"),
            overrides: overrides(),
            extra: Vec::new(),
        }
    }

    /// The table: every role, the resume form, paths with a space, non-ASCII and non-UTF-8
    /// bytes, unnormalized paths, TOML escapes, and each refusal.
    fn table() -> Vec<Case> {
        let mut cases = Vec::new();
        for (i, (role, _)) in crate::config::ROLE_AGENT.iter().enumerate() {
            let mut req = base(role);
            req.network = i % 2 == 0;
            if i % 3 == 0 {
                req.writable_dirs = vec![p(b"/extra/a"), p(b"/extra b/c")];
            }
            cases.push(case(req));
        }
        // Resume, for a writing role and for the read-only scout.
        for role in ["implementer", "scout"] {
            let mut req = base(role);
            req.resume = Some(PyStr::from("0f0f0f0f-0000-4000-8000-00000000c0de"));
            cases.push(case(req));
        }
        // An empty resume ID is no resume.
        let mut req = base("reviewer");
        req.resume = Some(PyStr::from(""));
        cases.push(case(req));
        // A resume ID that would read as an option is refused.
        let mut req = base("reviewer");
        req.resume = Some(PyStr::from("-x"));
        cases.push(case(req));
        // A resume ID holding a surrogate escape is its byte.
        let mut req = base("implementer");
        req.resume = Some(PyStr::from_os(&os(b"thread-\xff")));
        cases.push(case(req));
        // Paths with a space.
        let mut req = base("implementer");
        req.cwd = p(b"/w s/ws dir");
        req.writable_dirs = vec![p(b"/w s/extra dir")];
        let mut c = case(req);
        c.last = p(b"/w s/runs/r 1/workers/c1-a1/last-message.md");
        c.git_common = p(b"/w s/repo/.git");
        cases.push(c);
        // Non-ASCII paths, model and effort.
        let mut req = base("verifier");
        req.cwd = p("/w\u{e9}/ws".as_bytes());
        req.model = PyStr::from("gpt-\u{e9}");
        req.effort = PyStr::from("h\u{e9}gh \u{1f600}");
        req.writable_dirs = vec![p("/d\u{2192}".as_bytes())];
        let mut c = case(req);
        c.last = p("/r\u{e9}/last-message.md".as_bytes());
        cases.push(c);
        // Non-UTF-8 bytes in every path.
        let mut req = base("final_reviewer");
        req.cwd = p(b"/w\xff/ws");
        req.writable_dirs = vec![p(b"/d\xff"), p(b"/d\xc3")];
        let mut c = case(req);
        c.last = p(b"/w\xff/last-message.md");
        c.git_common = p(b"/r\xfe/.git");
        c.extra = vec![p(b"/r\xfe/.worktrees/ws--base")];
        cases.push(c);
        // Unnormalized paths, spelled as `str(Path)` spells them.
        let mut req = base("implementer_hard");
        req.cwd = p(b"/a//b/./c/");
        req.writable_dirs = vec![p(b"./rel/"), p(b"//two/slash/")];
        let mut c = case(req);
        c.last = p(b"rel//x/./last-message.md");
        c.git_common = p(b"/repo/.git/");
        c.extra = vec![p(b"/repo/.worktrees//x--base/")];
        cases.push(c);
        // TOML escapes in the effort, a space in the model.
        let mut req = base("reviewer_hard");
        req.model = PyStr::from("gpt 6");
        req.effort = PyStr::from("a\"b\\c\nd\te\u{1}f\u{7f}g\u{8}\u{c}\r");
        cases.push(case(req));
        // Override names: one `$` accepts (a trailing newline), and three refused.
        for name in [&b"NAME\n"[..], b"1BAD", b"A-B", b"", b"N\xff"] {
            let mut c = case(base("implementer"));
            c.overrides.push((os(name), os(b"v")));
            cases.push(c);
        }
        // A bad name is refused before a bad resume ID, as the Python checks them.
        let mut req = base("implementer");
        req.resume = Some(PyStr::from("--bad"));
        let mut c = case(req);
        c.overrides.insert(0, (os(b"BAD NAME"), os(b"v")));
        cases.push(c);
        // A lone surrogate that is no surrogate escape cannot be encoded.
        let mut req = base("implementer");
        req.effort = serde_json::from_str(r#""x\ud800y""#).unwrap();
        cases.push(case(req));
        let mut req = base("implementer");
        req.model = serde_json::from_str(r#""\udfff""#).unwrap();
        cases.push(case(req));
        // No overrides at all.
        let mut c = case(base("scout"));
        c.overrides.clear();
        cases.push(c);
        cases
    }

    #[derive(Serialize)]
    struct PyCase<'a> {
        req: &'a DispatchRequest,
        last: PyStr,
        git_common: PyStr,
        overrides: Vec<(PyStr, PyStr)>,
        extra: Vec<PyStr>,
    }

    fn shown(got: Result<Vec<OsString>>) -> String {
        match got {
            Ok(argv) => {
                let hex: Vec<String> = argv
                    .iter()
                    .map(|a| a.as_bytes().iter().map(|b| format!("{b:02x}")).collect())
                    .collect();
                format!("ok {}", hex.join(" "))
            }
            Err(e) => format!("err {}", e.message),
        }
    }

    /// One row of `codex-argv.json`: a case of `table()` as the reference read it, kept as its
    /// JSON text, and the argv its `codex_argv` made of it (`shown`'s form: `ok` and each
    /// argument's bytes in hex, or `err` and the `ValueError`'s text).
    #[derive(Deserialize)]
    struct Recorded {
        case: Box<serde_json::value::RawValue>,
        argv: String,
    }

    #[test]
    fn argv_matches_reference_table() {
        let cases = table();
        let py: Vec<PyCase> = cases
            .iter()
            .map(|c| PyCase {
                req: &c.req,
                last: PyStr::from_os(c.last.as_os_str()),
                git_common: PyStr::from_os(c.git_common.as_os_str()),
                overrides: c
                    .overrides
                    .iter()
                    .map(|(k, v)| (PyStr::from_os(k), PyStr::from_os(v)))
                    .collect(),
                extra: c
                    .extra
                    .iter()
                    .map(|e| PyStr::from_os(e.as_os_str()))
                    .collect(),
            })
            .collect();
        // The reference's argv as the OS gets it (`os.fsencode`, as `subprocess` encodes it),
        // for each case as `request.json` gives it.
        let theirs: Vec<Recorded> =
            serde_json::from_slice(&read_reference("codex-argv.json")).unwrap();
        assert_eq!(theirs.len(), cases.len());
        let mut refused = 0;
        for ((c, mine), want) in cases.iter().zip(&py).zip(&theirs) {
            assert_eq!(
                want.case.get(),
                to_line(mine).unwrap(),
                "the recorded case is not the table's"
            );
            let ours = shown(argv_for(
                OsString::from("codex"),
                &c.req,
                &c.last,
                &c.git_common,
                &c.overrides,
                &c.extra,
            ));
            assert_eq!(ours, want.argv, "{:?}", c.req);
            refused += usize::from(want.argv.starts_with("err "));
        }
        // The table reaches each refusal: the resume ID, four names, the mixed case, and
        // the two encodings.
        assert_eq!(refused, 8);
    }

    #[test]
    fn argv_spells_out_the_codex_command_line() {
        let mut req = base("scout");
        req.network = true;
        req.writable_dirs = vec![p(b"/extra")];
        req.resume = Some(PyStr::from("t-1"));
        let got = argv_for(
            OsString::from("codex"),
            &req,
            Path::new("/runs/r1/workers/c1-a1/last-message.md"),
            Path::new("/repo/.git"),
            &[(os(b"LEADHAND_RUN_ID"), os(b"r1"))],
            &[p(b"/repo/.worktrees/ws--base")],
        )
        .unwrap();
        let want = [
            "codex",
            "exec",
            "-m",
            "gpt-6-luna",
            "-c",
            "model_reasoning_effort=\"medium\"",
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "sandbox_workspace_write.network_access=true",
            "-C",
            "/ws",
            "-s",
            "read-only",
            "--add-dir",
            "/repo/.git",
            "--add-dir",
            "/extra",
            "--add-dir",
            "/repo/.worktrees/ws--base",
            "-c",
            "shell_environment_policy.set.LEADHAND_RUN_ID=\"r1\"",
            "--json",
            "-o",
            "/runs/r1/workers/c1-a1/last-message.md",
            "resume",
            "t-1",
            "-",
        ];
        assert_eq!(got, want.map(OsString::from));
        // A writing role gets the workspace-write sandbox; the program is `codex` from PATH
        // unless a testing build names its fake.
        let argv = codex_argv(
            &base("implementer"),
            Path::new("/l"),
            Path::new("/g"),
            &[],
            &[],
        )
        .unwrap();
        let program = crate::testing::codex_bin()
            .map_or_else(|| OsString::from("codex"), PathBuf::into_os_string);
        assert_eq!(argv[0], program);
        assert_eq!(argv[13], "workspace-write");
        assert_eq!(argv[9], "sandbox_workspace_write.network_access=false");
    }

    #[test]
    fn codex_argv_parts_keeps_refusals_apart_from_encodings() {
        let args = |req: &DispatchRequest| {
            codex_argv_parts(req, Path::new("/l"), Path::new("/g"), &[], &[])
        };
        let good = base("implementer");
        assert_eq!(
            args(&good).unwrap().unwrap(),
            codex_argv(&good, Path::new("/l"), Path::new("/g"), &[], &[]).unwrap()
        );
        // A model the OS cannot be given: the command line is built (the Python's
        // `codex_argv` returns), and only encoding it fails.
        let lone: PyStr = serde_json::from_str(r#""gpt\ud800""#).unwrap();
        let mut unencodable = base("implementer");
        unencodable.model = lone.clone();
        let inner = args(&unencodable).unwrap().unwrap_err();
        assert!(
            inner.message.contains("surrogates not allowed"),
            "{}",
            inner.message
        );
        // A refusal comes first, as the Python raises it before anything is encoded.
        let mut refused = base("implementer");
        refused.model = lone;
        refused.resume = Some(PyStr::from("-x"));
        let outer = args(&refused).unwrap_err();
        assert_eq!(outer.message, "bad session id '-x'");
        // A surrogate escape is a byte, and encodes.
        let mut escaped = base("implementer");
        escaped.effort = PyStr::from_os(&os(b"hi\xff"));
        assert!(args(&escaped).unwrap().is_ok());
    }

    #[test]
    fn toml_string_is_python_json_dumps_without_ascii_escapes() {
        let texts: Vec<PyStr> = [
            "",
            "medium",
            "a\"b\\c",
            "\n\r\t\u{8}\u{c}",
            "\u{0}\u{1}\u{1f}\u{20}\u{7f}",
            "caf\u{e9} \u{2192} \u{1f600}",
            "</script>/",
        ]
        .iter()
        .map(|t| PyStr::from(*t))
        .chain([
            serde_json::from_str(r#""\ud800\udcff""#).unwrap(),
            PyStr::from_os(&os(b"x\xffy")),
        ])
        .collect();
        let code = "import json, sys\nfrom leadhand import dispatch as d\n\
                    print(json.dumps([d._toml_string(t) for t in json.loads(sys.argv[1])]))\n";
        let arg = to_line(&texts).unwrap();
        let out = python(code, &[OsStr::new(&arg)]);
        let theirs: Vec<PyStr> = serde_json::from_str(std::str::from_utf8(&out).unwrap()).unwrap();
        let ours: Vec<PyStr> = texts.iter().map(toml_string).collect();
        assert_eq!(ours, theirs);
        assert_eq!(
            toml_string(&PyStr::from("a\"\n")),
            PyStr::from("\"a\\\"\\n\"")
        );
    }
}
