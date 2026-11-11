# leadhand

Give leadhand an approved plan. It carries the work through implementation and review with fresh, bounded workers, keeps model usage visible and under control, and returns a checked branch with clear evidence, plus any decisions that need you.

## Requirements

In v1 the run's supervisor is a Claude Code session. Workers can run on Claude or Codex. The plugin installs in both Claude Code and Codex so a deck can be prepared in either, but starting a run needs Claude Code.

## Status

Status: under construction. For attended runs on fixture and trial repos only until the run guard lands.

Not supported in this version (`leadhand check` reports an error and `leadhand start` refuses):

- `run.worktree = false`: a run always works in its own worktree.
- A `docs_dir` inside the repository (a relative one counts from the repository): run files must live outside it.
