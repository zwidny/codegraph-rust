# CodeGraph MCP – indexer and analyzers

`codegraph-mcp` builds the SurrealDB knowledge graph that the MCP server serves: it
collects files, parses them with tree-sitter, runs the analyzer stages, resolves edges
and writes nodes, chunks and embeddings. The MCP server crate (`codegraph-mcp-server`)
then exposes the graph as **9 direct tools**:

| Tool | Purpose |
|---|---|
| `semantic_code_search` | Find a symbol/concept; returns the `node_id` the other tools need |
| `get_reverse_dependencies` | Who calls or depends on a node |
| `get_transitive_dependencies` | Transitive dependency closure |
| `trace_call_chain` | Call path starting at a node |
| `calculate_coupling_metrics` | Coupling and instability for a node/module |
| `detect_circular_dependencies` | Dependency cycles |
| `find_complexity_hotspots` | Complexity/risk ranking |
| `get_hub_nodes` | High-degree hub ranking |
| `plan_check` | Validate a draft nodes/edges/constraints graph offline |

Every tool queries SurrealDB directly and returns structured rows with file paths and
line numbers. There is no server-side agent and no LLM call.

## Architecture at a Glance

- **Surreal graph storage** – compile with `codegraph-graph/surrealdb` and provide Surreal
  credentials via `CODEGRAPH_SURREALDB_*` env vars or `config/surrealdb_example.toml`.
- **Embedding providers** – choose via `CODEGRAPH_EMBEDDING_PROVIDER` and the
  `embeddings-*` Cargo features (Ollama, OpenAI, Jina, etc.). Semantic search needs them;
  the structural graph queries do not.
- **Transports (server crate)** – STDIO listens on stdin/stdout; the optional HTTP server
  exposes `/mcp` for streaming JSON-RPC over HTTP.

## Quick Start

### 1. Prepare configuration

```bash
cp config/surrealdb_example.toml ~/.codegraph/config.toml
export CODEGRAPH_SURREALDB_URL=ws://localhost:3004
export CODEGRAPH_SURREALDB_NAMESPACE=ouroboros
export CODEGRAPH_SURREALDB_DATABASE=codegraph
export CODEGRAPH_SURREALDB_USERNAME=root
export CODEGRAPH_SURREALDB_PASSWORD=root
export CODEGRAPH_EMBEDDING_PROVIDER=ollama
export CODEGRAPH_EMBEDDING_MODEL=qwen3-embedding:0.6b
```

### 2. Index the project

```bash
codegraph index . --scip-auto
```

Without a SCIP index the graph is AST-only. Generate one first with
`scripts/scip-index.sh` (or let `--scip-auto` do it) so definition and reference edges
exist.

### 3. Run the STDIO MCP server

```bash
cargo run -p codegraph-mcp-server --bin codegraph \
  --features "daemon,server-http,embeddings-ollama" \
  -- start stdio
```

Hook this process up to your MCP host (Claude Code, OpenCode, etc.).

### 4. (Optional) Run the HTTP transport

```bash
cargo run -p codegraph-mcp-server --bin codegraph \
  --features "server-http,embeddings-ollama" \
  -- start http --host 127.0.0.1 --port 3003
```

## Invoking Tools Manually

All MCP requests follow the standard JSON-RPC envelope. Example for
`get_reverse_dependencies`:

```jsonc
{
  "jsonrpc": "2.0",
  "id": "1",
  "method": "tools/call",
  "params": {
    "name": "get_reverse_dependencies",
    "arguments": {
      "node_id": "nodes:`0f0f013b-df13-d632-1366-59ce8ed1c0b5`",
      "edge_type": "calls"
    }
  }
}
```

Responses contain the matching edges with `file_path`, `line_number` and bounded
`content` rows.

## Configuration + Environment Variables

| Variable | Purpose |
|---|---|
| `CODEGRAPH_SURREALDB_URL` (+ namespace/db/user/password) | Points the server at your SurrealDB instance. |
| `CODEGRAPH_EMBEDDING_PROVIDER` | Chooses embedding backend (see Cargo feature flags above). |
| `CODEGRAPH_HTTP_HOST` / `CODEGRAPH_HTTP_PORT` | Used by the HTTP server. |
| `CODEGRAPH_TOOL_CONTENT_CHARS` | Snippet length per result row (`0` disables shortening). |

For tuning see `crates/codegraph-mcp/src/context_aware_limits.rs` and the prompt files
under `crates/codegraph-mcp-server/src/prompts/`.

## Testing

- `cargo test -p codegraph-mcp --lib` – indexer/analyzer unit tests.
- `cargo test -p codegraph-mcp --test reconciliation_test --test daemon_watch` –
  reconciliation and watch regressions.
- `cargo test -p codegraph-mcp --features embeddings --test deferred_policy_test` –
  deferred inference policy.

## Need More?

- [Indexing pipeline](../docs/INDEXING_PIPELINE.md)
- [Usage guide](../docs/USAGE_GUIDE.md)
- [Testing guide](../docs/TESTING.md)
