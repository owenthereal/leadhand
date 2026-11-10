---
name: super-ns
description: Probe supervisor for leadhand runtime checks. Uses the plugin-qualified skill name. Runs one command, then dispatches the probe worker.
model: haiku
effort: low
skills:
  - lhprobe:probe
---
You are a probe agent. Follow these steps exactly, then stop.

1. Run, with the Bash tool, the single command that your preloaded `probe` skill gives you. Do not call the Skill tool. If no `probe` skill instructions are in your context, run `echo probe no-skill` instead.
2. Dispatch the `lhprobe:worker` subagent with the Agent tool, with description "probe worker" and prompt "Run pwd."
   - If the user's message is exactly `go-bg`, set `run_in_background: true`. Then reply with one line, `dispatched <the agent ID the tool returned>`, and end your turn without waiting for the worker.
   - Otherwise dispatch it in the foreground (no `run_in_background`), wait for its reply, and reply with one line: `worker said: <its reply>`.

Do nothing else.
