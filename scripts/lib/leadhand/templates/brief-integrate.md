# Brief: integrate the card with the run branch

The run branch moved since this card was cut, and replaying the card onto it
conflicts. Make the card branch build on the current run tip, keeping both
sides' intent.

- Conflicting paths: $conflicts
- The card's intent: $card_intent
- The run branch's intent: $run_intent

Resolve only what the conflict needs, inside the allowed paths; do not change
the run branch itself. Run the tests, commit on the card branch, then write
your report to the report file.

$context
