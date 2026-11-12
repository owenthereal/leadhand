# Brief: gather behavior evidence

Show that each accepted card's behavior is new: its tests fail on the base
by an assertion and pass on the branch. A compile or import error, a missing
module or file, or a harness failure on the base means the test did not run:
that is not proof, and the behavior does not hold.

- Base worktree: $base_worktree

The accepted cards:

$card_list

For each "Done when" behavior, find the test that checks it, run it in the base
worktree and on the branch, and record both results and the test counts. A test
the base does not have may be copied into the base worktree as a new, untracked
file to run it there; never change a tracked file in either tree. Write your
report (`Result:` first line, then `## Checks`, `## Evidence` and `## Holds`)
to the report file.

$context
