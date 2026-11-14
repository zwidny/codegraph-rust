# Retrieval modes and needle results (measured) — with a correction

Two evaluations of the search stack, using `scripts/benchmarks/retrieval-eval.py`
on this repository's index (`codegraph-rust`, ~31,600 chunks, 1024-dim F2LLM
embeddings, Postgres backend). Three rankings from the same candidate pool:

- **vector** — `chunks.embedding <=> query`
- **text** — `ts_rank_cd(chunks.content_tsv, <tsquery>)`. **This is not BM25.**
  `ts_rank_cd` has no corpus IDF; the product's own code says so
  (`postgres_storage.rs`: "Ranking uses `ts_rank_cd`, not BM25").
- **hybrid** — reciprocal rank fusion of the two (k=60), top 10.

Relevant docs per query are the chunks of the target symbol (`parent_node`).

## The correction

An earlier version of this doc concluded "RRF helps identifier queries but hurts
natural-language queries". **That NL conclusion is retracted**: it was produced
by a text channel that could not rank anything.

`ts_rank_cd` only scores documents that satisfy the tsquery, and the tsquery
semantics swing the result entirely:

| query set | tsquery | text nDCG@10 | note |
|---|---|---|---|
| natural language | `websearch_to_tsquery` (AND) | 0.0267 | a 6-word description needs all 6 in one chunk — near-zero |
| natural language | `to_tsquery` of OR'd lexemes | 0.0023 | OR over generic words ("server", "http") matches thousands — also near-zero |
| identifier | AND | 0.7200 | short name, both terms usually present |
| identifier | OR | 0.1717 | OR lets a common token dominate — noise |

So there is no single "fair" `ts_rank_cd` baseline, and a fusion metric built on
top of a near-random list cannot tell us whether RRF helps. To evaluate the text
channel honestly it must be a **real IDF-weighted BM25** (compute document
frequency over the corpus), which `ts_rank_cd` does not provide.

## What the numbers do show (identifier queries, AND tsquery)

| mode | nDCG@10 | MRR@10 | Recall@10 | Hit@1 | Hit@5 |
|---|---|---|---|---|---|
| vector | 0.5257 | 0.4905 | 0.640 | 0.435 | 0.565 |
| text (ts_rank_cd, AND) | 0.6726 | 0.6276 | 0.815 | 0.545 | 0.755 |
| hybrid (RRF) | **0.7381** | **0.7055** | **0.840** | **0.640** | **0.815** |

For identifier queries the text channel is a strong ranker and RRF over it adds
over vector. This much is supported.

## Natural language (vector only is trustworthy here)

| mode | nDCG@10 | MRR@10 | Recall@10 | Hit@1 | Hit@5 |
|---|---|---|---|---|---|
| vector | 0.5222 | 0.4656 | 0.701 | 0.346 | 0.598 |
| text (ts_rank_cd) | ~0.00 | ~0.00 | ~0.01 | 0.00 | 0.04 |

The vector channel retrieves the described symbol about a third of the time at
rank 1 and six in ten within five. What the text channel does on descriptions is
**undefined until a real BM25 exists**; its column here only shows that
`ts_rank_cd` cannot serve as that baseline.

## Follow-ups this exposes

1. **Implement IDF-weighted BM25** in the text channel (or vendor a real BM25 in
   SQL), then re-run both query sets. Only then can vector-vs-hybrid be decided
   on natural language.
2. The MCP tool description said `text_score (BM25)` while the implementation is
   `ts_rank_cd`; the label is corrected. If BM25 is wanted, implement it, don't
   relabel.
3. `full_text_nodes` uses `to_tsquery('english', $1)` (AND) — multi-word
   natural-language queries hit the AND wall above.

## Caveats

- Doc comments are a proxy for natural-language intent, not human-labelled queries.
- Identifier queries trivially favour exact-token matching.
- Embeddings come from the configured local model; a different model moves the vector numbers.

## Reproduce

```bash
python3 scripts/benchmarks/retrieval-eval.py results.json
python3 scripts/benchmarks/retrieval-eval.py results.json --mode vector,hybrid
```
