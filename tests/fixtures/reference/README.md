# Reference fixtures

What the Python reference CLI did, recorded so that the Rust tests that once called into it
can still check their assertions against it. The Rust tests read these files at run time
(from `CARGO_MANIFEST_DIR`), never through `include_str!`.

These files were made by the generator in this folder. It imports the reference's library,
writes every file below and prints each one with its size. Every input is fixed: no
timestamps, process IDs, random values or host paths reach a fixture, and a temporary path is
replaced by a neutral stand-in. Running it again reproduces every file byte for byte.

Text that is not UTF-8, or holds a lone surrogate, is written the way the reference writes
JSON: ASCII escapes, an undecodable byte as its surrogate escape (`\udcff` for the byte
`0xff`, contract item 13).

## Files

| File | What it is | Read by |
|---|---|---|
| `procs-start-times.json` | Process-table records of a `sleep` process, with what the reference's parser derives from each. `linux_stat`: `/proc/<pid>/stat` lines (`stat`, as JSON text), synthetic but with every field Linux writes. `macos_kinfo`: `KERN_PROC_PID` answers (`kinfo`, in hex): 648-byte `kinfo_proc` records with the parsed fields, the command name `sleep` and a fixed owner, every other byte zero; plus an empty answer (no such pid) and a short one. Each row has a `name`, the `pid` asked for, `info` (`pid`, `ppid`, `pgid`, `start`, `zombie`; `null` when the record cannot be read) and `start_time` (the start string the reference reports for the pid: `null` for a zombie or an unreadable record). On macOS the generator first checks the layout against a live `sleep` child's record. | `procs::tests::start_time_parsers_match_reference_samples` |
| `python-events.jsonl` | Five records `{"path": ...}`, appended by the reference's JSON-lines writer under the run's state lock: a surrogate escape (`a\xffb`), non-ASCII text, an astral character, a path holding all three, and a lone surrogate. | `wire::tests::reference_events_with_raw_bytes_round_trip` |
| `charge-checkpoint/transcript.jsonl` | A Claude transcript of four lines: `m1` mid-stream and `m2` whole, then `m1` grown and `m3` new. | `usage::charges::tests::charge_continues_reference_checkpoint` |
| `charge-checkpoint/charges.jsonl` | The two charges the reference wrote (`py1:claude`, `py1:codex`) for the first two transcript lines and the first 18 lines of `tests/fixtures/codex-rollout-interrupted.jsonl`. Their `ts` is the fixed `2026-01-01T00:00:00.000Z`. | the same |
| `charge-checkpoint/totals-by-source.json` | One line: the reference's totals by source (each one's `usage_to_dict`), after it charged the first half, then the rest (`rs1`), then again (`rs2`, nothing new). | the same |
| `usage-mixed.json` | The sum of a 1000/1000-token `claude-opus-4-1` usage and a 2000/2000-token `claude-sonnet-4-5` one: `usage` is its `usage_to_dict`, `cost` the `repr` of its estimated cost at opus 15/75 and sonnet 3/15 per million tokens. | `usage::charges::tests::mixed_model_usage_matches_reference_dict` |
| `worker-prompts/<role>.txt` | For each of the seven worker roles, the reference's Codex worker prompt for four brief and report path pairs (plain, a non-UTF-8 byte, non-ASCII text with a space, and an unnormalized path), as the OS gets it, the four joined by NUL bytes. The prompts hold the agent files' bodies as they stand. | `briefs::tests::worker_prompts_match_reference` |
| `codex-argv.json` | A JSON array, one row per line: each `case` of the Rust argv test's table (`req` as `request.json` gives it, `last`, `git_common`, `overrides` as name and value pairs, `extra`) and the `argv` the reference made of it: `ok` and each argument's bytes in hex, or `err` and the refusal's text. | `dispatch::argv::tests::argv_matches_reference_table` |
| `cli-version.txt` | What the reference printed for `version`. | `rust_cli::version::test_version_matches_reference` |

The worker prompts hold the agent files' text, so a change to an agent file makes
`worker_prompts_match_reference` fail until those fixtures are updated with it.
