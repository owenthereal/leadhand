---
name: run
description: "leadhand supervisor only: the run loop for one leadhand deck. Use it when the leadhand launch command's first message says to; never otherwise."
---

# Run a leadhand deck

You drive one run. The engine decides every step. You carry out one action at
a time and record what happened. You judge only three things: a worker's
question, a fix diff, and your ruling.

Below, `<cli>` is the leadhand CLI's absolute path and `<run_dir>` the run
directory, both exactly as the first message gives them. Write them out
literally in every command.

## Preflight

1. Run `printenv LEADHAND_RUN_ID` with the Bash tool. If it prints nothing,
   reply "start this run with the leadhand launch command" and stop.
2. Take the run directory and the CLI's absolute path from the first message.
   If either is missing, reply the same and stop.

## The loop

Run `<cli> next <run_dir>`, act on the `action` it prints, and repeat. One
action at a time: finish an action before the next `next`, and never start a
worker while another one is running.

How to call the CLI:

- Every call is one plain command on one line: `<cli>` and its arguments,
  written out literally. No pipes, redirects, heredocs, `&&` or `;`, no
  `cd`, no variables or `$`. The CLI runs outside the sandbox only when the
  command is plain.
- Write `<cli>` as given, and put every other path (the run directory, a
  reply file) in single quotes: paths may contain spaces.
- Free text for `--text`, `--reason` and `--note` goes in single quotes, and
  the text itself must contain no single quote and no line break: write
  do not, not don't, and rephrase any quote or line break from a copied
  question. Example:
  `<cli> record <run_dir> ruling --card 3 --text 'Keep the old flag, because the plan says do not break existing callers (plan: Task 3).'`
- The CLI prints JSON. On failure it prints `error: ...` and exits 1. If the
  error shows your command was wrong (a flag, a number, an ID), correct it and
  run it once more. Otherwise, TaskStop any worker you launched in this step,
  print the error and stop.
- Never run `sleep`; the Bash tool refuses it. Waiting means a background
  timer and ending your turn.
- When a completion notice and a timer notice are both waiting, handle the
  completion notice first.

### `dispatch` without `resume`

1. Call the Agent tool:
   - `subagent_type`: the action's `agent`;
   - `model`: `agent_model`, and `effort`: `agent_effort`, each only when it
     is not null (a Codex route has both null: leave both out);
   - `description`: `card <card> <kind> (attempt <attempt>)`; for a
     run-level dispatch (`card` is null), `run <kind> (wave <wave>)`, or
     just `run <kind>` when `wave` is null;
   - `prompt`: the action's `message`, verbatim.

   It runs in the background and returns the worker's agent ID at once.
   Remember the `dispatch_id` and `reply_file` you launched it for.
2. Immediately record the start with that ID:
   `<cli> record <run_dir> started --dispatch <dispatch_id> --worker <agent id>`
3. Start the timer with the Bash tool and `run_in_background: true`:
   `<cli> timer <run_dir> --dispatch <dispatch_id>`
4. End your turn. The worker's completion notice or the timer's wakes you.

For a Codex route, `agent` is `leadhand:codex` and `message` is one
`leadhand dispatch ...` command for that proxy to run; a Codex resume is
already inside the command. Dispatch it like any other worker.

If `record started` fails because the dispatch already has another worker,
stop the worker you just launched with TaskStop and run `next`.

### Run-level dispatches

Once every card is accepted or parked, the engine runs the end-of-run gates
(`final_review`, `evidence`, `security` when the risk calls for it, and
`verify`). A failing gate gets a fix wave: a `final_fix` implementer, then a
`final_review` of its diff. These come as `dispatch` actions with `card`
null, `gate` (the gate it runs, or null) and `wave` (the fix wave, or null),
and never with `resume`. Handle them exactly like card dispatches, with the
same `started`, timer, result and `stop_worker` commands; those commands take
no `--card`. The engine judges their results itself: no `answer` or
`check_diff` follows them, and you never rule on or park them (`--card` takes
only a card number).

### `dispatch` with `resume`

1. SendMessage to `resume.worker_id` with the action's `message`, verbatim.
   This resumes that finished worker in its own transcript. Remember the
   `dispatch_id` and `reply_file` you resumed it for.
2. Record the start, where `<agent id>` is `resume.worker_id`:
   `<cli> record <run_dir> started --dispatch <dispatch_id> --worker <agent id>`
3. Start the timer with the Bash tool and `run_in_background: true`:
   `<cli> timer <run_dir> --dispatch <dispatch_id>`
4. End your turn.

### A worker's completion notice

When a worker's completion notice arrives, record its reply against the
`dispatch_id` and `reply_file` you remembered when you launched or resumed
that worker. Only if you have lost them, run `next`: a `wait` names
`dispatch_id`, `worker_id` and `reply_file`.

1. Write the worker's reply to `reply_file` with the Write tool: the notice's
   result text, verbatim, never a summary. If the Write tool refuses because
   the file already exists, Read it, then write it.
2. Record it with one plain command:
   `<cli> record <run_dir> result --dispatch <dispatch_id> --worker <agent id> --reply-file '<reply_file>'`
3. Run `next`.

For a Codex route the engine takes the result from the Codex runner, not from
the proxy's reply. If the runner is still running, `record result` prints a
`wait` and records nothing; the timer reports the result when it lands.

A notice with status `killed` is a worker you stopped. Its result is already
recorded; do nothing.

### A timer finishes

Run `next`. It returns `wait`, `stop_worker`, or the next step if the result
is already recorded.

### `wait`

Do nothing until a completion notice arrives or a timer finishes: end your
turn. If no timer of yours is running for `dispatch_id`, start one first:
`<cli> timer <run_dir> --dispatch <dispatch_id>` with the Bash tool and
`run_in_background: true`. Never call `next` repeatedly while you wait.

### `stop_worker`

1. TaskStop the worker, with `worker_id` as the task ID. If TaskStop says the
   task is unknown or already finished, carry on with step 2.
2. If `record_result` is true (a Claude worker), record it as stopped:
   `<cli> record <run_dir> result --dispatch <dispatch_id> --worker <worker_id> --stopped --reply 'Status: SPENT (time limit)'`
   If `record_result` is false (a Codex route), the engine has already
   stopped the Codex worker and recorded its result: record nothing.
3. Run `next`.

### `check_diff`

A fix changed only tests or docs, so no re-review runs; you check it. Read the
`diff` file and the `findings` file it answers. The diff is acceptable when it
addresses every finding and weakens, skips or deletes no test. Record one of:

- `<cli> record <run_dir> diff-check --card <card> --ok yes`
- `<cli> record <run_dir> diff-check --card <card> --ok no --note '<why>'`

Then run `next`.

### `answer`

A worker asked a question: `question`, with the full text in the `report`
file. Look for the answer in the card (in `<run_dir>/deck.md`), the plan the
deck names as its source, and `decisions.md` in the docs directory (the
directory two levels above `<run_dir>`).

- If they settle it, record the ruling, with its source, in a sentence or two:
  `<cli> record <run_dir> ruling --card <card> --text '<answer>'`
  The engine then resumes the worker with your ruling.
- If it is a product decision (behavior, scope or wording they leave open),
  do not decide it:
  `<cli> record <run_dir> park --card <card> --reason 'decision needed: <question>'`

Then run `next`.

### `done`

Print the first ten lines of the file the action's `report` names (Read
tool, 10 lines). If `report` is null, print the action's `outcome`,
`qualifier` and `reasons` instead. Then stop.

## Hygiene

- Never read worker transcripts. A worker's reply and the files the engine
  names are all you need.
- Pass files by path. Never paste a report, brief or diff into a message.
- Never edit code, or any file in the workspace. Workers do the work. The one
  file you write is a dispatch's `reply_file` (in the git-ignored scratch
  folder), with the worker's reply verbatim.
- Never run `git push`, `gh pr` or `gh merge`.
- Record every ruling with `record ruling`. Never answer a worker any other
  way.
- If you are unsure whether a worker was started, run `next` before starting
  another. `wait` names the running worker; `dispatch` again means no start
  was recorded.
- Keep your own messages to one short line per step. Do not restate the JSON.

## Steering

When the operator tells you to skip a card:

1. Run `next`. If it returns a `wait` for that card (its `dispatch_id`
   starts with `c<card>-`), stop its worker first: TaskStop `worker_id`, then
   `<cli> record <run_dir> result --dispatch <dispatch_id> --worker <worker_id> --stopped --reply 'Status: SPENT (time limit)'`
   For a Codex dispatch, `worker_id` is the proxy; this record also stops the
   Codex worker itself (the engine's stop path handles the runner).
2. Record the card parked:
   `<cli> record <run_dir> park --card <card> --reason 'operator: skip'`
3. Run `next` and carry on.

When the operator tells you to pause or cancel the run: to pause, run `<cli> pause <run_dir>`; to cancel, `<cli> cancel <run_dir>`.
Then run `next` and act on what it returns. A pause lets running workers
finish (`wait`) and starts nothing new; a cancel returns `stop_worker` for
each worker you started. Either ends with `done`.

When the operator tells you to let a parked card continue, with their answer
or instruction, run `<cli> unpark <run_dir> --card N --note '…'` with `N` the
card and their words as the note (the same quoting rules as `--text`), then
run `next`. The card resumes once no other work is in flight.

After a relaunch, `next` first settles what the earlier session left running.
A `wait` can then name a Codex worker started by that session: no completion
notice comes for it, so start the timer and end your turn as usual.
