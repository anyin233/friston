"""Unit tests for scripts/eval-captions.py metrics (no engine or `say` required)."""

import importlib.util
import sys
import unittest
from pathlib import Path

_path = Path(__file__).resolve().parents[1] / "eval-captions.py"
_spec = importlib.util.spec_from_file_location("eval_captions", _path)
ec = importlib.util.module_from_spec(_spec)
sys.modules["eval_captions"] = ec  # dataclasses resolve annotations through sys.modules
_spec.loader.exec_module(ec)


class TextTests(unittest.TestCase):
    """CER and caption matching ignore punctuation and tolerate one wrong character."""

    def test_cer_ignores_punctuation(self):
        self.assertEqual(ec.cer("大家好，我们开始。", "大家好我们开始"), 0.0)
        self.assertAlmostEqual(ec.cer("退款接口联调", "退款接口连调"), 1 / 6)

    def test_shows_allows_one_homophone(self):
        self.assertTrue(ec.shows("很好，那退款接口连调的时候", "联调的"))
        self.assertFalse(ec.shows("很好", "联调的"))


class MeasureTests(unittest.TestCase):
    """Latencies are measured from the reference timeline to caption arrival."""

    def test_latencies_and_partial_interval(self):
        refs = [ec.RefSentence("A", "大家好我们开始开会", 1000, 3000)]
        events = [
            ec.Event("partial", 1700, 1, 1000, "大家好"),
            ec.Event("partial", 2000, 1, 1000, "大家好我们开始"),
            ec.Event("partial", 3100, 1, 1000, "大家好我们开始开会"),
            ec.Event("final", 4200, 1, 1000, "大家好，我们开始开会。", 3000),
        ]
        r = ec.measure(refs, events)
        self.assertEqual(r.first_caption_ms, [700])
        self.assertEqual(r.follow_lag_ms, [100])
        self.assertEqual(r.final_latency_ms, [1200])
        self.assertEqual(r.partial_interval_ms, [300, 1100])
        self.assertEqual(r.cer, 0.0)

    def test_missing_captions_are_counted(self):
        refs = [ec.RefSentence("A", "完全没有识别出来", 0, 2000)]
        r = ec.measure(refs, [])
        self.assertEqual((r.missed_first, r.missed_final), (1, 1))


if __name__ == "__main__":
    unittest.main()
