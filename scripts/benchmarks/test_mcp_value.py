#!/usr/bin/env python3
"""Unit tests for the MCP value benchmark's pure helpers.

    python3 -m unittest discover -s scripts/benchmarks -p 'test_*.py'
"""
import json
import tempfile
import unittest
from pathlib import Path

import mcp_value


class Paths(unittest.TestCase):
    def test_relative_paths_compare_equal_across_arms(self):
        root = Path("/repo/proj")
        self.assertEqual(mcp_value.normalize_path("/repo/proj/src/a.rs", root), "src/a.rs")
        self.assertEqual(mcp_value.normalize_path("./src/a.rs", root), "src/a.rs")
        self.assertEqual(mcp_value.normalize_path("src/a.rs", root), "src/a.rs")

    def test_absolute_outside_root_is_preserved(self):
        self.assertEqual(
            mcp_value.normalize_path("/home/x/.cargo/registry/a.rs", Path("/repo/proj")),
            "/home/x/.cargo/registry/a.rs",
        )


class Keywords(unittest.TestCase):
    def test_extracts_content_words_longest_first(self):
        keywords = mcp_value.extract_keywords("The reconciler removes stale catalog records and rebinds callers")
        self.assertIn("reconciler", keywords)
        self.assertNotIn("the", keywords)
        self.assertEqual(keywords[0], "reconciler")

    def test_drops_short_words_and_dedupes(self):
        keywords = mcp_value.extract_keywords("cosine cosine cos normalized normal")
        self.assertEqual(len(keywords), len({w.lower() for w in keywords}))
        self.assertNotIn("cos", keywords)


class Ranking(unittest.TestCase):
    def test_hit_rank_is_one_based_and_zero_on_miss(self):
        self.assertEqual(mcp_value.hit_rank(["a.rs", "b.rs"], "b.rs"), 2)
        self.assertEqual(mcp_value.hit_rank(["a.rs"], "b.rs"), 0)
        self.assertEqual(mcp_value.hit_rank([], "b.rs"), 0)

    def test_hit_rank_ignores_positions_beyond_five_by_construction(self):
        self.assertEqual(mcp_value.hit_rank(["a", "b", "c", "d", "e", "f"], "f"), 6)


class Summary(unittest.TestCase):
    def test_summarize_averages_only_present_rows(self):
        per_task = [
            {"grep": {"rank": 1, "rounds": 2, "bytes": 100, "ms": 10}, "codegraph": None},
            {"grep": {"rank": 0, "rounds": 3, "bytes": 300, "ms": 30}, "codegraph": None},
        ]
        summary = mcp_value.summarize(per_task, "grep")
        self.assertEqual(summary["tasks"], 2)
        self.assertEqual(summary["hit@1"], 0.5)
        self.assertEqual(summary["hit@5"], 0.5)
        self.assertEqual(summary["miss"], 0.5)
        self.assertEqual(summary["bytes"], 200)

    def test_inactive_arm_reports_zero_tasks(self):
        self.assertEqual(mcp_value.summarize([{"grep": None}], "codegraph"), {"tasks": 0})


class Rendering(unittest.TestCase):
    def test_markdown_lists_both_arms_and_escapes_pipes(self):
        report = {
            "project": "/repo/proj",
            "tasks": 1,
            "generated_from": "index",
            "limit": 10,
            "max_rounds": 3,
            "arms": {
                "grep": {"tasks": 1, "hit@1": 0.0, "hit@5": 1.0, "miss": 0.0, "rounds": 2, "bytes": 2048, "ms": 100},
                "codegraph": {"tasks": 1, "hit@1": 1.0, "hit@5": 1.0, "miss": 0.0, "rounds": 1, "bytes": 4096, "ms": 900},
            },
            "per_task": [{
                "id": "t01", "kind": "symbol", "question": "Where is `a | b` defined?", "truth": "src/a.rs",
                "grep": {"rank": 2, "rounds": 2, "bytes": 2048, "ms": 100},
                "codegraph": {"rank": 1, "rounds": 1, "bytes": 4096, "ms": 900},
            }],
        }
        text = mcp_value.render_markdown(report)
        self.assertIn("| grep |", text)
        self.assertIn("| codegraph |", text)
        self.assertIn("a \\| b", text)
        self.assertIn("src/a.rs", text)


class Overlap(unittest.TestCase):
    def test_recall_and_precision_of_caller_files(self):
        self.assertEqual(mcp_value.overlap(["a.rs", "b.rs"], {"a.rs"}), (1.0, 0.5))
        self.assertEqual(mcp_value.overlap(["a.rs"], {"a.rs", "b.rs"}), (0.5, 1.0))

    def test_empty_inputs_report_zero(self):
        self.assertEqual(mcp_value.overlap([], {"a.rs"}), (0.0, 0.0))
        self.assertEqual(mcp_value.overlap(["a.rs"], set()), (0.0, 0.0))


class ImpactSummary(unittest.TestCase):
    def test_averages_recall_precision_and_complete_rate(self):
        per_task = [
            {"grep": {"recall": 1.0, "precision": 1.0, "rounds": 1, "bytes": 10, "ms": 5}},
            {"grep": {"recall": 0.5, "precision": 0.25, "rounds": 1, "bytes": 30, "ms": 15}},
        ]
        summary = mcp_value.summarize_impact(per_task, "grep")
        self.assertAlmostEqual(summary["recall"], 0.75)
        self.assertAlmostEqual(summary["precision"], 0.625)
        self.assertAlmostEqual(summary["complete"], 0.5)
        self.assertEqual(summary["bytes"], 20)
        self.assertEqual(mcp_value.summarize_impact(per_task, "codegraph"), {"tasks": 0})


class GrepOutput(unittest.TestCase):
    def test_parses_path_line_pairs(self):
        output = "./crates/a.rs:12:fn alpha() {}\n./crates/b.rs:7:let x = 1;\n"
        self.assertEqual(
            mcp_value.parse_hits(output),
            [("./crates/a.rs", 12), ("./crates/b.rs", 7)],
        )

    def test_ignores_rows_without_a_line_number(self):
        self.assertEqual(mcp_value.parse_hits("binary file matches\n"), [])

    def test_keeps_first_occurrence_order_and_first_line(self):
        hits = [("./a.rs", 30), ("./b.rs", 5), ("./a.rs", 9)]
        self.assertEqual(mcp_value.ordered_files(hits), ["./a.rs", "./b.rs"])
        self.assertEqual(mcp_value.first_lines(hits), {"./a.rs": 30, "./b.rs": 5})


class WindowBytes(unittest.TestCase):
    def test_reads_only_the_window_around_the_hit(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "a.rs"
            path.write_text("".join(f"line {i}\n" for i in range(1, 201)))
            near = mcp_value.window_bytes(path, 100, window=10)
            whole = path.stat().st_size
            self.assertGreater(near, 0)
            self.assertLess(near, whole / 4)

    def test_window_clamps_at_file_boundaries(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "a.rs"
            path.write_text("only line\n")
            self.assertEqual(mcp_value.window_bytes(path, 1, window=40), len("only line\n"))
            self.assertEqual(mcp_value.window_bytes(path, 999, window=40), len("only line\n"))

    def test_missing_file_costs_nothing(self):
        self.assertEqual(mcp_value.window_bytes(Path("/nonexistent/a.rs"), 1, 5), 0)


class Median(unittest.TestCase):
    def test_odd_and_even_sizes(self):
        self.assertEqual(mcp_value.median([3, 1, 2]), 2.0)
        self.assertEqual(mcp_value.median([4, 1, 3, 2]), 2.5)
        self.assertEqual(mcp_value.median([]), 0.0)

    def test_summary_exposes_median_next_to_mean(self):
        per_task = [
            {"grep": {"rank": 1, "rounds": 1, "bytes": 100, "ms": 1}},
            {"grep": {"rank": 1, "rounds": 1, "bytes": 900, "ms": 1}},
        ]
        summary = mcp_value.summarize(per_task, "grep")
        self.assertEqual(summary["bytes"], 500)
        self.assertEqual(summary["bytes_median"], 500)


class CalledNames(unittest.TestCase):
    def test_extracts_call_sites_and_skips_control_words(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "a.rs"
            path.write_text("fn main() { helper(x); if cond { other_one(); } }")
            names = mcp_value.called_names([path], limit=10)
            self.assertIn("helper", names)
            self.assertIn("other_one", names)
            self.assertNotIn("if", names)

    def test_honours_the_limit_and_dedupes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "a.rs"
            path.write_text("a_one(); a_one(); b_two(); c_three(); d_four();")
            self.assertEqual(len(mcp_value.called_names([path], limit=2)), 2)

    def test_missing_file_is_ignored(self):
        self.assertEqual(mcp_value.called_names([Path("/nonexistent/x.rs")], limit=5), [])


class ConceptComments(unittest.TestCase):
    def test_accepts_prose_comment_and_strips_aboutme(self):
        self.assertEqual(
            mcp_value.natural_comment(
                "fn a() {}\n// ABOUTME: Reconciles the catalog per file and rebinds callers."
            ),
            "Reconciles the catalog per file and rebinds callers.",
        )

    def test_rejects_rust_attributes_and_code_lines(self):
        self.assertEqual(mcp_value.natural_comment('#[serde(rename = "protocolVersion")]\nfn a() {}'), "")
        self.assertEqual(mcp_value.natural_comment("// fn helper(a: u32) -> bool {"), "")

    def test_rejects_compressed_payloads(self):
        self.assertEqual(mcp_value.natural_comment("zstd:KLUv/QBYrSAAtreaJwDRpgO8brbJyWgiG"), "")

    def test_accepts_python_comment(self):
        self.assertEqual(
            mcp_value.natural_comment("# Keep a developer's own config file out of the test.\nx = 1"),
            "Keep a developer's own config file out of the test.",
        )


class EnvLayers(unittest.TestCase):
    def test_project_env_overrides_home_file(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as project:
            home_file = Path(home) / ".codegraph.env"
            home_file.write_text("A=home\nB=home\n")
            project_file = Path(project) / ".env"
            project_file.write_text("B=project\n")
            old = Path.home
            Path.home = staticmethod(lambda: Path(home))
            try:
                merged = mcp_value.load_env_layers(project)
            finally:
                Path.home = old
            self.assertEqual(merged["A"], "home")
            self.assertEqual(merged["B"], "project")

    def test_comments_and_quotes_are_stripped(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / ".env"
            path.write_text('# note\nKEY="value"  # trailing\nURL=ws://x\n')
            values = mcp_value.read_env(path)
            self.assertEqual(values["KEY"], "value")
            self.assertEqual(values["URL"], "ws://x")


class Tasks(unittest.TestCase):
    def test_task_file_round_trips(self):
        tasks = [{"id": "t01", "kind": "symbol", "question": "q", "keywords": ["k"], "truth": "a.rs"}]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tasks.json"
            path.write_text(json.dumps(tasks))
            self.assertEqual(json.loads(path.read_text()), tasks)


if __name__ == "__main__":
    unittest.main()
