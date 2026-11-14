#!/usr/bin/env python3
"""L2 retrieval metrics from ranked results: nDCG@10, Recall@k, MRR, Precision.

Input is a results JSON with per-query, per-mode ranked document ids and the set
of relevant ("expected") ids. This is search-engine-side evaluation only: it
does not know how the ranking was produced, so the same file compares vector,
BM25, hybrid RRF or any candidate reranker.

Shape:
  {
    "meta": {"project": "...", "k": 10},
    "queries": [
      {"id": "q1", "expected": ["docA", "docB"],
       "ranked": {"vector": ["docA", "docZ", ...], "bm25": [...], "hybrid": [...]}}
    ]
  }

Metrics (binary relevance):
  nDCG@10   DCG/IDCG, rel=1, ideal = min(|expected|, k) relevant docs at the top
  Recall@k  |ranked[:k] ∩ expected| / |expected|   (k = 5, 10 by default)
  MRR@10    1/rank of the first relevant doc in the top 10, else 0
  P@10      |ranked[:10] ∩ expected| / 10
  Hit@10    1 if any relevant doc in the top 10

Usage:
  scripts/benchmarks/retrieval-eval.py results.json
  scripts/benchmarks/retrieval-eval.py results.json --mode hybrid --json
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path


def dcg(rels: list[int]) -> float:
    return sum(rel / math.log2(i + 2) for i, rel in enumerate(rels))


def ndcg_at(ranked: list[str], expected: set[str], k: int) -> float:
    rels = [1 if doc in expected else 0 for doc in ranked[:k]]
    ideal = [1] * min(len(expected), k)
    idcg = dcg(ideal)
    return dcg(rels) / idcg if idcg > 0 else 0.0


def recall_at(ranked: list[str], expected: set[str], k: int) -> float:
    if not expected:
        return 0.0
    hits = sum(1 for doc in ranked[:k] if doc in expected)
    return hits / len(expected)


def mrr_at(ranked: list[str], expected: set[str], k: int) -> float:
    for i, doc in enumerate(ranked[:k], start=1):
        if doc in expected:
            return 1.0 / i
    return 0.0


def precision_at(ranked: list[str], expected: set[str], k: int) -> float:
    hits = sum(1 for doc in ranked[:k] if doc in expected)
    return hits / k if k else 0.0


def evaluate(results: dict, k: int, recall_ks: list[int]) -> dict:
    modes: dict[str, list[dict]] = {}
    for q in results["queries"]:
        expected = set(q.get("expected") or [])
        if not expected:
            continue
        for mode, ranked in (q.get("ranked") or {}).items():
            modes.setdefault(mode, []).append(
                {
                    "ndcg@10": ndcg_at(ranked, expected, 10),
                    "mrr@10": mrr_at(ranked, expected, 10),
                    "p@10": precision_at(ranked, expected, 10),
                    **{f"recall@{rk}": recall_at(ranked, expected, rk) for rk in recall_ks},
                }
            )

    summary = {}
    for mode, rows in modes.items():
        n = len(rows)
        summary[mode] = {
            "queries": n,
            "ndcg@10": round(sum(r["ndcg@10"] for r in rows) / n, 4),
            "mrr@10": round(sum(r["mrr@10"] for r in rows) / n, 4),
            "p@10": round(sum(r["p@10"] for r in rows) / n, 4),
            **{
                f"recall@{rk}": round(sum(r[f"recall@{rk}"] for r in rows) / n, 4)
                for rk in recall_ks
            },
        }
    return summary


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("results", type=Path)
    p.add_argument("--k", type=int, default=10, help="top-k for precision/nDCG (default 10)")
    p.add_argument("--recall-ks", default="5,10")
    p.add_argument("--mode", help="only print these modes (comma separated)")
    p.add_argument("--json", action="store_true")
    args = p.parse_args()

    results = json.loads(args.results.read_text())
    recall_ks = [int(x) for x in args.recall_ks.split(",") if x.strip()]
    summary = evaluate(results, args.k, recall_ks)

    if args.mode:
        keep = {m.strip() for m in args.mode.split(",")}
        summary = {m: v for m, v in summary.items() if m in keep}

    if args.json:
        print(json.dumps(summary, indent=2))
        return 0

    if not summary:
        print("no queries with expected ids", file=sys.stderr)
        return 1
    cols = ["ndcg@10", "mrr@10", "p@10"] + [f"recall@{rk}" for rk in recall_ks]
    header = f"{'mode':10} {'q':>4} " + " ".join(f"{c:>9}" for c in cols)
    print(header)
    for mode, s in summary.items():
        row = f"{mode:10} {s['queries']:>4} " + " ".join(f"{s[c]:>9.4f}" for c in cols)
        print(row)
    return 0


if __name__ == "__main__":
    sys.exit(main())
