# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

CodeGraph indexes a codebase into a SurrealDB knowledge graph (AST nodes, edges, chunk embeddings) and serves it to AI clients over MCP as nine direct graph tools (`semantic_code_search`, `get_reverse_dependencies`, `get_transitive_dependencies`, `trace_call_chain`, `calculate_coupling_metrics`, `detect_circular_dependencies`, `find_complexity_hotspots`, `get_hub_nodes`, `plan_check`). It is a Rust workspace of 11 crates under `crates/`; the single shipped binary is `codegraph`, built from `crates/codegraph-mcp-server/src/bin/codegraph.rs`.

## Commands

**Workspace gotcha:** `default-members = ["crates/codegraph-core"]`, so a bare `cargo build` / `cargo test` / `cargo clippy` only touches `codegraph-core`. Always pass `--workspace` or `-p <crate>`.

```bash
# Fast hygiene pass
cargo check --workspace

# Build the binary (default features = ["daemon"] only: no embeddings)
cargo build --release -p codegraph-mcp-server --bin codegraph --features full
# What the install scripts do (install-codegraph*.sh):
cargo install --path crates/codegraph-mcp-server --bin codegraph --all-features --force

# Tests
cargo test --workspace                      # or: make test
cargo test -p codegraph-mcp                 # one crate
cargo test -p codegraph-mcp <test_name>     # one test by name filter
cargo test -p codegraph-graph --test schema_indexes_test   # one integration-test file

# Format / lint
cargo fmt --all                             # rustfmt.toml: max_width = 100
cargo clippy --workspace --all-targets --all-features -- -D warnings   # what CI runs
```

- `make lint` is effectively a no-op (clippy on `codegraph-core` only, with `-A clippy::all -A warnings`). Use the CI form above.
- On macOS, `make build-llvm` / `make test-llvm` link with `lld` for faster builds (requires `ld64.lld` on PATH, e.g. `brew install llvm`).
- Release profile is `lto = "fat"`, `codegen-units = 1`: release builds are slow. Use dev builds (or `--profile fast-dev`) while iterating.

### Stale references to ignore

- Makefile targets referring to `codegraph-api`, `high_perf_test/`, or `scripts/` point at things that no longer exist.
- `[workspace.metadata.cargo-features]` in the root `Cargo.toml` is documentation only; cargo does not consume it. The real feature flags are on `codegraph-mcp-server` (`ai-enhanced`, `server-http`, `daemon`, `embeddings-*`, `full`). `ai-enhanced` only forwards to `codegraph-mcp/ai-enhanced`; it does not gate MCP tool registration.
- `docs/README.md` mentions a `codegraph-cache` crate that has been removed.

### Tests that need external services

There are no `#[ignore]` tests; DB-dependent tests are gated by env vars and silently skip (pass) otherwise:

- `crates/codegraph-mcp/tests/graph_tools_smoke.rs` runs only when `CODEGRAPH_SURREALDB_URL` is set (plus optional `CODEGRAPH_SURREALDB_NAMESPACE|DATABASE|USERNAME|PASSWORD`, `CODEGRAPH_EMBEDDING_MODEL`, `CODEGRAPH_EMBEDDING_DIMENSION`), against an already-indexed database.
- `crates/codegraph-graph/tests/semantic_search_nodes_via_chunks_test.rs` runs only with `CODEGRAPH_RUN_SEMANTIC_SEARCH_NODES_VIA_CHUNKS_TEST=1` (uses in-memory SurrealDB).
- `crates/codegraph-mcp-server/tests/http_server_integration.rs` exercises the HTTP transport in-process (needs `server-http`).

### Running locally

```bash
codegraph db-check                      # opens the store, applying the bundled schema if the store is new
codegraph init /path/to/project         # choose project hooks, merge agent instructions, then index recursively
codegraph init . --hooks none --no-index # instruction setup only; no providers or database
codegraph index /path/to/project -r -l rust,typescript --index-tier fast|balanced|full
codegraph start stdio --watch           # MCP server over stdio, with re-index-on-change daemon
```

Storage should use a **shared SurrealDB server** (`CODEGRAPH_SURREALDB_URL`, container or self-hosted; apply `schema/codegraph_v2.surql` yourself, the schema is not applied automatically in server mode). Server mode has no per-client file lock: read-only MCP clients and `codegraph index` can run concurrently, and several clients can share one database. Without the URL, `codegraph` falls back to an embedded SurrealKV store at `<project>/.codegraph/db` (`SurrealDbConfig::for_project` in `crates/codegraph-graph/src/surrealdb_storage.rs`), which gets `schema/codegraph_v2.surql` applied on first open; that fallback is **compatibility-only and substantially slower**, and its engine locks the directory, so only one process per project. This repository's `.env` uses the shared mode (`ws://127.0.0.1:8010`, Docker `surrealdb/surrealdb` with SurrealKV), so do not assume the single-writer restriction applies here and do not recommend the embedded store.

Config resolution (`crates/codegraph-core/src/config_manager.rs`): `./.codegraph.toml`, then `~/.codegraph/config.toml`, overridden by `CODEGRAPH_*` env vars. Environment files are layered: `./.env` loads first and `~/.codegraph.env` loads second as a fallback (dotenv never overwrites an already-set variable), so a project `.env` only needs the keys that differ from the machine-wide defaults. Previously the project `.env` suppressed `~/.codegraph.env` entirely; keep that fallback intact. See `.env.example` and `config/example.toml`. The `[llm]` section was removed in v0.3.0 (no shipped code calls an LLM) and a leftover block is ignored silently. The loader ignores unknown TOML keys silently, and `[rerank]`, `[performance]`, `[logging]` are parsed but only shown in status output.

Project init runs before application configuration or the async runtime. `project_init.rs` offers Claude/Codex/both/none, reuses existing CodeGraph hooks, validates both guide files and selected settings, then merges owned instruction blocks. `agent_hooks.rs` preserves unrelated project settings and reuses Codex inline TOML hooks without rewriting TOML. `--no-index` stops after setup; otherwise init loads the selected project's `.env` and configuration and uses the normal index command. Explicit `--config` paths stay relative to the invoking directory. This is separate from global `codegraph config init`; no user-level hooks are created. Test with `cargo test -p codegraph-mcp-server --lib` and `cargo test -p codegraph-mcp-server --test project_init_integration`.

## Architecture

### Crate layering

Dependencies point downward; do not add upward imports (they create cycles):

- `codegraph-core`: types, config, `CodeNode`. Everything depends on it.
- `codegraph-parser` (tree-sitter) and `codegraph-graph` (SurrealDB storage) depend only on core. `codegraph-vector` (embedding providers, reranking) sits on graph.
- `codegraph-mcp-core`: shared MCP types (`ContextTier`, `AgentArchitecture`), on core only. `codegraph-mcp-tools` (inner graph tools) adds graph + vector.
- On top of mcp-tools / graph: `codegraph-mcp` (indexer + analyzers).
- `codegraph-mcp-daemon` (file watching) depends on `codegraph-mcp`.
- `codegraph-mcp-server` (CLI, stdio/HTTP transports, MCP tool entrypoints) is the only crate that depends on everything.
- `codegraph-concurrent` and `codegraph-zerocopy` are standalone utility crates.

Provider and backend code is heavily `#[cfg(feature = ...)]`-gated, so a change can compile under default features and break under `--all-features` (or vice versa). Check with the features that actually enable the code you touched.

### Flow 1: indexing (`codegraph index`)

Explicit `--batch-size` wins over `CODEGRAPH_EMBEDDINGS_BATCH_SIZE`, legacy `CODEGRAPH_EMBEDDING_BATCH_SIZE`, TOML `[embedding] batch_size`, then default 64. Environment aliases resolve in `ConfigManager`; explicit positive values are not memory-tuned, and the indexer passes its final row limit to provider engines and the submitted-text cache. Ollama/LM Studio have no extra 256-text cap. Token/byte/provider limits can split inference requests; DB writes use separate bounds (chunk writes default to at most 32 rows). Logs distinguish those limits. The full-feature `index_cli_integration` mock verifies actual request sizes and precedence without live providers.

Embedding preparation is model-aware for Ollama: `/api/show` and `/api/ps` resolve
context; `CODEGRAPH_OLLAMA_NUM_CTX` selects a bounded request context. Recognized
models load matching publisher tokenizers (tokenizer-only download/cache); unknown
models require `CODEGRAPH_TOKENIZER_PATH` or `CODEGRAPH_TOKENIZER_REPO`. Complete
counts include retrieval prefixes and special tokens, with truncation/padding
removed. Nomic text uses document/query prefixes; Qwen3/Nomic Code queries use
retrieval instructions. Requests use `truncate=false` and reject context overflow.

Fitting AST node spans stay intact. Oversized units use syntax boundaries from
`codegraph-parser::chunk_boundaries`, then lossless line/UTF-8 fallback for oversized
leaves; adjacent pieces merge within budget. `CODEGRAPH_CHUNK_MAX_TOKENS` lowers the
complete-input target (legacy `CODEGRAPH_MAX_CHUNK_TOKENS` has lower precedence),
`CODEGRAPH_CHUNK_SMART_SPLIT=0` disables AST cuts, and overlap is token-counted/shrunk
to fit. `CODEGRAPH_EMBEDDING_SKIP_CHUNKING=1` keeps units whole and errors before
inference if oversized. The default request token budget grows to at least the
resolved input context; explicit token/byte bounds remain authoritative. Chunk,
input-policy and catalog identities invalidate prior preparation artifacts.
`EmbeddingGenerator::with_config`, `with_auto_from_env` and chunk-planning APIs
return `Result`. Changing the advanced backend clears its old provider policy/cache.
No semchunk paths remain. Mock/unit regressions cover actual request limits/prefixes,
strict overflow, unknown models, warm-cache skip errors and Unicode/source recovery.

Jina uses `JINA_API_TASK` > legacy `JINA_TASK` > TOML `jina_task` > `auto` (v4
`code.passage`, v3/v5 `retrieval.passage`). Explicit tasks remain intact. Queries
pair passage tasks with the matching query task and retain symmetric tasks.
Batch/query/health calls share the request builder; `JINA_NORMALIZED` defaults to
true for v3/v5/CLIP v2 and is omitted for v4. V5 small/nano dimensions are
1024/768. Invalid known task/model pairs propagate initialization errors; permanent
embedding HTTP 4xx errors fail once (408/429 remain retryable). Actual request
options participate in vector and reconciliation cache identities. Verify with
`cargo test -p codegraph-vector --features jina --lib jina_provider::tests` and
the full-feature `index_cli_integration` Jina mock; no live API keys are needed.

`bin/codegraph.rs` `handle_index` → `ProjectIndexer::index_project` in `crates/codegraph-mcp/src/indexer.rs`:

1. Collect/prune files and capture immutable, hashed source snapshots with bounded retained bytes and temporary spill files. Fingerprint sources, build/doc inputs and output policy. All entry points (full, single-file, delete and watch) use complete-project reconciliation. `--force` ignores the prior catalog; preparation does not wipe live records.
2. Reuse versioned compressed AST artifacts; pooled tree-sitter parsers honor worker limits, cancellation and early tier gating. FastML state is file-local. Cargo metadata preparation overlaps parsing.
3. Run cached analyzer stages in `crates/codegraph-mcp/src/analyzers/`: Cargo build context, rustdoc/API enrichment, module linking, scope-aware AST dataflow, docs/contracts and architecture. Import a validated SCIP artifact to substitute compiler definitions/references; there is no language-server path. Preserve direct target IDs and ambiguity.
4. Keep fitting AST units whole and plan oversized chunks in parallel with the complete provider token budget. Exact submitted-text caches deduplicate persistent and concurrent inference. Provider-wide permits, row/token/byte budgets and blocking local-runtime scheduling bound work. Independent `sync|deferred|off` embedding/semantic policies control inference without changing extraction tier.
5. Upsert only changed nodes/chunks/file metadata and new stable edges through bounded durable writers. Resolve exact/contextual/normalized targets, then indexed lexical candidates, then only genuinely unresolved semantic candidates. Semantic scoring in `semantic_scoring.rs` caches vector norms and runs independent targets in the shared worker-limited CPU pool through `spawn_blocking`. It retains scalar arithmetic, candidate order, threshold/tie rules and bit-identical scores against the frozen reference. Resolution `phase_ms` separates exact/lexical, candidates, symbol embeddings, scoring and writes. Persist method/similarity provenance; scores are not calibrated probabilities.
6. Flush acknowledged writes, remove stale project-scoped records, verify requested vector indexes and revalidate final inputs. Persist the project ready marker last. Graph readiness, pending/off inference and vector-index readiness remain separate. Deferred jobs survive restart; `codegraph index <root> --complete-deferred` reconciles current inputs before completing them.

The index tier controls analyzers and early extraction: `fast` (default; core AST edges, `Uses`/`References` filtered), `balanced` (adds build context, enrichment, module linking, docs; `References` filtered), `full` (all analyzers; no edge filtering). Enabled-feature inference defaults to synchronous independently of tier. `CODEGRAPH_ANALYZERS=0` disables analyzers. Compiler-grade enrichment is only SCIP: pass `CODEGRAPH_SCIP_INDEX` (or `--scip-auto`); a language without an index stays AST-only. `scripts/scip-index.sh` generates per-language indexes. Do not add language-server processes or per-language tool checks. Details: `docs/architecture/indexing-analyzers.md`, `docs/specifications/indexing_tiers.spec.md`.

Performance and correctness controls are documented in `docs/indexing-performance-work.md`; use `docs/indexing-benchmarks.md` for reproducible cold/warm/edit/rename/delete/doc/manifest/tier scenarios and same-model retrieval probes. Preserve compatible SHA node IDs, canonical cache fingerprints, source spans, ambiguity and completion barriers. Never silently change the selected model/backend or mix query/document tasks in caches. Reduced precision, candidate caps, alternate splitters and compiler imports stay opt-in until relevant quality measurements support adoption.

Targeted offline checks include reconciliation/watch tests, `cargo test -p codegraph-mcp --features embeddings --test deferred_policy_test`, `cargo test -p codegraph-graph --features surrealdb --test ingestion_write_test`, parser/vector unit tests and the Python benchmark regressions. Strict workspace Clippy currently fails on existing core/zerocopy diagnostics; do not describe non-strict targeted linting as a strict workspace pass. Debug synthetic measurements do not establish production speedups or live-model quality.

### Flow 2: MCP graph tools

`crates/codegraph-mcp-server/src/official_server.rs` exposes nine direct tools via a
single rmcp `#[tool_router]` (the SDK does not support multiple router blocks). Each tool
validates its parameters, runs the corresponding graph function and returns raw rows:

1. `semantic_code_search` performs hybrid retrieval and returns `node_id` values.
2. The node tools (`get_reverse_dependencies`, `get_transitive_dependencies`,
   `trace_call_chain`, `calculate_coupling_metrics`, `detect_circular_dependencies`,
   `find_complexity_hotspots`, `get_hub_nodes`) call the same inner graph functions in
   `codegraph-mcp-tools` (`GraphToolSchemas` / `GraphToolExecutor`).
3. `plan_check` validates a draft nodes/edges/constraints graph offline; it needs no
   database.
4. Those call `fn::*` SurrealQL functions defined in `schema/codegraph.surql`, wrapped by
   `crates/codegraph-graph/src/graph_functions.rs`.

There is no server-side agent or model call in this path. The former Rig backend and its
tier prompts were removed in v0.3.0; historical design notes remain in
`docs/architecture/agent-memory-design.md`.

### Schema

Three SurrealDB schemas must stay in sync with the storage layer and with each other: `schema/codegraph_v2.surql` (default; SurrealDB 3.x-optimised functions, tested end to end by `crates/codegraph-graph/tests/schema_v2_test.rs`), `schema/codegraph.surql` (the original, `CODEGRAPH_SCHEMA=v1`), and `schema/codegraph_graph_experimental.surql` (selected with `CODEGRAPH_USE_GRAPH_SCHEMA=true` + `CODEGRAPH_GRAPH_DB_DATABASE`). `crates/codegraph-graph/tests/schema_indexes_test.rs` statically validates them (project-scoped indexes, required `fn::*` functions, SurrealQL parsing constraints) and `schema_runtime_test.rs` applies each to an in-memory engine; run both after any schema edit. A new graph function generally needs: the `fn::` definition in each schema, a wrapper in `graph_functions.rs`, and a schema + executor entry in `codegraph-mcp-tools`. SurrealQL gotcha: `LET` inside a `FOR` body is block-scoped in 3.x, so iterative walks use `array::fold` (see `fn::expand` in v2).

## Conventions

- About half the source files start with two `// ABOUTME:` comment lines summarising the file's purpose (`# ABOUTME:` in shell scripts). Follow this on new files.
- Analyzer output must be deterministic, scoped by `project_id`, and carry provenance metadata (analyzer name, confidence).
- Adding a language touches three layers: the enum in `codegraph-core/src/types.rs`, grammar registration in `codegraph-parser/src/language.rs`, and an extractor in `codegraph-parser/src/languages/`; then update `docs/SUPPORTED_LANGUAGES.md`.
- User-facing changes to configuration, providers, or tiers should update `docs/AI_PROVIDERS.md`. `CONTRIBUTING.md` has the full "what to change where" map.
- `rules-for-claude-code/codegraph_rule.md` is guidance for *consumers* of the CodeGraph MCP server, not for developing this repo.

<!-- codegraph:begin -->
# codegraph

CodeGraph is a CLI tool available through Bash as the `codegraph` command. Run the
commands below using your Bash or shell execution tool. Check that CodeGraph is
on PATH with `command -v codegraph`.

When CodeGraph is available and the project is indexed, start code exploration
with a direct graph query instead of a broad grep/rg search:

- `codegraph query <symbol>` locates a definition; add `--callers` (who calls it),
  `--callees` (what it calls), `--kind function|struct|...` or `--file <path>` to
  narrow the results.
- `codegraph plan check <file>` validates a draft node/edge graph offline;
  `codegraph plan verify <file>` compares it with the indexed code graph.
- `codegraph index .` re-indexes incrementally (`--scip-auto` also generates and
  imports a SCIP index); `codegraph status` reports server/provider/database state.

There is **no `agent` subcommand**. For a synthesized answer, compose these
queries yourself and read the returned source locations.

Run from the indexed project root, or append `--project /path/to/project`; retain
the indexed `--project-id` if one was configured. Prefer the default JSON output:
inspect source locations, then read the specific files/lines before editing. After
changes, verify against current source and run relevant tests; the index may lag
uncommitted work.

If CodeGraph is unavailable, the project is not indexed, a command fails or returns
insufficient evidence, fall back to targeted source reads and rg/grep.

**Trigger.** If the question touches this repository's source — structure, location,
relationship, call flow, or behavior — the first tool call must be CodeGraph, and it
applies to the whole task (exploration, verification, debugging, locating code before
an edit). "I just want to confirm" or "I roughly know where it is" is not an exemption.

Decide by what you hold, not by how fast grep feels:

- Symbol - a function, type, module, trait, field or command name -> use codegraph.
  `grep "analyze_cargo_workspace"` is a symbol lookup, so it is not a fallback.
- Text - a string literal, log line, error message, config key, file path or a
  verbatim quote from a comment -> grep is correct. `grep "SURREALDB_URL"` is text.
- Knowing the name already is the reason to use codegraph, not the reason to grep.
- Every fallback used to locate source must state why in the same reply; an unstated
  fallback is a violation even when the result is correct.
- Fallback is allowed only when the project has no `.codegraph/db`, or the command
  fails / returns empty / visibly disagrees with the source, or what you hold is text
  or a known exact file path.
- Per-turn self-check: if this turn's reads, greps or listings landed on a source
  location, name the CodeGraph call that preceded them.

Do not install CodeGraph, download models or reindex solely to satisfy these
instructions.
<!-- codegraph:end -->
