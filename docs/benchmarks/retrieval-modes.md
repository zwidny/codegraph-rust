# Retrieval modes and needle results (measured, corrected)

Search-stack evaluation on this repository's index (`codegraph-rust`, ~31,600
chunks, 1024-dim F2LLM embeddings, Postgres). Metrics from
`scripts/benchmarks/retrieval-eval.py`. Relevant docs per query are the chunks of
the target symbol (`parent_node`).

Three rankings from the same candidate pool:

- **vector** — `chunks.embedding <=> query`
- **text** — full-text over `chunks.content_tsv`. Two variants are compared:
  the product's `ts_rank_cd`, and a **real BM25** (IDF + tf saturation + length
  normalisation) computed in SQL for this evaluation.
- **hybrid** — reciprocal rank fusion of vector and text (k=60), top 10.

## ts_rank_cd is not BM25

The product's text channel is `ts_rank_cd`, which has no corpus IDF and only
scores documents that satisfy the tsquery. The semantics swing the result
entirely, so it is unusable as a BM25 baseline:

| query set | tsquery | text nDCG@10 |
|---|---|---|
| natural language | `websearch_to_tsquery` (AND) | 0.0267 |
| natural language | OR'd lexemes | 0.0023 |
| identifier | AND | 0.7200 |
| identifier | OR | 0.1717 |

So an earlier version of this doc, which judged RRF on the AND/OR variant, was
measuring a broken ranker. That is why a real BM25 had to be built before
comparing vector and hybrid on natural language.

## Real BM25 (IDF + tf + length, computed in SQL)

`df` from `unnest(content_tsv)` per project, `tf = cardinality(positions)`,
`dl = sum of tf`, `avgdl` per project, `k1=1.2, b=0.75`, `idf = ln(1 + (N-df+0.5)/(df+0.5))`.
No extension, runs on Postgres 16.

Identifier queries (symbol name):

| mode | nDCG@10 | MRR@10 | Recall@10 | Hit@1 | Hit@5 |
|---|---|---|---|---|---|
| vector | 0.5425 | 0.5059 | 0.6609 | 0.443 | 0.583 |
| bm25 | 0.6423 | 0.5960 | 0.7913 | 0.513 | 0.687 |
| **hybrid** | **0.6797** | **0.6362** | **0.8174** | **0.548** | **0.722** |

Natural-language queries (doc comment):

| mode | nDCG@10 | MRR@10 | Recall@10 | Hit@1 | Hit@5 |
|---|---|---|---|---|---|
| **vector** | **0.5300** | **0.4715** | **0.7179** | **0.359** | **0.577** |
| bm25 | 0.2505 | 0.2112 | 0.3718 | 0.115 | 0.321 |
| hybrid | 0.4049 | 0.3393 | 0.6154 | 0.218 | 0.487 |

## Conclusion

- For identifier queries the text channel is strong and **uniform RRF adds over
  vector** (0.54 -> 0.68 nDCG).
- For natural-language queries the text channel is about half as good as vector
  (0.25 vs 0.53), and **uniform RRF regresses vector to 0.40**. This is now a
  real result on a real BM25, not an artifact.
- Therefore: **weighted RRF, or a confidence gate** that drops a channel whose
  top score is weak, is the correct next change. Equal fusion is only safe when
  both channels are comparable for the query type.

## Libraries evaluated for real BM25 in Postgres

| option | licence | BM25 | notes |
|---|---|---|---|
| `timescale/pg_textsearch` | PostgreSQL licence | yes | requires **Postgres 17/18** and `shared_preload_libraries`; our deployment is PG 16 |
| ParadeDB `pg_search` | **AGPL-3.0** | yes | PG14+; AGPL is a risk for product distribution |
| Tantivy (Rust) | Apache-2.0 | yes | in-app engine; large architectural change |
| SQL BM25 (this doc) | none | yes | validated here on PG16; slower, keeps the current image |

Production still runs `ts_rank_cd`; adopting `pg_textsearch` (PG17 bump) or
porting this SQL BM25 (with a maintained `df` table) are the two paths.

## Caveats

- Doc comments are a proxy for natural-language intent, not human-labelled queries.
- Identifier queries trivially favour exact-token matching.
- The BM25 numbers use a one-off SQL harness, not the product's text channel.

## Reproduce

```bash
python3 scripts/benchmarks/retrieval-eval.py results.json
python3 scripts/benchmarks/retrieval-eval.py results.json --mode vector,hybrid
```
