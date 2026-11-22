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

`pytext.json`, `wire.json`, `deck.json`, `config/config.json` and `state/run-paths.json` are
JSON objects of sections, each an array with one row per line: `{"in": ..., "out": ...}`, the
input the test gives its own code and what the reference made of it. The tests check each
section's inputs are theirs, in order, before comparing outputs. An input float is given by its
bits in hex, so that it reads back exactly (in `pytext.json` its value `x` stands beside it;
`wire.json`'s `floats` row gives the bits only). A text that may hold a lone surrogate is
given as its JSON literal.

## Files

| File | What it is | Read by |
|---|---|---|
| `procs-start-times.json` | Process-table records of a `sleep` process, with what the reference's parser derives from each. `linux_stat`: `/proc/<pid>/stat` lines (`stat`, as JSON text), synthetic but with every field Linux writes. `macos_kinfo`: `KERN_PROC_PID` answers (`kinfo`, in hex): 648-byte `kinfo_proc` records with the parsed fields, the command name `sleep` and a fixed owner, every other byte zero; plus an empty answer (no such pid) and a short one. Each row has a `name`, the `pid` asked for, `info` (`pid`, `ppid`, `pgid`, `start`, `zombie`; `null` when the record cannot be read) and `start_time` (the start string the reference reports for the pid: `null` for a zombie or an unreadable record). On macOS the generator first checks the layout against a live `sleep` child's record. | `procs::tests::start_time_parsers_match_reference_samples` |
| `python-events.jsonl` | Five records `{"path": ...}`, appended by the reference's JSON-lines writer under the run's state lock: a surrogate escape (`a\xffb`), non-ASCII text, an astral character, a path holding all three, and a lone surrogate. | `wire::tests::reference_events_with_raw_bytes_round_trip` |
| `charge-checkpoint/transcript.jsonl` | A Claude transcript of four lines: `m1` mid-stream and `m2` whole, then `m1` grown and `m3` new. | `usage::charges::tests::charge_continues_reference_checkpoint` |
| `charge-checkpoint/charges.jsonl` | The two charges the reference wrote (`py1:claude`, `py1:codex`) for the first two transcript lines and the first 18 lines of the frozen rollout `usage/codex-rollout/codex-rollout-interrupted.jsonl`. Their `ts` is the fixed `2026-01-01T00:00:00.000Z`. | the same |
| `charge-checkpoint/totals-by-source.json` | One line: the reference's totals by source (each one's `usage_to_dict`), after it charged the first half, then the rest (`rs1`), then again (`rs2`, nothing new). | the same |
| `usage-mixed.json` | The sum of a 1000/1000-token `claude-opus-4-1` usage and a 2000/2000-token `claude-sonnet-4-5` one: `usage` is its `usage_to_dict`, `cost` the `repr` of its estimated cost at opus 15/75 and sonnet 3/15 per million tokens. | `usage::charges::tests::mixed_model_usage_matches_reference_dict` |
| `charge-checkpoint/charges-all.jsonl` | All six charges of that run by the reference alone (`py1`, `rs1`, `rs2`, each `:claude` and `:codex`), checkpoints included, `ts` fixed as above. | the same |
| `worker-prompts/agents/<name>.md` | An input, not an output: frozen copies of the four worker agent files (`implementer`, `reviewer`, `scout`, `verifier`), frontmatter included, byte for byte as `agents/` held them when the prompts were recorded. The generator reads them in place of `agents/`, and the Rust test builds its prompts from them too, so a later edit to an agent file changes neither side. `config/config.json` was recorded with them too (their `maxTurns`), and the configuration tests read them. | `briefs::tests::worker_prompts_match_reference`, `config::tests` |
| `worker-prompts/<role>.txt` | For each of the seven worker roles, the reference's Codex worker prompt for four brief and report path pairs (plain, a non-UTF-8 byte, non-ASCII text with a space, and an unnormalized path), as the OS gets it, the four joined by NUL bytes. Made from the frozen agent files above. | the same |
| `codex-argv.json` | A JSON array, one row per line: each `case` of the Rust argv test's table (`req` as `request.json` gives it, `last`, `git_common`, `overrides` as name and value pairs, `extra`) and the `argv` the reference made of it: `ok` and each argument's bytes in hex, or `err` and the refusal's text. | `dispatch::argv::tests::argv_matches_reference_table` |
| `cli-version.txt` | What the reference printed for `version`. | `rust_cli::version::test_version_matches_reference` |
| `pytext.json` | The Python text and path functions the crate emulates, each on the inputs its test gives the Rust: `str(PurePosixPath(x))`, `f"{x:.1f}"`/`.2f`/`.6f`, `repr` of text and of floats, `strip`/`split`/`splitlines`, a surrogate-escaped `strip` and `splitlines` of OS bytes, `os.path.realpath` over a set of links, `shutil.which`, `os.path.expanduser` under five `HOME` values, `os.path.normpath`, and `os.path.abspath` in two working directories. `realpath` and `which` ran in a temporary root laid out as the test lays out its own, written `{root}`. | `pytext::tests` |
| `wire.json` | How the reference writes and reads JSON text: surrogate-escape decoding of byte strings, ASCII-escaped dumps of text and a small dict, a dict read back from a line with a repeated key, a line of floats, `repr` and strict UTF-8 encoding of strings given as JSON literals, and a ruling the reference appended with a surrogate-escaped path, with the action line the Rust test prints for it and whether the reference reads that line back to the same path. | `wire::tests` |
| `deck.json` | What the deck parser's patterns (requirement, header split, field, card heading, `Hard:` value) make of each test line: the match's groups, `null` for no match, or the split. | `deck::tests` |
| `config/presets/<name>.toml` | An input, not an output: frozen copies of the two presets, as `presets/` held them when `config/config.json` was recorded. The generator loads them in place of `presets/`, and the Rust tests load them too, so a later edit to a preset changes neither side. | `config::tests` |
| `config/config.json` | The reference's configuration for user and repository files: `snapshot` is its snapshot (`config_to_dict`, pretty, with `Infinity` and `NaN` prices) and the prices it reads back from that snapshot; `messages`, for each user file, its error, or its refusals and warnings. Loaded with the frozen presets above and the frozen agent files of `worker-prompts/agents/` (for `maxTurns`). | `config::tests` |
| `cli-check-json.json` | Python's `json.dumps` of the `check --json` report the Rust test prints for a deck it cannot open (the failure's text, a surrogate escape for the byte that is not UTF-8), and the message's bytes as Python reads them back from it. | `cli::tests::check_json_keeps_path_bytes_as_python_writes_them` |
| `state/event-kinds.txt` | The reference's event kinds, one per line, in its order. | `state::events::tests::event_kinds_listed` |
| `state/round-trip.jsonl`, `state/violations.jsonl`, `state/end-of-run.jsonl` | Event logs the reference appended: events with a non-UTF-8, a non-ASCII and an astral path in lists, map keys and map values; its six violation records; and the records of a run's end. Every `ts` is the fixed time above. | `state::events::tests` |
| `state/seq-after-two.jsonl`, `state/seq-after-two-read.txt` | The two rulings the Rust test writes first (their `ts` fixed), the `paused` event the reference appended after them, and how many events it then read. | `state::write::tests::append_event_assigns_seq_and_ts` |
| `state/attempt-3.jsonl` | The line the reference's `append_attempt` wrote for `{"card": 1, "attempt": 3}`. | `state::write::tests::append_attempt_and_read_attempts` |
| `state/attempts.jsonl`, `state/attempts-read.txt` | A Claude worker's and a Codex worker's attempt records as the reference's `append_attempt` wrote them, and whether its `read_attempts` read them back as the same records, keys in order (`True True`). | `state::write::tests::attempt_records_are_the_python_writers_records` |
| `state/run-paths.json` | Each field of the reference's `run_paths(root)`, in its order, for six roots. | `state::paths::tests` |
| `usage/charge-record.txt` | The reference's charge line for a one-message transcript (`ts` fixed), then its charged checkpoints of that charge for `agent1` and for `other`. | `usage::charges::tests::charge_record_is_durable_and_round_trips` |
| `usage/single-model.json` | The reference's `usage_to_dict` of two `claude-opus-4-1` usages and a `<synthetic>` one, added from an empty usage. | `usage::charges::tests::single_model_usage_serializes_without_by_model` |
| `usage/serialized.jsonl` | The reference's `usage_to_dict` lines of an opus usage, of its sum with a sonnet one, and of its unknown usage. | `usage::tests::usage_serializes_as_the_python_writes_it` |
| `usage/estimate-cost.txt` | The `repr` of the reference's estimated cost of a usage at opus prices for `claude-Opus-4`, then its estimate for `gpt-5` (`None`). | `usage::tests::estimate_cost_matches_python` |
| `usage/claude-records/transcript.jsonl`, `usage/claude-records/messages.txt` | Odd Claude transcript records (the bytes the Rust test writes, built the same way by the generator), and each message ID and usage the reference read from them. | `usage::claude::tests::records_read_as_the_python_reads_them` |
| `usage/codex-rollout/codex-rollout-interrupted.jsonl` | An input, not an output: a frozen copy of `tests/fixtures/codex-rollout-interrupted.jsonl`, so a later edit to that fixture changes neither side. The `charge-checkpoint/` files were recorded from it too. | `usage::codex::tests::rollout_reads_as_the_python_reads_it`, `usage::charges::tests::charge_continues_reference_checkpoint` |
| `usage/codex-rollout/read.txt` | What the reference read from that rollout with four more lines: each response ID and usage, then the thread total and the last token count's total. | the same |
