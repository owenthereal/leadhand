"""Claude usage is priced per model (final review I9): a batch that mixes models keeps a
sub-total per model, so it is priced as such and never silently unpriced."""
import json
import shutil
import tempfile
import unittest
from pathlib import Path

from leadhand import usage
from leadhand.state import new_run_dir, run_paths
from leadhand.usage import Usage, add, estimate_cost

PRICES = {
    "opus": {"input": 15.0, "output": 75.0, "cache_read": 1.5, "cache_write_5m": 18.75, "cache_write_1h": 30.0},
    "haiku": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write_5m": 1.25, "cache_write_1h": 2.0},
}
OPUS = Usage(input=1000, output=1000, model="claude-opus-4")
HAIKU = Usage(input=1000, output=1000, model="claude-haiku-4")


def one(u, model):
    return estimate_cost(u, model, PRICES)


class PerModel(unittest.TestCase):
    def test_mixed_batch_is_priced_per_model(self):
        total = Usage()
        for u in (OPUS, HAIKU, HAIKU):
            total = add(total, u)
        self.assertEqual((total.input, total.output), (3000, 3000))
        expected = one(OPUS, "claude-opus-4") + 2 * one(HAIKU, "claude-haiku-4")
        self.assertAlmostEqual(estimate_cost(total, total.model, PRICES), expected)
        # The order does not matter, and the passed model does not override the parts.
        other = add(add(HAIKU, OPUS), HAIKU)
        self.assertAlmostEqual(estimate_cost(other, "claude-opus-4", PRICES), expected)

    def test_single_model_and_empty_synthetic_records_stay_single(self):
        synthetic = Usage(model="<synthetic>")
        total = add(add(OPUS, synthetic), OPUS)
        self.assertEqual(total.model, "claude-opus-4")
        self.assertAlmostEqual(estimate_cost(total, total.model, PRICES), 2 * one(OPUS, "claude-opus-4"))

    def test_unpriced_part_makes_the_estimate_unknown(self):
        total = add(OPUS, Usage(input=10, output=10, model="some-other-model"))
        self.assertIsNone(estimate_cost(total, total.model, PRICES))

    def test_charge_keeps_the_per_model_parts(self):
        tmp = Path(tempfile.mkdtemp(prefix="lh usage "))
        self.addCleanup(shutil.rmtree, tmp, True)
        root = new_run_dir(tmp / "docs", "demo", __import__("datetime").date(2026, 11, 10))
        paths = run_paths(root)
        transcript = tmp / "agent.jsonl"
        lines = []
        for i, model in enumerate(("claude-opus-4", "claude-haiku-4", "claude-haiku-4")):
            lines.append({"type": "assistant", "message": {"id": f"m{i}", "model": model, "usage": {
                "input_tokens": 1000, "output_tokens": 1000, "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0}}})
        transcript.write_text("".join(json.dumps(x) + "\n" for x in lines))
        usage.charge(paths, charge_id="c1", source="claude", key="agent", dispatch_id="d1", role="implementer",
                     path_or_thread=transcript)
        (c,) = usage.read_charges(paths)
        expected = one(OPUS, "claude-opus-4") + 2 * one(HAIKU, "claude-haiku-4")
        self.assertAlmostEqual(estimate_cost(c.usage, c.usage.model, PRICES), expected)


if __name__ == "__main__":
    unittest.main()
