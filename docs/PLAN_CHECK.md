# Plan check: validating reasoning before code exists

`codegraph plan` checks an LLM's **draft logic graph** — the entities, relations and
invariants it is planning — without reading code, opening SurrealDB, or contacting a
provider. It exists because most of the reasoning behind a new task does not exist in
the repository yet, so the usual "search the code" workflow cannot see it.

The operators are the same ones used against a real code graph (reachability, cycles,
layering, degree). They only depend on the graph you hand them, so they work on plans
exactly as they work on indexed code.

## Usage

```bash
codegraph plan plan.json
codegraph plan plan.yaml --format json
cat plan.json | codegraph plan -
```

Exit code is `0` when there are no violations and `1` when at least one rule fails, so
it can gate a workflow before any code is written.

`codegraph plan` runs before configuration and provider loading, like `codegraph query`.

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

## Example

```
$ codegraph plan provider.json
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

## Bootstrap checklist

1. Write the planned nodes, edges and invariants as a plan file.
2. Run `codegraph plan` and fix violations until it exits `0`.
3. Map draft nodes to real anchors with `codegraph query` / `codegraph agent`.
4. Implement, re-index, then verify the real graph (`--callers`, `trace_call_chain`)
   before deleting the draft.
