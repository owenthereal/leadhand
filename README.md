# leadhand

Give leadhand an approved plan. It carries the work through implementation and review with fresh, bounded workers, keeps model usage visible and under control, and returns a checked branch with clear evidence, plus any decisions that need you.

## Requirements

In v1 the run's supervisor is a Claude Code session. Workers can run on Claude or Codex. The plugin installs in both Claude Code and Codex so a deck can be prepared in either, but starting a run needs Claude Code.

The CLI is a single binary, for macOS (Apple silicon) and Linux (x86_64 and arm64; not yet observed in a live run, see `docs/compatibility.md`). Besides Claude Code (and Codex, for Codex workers), it needs only Git: no Python or other runtime.

## Install

Install the plugin from its marketplace in Claude Code:

```
/plugin marketplace add owenthereal/leadhand
/plugin install leadhand --marketplace owenthereal/leadhand
```

The plugin's `bin/leadhand` runs the leadhand binary; Claude Code puts it on the Bash tool's PATH.

## Status

Status: under construction. For attended runs on fixture and trial repos only until the run guard lands.

Not supported in this version (`leadhand check` reports an error and `leadhand start` refuses):

- A `docs_dir` inside the repository (a relative one counts from the repository): run files must live outside it.

## Development

leadhand is written in Rust; the toolchain version is in `Cargo.toml` (`rust-version`).

```
cargo build --locked --release
cargo test --locked --features testing
cargo test --locked --lib
```

Build the release binary before the tests: some of them run the release build beside the testing build (`target/release/leadhand`, or `target/<triple>/release/leadhand` under `--target`, in whatever target folder Cargo uses), built from this checkout, and refuse one older than its sources.

- `cargo build --locked --release` builds `target/release/leadhand`, which `bin/leadhand` runs in a source checkout (`claude --plugin-dir <checkout>`).
- `cargo test --locked --features testing` runs the unit tests and the contract tests (`tests/*.rs`), against a build with the test hooks compiled in.
- `cargo test --locked --lib` runs the unit tests without the hooks, which checks that a release build has none.

The live tests run the real `claude` and `codex` against a fixture repository, and cost money. They run only when asked for, from a terminal outside Claude Code:

```
LEADHAND_BIN="$PWD/target/release/leadhand" LEADHAND_LIVE_OUT=<folder> cargo test --locked --features testing --test live -- --ignored --test-threads 1
```

`tests/live.rs` lists what they need and every setting they take.

The CLI's contract (its commands, files and outputs) is `docs/cli-contract.md`; the deck format is `docs/deck-format.md`; what was observed of Claude Code and Codex is `docs/compatibility.md`.
