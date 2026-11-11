# Migrating `fn::*` stored procedures into Rust

Status: in progress. First slice landed (`codegraph-graph/src/analysis/`).

## Why

`schema/codegraph_v2.surql` defines 25 `fn::*` functions and 17 in `codegraph.surql`.
They hold the analysis logic for the whole product: circular dependencies, coupling
metrics, hub nodes, call chains, kNN search, k-means clustering.

That placement causes four concrete problems:

1. **Not unit-testable.** Every rule needs a live datastore; a wrong `array::intersect`
   is only visible end-to-end.
2. **Hard SurrealQL limits.** `fn::get_hub_nodes` documents that `array::concat` output is
   capped at 1 MiB, so it can only rank the top 500 nodes per direction. Rust has no such
   cap and can count exactly.
3. **Backend lock-in.** Swapping SurrealDB currently means re-implementing all 25
   functions in a second dialect.
4. **Debuggability.** A failing analyzer reports a SurrealQL error, not a stack frame.

The database should be storage plus indexes. Algorithms belong in Rust.

## Target layering (validated with `plan_check`)

```
consumer   McpTools            (codegraph-mcp-tools)
api        AnalysisApi         (codegraph-graph/src/graph_functions.rs)
query      GraphQueries        (load subgraph / KNN candidates)
algorithm  AnalysisCore        (codegraph-graph/src/analysis, pure functions)
model      GraphSnapshot       (EdgeRow, NodeRow)
storage    SurrealSchema       (tables, indexes, RELATION, access)
```

Constraints: `acyclic` and `layering` over the order above. `AnalysisCore` must never
depend on `GraphQueries`, so it stays a pure function library.

**Boundary that does not move:** KNN candidate retrieval still has to send SurrealQL
(`<|K,EF|>`, `vector::distance::knn`) because the operator needs literal K/EF. Rust owns
scoring, dedupe, context assembly and every graph algorithm.

## Batches

| # | Functions | Risk | Status |
|---|---|---|---|
| 1 | `mutual_pairs` (cycles), `degrees`/`hubs`, `coupling` | low | done |
| 2 | `expand`, `get_transitive_dependencies`, `get_reverse_dependencies`, `trace_call_chain` | medium | done |
| 3 | `get_complexity_hotspots`, `find_nodes_by_name`, `get_top_directories`, `count_nodes_for_project` | low | hotspots done; the other three pending |
| 4 | `knn_chunks`, `knn_nodes`, `semantic_search_*`, `search_snippets` | medium-high (keeps HNSW SQL) | not started |
| 5 | `kmeans` | medium | done |
| 6 | helpers: `edge_types`, `parse_record_id`, `resolve_node`, `node_info`, `edge_context` | low | not started |

### Known differences for batch 2 (deliberate, do not "fix" silently)

| Difference | Detail | Action |
|---|---|---|
| `chunks:` start ids | `fn::resolve_node` maps `chunks:x` to its `parent_node`; the native walk only resolves `nodes:`. | Add a follow-up task; the native methods have no caller yet, so this is not a regression. |
| `requested_depth` | The SurrealQL functions return it; `DependencyNode` has no such field, and the pre-existing Rust path also drops it. | Leave as-is; changing the struct is out of scope. |
| empty `edge_type` | `$edge_type ?? 'calls'` only defaults `NONE`/null, so `''` searches for the empty edge type. The native path treats `''` as `calls`. | Intentional: empty string is not a meaningful edge type. Documented here because it is not differentially covered. |
| same-depth ordering | `ORDER BY depth` has no tie-break; the native path emits BFS discovery order. | Tests compare after sorting by `(id, depth)`, so ordering is not part of the contract. |
| `trace_bfs` path elements | Path stores node ids, not names: `TraceStep` only receives `EdgeRow`, which has no callee name. | Unobservable: `CallChainNode` has no `call_path` field, so the native path never emits it. |
| `called_by` truncation | SurrealQL slices to 10 before hydrating; the native path does the same. Group order is undefined in SurrealQL. | Tests assert the count is 10, not which ten, to stay non-flaky. |
| cycle multiplicity | `fn::detect_circular_dependencies` emits one row per matching **edge record**, so a pair backed by three edges appears three times (real EasyTier data: 187 rows for 75 unique pairs on `calls`). The native path emits each pair once. | Intentional: edges are occurrence-aware, and showing the same cycle three times adds nothing. The unique-pair **set** is identical (verified on real data: 75 = 75). Callers must not rely on row counts. |

### Degree counting and self-loops

Self-loops are **not** rare: EasyTier has 8317 of 72940 edges (11.4%) with `from = to`,
mostly recursive calls. `fn::get_hub_nodes` and `fn::get_complexity_hotspots` count them
via `GROUP BY \`to\`` / `GROUP BY \`from\``, so a self-loop contributes one afferent and
one efferent. `analysis::degrees` must mirror that; the first version skipped self-loops
and silently under-counted every degree by up to 11%. Verified against live data:
a node with one real incoming edge plus one self-loop reports `afferent=2, efferent=1`.

`fn::detect_circular_dependencies` is the opposite case: it explicitly filters
`\`from\` != \`to\``, so `analysis::mutual_pairs` must keep skipping self-loops.

Any new degree-based function must decide which of the two rules applies and test the
self-loop case explicitly — the fixture-based differential tests passed for weeks while
this bug was live because the fixture had no self-loops.

### Latent decoding hazard (pre-existing, both paths)

`NodeLocation.file_path` is a `String` while the schema declares
`DEFINE FIELD file_path ON nodes TYPE option<string>`. A node with `file_path = NONE`
would fail to deserialize, turning the whole call into `Err`. This affects the SurrealQL
path too (`fn::node_info` builds the same object), so it is not a migration regression.
Measured on all four project databases: 0 nodes with a null `file_path`, so it does not
trigger today. If a future analyzer emits location-less nodes, relax `NodeLocation` to
`Option<String>` and update both paths.


Each batch: pure function + unit tests, then a `*_native` method beside the existing
call, then the differential test below, then remove the SurrealQL function.

## Differential testing (required per batch)

The SurrealQL function stays until its Rust replacement is proven equal:

```rust
// same fixture project in both paths
let sql = functions.detect_circular_dependencies("calls").await?;
let native = functions.detect_circular_dependencies_native("calls").await?;
assert_eq!(normalize(sql), normalize(native));
```

Normalisation means sorting by `node1_id, node2_id` because `HashMap` iteration order is
not part of the contract. `schema_v2_test.rs` already seeds a fixture project, so the
comparison runs against real schema rows, not mocks.

Deleting a SurrealQL function is the last step, never the first — otherwise a mismatch
has no reference to compare against.

## Enabling SurrealDB features (approved scope)

| Feature | Decision | Note |
|---|---|---|
| `TYPE RELATION` edge table | yes | replaces `TYPE NORMAL` + `record<nodes>` references; needs data migration and `RELATE` write path |
| `->` traversal syntax | partial | only for 1–2 hop direct neighbours; deeper walks stay in `AnalysisCore` |
| `DEFINE ACCESS` least-privilege user | yes | replaces `root/root`; per-database user, read/write on its own database |
| `DEFINE EVENT` | no | the project deliberately avoids cascades (`edges survive a node delete on purpose`) |
| Live query | no | `--watch` is in-process; no consumer needs DB push today |

## Switching callers over (the step that actually pays off)

Every `*_native` method written so far has **no caller**: MCP tools still go through the
SurrealQL functions. Until callers switch, the Rust code is dead weight and the differences
above stay theoretical. Switching is its own batch and needs a different test technique than
the fixtures:

1. Pick one method, switch its one caller behind `CODEGRAPH_NATIVE_ANALYSIS=1`.
2. Run both paths against a real project database (EasyTier: 38057 nodes / 72940 edges /
   8317 self-loops) and diff the outputs, not just counts.
3. Only then flip the default and delete the SurrealQL function.

Real-data comparison matters because fixture tests missed a live 11.4% self-loop rate.

## Non-goals

- No backend swap (SurrealDB stays).
- No change to existing result structs (`CircularDependency`, `HubNode`, ...); the
  migration swaps implementations, not signatures, so MCP tool output stays stable.
- No removal of `schema/codegraph.surql` (v1) until the v2-only path is proven.
