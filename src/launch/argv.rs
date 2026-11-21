//! The supervisor's command line (contract item 11, P1): `launch.launch_argv`, its first
//! message and its `--settings` overlay. The first message and the overlay embed the CLI's
//! path and the run directory, so they are built from OS bytes and kept lossless (ruling
//! S2-R15).

use std::ffi::{OsStr, OsString};
use std::path::Path;

use serde::Serialize;

use crate::error::Result;
use crate::wire::{self, PyStr};

/// The supervisor agent (`docs/compatibility.md`, Supervisor launch).
pub const SUPERVISOR_AGENT: &str = "leadhand:supervisor";
/// `run --print` (hidden; end-to-end drivers only): a headless session whose stream goes to
/// the launcher's stdout. The prompt stays the last argument.
pub const PRINT_ARGS: [&str; 4] = ["-p", "--output-format", "stream-json", "--verbose"];

/// `launch_argv`'s inputs, as the Python's keyword arguments.
#[derive(Clone, Copy, Debug)]
pub struct LaunchArgs<'a> {
    pub claude_bin: &'a OsStr,
    pub agent: &'a str,
    pub first_message: &'a OsStr,
    pub plugin_dir: Option<&'a Path>,
    pub settings: Option<&'a OsStr>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub print_mode: bool,
}

/// The `claude` argv, in P1's order: `--plugin-dir` (when given), `--settings`, `--agent`,
/// `--model` and `--effort` (when not empty), `--permission-mode auto`, the print flags (with
/// `--print`), then the first message.
pub fn launch_argv(a: &LaunchArgs) -> Vec<OsString> {
    let mut argv = vec![a.claude_bin.to_os_string()];
    if let Some(dir) = a.plugin_dir {
        argv.extend(["--plugin-dir".into(), dir.as_os_str().to_os_string()]);
    }
    if let Some(settings) = a.settings {
        argv.extend(["--settings".into(), settings.to_os_string()]);
    }
    argv.extend(["--agent".into(), a.agent.into()]);
    if let Some(model) = a.model.filter(|m| !m.is_empty()) {
        argv.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = a.effort.filter(|e| !e.is_empty()) {
        argv.extend(["--effort".into(), effort.into()]);
    }
    argv.extend(["--permission-mode".into(), "auto".into()]);
    if a.print_mode {
        argv.extend(PRINT_ARGS.map(OsString::from));
    }
    argv.push(a.first_message.to_os_string());
    argv
}

#[derive(Serialize)]
struct Settings {
    sandbox: Sandbox,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Sandbox {
    excluded_commands: [PyStr; 1],
}

/// The `--settings` overlay (`launch.cli_settings`): only the leadhand CLI's sandbox
/// exclusion, `"<cli> *"`, as Python's `json.dumps` writes it (ASCII; a byte that is not
/// UTF-8 as its `\udcXX` escape).
pub fn cli_settings(cli: &Path) -> Result<OsString> {
    let entry = PyStr::from_os_parts(&[cli.as_os_str(), OsStr::new(" *")]);
    let settings = Settings {
        sandbox: Sandbox {
            excluded_commands: [entry],
        },
    };
    Ok(wire::to_output(&settings)?.into())
}

/// The first message (`launch.first_message`): the run directory and the CLI, then the
/// instruction to invoke the run skill first.
pub fn first_message(run_dir: &Path, cli: &Path) -> OsString {
    let mut text = OsString::from("Run the leadhand deck in ");
    text.push(run_dir);
    text.push(". leadhand CLI: ");
    text.push(cli);
    text.push(". First invoke the Skill tool with leadhand:run, then follow it.");
    text
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn words(argv: &[OsString]) -> Vec<&str> {
        argv.iter().map(|w| w.to_str().unwrap()).collect()
    }

    fn args<'a>(first_message: &'a OsStr, settings: &'a OsStr) -> LaunchArgs<'a> {
        LaunchArgs {
            claude_bin: OsStr::new("claude"),
            agent: SUPERVISOR_AGENT,
            first_message,
            plugin_dir: None,
            settings: Some(settings),
            model: Some("sonnet"),
            effort: Some("low"),
            print_mode: false,
        }
    }

    #[test]
    fn launch_argv_order_and_auto_mode() {
        let (msg, settings) = (OsStr::new("go"), OsStr::new("{}"));
        assert_eq!(
            words(&launch_argv(&args(msg, settings))),
            [
                "claude",
                "--settings",
                "{}",
                "--agent",
                "leadhand:supervisor",
                "--model",
                "sonnet",
                "--effort",
                "low",
                "--permission-mode",
                "auto",
                "go"
            ]
        );
        let full = LaunchArgs {
            plugin_dir: Some(Path::new("/p dir")),
            print_mode: true,
            ..args(msg, settings)
        };
        assert_eq!(
            words(&launch_argv(&full)),
            [
                "claude",
                "--plugin-dir",
                "/p dir",
                "--settings",
                "{}",
                "--agent",
                "leadhand:supervisor",
                "--model",
                "sonnet",
                "--effort",
                "low",
                "--permission-mode",
                "auto",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "go"
            ]
        );
        // No route, or an empty model or effort: neither flag (`if model:`).
        let bare = LaunchArgs {
            model: Some(""),
            effort: None,
            settings: None,
            ..args(msg, settings)
        };
        assert_eq!(
            words(&launch_argv(&bare)),
            [
                "claude",
                "--agent",
                "leadhand:supervisor",
                "--permission-mode",
                "auto",
                "go"
            ]
        );
    }

    #[test]
    fn settings_and_first_message_keep_the_paths_bytes() {
        let cli = Path::new(OsStr::from_bytes(b"/opt/lead hand/\xffbin/leadhand"));
        assert_eq!(
            cli_settings(Path::new("/opt/leadhand")).unwrap(),
            "{\"sandbox\": {\"excludedCommands\": [\"/opt/leadhand *\"]}}"
        );
        assert_eq!(
            cli_settings(cli).unwrap(),
            "{\"sandbox\": {\"excludedCommands\": [\"/opt/lead hand/\\udcffbin/leadhand *\"]}}"
        );
        let run = Path::new(OsStr::from_bytes(b"/runs/r\xfe"));
        assert_eq!(
            first_message(run, cli).as_bytes(),
            b"Run the leadhand deck in /runs/r\xfe. leadhand CLI: /opt/lead hand/\xffbin/leadhand. \
              First invoke the Skill tool with leadhand:run, then follow it."
        );
    }
}
