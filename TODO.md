# TODO — indexing performance & LSP → SCIP migration

Context for a fresh session. State below reflects commit `812d580` (2026-11-12).

## Where things stand

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

### 1. Per-language analyzer selection (highest priority)

Today `CODEGRAPH_SCIP_INDEX` disables LSP **globally**:

```rust
// indexer.rs ~1141
if analyzer_settings.lsp_enabled() && !analyzer_languages.is_empty() && scip_path.is_none()
```

Consequence: in a mixed-language project, languages without an indexer (Kotlin, C, and any
language whose SCIP file is missing) get **zero** enrichment — worse than before.

Needed: for each file, use SCIP when the index covers that language, otherwise fall back to
LSP for the remaining languages in the same run.

Acceptance: EasyTier with Rust+Go SCIP **plus** LSP for Kotlin/C must have
`lsp_nodes_enriched` ≥ 15,673 (Rust+Go only) and the Kotlin/C files enriched.

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
| Existing stores keep per-insert HNSW | `deferred` only helps fresh stores; migrating an existing project needs dropping and rebuilding the index |
| 9 HNSW index definitions in schema | Only one dimension is populated; the other 8 are empty definitions (`idx_chunks_embedding_384/768/1536/…`) |
| `documents()` memory | SCIP parsing is already streaming; the 3.5 GB peak is the node/edge structures (~18.5 KB/node). `CODEGRAPH_SCIP_MAX_BYTES` is the opt-in guard |
| `scip` CLI has no merge | Merging is unnecessary now that multiple indexes are supported directly |
| `pnpm dev` during indexing | It wrecked one measurement (`lsp_nodes_enriched` 42,874 → 2,556). Never measure with a dev server running |
| paperclip's LSP-enriched count | 42,874 (LSP) vs 44,587 (SCIP). The earlier 2,556 sample was the polluted run |
| LSP determinism | Still open, see `docs/LSP_DETERMINISM.md`; SCIP is the intended exit |

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

# Read a run
jq '.phase_ms' /tmp/stats.json | sort -k2 -n
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
