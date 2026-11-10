# Brief: gather behavior evidence

Show that each accepted card's behavior is new: its tests fail (or do not
exist) on the base and pass on the branch.

- Base worktree: $base_worktree

The accepted cards:

$card_list

For each "Done when" behavior, find the test that checks it, run it in the base
worktree and on the branch, and record both results and the test counts. Never
edit either tree. Write your report (`Result:` first line, then `## Checks`,
`## Evidence` and `## Holds`) to the report file.

$context
