# Brief: integrate the card with the run branch

The run branch moved since this card was cut, and replaying the card onto it
conflicts. The card branch is checked out as it was, not yet rebased.

- Run tip to rebase onto: $run_tip
- Conflicting paths: $conflicts
- The card's intent: $card_intent
- The run branch's intent: $run_intent

This brief explicitly permits one history rewrite: rebase the card branch (and
only the card branch) onto the run tip above, resolving each conflict so that
both sides' intent holds. Resolve only what the conflicts need, inside the
allowed paths; never change the run branch itself. Then run the tests again,
make sure everything is committed on the card branch, and write your report to
the report file.

$context
