# Backend evaluation: SurrealDB vs Postgres + pgvector

Status: measured, decision pending. This document records an end-to-end comparison run on
one machine (16 cores, NVMe, SurrealDB 4.0.1 in Docker, Postgres 16 + pgvector in Docker)
so the numbers do not have to be re-derived.

## Why this was measured

Indexing spends almost all of its time in storage writes, and every client-side optimisation
attempted earlier produced nothing (see "Falsified hypotheses" below). The suspicion was that
the bottleneck sat in `surrealkv`, the storage engine SurrealDB ships. Reading its source
confirmed it:

```
surrealkv/src/ring/pipeline.rs:76-77
/// ... **The flusher is the only user**, so there is one set...
```

A single flusher task performs, serially: VLog append, WAL append + sync, and memtable
apply. Commit threads prepare batches in parallel (`prepare_inline`), which is why the
container shows 3.7 cores busy while throughput barely moves — measured `CPU 374%` with
4 concurrent writers, yet only 1.4x the throughput of a single writer.

## Method

Same data, same machine, same parameters, both databases loaded with:

- **62,233 rows x 1024-dimensional vectors** (random f32, `vector(1024)` / `array<float,1024>`)
- **97,845 edges** exported from a real indexed graph (`codegraph-rust` project)
- HNSW parameters identical where comparable: `m=12`, `ef_construction=150`
- KNN measured with `K=10`, Postgres `ef_search` default (40), SurrealDB `<|10,40|>`

SurrealDB numbers are from the running `codegraph_rust_1954fc594668` database (29,220 nodes /
97,845 edges / 29,538 chunks), i.e. **half the row count** of the Postgres fixtures, so every
Postgres win below is conservative.

## Results

| Operation | SurrealDB 4.0.1 | Postgres 16 + pgvector | Speedup |
|---|---|---|---|
| Write 62,233 rows x 1024-d | ~600 s (9.7 ms/row) | **5.62 s** (0.09 ms/row) | **107x** |
| HNSW build, 62k rows | 18.9 s (75k rows) | 13.4 s (62k) | 1.4x |
| KNN K=10 | 39.0 ms (29.5k rows) | **0.69 ms** (62.2k rows) | **56x** |
| Full-text search | 25.0 ms | **0.16 ms** | **156x** |
| Reverse dependencies, depth 1 (1,324 rows) | **420 ms** | **1.7 ms** | **247x** |
| Reverse dependencies, depth 3 | **2,040 ms** | **59.7 ms** | **34x** |
| Load 97,845 edges | (inside the 600 s) | 0.11 s | — |
| Storage size | — | 808 MB (62k vectors + 262k edges) | — |

The graph traversal row is the most significant one and was not expected: SurrealDB's
`fn::get_reverse_dependencies` recurses through `fn::expand` using SurrealQL array operations
rather than an index walk, so a single hop over 1,324 edges costs 420 ms. This is the
operation agents call most often.

## Caveats

1. **Full-text 0.16 ms is optimistic.** The fixture corpus used 12 distinct words, 30 per row,
   with the `simple` dictionary. Real source text (many identifiers, long documents) will be
   slower — expect 1-5 ms, still far below 25 ms.
2. **Postgres full-text ranking is not strict BM25.** `ts_rank_cd` is a coverage ranking. The
   current hybrid path (vector + BM25 + RRF fusion) would need the RRF step in SQL or the
   application, and ranking relevance must be compared on real queries before switching.
3. **KNN 0.69 ms is warm.** The first query after a cold start measured 2.9 ms.
4. **Depth-3 traversal is slower than depth-1 on both** (59.7 ms vs 1.7 ms) because the
   recursive CTE visits substantially more rows; that is expected, not a regression.
5. **Write comparison is COPY vs client RPC.** Postgres used `COPY FROM STDIN`, which is
   process-local; codegraph writes over a network client. Even so, a networked Postgres bulk
   `INSERT` typically costs 1-3 ms/row, and codegraph can adopt the `COPY` protocol
   (`CopyFrom`) directly, so 0.09 ms/row is reachable rather than theoretical.

## Falsified hypotheses (do not re-run)

Every one of these was measured on the real indexing path and produced no improvement:

| Hypothesis | Measurement | Outcome |
|---|---|---|
| Disk cache reads dominate | parallelised them | saved 5 s, reverted |
| JSON serialisation of 1024-d vectors | `SYMBOL_EMBED_TRACE` record segment | 190-218 ms total |
| Batch too small | 897 -> 113 batches | no change |
| Per-insert HNSW maintenance | dropped the index before writes, rebuilt after (36 s) | no change, reverted |
| REFERENCE fields | 1,000 rows with/without `node_id` + `source_edge_id` | identical (1.7-2.3 ms/row) |
| Empty vector indexes (8 of 9) | 0 / 1 / 9 HNSW definitions | identical (2.8 ms/row) |
| `to_vec` clone | `WRITE_SPLIT` probe | 0.02 s total |
| `bind` -> `Value` construction | `WRITE_SPLIT` probe | 0.56 s total |
| Protocol ws vs http | A/B on the same workload | http 10% faster, not the cause |
| SQL literal rewrite | full A/B | **10% slower**, reverted |
| `/import` + `OPTION IMPORT` | measured | 6.41 ms/row, slower than bind |
| Vector dimension | 384 / 768 / 1024 / 2048 (no RNG) | 0.20 / 0.24 / 0.42 / 0.38 ms/row (server side) |
| Client `insert().content()` | code reading | same `into_value()` path as `bind` |

Two methodology notes that cost time and should not be repeated:

- **Single-request benchmarks do not predict concurrent behaviour.** A 64-row SQL literal
  request costs 2.72 ms/row alone and 10.7 ms/row at 4 connections, because the server is
  saturated. Conversely, an early "631 rows/s ceiling" measured over HTTP `/sql` was not the
  writer's ceiling (the writer reached ~1,970 rows/s on a smaller project).
- **SurrealDB's `array::map(... rand::float() ...)` generation dominates any synthetic
  benchmark.** 1,024 `rand()` calls per row cost 2.4-2.9 ms, which masked the real storage cost
  (0.36 ms/row) in several early experiments. Always use `array::repeat(const, N)`.

## Prerequisite validations (both done)

Before any migration code, two things had to hold: the graph queries had to translate, and
retrieval quality had to survive.

### 1. Graph traversal, both directions

Same graph (29,220 nodes / 97,845 edges), same node (`'calls'` edges only):

| Query | SurrealDB | Postgres | Speedup |
|---|---|---|---|
| reverse deps, depth 1 (1,324 rows) | 420 ms | **1.7 ms** | 247x |
| reverse deps, depth 3 | 2,040 ms | **59.7 ms** | 34x |
| forward call chain, depth 1 (379 callees) | 940 ms | **1.47 ms** | 640x |
| forward call chain, depth 3 | 2,530 ms | **26.0 ms** | 97x |

SurrealDB's `fn::*` walkers recurse through `array::fold` / `fn::expand` (SurrealQL array
operations with a 1 MiB output cap), not an index walk; the forward direction is worst because
it additionally tracks the path at every step. Recursive CTEs with an index on the join column
are 1–2 orders of magnitude cheaper.

### 2. Vector retrieval quality

29,538 real chunk embeddings (1024-d) loaded into both backends, HNSW with the same
`m=12 / ef_construction=150`, `ef_search = 40` on both sides.

- Top-10 overlap between backends: mean **8.1/10** across 10 queries, but three queries
  overlapped 1–3/10.
- Against exact (`ORDER BY <=>` with sequential scan as ground truth), both approximations are
  equally imprecise: **SurrealDB 6.8/10, Postgres 6.5/10** recall@10, and the three low-overlap
  queries score 0–1/10 against exact **on both sides**.

Conclusion: the low-overlap pairs are a property of HNSW on this data distribution (many
near-duplicate chunk vectors), not a Postgres deficiency. Retrieval quality does not regress.

**Not yet measured**: full-text ranking. SurrealDB uses BM25; Postgres `ts_rank_cd` is a
coverage ranking, so top-10 lists will differ even when recall is comparable. The hybrid path
(vector + full text + RRF) therefore needs a relevance comparison on 20 real queries before the
switch — plan for RRF fusion in SQL or the application.

## What SurrealDB is actually used for here

Inventory of the current schema and queries (8 tables, 55 indexes, 28 HNSW definitions,
148 `fn::*` call sites, 26 `DEFINE FUNCTION`):

| Capability | Usage | Available in Postgres |
|---|---|---|
| HNSW KNN (`vector::similarity`, `<|K,EF|>`) | core | `pgvector` + `hnsw` opclass |
| BM25 full text (`FULLTEXT ... BM25`) | 2 indexes | `tsvector` + GIN (not strict BM25) |
| Hybrid search + RRF | schema `fn::search_*` | SQL or application-side RRF |
| Bulk upsert (`array::map` + `ON DUPLICATE KEY UPDATE`) | core | `COPY` + `ON CONFLICT` |
| Transactions | 4 sites | native |
| `fn::*` stored procedures | 148 calls | **must move to Rust** (already in progress) |
| Record relations (`edges.from/to`) | whole table | text FK + indexes |
| Aggregates (`GROUP ALL`) | 18 sites | native |

Not used: `RELATE`, `LIVE SELECT`, graph traversal syntax (`->`).

## Migration shape

`docs/INTERNAL_FUNCTION_MIGRATION.md` already established the layering and moved the analysis
algorithms into Rust (`AnalysisCore`: batches 1, 2, 3 and 5 done). What remains is exactly the
part that talks to the database:

| Layer | Current | Migration work |
|---|---|---|
| `AnalysisCore` (algorithms) | pure Rust | **none** |
| Batch 4 (KNN / semantic_search / BM25) | not started | PG SQL rewrite (would be done anyway) |
| `GraphQueries` (subgraph, reverse deps, call chains) | SurrealQL `fn::*` | recursive CTE |
| `surrealdb_storage.rs` (~3,800 lines) | SurrealQL | new `postgres_storage.rs`, env-switched |
| schema (8 tables / 55 indexes / 28 HNSW) | `.surql` | DDL with `vector(1024)` + `tsvector` |
| Multi-tenancy | namespace/database | Postgres schema per project |
| Reconciliation | node/chunk/edge sweep | same logic, PG SQL |

Estimated 2-3 weeks. Both prerequisite validations are done (see above): graph traversal is
34-640x faster in both directions, and vector recall matches (6.8 vs 6.5 out of 10 against
exact). The only remaining unknown is full-text **ranking** — `ts_rank_cd` is not BM25, so a
20-query relevance comparison of the hybrid path is still required before the switch.

## Expected effect on indexing

EasyTier (847 files, 476k rows written):

```
now:         494 s   (480 s of it is storage writes)
after switch: ~20-30 s  (writes ~5 s)
queries:      39 ms -> 0.7 ms (KNN), 420 ms -> 1.7 ms (reverse deps)
```
