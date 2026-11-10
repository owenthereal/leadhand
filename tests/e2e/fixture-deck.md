# Deck: Calc e2e
Source: tests/e2e/fixture-deck.md
Base: main @ @BASE_SHA@
Branch: leadhand/calc-e2e
Build: python3 -m compileall -q calc
Test: python3 -m unittest discover -s tests

## Requirements
- R1 calc can subtract two numbers (plan: Card 1) → cards 1
- R2 calc can multiply two numbers (plan: Card 2) → cards 2

## Card 1: Add subtract()
- Covers: R1
- Role: implementer
- Hard: no
- Risk: low
- Depends on: none
- Source: e2e fixture, card 1
- Allowed paths: calc/, tests/
- Out of scope: any operation other than subtract; packaging
- Done when: tests/test_calc.py has a test for subtract() and it passes; python3 -m unittest discover -s tests passes
- Stop if: add() would have to change
- Route: claude sonnet low

Context: calc/__init__.py defines add(a, b). Add subtract(a, b), returning a - b, beside it, and a unittest test for it in tests/test_calc.py.

## Card 2: Add multiply()
- Covers: R2
- Role: implementer
- Hard: no
- Risk: low
- Depends on: 1
- Source: e2e fixture, card 2
- Allowed paths: calc/, tests/
- Out of scope: any operation other than multiply; packaging
- Done when: tests/test_calc.py has a test for multiply() and it passes; python3 -m unittest discover -s tests passes
- Stop if: add() or subtract() would have to change
- Route: codex gpt-6-luna medium

Context: calc/__init__.py defines add(a, b) and, after card 1, subtract(a, b). Add multiply(a, b), returning a * b, beside them, and a unittest test for it in tests/test_calc.py.
