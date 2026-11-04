#!/usr/bin/env python3
"""Unit tests for the agent-value benchmark's pure helpers.

    python3 -m unittest discover -s scripts/benchmarks -p 'test_*.py'
"""
import unittest

import agent_value as av


class ParseAction(unittest.TestCase):
    def test_plain_object(self):
        action = av.parse_action('{"thought":"t","tool":"read_file","args":{"path":"a.rs"}}')
        self.assertEqual(action["tool"], "read_file")
        self.assertEqual(action["args"]["path"], "a.rs")

    def test_prose_around_the_object(self):
        reply = 'Sure, here is my plan.\n{"thought":"t","tool":"grep","args":{"pattern":"x"}}\nDone.'
        self.assertEqual(av.parse_action(reply)["tool"], "grep")

    def test_last_object_wins(self):
        reply = ('{"tool":"read_file","args":{"path":"a"}} then '
                 '{"tool":"done","args":{"summary":"ok"}}')
        action = av.parse_action(reply)
        self.assertEqual(action["tool"], "done")

    def test_object_without_tool_is_ignored(self):
        self.assertIsNone(av.parse_action('{"thought":"only thinking"}'))

    def test_garbage_and_empty(self):
        self.assertIsNone(av.parse_action(""))
        self.assertIsNone(av.parse_action(None))
        self.assertIsNone(av.parse_action("no json at all"))
        self.assertIsNone(av.parse_action("{broken json"))

    def test_nested_object_does_not_confuse_the_scanner(self):
        reply = '{"thought":"{\\"nested\\": 1}","tool":"done","args":{"summary":"s"}}'
        self.assertEqual(av.parse_action(reply)["tool"], "done")


class Scoring(unittest.TestCase):
    def test_exact_match_is_solved(self):
        score = av.score(["a.rs"], ["a.rs"])
        self.assertTrue(score["solved"])
        self.assertEqual((score["precision"], score["recall"]), (1.0, 1.0))

    def test_partial_match(self):
        score = av.score(["a.rs", "b.rs"], ["a.rs", "c.rs"])
        self.assertFalse(score["solved"])
        self.assertAlmostEqual(score["precision"], 0.5)
        self.assertAlmostEqual(score["recall"], 0.5)

    def test_no_edits_scores_zero(self):
        score = av.score([], ["a.rs"])
        self.assertEqual((score["precision"], score["recall"]), (0.0, 0.0))
        self.assertFalse(score["solved"])

    def test_empty_golden_does_not_divide_by_zero(self):
        score = av.score(["a.rs"], [])
        self.assertEqual(score["recall"], 0.0)

    def test_duplicate_edits_are_deduped(self):
        score = av.score(["a.rs", "a.rs"], ["a.rs"])
        self.assertTrue(score["solved"])


class Summary(unittest.TestCase):
    def test_averages_over_present_arms(self):
        runs = [
            {"baseline": {"score": {"solved": True, "recall": 1.0, "precision": 1.0},
                          "run": {"steps": [1, 2], "elapsed": 10.0}, "tokens": 100},
             "codegraph": None},
            {"baseline": {"score": {"solved": False, "recall": 0.0, "precision": 0.0},
                          "run": {"steps": [1, 2, 3, 4], "elapsed": 30.0}, "tokens": 300},
             "codegraph": None},
        ]
        summary = av.summarize(runs, "baseline")
        self.assertEqual(summary["tasks"], 2)
        self.assertAlmostEqual(summary["solved"], 0.5)
        self.assertAlmostEqual(summary["steps"], 3.0)
        self.assertEqual(summary["tokens"], 200)
        self.assertAlmostEqual(summary["seconds"], 20.0)

    def test_missing_arm(self):
        self.assertEqual(av.summarize([{"baseline": None}], "codegraph"), {"tasks": 0})


class Rendering(unittest.TestCase):
    def test_markdown_contains_both_arms(self):
        report = {
            "project": "/repo",
            "tasks": 1,
            "max_steps": 8,
            "model": "m",
            "arms": {
                "baseline": {"tasks": 1, "solved": 0.0, "recall": 0.0, "precision": 0.0,
                             "steps": 5, "tokens": 100, "seconds": 10},
                "codegraph": {"tasks": 1, "solved": 1.0, "recall": 1.0, "precision": 1.0,
                              "steps": 4, "tokens": 120, "seconds": 12},
            },
            "per_task": [{
                "id": "t01", "sha": "abcdef1234", "subject": "fix: a | b",
                "golden": ["a.rs"],
                "baseline": {"score": {"hit": 0, "golden": ["a.rs"]}, "run": {"steps": [1]}},
                "codegraph": {"score": {"hit": 1, "golden": ["a.rs"]}, "run": {"steps": [1, 2]}},
            }],
        }
        text = av.render_markdown(report)
        self.assertIn("| baseline |", text)
        self.assertIn("| codegraph |", text)
        self.assertIn("0/1 files", text)
        self.assertIn("1/1 files", text)


class CodeFileFilter(unittest.TestCase):
    def test_only_code_suffixes_become_tasks(self):
        self.assertTrue(av._is_code("crates/x/src/lib.rs"))
        self.assertTrue(av._is_code("app/main.py"))
        self.assertFalse(av._is_code("README.md"))
        self.assertFalse(av._is_code("Cargo.lock"))
        self.assertFalse(av._is_code("schema/x.surql"))


if __name__ == "__main__":
    unittest.main()
