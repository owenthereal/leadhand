# Deck: Basic fixture
Source: docs/plans/basic.md
Base: main @ 3f2a9c1
Branch: leadhand/basic
Build: make build
Test: make test

## Requirements
- R1 Widgets are stored and listed (plan: Goal) → cards 1, 2

## Card 1: Store widgets
- Covers: R1
- Role: implementer
- Hard: no
- Risk: low
- Depends on: none
- Source: plan Task 1
- Allowed paths: src/, tests/
- Out of scope: listing
- Done when: tests/test_store.py passes
- Stop if: the schema has to change

Context: see plan Task 1.

## Card 2: List widgets
- Covers: R1
- Role: implementer
- Hard: no
- Risk: medium
- Depends on: 1
- Source: plan Task 2
- Allowed paths: src/list/
- Out of scope: storage
- Done when: tests/test_list.py passes; full suite passes
- Stop if: two approaches fail
- Inferred: test name (plan named none)
- Inferred: dependency on card 1

Context: see plan Task 2.
