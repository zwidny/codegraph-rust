# CodeGraph agentic CLI

CodeGraph is a CLI tool available through Bash as the `codegraph` command. Run the
commands below using your Bash or shell execution tool. Check that CodeGraph is
on PATH with `command -v codegraph`.

Use CodeGraph's built-in reasoning agents for code discovery, change impact,
architecture, and quality questions. They gather evidence with internal graph tools
and return synthesized answers with source locations. Call these public commands:

| Need | Command | Optional `--focus` |
| --- | --- | --- |
| Find code or gather context | `codegraph agent context "question or task"` | `search`, `builder` (default), `question` |
| Assess a proposed change | `codegraph agent impact "what would change?"` | `dependencies` (default), `call_chain` |
| Explain structure/interfaces | `codegraph agent architecture "area to understand"` | `structure` (default), `api_surface` |
| Identify quality risks | `codegraph agent quality "area to assess"` | `complexity`, `coupling`, `hotspots` (all use the quality workflow) |

Examples:
```sh
codegraph agent context "Where is request authentication implemented?" --focus search
codegraph agent impact "What breaks if token validation changes?" --focus call_chain
codegraph agent architecture "Describe the public authentication interfaces" --focus api_surface
codegraph agent quality "Find coupling and hotspots in authentication" --focus coupling
```

Run from the indexed project root, or pass `--project /path/to/project`. For a custom
indexed identifier, pass `--project-id ID` or retain `CODEGRAPH_PROJECT_ID`. Existing
CodeGraph provider/database configuration applies; no running MCP server is needed.
The binary must be built with `ai-enhanced` (or `full`), and the project must already
be indexed in SurrealDB. Calls use the configured model and can incur provider costs.

Stdout is one JSON response with `answer`, `findings`, workflow metadata, and
`structured_output` when available, matching MCP. Read source locations and risk
notes from this evidence. `--format text` prints just the answer; use JSON when
checking warnings and partial results. Logs go to stderr. Runtime failures emit
`{"error": {...}}` and exit 1; argument errors exit 2. The default command deadline
is 600 seconds; override with `--timeout-secs N`. Use `--query-file FILE` instead of
a positional query for long questions, or `--query-file -` to read stdin.

When CodeGraph is available, start code discovery with a specific agentic question
including relevant symbols, paths, or the intended change before broad grep/rg searches.
Check impact before editing. Follow returned file:line locations with targeted reads.
Verify the answer against current code: an index can lag behind uncommitted work,
and a successful response can contain a timeout/partial-result warning. If the
index or service is unavailable, results lack evidence, or a location is already
known, use ordinary repository tools. Do not install, configure, download models or
reindex merely because this guidance was injected. Internal graph analysis tools
are for CodeGraph's built-in agents; use only the four agentic commands above.

Externalise the design and gate it before editing, and also **while you investigate or
design** — `plan check` is offline (no DB, no LLM), so it is cheap to iterate on:

```sh
codegraph plan --project . check plan.json     # offline: nodes/edges/constraints
codegraph plan --project . verify plan.json    # compare with the indexed code graph
codegraph plan --project . store plan.json --plan-id <id>   # keep it; `show <id>` reloads
```

`plan check` catches undeclared references, layering violations and cycles before the
code exists. `plan verify` reports `unplanned_code_edges` (dependencies the code already
has that the plan forgot) and `existing_planned_nodes` (symbols you may be re-creating).
Use it when investigating a multi-module behaviour, designing an approach, adding a
module, crossing layers, chaining 3+ hops, or changing the public interfaces of 3+ files;
skip it only for a one-line lookup or a mechanical rename.

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

A partial re-index updates only the scanned scope. Compiler-grade enrichment comes from the
SCIP index you pass in `CODEGRAPH_SCIP_INDEX`; files the index does not cover stay AST-only.
Pin the project so the index and the scan agree:

```sh
CODEGRAPH_SCIP_INDEX=/abs/rust.scip codegraph index crates/codegraph-core \
  --project-root . --index-tier balanced
```

`--project-root` (or `CODEGRAPH_PROJECT_ID`) must match the indexed project; the scan
path stays relative to it.

The index tier filters edges, it does not gate enrichment: `fast` drops `Uses`/`References`,
`balanced` drops `References`, and `full` keeps everything. A supplied SCIP index is imported
on every tier.

**Prefer SCIP over AST-only.** Generate a compiler index first and pass it via
`CODEGRAPH_SCIP_INDEX` (`rust-analyzer scip .`, `scip-typescript index`, `scip-go`,
`scip-python index`; comma-separated entries, `path@subdir` when the index was built inside
a subdirectory). On a 10k-file tree this cut analysis from 21 minutes to 2.4 s plus a 3.9 s
import, and it contributes definition edges even on `balanced`. `scripts/scip-index.sh`
generates the ones it knows and prints the value.

Any language with a SCIP indexer can be enriched, not only those four: CodeGraph parses
Python natively, so a Python SCIP index contributes its edges too. Generate it with
`scip-python index --project-name <name> --output <path>` from the project root and do
**not** add `--cwd` (it produces a ~72-byte empty index); check `wc -c` before importing,
since a 72-byte file holds zero documents and the import aborts. A language without an index
gets no compiler enrichment but is still parsed; there is no language-server fallback.

Reload this guidance with `codegraph agent instructions` after context loss.
