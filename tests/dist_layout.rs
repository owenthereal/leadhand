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

const VERSION: &str = "0.2.0";

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

    // Exactly the listed paths and the binaries, byte for byte, except the marketplace's
    // rewritten source (assembled_marketplace_points_at_its_own_root).
    let mut want = BTreeMap::new();
    for path in PLUGIN_PATHS {
        files_under(&root(), path, &mut want);
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
        "a manifest that isn't JSON",
        ".codex-plugin/plugin.json",
        &|dir| fs::write(dir.join(".codex-plugin/plugin.json"), "{\"version\": ").unwrap(),
    );
    for manifest in &VERSIONED[1..] {
        case(
            &format!("{manifest} at another version"),
            manifest,
            &|dir| set_version(dir, manifest, "0.2.1"),
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

/// A copy of the package's sources the assembly reads (its script, `Cargo.toml` and the plugin
/// files) in `<tmp>/src`, for changing a version without touching the package.
fn source_copy(tmp: &Path) -> PathBuf {
    let src = tmp.join("src");
    for path in PLUGIN_PATHS
        .iter()
        .chain(&["Cargo.toml", "scripts/assemble-dist.sh"])
    {
        copy_tree(&root().join(path), &src.join(path));
    }
    src
}

/// Review Focus 4: a version that disagrees with `Cargo.toml`, either `plugin.json` or the
/// marketplace's plugin entry is refused before anything is written.
#[test]
fn assemble_refuses_tag_version_mismatch() {
    let tmp = TempRoot::new("leadhand dist");
    for version in ["0.2.1", "v0.2.0", "0.2", "0.2.0 ", ""] {
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
        set_version(&src, file, "0.2.1");
        for version in [VERSION, "0.2.1"] {
            let (ran, out) = assemble_with(&src.join("scripts/assemble-dist.sh"), &case, version);
            assert_eq!(
                ran.code,
                Some(1),
                "{file} at 0.2.1, {version}: {}",
                ran.stderr
            );
            assert!(!out.exists(), "{file}, {version}: wrote {}", out.display());
            // Every place that disagrees with the version is named, and no other: at 0.2.0
            // the spoiled file, at 0.2.1 the other three.
            for other in VERSIONED {
                let disagrees = (other == *file) == (version == VERSION);
                assert_eq!(
                    ran.stderr.contains(&format!("{other}'s version")),
                    disagrees,
                    "{file} at 0.2.1, {version}, naming {other}: {}",
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
    assert!(!build.contains("testing"));
    for target in [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ] {
        assert!(build.contains(target), "{target}");
    }
    // The tests are the post-cut-over pair (ruling S3-R18).
    let test = jobs["test"].join("\n");
    assert!(test.contains("cargo test --locked --features testing"));
    assert!(test.contains("cargo test --locked --lib"));
}
