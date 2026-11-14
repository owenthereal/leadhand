# The leadhand CLI contract

This is the written index of everything outside the leadhand binary that another program
depends on: the plugin's skills and agents, the Codex proxy, the detached runners, the
operator's terminal, and any other leadhand binary sharing the same run directory.

- **The executable truth is the contract suite**, `tests/contract/`. It drives any binary as a
  subprocess at `LEADHAND_BIN`.
- **The reference is the Python CLI**, `scripts/leadhand` with `scripts/lib/leadhand/`. Where
  this document and the Python disagree, the Python is right and this document is a bug.
- **`tests/contract/CATALOG.md`** maps every Python unit test to the contract test that converts
  it, or to `internal: Rust unit test (<module>)` when it has no effect outside the binary.

The sections follow the thirteen contract items. Where a ruling settled a question, its
number (`S1-R…`) is given. Paths are written generically: `<run>` is a run directory, `<ws>`
the run's worktree, `<repo>` the repository's top level, `<docs_dir>` the configured docs
folder.

## Conventions used throughout

- **JSON files** (`run.json`, `config.json`, `holder.json`, the worker folder's `*.json`) are
  written atomically: a temporary file in the same folder, `fsync`, then `rename`. They are
  indented by 2 and ASCII-only (see "Non-UTF-8 paths").
- **JSON-lines files** (`events.jsonl`, `attempts.jsonl`, `charges.jsonl`) hold one compact
  JSON object per line (`separators=(",", ":")`), ASCII-only, appended under `state.lock`.
  - A record is a line. A last line without its newline is an interrupted append: readers skip
    it, even when its JSON is whole, and the next append drops it before writing.
  - A complete line that is not JSON makes the reader fail (`next` exits 1); `status` shows it
    as an attention line instead, and `start` skips that run with a warning.
- **Timestamps** are UTC ISO 8601 with a `Z` suffix: `YYYY-MM-DDTHH:MM:SS.mmmZ` in records,
  `YYYY-MM-DDTHH:MM:SSZ` for a dispatch deadline.
- **Process identity** is a pid plus its start time. A start time is an opaque string, compared
  only for equality with a value made the same way on the same machine:
  - macOS: `<seconds>.<microseconds, 6 digits>` from `sysctl` (`KERN_PROC_PID`);
  - Linux: `ticks:<start time in clock ticks>` from `/proc/<pid>/stat`;
  - elsewhere, or when those fail: `lstart:<ps lstart text>`.
- **The binary's own path.** Python names itself `scripts/leadhand` by its fully resolved
  absolute path. Under `LEADHAND_TESTING=1`, `LEADHAND_TEST_CLI` takes its place everywhere the
  binary names itself. Tests compare that path by resolved file, never as a string (S1-R11).
  This covers:
  - the launch line, the first message and the sandbox exclusion;
  - a dispatch line's argv[0] and `LEADHAND_CLI`;
  - unpark hints, and the `run` command named by `pause` and `cancel`.

## 1. Commands, flags and positional arguments

There are seventeen subcommands. Every form that works today keeps working, except where
"Deliberate changes" says otherwise. A flag value of `-` reads standard input where noted.

| Command | Arguments and flags |
|---|---|
| `version` | none |
| `check` | `DECK`, `--repo REPO` (default `.`) |
| `route` | `DECK CARD ROLE`, `--attempt N` (default 1), `--last-vendor {claude,codex}`, `--repo REPO` (default `.`), hidden `--installed LIST` |
| `start` | `DECK`, `--repo REPO` (default `.`), `--new` |
| `next` | `RUN_DIR` |
| `record` | `RUN_DIR KIND`, where `KIND` is one of `started`, `result`, `ruling`, `park` or `diff-check`; then `--dispatch ID`, `--worker ID`, either `--reply TEXT` (`-` reads stdin) or `--reply-file PATH`, `--stopped`, `--card N`, `--text TEXT` (`-` reads stdin), `--reason TEXT`, `--ok {yes,no}`, `--note TEXT` |
| `timer` | `RUN_DIR`, `--dispatch ID` (required) |
| `pause` | `RUN_DIR` |
| `cancel` | `RUN_DIR` |
| `unpark` | `RUN_DIR`, `--card N` (required), `--note TEXT` (required; `-` reads stdin) |
| `status` | `[RUN_DIR]` |
| `report` | `RUN_DIR` |
| `run` | `RUN_DIR` (the run directory or its `deck.md`), `--plugin-dir DIR`, hidden `--print` |
| `launch-line` | `RUN_DIR` (the run directory or its `deck.md`), `--plugin-dir DIR` |
| `dispatch` | `--run RUN_DIR`, `--dispatch-id ID`, `--attempt N`, `--role ROLE`, `--model MODEL`, `--effort EFFORT`, `--brief PATH`, `--report PATH` and `--cwd DIR`, all required; then `--card N`, `--resume THREAD_ID`, `--kind KIND` and `--branch BRANCH` |
| `stop-dispatch` | `RUN_DIR`, `--dispatch ID` (required) |
| `dispatch-runner` (hidden) | `WORKER_DIR` |

What each `record` kind needs:

| Kind | Flags |
|---|---|
| `started` | `--dispatch`, `--worker` |
| `result` | `--dispatch`, `--worker`, and `--reply` or `--reply-file`; `--stopped` when the worker was stopped at its deadline |
| `ruling` | `--card`, `--text` |
| `park` | `--card`, `--reason` |
| `diff-check` | `--card`, `--ok`, optional `--note` |

`--reply-file` must be the dispatch's own reply file, `<ws>/.leadhand/<card or run>/<dispatch
id>/reply.txt` (the `reply_file` of the dispatch action). Any other path is refused (security
review L1).

Hidden forms that work today, and so are contract:

- **`route --installed LIST`** (S1-R9). A comma-separated vendor list (`claude`, `codex`) that
  replaces the `PATH` lookup of installed vendors. An unknown name is `error: --installed:
  unknown vendor '<name>'`. Python honours it whatever `LEADHAND_TESTING` says: unlike the
  `LEADHAND_*` test hooks, it is not gated.
- **`run --print`** appends `-p --output-format stream-json --verbose` to the `claude` argv,
  before the first message. It is for end-to-end drivers.
- **`dispatch-runner WORKER_DIR`** is the detached runner that `dispatch` spawns.

## 2. Output

### JSON that `next` and `record` print

Both print one line of ASCII JSON on stdout and exit 0.

**`next`** prints the supervisor's next action. Every action has an `action` field:

| `action` | Fields |
|---|---|
| `dispatch` | `dispatch_id`, `kind`, `card` (null for a run-level dispatch), `attempt`, `role`, `agent`, `agent_model`, `agent_effort`, `route` (`{vendor, model, effort}`), `message`, `brief`, `report`, `resume` (`{worker_id}` or null), `reply_file`, `deadline`; a run-level dispatch adds `gate` and `wave` |
| `wait` | `dispatch_id`, `worker_id`, `deadline`, `reply_file`, `reason` |
| `check_diff` | `card`, `dispatch_id`, `diff` (a `fix.diff` path in the card folder), `findings`, `paths` |
| `answer` | `card`, `dispatch_id`, `worker_id`, `report`, `question` |
| `stop_worker` | `dispatch_id`, `worker_id`, `vendor`, `record_result`; a stop for a cancel adds `reason` |
| `done` | `outcome`, `qualifier`, `reasons`, `report` (a path, or null), `cards` (a list of `{card, state, reason}`) |

- **The `agent` of a dispatch** is `leadhand:<role agent>` for Claude, or `leadhand:codex` for a
  Codex route.
- **The `message`.**
  - For a Codex route, it is exactly one shell-quoted command line, the dispatch line (see
    "The Codex command line").
  - For Claude, it is `Brief: <path>`, then `Report file: <path>`, then the instruction line.
- **The dispatch ID format** is `c<card>-a<attempt>-<kind>-<n>` for a card,
  `run-<kind>-<n>` for the run, and `run-w<wave>-<kind>-<n>` for a fix wave.
- **A card-loop-only `done`.** Under `LEADHAND_TEST_CARD_LOOP_ONLY=1`, which is testing only,
  `done` comes with `outcome` null once the card loop ends.

**`record`** prints the event it appended: `worker_started`, `result_recorded`, `ruling`,
`parked` or `diff_checked`, with the fields in "Run state". A repeated `record started` for the
same worker, or `record result` for a dispatch already recorded, prints the event already
there.

### Text lines that something reads

| Command | Output |
|---|---|
| `version` | `leadhand 0.1.0`. The string changes at the cut-over (deliberate change 6). |
| `check` | One problem per line on **stdout**, `error: <message>` or `warning: <message>`, then `ok: <n> cards` when there are no errors. Exit 1 when there is an error. A deck or configuration that does not load prints its single `error: …` line on stdout too. |
| `route` | The route line `<vendor> <model> <effort>`, then one line per reason. When the ladder is exhausted: `ladder exhausted: …` on stderr, exit 1. |
| `start` | For a new run, `run: <run dir>`; when another run holds the deck's branch, a note before it, `run branch: <branch> (<requested> belongs to another run)`. For an unfinished run of the same deck: `existing run: <run dir>`. Unsupported settings give `warning: <message>` lines on stderr. |
| `dispatch`, `stop-dispatch` | The worker's reply (its first 15 lines; with no reply, `Status: …`, maybe `Blocked by: …`, then `Stopped: <exit reason>`), then `session: <thread id or none>`. |
| `dispatch` exit 75 | `still running: run the same command again (dispatch <id>)`, printed after the foreground wait, `FOREGROUND_WAIT` = 540 s by default; `LEADHAND_TEST_FOREGROUND_WAIT` replaces it in tests (S1-R12). |
| `timer` | One line: `result recorded`, `deadline passed`, or `pause or cancel requested`. |
| `pause` | `pause requested: … Continue it later with: <cli> run <run dir>`, where `<cli>` and the run dir are shell-quoted. |
| `cancel` | `cancel requested: … If no supervisor is running, <cli> run <run dir> finishes the cancel.` |
| `unpark` | `card <n> unparked` |
| `report` | Only the report's path. The branch release changes report.md's content, not this output, so it stays exact parity (S1-R15). |
| `launch-line` | One line: `<cli> run <run>/deck.md`, plus `--plugin-dir <abs dir>` when one is given, or when the last launch recorded one in holder.json. Every word is shell-quoted. |
| `status` | The rendered status (headline, attention lines, card rows, usage). Without `RUN_DIR`, Python shows the most recently updated unfinished run under the current repository's `docs_dir`; deliberate change 3 replaces that. |
| `run` | On stderr, the attended-use warning naming the sandbox exclusion. When the CLI path contains whitespace, also `leadhand: warning: the leadhand CLI path contains whitespace (<cli>); the sandbox exclusion may not match; install leadhand under a path without spaces.` |

Other lines:

- **Errors.** `error: <message>` on stderr, with exit 1, is the error form for every command
  except `check` (stdout, above) and `dispatch-runner` (`leadhand: <message>` on stderr).
- **A failed `status.json` write.** `next` and `record` rewrite status.json at every
  checkpoint. When that fails, they print `leadhand: could not write status.json: <error>` on
  stderr and carry on. A contract test reads this line.
- **Briefs are contract output** (S1-R10). Their text lines carry the card's fields and the
  settings that shape the work, so a field or setting a brief renders is asserted there.
  Briefs are written to `<ws>/.leadhand/<card or run>/<dispatch id>/brief.md` and copied to the
  card folder.

## 3. Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. |
| 1 | An error, with `error: …` on stderr (`check` prints it on stdout). Also `check` finding an error in the deck, and `route` with an exhausted ladder. |
| 2 | A usage error, with a message on stderr. The wording may differ: Rust's comes from clap (deliberate change 4). Tests assert the code only. |
| 3 | `run` refused because the run is owned (the ownership message on stderr). Also a second `dispatch-runner` for a worker folder that already has a live runner (`leadhand: another runner owns <dir>`). |
| 75 | `dispatch` still running: run the same command again. Printed after the foreground wait: 540 s by default, `LEADHAND_TEST_FOREGROUND_WAIT` in tests. |
| 97 | A test crash point. Testing builds only (`LEADHAND_TESTING=1`). |

`run` otherwise returns the supervisor's own exit code, or 128 plus the signal number when the
supervisor was killed by a signal.

## 4. Files in the run directory

A run directory is `<docs_dir>/runs/<local date YYYY-MM-DD>-<slug>`. When that name is taken,
it gets the first free suffix: `-2`, `-3` and so on. The slug comes from the deck title:
lowercased, every run of characters other than `[a-z0-9]` becomes `-`, then it is trimmed of
`-`, cut to 40 characters and trimmed again (`run` when nothing is left).

| File | Owner (who writes) | Format |
|---|---|---|
| `deck.md` | `start` | The deck's bytes as they were when the run started. |
| `run.json` | `start` (then `next` adds supervisor sessions) | JSON: `run_id` (12 hex digits), `repo`, `deck_source`, `deck_digest` (sha256 hex), `base_ref`, `base_sha`, `branch`, `branch_requested`, `workspace` (absolute), `slug`, `created_at`, `supervisor_sessions` (a list of Claude session IDs). |
| `config.json` | `start` | JSON: the effective configuration, snapshotted, so the run never reads the live files again (S1-R8). See below. |
| `events.jsonl` | every command that changes state | JSON lines, the source of truth. See "Run state". |
| `status.json` | `next` and `record` (each checkpoint) | JSON: `run_id`, `deck`, `state`, `headline`, `activity`, `progress` (`{accepted, total}`), `attention` (a list of strings), `cards` (rows), `usage` (`{estimated_usd, unknown_attempts, by_role, codex_meter: {percent, observed_at}}`), `updated_at`. |
| `meter-start.json` | the first `status.json` write | JSON: `{percent, observed_at}`, the Codex meter before the run. |
| `attempts.jsonl` | `record result` (Claude workers and Codex proxies, `recorded_by: "engine"`); the runner (Codex workers) | JSON lines. Each line has `dispatch`, `run`, `card`, `attempt`, `role`, `vendor`, `model`, `effort`, `worker_id`, `started_at`, `ended_at`, `steps`, `charge_ids`, `usage`, `usage_known`, `cost_estimate`, `exit_reason`, `status` and `blocked_by`. Engine records add `agent` and `recorded_by`; runner records add `usage_source`, `generation` and `possibly_incomplete`. |
| `charges.jsonl` | `next`, `record result` and the runner | JSON lines: `charge_id`, `source` (`claude`, `codex-stream`, …), `key`, `dispatch_id` (`supervisor` for the supervisor's own session), `role`, `usage` (`{input, cached, cache_write_5m, cache_write_1h, output, reasoning, known, model}`), `checkpoint`, `ts`. |
| `state.lock` | every writer | An empty lock file (see "Locks"). |
| `untracked-at-start.json` | `start` | JSON: `{paths}`, the run worktree's untracked files when the run started. They are never swept or committed (R48'). |
| `report.md` | the end of the run, and `report` | Markdown. |
| `handoff.md` | a blocked end (pause and others) | Markdown. |
| `run.lock` | `run` | A lock file, held for the supervisor's life (see "Locks"). |
| `holder.json` | `run` | JSON: `run_id`, `launcher_pid`, `launcher_start`, `supervisor_pid`, `supervisor_start`, `started_at`, and `plugin_dir` when given. The two `*_start` fields use the process start-time format. |
| `pause`, `cancel` | `pause`, `cancel` | Flag files, JSON `{requested_at, by_pid}`. Their existence is the request. |
| `cards/<card>/<dispatch id>/` | `record result` | `brief.md`, `report.md` and `reply.md` (copies; a copy is never read through a symlink), plus `fix.diff` for a test-only fix's diff check. Run-level dispatches use `cards/run/<dispatch id>/`. |
| `workers/<dispatch id>/` | `dispatch` and the runner | The Codex worker folder. See item 5. |

`config.json` keys, values and types (S1-R8):

| Key | Type |
|---|---|
| `profile` | string |
| `docs_dir` | string |
| `run` | `{worktree: bool, test_first: string, evidence: string, card_review: string, cost_limit_usd: number, branch_prefix: string}` |
| `after_run` | `{step: string, on_incomplete: string, repeat_safe: bool, check: string}` |
| `capacity` | `{codex_pause_percent: int}` |
| `codex` | `{network: bool, writable_dirs: [string]}` |
| `limits` | `{<role>: {steps: int, minutes: int}}` |
| `roles` | `{<role>: {<vendor>: "<model> <effort>", …, prefer: "claude" or "codex" or "other"}}` |
| `notify` | `{command: string}` |
| `prices` | `{<model family>: {input, output, cache_read, cache_write_5m, cache_write_1h: number}}` |

Configuration sources, merged in this order: the preset, the user file, then the repository's
`<repo>/.leadhand.toml`.

- The user file is `$LEADHAND_CONFIG`, else `~/.config/leadhand/config.toml`.
- `{repo}` in `docs_dir` is replaced by the repository folder's name, then `~` is expanded. A
  relative `docs_dir` is resolved against the repository.

## 5. Files in each worker folder

`<run>/workers/<dispatch id>/` belongs to one Codex dispatch. `dispatch` creates it and writes
`request.json`; the detached runner writes the rest.

| File | Writer | Format |
|---|---|---|
| `request.json` | `dispatch` | JSON: `model`, `effort`, `role`, `brief`, `report`, `cwd`, `run_dir`, `card`, `attempt`, `dispatch_id`, `steps`, `minutes`, `network`, `writable_dirs`, `resume`, `kind`, `branch`. It never names an executable (R49). |
| `spawn.json` | `dispatch` | JSON `{pid, start, at}`: the runner it spawned. `start` uses the process start-time format; `at` is epoch seconds. |
| `runner.json` | runner | JSON: `runner` (`{pid, pgid, start}`), `codex` (`{pid, pgid, start}` or null), `generation`, `tag`, `started_wall`, `started_at`, and `launching` (`{at}`, plus `aborted: true` when the launch was abandoned). |
| `heartbeat` | runner | An empty file, touched every second (wall clock). It is stale after 60 s. |
| `procs.json` | runner | JSON `{descendants: [{pid, pgid, start}]}`, plus `tag_unreadable` (pids) when some environments could not be read. |
| `stream.jsonl` | Codex (`--json` stdout) | Codex's own events. |
| `codex.stderr` | Codex | Text. |
| `last-message.md` | Codex (`-o`) | The reply. |
| `reply.raw.md` | runner | The reply, kept when it has no parseable status line. |
| `untracked.json` | runner | JSON `{at, paths}`, plus `error`: the untracked files when the first runner launched Codex. They are never committed as the worker's (I3). |
| `commit.json` | runner | JSON `{at, branch, head_before}`: the commit intent. |
| `cancel` | `stop-dispatch`, `next` | JSON `{requested_at, by_pid, token, deadline_wall}`. Its existence is the request. |
| `stopping.json` | runner | JSON `{reason, at}`: the runner's acknowledgement. `reason` is a stop reason or `finished`. |
| `result.json` | runner (written last) | JSON. See below. |
| `owner.lock`, `runner.lock`, `finalize.lock` | `dispatch`, `stop-dispatch` and the runner | Lock files (see "Locks"). |
| `runner.out`, `runner.err` | runner | The runner's own output. |
| `gh-config/` | runner | The empty `GH_CONFIG_DIR` the worker gets. |
| `superseded/<n>/` | `dispatch` | The generation files (`spawn.json`, `runner.json`, `heartbeat`, `procs.json`, `stream.jsonl`, `codex.stderr`, `last-message.md`, `reply.raw.md`, `cancel`, `stopping.json`, `result.json`, `commit.json`, `runner.out` and `runner.err`) of generation `n`, which was cancelled and replaced. |

`result.json` holds:

- `dispatch_id`, `status`, `blocked_by`, `session_id`, `reply`, `steps`;
- `usage`, `usage_known`, `usage_source`, `possibly_incomplete`;
- `exit_reason`, one of `completed`, `step_limit`, `time_limit`, `usage_limit`, `cancelled`,
  `error` or `interrupted`;
- `codex_exit`, `charge_ids`, `generation`;
- `stopped`, the processes confirmed gone (`[{pid, pgid, start}]`);
- `confirmed_at`, `cancel_token`, `started_at`, `ended_at`;
- the commit outcome: `committed` (a sha or null), `changes`, and any of `commit_skipped`,
  `commit_error`, `gitlinks_skipped` (the gitlinks left out, R45), `worker_status` and
  `worker_exit_reason`.

### Timing that parity tests depend on

Contract tests wait on these, so a Rust binary keeps the same values. The foreground wait
(540 s) and the heartbeat (touched every second, stale after 60 s) are given above.

| Name in the Python | Value | What it times |
|---|---|---|
| `HUNG_RECHECK` | 5 s | A live runner whose heartbeat is stale is looked at again for this long, its heartbeat unchanged, before it counts as hung. |
| `SIGNAL_STEP` | 5 s | The stop ladder's spacing: SIGINT to Codex's group, SIGTERM 5 s later, SIGKILL 5 s after that. |
| `KILL_RETRY` | 5 s | After SIGKILL, it is sent again this often until the processes are confirmed gone. |
| `CANCEL_ACK` | 5 s | A runner that has not acknowledged a cancel (`stopping.json`) this long after the request is hung, and is killed. |
| `SPAWN_GRACE` | 60 s | A runner `dispatch` spawned that has not yet written `runner.json` counts as starting for this long after `spawn.json`'s `at`. |
| `CANCEL_FINISH_MARGIN` | 2 s | Kept back from a cancel's deadline for charging and writing the result. |
| `TIMER_INTERVAL` (engine) | 10 s | How often `timer` looks for a result, a pause or cancel flag, or a finished runner; it never sleeps past the worker's deadline. |

## 6. Lines in `<docs_dir>/runs.md`

`runs.md` has one line per run. It is written when the run ends, and replaced in place when the
run ends again (a resumed blocked run):

```
| <local date YYYY-MM-DD> | <deck title> | <outcome>[ (<qualifier>)] | <accepted>/<total> cards | $<usd, 2 places> or unknown | <run dir> |
```

- **A new file** starts with `# Runs`, a blank line, the header
  `| Date | Deck | Outcome | Cards | Estimated cost | Run |` and its separator line.
- **Cells.** Whitespace in a cell is collapsed to single spaces and `|` is written `\|`. A
  run's line is found by its last cell.
- **Locking.** The whole read-modify-replace holds `<docs_dir>/runs.md.lock` (see "Locks"),
  because runs of different repositories may share `docs_dir`. The lock file is created mode
  `0600` and never followed through a symlink.

## 7. Git artefacts

- **Branches.**
  - The run branch is the deck's `Branch:`. When another run of the repository owns that name,
    or the branch exists, it gets the first free `-2`, `-3`, … suffix. A name is also taken
    when a branch under its card prefix exists (R54).
  - A card's branch is `<branch>--card-<n>`; the fix waves' branch is `<branch>--card-final`.
  - A card branch the run did not record is adopted only when it sits at the run tip (R54').
- **The run's worktree** is `<repo>/.worktrees/<run dir name>`, on the run branch at the base.
- **Refs under `refs/leadhand/<run id>/`:**

  | Ref | What it holds |
  |---|---|
  | `violations/<n>` | A check's violation: the worktree as the worker left it. |
  | `violations/<n>-branch` | The card branch's moved tip, when HEAD was elsewhere. |
  | `violations/<n>-autostash` | An operation's autostash. |
  | `violations/<n>-<slug>` | Each ref the worker moved (for example `violations/1-heads-leadhand-demo-card-1`). The slug is the ref name without `refs/`, with every run of characters other than `[A-Za-z0-9_]` turned into `-`; a clash gets `-2`, `-3`. |
  | `violations/<n>-base` | The evidence base worktree's state, when a gate worker changed it. |
  | `violations/<n>-final` | Work kept off the fix review. |
  | `leftovers/<n>` | An implementer's uncommitted leftovers, kept before the next check. |
  | `integration/<card>-<k>` | The card tip before its k-th integration (rebase onto the run tip). |
  | `final/<wave>` | The final branch's tip before fix wave `<wave>`. |

- **Saved commits.** A saved commit (`violations/<n>`, `leftovers/<n>`) is stash-shaped:
  - its first parent is the HEAD it found;
  - its second parent holds the index;
  - its tree holds the tracked working tree.

  An unmerged index (a conflicted merge, rebase or cherry-pick) gives a commit whose tree holds
  the conflict markers and whose second parent is the merged-in commit. The `restored` event
  lists the sibling refs it made under `siblings`. `refs/stash` is never touched.
- **The scratch folder** is `<ws>/.leadhand/<card or run>/<dispatch id>/`. It holds
  `brief.md`, `report.md` and `reply.txt`. It is never reached through a symlink (L2).
- **Excludes.** `.worktrees/` and `.leadhand/` are each added once to
  `<git common dir>/info/exclude`.
- **The evidence base worktree** is `<repo>/.worktrees/<workspace name>--base`. Its run marker
  is the file `leadhand-run` in that worktree's admin folder (`git rev-parse
  --absolute-git-dir`), and it is recognised only with the marker plus `base_worktree_ready`
  (R60).
- **Commits.** Every commit leadhand makes is unsigned (`-c commit.gpgsign=false`) and runs no
  hook. Commits of a worker's work (the runner's for Codex, a cancel's partial commit for
  Claude) carry the trailer `Leadhand-Dispatch: <dispatch id>`.
- **How leadhand runs Git.** Every Git call leadhand makes:
  - sets `GIT_TERMINAL_PROMPT=0` and `GIT_LFS_SKIP_SMUDGE=1`;
  - drops `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR` and
    `GIT_OBJECT_DIRECTORY`;
  - passes `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c diff.ignoreSubmodules=all
    -c diff.relative=false -c diff.noprefix=false -c diff.mnemonicPrefix=false
    -c diff.srcPrefix=a/ -c diff.dstPrefix=b/`.

  So no repository hook, fsmonitor command or LFS smudge runs (I6, H3), and a nested repository
  checked out at a gitlink path is never a change leadhand sees. The runner never commits a
  gitlink (R45).

## 8. Run state

### Event kinds and their fields

Every event line has `seq` (1, 2, … with no gap), `ts` and `kind`; those three names are
reserved. The fields below are the ones the reference writes. A field marked *opt* is present
only in some cases.

| Kind | Fields |
|---|---|
| `run_started` | `run_id`, `base_sha`, `branch` |
| `card_cut` | `card`, `branch`, `base`; *opt* `recut_after`, `wave` |
| `brief_written` | Declared in the kind list; the reference never writes it. |
| `dispatch_requested` | `dispatch_id`, `card`, `attempt`, `dispatch_kind`, `role`, `vendor`, `model`, `effort`, `brief`, `report`, `extra`, `notes`, `resumes`, `resume_worker`, `resume_session`, `minutes`. A check adds `expected_branch`, `expected_head` and `refs`. An implementer-side kind adds `refs`, `ref_scope`, `start_tip` and `untracked`. *opt* `continues`; a run-level dispatch adds `gate` and `wave`. |
| `worker_started` | `dispatch_id`, `worker_id`, `card`, `deadline`, `session`; *opt* `reconciled` |
| `result_recorded` | `dispatch_id`, `card`, `attempt`, `dispatch_kind`, `worker_id`, `tip`, `session_id`, `files`, `stopped`, `charge_ids`, plus the head: `status`, `blocked_by`, `head`, `verdict`, `result`, `source`, `raw_kept`. *opt*: `interrupted`, `reconciled`, `files_skipped`, `partial_commit`, `partial_commit_error`, `partial_gitlinks_skipped`; and for Codex, `committed`, `commit_error`, `commit_skipped` and `gitlinks_skipped` |
| `verdict_recorded` | `card`, `dispatch_id`, `verdict`, `findings`; *opt* `reason`, `wave` |
| `fix_round` | `card`, `n`, `attempt`, `findings`, `review`, `pre_tip` |
| `diff_checked` | `card`, `dispatch_id`, `ok`, `note` |
| `integration_started` | `card`, `onto`, `tip`, `keep` (the `integration/<card>-<k>` ref) |
| `rebased` | `card`, `tip`, `onto`; *opt* `dispatch_id` |
| `conflict` | `card`, `onto`, `paths` |
| `accept_started` | `card`, `target`; *opt* `dispatch_id`, `wave` |
| `accepted` | `card`, `sha`; *opt* `dispatch_id`, `wave` (`card` is `final` for a fix wave) |
| `parked` | `card`, `reason`; *opt* `decision`, `cap_dispatch`, `cap_kind` |
| `unparked` | `card`, `note` |
| `ruling` | `card`, `dispatch_id`, `text` |
| `violation` | `stage` (`found` or `restored`), `by` (`check`, `implementer`, …), `dispatch_id`, `card`, `n`, `ref`. `found` adds `dispatch_kind`, `paths`, `head_moved`, `index_dirty`, `expected_head`, `expected_branch`, `refs` (`{ref: {old, new}}`), `preserved` (`{ref: kept ref}`), and *opt* `hidden`, `untracked`, `operation`, `after_dispatch`, `why`, `base_worktree`, `base_head`. `restored` adds `saved`, `siblings` and `preserved`. |
| `gate_started` | `gate`, `examined_sha`, `dispatch_id` |
| `gate_result` | `gate`, `examined_sha`, `passed`, `findings`, `dispatch_id` |
| `outcome` | `outcome`, `qualifier`, `reasons`, `next_step`, `head` |
| `report_written` | `report`, `outcome`, `outcome_seq` |
| `paused` | `by` |
| `cancelled` | `by` |
| `resumed` | `run_id` |
| `reconciled` | `stage`, plus by stage `found`, `headline`, `resumed_seq`, `dispatch_id`, `runner`, `branch`, `head_before` |
| `base_worktree_added` | `path` |
| `base_worktree_ready` | `path` |

Events come in intent and completion pairs around every side effect:

- `dispatch_requested` → `worker_started` → `result_recorded`;
- `integration_started` → `rebased` or `conflict`;
- `accept_started` → `accepted`;
- `gate_started` → `gate_result`;
- `base_worktree_added` → `base_worktree_ready`.

A crash between the two halves is reconciled by inspecting Git and the run directory.

### Values

- **Card states:** `pending`, `cut`, `requested`, `started`, `reported`, `reviewing`,
  `reviewed`, `fixing`, `integrating`, `accepting`, `accepted`, `parked`.
- **Outcomes:** `complete`, `incomplete`, `blocked`, `failed`.
  - Blocked qualifiers: `paused`, `provider capacity`, `decision needed`, `environment`.
  - An incomplete run cancelled by the operator has the qualifier `cancelled`.
  - Only a blocked or unfinished run continues: `run` and `unpark` refuse any other ending.
- **Dispatch kinds.**
  - Card kinds: `implement`, `fix`, `answer`, `continue` and `integrate` (the implementer
    side); `review`, `rereview` and `verify` (checks).
  - Run-level kinds: `final_review`, `evidence`, `security`, `verify` and `final_fix`.

### Crash points

A testing build exits 97 at a crash point when `LEADHAND_CRASH_AT` names it:

| Crash point | Where it is passed |
|---|---|
| `before_worker_launch` | the runner, before it starts Codex |
| `after_worker_launch` | the runner, after it records Codex |
| `before_rebase` | integration, before the rebase |
| `after_rebase` | integration, after the rebase |
| `before_fast_forward` | acceptance, before the run branch moves |
| `after_fast_forward` | acceptance, after the run branch moves, before `accepted` |
| `before_result_record` | `record result` and the engine's result recording, before anything |
| `after_result_files` | after the card folder's files are copied, before the event |
| `after_charge_before_attempt` | after the usage charge, before the attempt record |
| `after_cancel_request` | a runner asked to stop, its stop not yet confirmed |
| `before_gate_record` | before a gate's result is recorded |

The contract suite's harness adds two of its own, `before_commit` and `after_commit`, in the
fake workers. They follow the same protocol.

## 9. Environment variables

### Production

Set by leadhand:

| Variable | Set for | Value |
|---|---|---|
| `LEADHAND_RUN_ID`, `LEADHAND_RUN_DIR`, `LEADHAND_CLI` | the supervisor (`run`) | The run ID, the run directory and the binary's own path. |
| `LEADHAND_LOCK_FD` | the supervisor (`run`) | The descriptor of the inherited `run.lock`. |
| `LEADHAND_RUN_ID` | Codex | The run ID. |
| `LEADHAND_DISPATCH_TAG` | Codex and its tool shells | `<run id>:<dispatch id>:<generation>`, which finds processes that left Codex's tree. |
| The blocking set | Codex | `GIT_SSH_COMMAND=false`, `GIT_TERMINAL_PROMPT=0`, `GH_CONFIG_DIR=<worker>/gh-config`. The parent's `GIT_CONFIG_KEY_<i>`/`GIT_CONFIG_VALUE_<i>` entries are kept in order, then `credential.helper` reset to empty is appended, and `GIT_CONFIG_COUNT` is raised by one. `GIT_CONFIG_PARAMETERS` gets the parent's entries plus `'core.askPass='`. |

The Codex worker's environment is the parent's, minus `GH_TOKEN`, `GITHUB_TOKEN`,
`GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `SSH_AUTH_SOCK`, `GIT_ASKPASS` and
`SSH_ASKPASS`, plus the variables above.

Read by leadhand:

| Variable | Read by | Use |
|---|---|---|
| `LEADHAND_CONFIG` | every command that loads configuration | The user config file. |
| `CODEX_HOME` | usage | Codex sessions: `$CODEX_HOME/sessions`, else `~/.codex/sessions`. |
| `CLAUDE_CONFIG_DIR` | usage | Claude transcripts: `$CLAUDE_CONFIG_DIR/projects`. A relative value makes `record result` fail with `error: transcript path must be absolute: …` (S1-R13). |
| `CLAUDE_CODE_SESSION_ID` | `next`, `record` | The supervisor's session, recorded in run.json and charged. |
| `LEADHAND_RUN_DIR`, `LEADHAND_RUN_ID` | `dispatch`, `stop-dispatch`, `dispatch-runner` | The run anchor (H2, M1). See below. |
| `LEADHAND_LOCK_FD` | every command, at start | See "Locks". |
| `PATH` | `run`, `dispatch`, `route` | `claude`, `codex` (always found on `PATH`, never named by request.json, R49), the keep-awake tool, and installed-vendor detection. |

**The run anchor.** The dispatch commands act only on an anchored run:

- With `LEADHAND_RUN_DIR` set (a command from the supervisor's session):
  - it must resolve to the same folder as the run given;
  - `LEADHAND_RUN_ID`, when set, must match run.json.
- Set but empty, it is refused.
- Unset (an operator's terminal): a run whose holder.json names a live supervisor is refused.
- In every case:
  - run.json must have a `run_id` and an absolute `workspace`;
  - the run directory must not lie inside the workspace or the repo.

A refusal prints `error: refused: …`.

### Test-only

These are honoured only under `LEADHAND_TESTING=1`; release builds ignore them all (deliberate
change 5). Without `LEADHAND_TESTING=1` each is ignored, and contract tests check that for the
clock offset, the self path, the Codex binary override, the foreground wait and the crash points.

| Variable | Effect |
|---|---|
| `LEADHAND_TESTING` | `1` turns the test hooks on. |
| `LEADHAND_CRASH_AT` | The crash point at which to exit 97. |
| `LEADHAND_CRASH_LOG` | Every crash point passed appends its name and a newline to this file, crash or not. |
| `LEADHAND_CRASH_COUNTER` | With `LEADHAND_CRASH_SKIP`: the named point counts its passes in this file. |
| `LEADHAND_CRASH_SKIP` | How many passes of the named point to let through before crashing. |
| `LEADHAND_CRASH_KILL_PGID` | The process group to SIGKILL just before exiting 97. |
| `LEADHAND_TEST_CODEX_BIN` | The program run as Codex, in place of `codex` on `PATH`. |
| `LEADHAND_TEST_CARD_LOOP_ONLY` | `1`: `next` returns `done` with a null outcome once the card loop ends (no gates). |
| `LEADHAND_INSTALLED_VENDORS` | A comma-separated list that replaces the `PATH` lookup of installed vendors. |
| `LEADHAND_MINUTE_SECONDS` | Seconds per "minute" of a worker's time limit. |
| `LEADHAND_TEST_CLOCK_OFFSET` | Seconds added to the engine's clock, `engine._now`. That is the clock for dispatch deadlines (`next`'s `deadline`, `worker_started.deadline`), deadline checks and `timer`. It does not move `ts` stamps, file times, the runner's limits or the heartbeat. |
| `LEADHAND_TEST_CLI` | The path the binary names itself by, everywhere (see "Conventions"). |
| `LEADHAND_TEST_FOREGROUND_WAIT` | Seconds, read as a float, that replace `FOREGROUND_WAIT` (540 s) as how long `dispatch` waits in the foreground before exit 75. It does not change the runner's limits, the heartbeat or `LEADHAND_MINUTE_SECONDS`. |

**The crash protocol.** A testing build must honour it in every process: the CLI and the
detached runners alike. The contract suite's crash matrix depends on that. At each crash point
`P`:

1. If `LEADHAND_TESTING` is not `1`, nothing happens.
2. If `LEADHAND_CRASH_LOG` is set, append `P\n` to it (open with `O_APPEND`, create mode 0644).
3. If `LEADHAND_CRASH_AT` is not `P`, continue.
4. If `LEADHAND_CRASH_COUNTER` is set, open it, take an exclusive `flock`, read the integer
   (empty is 0), add one, then truncate, write `<n>\n`, `fsync` and close. If `n` is at most
   `LEADHAND_CRASH_SKIP` (default 0), continue.
5. If `LEADHAND_CRASH_KILL_PGID` is set, SIGKILL that process group, ignoring errors.
6. Exit 97 at once, with no cleanup.

The fake vendors' variables (`FAKE_CODEX_*`, `FAKE_CLAUDE_*`) belong to the test fakes in
`tests/fakes/`, not to the CLI.

## 10. The Codex command line

The **dispatch line** that `next` gives the `leadhand:codex` proxy is one shell-quoted line:

```
<cli> dispatch --run <run> --dispatch-id <id> [--card <n>] --attempt <n> --role <role> --model <model> --effort <effort> --brief <brief> --report <report> --cwd <ws> --kind <kind> --branch <branch> [--resume <thread id>]
```

The runner then runs Codex in a new session (`start_new_session`), in the workspace, with the
prompt on stdin:

```
codex exec -m <model>
  -c model_reasoning_effort="<effort>"
  -c approval_policy="never"
  -c sandbox_workspace_write.network_access=<true|false>
  -C <cwd>
  -s <read-only for scout, else workspace-write>
  --add-dir <git common dir>
  [--add-dir <writable dir>]...           # config codex.writable_dirs, then an evidence worker's base worktree
  -c shell_environment_policy.set.<NAME>="<value>"   # one per variable, in this order: the blocking set, LEADHAND_RUN_ID, LEADHAND_DISPATCH_TAG
  --json -o <worker>/last-message.md
  [resume <thread id>]
  -
```

Values are TOML basic strings (JSON-quoted, not ASCII-forced). A resume ID that starts with
`-` is refused. Codex never commits: its sandbox keeps the Git folder read-only, so the runner
commits the work of `implement`, `fix`, `continue`, `answer` and `final_fix` once Codex is
confirmed stopped (R39).

## 11. The launch

`run` starts the supervisor in the run's worktree:

```
claude [--plugin-dir <abs dir>] --settings <overlay> --agent leadhand:supervisor [--model <model>] [--effort <effort>] --permission-mode auto [-p --output-format stream-json --verbose] <first message>
```

- **The model and effort** are the supervisor's Claude route in the run's `config.json`
  (`roles.supervisor.claude`). Without one, `--model` and `--effort` are left out.
- **The settings overlay** is `{"sandbox": {"excludedCommands": ["<cli> *"]}}`.
- **The first message** is `Run the leadhand deck in <run>. leadhand CLI: <cli>. First invoke
  the Skill tool with leadhand:run, then follow it.`
- **The environment** is the launcher's, plus `LEADHAND_RUN_ID`, `LEADHAND_RUN_DIR`,
  `LEADHAND_CLI` and `LEADHAND_LOCK_FD`.
- **`claude`** is found on `PATH`. Its absence is `error: claude not found on PATH`.
- **Refusals.** A run that has ended (other than blocked) is refused with exit 1. So is a CLI
  or `--plugin-dir` inside the workspace (L6), and a sparse checkout.
- **`resumed`.** `run` appends it only when the run has events beyond `run_started`.
- **holder.json** names the supervisor once it has started.
- **Keep-awake.** A keep-awake tool, found through `PATH`, runs beside the supervisor:
  - macOS: `caffeinate -i -w <supervisor pid>`;
  - Linux with `systemd-inhibit`: `systemd-inhibit --what=idle --why=leadhand run tail
    --pid=<pid> -f /dev/null`.
- **Signals.** SIGINT and SIGTERM are forwarded to the supervisor.

## 12. Locks

Every lock is `flock(2)` on the same file the Python locks, so a Rust process and a Python
process exclude each other.

| File | Mode | Held |
|---|---|---|
| `<run>/state.lock` | exclusive, blocking | Around every JSONL append and tail repair, every read, and every read-compute-append sequence. It is re-entrant within a process. |
| `<run>/run.lock` | exclusive, non-blocking | By `run`, for the supervisor's whole life. See below. |
| `<docs_dir>/runs.md.lock` | exclusive, blocking | Around the read-modify-replace of `runs.md`. |
| `workers/<id>/owner.lock` | exclusive, non-blocking with retries | Around each ownership decision (inspect, cancel, supersede, spawn) and each cancel. Never while waiting for a runner. |
| `workers/<id>/runner.lock` | exclusive, non-blocking (2 s of retries) | By a runner for its whole life. A held lock means a live runner, whatever its recorded pid says. Others probe it with a non-blocking lock and unlock at once. |
| `workers/<id>/finalize.lock` | exclusive, blocking | Around finalizing a dispatch: classify, charge, attempt, `result.json`. |
| `$LEADHAND_CRASH_COUNTER` | exclusive, blocking | Testing only: around the crash counter's update. |

**How `run.lock` is held:**

- `run` refuses while holder.json names a supervisor whose pid and start time still match,
  before and after taking the lock.
- The locked descriptor is passed to `claude` (`pass_fds`), and its number goes in
  `LEADHAND_LOCK_FD`.
- The launcher never unlocks while the supervisor lives.

**What every command does first** (`close_inherited_lock`):

- It removes `LEADHAND_LOCK_FD` from its environment, so no child, and no worker, ever
  inherits it.
- It closes that descriptor when it is open and is the same file (device and inode) as
  `$LEADHAND_RUN_DIR/run.lock`. It never unlocks it, so the supervisor's lock stays held.
- It never touches any other descriptor.

The in-place checkout claim (a lock on the checkout folder, with `LEADHAND_CHECKOUT_FD`) goes
with in-place runs (deliberate change 1).

## 13. Non-UTF-8 paths

The Python decodes Git's output as UTF-8 with `surrogateescape`, so an undecodable byte `0xXX`
becomes the lone surrogate U+DCXX. It writes all JSON and JSON lines ASCII-only
(`ensure_ascii=True`), so that byte appears in files and on stdout as the six characters
`\udcXX` (lowercase hex). For example, the path bytes `notes-\xff.txt` are written as
`"notes-\udcff.txt"`.

Rust keeps paths as bytes and reads and writes the same encoding, so either implementation
reads the other's files:

- each `\udcXX` with `XX` ≥ `80` stands for the raw byte `0xXX`;
- every other non-ASCII character is JSON's usual `\uXXXX` escape of its code point (a
  surrogate pair above U+FFFF).

## Deliberate changes and parity tests

Parity tests pass unchanged on Rust. Python's behaviour that a deliberate change alters is
pinned only in `@reference_only` tests:

- in-place runs (change 1);
- `status` with no argument (change 3);
- usage-error wording (change 4).

The version string (change 6) is the exception. Rust reports Python's version through stage 2,
so a parity test pins `leadhand 0.1.0` (`contract_cli.Version`). Stage 3 changes that test at
the cut-over.

**The branch release (change 2, S1-R15).** When a run ends `complete`, `incomplete` or
`failed`, Rust (not Python) does three things:

- records `branch_release_started`, then `branch_released`, between `outcome` and the report;
- detaches the run worktree at the run head;
- adds a branch line to the report's Result section.

So parity tests never pin any of these:

- the absence of those events, or "nothing between `outcome` and the report";
- the run worktree still on the run branch after a terminal outcome;
- the whole text of a terminal run's report.md.

Python's exact versions of these are `@reference_only` tests. A blocked run keeps its branch
in both implementations, so that stays parity. `report RUN_DIR`'s stdout is only the path, so
it stays exact parity.
