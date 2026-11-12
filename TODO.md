# TODO — indexing performance & LSP → SCIP migration

Context for a fresh session. State below reflects commit `0a075d3` (2026-11-12).

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
  (`crates/codegraph-mcp/src/analyzers/scip.rs:183`): `byte_position` rejects a recorded column
  that is past the line or off a char boundary. The message names neither the document nor the
  cause. Check with `find crates -name '*.rs' -newer .codegraph/scip/rust.scip`.
- **`phase_ms` mixes counters with durations.** `resolution_fuzzy_len_pruned=130849978` is a
  count, not milliseconds, so `jq '.phase_ms | sort_by(.value)'` misreads the run.
- **`ingestion_write_test` flakes under parallel threads.** `bulk_ingestion_is_repeatable_and_
  cleanup_is_typed_and_scoped` fails ~2/20 on a clean HEAD with `The query was not executed due
  to a failed transaction`; `--test-threads=1` is stable. Pre-existing `mem://` engine issue.

## TODO

### 0. Let `codegraph index` generate SCIP itself (today: manual script only)

Current workflow is manual: run `scripts/scip-index.sh`, export
`CODEGRAPH_SCIP_INDEX`, then index. A fresh session that just runs `codegraph index .`
silently takes the LSP path (21 min on a 10k-file project instead of ~6 s).

`codegraph index --scip-auto` should:

- detect languages (`Cargo.toml` / `package.json` / `go.mod`)
- run the matching indexer into `.codegraph/scip/`, cached by content fingerprint
  (regenerate only when sources or the indexer version change)
- fall back to LSP with a warning when an indexer is missing or fails
- offer `--scip-strict` to fail instead of falling back

Acceptance: on a fresh clone with `rust-analyzer` installed, plain `codegraph index .`
produces a SCIP-backed run with no environment variables set. Until then, `AGENTS.md`
tells agents to run `scripts/scip-index.sh` first.

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

Still open: the EasyTier acceptance target (`lsp_nodes_enriched` ≥ 15,673 with Rust+Go SCIP
plus Kotlin/C LSP) needs that project's indexes generated and measured.

### 2. TypeScript on EasyTier

`scip-typescript 0.6.2` is installed globally. EasyTier has 79 `.ts` + 19 `.js` files
(`easytier-js`, `easytier-gui`). Add `ts.scip@<subdir>` and measure. Expected: modest gain
(98 files) but completes the multi-language picture on this project.

Note: `scip-typescript` needs `pnpm install` in that package and a resolvable tsconfig.

### 3. Kotlin and C

- Kotlin (13 files): `scip-java` requires JDK 11+ (host has only JDK 8) plus Kotlin support.
- C (1 file): `scip-clang` requires `compile_commands.json`.
Low value on EasyTier; revisit only if other projects need them.

### 4. CI: generate indexes per language

Long term, `codegraph index` should not depend on a developer having produced `index.scip`
by hand. Decide: generate in CI and cache, or run the indexers as a pre-step.

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
| Stale SCIP error message | A stale `index.scip` fails as `SCIP column is invalid or document position encoding is unspecified` with no document name. Consider comparing the index mtime to the source tree and reporting "SCIP index is older than N sources" instead |
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
