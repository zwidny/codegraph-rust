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

For a large or multi-hop change, externalise the design and gate it before editing:

```sh
codegraph plan --project . check plan.json     # offline: nodes/edges/constraints
codegraph plan --project . verify plan.json    # compare with the indexed code graph
```

`plan check` catches undeclared references, layering violations and cycles before the
code exists. `plan verify` reports `unplanned_code_edges` (dependencies the code already
has that the plan forgot) and `existing_planned_nodes` (symbols you may be re-creating).
Use it when adding a module, crossing layers, chaining 3+ hops, or changing the public
interfaces of 3+ files; skip it for a single-file fix.

A partial re-index skips language servers by default (they load the whole workspace).
Enable them deliberately and pin the project:

```sh
CODEGRAPH_LSP_ON_PARTIAL=1 codegraph index packages/happy-android \
  --project-root . --index-tier balanced --languages kotlin
```

`--project-root` (or `CODEGRAPH_PROJECT_ID`) must match the indexed project; the scan
path stays relative to it. A cold LSP import may exceed the 30s `initialize` deadline on
large repos — retry once warmed up, or do one full `codegraph index <root>` pass first.

LSP edges depend on the tier: `balanced` uses `SymbolsOnly` and produces **0 definition
edges** (only `documentSymbol`); `full` uses `SymbolsAndDefinitions` and resolves them.
`0 edges resolved` on balanced is expected. Set `CODEGRAPH_LSP_DEFINITIONS=1` to force
definition resolution on balanced.

Reload this guidance with `codegraph agent instructions` after context loss.
