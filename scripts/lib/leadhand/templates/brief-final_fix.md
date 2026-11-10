# Brief: fix a failed final gate

The run's final gate "$gate" failed. Its findings:

- Findings: $findings
- Commit on: $branch
- Allowed paths: $allowed_paths

Fix what the findings name with the smallest change, inside the allowed paths
above, without undoing any card's "Done when". The run's cards:

$card_list

Commit on the branch above, then write your report to the report file, listing
each finding as fixed or not and why.

$context
