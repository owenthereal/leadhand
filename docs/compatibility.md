# Compatibility

## Supervisor launch

Tested with Claude Code 2.1.322 and tmux 3.8 on macOS.

- `claude --agent leadhand:supervisor` accepts the plugin-scoped agent name, in print mode
  (`-p --output-format stream-json --verbose`) and interactively. Decision:
  `SUPERVISOR_AGENT = "leadhand:supervisor"`.
- For the main-session agent, frontmatter `model` is applied but frontmatter `effort` is not
  (the session ran at the default effort), and `skills` preloading did not put the skill body
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
  Only one background worker was observed at a time.
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
- Claude Code's Bash tool refuses foreground `sleep N`, and `ps` is not permitted inside its
  sandbox; supervisor and worker instructions must not depend on either.

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

Not yet checked.

## Limits

Tested with Claude Code 2.1.322 and codex-cli 0.167.0 on macOS.

Claude workers:

- `maxTurns` in a subagent's frontmatter is a hard cap: with `maxTurns: 2` the worker made
  exactly two assistant turns. The stop is not an error (notification `status` is
  `completed`); it is marked only in text — the result begins "NOTE: this agent stopped at
  its 2-turn limit before finishing" and the notification summary says "stopped at its
  2-turn limit (partial result; ...)". The agent can be continued with SendMessage.
  Decision: such a reply has no status line, so it is handled by the no-status-line rule
  (BLOCKED, raw reply kept); the dispatcher also recognises the
  `stopped at its <N>-turn limit` text and records the worker status as `SPENT`.
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
- `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=credential.helper`, `GIT_CONFIG_VALUE_0=` adds an
  empty `credential.helper` entry after the user's helpers, which resets Git's helper list.

## Interrupted usage

Not yet checked.
