import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

_spec = importlib.util.spec_from_file_location("index_quality", Path(__file__).with_name("index-quality.py"))
index_quality = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(index_quality)


def good_stats(**over):
    base = {
        "files": 10,
        "nodes": 100,
        "edges": 200,
        "resolved_edges": 150,
        "unresolved_edges": 50,
        "resolution_rate": 75.0,
        "chunks": 100,
        "embedding_dimension": 1024,
        "lsp_nodes_enriched": 10,
        "lsp_edges_resolved": 20,
        "index_ms": 1000,
        "startup_ms": 10,
        "errors": 0,
        "graph_complete": True,
        "vector_index_ready": True,
        "complete": False,
        "semantic_status": "pending",
        "embedding_status": "ready",
    }
    base.update(over)
    return base


LOG = """SCIP import complete: 1 indexes, 3842 nodes enriched + 13581 edges resolved in 1.0s
SCIP coverage: 220/265 files; 45 remain AST-only
Resolved 106603 of 149978 relationships (exact/contextual=100983, lexical=5620, semantic=0) in 5s
"""


class IndexQualityTests(unittest.TestCase):
    def args(self, min_exact=0.95):
        return SimpleNamespace(min_exact_share=min_exact)

    def test_parse_log_extracts_split_and_scip(self):
        with tempfile.NamedTemporaryFile("w", suffix=".log", delete=False) as f:
            f.write(LOG)
            path = Path(f.name)
        log = index_quality.parse_log(path)
        self.assertEqual((log["exact"], log["lexical"], log["semantic"]), (100983, 5620, 0))
        self.assertEqual((log["scip_files"], log["scip_total_files"], log["scip_ast_only"]), (220, 265, 45))
        self.assertEqual((log["scip_nodes_enriched"], log["scip_edges_resolved"]), (3842, 13581))

    def test_derived_exact_share_and_per_file(self):
        d = index_quality.derived(good_stats(), {"exact": 990, "lexical": 10, "semantic": 0})
        self.assertAlmostEqual(d["exact_share"], 0.99, places=4)
        self.assertEqual(d["nodes_per_file"], 10.0)

    def test_check_passes_for_healthy_index(self):
        stats = good_stats()
        log = {"exact": 990, "lexical": 10, "semantic": 0}
        d = index_quality.derived(stats, log)
        self.assertEqual(index_quality.check(stats, d, log, self.args()), [])

    def test_check_flags_unready_vector_index(self):
        stats = good_stats(vector_index_ready=False)
        problems = index_quality.check(stats, index_quality.derived(stats, {}), {}, self.args())
        self.assertTrue(any("vector_index_ready" in p for p in problems))

    def test_check_flags_errors(self):
        stats = good_stats(errors=3)
        problems = index_quality.check(stats, index_quality.derived(stats, {}), {}, self.args())
        self.assertTrue(any("errors" in p for p in problems))

    def test_check_flags_low_exact_share(self):
        stats = good_stats()
        log = {"exact": 500, "lexical": 500, "semantic": 0}
        d = index_quality.derived(stats, log)
        problems = index_quality.check(stats, d, log, self.args(min_exact=0.95))
        self.assertTrue(any("exact/contextual share" in p for p in problems))

    def test_complete_false_with_deferred_semantic_is_not_a_failure(self):
        stats = good_stats(complete=False, semantic_status="pending")
        problems = index_quality.check(stats, index_quality.derived(stats, {}), {}, self.args())
        self.assertEqual(problems, [])

    def test_cli_exit_codes(self):
        script = Path(__file__).with_name("index-quality.py")
        with tempfile.TemporaryDirectory() as tmp:
            ok = Path(tmp, "ok.json")
            ok.write_text(json.dumps(good_stats()))
            bad = Path(tmp, "bad.json")
            bad.write_text(json.dumps(good_stats(errors=1)))
            self.assertEqual(subprocess.run([sys.executable, str(script), str(ok), "--check"]).returncode, 0)
            self.assertEqual(subprocess.run([sys.executable, str(script), str(bad), "--check"]).returncode, 1)


if __name__ == "__main__":
    unittest.main()
