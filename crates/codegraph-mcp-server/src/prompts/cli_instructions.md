# CodeGraph CLI

CodeGraph is a CLI tool available through Bash as the `codegraph` command. Run the
commands below using your Bash or shell execution tool. Check that CodeGraph is
on PATH with `command -v codegraph`.

There is **no `agent` subcommand** and no built-in LLM workflow. CodeGraph
answers code questions by querying the graph directly; when you need a synthesized
answer, compose the queries below yourself and read the returned `file:line`
locations. Existing CodeGraph database configuration applies; no running MCP server
is needed.

## Find a symbol

```sh
codegraph query <symbol>                  # exact match first, then prefix matches
codegraph query <symbol> --callers        # who calls it (incoming `calls` edges)
codegraph query <symbol> --callees        # what it calls (outgoing `calls` edges)
codegraph query <symbol> --kind function  # only match this node kind
codegraph query <symbol> --file src/auth  # only the definition in this file/suffix
```

Run from the indexed project root, or pass `--project /path/to/project`. For a custom
indexed identifier pass `--project-id ID` (or set `CODEGRAPH_PROJECT_ID`). Use
`--limit N` to widen or narrow each result section (default 20, clamped to 1..=1000).
`--kind` matches case-insensitively against `function`, `method`, `struct`, `enum`,
`trait`, `module`, `import`, `impl`, `variable`, `macro`, `feature`, and similar.
`--file` accepts an exact path or a path suffix and also scopes `--callers`/`--callees`
to that exact node.

Output is JSON with the matched definitions plus the requested caller/callee lists.
Read the reported file and line, then open that source range before editing. The
graph can lag uncommitted work, so verify against current source.

## Plan before you edit

`plan check` is offline (no database, no LLM), so it is cheap to iterate on:

```sh
codegraph plan check plan.json                  # offline: nodes/edges/constraints
codegraph plan verify plan.json                 # compare with the indexed code graph
codegraph plan store plan.json --plan-id <id>   # keep it; `plan show <id>` reloads
```

`plan check` catches undeclared references, layering violations and cycles before the
code exists. `plan verify` reports `unplanned_code_edges` (dependencies the code already
has that the plan forgot) and `existing_planned_nodes` (symbols you may be re-creating).
Use it when investigating a multi-module behaviour, designing an approach, adding a
module, crossing layers, chaining 3+ hops, or changing the public interfaces of 3+ files;
skip it only for a one-line lookup or a mechanical rename. All plan subcommands accept
`--project <root>`; `--format json` gives machine-readable check/verify output.

A plan file is the entities you believe exist plus how they connect; only `id` is required
per node, edge `type` defaults to `depends`, and constraints are tagged by `kind`:

```json
{
  "nodes": [
    {"id": "cli.parse", "layer": "cli"},
    {"id": "core.reconcile", "layer": "core"},
    {"id": "vector.embed", "layer": "vector"}
  ],
  "edges": [
    {"from": "cli.parse", "to": "core.reconcile", "type": "calls"},
    {"from": "core.reconcile", "to": "vector.embed", "type": "calls"}
  ],
  "constraints": [
    {"kind": "acyclic", "edge_kind": "calls"},
    {"kind": "layering", "order": ["cli", "core", "vector"]}
  ]
}
```

## Index and check status

```sh
codegraph index .                    # incremental; detects changed files and reuses the cache
codegraph index . --scip-auto        # also generate and import a SCIP index when a tool is available
codegraph status                     # MCP server status
codegraph status --detailed          # provider, database and index state
```

**Prefer SCIP over AST-only.** `codegraph index` without a compiler index produces an
AST-only graph with no definition or reference edges. Generate the index first with
`scripts/scip-index.sh` (it picks the indexer per language, writes `.codegraph/scip/`
and prints the value for `CODEGRAPH_SCIP_INDEX`), or let `--scip-auto` do it. A language
without a SCIP index still parses; there is no language-server fallback. On a large
repository, run one full pass with a SCIP index and use partial re-indexes for later
edits: `codegraph index <subdir>` updates only that scope, and `--project-root` pins the
owning project.

The index tier filters edges, it does not gate enrichment: `fast` drops `Uses`/`References`,
`balanced` drops `References`, and `full` keeps everything. A supplied SCIP index is
imported on every tier.

## Compose an analysis yourself

There is no single "architecture" or "impact" command. Chain the queries and read the
evidence:

- **Understand behavior:** `codegraph query <symbol>` to locate it, read the file, then
  `codegraph query <symbol> --callers` (and `--callees`) to see how it is wired.
- **Change impact:** `codegraph query <symbol>` for the definition, then
  `codegraph query <symbol> --callers` for direct callers, and repeat on those callers
  for a transitive view.
- **Architecture / structure:** `codegraph query <module> --kind module` to find the
  module, then follow `--callers`/`--callees` for its connections; use `plan verify` to
  check a model of the layout against the graph.
- **Quality risks:** combine callers/callees with the file locations returned by
  `codegraph query`, and write a plan over the risky area before changing it.

This guidance is re-emitted by the project-local `SessionStart` / `SubagentStart`
hooks that `codegraph hooks install` adds.
