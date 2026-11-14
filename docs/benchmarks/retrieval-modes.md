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

## Ablation: index the doc comment

The natural-language query is the symbol's doc comment, but `content_tsv` is
`name || text` — the comment lives in node metadata, not the searchable text. In
the 200-node sample only **33.7%** of doc terms appear in `content_tsv` (avg
13.4 terms/doc). That is a **field mismatch**, not a property of BM25.

Re-indexing the doc comment into the text field (`name || text || doc`) and
re-running with the same doc as the query:

| doc indexed | mode | nDCG@10 | MRR@10 | Recall@10 |
|---|---|---|---|---|
| no | vector | 0.5300 | 0.4715 | 0.7179 |
| no | bm25 | 0.2505 | 0.2112 | 0.3718 |
| no | hybrid | 0.4049 | 0.3393 | 0.6154 |
| **yes** | vector | 0.5300 | 0.4715 | 0.7179 |
| **yes** | **bm25** | **0.8439** | **0.8033** | **0.9744** |
| **yes** | hybrid | 0.7363 | 0.6747 | 0.9231 |

So the 0.25 was mostly the described text not being indexed; once it is, BM25
reaches 0.84 on these queries. Caveat: the query is the stored comment, so this
is an **exact-match upper bound**; a human paraphrase would be harder for both.

The fusion result flips with which channel is stronger: uniform RRF dilutes the
better list. Before (text weak) hybrid 0.40 < vector 0.53; after (text strong)
hybrid 0.74 < bm25 0.84. That is the theoretical reason a **confidence gate or
learned weights** are needed — equal-weight RRF is not adaptive.

## Implemented, and what it does (and does not) fix

Two changes are in:

- **Doc indexed** (`cb178c2`): `nodes.search_tsv` = name + content + `metadata->>'doc'`,
  generated column + GIN, used by `FULL_TEXT_NODES_SQL`. This makes the prose
  searchable. It is what lifts a **real BM25** to 0.84 in the ablation above.
- **Weighted RRF** (`72a2c11`): default `vector=0.5, name=1.0, content=1.0`
  (`CODEGRAPH_RRF_WEIGHTS` overrides). Helps identifier queries, where the text
  list is populated and strong.

**But the product still uses `ts_rank_cd`, not BM25, and the text query is
AND-only** (`tsquery_prefix` joins every term with `&`). For a long natural-language
description the AND query requires all terms to co-occur, so the content list is
**empty**; `ts_rank_cd` also has no IDF (an OR variant measured 0.0025). Net: on
descriptions the product's text channel contributes nothing, so
`semantic_code_search` falls back to vector alone. Neither of the two changes
makes NL hybrid better on its own.

The remaining step is therefore **a real BM25 text channel with IDF and an
OR/term-weight query mode** (SQL BM25 on PG16, or `pg_textsearch` on PG17+).
Only then do the doc indexing and the RRF weights pay off for descriptions, and
only then should the weights be re-tuned.

## Conclusion

- For identifier queries the text channel is strong and **weighted RRF adds over
  vector** (0.54 -> 0.68 nDCG).
- On natural language, the original "vector beats BM25" was **confounded**: the
  described text was not in the searchable field (33.7% term overlap). Indexing it
  lifts a **real BM25** to 0.84 - but the product's `ts_rank_cd` + AND query does
  not reach that, because it has no IDF and ANDs every term.
- In both directions **uniform RRF regresses toward the weaker channel**; weighted
  RRF (now shipped) fixes the identifier side. The description side needs the real
  BM25 first.
- Next: BM25 text channel (IDF + OR), then re-tune `CODEGRAPH_RRF_WEIGHTS`.

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
