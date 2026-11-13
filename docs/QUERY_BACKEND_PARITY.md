# Making `query` and `plan` backend-neutral (B2)

Status: **implemented**. `query` and `plan` run over `IndexStorage`; Postgres is the default
backend and `surrealdb` is an opt-in feature. The design notes below are kept because they
explain why the eight methods look the way they do.

## Why this exists

The index write path is backend-neutral (`IndexStorage` + `CODEGRAPH_BACKEND`). Two
user-visible commands still talk to SurrealDB directly, so a Postgres-only build would have to
compile them out:

| Command | Entry point | What it touches |
|---|---|---|
| `codegraph query` | `crates/codegraph-mcp-server/src/query_cli.rs` (188 lines, 3 SurrealQL statements) | `nodes` by name/prefix, `edges` joined to `nodes` for callers/callees |
| `codegraph plan` | `crates/codegraph-mcp-server/src/plan_cli.rs` (`store`/`show`/`drop`/`verify`) | `nodes`/`edges` scoped to `project_id = plan:<id>`, plus `project_metadata.metadata` |

`codegraph plan check` (`run_offline`) does not touch the database and is unaffected.

## What the two commands actually need

Measured from the current SQL, not guessed:

### `query_cli.rs`

1. `SELECT name, node_type, language, file_path, start_line, end_line FROM nodes
    WHERE project_id = $project AND (name = $symbol OR string::starts_with(name, $symbol)) LIMIT $limit`
2. callers: `edges` where `to.name = target` and `edge_type = 'calls'`, projecting
   `from.{name,node_type,file_path,start_line}` plus `metadata.resolution_method`
3. callees: the mirror image of 2

### `plan_cli.rs`

| Operation | Current behaviour |
|---|---|
| `store` | `clean_project_data(plan:<id>)` when `--force`, then write draft `CodeNode`s and `CodeEdge`s with `project_id = plan:<id>` |
| `count_plan_nodes` | `count()` over `nodes` for that project |
| `show` | read `nodes` (`id`, `name`, `metadata`) and `project_metadata.metadata.plan_constraints` |
| `verify` | `load_code_index`: every node's `(id, name, file_path)` and every edge's `(from, to)`, then diff against the draft |
| `drop` | `clean_project_data(plan:<id>)` |

Note the plan draft reuses the **`nodes`/`edges` tables** with a `plan:<id>` project scope. That
is why most of `plan` needs no new storage concepts at all: `upsert_nodes`, `upsert_edges`,
`node_count` and `upsert_project_metadata` already exist on `IndexStorage`.

## New `IndexStorage` methods (8)

```rust
pub struct SymbolMatch {
    pub name: String,
    pub node_type: Option<String>,
    pub language: Option<String>,
    pub file_path: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
}

pub struct CallRelation {
    pub symbol: String,
    pub node_type: Option<String>,
    pub file: String,
    pub line: Option<i32>,
    pub resolution: Option<String>,
}

pub struct PlanNodeRow {
    pub id: String,
    pub name: String,
    /// Already decoded: backends store metadata differently (see below).
    pub metadata: Option<serde_json::Value>,
}
```

| Method | Used by | Postgres shape |
|---|---|---|
| `find_nodes_by_name(project, symbol, limit) -> Vec<SymbolMatch>` | query | `name = $2 OR name LIKE $2 \|\| '%'` |
| `callers_of_name(project, target, limit) -> Vec<CallRelation>` | query | `edges JOIN nodes ON edges.from_id = nodes.id WHERE nodes.name = $2 AND edge_type = 'calls'` |
| `callees_of_name(project, target, limit) -> Vec<CallRelation>` | query | mirror, joining `to_id` |
| `node_identities(project) -> Vec<(String, String, String)>` | plan verify | `SELECT id, name, file_path FROM nodes` |
| `edge_endpoints(project) -> Vec<EdgeEndpoint>` | plan verify/show | `SELECT from_id, to_id, edge_type FROM edges` (type is needed so `plan show` round-trips the kind) |
| `nodes_with_metadata(project) -> Vec<PlanNodeRow>` | plan show | `SELECT id, name, metadata FROM nodes` |
| `project_metadata_value(project) -> Option<serde_json::Value>` | plan show | `SELECT metadata FROM project_metadata` |
| `delete_project(project) -> u64` | plan store/drop | `DELETE` from all five tables for the project, in one transaction |

## The one real backend difference: `metadata`

- SurrealDB stores `nodes.metadata` as a **compressed JSON string**; readers call
  `codegraph_core::decompress_json`.
- Postgres stores the same attributes as **`jsonb`** (`code_node_to_row` writes
  `serde_json::to_value(&node.metadata.attributes)`), so no decompression is involved.

This is exactly why `nodes_with_metadata` returns `Option<serde_json::Value>` and why the
backend, not the caller, resolves the representation. `query_cli` projections must not read
`metadata` directly for the same reason.

A second, smaller difference: SurrealDB's `metadata.resolution_method` lives inside the same
compressed blob and is read with `metadata.resolution_method` in SurrealQL; in Postgres it is
`metadata->>'resolution_method'`.

## Order of work

1. Add the 8 methods to `IndexStorage` + both backends + integration tests in
   `postgres_storage.rs` (`#[ignore]`, real pgvector).
2. Rewrite `query_cli.rs` and `plan_cli.rs` over `Arc<dyn IndexStorage>`, choosing the concrete
   store from `CODEGRAPH_BACKEND` (the same selector the indexer uses).
3. Only then drop `features = ["surrealdb"]` from `codegraph-mcp` / `codegraph-vector` and add
   `#[cfg(feature = "surrealdb")]` where concrete types remain
   (see `docs/BACKEND_EVALUATION.md`, "Removing the hard SurrealDB dependency").
4. Verify the three build shapes: `--no-default-features --features postgres`,
   the default, and `--features surrealdb`.

## Risks

| Risk | Mitigation |
|---|---|
| `delete_project` is destructive; a wrong `project_id` wipes a real index | Tests use `plan:`-prefixed ids only; the method is not reachable from `index` |
| `name LIKE $2 || '%'` treats `%` and `_` in a symbol name as wildcards | Escape `%`, `_`, `\` before building the pattern; test a symbol containing `_` |
| Plan drafts share the `nodes` table, so a plan id must never collide with a project id | `draft_project_id` already prefixes `plan:`; keep that invariant and assert it in a test |
| `limit` semantics differ (`LIMIT` in SurrealQL vs Postgres) | Both clamp to 1..=1000 in the implementation |
