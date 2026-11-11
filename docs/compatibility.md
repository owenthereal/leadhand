# Compatibility

## Supervisor launch

Tested with Claude Code 2.1.322 and tmux 3.8 on macOS.

- `claude --agent leadhand:supervisor` accepts the plugin-scoped agent name, in print mode
  (`-p --output-format stream-json --verbose`) and interactively. Decision:
  `SUPERVISOR_AGENT = "leadhand:supervisor"`.
- For the main-session agent, frontmatter `model` is applied but frontmatter `effort` is not
  (the session ran at medium effort although the frontmatter said low; where medium came
  from was not established), and `skills` preloading did not put the skill body
  in context under either the bare or the plugin-qualified name; the skill is listed only as
  `<plugin>:<skill>`. Decisions: the launcher passes `--model` and `--effort` explicitly;
  `agents/supervisor.md` sets `skills: [leadhand:run]` but does not rely on it — its body,
  and the launcher's first message, tell the supervisor to invoke the Skill tool with
  `leadhand:run` first.
- Subagent workers run in the launch working directory, including paths with spaces, when
  the launch happens from an unrelated directory with `cwd` set to the repository.
- Agent dispatches are asynchronous even without `run_in_background`: the Agent tool returns
  the agent ID at once and the result arrives later as a task notification.
- Print mode waits for background workers: when a worker finishes, the stream carries a
  `system` record with subtype `task_notification`, the session runs another turn, and a
  second `result` record (`result_index` 1, `origin.kind` `task-notification`) is written
  before the process exits. Decision: end-to-end tests drive the supervisor headless, read
  the stream until the process exits, and use the result with the highest `result_index`.
  Only one background worker was observed at a time, and it finished about 1.2 s after the
  main turn ended. Confirmed end to end (Claude Code 2.1.322, a complete two-card run, 7
  dispatches, one process of about 275 s): the headless `-p` supervisor stayed alive while
  background workers ran and exited only after the run's outcome was recorded. In that run
  every wait after a `result` record was under a second, because the supervisor polled with
  the CLI's `timer` command; a failed earlier run showed a session sitting 570 s after a
  `result` record before the next stream line, so `-p` does not exit merely because the
  workers are slow. Not established: a worker silent for much longer than that.
- Interactive launch in tmux works. The workspace trust dialog's default choice is
  "No, exit"; a driver must select "Yes, I trust this folder" (Down, then Enter) after
  checking the highlighted line.
- `--settings '{"sandbox":{"excludedCommands":["<absolute path> *"]}}'` takes effect: a
  matching command runs outside the sandbox, with network access and writes outside the
  workspace. Decision: the launcher supplies the leadhand CLI's exclusion itself through
  this overlay, and the supervisor calls the CLI as a plain command (absolute path and
  arguments, no redirection, pipes or command chains). Not established: whether the overlay
  merges with or replaces the user's own `excludedCommands` (the user-exclusion control used
  a command with a redirection, which ran sandboxed even without the overlay).
- In end-to-end runs (31 CLI calls in the complete run, none sandboxed, while control
  commands such as `printenv` did run sandboxed), the `"<cli path> *"` exclusion matched every
  plain `<cli> …` invocation, including ones whose run-directory argument was single-quoted
  because it contained spaces, but not a command with a heredoc (`<cli> record … --reply - <<'EOF'`):
  that one ran sandboxed and could not write the run directory. Decision: the supervisor
  never uses heredocs, pipes or redirects. It writes a worker's reply to the dispatch's
  `reply_file` (a git-ignored file in the workspace) with its Write tool and records it with
  the plain command `<cli> record <run dir> result … --reply-file '<path>'`. The complete run
  used no heredoc `record` call, so the reply-file route is the one confirmed working.
- Not established: whether the `"<cli path> *"` exclusion matches when the CLI's path contains
  spaces (the supervisor then has to quote it). Until an end-to-end run shows it does,
  `leadhand run` warns when the CLI path contains whitespace: install leadhand under a path
  without spaces.
- With Claude Code's sandbox on, its Bash tool refused foreground `sleep N` and `ps` was not
  permitted; supervisor and worker instructions must not depend on either.

## Hook identity

Tested with Claude Code 2.1.322, print mode and interactive (`--permission-mode auto`).

- PreToolUse hook input for the main session has no `agent_id` and has `agent_type` equal to
  the `--agent` name (for leadhand, `leadhand:supervisor`).
- Hook input for a subagent worker has `agent_id` and `agent_type` equal to the subagent type.
- `session_id`, `transcript_path` and `permission_mode` are the same for the supervisor and
  its workers (workers inherit the session's permission mode), so they cannot tell them
  apart. Other keys present: `cwd`, `effort`, `hook_event_name`, `prompt_id`, `tool_input`,
  `tool_name`, `tool_use_id`, and `scratchpad_dir` in interactive sessions.
- An environment variable set at launch (`LEADHAND_RUN_ID`) reaches the hook for both the
  supervisor and its workers.
- Decision: a hook call comes from the supervisor if and only if `agent_id` is absent and
  `agent_type` is `leadhand:supervisor`. A call with `agent_id` is a worker. A call without
  `agent_type` is not a leadhand supervisor. The run is identified by `LEADHAND_RUN_ID`.

## Worker lifecycle

Tested with Claude Code 2.1.322 and codex-cli 0.167.0 on macOS.

Claude workers:

- Claude Code runs each Bash command in its own process group, as a child of `claude`. When
  the session exits on SIGTERM, it kills its workers' running commands within about 5 s; a
  worker's transcript then just stops after its last `tool_use` record. Decision: after a
  supervisor crash or exit, every Claude worker without a recorded result counts as
  interrupted.
- TaskStop with the agent ID returned at launch stops a background worker and kills its
  running command within a few seconds. The worker's transcript ends with a rejected
  `tool_result` and the user text `[Request interrupted by user for tool use]`, with no final
  assistant message; the task notification has status `killed`. Decision: such a worker is
  recorded as stopped (no status line), and its usage is still charged from its transcript.
- SendMessage to a finished worker's agent ID resumes it: it answers from its history,
  appends to the same transcript, and produces another task notification with the same task
  ID. Decision: resume uses SendMessage on the recorded agent ID, and the reply is read from
  the worker's own hand-back.

Codex workers and the runner:

- A process started by a worker's Bash command survived a SIGKILL of `claude` (it was
  orphaned, not signalled), but not SIGTERM or TaskStop (observed); an orderly exit is
  inferred to behave like SIGTERM. A runner detached into a new session (double fork,
  `start_new_session`) is not a descendant of `claude`; it survived the SIGKILL, kept running
  and wrote its result. That it also survives SIGTERM, an orderly exit and TaskStop is
  inferred from it not being a descendant, not observed. Decision: the dispatcher hands
  each Codex worker to a detached runner. The runner writes `runner.json` (runner pid, pgid
  and process start time; Codex pid and pgid) before starting Codex and `result.json` when
  Codex exits. Restart treats the runner as alive only if its pid exists and its start time
  matches; `result.json` means finished; neither means interrupted.
- Codex starts each tool command and each helper process (MCP server processes, its
  code-mode host) in its own process group, as a direct child of Codex; their children stay
  in those groups. Signalling Codex's process group does not reach them. On SIGINT Codex
  shut them all down itself. On SIGKILL it cannot, and they would be orphaned.
- Decision: the runner confirms that Codex and its tool processes have all stopped. It
  snapshots Codex's descendants by walking parent pids from the process table (`ps` on
  macOS, `/proc` on Linux; the runner runs outside Claude Code's sandbox, where `ps` is
  allowed). It records each as (pid, pgid, start time), before the first signal and again
  before each escalation, because orphans are reparented to init and can no longer be found
  by the walk. The ladder is SIGINT to Codex's group, up to 5 s, SIGTERM to Codex's group
  and to each recorded descendant's group (never the runner's own), 5 s, then SIGKILL to
  whatever remains. Stopped means Codex has been reaped and no recorded (pid, start time) is
  still alive. Recovery re-checks the recorded set before re-dispatching. Only the SIGINT
  step was exercised (nothing was left after it); the SIGTERM/SIGKILL escalation to
  descendant groups is design, not observation.
- The thread ID is in `thread.started.thread_id`. `codex exec` accepts options before or
  after `resume` and after the thread ID, and `-m` and `-c` are accepted with `resume`.
  Resumed turns ran on the requested model and effort, which were also the thread's original
  ones; every resume passed the same `-m`/`-c` as the first run. Whether resume honours a
  different `-m`/`-c` is untested. Decision:
  `codex_argv` for a resume is
  `codex exec <the same options as the first run> resume <thread id> -`, prompt on stdin.
- Codex's `workspace-write` sandbox keeps the repository's Git directory read-only, even when
  that directory is passed with `--add-dir`. In an end-to-end run a Codex implementer wrote
  and tested its change, but `git commit` failed with "Operation not permitted" creating the
  worktree's `index.lock`, so it reported `Status: BLOCKED` (environment). Decision:
  - Codex workers never commit. Their prompt overrides the contract's commit rule: leave the
    changes in the working tree and list the changed files in the report.
  - The runner, which runs outside Codex's sandbox, commits the work once Codex is confirmed
    stopped and before it writes the result. This applies to implementer-side dispatches
    (implement, fix, continue, answer, final fix), whatever the final status, so partial work
    survives.
  - The runner commits only when the workspace is on the expected branch and no Git
    operation is in progress. It runs `git add -A`, which respects `.gitignore` and
    `info/exclude`, so the `.leadhand/` scratch folder is never committed, then an unsigned
    commit with hooks off. The message is `<kind> card <n>: <summary>`, with a
    `Leadhand-Dispatch: <dispatch id>` trailer.
  - The runner records a commit intent first. After a restart, a HEAD that already carries
    the trailer is not committed again. The commit's SHA is in `result.json` (`committed`).
  - Integrating a card (a rebase) needs Git writes too. An integrate attempt always runs on
    the role's Claude route; without Claude installed the card is parked with "integration
    needs a Claude worker".

## Limits

Tested with Claude Code 2.1.322 and codex-cli 0.167.0 on macOS.

Claude workers:

- `maxTurns` in a subagent's frontmatter is a hard cap: with `maxTurns: 2` the worker made
  exactly two assistant turns. The stop is not an error (notification `status` is
  `completed`); it is marked only in text — the result begins "NOTE: this agent stopped at
  its 2-turn limit before finishing" and the notification summary says "stopped at its
  2-turn limit (partial result; ...)". The agent can be continued with SendMessage.
  Decision: the worker status is `SPENT` when the dispatcher recognises the
  `stopped at its <N>-turn limit` text, otherwise `BLOCKED` (the no-status-line rule, raw
  reply kept).
- Per-dispatch Agent `model` and `effort` override the subagent's frontmatter (a haiku/low
  worker dispatched with `model: "sonnet"`, `effort: "medium"` ran as sonnet at medium).
  Decision: the dispatcher sets model and effort on every Agent call.
- `autoCompactWindow`: unverified. The check did not reach the window (the Read tool refuses
  reads over 25,000 tokens, so context stayed under 8K). Leadhand does not rely on it.

Codex workers (`codex exec --json`):

- Event types: `thread.started`, `turn.started`, `item.started`, `item.completed`,
  `turn.completed`. Shell commands complete as `command_execution` items (`command`,
  `aggregated_output`, `exit_code`, `status`); edits as `file_change` items (`changes` with
  `path` and `kind`, `status`). Decision:
  `STEP_ITEM_TYPES = ("command_execution", "file_change")`, counted on `item.completed`.
  A sanitized recorded stream is in `tests/fixtures/codex-exec-stream.jsonl`.
- `turn.completed.usage` fields: `input_tokens`, `cached_input_tokens`,
  `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens`.
- Step stop: SIGINT to Codex's process group (Codex started in its own group) ends it in
  well under a second with exit code 1 and no `turn.completed`, so the interrupted turn has
  no usage record in the stream. SIGTERM after 5 s remains the fallback.
  `codex exec [options] resume <thread id> -` continues the same thread afterwards.
- `--strict-config` rejects unknown `-c` keys and accepts
  `model_auto_compact_token_limit`. No compaction was observed (the check never exceeded the
  limit), so Codex context control is unverified; leadhand may set the key but does not rely
  on it.
- `-c sandbox_workspace_write.network_access=false` overrides a user config that enables
  network access (the worker's `curl` failed to resolve the host). Decision: the dispatcher
  always passes it.
- Worker environment: with Codex's default `shell_environment_policy`, process-environment
  variables reached the shell, including names containing `KEY` and `TOKEN`; with
  `inherit = "core"` (a setting users do use) none of them did. Values given with
  `-c shell_environment_policy.set.<NAME>=<value>` reached the shell under both policies and
  override a same-named process variable. Decision: the dispatcher delivers worker
  variables through `shell_environment_policy.set`. Under the default policy, other
  variables from the launching environment (for example `SSH_AUTH_SOCK`) also reach the
  worker shell.
- Push and `gh` blocking confirmed inside a real Codex shell (a worker started by the real
  dispatcher, with a host control showing the same pushes succeed outside Codex): `git push`
  to an `ssh://` remote, an SCP-style remote and a remote with an explicit `pushurl` all
  failed with exit 128 ("Could not read from remote repository") before any SSH process ran
  (`GIT_SSH_COMMAND=false`), and `gh auth status` exited 1 ("not logged into any GitHub
  hosts") with `GH_CONFIG_DIR` set to the per-dispatch empty folder. Nothing reached any
  target remote.
- `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=credential.helper`, `GIT_CONFIG_VALUE_0=` adds an
  empty `credential.helper` entry after the user's helpers, which resets Git's helper list.

Known limits of the Codex worker environment. Leadhand removes the token variables, the SSH
agent socket and the askpass variables, resets `credential.helper` and `core.askPass`, points
`gh` at an empty config folder and makes SSH fail. Credentials that reach Git by other paths
are not blocked:

- `~/.netrc` (or `_netrc`): Git's HTTP layer reads it for matching hosts.
- `http.extraHeader` (or `http.<url>.extraHeader`) in the user's Git config, for example an
  `Authorization` header.
- Remote URLs that embed a token (`https://<token>@host/...`) in the repository's config.
- Hooks: the worker can write the repository's common Git directory (it is a writable
  folder for commits), so it can plant hooks that later run in the operator's own Git
  commands.
- A push to a local path needs no credentials and still works.

## End to end

Tested with Claude Code 2.1.322, codex-cli 0.167.0 and tmux 3.8 on macOS, headless, from a
directory unrelated to the repository.

- A two-card run (card 1 pinned to `claude sonnet low`, card 2 to `codex gpt-6-luna medium`,
  depending on card 1) reached outcome `complete` in about 266 s with the supervisor driven by
  `claude -p`. Both cards' commits were on the run branch, every gate (final review, evidence,
  verify) passed on the run head, and `report.md` was written by the run itself.
- The Agent tool accepted the route model names (`sonnet`, `haiku`) and efforts; workers ran
  on the requested models. `leadhand:run` loaded through the Skill tool.
- Every worker, Claude and Codex, ran in the run's workspace (or the evidence base worktree).
  No `git push` or `gh pr` command ran, and the fixture's origin was unchanged.
- Usage was recorded for every attempt, including the supervisor and the Claude proxy that
  relays each Codex dispatch; the report prices Claude only and reports the Codex meter before
  and after.
- Runs that failed before it, and what changed: a heredoc `record` ran sandboxed and could not
  write the run directory (the reply-file route above); a Codex implementer could not commit
  (the runner-commits rule under Worker lifecycle).

## Interrupted end to end

Same tools and fixture as above, with two interruptions: card 2's Codex worker was SIGKILLed
about 1 s after it started, and the supervisor session was SIGKILLed while card 2's second
attempt (a detached runner) was running. The run reached outcome `complete` after a relaunch
with the `launch-line` command; each card was accepted once and the run branch had no
duplicate commits.

- A Codex killed before its first response leaves a rollout with no `token_usage_record`. The
  attempt is charged as unknown and possibly incomplete (not zero), the report's
  `Attempts with unknown usage` counts it, and the other threads' charges equal their rollout
  totals.
- A relaunch while the supervisor lives is refused (exit 3, "Run already in progress",
  nothing written to the event log). Once the supervisor is gone the relaunch succeeds within
  seconds, so the run lock is released with the supervisor.
- A detached runner outlives its killed supervisor, keeps its process and generation, and
  writes its result; the resumed supervisor folds that result in instead of restarting the
  worker, and shows `Resumed at card 2; card 2's Codex worker is still running; waiting for it`.
- The resumed headless `-p` session waited through the running Codex worker (about 99 s
  without a model turn) and exited only after the outcome was recorded.

## Interrupted usage

Tested with Claude Code 2.1.322 and codex-cli 0.167.0 on macOS.

Codex:

- Rollout files are `~/.codex/sessions/YYYY/MM/DD/rollout-<local time>-<thread id>.jsonl`
  (under `CODEX_HOME` when set). A resumed thread appends to the same file; earlier lines are
  unchanged.
- After every model response Codex writes a top-level `token_usage_record` with
  `payload.response_id` and three usage objects: `payload.usage` (that response),
  `payload.turn_token_usage`, and `payload.thread_token_usage` (the whole thread, across
  processes). The `event_msg` record with `payload.type == "token_count"` carries
  `payload.info.total_token_usage` and `payload.info.last_token_usage`.
- A `codex exec` killed (SIGKILL) after its first command left a `token_usage_record` for its
  completed response but no `token_count`. After one resume, `total_token_usage` covered
  only the resumed turn, while `thread_token_usage` equalled the killed response plus the
  resumed turn. In a thread that was never killed, the two totals agreed at every turn.
- Decision: Codex usage is the sum of `payload.usage` over distinct `payload.response_id` in
  `token_usage_record` records, cross-checked against the last `payload.thread_token_usage`.
  `token_count`'s `total_token_usage` is only a fallback for a file with no
  `token_usage_record`, because it undercounts a killed and resumed thread. A killed attempt
  with no usage record is unknown, not zero. A response still in flight at the kill leaves
  no record anywhere. `input_tokens` includes `cached_input_tokens`.
- Fixture: `tests/fixtures/codex-rollout-interrupted.jsonl` is the sanitized rollout after
  the kill and one resume.

Claude:

- A worker's transcript is `~/.claude/projects/<project dir>/<session id>/subagents/agent-<agent id>.jsonl`.
  Assistant records carry `message.id`, `message.model` and `message.usage`, with
  `isSidechain: true` and `agentId`. One response is written as one record per content block
  with the same `message.id`; in every repeated ID observed, the usage on those records was
  identical (including `output_tokens`). Decision: usage is charged once per `message.id`,
  taking the record with the largest `output_tokens`.
- A stopped worker's transcript keeps the usage of every completed response
  (`tests/fixtures/claude-subagent-stopped.jsonl`). A worker resumed with SendMessage appends
  to the same file (`tests/fixtures/claude-subagent-resumed.jsonl`). Only message IDs not yet
  charged are new.
- The session's own transcript is `~/.claude/projects/<project dir>/<session id>.jsonl`, the
  same path hooks receive as `transcript_path`. `CLAUDE_CODE_SESSION_ID` is set in the
  session's Bash commands and equals the session ID. The session transcript holds no
  sidechain or worker records, and its message IDs are disjoint from the workers'. Decision:
  the supervisor's usage comes from the session transcript and each worker's from its own
  file, so charging both does not double-count. The session transcript's `cost-state` record
  and the `<usage>` summary in task notifications already include worker usage, so they are
  never added to the charges.
