//! The `dist` tree and the release workflow: `scripts/assemble-dist.sh` builds a complete,
//! self-installable plugin tree or refuses (Review Focus 3 and 4), its `--check` can't pass a
//! broken tree, and `.github/workflows/release.yml` publishes only from a version tag. Static:
//! these run the assembly script on stub binaries and read the workflow as text, and never run
//! LEADHAND_BIN.

#[macro_use]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::TempRoot;
use common::plugin::{read, root};

const VERSION: &str = "0.2.1";

/// Another version, for the cases where a version must disagree with `VERSION`. Plainly
/// synthetic, so that no bump of `VERSION` can turn a mismatch case into a match.
const OTHER_VERSION: &str = "999.999.999";

/// What a `dist` tree holds besides the binaries, as the brief lists it.
const PLUGIN_PATHS: [&str; 9] = [
    ".claude-plugin",
    ".codex-plugin",
    "agents",
    "skills",
    "presets",
    "bin/leadhand",
    "docs",
    "README.md",
    "LICENSE",
];

/// The platforms, as `bin/<target>/leadhand` names them.
const TARGETS: [&str; 3] = ["darwin-arm64", "linux-x86_64", "linux-arm64"];

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `sh <script> <args>` in the package root.
fn sh(script: &Path, args: &[&str]) -> Ran {
    let out = Command::new("/bin/sh")
        .arg(script)
        .args(args)
        .current_dir(root())
        .stdin(Stdio::null())
        .output()
        .expect("running assemble-dist.sh");
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn script() -> PathBuf {
    root().join("scripts/assemble-dist.sh")
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 temp path")
}

/// `<dir>/<target>/leadhand` for each target, a stub naming its target, mode 0644: the
/// assembly must set the executable bit itself, as it must for binaries downloaded from an
/// artefact.
fn stub_binaries(dir: &Path) {
    for t in TARGETS {
        let bin = dir.join(format!("{t}/leadhand"));
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, format!("stub binary for {t}\n")).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

/// Assembles `version` with `script` into `<tmp>/dist`, from stub binaries in `<tmp>/bins`.
fn assemble_with(script: &Path, tmp: &Path, version: &str) -> (Ran, PathBuf) {
    let bins = tmp.join("bins");
    if !bins.exists() {
        stub_binaries(&bins);
    }
    let out = tmp.join("dist");
    let ran = sh(script, &[version, path_str(&out), path_str(&bins)]);
    (ran, out)
}

/// Assembles the package's own tree at its version, which must succeed.
#[track_caller]
fn assembled(tmp: &Path) -> PathBuf {
    let (ran, out) = assemble_with(&script(), tmp, VERSION);
    assert_eq!(ran.code, Some(0), "assembly failed: {}", ran.stderr);
    out
}

fn check(dir: &Path) -> Ran {
    sh(&script(), &["--check", path_str(dir)])
}

/// Every file under `path`, relative to `base`, with its bytes.
fn files_under(base: &Path, path: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    let full = base.join(path);
    if full.is_dir() {
        for entry in fs::read_dir(&full).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            files_under(base, &format!("{path}/{name}"), out);
        }
    } else {
        out.insert(path.to_string(), fs::read(&full).unwrap());
    }
}

fn all_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        files_under(dir, &name, &mut out);
    }
    out
}

/// Copies `from` to `to`, keeping file modes.
fn copy_tree(from: &Path, to: &Path) {
    if from.is_dir() {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        }
    } else {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(from, to).unwrap();
    }
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&read(path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn write_json(path: &Path, value: &serde_json::Value) {
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

/// The leadhand entry of a marketplace.json.
fn leadhand_entry(marketplace: &mut serde_json::Value) -> &mut serde_json::Value {
    marketplace["plugins"]
        .as_array_mut()
        .expect("a plugins array")
        .iter_mut()
        .find(|p| p["name"] == "leadhand")
        .expect("a leadhand entry")
}

/// The four places that carry the version (ruling S3-R13); the dist tree has the last three.
const VERSIONED: [&str; 4] = [
    "Cargo.toml",
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    ".claude-plugin/marketplace.json",
];

/// Sets the version of `file`, one of `VERSIONED`, in the tree at `dir`.
fn set_version(dir: &Path, file: &str, version: &str) {
    let path = dir.join(file);
    match file {
        "Cargo.toml" => {
            let from = format!("version = \"{VERSION}\"");
            let text = read(&path);
            assert!(text.contains(&from), "Cargo.toml's version");
            let to = format!("version = \"{version}\"");
            fs::write(&path, text.replacen(&from, &to, 1)).unwrap();
        }
        _ => {
            let mut manifest = json(&path);
            match file {
                ".claude-plugin/marketplace.json" => {
                    leadhand_entry(&mut manifest)["version"] = version.into()
                }
                _ => manifest["version"] = version.into(),
            }
            write_json(&path, &manifest);
        }
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Review Focus 3: the assembly copies exactly the plugin files and the three binaries, mode
/// 0755, and its own `--check` passes on the result.
#[test]
fn assembled_tree_has_every_plugin_file_and_executable_binaries() {
    let tmp = TempRoot::new("leadhand dist");
    let out = assembled(&tmp);
    let ran = check(&out);
    assert_eq!(ran.code, Some(0), "--check failed: {}", ran.stderr);
    assert!(ran.stdout.contains(VERSION), "{}", ran.stdout);

    // Exactly the tracked files under the listed paths and the binaries, byte for byte, except
    // the marketplace's rewritten source (assembled_marketplace_points_at_its_own_root).
    let mut want = BTreeMap::new();
    for path in tracked_plugin_files(&root()) {
        let bytes = fs::read(root().join(&path)).unwrap();
        want.insert(path, bytes);
    }
    for t in TARGETS {
        want.insert(
            format!("bin/{t}/leadhand"),
            format!("stub binary for {t}\n").into_bytes(),
        );
    }
    let mut got = all_files(&out);
    let marketplace = ".claude-plugin/marketplace.json";
    assert!(got.remove(marketplace).is_some(), "no {marketplace}");
    assert!(want.remove(marketplace).is_some());
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    );
    for (path, bytes) in &want {
        assert!(got[path] == *bytes, "{path} differs from its source");
    }
    // Every file the brief names is there: a source folder that lost its files would leave
    // the comparison above with less checked.
    for path in [
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        "agents/supervisor.md",
        "skills/run/SKILL.md",
        "docs/cli-contract.md",
        "README.md",
        "LICENSE",
    ] {
        assert!(got.contains_key(path), "no {path}");
    }
    assert!(got.keys().filter(|p| p.starts_with("presets/")).count() >= 2);

    for t in TARGETS {
        assert_eq!(mode(&out.join(format!("bin/{t}/leadhand"))), 0o755, "{t}");
    }
    assert_eq!(mode(&out.join("bin/leadhand")) & 0o111, 0o111);

    // An output folder that already holds files is refused, so no stale file can ride along.
    let (ran, _) = assemble_with(&script(), &tmp, VERSION);
    assert_eq!(ran.code, Some(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("not empty"), "{}", ran.stderr);
}

/// `--check` can't pass a broken tree: each missing plugin path, each missing or
/// non-executable binary, a stray file, a manifest that isn't JSON and manifests that disagree
/// on the version each fail it, naming what's wrong.
#[test]
fn check_refuses_an_incomplete_tree() {
    let tmp = TempRoot::new("leadhand dist");
    let good = assembled(&tmp);
    let mut n = 0;
    let mut case = |what: &str, named: &str, spoil: &dyn Fn(&Path)| {
        n += 1;
        let dir = tmp.join(format!("case-{n}"));
        copy_tree(&good, &dir);
        spoil(&dir);
        let ran = check(&dir);
        assert_eq!(ran.code, Some(1), "{what}: {}", ran.stderr);
        assert!(ran.stderr.contains(named), "{what}: {}", ran.stderr);
        assert_eq!(ran.stdout, "", "{what}");
    };
    for path in PLUGIN_PATHS {
        case(&format!("without {path}"), path, &|dir| {
            let p = dir.join(path);
            if p.is_dir() {
                fs::remove_dir_all(p).unwrap();
            } else {
                fs::remove_file(p).unwrap();
            }
        });
    }
    for t in TARGETS {
        let bin = format!("bin/{t}/leadhand");
        case(&format!("without {bin}"), &bin, &|dir| {
            fs::remove_file(dir.join(&bin)).unwrap()
        });
        case(&format!("{bin} not executable"), &bin, &|dir| {
            fs::set_permissions(dir.join(&bin), fs::Permissions::from_mode(0o644)).unwrap()
        });
    }
    case("bin/leadhand not executable", "bin/leadhand", &|dir| {
        fs::set_permissions(dir.join("bin/leadhand"), fs::Permissions::from_mode(0o644)).unwrap()
    });
    case("bin/leadhand a folder", "bin/leadhand", &|dir| {
        fs::remove_file(dir.join("bin/leadhand")).unwrap();
        // Not empty, so only the file check can catch it.
        fs::create_dir(dir.join("bin/leadhand")).unwrap();
        fs::write(dir.join("bin/leadhand/leadhand"), "").unwrap();
    });
    // A manifest gone from a folder that is still there.
    for manifest in &VERSIONED[1..] {
        case(
            &format!("without {manifest}"),
            &format!("missing {manifest}"),
            &|dir| fs::remove_file(dir.join(manifest)).unwrap(),
        );
    }
    // A stray name that is also a glob matching an expected name (in the package root, the
    // cwd): the check must compare names, not expand them.
    case("a stray [a]gents", "unexpected [a]gents", &|dir| {
        fs::write(dir.join("[a]gents"), "").unwrap()
    });
    case("an empty agents folder", "agents", &|dir| {
        fs::remove_dir_all(dir.join("agents")).unwrap();
        fs::create_dir(dir.join("agents")).unwrap();
    });
    case("a stray file", "Cargo.toml", &|dir| {
        fs::write(dir.join("Cargo.toml"), "").unwrap()
    });
    case("a stray platform", "bin/windows-x86_64", &|dir| {
        fs::create_dir(dir.join("bin/windows-x86_64")).unwrap()
    });
    case(
        "an extra file for a target",
        "unexpected bin/darwin-arm64/extra",
        &|dir| fs::write(dir.join("bin/darwin-arm64/extra"), "").unwrap(),
    );
    // A nested folder emptied, so the top-level skills/ still holds something.
    case("an empty skills/run", "empty folder skills/run", &|dir| {
        for entry in fs::read_dir(dir.join("skills/run")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
    });
    case(
        "a manifest that isn't JSON",
        ".codex-plugin/plugin.json",
        &|dir| fs::write(dir.join(".codex-plugin/plugin.json"), "{\"version\": ").unwrap(),
    );
    for manifest in &VERSIONED[1..] {
        case(
            &format!("{manifest} at another version"),
            manifest,
            &|dir| set_version(dir, manifest, OTHER_VERSION),
        );
    }
    case("a missing folder", "no such folder", &|dir| {
        fs::remove_dir_all(dir).unwrap()
    });
    // And `--check` changed nothing in the good tree.
    let before = all_files(&good);
    assert_eq!(check(&good).code, Some(0));
    assert!(all_files(&good) == before);
}

/// Copies the package's sources the assembly reads (its script, `Cargo.toml` and the plugin
/// files) into `dir`.
fn copy_sources(dir: &Path) {
    for path in PLUGIN_PATHS
        .iter()
        .chain(&["Cargo.toml", "scripts/assemble-dist.sh"])
    {
        copy_tree(&root().join(path), &dir.join(path));
    }
}

/// A copy of the package's sources in `<tmp>/src`, a git repository with the copied files
/// committed, for changing a version or a file without touching the package.
fn source_copy(tmp: &Path) -> PathBuf {
    let src = common::git::make_repo_named(tmp, "src", "main");
    copy_sources(&src);
    common::git::run_git(&src, &["add", "--all"]);
    common::git::run_git(&src, &["commit", "--quiet", "-m", "the sources"]);
    src
}

/// The plugin files git tracks in `dir`, sorted: `git ls-files -z -- <PLUGIN_PATHS>`.
fn tracked_plugin_files(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z", "--"])
        .args(PLUGIN_PATHS)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("running git ls-files");
    assert!(
        out.status.success(),
        "git ls-files in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut files: Vec<String> = String::from_utf8(out.stdout)
        .expect("UTF-8 file names")
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    files.sort();
    files
}

/// Assembles the source copy `src` at `VERSION`, with its own script, into `<tmp>/dist`.
fn assemble_copy(src: &Path, tmp: &Path) -> (Ran, PathBuf) {
    assemble_with(&src.join("scripts/assemble-dist.sh"), tmp, VERSION)
}

/// Asserts the source copy `src` assembles into `<tmp>/dist` and the result passes `--check`.
#[track_caller]
fn assembles(src: &Path, tmp: &Path) -> PathBuf {
    let (ran, out) = assemble_copy(src, tmp);
    assert_eq!(ran.code, Some(0), "assembly failed: {}", ran.stderr);
    let ran = check(&out);
    assert_eq!(ran.code, Some(0), "--check failed: {}", ran.stderr);
    out
}

/// Asserts the source copy `src` is refused, naming `named`, with nothing written.
#[track_caller]
fn refused(src: &Path, tmp: &Path, named: &str) {
    let (ran, out) = assemble_copy(src, tmp);
    assert_eq!(ran.code, Some(1), "{named}: {}", ran.stderr);
    assert!(ran.stderr.contains(named), "{named}: {}", ran.stderr);
    assert!(!out.exists(), "{named}: wrote {}", out.display());
}

/// An untracked file in a plugin folder, at its top or nested, isn't copied.
#[test]
fn assembly_copies_only_tracked_files() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    fs::write(src.join("skills/.DS_Store"), "untracked\n").unwrap();
    fs::write(src.join("skills/run/notes.txt"), "untracked\n").unwrap();
    let out = assembles(&src, &tmp);
    for path in ["skills/.DS_Store", "skills/run/notes.txt"] {
        assert!(!out.join(path).exists(), "{path} was copied");
    }
    assert!(out.join("skills/run/SKILL.md").is_file());
}

/// Review Focus 5: a local dry run copies the working tree's content of a tracked file,
/// uncommitted edits included.
#[test]
fn assembly_copies_the_working_tree_of_tracked_files() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    let path = "docs/deck-format.md";
    let edited = format!("{}\nAn uncommitted edit.\n", read(&src.join(path)));
    fs::write(src.join(path), &edited).unwrap();
    let out = assembles(&src, &tmp);
    assert!(fs::read(out.join(path)).unwrap() == edited.as_bytes());
}

/// Review Focus 3: tracked names with a space or a leading `-` arrive byte for byte.
#[test]
fn assembly_keeps_names_with_spaces_and_dashes() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    let files = [
        ("docs/with space.md", "a name with a space\n"),
        ("docs/-dash.md", "a name with a leading dash\n"),
    ];
    common::git::commit(&src, &files, "odd names");
    let out = assembles(&src, &tmp);
    for (path, text) in files {
        assert!(
            fs::read(out.join(path)).unwrap() == text.as_bytes(),
            "{path}"
        );
    }
}

/// A tracked file deleted from the working tree, still in the index, is refused by name before
/// anything is written.
#[test]
fn assembly_refuses_a_missing_tracked_file() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    fs::remove_file(src.join("docs/compatibility.md")).unwrap();
    refused(&src, &tmp, "missing tracked file docs/compatibility.md");
}

/// A tracked file replaced by a directory is refused by name before anything is written; its
/// untracked contents aren't packaged.
#[test]
fn assembly_refuses_a_tracked_file_replaced_by_a_directory() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    let path = src.join("docs/compatibility.md");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("notes.md"), "untracked\n").unwrap();
    refused(
        &src,
        &tmp,
        "tracked file docs/compatibility.md is not a regular file",
    );
}

/// A tracked file replaced by a symlink, to a file with the same content, is refused by name
/// before anything is written.
#[test]
fn assembly_refuses_a_tracked_file_replaced_by_a_symlink() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    let path = src.join("docs/compatibility.md");
    let elsewhere = tmp.join("compatibility.md");
    fs::copy(&path, &elsewhere).unwrap();
    fs::remove_file(&path).unwrap();
    common::symlink(&elsewhere, &path);
    refused(
        &src,
        &tmp,
        "tracked file docs/compatibility.md is not a regular file",
    );
}

/// A tracked file under a folder replaced by a symlink, to a folder holding the same names, is
/// refused by name before anything is written: the file itself is regular, but the copy would
/// follow the symlink.
#[test]
fn assembly_refuses_a_tracked_file_under_a_symlinked_folder() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    let elsewhere = tmp.join("elsewhere docs");
    copy_tree(&src.join("docs"), &elsewhere);
    fs::remove_dir_all(src.join("docs")).unwrap();
    common::symlink(&elsewhere, &src.join("docs"));
    assert!(src.join("docs/compatibility.md").is_file());
    refused(
        &src,
        &tmp,
        "tracked file docs/compatibility.md is under a symlink, docs",
    );
}

/// Without git installed, the assembly says so, in one line and before writing anything,
/// rather than calling the sources not a checkout.
#[test]
fn assembly_without_git_says_so() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp);
    // A PATH with what the script runs before its checkout check, and no git.
    let tools = tmp.join("tools");
    fs::create_dir(&tools).unwrap();
    for tool in ["jq", "awk", "dirname"] {
        let real = common::which(tool).unwrap_or_else(|| panic!("no {tool} on PATH"));
        common::symlink(&real, &tools.join(tool));
    }
    let bins = tmp.join("bins");
    stub_binaries(&bins);
    let out = tmp.join("dist");
    let ran = Command::new("/bin/sh")
        .arg(src.join("scripts/assemble-dist.sh"))
        .args([VERSION, path_str(&out), path_str(&bins)])
        .env("PATH", &tools)
        .current_dir(&src)
        .stdin(Stdio::null())
        .output()
        .expect("running assemble-dist.sh");
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert_eq!(ran.status.code(), Some(1), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("needs git"), "{stderr}");
    assert!(!out.exists(), "wrote {}", out.display());
}

/// Sources that aren't a git checkout of their own are refused before anything is written:
/// a plain copy, and a plain copy inside an unrelated repository.
#[test]
fn assembly_refuses_outside_a_git_checkout() {
    let tmp = TempRoot::new("leadhand dist");
    let plain = tmp.join("plain");
    let src = plain.join("src");
    copy_sources(&src);
    refused(&src, &plain, "needs a git checkout");

    let inside = tmp.join("inside");
    let outer = common::git::make_repo_named(&inside, "outer", "main");
    let src = outer.join("src");
    copy_sources(&src);
    refused(&src, &inside, "needs a git checkout");
}

/// Review Focus 2: a linked worktree, whose `.git` is a file, assembles normally.
#[test]
fn assembly_works_from_a_linked_worktree() {
    let tmp = TempRoot::new("leadhand dist");
    let src = source_copy(&tmp.join("repo"));
    let worktree = tmp.join("worktree");
    common::git::run_git(
        &src,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            path_str(&worktree),
        ],
    );
    assert!(worktree.join(".git").is_file());
    let out = assembles(&worktree, &tmp);
    assert!(out.join("docs/compatibility.md").is_file());
}

/// Review Focus 1: a checkout of the dist branch, whose top-level `.git` holds empty folders,
/// passes `--check`.
#[test]
fn check_accepts_a_dist_checkout_with_its_git_folder() {
    let tmp = TempRoot::new("leadhand dist");
    let good = assembled(&tmp);
    common::git::run_git(&good, &["init", "--quiet"]);
    assert!(
        common::walk(&good.join(".git"))
            .iter()
            .any(|p| p.is_dir() && fs::read_dir(p).unwrap().next().is_none()),
        "git init left no empty folder in .git, so --check has nothing to skip"
    );
    let ran = check(&good);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
}

/// Only plain X.Y.Z versions are published: a source copy at any other version, in all four
/// places, is refused before anything is written.
#[test]
fn assemble_refuses_pre_release_versions() {
    let tmp = TempRoot::new("leadhand dist");
    for (n, version) in ["0.3.0-rc1", "1.02.3", "1.2", "1.2.3.4", "01.2.3"]
        .into_iter()
        .enumerate()
    {
        let case = tmp.join(format!("case-{n}"));
        let src = source_copy(&case);
        for file in VERSIONED {
            set_version(&src, file, version);
        }
        let (ran, out) = assemble_with(&src.join("scripts/assemble-dist.sh"), &case, version);
        assert_eq!(ran.code, Some(1), "{version}: {}", ran.stderr);
        assert!(
            ran.stderr.contains("plain X.Y.Z"),
            "{version}: {}",
            ran.stderr
        );
        assert!(!out.exists(), "{version}: wrote {}", out.display());
    }
}

/// Review Focus 4: a version that disagrees with `Cargo.toml`, either `plugin.json` or the
/// marketplace's plugin entry is refused before anything is written.
#[test]
fn assemble_refuses_tag_version_mismatch() {
    let tmp = TempRoot::new("leadhand dist");
    // Another version, and near misses of the package's own.
    let short = VERSION.rsplit_once('.').expect("a three-part version").0;
    for version in [
        OTHER_VERSION,
        &format!("v{VERSION}"),
        short,
        &format!("{VERSION} "),
        "",
    ] {
        let (ran, out) = assemble_with(&script(), &tmp, version);
        assert_eq!(ran.code, Some(1), "{version:?}: {}", ran.stderr);
        assert!(
            ran.stderr.contains("version"),
            "{version:?}: {}",
            ran.stderr
        );
        assert!(!out.exists(), "{version:?}: wrote {}", out.display());
    }

    // The package itself at another version: each of the four places in turn.
    for (n, file) in VERSIONED.iter().enumerate() {
        let case = tmp.join(format!("case-{n}"));
        let src = source_copy(&case);
        // Unchanged, the copy assembles: the refusal below is the version's doing.
        let (ran, _) = assemble_with(&src.join("scripts/assemble-dist.sh"), &case, VERSION);
        assert_eq!(ran.code, Some(0), "{file}, unchanged: {}", ran.stderr);
        fs::remove_dir_all(case.join("dist")).unwrap();
        set_version(&src, file, OTHER_VERSION);
        for version in [VERSION, OTHER_VERSION] {
            let (ran, out) = assemble_with(&src.join("scripts/assemble-dist.sh"), &case, version);
            assert_eq!(
                ran.code,
                Some(1),
                "{file} at {OTHER_VERSION}, {version}: {}",
                ran.stderr
            );
            assert!(!out.exists(), "{file}, {version}: wrote {}", out.display());
            // Every place that disagrees with the version is named, and no other: at VERSION
            // the spoiled file, at OTHER_VERSION the other three.
            for other in VERSIONED {
                let disagrees = (other == *file) == (version == VERSION);
                assert_eq!(
                    ran.stderr.contains(&format!("{other}'s version")),
                    disagrees,
                    "{file} at {OTHER_VERSION}, {version}, naming {other}: {}",
                    ran.stderr
                );
            }
        }
    }

    // A missing binary is refused before anything is written, too.
    let case = tmp.join("missing binary");
    stub_binaries(&case.join("bins"));
    fs::remove_file(case.join("bins/linux-arm64/leadhand")).unwrap();
    let (ran, out) = assemble_with(&script(), &case, VERSION);
    assert_eq!(ran.code, Some(1), "{}", ran.stderr);
    assert!(
        ran.stderr.contains("linux-arm64/leadhand"),
        "{}",
        ran.stderr
    );
    assert!(!out.exists());
}

/// In the `dist` tree, the marketplace's plugin source is `"./"`, so the tree installs on its
/// own; on `main` it points at the `dist` branch. Nothing else in the marketplace changes, and
/// `--check` refuses any other source.
#[test]
fn assembled_marketplace_points_at_its_own_root() {
    let mut main = json(&root().join(".claude-plugin/marketplace.json"));
    assert_eq!(
        leadhand_entry(&mut main)["source"],
        serde_json::json!({"source": "github", "repo": "owenthereal/leadhand", "ref": "dist"})
    );

    let tmp = TempRoot::new("leadhand dist");
    let out = assembled(&tmp);
    let path = out.join(".claude-plugin/marketplace.json");
    let mut dist = json(&path);
    assert_eq!(leadhand_entry(&mut dist)["source"], "./");
    leadhand_entry(&mut main)["source"] = "./".into();
    assert_eq!(dist, main);

    let ran = check(&out);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    for source in [
        serde_json::json!({"source": "github", "repo": "owenthereal/leadhand", "ref": "dist"}),
        serde_json::json!("."),
    ] {
        leadhand_entry(&mut dist)["source"] = source.clone();
        write_json(&path, &dist);
        let ran = check(&out);
        assert_eq!(ran.code, Some(1), "{source}: {}", ran.stderr);
        assert!(ran.stderr.contains("source"), "{source}: {}", ran.stderr);
    }
}

// --------------------------------------------------------------------------
// The release workflow, read as text.

fn workflow() -> String {
    read(&root().join(".github/workflows/release.yml"))
}

/// The lines of `text` that aren't blank or comments.
fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The workflow's jobs, by name: each `  <name>:` line under `jobs:`, with the lines below it.
fn jobs(text: &str) -> BTreeMap<String, Vec<&str>> {
    let lines = code_lines(text);
    let start = lines
        .iter()
        .position(|l| *l == "jobs:")
        .expect("a jobs: block");
    let mut out: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut current = None;
    for line in &lines[start + 1..] {
        match indent(line) {
            0 => break,
            2 => {
                let name = line
                    .trim()
                    .strip_suffix(':')
                    .expect("a job name")
                    .to_string();
                assert!(!out.contains_key(&name), "job {name} twice");
                out.insert(name.clone(), Vec::new());
                current = Some(name);
            }
            _ => out.get_mut(current.as_ref().unwrap()).unwrap().push(line),
        }
    }
    out
}

/// The lines of the block opened by the key line `<key>:` at `at` spaces in `lines`, trimmed.
fn block<'a>(lines: &[&'a str], at: usize, key: &str) -> Option<Vec<&'a str>> {
    let open = format!("{key}:");
    let i = lines
        .iter()
        .position(|l| indent(l) == at && l.trim() == open)?;
    Some(
        lines[i + 1..]
            .iter()
            .take_while(|l| indent(l) > at)
            .map(|l| l.trim())
            .collect(),
    )
}

/// The value of the key line `<key>: <value>` at `at` spaces in `lines`.
fn value<'a>(lines: &[&'a str], at: usize, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    lines
        .iter()
        .find(|l| indent(l) == at && l.trim_start().starts_with(&prefix))
        .map(|l| &l.trim_start()[prefix.len()..])
}

/// Only a version tag publishes: the `publish` job's `if:` is exactly the tag condition, it
/// alone has `contents: write` (and nothing else), it needs the tests and the assembly, and
/// it alone pushes. Every other job gets the workflow's `contents: read`.
#[test]
fn publish_job_runs_only_for_version_tags() {
    let text = workflow();
    let lines = code_lines(&text);
    assert_eq!(
        block(&lines, 0, "permissions"),
        Some(vec!["contents: read"])
    );

    let jobs = jobs(&text);
    assert!(
        ["test", "build", "assemble", "publish"]
            .iter()
            .all(|j| jobs.contains_key(*j)),
        "jobs: {:?}",
        jobs.keys().collect::<Vec<_>>()
    );
    let publish = &jobs["publish"];
    assert_eq!(
        value(publish, 4, "if"),
        Some("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')")
    );
    assert_eq!(
        block(publish, 4, "permissions"),
        Some(vec!["contents: write"])
    );
    let needs = value(publish, 4, "needs").expect("publish needs other jobs");
    for job in ["test", "assemble"] {
        assert!(needs.contains(job), "publish needs {needs}");
    }
    assert!(publish.iter().any(|l| l.contains("git push")));

    for (name, lines) in &jobs {
        if name == "publish" {
            continue;
        }
        // No job widens its token: none sets permissions, so all keep `contents: read`.
        assert_eq!(block(lines, 4, "permissions"), None, "{name}");
        assert!(
            !lines.iter().any(|l| l.contains("git push")),
            "{name} pushes"
        );
    }
    // The only write grant anywhere is the publish job's.
    let writes: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains(": write") || l.contains("write-all"))
        .collect();
    assert_eq!(writes, [&"      contents: write"]);
}

/// The publish job checks the tag before anything else (spec task 5): its checkout fetches the
/// whole history and keeps its token, the main check comes right after it, before the download
/// and the push, and the job's concurrency group queues publishes instead of cancelling one.
#[test]
fn publish_job_checks_main_first_and_queues() {
    let text = workflow();
    let jobs = jobs(&text);
    let publish = &jobs["publish"];
    let steps = steps(publish);
    let at = |what: &str| {
        steps
            .iter()
            .position(|s| s.iter().any(|l| l.contains(what)))
            .unwrap_or_else(|| panic!("no publish step with {what}"))
    };

    assert!(
        steps[0][0].starts_with("- uses: actions/checkout@"),
        "{:?}",
        steps[0]
    );
    assert_eq!(steps[1][0], format!("- name: {MAIN_CHECK_STEP}"));
    assert!(
        steps[1].contains(&"REF_NAME: ${{ github.ref_name }}"),
        "{:?}",
        steps[1]
    );
    assert!(1 < at("uses: actions/download-artifact@"));
    assert!(1 < at("git push"));

    assert_eq!(steps[0][1..], ["with:", "fetch-depth: 0"]);
    assert_eq!(
        block(publish, 4, "concurrency"),
        Some(vec![
            "group: release-dist",
            "cancel-in-progress: false",
            "queue: max"
        ])
    );
}

/// The workflow's other safety nets: every action is pinned to a commit SHA; the assembly is
/// checked with `--check` where it's built and again before it's published; and the tree
/// travels as one tar archive, which keeps the executable bits (ruling S3-R16).
#[test]
fn release_workflow_pins_actions_and_checks_the_tree() {
    let text = workflow();
    let uses: Vec<&str> = code_lines(&text)
        .into_iter()
        .filter_map(|l| {
            l.trim_start()
                .trim_start_matches("- ")
                .strip_prefix("uses: ")
        })
        .collect();
    assert!(uses.len() >= 5, "{uses:?}");
    for u in &uses {
        let (action, rest) = u.split_once('@').unwrap_or_else(|| panic!("unpinned: {u}"));
        let (sha, comment) = rest.split_once(" # v").unwrap_or((rest, ""));
        assert!(action.starts_with("actions/"), "{u}");
        assert!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{u}: not a commit SHA"
        );
        assert!(!comment.is_empty(), "{u}: no version comment");
    }

    let jobs = jobs(&text);
    for job in ["assemble", "publish"] {
        assert!(
            jobs[job]
                .iter()
                .any(|l| l.contains("scripts/assemble-dist.sh --check")),
            "{job} doesn't run --check"
        );
    }
    let assemble = jobs["assemble"].join("\n");
    assert!(assemble.contains("tar "), "assemble doesn't pack a tar");
    let publish = jobs["publish"].join("\n");
    assert!(publish.contains("tar "), "publish doesn't unpack the tar");

    // The release build: no test hooks, the lock file honoured, Linux on musl.
    let build = jobs["build"].join("\n");
    assert!(build.contains("cargo build --locked --release"));
    // The static-link check reads readelf's output whole, so a failing readelf fails the step,
    // and no pipe can end early and pass it vacuously.
    assert!(build.contains("headers=$(readelf --program-headers \"$bin\")"));
    assert!(
        !build
            .lines()
            .any(|l| l.contains("readelf") && l.contains('|'))
    );
    for target in [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ] {
        assert!(build.contains(target), "{target}");
    }
    // The tests are the post-cut-over pair (ruling S3-R18), after the release build that
    // rust_cli's release tests run (ruling S3-R35), on CI's three platforms (S3-R36).
    let test = jobs["test"].join("\n");
    assert!(test.contains("cargo test --locked --features testing"));
    assert!(test.contains("cargo test --locked --lib"));
    assert_eq!(
        cargo_runs(&jobs["test"]),
        [
            (None, "cargo build --locked --release".to_string()),
            (None, "cargo test --locked --features testing".to_string()),
            (None, "cargo test --locked --lib".to_string()),
        ]
    );
    assert_eq!(
        value(&jobs["test"], 8, "os"),
        Some("[macos-15, ubuntu-24.04, ubuntu-24.04-arm]")
    );
    // Every leg runs the full suite and the unit tests on the target it ships (S3-R36, and
    // follow-ups round 2: macOS too), after the release build, so rust_cli's release-build
    // tests check the binary the job uploads, and before it's uploaded (spec task 6).
    let release_build = "cargo build --locked --release --target \"$RUST_TARGET\"";
    assert_eq!(
        cargo_runs(&jobs["build"]),
        [
            (None, release_build.to_string()),
            (
                None,
                "cargo test --locked --features testing --target \"$RUST_TARGET\"".to_string()
            ),
            (
                None,
                "cargo test --locked --lib --target \"$RUST_TARGET\"".to_string()
            ),
        ]
    );
    let build_steps = steps(&jobs["build"]);
    let at = |what: &str| {
        build_steps
            .iter()
            .position(|s| s.iter().any(|l| l.contains(what)))
            .unwrap_or_else(|| panic!("no build step with {what}"))
    };
    let upload = at("uses: actions/upload-artifact@");
    assert!(at("cargo test --locked --features testing") < upload);
    assert!(at("cargo test --locked --lib") < upload);
    // The release build has no test hooks: its step builds without `--features`, and the
    // staged binary is that build's, which `cargo test` (the dev profile) leaves untouched.
    let release_step = &build_steps[at(release_build)];
    assert_eq!(release_step, &[format!("- run: {release_build}")]);
    assert!(!release_step[0].contains("--features"));
    // And no other cargo command, not even inside a `run: |` block, which `cargo_runs` can't
    // read: three lines name cargo, and one names a feature, the full suite's.
    let lines_with = |what: &str| jobs["build"].iter().filter(|l| l.contains(what)).count();
    assert_eq!(lines_with("cargo "), 3, "{build}");
    assert_eq!(lines_with("--features"), 1, "{build}");
    let stage = &build_steps[at("- name: Stage the binary")];
    assert!(
        stage.contains(
            &"cp \"target/$RUST_TARGET/release/leadhand\" \"$RUNNER_TEMP/bins/$TARGET/leadhand\""
        ),
        "{stage:?}"
    );
    // A test-only change gets every build leg's test run on its pull request.
    let paths = block(&code_lines(&text), 4, "paths").expect("the pull_request paths");
    assert!(paths.contains(&"- 'tests/**'"), "{paths:?}");
}

/// The steps of a job: the lines from each `- ` item under `steps:` to the next.
fn steps<'a>(job: &[&'a str]) -> Vec<Vec<&'a str>> {
    let lines = block(job, 4, "steps").expect("a steps: block");
    let mut out: Vec<Vec<&str>> = Vec::new();
    // `block` trims its lines; a step's first line is the one that opens a list item.
    for line in lines {
        if line.starts_with("- ") {
            out.push(vec![line]);
        } else {
            out.last_mut().expect("a step").push(line);
        }
    }
    out
}

/// Each step of a job whose `run:` names `cargo`, in order: its `if:` condition, and its
/// command. A step's own lines are trimmed, so its keys are read without their indentation.
fn cargo_runs(job: &[&str]) -> Vec<(Option<String>, String)> {
    steps(job)
        .into_iter()
        .filter_map(|step| {
            let key = |key: &str| {
                let prefix = format!("{key}: ");
                step.iter()
                    .map(|l| l.trim_start_matches("- "))
                    .find_map(|l| l.strip_prefix(&prefix))
                    .map(str::to_string)
            };
            let run = key("run")?;
            run.starts_with("cargo ").then(|| (key("if"), run))
        })
        .collect()
}

// --------------------------------------------------------------------------
// The publish step, run as the workflow's own text against temporary repositories.

/// The step that commits the tree onto dist.
const PUBLISH_STEP: &str = "Commit the tree onto dist and push it";

/// The `run: |` body of the step named `name`, its indentation removed, as bash gets it.
fn step_script(text: &str, name: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim() == format!("- name: {name}"))
        .unwrap_or_else(|| panic!("no step {name:?}"));
    let step = indent(lines[at]);
    let rest = &lines[at + 1..];
    let end = rest
        .iter()
        .position(|l| !l.trim().is_empty() && indent(l) <= step)
        .unwrap_or(rest.len());
    let own = &rest[..end];
    let run = own
        .iter()
        .position(|l| indent(l) == step + 2 && l.trim() == "run: |")
        .unwrap_or_else(|| panic!("step {name:?} has no `run: |`"));
    let body = step + 4;
    let mut out = String::new();
    for line in own[run + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || indent(l) >= body)
    {
        out.push_str(line.get(body..).unwrap_or(""));
        out.push('\n');
    }
    out
}

/// A checkout of the tagged sources with an `origin`, as the publish job has, and the step's
/// script.
struct Publisher {
    tmp: TempRoot,
    source: PathBuf,
    origin: PathBuf,
    script: String,
    runs: usize,
}

impl Publisher {
    fn new() -> Publisher {
        let tmp = TempRoot::new("leadhand publish");
        let source = common::git::make_repo(&tmp);
        let origin = common::git::make_origin(&source);
        let script = step_script(&workflow(), PUBLISH_STEP);
        assert!(
            script.contains("git push"),
            "{PUBLISH_STEP:?}'s body: {script}"
        );
        Publisher {
            tmp,
            source,
            origin,
            script,
            runs: 0,
        }
    }

    /// Runs the step for the tag `v<version>` on a copy of `tree`, as the job would after
    /// unpacking it into `$RUNNER_TEMP/dist`.
    fn publish(&mut self, tree: &Path, version: &str) -> (Option<i32>, String) {
        self.runs += 1;
        let runner = self.tmp.join(format!("runner {}", self.runs));
        copy_tree(tree, &runner.join("dist"));
        let bot = "github-actions[bot]";
        let email = "41898282+github-actions[bot]@users.noreply.github.com";
        let mut cmd = Command::new("bash");
        cmd.args(["--noprofile", "--norc", "-e", "-c", &self.script])
            .current_dir(&self.source)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.tmp.path())
            .envs(common::git::HERMETIC_ENV)
            .env("GITHUB_WORKSPACE", &self.source)
            .env("RUNNER_TEMP", &runner)
            .env("REF_NAME", format!("v{version}"))
            .env("GITHUB_SHA", "0123456789abcdef0123456789abcdef01234567")
            .env("GITHUB_SERVER_URL", "https://example.invalid")
            .env("GITHUB_REPOSITORY", "example/leadhand")
            .env("GITHUB_RUN_ID", "1")
            .envs([
                ("GIT_AUTHOR_NAME", bot),
                ("GIT_AUTHOR_EMAIL", email),
                ("GIT_COMMITTER_NAME", bot),
                ("GIT_COMMITTER_EMAIL", email),
            ]);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .expect("running the publish step");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    }

    /// dist's commits in the origin, newest first: (commit, parents, subject).
    fn dist(&self) -> Vec<(String, String, String)> {
        if !common::git::git_ok(
            &self.origin,
            &["rev-parse", "--verify", "--quiet", "refs/heads/dist"],
        ) {
            return Vec::new();
        }
        common::git::run_git(
            &self.origin,
            &["log", "--format=%H|%P|%s", "refs/heads/dist"],
        )
        .lines()
        .map(|l| {
            let mut parts = l.splitn(3, '|').map(str::to_string);
            (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            )
        })
        .collect()
    }

    /// Asserts the step refused: exit 1, one `::error::` line naming each of `named`, and dist
    /// left as it was.
    #[track_caller]
    fn refused(&mut self, tree: &Path, version: &str, named: &[&str]) {
        let before = self.dist();
        let (code, out) = self.publish(tree, version);
        assert_eq!(code, Some(1), "{version}: {out}");
        let errors: Vec<&str> = out.lines().filter(|l| l.starts_with("::error::")).collect();
        assert_eq!(errors.len(), 1, "{version}: {out}");
        for name in named {
            assert!(
                errors[0].contains(name),
                "{version}: {name:?} not in {}",
                errors[0]
            );
        }
        assert_eq!(self.dist(), before, "{version}: dist changed");
    }
}

/// The assembled tree at `version`, `<tmp>/trees/<name>`, from `base` (at `VERSION`), with
/// README.md changed when `changed`.
fn tree_at(tmp: &Path, base: &Path, name: &str, version: &str, changed: bool) -> PathBuf {
    let tree = tmp.join(format!("trees/{name}"));
    copy_tree(base, &tree);
    for file in &VERSIONED[1..] {
        set_version(&tree, file, version);
    }
    if changed {
        let readme = tree.join("README.md");
        fs::write(&readme, format!("{}\nchanged\n", read(&readme))).unwrap();
    }
    assert_eq!(check(&tree).code, Some(0), "{name}");
    tree
}

/// The publish step (ruling for Task 11 fix round 1): no dist yet starts it as an orphan; a
/// higher version adds one commit on top; the same version with the same tree is a no-op; the
/// same version with another tree, a lower version (by version order, not text: 900.2.9 is lower
/// than 900.2.10), and a dist head with no readable version are refused, naming both versions,
/// and leave dist as it was.
#[test]
fn publish_step_adds_one_commit_per_higher_release_and_never_rolls_back() {
    let trees = TempRoot::new("leadhand dist");
    let base = assembled(&trees);
    let v2_0 = tree_at(&trees, &base, "900.2.0", "900.2.0", false);
    let v2_0_rebuilt = tree_at(&trees, &base, "900.2.0 rebuilt", "900.2.0", true);
    let v2_1 = tree_at(&trees, &base, "900.2.1", "900.2.1", false);
    let v1_9 = tree_at(&trees, &base, "900.1.9", "900.1.9", false);
    let v2_10 = tree_at(&trees, &base, "900.2.10", "900.2.10", false);
    let v2_9 = tree_at(&trees, &base, "900.2.9", "900.2.9", false);

    let mut p = Publisher::new();
    // No dist yet: an orphan commit.
    let (code, out) = p.publish(&v2_0, "900.2.0");
    assert_eq!(code, Some(0), "{out}");
    let dist = p.dist();
    assert_eq!(dist.len(), 1, "{dist:?}");
    assert_eq!(
        (dist[0].1.as_str(), dist[0].2.as_str()),
        ("", "leadhand 900.2.0")
    );
    // The commit's tree is the assembled tree, executable bits included.
    let modes = common::git::run_git(&p.origin, &["ls-tree", "-r", "refs/heads/dist", "bin"]);
    for path in TARGETS
        .map(|t| format!("bin/{t}/leadhand"))
        .into_iter()
        .chain(["bin/leadhand".to_string()])
    {
        assert!(
            modes
                .lines()
                .any(|l| l.starts_with("100755 ") && l.ends_with(&format!("\t{path}"))),
            "{path} is not 100755: {modes}"
        );
    }

    // The same version with the same tree: a re-run, which succeeds and adds nothing.
    let (code, out) = p.publish(&v2_0, "900.2.0");
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(p.dist(), dist);

    // The same version with another tree: refused.
    p.refused(&v2_0_rebuilt, "900.2.0", &["900.2.0"]);

    // A higher version: one commit on top.
    let (code, out) = p.publish(&v2_1, "900.2.1");
    assert_eq!(code, Some(0), "{out}");
    let dist = p.dist();
    assert_eq!(dist.len(), 2, "{dist:?}");
    assert_eq!(dist[0].1, dist[1].0, "900.2.1's parent is 900.2.0");
    assert_eq!(dist[0].2, "leadhand 900.2.1");

    // A lower version: refused, naming both.
    p.refused(&v1_9, "900.1.9", &["900.1.9", "900.2.1"]);
    p.refused(&v2_0, "900.2.0", &["900.2.0", "900.2.1"]);

    // Version order, not text order.
    let (code, out) = p.publish(&v2_10, "900.2.10");
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(p.dist().len(), 3);
    p.refused(&v2_9, "900.2.9", &["900.2.9", "900.2.10"]);

    // A dist head with no readable version: refused rather than published.
    let mut p = Publisher::new();
    let stray = common::git::run_git(
        &p.source,
        &["commit-tree", "HEAD^{tree}", "-m", "not a release"],
    );
    common::git::run_git(
        &p.source,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("{stray}:refs/heads/dist"),
        ],
    );
    p.refused(&v2_1, "900.2.1", &["900.2.1", "version"]);
    assert_eq!(p.dist().len(), 1);
}

/// The step that refuses a tag on a commit that isn't on main.
const MAIN_CHECK_STEP: &str = "Check the tagged commit is on main";

/// The main check (spec task 5), run as the workflow's own text: a tagged commit on main passes,
/// at main's tip and further back; the tip of a side branch that forked from main is refused,
/// with one `::error::` line naming the tag, the commit and main, and the origin is untouched.
/// Each case runs as the publish job's checkout is: detached at the tagged commit, with no local
/// branch, only origin's, so neither `HEAD` nor a bare `main` could stand in for origin's main.
#[test]
fn publish_refuses_a_tag_off_main() {
    let script = step_script(&workflow(), MAIN_CHECK_STEP);
    let tmp = TempRoot::new("leadhand main check");
    let source = common::git::make_repo(&tmp);
    let c1 = common::git::run_git(&source, &["rev-parse", "HEAD"]);
    let c2 = common::git::commit(&source, &[("main.txt", "c2\n")], "c2");
    common::git::run_git(&source, &["switch", "--quiet", "--create", "side", &c1]);
    let s1 = common::git::commit(&source, &[("side.txt", "s1\n")], "s1");
    common::git::run_git(&source, &["switch", "--quiet", "main"]);
    let origin = common::git::make_origin(&source);
    common::git::run_git(&source, &["fetch", "--quiet", "origin"]);
    common::git::run_git(&source, &["checkout", "--quiet", "--detach"]);
    common::git::run_git(&source, &["branch", "--quiet", "-D", "main", "side"]);
    assert_eq!(
        common::git::run_git(&source, &["for-each-ref", "refs/heads"]),
        ""
    );
    let refs = || common::git::run_git(&origin, &["for-each-ref"]);
    let before = refs();
    assert!(before.contains("refs/heads/side"), "{before}");

    let tag = "v900.2.0";
    let check = |sha: &str| {
        common::git::run_git(&source, &["checkout", "--quiet", "--detach", sha]);
        let out = Command::new("bash")
            .args(["--noprofile", "--norc", "-e", "-c", &script])
            .current_dir(&source)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", tmp.path())
            .envs(common::git::HERMETIC_ENV)
            .env("GITHUB_WORKSPACE", &source)
            .env("REF_NAME", tag)
            .env("GITHUB_SHA", sha)
            .stdin(Stdio::null())
            .output()
            .expect("running the main check");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    };

    for (what, sha) in [("main's tip", &c2), ("main's first commit", &c1)] {
        let (code, out) = check(sha);
        assert_eq!(code, Some(0), "{what}: {out}");
        assert!(!out.contains("::error::"), "{what}: {out}");
    }

    let (code, out) = check(&s1);
    assert_eq!(code, Some(1), "side's tip: {out}");
    let errors: Vec<&str> = out.lines().filter(|l| l.starts_with("::error::")).collect();
    assert_eq!(errors.len(), 1, "side's tip: {out}");
    for name in [tag, s1.as_str(), "main"] {
        assert!(errors[0].contains(name), "{name:?} not in {}", errors[0]);
    }
    assert_eq!(refs(), before, "the origin changed");
}
