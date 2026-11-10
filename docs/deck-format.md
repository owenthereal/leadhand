# Deck format

A deck is one Markdown file per run (`deck.md`): a work list of cards that
leadhand runs across bounded workers. `leadhand check DECK [--repo PATH]`
validates a deck before a run starts; it prints one problem per line
(`error: ...` or `warning: ...`) and exits 1 on any error. `--repo` defaults to
the current directory.

## Example

```markdown
# Deck: Retry uploads
Source: docs/plans/2026-11-09-retry.md
Base: origin/main @ 3f2a9c1      Branch: leadhand/retry-uploads
Build: make build                 Test: make test
Profile: standard                 Finish: none

## Requirements
- R1 Uploads retry on transient errors (plan: Goal) → cards 2, 3
- R2 Retries are bounded and logged (plan: Task 3) → card 3

## Card 3: Retry uploads on transient errors
- Covers: R1, R2
- Role: implementer
- Hard: no
- Risk: medium
- Depends on: 2
- Source: plan Task 3
- Allowed paths: src/sync/, tests/sync/
- Out of scope: the upload queue
- Done when: tests/sync/retry.rs passes; full test suite passes
- Stop if: the client API has to change; two approaches fail
- Inferred: "Done when" test name (plan named no test)

Context: pointers to the plan section and files, not copies.
```

## Header

`# Deck: <title>`, then header lines before the first `## `. The canonical
form is one `Key: value` per line. The compact form above is also accepted: a
second known key may follow on the same line after two or more spaces.

| Key | Meaning |
| --- | --- |
| `Source` | the plan or tickets the deck was dealt from |
| `Base` | `<ref> @ <sha>`; the commit must exist in the repo |
| `Branch` | the work branch |
| `Build`, `Test` | commands; both required, non-empty |
| `Profile` | optional |
| `Finish` | optional; only `none` is supported in this version |

## Requirements

Optional `## Requirements` section. Each line is
`- R<n> <text> → cards <n>[, <n>]` (`->` is accepted). `cards none` marks a
requirement as uncovered, which `check` reports as a warning. A requirement
that maps to a card that does not exist is an error.

## Cards

`## Card <n>: <title>`, then one `- Key: value` line per field:

| Field | Value |
| --- | --- |
| `Covers` | requirement IDs, comma separated |
| `Role` | `implementer` or `scout` (scout cards are not supported in this version) |
| `Hard` | `no`, or `yes — <reason>` (`-` or `:` also accepted after `yes`) |
| `Risk` | `low`, `medium` or `high` |
| `Depends on` | `none`, or a comma-separated list of card numbers |
| `Source` | where in the plan the card comes from |
| `Allowed paths` | comma-separated paths the worker may change |
| `Out of scope` | what the worker must leave alone |
| `Done when` | the tests or checks that prove the card done |
| `Stop if` | conditions under which the worker stops and reports |
| `Inferred` | something not in the source; may repeat |
| `Route` | optional pin: `<vendor> <model> <effort>`, e.g. `codex gpt-6.1-sol xhigh` |

`Role`, `Hard`, `Risk`, `Allowed paths`, `Done when` and `Stop if` are
required. Any other text after the fields (with or without a leading
`Context:`) is the card's context.

`Hard: yes` needs a reason, and only these count: no test or check can verify
the result; the work crosses module boundaries; the cause of a bug is unknown;
or the design is new with no example to follow. Risk is high for auth, billing,
secrets, CI, release or install scripts, data migrations, public API changes
and concurrency; low for docs, tests, internal-only code and additive changes
behind a flag; medium otherwise.

## What `check` reports

Errors: a missing required field; `Hard: yes` without a reason; a scout card;
a `Finish` other than `none`; unsupported config settings; an unknown
dependency; a dependency cycle; duplicate card numbers; an empty `Build` or
`Test`; a base commit the repo does not have; a requirement mapped to a
nonexistent card. Warnings: an allowed path that does not exist yet; an
uncovered requirement.
