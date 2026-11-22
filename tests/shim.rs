//! `bin/leadhand`, the plugin's entry point: which binary it runs, and what it says when it
//! can't run one (Review Focus 2). Static: these run a copy of `bin/leadhand` in a temporary
//! plugin folder, with stub binaries and a stub `uname` on PATH, and never run LEADHAND_BIN.

#[macro_use]
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::TempRoot;
use common::plugin::root;

/// The arguments every run passes, which the binary must receive unchanged.
const ARGS: [&str; 4] = ["status", "a b", "", "--x=$HOME*"];

/// Writes an executable `sh` script.
fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stub binary that prints `tag`, then each argument it got, one per line.
fn stub_binary(path: &Path, tag: &str) {
    script(path, &format!("printf '%s\\n' '{tag}' \"$@\"\n"));
}

/// A plugin folder holding a copy of `bin/leadhand`, and a folder holding a `uname` that
/// reports `os` for `-s` and `arch` for `-m`.
struct Tree {
    tmp: TempRoot,
}

impl Tree {
    fn new(os: &str, arch: &str) -> Tree {
        let tmp = TempRoot::new("leadhand shim");
        let shim = tmp.join("plugin/bin/leadhand");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        fs::copy(root().join("bin/leadhand"), &shim).unwrap();
        script(
            &tmp.join("stubs/uname"),
            &format!("case $1 in -s) echo '{os}' ;; -m) echo '{arch}' ;; *) exit 2 ;; esac\n"),
        );
        Tree { tmp }
    }

    fn plugin(&self) -> PathBuf {
        self.tmp.join("plugin")
    }

    /// The plugin folder as the shim names it, with symlinks resolved (`pwd -P`).
    fn plugin_real(&self) -> PathBuf {
        fs::canonicalize(self.plugin()).unwrap()
    }

    /// PATH: the stub `uname` first, then the system's tools.
    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.tmp.join("stubs").display())
    }

    /// Runs the shim by its absolute path, in an environment holding only PATH.
    fn run(&self) -> Ran {
        let mut cmd = Command::new(self.plugin().join("bin/leadhand"));
        cmd.env_clear().env("PATH", self.path());
        ran(cmd.args(ARGS))
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn ran(cmd: &mut Command) -> Ran {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .expect("running bin/leadhand");
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// What a stub binary tagged `tag` prints for `ARGS`.
fn expected_output(tag: &str) -> String {
    std::iter::once(tag)
        .chain(ARGS)
        .map(|line| format!("{line}\n"))
        .collect()
}

/// The single line a failing shim prints on stderr, after checking it failed as Review Focus 2
/// says: exit 1, nothing on stdout, one line on stderr, and never the shell's own
/// `exec: ... not found`.
#[track_caller]
fn one_line_failure(ran: &Ran) -> String {
    assert_eq!(ran.code, Some(1), "stderr: {}", ran.stderr);
    assert_eq!(ran.stdout, "");
    assert!(
        ran.stderr.ends_with('\n') && ran.stderr.matches('\n').count() == 1,
        "not one line: {:?}",
        ran.stderr
    );
    assert!(!ran.stderr.contains("not found"), "{:?}", ran.stderr);
    assert!(ran.stderr.starts_with("leadhand: "), "{:?}", ran.stderr);
    ran.stderr.trim_end().to_string()
}

/// In a `dist` tree, the shim runs `bin/<target>/leadhand` for the platform `uname` reports,
/// ahead of any `target/release/leadhand`, with the arguments unchanged.
#[test]
fn runs_platform_binary_from_dist_layout() {
    for (os, arch, target) in [
        ("Darwin", "arm64", "darwin-arm64"),
        ("Linux", "x86_64", "linux-x86_64"),
        ("Linux", "aarch64", "linux-arm64"),
        ("Linux", "arm64", "linux-arm64"),
    ] {
        let tree = Tree::new(os, arch);
        for t in ["darwin-arm64", "linux-x86_64", "linux-arm64"] {
            stub_binary(&tree.plugin().join(format!("bin/{t}/leadhand")), t);
        }
        stub_binary(&tree.plugin().join("target/release/leadhand"), "checkout");
        let ran = tree.run();
        assert_eq!(ran.code, Some(0), "{os} {arch}: {}", ran.stderr);
        assert_eq!(ran.stdout, expected_output(target), "{os} {arch}");
        assert_eq!(ran.stderr, "", "{os} {arch}");
    }

    // As Claude Code runs it, found on PATH; and by a relative path with CDPATH set, which
    // must not move the shim's `cd` into another `bin/..`.
    let tree = Tree::new("Darwin", "arm64");
    stub_binary(&tree.plugin().join("bin/darwin-arm64/leadhand"), "dist");
    let elsewhere = tree.tmp.join("elsewhere");
    stub_binary(&elsewhere.join("bin/darwin-arm64/leadhand"), "elsewhere");
    let path = format!("{}:{}", tree.plugin().join("bin").display(), tree.path());
    let mut on_path = Command::new("leadhand");
    on_path.env_clear().env("PATH", &path).args(ARGS);
    let mut relative = Command::new("/bin/sh");
    relative
        .env_clear()
        .env("PATH", tree.path())
        .env("CDPATH", &elsewhere)
        .current_dir(tree.plugin())
        .arg("bin/leadhand")
        .args(ARGS);
    for (how, cmd) in [("on PATH", &mut on_path), ("relative", &mut relative)] {
        let ran = ran(cmd);
        assert_eq!(ran.code, Some(0), "{how}: {}", ran.stderr);
        assert_eq!(ran.stdout, expected_output("dist"), "{how}");
        assert_eq!(ran.stderr, "", "{how}");
    }
}

/// In a source checkout, with no `bin/<target>/`, the shim runs the release build.
#[test]
fn falls_back_to_target_release_in_a_checkout() {
    for (os, arch) in [
        ("Darwin", "arm64"),
        ("Linux", "x86_64"),
        ("Linux", "aarch64"),
    ] {
        let tree = Tree::new(os, arch);
        stub_binary(&tree.plugin().join("target/release/leadhand"), "checkout");
        let ran = tree.run();
        assert_eq!(ran.code, Some(0), "{os} {arch}: {}", ran.stderr);
        assert_eq!(ran.stdout, expected_output("checkout"), "{os} {arch}");
        assert_eq!(ran.stderr, "", "{os} {arch}");
    }
}

/// On a platform with no leadhand binary, one line naming the platform, and exit 1, even when
/// a release build is there.
#[test]
fn unsupported_platform_message() {
    for (os, arch) in [
        ("Darwin", "x86_64"),
        ("Linux", "armv7l"),
        ("FreeBSD", "amd64"),
    ] {
        let tree = Tree::new(os, arch);
        stub_binary(&tree.plugin().join("target/release/leadhand"), "checkout");
        let line = one_line_failure(&tree.run());
        assert!(line.contains(&format!("{os} {arch}")), "{line}");
        assert!(line.contains("unsupported platform"), "{line}");
        for supported in ["darwin-arm64", "linux-x86_64", "linux-arm64"] {
            assert!(line.contains(supported), "{line}");
        }
    }
}

/// With no binary at either path, one line naming the platform, both paths it looked for and
/// how to fix it, and exit 1.
#[test]
fn missing_binary_message() {
    let tree = Tree::new("Linux", "aarch64");
    let line = one_line_failure(&tree.run());
    let plugin = tree.plugin_real();
    assert!(line.contains("linux-arm64"), "{line}");
    assert!(
        line.contains(&format!(
            "{}",
            plugin.join("bin/linux-arm64/leadhand").display()
        )),
        "{line}"
    );
    assert!(
        line.contains(&format!(
            "{}",
            plugin.join("target/release/leadhand").display()
        )),
        "{line}"
    );
    assert!(line.contains("reinstall the plugin"), "{line}");
    assert!(line.contains("cargo build --release"), "{line}");
}

/// A platform binary that is there but not executable (a lost executable bit) gets one line
/// naming it, not the shell's `Permission denied`.
#[test]
fn non_executable_binary_message() {
    let tree = Tree::new("Darwin", "arm64");
    let bin = tree.plugin().join("bin/darwin-arm64/leadhand");
    stub_binary(&bin, "dist");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
    let line = one_line_failure(&tree.run());
    let real = tree.plugin_real().join("bin/darwin-arm64/leadhand");
    assert!(line.contains(&format!("{}", real.display())), "{line}");
    assert!(line.contains("not executable"), "{line}");
    assert!(line.contains("reinstall the plugin"), "{line}");
}
