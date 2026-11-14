#!/usr/bin/env python3
"""L1 index-quality report from `codegraph index --stats-json` files.

Reads one or more IndexStats JSON files and prints a normalized metric table,
optionally parses the matching index log for the resolution split
(exact/contextual, lexical, semantic) and SCIP coverage, optionally diffs
against a baseline, and can fail (non-zero exit) when hard checks regress.

Why: coverage and a single resolution rate are not a quality score. The
denominator includes external/stdlib/dynamic targets, so the report separates
the absolute rate from the parts that are meaningful (exact/contextual share,
SCIP file coverage, readiness flags, cost).

Usage:
  scripts/benchmarks/index-quality.py stats.json
  scripts/benchmarks/index-quality.py a.json b.json
  scripts/benchmarks/index-quality.py stats.json --log index.log
  scripts/benchmarks/index-quality.py candidate.json --baseline baseline.json
  scripts/benchmarks/index-quality.py stats.json --check
  scripts/benchmarks/index-quality.py stats.json --json

Hard checks (with --check): graph_complete, vector_index_ready (when chunks > 0),
complete, and no index errors. Resolution rate is reported but NOT thresholded
per language; only the exact/contextual share (when a log is given) must stay
high, because a high lexical/semantic share means name resolution is guessing.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# Resolution split comes from a log line, not the stats json.
RE_RESOLVED = re.compile(
    r"Resolved (\d+) of (\d+) relationships \(exact/contextual=(\d+), lexical=(\d+), semantic=(\d+)\)"
)
RE_SCIP_COVERAGE = re.compile(r"SCIP coverage: (\d+)/(\d+) files; (\d+) remain AST-only")
RE_SCIP_IMPORT = re.compile(r"SCIP import complete:.*?(\d+) nodes enriched \+ (\d+) edges resolved")

CORE = [
    "files",
    "nodes",
    "edges",
    "resolved_edges",
    "unresolved_edges",
    "resolution_rate",
    "chunks",
    "embedding_dimension",
    "lsp_nodes_enriched",
    "lsp_edges_resolved",
    "index_ms",
    "startup_ms",
    "errors",
]
FLAGS = ["graph_complete", "vector_index_ready", "complete"]
PHASES = [
    "analyzers",
    "ast_parse",
    "embeddings_and_writes",
    "durable_reconcile_and_indexes",
    "resolution_exact_lexical",
    "resolution_fuzzy_ms",
]


def load(path: Path) -> dict:
    return json.loads(path.read_text())


def parse_log(path: Path) -> dict:
    text = path.read_text(errors="replace")
    out: dict = {}
    if m := RE_RESOLVED.search(text):
        out["resolved_total"], out["attempted_total"] = int(m.group(1)), int(m.group(2))
        out["exact"], out["lexical"], out["semantic"] = (int(m.group(i)) for i in (3, 4, 5))
    if m := RE_SCIP_COVERAGE.search(text):
        out["scip_files"], out["scip_total_files"], out["scip_ast_only"] = (
            int(m.group(1)),
            int(m.group(2)),
            int(m.group(3)),
        )
    if m := RE_SCIP_IMPORT.search(text):
        out["scip_nodes_enriched"], out["scip_edges_resolved"] = int(m.group(1)), int(m.group(2))
    return out


def derived(stats: dict, log: dict) -> dict:
    d = {}
    rate = stats.get("resolution_rate")
    d["resolution_rate"] = round(float(rate), 2) if isinstance(rate, (int, float)) else None
    nodes, edges = stats.get("nodes") or 0, stats.get("edges") or 0
    files = stats.get("files") or 0
    d["nodes_per_file"] = round(nodes / files, 1) if files else None
    d["edges_per_node"] = round(edges / nodes, 2) if nodes else None
    exact = log.get("exact")
    lex = log.get("lexical")
    sem = log.get("semantic")
    if exact is not None:
        resolved = exact + (lex or 0) + (sem or 0)
        d["exact_share"] = round(exact / resolved, 4) if resolved else None
        d["lexical_share"] = round((lex or 0) / resolved, 4) if resolved else None
        d["semantic_share"] = round((sem or 0) / resolved, 4) if resolved else None
    if log.get("scip_total_files"):
        d["scip_file_coverage"] = round(
            log["scip_files"] / log["scip_total_files"], 4
        )
    return d


def check(stats: dict, d: dict, log: dict, args) -> list[str]:
    problems = []
    if not stats.get("graph_complete", False):
        problems.append("graph_complete is false")
    if (stats.get("chunks") or 0) > 0:
        if not stats.get("vector_index_ready", False):
            problems.append("vector_index_ready is false but chunks were written")
        if stats.get("embedding_status") not in (None, "ready"):
            problems.append(f"embedding_status={stats.get('embedding_status')} (not ready)")
    if (stats.get("errors") or 0) > 0:
        problems.append(f"{stats['errors']} index errors")
    # `complete` also folds in deferred semantic inference: with
    # CODEGRAPH_SEMANTIC_RESOLUTION=deferred it stays false by design, so only
    # fail when semantic resolution was expected to have finished.
    if stats.get("complete") is False and stats.get("semantic_status") not in (
        "pending",
        "deferred",
    ):
        problems.append("complete is false")
    if log.get("exact") is not None:
        share = d.get("exact_share")
        if share is not None and share < args.min_exact_share:
            problems.append(
                f"exact/contextual share {share:.3f} < {args.min_exact_share} (name resolution is guessing)"
            )
    return problems


def fmt_row(name: str, stats: dict, d: dict) -> str:
    r = d.get("resolution_rate")
    parts = [
        f"[{name}]",
        f"files={stats.get('files', '?')}",
        f"nodes={stats.get('nodes', '?')}",
        f"edges={stats.get('edges', '?')}",
        f"rate={r if r is not None else '?'}%",
    ]
    if d.get("exact_share") is not None:
        parts.append(f"exact={d['exact_share'] * 100:.1f}%")
    parts += [
        f"chunks={stats.get('chunks', '?')}",
        f"scip_nodes={stats.get('lsp_nodes_enriched', '?')}",
        f"scip_edges={stats.get('lsp_edges_resolved', '?')}",
        f"graph_ok={stats.get('graph_complete')}",
        f"vec_ready={stats.get('vector_index_ready')}",
        f"semantic={stats.get('semantic_status', '?')}",
        f"embedding={stats.get('embedding_status', '?')}",
        f"index_ms={stats.get('index_ms', '?')}",
    ]
    return "  ".join(parts)


def report(path: Path, args) -> tuple[dict, dict, list[str]]:
    stats = load(path)
    log = parse_log(Path(args.log)) if args.log else {}
    d = derived(stats, log)
    problems = check(stats, d, log, args) if args.check else []
    return stats, d, problems


def diff(cand: dict, base: dict, keys: list[str]) -> list[str]:
    lines = []
    for k in keys:
        a, b = base.get(k), cand.get(k)
        if isinstance(a, (int, float)) and isinstance(b, (int, float)) and a != b:
            lines.append(f"    {k}: {b} -> {a}  (delta {a - b:+g})")
    return lines


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("files", nargs="+", type=Path)
    p.add_argument("--log", type=Path, help="index log to parse the resolution split/SCIP coverage")
    p.add_argument("--baseline", type=Path, help="stats json to diff against")
    p.add_argument("--json", action="store_true")
    p.add_argument("--check", action="store_true", help="fail on regressions/hard checks")
    p.add_argument("--min-exact-share", type=float, default=0.95)
    args = p.parse_args()

    results = []
    failed = False
    for path in args.files:
        stats, d, problems = report(path, args)
        results.append({"path": str(path), "stats": stats, "derived": d, "problems": problems})
        if problems:
            failed = True

    if args.json:
        print(json.dumps(results, indent=2))
    else:
        for r in results:
            print(fmt_row(r["path"], r["stats"], r["derived"]))
            phase = r["stats"].get("phase_ms") or {}
            shown = " ".join(f"{k}={phase[k]}ms" for k in PHASES if k in phase)
            if shown:
                print(f"    phases: {shown}")
            if r["problems"]:
                for pr in r["problems"]:
                    print(f"    CHECK FAIL: {pr}")
            else:
                if args.check:
                    print("    checks: PASS")

        if args.baseline:
            base = load(args.baseline)
            keys = CORE  # already includes resolution_rate
            for r in results:
                print(f"diff vs {args.baseline}: {r['path']}")
                for line in diff(r["stats"], base, keys):
                    print(line)

    return 1 if (args.check and failed) else 0


if __name__ == "__main__":
    sys.exit(main())
