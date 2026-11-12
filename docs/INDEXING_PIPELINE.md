# Indexing pipeline

What `codegraph index` actually does, in order, with the phase timings it reports and the
controls that change each phase. Numbers are measured on a 10,763-file TypeScript
monorepo (169,528 nodes / 1,942,741 relationships) unless stated otherwise.

## Stage overview

```
1  input_validation        args, providers, tier, tool availability
2  source_capture          read every source file once into snapshots
3  file collection         walk + gitignore + extension filter
4  ast_parse               tree-sitter: nodes and raw edges        (~1s)
5  analyzers               LSP or SCIP: qualified names + definition edges
6  module_linking          module nodes, contains/imports edges
7  docs_contracts          documentation and contract edges
8  chunk_plan              split nodes into embedding-sized chunks (~4s)
9  enqueue_nodes           batched node writes (512 rows/batch)
10 embeddings_and_writes   embed + write chunks                    (dominant)
11 resolve_and_enqueue_edges   exact → lexical → semantic
12 edge writes + file_metadata
13 durable_reconcile_and_indexes  prune stale rows, build vector index, mark ready
14 cache_eviction
```

`--stats-json` writes `phase_ms` with these keys:

| key | meaning |
|---|---|
| `input_validation`, `source_capture`, `ast_parse` | stages 1–4 |
| `analyzers` | stage 5 (dominated by the language server or SCIP import) |
| `chunk_plan` | stage 8 |
| `enqueue_nodes` | stage 9 |
| `embeddings_and_writes` | stage 10, including chunk fingerprinting and chunk writes |
| `resolve_and_enqueue_edges` | stage 11 total |
| `resolution_catalog_build`, `resolution_unique_aliases`, `resolution_exact_loop`, `resolution_tail` | breakdown of stage 11 |
| `resolution_fuzzy_calls`, `_candidates`, `_len_pruned`, `_levenshtein_calls`, `_above_cutoff`, `_ms`, `_counts_ms`, `_lev_ms`, `_trigram_ms`, `_limit`, `_threads` | fuzzy path counters |
| `durable_reconcile_and_indexes` | stage 13, including HNSW construction |
| `cache_eviction` | stage 14 |

## Where the time actually goes

Measured on the TypeScript monorepo, before optimisation:

| phase | time | share |
|---|---|---|
| `embeddings_and_writes` | 23.4 min | 46% |
| `resolve_and_enqueue_edges` | 25.0 min | 49% |
| `analyzers` (LSP) | 16.0 min | 32% |
| `chunk_plan` | 4 s | — |
| `ast_parse` | 3 s | — |

Two of those numbers are counter-intuitive and worth remembering:

**`embeddings_and_writes` is a database phase, not an embedding phase.** Observed while it
ran: GPU utilisation 0%, embedding container CPU 0.4%, indexer CPU 3.4%, embedding API calls
18 (cache hits), SurrealDB CPU 101% on one core, chunk writes at 125 rows/s. The cost is
HNSW insertion: every chunk's 1024-dimension vector is inserted into the HNSW graph one row
at a time inside SurrealDB, which is single-threaded. 170k rows × ~8 ms ≈ 23 min.

**`resolve_and_enqueue_edges` is mostly fuzzy matching.** 639k lookups fall back to fuzzy
because the target did not match exactly; each one gathered trigram-overlapping candidates
and ran Levenshtein. The breakdown showed `counts_ms` at 85.6% of fuzzy time — the candidate
counting pass ran *before* the length prune.

## Controls that change each phase

### Stage 5 — analyzers: LSP or SCIP

| control | effect |
|---|---|
| `CODEGRAPH_INDEX_TIER=fast` | no language server at all |
| `CODEGRAPH_SCIP_INDEX=<index.scip>` | import a pre-built SCIP index instead of driving a language server; also skips LSP tool validation |
| `CODEGRAPH_SCIP_MAX_BYTES=<n>` | opt-in safety valve that rejects an artifact larger than `n` bytes; unset/`0` disables it (default) |
| `CODEGRAPH_LSP_SKIP_TEST_FILES` | default on; skips `.stories.`, `.generated.`, `__mocks__/`, `__fixtures__/`, `generated/`. **Do not** add `.test.`/`.spec.` back to the skip list — that dropped 25,970 resolvable edges (31.57% → 30.23%) |
| `CODEGRAPH_LSP_ON_PARTIAL=1` | allow language servers on partial (subdirectory) indexes |

SCIP measured on the same project: **260 s for the whole index** versus 16 min of LSP
round-trips, 12.3 GB peak (needs `NODE_OPTIONS=--max-old-space-size=16384`; the default
4 GB heap OOMs in 38 s), producing a 525 MB `index.scip`.

`scip-typescript` embeds no document text and writes no `index.sources.json`. Documents
without a manifest entry are accepted with a single warning that reports the count;
an entry that exists but disagrees with the captured source is rejected as stale, and
`CODEGRAPH_SCIP_TRUST_SOURCE=1` opts out of hash validation entirely. SCIP paths are
normalised to the same project-relative posix keys as nodes, so indexes built from `.`,
an absolute root or Windows separators all bind. Import fails loudly when zero documents
decode or zero documents match an indexed source instead of reporting success with no
enrichment. Documents are decoded one length-delimited message at a time and dropped before
the next, so peak memory tracks the largest single document, not the artifact size; there is
no default file-size cap. `CODEGRAPH_SCIP_MAX_BYTES` opts into a size limit instead.

### Stage 10 — embedding and chunk writes

| control | effect |
|---|---|
| `CODEGRAPH_PROVIDER_CONCURRENCY` | parallel embedding requests; 4 is the practical sweet spot on a local GPU (2→4 gave +22%, 4→8 only +13%) |
| `CODEGRAPH_EMBEDDING_POLICY=off\|deferred` | skip or defer inference |
| `CODEGRAPH_CHUNK_DB_BATCH_SIZE` | chunk write batch (default 256) |
| `CODEGRAPH_VECTOR_INDEX_MODE=deferred` | **build HNSW after all rows are written** instead of maintaining it per insert; only affects fresh stores (existing indexes are preserved) |
| `CODEGRAPH_VECTOR_INDEX_MODE=off` | never build the vector index |
| `CODEGRAPH_VECTOR_INDEX_TIMEOUT_SECS` | bound on HNSW construction |

### Stage 11 — resolution

| control | effect |
|---|---|
| `CODEGRAPH_FUZZY_CANDIDATE_LIMIT` | cap fuzzy candidates (0 = unlimited, the default; a 256 cap changed nothing on real data) |
| `CODEGRAPH_SEMANTIC_RESOLUTION=deferred\|off` | skip vector-similarity resolution |
| `CODEGRAPH_SURREAL_POOL_SIZE` | writer connections (default 1) |

### Stage 13 — reconcile and vector index

`ensure_ready` runs unconditionally unless the mode is `off`. In `deferred` mode the HNSW
index is created here, after all rows are durable — this is the batch build.

## Practical recipes

**Large monorepo, fastest reproducible index**

```bash
# once, with a 16 GB heap
cd <project> && NODE_OPTIONS=--max-old-space-size=16384 scip-typescript index --pnpm-workspaces
# then
CODEGRAPH_SCIP_INDEX=<project>/index.scip \
CODEGRAPH_VECTOR_INDEX_MODE=deferred \
CODEGRAPH_PROVIDER_CONCURRENCY=4 \
codegraph index . --index-tier balanced
```

**CI / verification runs where the graph must be byte-identical**

```bash
codegraph index . --index-tier fast
```

`fast` skips every analyzer, so the result depends only on the source tree. `balanced`
depends on language-server state (see [LSP determinism](LSP_DETERMINISM.md)).

**A stalled or OOM-killed index**

Check `docker logs` for the SurrealDB container. 3.x bounds memtables by entry count and can
grow past the container limit; 4.x bounds them by value bytes. See
[deploy/README.md](../deploy/README.md).

## Reading a run

```bash
codegraph index . --stats-json /tmp/stats.json
jq '.phase_ms' /tmp/stats.json | sort -k2 -n
```

`graph_complete`, `vector_index_ready` and `embedding_status` are separate states: a run can
finish the graph but leave the vector index unbuilt (`vector_index_ready: false`).
