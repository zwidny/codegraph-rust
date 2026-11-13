# TODO — indexing performance & LSP → SCIP migration

Context for a fresh session. State below reflects commit `5a29ce1` (2026-11-12).

## Where things stand

### Fixed in `610fd05` (both were measurement/graph defects, verified on real data)

| defect | before | after |
|---|---|---|
| resolution log counted unresolved edges as resolved | `exact/contextual=138859 + lexical=1099 = 139958` = the **total** edge count; `semantic=0` while the store held 3,182 semantic edges | `94276 + 1100 + 3225 = 98601` = `resolved` |
| stale edges never pruned | store 102,832 rows vs a 96,411-edge graph (6,421 residue); every old edge had live endpoints, so `from IN $stale_nodes OR to IN $stale_nodes` never matched | 96,733 == 96,733; a forged edge with live endpoints is removed by the next run |

`reconcile_catalog_by_file` now takes `fresh_edge_ids` and deletes ids absent from that set
whose endpoints live in a scanned file. Edge identity includes provenance/`source_span`, so
switching `--index-tier` changes edge ids while node ids stay put — that is what orphaned the
residue. An empty `fresh_edge_ids` skips the sweep instead of wiping the table.

### Best-quality configuration (measured on this repo)

```bash
CODEGRAPH_SCIP_INDEX=$PWD/.codegraph/scip/rust.scip \
CODEGRAPH_SEMANTIC_RESOLUTION=sync CODEGRAPH_EMBEDDING_POLICY=sync \
CODEGRAPH_FUZZY_CANDIDATE_LIMIT=0 CODEGRAPH_VECTOR_INDEX_MODE=deferred \
CODEGRAPH_PROVIDER_CONCURRENCY=4 \
codegraph index . --index-tier full --force --recursive
```

`full` is not cosmetic: `extraction_policy_for_tier` emits `references` only for `Full`, and
`filter_edges_for_tier` strips `References` from `Balanced`.

| | balanced + deferred | full + sync |
|---|---|---|
| nodes | 14,041 | 29,220 |
| edges | 24,205 | 96,733 |
| `resolution_rate` | 38.28% | 70.23% |
| semantic | `pending` (0 edges) | `ready` (3,192 edges) |
| `index_ms` | 126,426 | 196,425 |

Note: `semantic=sync` on this repo yields ~3.2k edges (565 distinct names above the 0.75
threshold out of 1,614 unresolved names). It is real but modest; `full` contributes far more.

### Datastore

SurrealDB **4.0.1** (container `codegraph-db`, volume `cg-server-data-v4`, mem limit 12g).
4.x is **required**: 3.x bounds memtables by entry count and the kernel OOM-kills it on a
first index of a large project (measured 11.95 GiB peak, 5 restarts) versus 0.86 GiB on 4.x.

Old volume `cg-server-data` (3.x format) is kept as the only rollback path. 4.x refuses to
open it without `SURREAL_EXPERIMENTAL_UPGRADE_FROM_3X=true` (one-way conversion).

Project databases: `EasyTier_8a11a0febb5c`, `codegraph_rust_1954fc594668`,
`happy_60842ea5efba`, `paperclip_fba0cae8cecd`. **Never write these in experiments.**

### Indexing speed (paperclip: 10,763 files / 169,528 nodes / 1.94M relationships)

```
79 min (original)  ->  17 min  (4.6x)
```

| stage | before | after | lever |
|---|---|---|---|
| analyzers | 21 min | 2.4 s + 3.9 s import | SCIP instead of per-file LSP |
| embeddings_and_writes | 23 min | 5 min | `CODEGRAPH_VECTOR_INDEX_MODE=deferred` (125 → 357–499 rows/s; HNSW built once in 51 s) |
| resolve_and_enqueue_edges | 25 min | 7.8 min | length pruning before candidate counting |
| chunk fingerprint | 9 min | 2.5 min | `FingerprintTail`: fixed config hashed once |

### Measured: writer tuning is the single biggest lever

Full tier, identical graph, only env changed (`--scip-auto --index-tier full --force`,
semantic=sync, embedding=sync). Three runs per configuration are not needed here because the
default configuration was measured twice and agrees.

| config | index_ms | embeddings_and_writes | resolve | symbol_embeddings |
|---|---|---|---|---|
| default (pool 1, chunk batch 256) | 415–428 s | 241 s | 136 s | 117 s |
| `CODEGRAPH_SURREAL_POOL_SIZE=4` | **130 s** | 71 s | 47 s | 36 s |
| pool 4 + `CODEGRAPH_CHUNK_DB_BATCH_SIZE=512` | **89 s** | 23 s | 52 s | 39 s |

`chunk_embedding_db_batch_size` clamps to **512** (`indexer.rs`, `MAX`), and
`CODEGRAPH_SURREAL_POOL_SIZE` clamps to **1..=4**, so the tuned run above was executed with
the variable set to 1024 but the effective value was 512. Set these to 512 and 4 any other way
and nothing further happens; raising the caps is a separate code change (`Surreal query depth is
limited`, per the comment above the clamp).

Two default-config runs (427.6 s and 415.3 s) bracket the baseline, so the 4.7x is real and not
load noise. The `analyzers` figure swings 5–27 s between identical runs with the same
478 nodes / 458 edges — that is the LSP non-determinism from `docs/LSP_DETERMINISM.md`, not a
tuning effect; do not attribute it to `POOL_SIZE`.

Direct measurements that rule out the usual suspects:

- **Embedding service is fast**: 45–54 ms for 1 text, 35–61 ms for 8, 85–610 ms for 64.
- **Per-insert HNSW is not the cost**: 1,000 random 1024-d rows take 3.22 s with no index and
  3.18 s with the HNSW index defined. The 1024-d row write itself dominates.
- **What actually changed**: with `pool=4` the same rows land at 2.4 ms/row; adding
  `chunk batch=512` (the clamp) drops the chunk window to 0.79 ms/row (23.4 s for 29,538 chunks).
- 29,538 vectors produced only 65 embedding inferences (`inference_texts: 65`,
  `cache_hits: 47,142`), so the phase named "embeddings_and_writes" is ~99% writes.
- Skipping inference entirely (`CODEGRAPH_EMBEDDING_POLICY=deferred`) needs 115 s and leaves
  `chunks=0`, `embedding_status=pending`; reconcile then costs 26 s (up from 4–11 s) to delete
  the missing rows. Useful for "graph now, vectors later", not for a one-shot build.

### Measured: `symbol_embeddings` persist is not batch-, cache- or HNSW-bound

Four EasyTier runs with `CODEGRAPH_TRACE_SYMBOL_EMBED=1`, ~57,400 symbol names each:

| run | outer batch | DB batch | 1024 HNSW index | `persist` |
|---|---|---|---|---|
| 1 | 64 | 128 | present | 121,645 ms |
| 2 | 64 | 512 | present | 117,443 ms |
| 3 | 512 | 512 | present | 118,986 ms |
| 4 | 512 | 512 | **dropped, rebuilt after** | 117,402 ms |

All four land in 117–122 s ≈ **2.0 ms/row**, while `chunks` writes 75,376 rows at 0.3–0.8 ms/row.

Everything that looked plausible was measured and falsified:

- **batch count**: 897 → 113 batches changed nothing (so `CODEGRAPH_SYMBOL_BATCH_SIZE` and
  `CODEGRAPH_SYMBOL_DB_BATCH_SIZE` are both irrelevant here)
- **HNSW per-insert maintenance**: dropping `idx_chunks_embedding_1024` and
  `idx_symbol_embeddings_vector_1024` before the write and rebuilding them afterwards (36 s)
  changed nothing; the rebuild was pure extra cost, so that change was reverted
- **disk cache reads**: parallelising them saved 5 s (reverted)
- **JSON serialisation** of 1024-d vectors: 190–218 ms total (not the cost)

Remaining candidates, none verified: the composite index
`idx_symbol_embeddings_project_symbol`, the wider row (7 extra fields vs chunks), or a
storage-level property of that table. **Do not re-run the batch or HNSW experiments.**

### What was learned (do not re-derive)

- `embeddings_and_writes` is a **database write** phase: GPU 0%, embedding container 0.4%
  CPU, 18 API calls, SurrealDB 101% on one core. Cost was HNSW insertion, not inference.
- fuzzy matching was 85.6% candidate **counting**, 14.3% Levenshtein. Pre-pruning the
  length window before the HashMap took `counts_ms` from 14.6M to 2.2M.
- Self-loops are 11.4% of EasyTier's edges (8,317/72,940). Rust degree counting must include
  them; `mutual_pairs` must exclude them. Fixture-only tests missed this for three commits.
- LSP enrichment is non-deterministic (depends on server readiness);
  see `docs/LSP_DETERMINISM.md`. SCIP is static and does not have this problem.
- Native (`*_native`) analysis methods are gated by `CODEGRAPH_NATIVE_ANALYSIS` (opt-in) and
  verified on real data by `crates/codegraph-graph/tests/native_real_data_diff.rs` (`#[ignore]`).
- **Regenerate `rust.scip` after editing sources.** Reusing a stale index aborts the whole run
  with `SCIP column is invalid or document position encoding is unspecified`
  (`crates/codegraph-mcp/src/analyzers/scip.rs`): `byte_position` rejects a recorded column that
  is past the line or off a char boundary. Since `291192f` the import first warns
  `SCIP index … is older than N of M captured sources` and the position error names the document
  and encoding, so this is now diagnosable instead of mysterious. `--scip-auto` regenerates by
  fingerprint; with an explicit `CODEGRAPH_SCIP_INDEX` check with
  `find crates -name '*.rs' -newer .codegraph/scip/rust.scip`.
- **`phase_ms` mixes counters with durations.** `resolution_fuzzy_len_pruned=130849978` is a
  count, not milliseconds, so `jq '.phase_ms | sort_by(.value)'` misreads the run.
- **`ingestion_write_test` flakes under parallel threads.** `bulk_ingestion_is_repeatable_and_
  cleanup_is_typed_and_scoped` fails ~2/20 on a clean HEAD with `The query was not executed due
  to a failed transaction`; `--test-threads=1` is stable. Pre-existing `mem://` engine issue.

## TODO

### 0. Let `codegraph index` generate SCIP itself — **done in `ee749b1`**

`--scip-auto` detects languages from manifests (root `Cargo.toml`, any `go.mod`, any
`package.json` within depth 3, skipping `vendor`/`target`/`node_modules`/`dist`), generates
one index per language into `.codegraph/scip/`, and feeds the paths back as
`CODEGRAPH_SCIP_INDEX` entries.

Caching lives in the `scip-auto-v1` artifact namespace, keyed by a fingerprint over
`(relative path, byte length, mtime seconds)` of **that language's** sources, so a Rust edit
does not invalidate a TypeScript index. Per-language failure handling: a missing indexer or a
non-zero exit warns and falls back to language servers; `--scip-strict` turns both into errors
carrying the last 20 stderr lines. An explicit `CODEGRAPH_SCIP_INDEX` wins and skips
auto-generation.

Measured here with no environment variables set:

| run | SCIP step | index_ms | graph |
|---|---|---|---|
| 1 (generate) | ~70 s `rust-analyzer scip`, import 180 ms | 506,541 | nodes 29,486 / edges 97,768 |
| 2 (cache hit) | `reusing cached SCIP index for rust:`, import 187 ms | 407,901 | identical, `complete: true` |

Both runs produced the same 228/228 documents, 4,069 nodes + 14,360 edges, and a
bit-identical `resolution_rate`. **Known gap:** an indexer that exits 0 without writing its
output still yields a spec, so the run fails later with "file not found" instead of falling
back.

Still manual: CI. `scripts/scip-index.sh` and `--scip-auto` both work locally; wiring either
into CI (and caching the artifacts between jobs) is unstarted.

### 1. Per-language analyzer selection — **done in `0a075d3`**

Previously `CODEGRAPH_SCIP_INDEX` disabled LSP **globally**, so any language without an
index (Kotlin, C, a missing Go index) got zero enrichment — worse than the LSP-only baseline.

Now `ImportStats.covered_files` records the project-relative keys of every document an index
matched, and the indexer imports SCIP first, then runs LSP only on
`files_without_scip_coverage`. LSP stats accumulate with `+=` (SCIP contributes first) and the
LSP summary line reports its own `total_stats`.

Measured on this repo: SCIP covers **227 of 260** files, LSP runs on the remaining **33**
(5.1 s, `languages: [Python, Rust]`, **478 nodes + 458 edges** — previously zero).
`lsp_nodes_enriched` went 4,035 → 4,520 == 4,042 (SCIP) + 478 (LSP) exactly.

EasyTier acceptance (2026-11-12, four indexes: rust, go@easytier-go, ts, ts-js@easytier-js):

| metric | value |
|---|---|
| SCIP coverage | **743 of 847** scanned files (104 to LSP) |
| SCIP contribution | 15,903 nodes + 62,011 edges in 957 ms |
| LSP contribution | 115 nodes + 748 edges in 37.5 s |
| `lsp_nodes_enriched` | **16,018** = 15,903 + 115 (target ≥ 15,673 ✅) |
| LSP-enriched by language | **Kotlin 73**, TypeScript 25, JavaScript 15, Cpp 2 |
| graph | 75,353 nodes / 262,138 edges, `resolution_rate` 71.13%, `complete: true`, 0 errors |

The Kotlin/C files that used to get nothing now carry `analyzer: lsp_symbols` in
`metadata` (note: `metadata` is stored as a *stringified* JSON on nodes, so query it with
`string::contains(metadata, 'lsp_symbols')`, not `metadata.attributes.analyzer`).

### 2. TypeScript on EasyTier — **done**

`scip-typescript 0.6.2` + `pnpm install` produced two indexes because EasyTier has **two**
independent pnpm workspaces:

```bash
# root workspace: easytier-gui, easytier-web/{frontend,frontend-lib,config-generator},
# tauri-plugin-vpnservice (7 tsconfig projects)
cd <project> && pnpm install   # 1m32s, 835 packages
NODE_OPTIONS=--max-old-space-size=16384 scip-typescript index --pnpm-workspaces \
  --output /tmp/opencode/easytier-ts.scip      # 2.1 MB

# easytier-js is its OWN workspace (browser/cloudflare/runtime/examples), NOT covered
# by the root --pnpm-workspaces run
cd <project>/easytier-js && pnpm install       # 3m03s
NODE_OPTIONS=--max-old-space-size=16384 scip-typescript index --pnpm-workspaces \
  --output /tmp/opencode/easytier-ts-js.scip   # 887 KB → use @easytier-js prefix
```

Measured: TS index covered 40 documents (188 nodes + 213 edges), `easytier-js` covered 28
documents (42 nodes + 87 edges). `examples/web` has no tsconfig and is skipped by the
indexer itself.

**Always pass `--output`**: `scip-typescript` defaults to `index.scip` in the current
directory, which would overwrite the rust-analyzer index. The Rust index lived at
`<project>/index.scip` (48 MB) and was copied aside before the first run.

### 3. Kotlin and C

- Kotlin (13 files): `scip-java` requires JDK 11+ (host has only JDK 8) plus Kotlin support.
- C (1 file): `scip-clang` requires `compile_commands.json`.
Low value on EasyTier; revisit only if other projects need them.

### 4. CI: cache the generated indexes (decision made, CI wiring open)

The "run the indexers as a pre-step" option is now implemented as `--scip-auto` (see item 0),
so the decision between the two routes is settled: precondition is part of `codegraph index`
itself, not a separate CI-only artifact.

What remains is CI plumbing: run `codegraph index --scip-auto` (or
`scripts/scip-index.sh --export`) in the workflow and cache `.codegraph/scip/` between jobs
keyed by the same source fingerprint, so a CI run does not rebuild indexes it already has.
Nothing is wired up yet.

### 5. Known limits not yet addressed

| Item | Detail |
|---|---|
| Stale-edge sweep is endpoint-scoped | Fixed in `610fd05`, but a file that produces zero nodes never enters `fresh_nodes_by_file`, so its edges only get pruned when the *other* endpoint is in a scanned file |
| `fresh_edge_ids.is_empty()` skips the sweep | Safe default, but a genuinely edge-less project never prunes. Callers must always pass the real set |
| Existing stores keep per-insert HNSW | `deferred` only helps fresh stores; migrating an existing project needs dropping and rebuilding the index. This repo's store was rebuilt from empty at `610fd05` |
| 9 HNSW index definitions in schema | Only one dimension is populated; the other 8 are empty definitions (`idx_chunks_embedding_384/768/1536/…`) |
| `documents()` memory | SCIP parsing is already streaming; the 3.5 GB peak is the node/edge structures (~18.5 KB/node). `CODEGRAPH_SCIP_MAX_BYTES` is the opt-in guard |
| `scip` CLI has no merge | Merging is unnecessary now that multiple indexes are supported directly |
| `pnpm dev` during indexing | It wrecked one measurement (`lsp_nodes_enriched` 42,874 → 2,556). Never measure with a dev server running |
| paperclip's LSP-enriched count | 42,874 (LSP) vs 44,587 (SCIP). The earlier 2,556 sample was the polluted run |
| LSP determinism | Still open, see `docs/LSP_DETERMINISM.md`; SCIP is the intended exit |
| Stale SCIP error message | **Fixed in `291192f`**: the import now warns `SCIP index … is older than N of M captured sources` (mtime comparison, non-fatal) and position failures carry `<path>:<line>:<col> (end …, encoding N) … the index is probably stale`. The mtime heuristic can misfire on `git checkout`/`touch` — it only warns, so that is cosmetic |
| Occurrence-aware edge ids | ~31% of this repo's 96,733 edges share `(from,to,type)` with another edge (max 71 occurrences of one `references`). Deliberate (`edge.rs` keeps distinct call sites distinct), but degree/coupling metrics must `DISTINCT` or they over-count |

## Useful commands

```bash
# Build + install (always through make)
make install-dist-openai

# Multi-language index
CODEGRAPH_SCIP_INDEX="/abs/rust.scip,/abs/go.scip@easytier-go" \
  codegraph index . --index-tier balanced --force --stats-json /tmp/stats.json

# Generate indexes
rust-analyzer scip .                                  # Rust (~75s for 600 files)
NODE_OPTIONS=--max-old-space-size=16384 scip-typescript index --pnpm-workspaces
docker run --rm -v <go-module>:/src -v /tmp:/out -v /tmp/opencode/scip-go:/usr/local/bin/scip-go:ro \
  -w /src -e GOPROXY=https://goproxy.cn,direct golang:1.23-alpine \
  sh -c "apk add --no-cache git >/dev/null; scip-go --output /out/go.scip"

# Read a run (`phase_ms` also carries counters such as
# resolution_fuzzy_len_pruned, so filter to real durations)
jq '.phase_ms | to_entries | map(select(.key|endswith("_ms")))
    | sort_by(.value) | reverse | .[0:8]' /tmp/stats.json
```

Flags that matter: `CODEGRAPH_SCIP_INDEX`, `CODEGRAPH_SCIP_MAX_BYTES`,
`CODEGRAPH_SCIP_TRUST_SOURCE`, `CODEGRAPH_VECTOR_INDEX_MODE` (default `deferred`),
`CODEGRAPH_FUZZY_CANDIDATE_LIMIT` (default 0 = unlimited), `CODEGRAPH_NATIVE_ANALYSIS`,
`CODEGRAPH_LSP_SKIP_TEST_FILES` (default on; only stories/generated/mocks/fixtures),
`CODEGRAPH_PROVIDER_CONCURRENCY` (4 on this host).

## References

- `docs/INDEXING_PIPELINE.md` — every stage, its `phase_ms` key, and the controls
- `docs/LSP_DETERMINISM.md` — why balanced-mode indexing is not reproducible
- `docs/INTERNAL_FUNCTION_MIGRATION.md` — `fn::*` → Rust status and the caller-switch plan
- `deploy/README.md` — SurrealDB 4.x requirement, memory budgets, migration
