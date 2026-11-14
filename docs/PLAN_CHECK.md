# Plan check: validating reasoning before code exists

`codegraph plan` checks an LLM's **draft logic graph** — the entities, relations and
invariants it is planning — without reading code, opening SurrealDB, or contacting a
provider. It exists because most of the reasoning behind a new task does not exist in
the repository yet, so the usual "search the code" workflow cannot see it.

The operators are the same ones used against a real code graph (reachability, cycles,
layering, degree). They only depend on the graph you hand them, so they work on plans
exactly as they work on indexed code.

## When to use plan

Run `plan check` (and `plan verify` once the code graph is indexed) **before writing
code** when any of these hold:

- a new module, crate-level feature or provider/factory path is being added;
- the change crosses layers (e.g. `cli -> core -> vector`) or redefines that direction;
- a dependency is multi-hop (A calls B which calls C, or a trait is implemented by a
  new type elsewhere);
- the public interface of 3 or more files changes together (signature, trait method,
  config key, schema).

Skip it for a single-file bug fix, a doc/text change or a mechanical rename inside one
module: the plan would just restate code that is already visible to the graph.

## Usage

```bash
# `--project` is global: it may appear before or after the subcommand.
# It defaults to the current directory; a subdirectory resolves to its enclosing
# .git/.codegraph root, so every subcommand shares the index's project_id.

# Offline validation: no database, no provider (--project is accepted but unused).
codegraph plan --project . check plan.json
codegraph plan --project . check plan.yaml --format json
cat plan.json | codegraph plan --project . check -

# Persist the draft graph under project_id `plan:<id>`.
codegraph plan --project . store plan.json --plan-id feature-x
codegraph plan --project . show feature-x     # nodes, edges AND constraints
codegraph plan --project . drop feature-x

# Compare the plan with the code graph of the project.
codegraph plan --project /path/to/project verify plan.json
codegraph plan --project /path/to/project verify plan.json --format json

# A subdirectory is enough; it resolves to the repo root.
codegraph plan --project crates/codegraph-core/benches verify plan.json

# Query the draft graph like any other project.
codegraph query new_provider --project-id plan:feature-x
```

`plan check` exits `0` when there are no violations and `1` when at least one rule
fails, so it can gate a workflow before any code is written. It runs before
configuration and provider loading, like `codegraph query`.

`plan store`, `show`, `drop` and `verify` need a reachable database and load
configuration.

## Persistence and isolation

Draft nodes and edges reuse the existing `nodes` / `edges` tables, but every row is
scoped to `project_id = plan:<id>`:

- A real project query scopes by the project path, so it never sees drafts.
- Drafts share no index entries with code rows beyond the normal `project_id` index.
- `plan drop <id>` removes exactly one plan and cannot touch a real project.
- No schema change is required.

Constraints **are** persisted alongside the draft graph in
`project_metadata.metadata.plan_constraints`, so `plan show --format json` round-trips
the whole plan (nodes, edges and constraints). `plan store --force` replaces the stored
copy, including its constraints. `plan check` always re-reads the file, so use
`codegraph plan show <id> --format json` when you want to edit and re-check a stored
plan without keeping the original file around.

## Input

```json
{
  "nodes": [
    { "id": "config",         "layer": "cli" },
    { "id": "factory",        "layer": "vector" },
    { "id": "new_provider",   "layer": "vector" },
    { "id": "embedding_trait","layer": "vector" },
    { "id": "provider_tests", "layer": "tests" }
  ],
  "edges": [
    { "from": "config",         "to": "factory",         "type": "calls" },
    { "from": "factory",        "to": "new_provider",    "type": "calls" },
    { "from": "new_provider",   "to": "embedding_trait", "type": "implements" },
    { "from": "provider_tests", "to": "new_provider",    "type": "uses" }
  ],
  "constraints": [
    { "kind": "acyclic" },
    { "kind": "coverage", "sources": ["config"], "target": "embedding_trait", "edge_kind": "calls" },
    { "kind": "layering", "order": ["tests", "cli", "vector"] },
    { "kind": "max_fan_in", "threshold": 3 }
  ]
}
```

`type` defaults to `depends`. YAML accepts the same shape.

## Minimal template

A small, complete plan (8 nodes, all three common constraints) to copy and trim:

```json
{
  "nodes": [
    { "id": "plan_cli",       "layer": "cli",    "kind": "module" },
    { "id": "verify_cmd",     "layer": "cli",    "kind": "function" },
    { "id": "plan_doc",       "layer": "core",   "kind": "struct" },
    { "id": "code_index",     "layer": "core",   "kind": "struct" },
    { "id": "storage",        "layer": "vector", "kind": "trait" },
    { "id": "plan_tests",     "layer": "tests",  "kind": "module" }
  ],
  "edges": [
    { "from": "verify_cmd", "to": "plan_cli",  "type": "calls" },
    { "from": "verify_cmd", "to": "code_index", "type": "calls" },
    { "from": "code_index", "to": "storage",   "type": "uses" },
    { "from": "plan_tests", "to": "verify_cmd", "type": "calls" }
  ],
  "constraints": [
    { "kind": "acyclic" },
    { "kind": "layering", "order": ["tests", "cli", "core", "vector"] },
    { "kind": "coverage", "sources": ["verify_cmd"], "target": "storage" },
    { "kind": "max_fan_in", "threshold": 3 }
  ]
}
```

Index and compare with the code graph (both must resolve to the same `project_id`):

```bash
# project_id defaults to the canonical project root unless CODEGRAPH_PROJECT_ID is set
codegraph index /path/to/project --project-root /path/to/project

# --project defaults to the current directory; pass the indexed root explicitly
codegraph plan --project /path/to/project verify plan.json
```

If the project was indexed with a custom identifier, export it for both commands:

```bash
export CODEGRAPH_PROJECT_ID=my-project
codegraph plan --project /path/to/project verify plan.json
```

## Rules

| `kind` | Meaning | Fields |
| --- | --- | --- |
| `acyclic` | No directed cycles (optionally over one edge kind) | `edge_kind?` |
| `layering` | Edges must not point from a later layer back to an earlier one; same-layer edges are allowed | `order` (low → high) |
| `coverage` | Every source must reach the target transitively | `sources`, `target`, `edge_kind?` |
| `no_path` | `from` must not reach `to` | `from`, `to`, `edge_kind?` |
| `max_fan_in` | No node may have more incoming edges than the threshold | `threshold` |
| `max_fan_out` | No node may have more outgoing edges than the threshold | `threshold` |

An always-on check, `known_nodes`, flags edges that reference undeclared nodes — the
most common sign that the plan is still incomplete or has a typo.

Each rule is evaluated independently; the output lists every check with `ok` plus a
`detail` when it fails, and a `violations` array suitable for gating.

## Verify against the code graph

`plan verify` runs every rule above **and** compares the plan with the project's
already-indexed code graph (it needs the DB, unlike `plan check`):

```bash
codegraph plan --project /path/to/project verify plan.json
codegraph plan --project /path/to/project verify plan.json --format json
```

Node ids in the plan are matched against code node **names**, so name the planned
nodes exactly as the symbols you intend to write. If an existing symbol is not found,
check that the index is current (`codegraph index`): a stale index makes existing
symbols look like new work. The JSON report keeps the `ok`,
`checks`, `violations` and `warnings` fields and adds four comparison lists:

| Field | Meaning | What to do |
| --- | --- | --- |
| `existing_planned_nodes` | Planned node names that already exist in code, with their files (possible duplicate work) | Reuse or extend the existing symbol instead of recreating it, or rename the plan node if it is genuinely new |
| `edges_already_present` | Planned dependencies that the code graph already has | Confirm the relation is unchanged; nothing new to implement |
| `unplanned_code_edges` | Code dependencies **between planned nodes** that the plan does not record | Add them to the plan (and to the constraints) or consciously drop them — this is the "forgot an existing dependency" case |
| `missing_code_edges` | Planned dependencies not present in code yet | This is the work still to be done after you implement |

The command exits `1` only when the plan itself has a `violation`; the comparison
lists are informational, so an unplanned-but-intentional edge will not fail the gate.

## Example

```
$ codegraph plan check provider.json
Plan check: provider.json
Nodes: 5  Edges: 4
  [PASS] known_nodes
  [PASS] acyclic
  [FAIL] coverage: 'config' cannot reach 'embedding_trait'
  [PASS] layering
  [PASS] max_fan_in
Result: 1 violation(s)
```

Here the plan declares `config → … → embedding_trait` only through `calls`, but the
last hop is `implements`, so the expectation "every entry point reaches the trait" does
not hold for that edge kind. That is exactly the kind of gap worth finding before
implementation: either the relation is mis-modelled, or the constraint is too narrow.

## MCP

The same validation is available to coding agents as the `plan_check` MCP tool:

```json
{ "plan": { "nodes": [ ... ], "edges": [ ... ], "constraints": [ ... ] } }
```

It returns the identical report (`ok`, `checks`, `violations`, `warnings`) and needs
no database. `plan_check` is **offline**: it never reads the project's code graph. To
compare the plan with existing code use the CLI
`codegraph plan --project <root> verify <file>`; persistence
(`plan store` / `show` / `drop`) also stays on the CLI.

## Bootstrap checklist

1. Write the planned nodes, edges and invariants as a plan file.
2. Run `codegraph plan --project . check` and fix violations until it exits `0`.
3. `codegraph plan --project . store <file> --plan-id <id>` to keep the draft in the
   graph, then `codegraph query <node> --project-id plan:<id>` to inspect it.
   Constraints are stored too, so `plan show <id>` reads the whole plan back.
4. Once the project is indexed, run
   `codegraph plan --project <root> verify <file>` and resolve
   `unplanned_code_edges` / `existing_planned_nodes` before writing code.
5. Map draft nodes to real anchors with `codegraph query`.
6. Implement, re-index, then verify the real graph (`--callers`, `trace_call_chain`)
   before `codegraph plan drop <id>`.
