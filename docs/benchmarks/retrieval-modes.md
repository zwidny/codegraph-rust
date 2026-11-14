# Retrieval modes and needle results (measured)

Two evaluations of the search stack, using `scripts/benchmarks/retrieval-eval.py`.
Both run on this repository's index (`codegraph-rust`, 31,600 chunks, 1024-dim
F2LLM embeddings) against the Postgres backend, and compare three rankings built
from the same candidate pool:

- **vector** — `chunks.embedding <=> query`
- **bm25** — `ts_rank_cd(chunks.content_tsv, websearch_to_tsquery('english', q))`
- **hybrid** — reciprocal rank fusion of the two lists (k=60), top 10

Relevant docs per query are the chunks of the target symbol (its `parent_node`).
Metrics are binary relevance: nDCG@10, MRR@10, Recall@10, and needle Hit@1/@5.

## Two query sets

| set | queries | query text | what it tests |
|---|---|---|---|
| identifier | 200 | the symbol name (`reconcile_project`) | "I know the name, find the code" |
| natural language | 127 | the symbol's doc comment (`Start HTTP server with CodeGraph MCP service`) | "describe it, find the code" |

## Results

Identifier queries favour lexical matching; hybrid wins.

| mode | nDCG@10 | MRR@10 | Recall@5 | Recall@10 |
|---|---|---|---|---|
| vector | 0.5257 | 0.4905 | 0.565 | 0.640 |
| bm25 | 0.6726 | 0.6276 | 0.755 | 0.815 |
| **hybrid** | **0.7381** | **0.7055** | **0.815** | **0.840** |

Natural-language queries favour the vector side; **unweighted RRF is worse than
vector alone**, because the code FTS list is near-random for a description.

| mode | nDCG@10 | MRR@10 | Recall@5 | Recall@10 |
|---|---|---|---|---|
| **vector** | **0.5222** | **0.4656** | **0.598** | **0.701** |
| bm25 | 0.0267 | 0.0223 | 0.039 | 0.039 |
| hybrid | 0.4238 | 0.3521 | 0.551 | 0.646 |

Needle (RepoQA-style: is the target symbol found first?):

| set | mode | Hit@1 | Hit@5 | MRR@10 |
|---|---|---|---|---|
| identifier | vector | 0.435 | 0.565 | 0.490 |
| identifier | bm25 | 0.545 | 0.755 | 0.628 |
| identifier | hybrid | **0.640** | **0.815** | **0.706** |
| natural language | vector | **0.346** | **0.598** | **0.466** |
| natural language | bm25 | 0.008 | 0.039 | 0.022 |
| natural language | hybrid | 0.189 | 0.551 | 0.352 |

## What this means

- The product's RRF design is right for identifier queries (hybrid beats both
  components).
- RRF is **not** a free win: when one list has near-zero recall (BM25 on a
  natural-language description), fusing it uniformly pulls the good vector
  results down (nDCG 0.52 -> 0.42). Weighted RRF, or a confidence gate that
  drops a list whose top score is below a floor, is the obvious next step.
- `P@10` is not reported as a headline: most queries have a single relevant
  chunk, so P@10 is bounded by 0.1 and only tracks recall.

## Caveats

- Doc comments are a proxy for natural-language intent, not human-labelled
  queries.
- Identifier queries trivially favour BM25; the point is the two sets disagree,
  not which is "true".
- Embeddings come from the configured local model; a different model can move
  the vector numbers.

## Reproduce

The evaluator is committed; the results files are built with a one-off harness
(sample symbol -> expected chunks; embed the query text; one SQL for
vector/BM25 top-50; RRF(60) in Python).

```bash
python3 scripts/benchmarks/retrieval-eval.py results.json
python3 scripts/benchmarks/retrieval-eval.py results.json --mode vector,hybrid
```
