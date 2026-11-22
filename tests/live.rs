//! The live end-to-end runs: the real `claude` and `codex` against a fixture repository,
//! through the leadhand binary. They print PASS, FAIL, OBSERVED and INCONCLUSIVE check
//! lines and write raw results; the Rust form of the Python live drivers they replaced (the
//! end-to-end driver, its fixture builder and the Codex push probe).
//!
//! Both tests are `#[ignore]`: they need network access, a logged-in `claude` and `codex`,
//! real money, and a Python 3 interpreter on PATH for the fixture project's own build and
//! tests (`tests/live/fixture.rs`), so they run only when asked for, from an ordinary
//! terminal outside Claude Code (they refuse to start when CLAUDECODE is set):
//!
//! ```text
//! LEADHAND_BIN="$PWD/target/release/leadhand" LEADHAND_LIVE_OUT=<folder> \
//!     cargo test --locked --features testing --test live -- --ignored --test-threads 1
//! ```
//!
//! No `--nocapture` is needed: the drivers write their progress, section, check and summary
//! lines straight to the process's stdout, which libtest does not capture (ruling S3-R28).
//!
//! - `live_complete_run`: the Codex push probe first, then one complete run (the Python
//!   driver without `--interrupt`).
//! - `live_interrupted_run`: the interrupted run (the Python driver's `--interrupt`): card 2's
//!   Codex worker and then the supervisor are killed, the run is relaunched, and the recovery
//!   checks are added. It never runs the push probe.
//!
//! A test fails when a check's verdict is FAIL, when the push probe has a FAIL or REFUSED
//! section, or when no check ran; OBSERVED and INCONCLUSIVE are findings, not failures (the
//! heredoc-record open check is INCONCLUSIVE whenever no heredoc `record` call was made). It
//! fails only after writing everything.
//!
//! The settings (the Python drivers' command-line options):
//!
//! - `LEADHAND_LIVE_OUT` (required; `--out`): the folder for the results. The complete run
//!   writes under `$LEADHAND_LIVE_OUT/complete/` (`e2e-raw.md`, `e2e-artifacts/`, and the push
//!   probe's `push-probe-raw.md` and `push-probe-artifacts/`), the interrupted run under
//!   `$LEADHAND_LIVE_OUT/interrupt/` (ruling S3-R15). A test fails at once when it is unset.
//! - `LEADHAND_BIN`: the binary the driver starts (default: the testing build). A relative
//!   path is made absolute against the package root.
//! - `LEADHAND_LIVE_MODE` (`--mode`): `headless` (the default) or `tmux`.
//! - `LEADHAND_LIVE_BUDGET_MINUTES` (`--budget-minutes`): the run's budget, at least 1;
//!   default 45.
//! - `LEADHAND_LIVE_SKIP_PUSH_PROBE` (`--skip-push-probe`): `1` skips the push probe in the
//!   complete run.
//! - `LEADHAND_LIVE_KEEP` (`--keep`): `1` keeps the temp fixture folder. As in the Python
//!   driver, the push probe's own temp folder is removed either way.
//! - `LEADHAND_LIVE_DRY_RUN` (`--dry-run`): `1` prints each test's plan and starts nothing. A
//!   dry run works inside Claude Code too, as the Python's did (the refusal comes after it).
//!
//! The yes/no settings take `1`/`0` (or `true`/`false`, `yes`/`no`); unset is no. These
//! `LEADHAND_LIVE_*` settings are left out of the environment leadhand and its workers see.
//! A first Ctrl-C stops the run cleanly (sessions and runners stopped, results written, as
//! the Python driver's KeyboardInterrupt handling does); a second one ends it at once.
//!
//! The CLI path (ruling S3-R14): the driver starts LEADHAND_BIN, but checks the supervisor's
//! Bash calls against the CLI path the launch reports (the first message's "leadhand CLI:"
//! path, else `launch-line`'s first word), and records both in `e2e-raw.md`. When
//! LEADHAND_BIN is not that path (the `bin/leadhand` shim), it adds the check "launch CLI
//! path is the resolved platform binary".

#[macro_use]
mod common;

#[macro_use]
#[path = "live/push_probe.rs"]
mod push_probe;

#[path = "live/fixture.rs"]
mod fixture;

#[path = "live/drive.rs"]
mod drive;

#[test]
#[ignore = "live: runs the real claude and codex; see this file's header"]
fn live_complete_run() {
    drive::main("live_complete_run", false);
}

#[test]
#[ignore = "live: runs the real claude and codex; see this file's header"]
fn live_interrupted_run() {
    drive::main("live_interrupted_run", true);
}
