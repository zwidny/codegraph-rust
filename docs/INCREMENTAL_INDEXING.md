# Incremental Indexing

`codegraph index <path>` accepts a project root, a subdirectory, or a single file.
When the scanned path is narrower than the project, CodeGraph updates only that
scope and leaves the rest of the graph intact.

```bash
codegraph index /path/to/repo              # full project
codegraph index packages/core              # one directory
codegraph index packages/core/src/tool.ts  # one file
```

## Project identity vs. scan scope

The **scan scope** is the path you pass. The **project identity** (`project_id`)
is resolved separately:

1. `--project-root <PATH>`
2. `CODEGRAPH_PROJECT_ID`, when it names an existing directory
3. the nearest ancestor containing `.git` (preferred) or `.codegraph`
4. the scanned path itself (standalone directory / outside a repository)

So `codegraph index packages/core` run inside a git repository writes into that
repository's project instead of creating a separate
`/abs/path/packages/core` project.

```bash
# Explicit identity when the defaults do not fit
codegraph index packages/core --project-root /path/to/repo
```

## What a partial re-index does

1. **Parses only the scanned files.** Unchanged files are not re-parsed.
2. **Resolves edges against the project catalog.** Symbols for files outside the
   scope are loaded from storage, so a file that calls into the rest of the repo
   keeps its resolved edges.
3. **Reconciles only the scanned scope.** Files outside the scope are never
   treated as deleted, and their nodes, edges and chunks are untouched.
4. **Redirects inbound edges.** Node ids embed the source line, so moving a symbol
   changes its id. Edges in other files that pointed at the old id are repointed
   to the new id when the file, name and node kind are unchanged.

## File paths are normalized

Stored `file_path` values are project-relative, posix-style keys
(`packages/core/src/tool.ts`). This makes `./src/a.rs`, `src/a.rs` and an absolute
path resolve to the same file, regardless of the working directory used to run
`codegraph index`.

Because node ids embed the path, normalizing it changes ids for existing rows.
Older indexes converge automatically on the next run for that file, but a clean
pass is recommended after upgrading:

```bash
codegraph index /path/to/repo --force
```

## Language servers and memory

Language servers load a whole workspace (tsserver reads the tsconfig project,
rust-analyzer the Cargo workspace), so their memory is not bounded by the files you
re-index.

Partial re-indexes therefore **disable language servers by default**. Force them on
with `CODEGRAPH_LSP_ON_PARTIAL=1` when you accept the memory cost.

For full indexes, these controls bound the damage:

| Variable | Default | Effect |
|---|---|---|
| `CODEGRAPH_LSP_NODE_HEAP_MB` | `4096` | `NODE_OPTIONS=--max-old-space-size` for Node servers (tsserver, pyright); an explicit `--max-old-space-size` already in `NODE_OPTIONS` wins |
| `CODEGRAPH_LSP_CLANGD_BACKGROUND` | off | Set `1` to keep clangd's background index (default passes `--background-index=false`) |
| `CODEGRAPH_LSP_KLS_GRADLE_DAEMON` | off | Set `1` to keep the Gradle daemon KLS spawns; by default it is disabled (`-Dorg.gradle.daemon=false`) so a finished index does not leave a multi-GB JVM behind |
| `CODEGRAPH_LSP_MIN_FREE_MB` | `4096` | Skip language servers when available memory is below this |
| `CODEGRAPH_LSP_DEFINITIONS` | off | Force definition resolution on `balanced` (adds edge resolution, slower; `full` already enables it) |
| `CODEGRAPH_LSP_INITIALIZE_TIMEOUT_SECS` | `180` | `initialize` deadline (cold import of a large workspace) |
| `CODEGRAPH_LSP_REQUEST_TIMEOUT_SECS` | `30` | Deadline for other LSP requests (documentSymbol/definition) |

A language-server crash (including a Node out-of-memory abort) no longer fails the
run: the phase logs a warning and indexing completes with AST and
symbol-catalog results only.

For a hard machine-level bound, run indexing under a cgroup, for example
`systemd-run --scope -p MemoryMax=8G codegraph index .` or a container with
`--memory=8g`.

### Tier and LSP edge resolution

Language-server enrichment depends on the indexing tier:

| Tier | LSP mode | documentSymbol | definition edges |
|---|---|---|---|
| `fast` | off | no | no |
| `balanced` | `SymbolsOnly` | yes | **no (0 edges)** |
| `full` | `SymbolsAndDefinitions` | yes | yes |

So `N nodes enriched + 0 edges resolved` on `balanced` is expected, not a failure:
balanced only enriches node names. Use `--index-tier full` or
`CODEGRAPH_LSP_DEFINITIONS=1` if you need definition edges.

TypeScript 7 native preview ships `tsc --lsp --stdio` but no `lib/tsserver.js`, which
`typescript-language-server` requires. When the project's TypeScript has no tsserver,
indexing launches `tsc --lsp --stdio` instead; classic TypeScript still uses
`typescript-language-server`.

## Per-project databases (shared server)

A shared SurrealDB server now keeps **one database per project** by default. Previously
one `chunks` table (and one HNSW index) covered every project, so index builds grew with
the whole fleet (300k+ vectors) instead of the project being indexed.

- Database name: `<directory>_<short hash of the canonical path>`, e.g.
  `happy_3f9c2a1b7e40`; two checkouts with the same name get different hashes.
- The first `index` / `query` / MCP connection creates the database and applies the
  bundled schema automatically.
- `CODEGRAPH_DATABASE_PER_PROJECT=0` restores the old shared
  `CODEGRAPH_SURREALDB_DATABASE` behaviour.
- Existing data in the shared database is **not migrated**. Re-index each project
  (`codegraph index <root>`) to populate its own database; keep the old database until
  everything you need has been re-indexed.

Embedded stores (`surrealkv://`) are already one file per project and are unaffected.

## Vector index tuning

HNSW construction and chunk writes can be tuned for large shared stores:

| Variable | Default | Range | Effect |
|---|---|---|---|
| `CODEGRAPH_HNSW_M` | `12` | `2..=128` | Max links per node per layer. Lower = smaller/faster index, slightly lower recall. |
| `CODEGRAPH_HNSW_EFC` | `150` | `M..=4096` | Build-time candidate list. Lower = much faster build, lower index quality. |
| `CODEGRAPH_CHUNK_DB_BATCH_SIZE` | `32` | `1..=512` | Chunk rows per database write. Higher = fewer transactions/fsync, more memory per batch; values above 512 are clamped. |
| `CODEGRAPH_NODE_DB_BATCH_SIZE` | `config.batch_size` | `1..=512` | Node rows per write. |
| `CODEGRAPH_SYMBOL_DB_BATCH_SIZE` | `128` | `1..=512` | Symbol-embedding rows per write. |

`DEFINE INDEX IF NOT EXISTS` means an existing HNSW index is **not** rebuilt when these
values change. Remove it (`REMOVE INDEX idx_chunks_embedding_<dim> ON chunks;`) or use a
fresh per-project database for the new values to take effect.

## Limits

- **Rename or delete**: an inbound edge is redirected only when the symbol still
  exists under the same file, name and kind. Renamed or removed symbols keep
  their edges until reconciliation drops them (the correct outcome).
- **Unindexed changes elsewhere**: edges from files you did not re-index still
  describe the code as it was when those files were last indexed.
- **`metadata.target_node_id`** is not rewritten; resolution falls back to alias
  matching when the stored id no longer exists.
- **Cost**: a partial re-index adds one project-wide symbol query plus an
  in-memory catalog build. On a ~26k-node project this is on the order of a few
  hundred milliseconds.
- **`--watch` / daemon** still treats the directory passed to the daemon as the
  project root.

## Projects created before this behavior

Older runs may have created one project per subdirectory (for example
`/repo/packages/core` alongside `/repo`). Those are independent
`project_id` namespaces and are not merged automatically. Re-index the repository
root to rebuild the unified project; the stale per-directory projects can be
deleted separately once you confirm you no longer need them.

## Related tests

- `crates/codegraph-graph/tests/ingestion_write_test.rs`
  - `scope_leaves_files_outside_the_scanned_directory_untouched`
  - `scope_still_removes_files_that_disappeared_within_it`
- `crates/codegraph-mcp/tests/reconciliation_test.rs`
  - `inbound_edges_follow_symbols_that_only_shifted_lines`
  - `single_file_index_resolves_edges_against_the_project_catalog`
  - `incremental_catalog_matches_full_graph_and_removes_stale_definitions`
- `crates/codegraph-mcp-server/src/bin/codegraph.rs`
  - `project_root_*` CLI tests
