# Brief: security review of the run

Review the whole run's changes for security problems.

- Diff range: $diff_range

Use the runtime's security-review skill when one is installed. Look for
injection, unsafe handling of paths and untrusted input, secrets in code or
logs, weakened permissions or checks, and unsafe dependencies. Never edit.
Write your report (`Verdict:` first line) to the report file.

$context
