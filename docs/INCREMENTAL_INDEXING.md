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

## SCIP enrichment on partial indexes

Compiler-grade enrichment comes from the `CODEGRAPH_SCIP_INDEX` you supply, not from a
long-running language server. A partial re-index reads the same index and applies it to the
files it parses:

- Files the index covers get qualified names and definition/reference edges.
- Files it does not cover keep AST-only results.
- No language server starts for the partial scope, so there is no workspace-sized memory
  cost to bound.

For a hard machine-level bound, run indexing under a cgroup, for example
`systemd-run --scope -p MemoryMax=8G codegraph index .` or a container with
`--memory=8g`.

If an index has gone stale, regenerate it (`scripts/scip-index.sh` or the language's own
indexer) and re-run. The import warns when the artifact is older than the captured sources
and fails with the document and encoding named if a recorded position falls outside the
file.

### Tier and edge filtering

SCIP enrichment runs on every tier. The index tier only filters extracted edge types:

| Tier | SCIP enrichment | `Uses` edges | `References` edges |
|---|---|---|---|
| `fast` | when an index is supplied | filtered | filtered |
| `balanced` | when an index is supplied | kept | filtered |
| `full` | when an index is supplied | kept | kept |

A project with no SCIP index produces the same graph as before SCIP existed: AST-only,
with no compiler-derived definition edges.

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
