//! The Codex worker's environment (`dispatch.worker_env`, `blocking_env`; contract item 9).
//!
//! Codex 0.167's default environment policy passes KEY/TOKEN names through, so the
//! credential variables are removed from Codex's own environment; the blocking variables
//! are set there and also passed through `shell_environment_policy.set` (`codex_overrides`),
//! which survives `inherit = "core"` configs. `LEADHAND_DISPATCH_TAG` marks every process
//! the worker starts, so the runner finds one that left Codex's process tree (M4).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::pytext::{pure_path, py_isspace};
use crate::state::events::py_int;

/// An environment, as the process has it.
pub type Env = BTreeMap<OsString, OsString>;

/// Removed from Codex's environment.
pub const CREDENTIAL_VARS: [&str; 7] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "SSH_AUTH_SOCK",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
];
/// The fixed blocking variables. `blocking_env` adds `GH_CONFIG_DIR=<empty dir>` and the
/// `GIT_CONFIG_COUNT`/`KEY`/`VALUE` entries that reset `credential.helper` after any
/// existing ones.
pub const BLOCKING_ENV: [(&str, &str); 2] =
    [("GIT_SSH_COMMAND", "false"), ("GIT_TERMINAL_PROMPT", "0")];
/// Git's "clear the helper list".
pub const CREDENTIAL_HELPER_RESET: (&str, &str) = ("credential.helper", "");
/// Appended to `GIT_CONFIG_PARAMETERS` (command-line config, read after the `GIT_CONFIG_*`
/// entries): an empty `core.askPass` stops Git running an askpass program named in the
/// user's config, and also stops it falling back to `SSH_ASKPASS`.
pub const ASKPASS_RESET: &str = "'core.askPass='";
/// `<run id>:<dispatch id>:<generation>`, set in Codex's environment and its tool shells'.
pub const TAG_VAR: &str = "LEADHAND_DISPATCH_TAG";

/// The variables that block credential use, in the Python's order: `BLOCKING_ENV`, an empty
/// `GH_CONFIG_DIR`, every `GIT_CONFIG_*` entry (the parent's, in order, then
/// `credential.helper` reset to empty), and `GIT_CONFIG_PARAMETERS` with the parent's
/// entries followed by an empty `core.askPass`.
pub fn blocking_env(parent: &Env, gh_config_dir: &Path) -> Vec<(OsString, OsString)> {
    let get = |name: &str| parent.get(OsStr::new(name));
    let var = |name: &str| get(name).cloned().unwrap_or_default();
    let mut out: Vec<(OsString, OsString)> = BLOCKING_ENV
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    // `str(gh_config_dir)`.
    out.push((
        OsString::from("GH_CONFIG_DIR"),
        pure_path(gh_config_dir.as_os_str()).into_os_string(),
    ));
    // `max(0, int(… or "0"))`, 0 where `int` raises.
    let count = get("GIT_CONFIG_COUNT")
        .and_then(|v| v.to_str())
        .filter(|v| !v.is_empty())
        .map_or(Some(0), py_int)
        .unwrap_or(0)
        .max(0);
    for i in 0..count {
        let (key, value) = (
            format!("GIT_CONFIG_KEY_{i}"),
            format!("GIT_CONFIG_VALUE_{i}"),
        );
        out.push((OsString::from(&key), var(&key)));
        out.push((OsString::from(&value), var(&value)));
    }
    let (key, value) = CREDENTIAL_HELPER_RESET;
    out.push((format!("GIT_CONFIG_KEY_{count}").into(), key.into()));
    out.push((format!("GIT_CONFIG_VALUE_{count}").into(), value.into()));
    out.push(("GIT_CONFIG_COUNT".into(), (count + 1).to_string().into()));
    let params = var("GIT_CONFIG_PARAMETERS");
    let params = py_strip_os(&params);
    let mut joined = params.to_os_string();
    if !params.is_empty() {
        joined.push(" ");
    }
    joined.push(ASKPASS_RESET);
    out.push(("GIT_CONFIG_PARAMETERS".into(), joined));
    out
}

/// `str.strip()` of OS text decoded with `surrogateescape`: Python's whitespace is cut from
/// both ends; an undecodable byte (a surrogate escape) is never whitespace.
fn py_strip_os(s: &OsStr) -> &OsStr {
    let bytes = s.as_bytes();
    // Each piece of the text: (start, end, whether it is whitespace).
    let mut pieces = Vec::new();
    let mut at = 0;
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            pieces.push((at, at + c.len_utf8(), py_isspace(c)));
            at += c.len_utf8();
        }
        for _ in chunk.invalid() {
            pieces.push((at, at + 1, false));
            at += 1;
        }
    }
    let Some(first) = pieces.iter().position(|p| !p.2) else {
        return OsStr::new("");
    };
    let last = pieces.iter().rposition(|p| !p.2).unwrap_or(first);
    OsStr::from_bytes(&bytes[pieces[first].0..pieces[last].1])
}

/// Codex's environment: the parent's without `CREDENTIAL_VARS`, with `LEADHAND_RUN_ID`,
/// the blocking variables and the dispatch tag (`run_runner`: `{**worker_env(…), TAG_VAR:
/// tag}`).
pub fn worker_env(parent: &Env, run_id: &str, gh_config_dir: &Path, tag: &str) -> Env {
    let mut env: Env = parent
        .iter()
        .filter(|(k, _)| {
            !CREDENTIAL_VARS
                .iter()
                .any(|c| OsStr::new(c) == k.as_os_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.insert("LEADHAND_RUN_ID".into(), run_id.into());
    env.extend(blocking_env(parent, gh_config_dir));
    env.insert(TAG_VAR.into(), tag.into());
    env
}

/// What Codex passes on to its tool shells through `shell_environment_policy.set`, in order:
/// the blocking variables, `LEADHAND_RUN_ID` and the dispatch tag (`run_runner`).
pub fn codex_overrides(
    parent: &Env,
    run_id: &str,
    gh_config_dir: &Path,
    tag: &str,
) -> Vec<(OsString, OsString)> {
    let mut out = blocking_env(parent, gh_config_dir);
    out.push(("LEADHAND_RUN_ID".into(), run_id.into()));
    out.push((TAG_VAR.into(), tag.into()));
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    use super::*;
    use crate::wire::tests::reference_outputs;
    use crate::wire::{PyStr, ascii_json, to_line};

    fn env(pairs: &[(&str, &[u8])]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from_vec(v.to_vec())))
            .collect()
    }

    fn os_pairs(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    type Pairs = Vec<(PyStr, PyStr)>;

    fn to_os(pairs: Pairs) -> Vec<(OsString, OsString)> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_os().unwrap(), v.to_os().unwrap()))
            .collect()
    }

    /// The parents the Python and Rust environments are compared over.
    fn parents() -> Vec<Env> {
        vec![
            Env::new(),
            env(&[
                ("HOME", b"/home/o"),
                ("PATH", b"/usr/bin:/bin"),
                ("GH_TOKEN", b"ghp_x"),
                ("GITHUB_TOKEN", b"ghs_x"),
                ("GH_ENTERPRISE_TOKEN", b"e"),
                ("GITHUB_ENTERPRISE_TOKEN", b"e"),
                ("SSH_AUTH_SOCK", b"/tmp/agent.sock"),
                ("GIT_ASKPASS", b"/bin/askpass"),
                ("SSH_ASKPASS", b"/bin/ssh-askpass"),
                ("OPENAI_API_KEY", b"sk-kept"),
                ("LEADHAND_RUN_ID", b"another-run"),
                ("LEADHAND_DISPATCH_TAG", b"another:tag:1"),
                ("GIT_SSH_COMMAND", b"ssh -i key"),
                ("GH_CONFIG_DIR", b"/home/o/.config/gh"),
            ]),
            env(&[
                ("GIT_CONFIG_COUNT", b"2"),
                ("GIT_CONFIG_KEY_0", b"http.https://h/.extraHeader"),
                ("GIT_CONFIG_VALUE_0", b"Authorization: x \xff"),
                ("GIT_CONFIG_KEY_1", b"user.n\xc3\xa9"),
                ("GIT_CONFIG_KEY_5", b"stray.key"),
                ("GIT_CONFIG_PARAMETERS", b"  'a.b=c'  "),
            ]),
            env(&[("GIT_CONFIG_COUNT", b"-3")]),
            env(&[("GIT_CONFIG_COUNT", b"abc")]),
            env(&[("GIT_CONFIG_COUNT", b"")]),
            env(&[
                ("GIT_CONFIG_COUNT", b" +1 "),
                ("GIT_CONFIG_KEY_0", b"k\xfe"),
            ]),
            env(&[("GIT_CONFIG_COUNT", b"1_0")]),
            env(&[("GIT_CONFIG_COUNT", b"\xc2\xa02\xe2\x80\x83")]),
            env(&[("GIT_CONFIG_COUNT", b"1\xff")]),
            env(&[("GIT_CONFIG_PARAMETERS", b"   ")]),
            env(&[("GIT_CONFIG_PARAMETERS", b"\xc2\xa0'x.y=z'\xe2\x80\x83\n")]),
            env(&[("GIT_CONFIG_PARAMETERS", b"\xff'u.v=w' \xfe")]),
        ]
    }

    #[test]
    fn environments_match_python() {
        let parents = parents();
        let cases: Vec<Pairs> = parents
            .iter()
            .map(|p| {
                p.iter()
                    .map(|(k, v)| (PyStr::from_os(k), PyStr::from_os(v)))
                    .collect()
            })
            .collect();
        // `str(gh_dir)` is the pathlib spelling.
        let gh = OsStr::from_bytes(b"/w s//r\xff/./workers/c1-a1/gh-config/");
        // The reference's `blocking_env`, `worker_env` with the tag and the Codex overrides of
        // each parent, recorded for these parents (as JSON), this folder (its JSON text), the
        // run ID and the tag.
        let arg = to_line(&cases).unwrap();
        let given = [
            arg,
            ascii_json(&PyStr::from_os(gh)),
            "r-1".into(),
            "r-1:c1-a1:2".into(),
        ];
        let out = reference_outputs(
            "dispatch/env.json",
            "environments",
            &[serde_json::json!(given)],
        );
        let theirs: Vec<(Pairs, Pairs, Pairs)> =
            serde_json::from_str(out[0].as_str().unwrap()).unwrap();
        assert_eq!(theirs.len(), parents.len());
        let gh = Path::new(gh);
        for (parent, (b, w, o)) in parents.iter().zip(theirs) {
            assert_eq!(
                blocking_env(parent, gh),
                to_os(b),
                "blocking_env {parent:?}"
            );
            let want_env: Env = to_os(w).into_iter().collect();
            assert_eq!(
                worker_env(parent, "r-1", gh, "r-1:c1-a1:2"),
                want_env,
                "worker_env {parent:?}"
            );
            assert_eq!(
                codex_overrides(parent, "r-1", gh, "r-1:c1-a1:2"),
                to_os(o),
                "overrides {parent:?}"
            );
        }
    }

    #[test]
    fn blocking_env_seals_git_and_gh_credentials() {
        assert_eq!(
            blocking_env(&Env::new(), Path::new("/run/workers/c1/gh-config")),
            os_pairs(&[
                ("GIT_SSH_COMMAND", "false"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GH_CONFIG_DIR", "/run/workers/c1/gh-config"),
                ("GIT_CONFIG_KEY_0", "credential.helper"),
                ("GIT_CONFIG_VALUE_0", ""),
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_PARAMETERS", "'core.askPass='"),
            ])
        );
        // The parent's entries come first, so the reset is read after them.
        let parent = env(&[
            ("GIT_CONFIG_COUNT", b"1"),
            ("GIT_CONFIG_KEY_0", b"credential.helper"),
            ("GIT_CONFIG_VALUE_0", b"osxkeychain"),
            ("GIT_CONFIG_PARAMETERS", b"'core.askPass=/bin/ask'"),
        ]);
        assert_eq!(
            blocking_env(&parent, Path::new("/gh")),
            os_pairs(&[
                ("GIT_SSH_COMMAND", "false"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GH_CONFIG_DIR", "/gh"),
                ("GIT_CONFIG_KEY_0", "credential.helper"),
                ("GIT_CONFIG_VALUE_0", "osxkeychain"),
                ("GIT_CONFIG_KEY_1", "credential.helper"),
                ("GIT_CONFIG_VALUE_1", ""),
                ("GIT_CONFIG_COUNT", "2"),
                (
                    "GIT_CONFIG_PARAMETERS",
                    "'core.askPass=/bin/ask' 'core.askPass='"
                ),
            ])
        );
    }

    #[test]
    fn worker_env_drops_every_credential_variable_and_tags_the_worker() {
        let mut pairs: Vec<(&str, &[u8])> = CREDENTIAL_VARS
            .iter()
            .map(|k| (*k, &b"secret"[..]))
            .collect();
        pairs.push(("HOME", b"/home/o"));
        let got = worker_env(&env(&pairs), "r-1", Path::new("/gh"), "r-1:c1:1");
        for var in CREDENTIAL_VARS {
            assert!(!got.contains_key(OsStr::new(var)), "{var}");
        }
        let get = |k: &str| got.get(OsStr::new(k)).map(|v| v.to_str().unwrap());
        assert_eq!(get("HOME"), Some("/home/o"));
        assert_eq!(get("LEADHAND_RUN_ID"), Some("r-1"));
        assert_eq!(get(TAG_VAR), Some("r-1:c1:1"));
        assert_eq!(get("GIT_SSH_COMMAND"), Some("false"));
        assert_eq!(get("GH_CONFIG_DIR"), Some("/gh"));
        assert_eq!(TAG_VAR, "LEADHAND_DISPATCH_TAG");
    }
}
