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

Not yet checked.

## Interrupted usage

Not yet checked.
