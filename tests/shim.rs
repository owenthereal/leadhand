//! `bin/leadhand`, the plugin's entry point: which binary it runs, and what it says when it
//! can't run one (Review Focus 2). Static: these run a copy of `bin/leadhand` in a temporary
//! plugin folder, with stub binaries and a stub `uname` on PATH, and never run LEADHAND_BIN.
//! On Apple silicon, one more runs the copy under Rosetta with the system's own `uname`.

#[macro_use]
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::plugin::root;
use common::{TempRoot, write_executable};

/// The arguments every run passes, which the binary must receive unchanged.
const ARGS: [&str; 4] = ["status", "a b", "", "--x=$HOME*"];

/// `uname -v` on Apple silicon, natively and under Rosetta alike, as observed on a Mac.
const ARM64_KERNEL: &str = "Darwin Kernel Version 27.2.0: Thu Oct 15 00:02:46 PDT 2026; root:xnu-13432.41.4~25/RELEASE_ARM64_T6000";

/// `uname -v` on an Intel Mac.
const INTEL_KERNEL: &str = "Darwin Kernel Version 27.2.0: Thu Oct 15 00:02:46 PDT 2026; root:xnu-13432.41.4~25/RELEASE_X86_64";

/// `uname -v` on Linux.
const LINUX_KERNEL: &str = "#1 SMP PREEMPT_DYNAMIC Fri Nov  7 10:00:00 UTC 2026";

/// Writes an executable `sh` script.
fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_executable(path, format!("#!/bin/sh\n{body}"));
}

/// A stub binary that prints `tag`, then each argument it got, one per line.
fn stub_binary(path: &Path, tag: &str) {
    script(path, &format!("printf '%s\\n' '{tag}' \"$@\"\n"));
}

/// A plugin folder holding a copy of `bin/leadhand`, and a folder holding a `uname` that
/// reports `os` for `-s`, `arch` for `-m`, and that platform's kernel version for `-v`
/// (`ARM64_KERNEL` for Darwin arm64, `INTEL_KERNEL` for Darwin x86_64, else `LINUX_KERNEL`).
struct Tree {
    tmp: TempRoot,
    os: String,
    arch: String,
}

impl Tree {
    fn new(os: &str, arch: &str) -> Tree {
        let tmp = TempRoot::new("leadhand shim");
        let shim = tmp.join("plugin/bin/leadhand");
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        let source = root().join("bin/leadhand");
        let mode = fs::metadata(&source).unwrap().permissions().mode();
        assert!(
            mode & 0o100 != 0,
            "the checkout's bin/leadhand isn't executable (mode {mode:o}); the shim tests' copies would hide that"
        );
        write_executable(&shim, fs::read(&source).unwrap());
        let tree = Tree {
            tmp,
            os: os.to_string(),
            arch: arch.to_string(),
        };
        let kernel = match (os, arch) {
            ("Darwin", "arm64") => ARM64_KERNEL,
            ("Darwin", "x86_64") => INTEL_KERNEL,
            _ => LINUX_KERNEL,
        };
        tree.kernel(&format!("echo '{kernel}'"));
        tree
    }

    /// Makes the stub `uname` run the shell code `answer` when asked `uname -v`.
    fn kernel(&self, answer: &str) {
        script(
            &self.tmp.join("stubs/uname"),
            &format!(
                "case $1 in -s) echo '{}' ;; -m) echo '{}' ;; -v) {answer} ;; *) exit 2 ;; esac\n",
                self.os, self.arch
            ),
        );
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
        // For Darwin x86_64, the stub `uname -v` reports an Intel Mac's kernel.
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

/// A Rosetta shell on Apple silicon (an x86_64 process, as the Bash tool of a Claude Code
/// started by an x86_64 Node is) reports `Darwin x86_64`, but `uname -v` still names the arm64
/// kernel: the shim runs the darwin-arm64 binary. The kernel counts only for Darwin x86_64.
#[test]
fn rosetta_shell_runs_darwin_arm64() {
    let development = ARM64_KERNEL.replace("RELEASE_ARM64_T6000", "DEVELOPMENT_ARM64_T8103");
    let virtual_machine = ARM64_KERNEL.replace("RELEASE_ARM64_T6000", "RELEASE_ARM64_VMAPPLE");
    for (os, arch, kernel, target) in [
        ("Darwin", "x86_64", ARM64_KERNEL, "darwin-arm64"),
        ("Darwin", "x86_64", development.as_str(), "darwin-arm64"),
        ("Darwin", "x86_64", virtual_machine.as_str(), "darwin-arm64"),
        ("Darwin", "arm64", ARM64_KERNEL, "darwin-arm64"),
        ("Linux", "x86_64", ARM64_KERNEL, "linux-x86_64"),
    ] {
        let tree = Tree::new(os, arch);
        tree.kernel(&format!("echo '{kernel}'"));
        for t in ["darwin-arm64", "linux-x86_64", "linux-arm64"] {
            stub_binary(&tree.plugin().join(format!("bin/{t}/leadhand")), t);
        }
        let ran = tree.run();
        assert_eq!(ran.code, Some(0), "{os} {arch} {kernel}: {}", ran.stderr);
        assert_eq!(ran.stdout, expected_output(target), "{os} {arch} {kernel}");
        assert_eq!(ran.stderr, "", "{os} {arch} {kernel}");
    }
}

/// Darwin x86_64 that isn't translated (an Intel Mac, whose kernel says `RELEASE_X86_64`, or a
/// `uname -v` that fails, even after printing an arm64 kernel, or prints nothing or something
/// else) stays unsupported, with the one-line message and nothing of `uname`'s own.
#[test]
fn untranslated_darwin_x86_64_is_unsupported() {
    let check = |what: &str, ran: Ran| {
        let line = one_line_failure(&ran);
        assert!(
            line.contains("unsupported platform Darwin x86_64"),
            "{what}: {line}"
        );
    };
    let failing_arm64 = format!("echo '{ARM64_KERNEL}'; exit 1");
    for (what, answer) in [
        ("an Intel kernel", None),
        (
            "fails",
            Some("echo 'uname: illegal option -- v' >&2; exit 1"),
        ),
        (
            "prints an arm64 kernel and fails",
            Some(failing_arm64.as_str()),
        ),
        ("prints nothing", Some(":")),
        ("prints something else", Some("echo yes")),
    ] {
        let tree = Tree::new("Darwin", "x86_64");
        if let Some(answer) = answer {
            tree.kernel(answer);
        }
        stub_binary(&tree.plugin().join("bin/darwin-arm64/leadhand"), "dist");
        check(what, tree.run());
    }

    // No system tools beyond uname and dirname: PATH holds only the stub `uname` and the
    // `dirname` the shim needs.
    let tree = Tree::new("Darwin", "x86_64");
    stub_binary(&tree.plugin().join("bin/darwin-arm64/leadhand"), "dist");
    let tools = tree.tmp.join("tools");
    fs::create_dir_all(&tools).unwrap();
    std::os::unix::fs::symlink("/usr/bin/dirname", tools.join("dirname")).unwrap();
    let path = format!("{}:{}", tree.tmp.join("stubs").display(), tools.display());
    let mut cmd = Command::new(tree.plugin().join("bin/leadhand"));
    cmd.env_clear().env("PATH", path).args(ARGS);
    check("no system tools beyond uname and dirname", ran(&mut cmd));
}

/// Under real Rosetta, the way a translated shell runs the shim (through its `#!/bin/sh`
/// line, with the system's own `uname` and no `/usr/sbin` on PATH), it runs the darwin-arm64
/// binary with the arguments unchanged. Apple silicon only: on an Intel Mac, `x86_64` is the
/// truth and the shim rightly refuses.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn rosetta_translated_shell_runs_darwin_arm64_without_usr_sbin() {
    /// `arch -x86_64 env -i PATH=/usr/bin:/bin`: a translated process with a clean PATH, so
    /// no arm64-only `uname` (such as GNU coreutils') can answer for it.
    fn translated() -> Command {
        let mut cmd = Command::new("/usr/bin/arch");
        cmd.env_clear()
            .args(["-x86_64", "/usr/bin/env", "-i", "PATH=/usr/bin:/bin"]);
        cmd
    }

    let rosetta = Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("running /usr/bin/arch");
    if !rosetta.success() {
        panic!("needs Rosetta 2: softwareupdate --install-rosetta --agree-to-license");
    }

    // The premise: a process started this way really is translated.
    let machine = ran(translated().arg("/usr/bin/uname").arg("-m"));
    assert_eq!(machine.code, Some(0), "uname -m: {}", machine.stderr);
    assert_eq!(machine.stdout, "x86_64\n", "uname -m under arch -x86_64");

    // The stub `uname` (an Intel Mac's) is not on this PATH; the system's own answers.
    let tree = Tree::new("Darwin", "x86_64");
    for t in ["darwin-arm64", "linux-x86_64", "linux-arm64"] {
        stub_binary(&tree.plugin().join(format!("bin/{t}/leadhand")), t);
    }
    let ran = ran(translated()
        .arg(tree.plugin().join("bin/leadhand"))
        .args(ARGS));
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, expected_output("darwin-arm64"));
    assert_eq!(ran.stderr, "");
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
