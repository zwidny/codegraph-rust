# CodeGraph Installation & Setup Guide

This guide covers installing CodeGraph and setting up a project. The short version is three steps: build the binary, configure a model provider, and run `codegraph init` in your project. There is no database to install or start.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Building CodeGraph](#building-codegraph)
3. [Quick Setup with `codegraph init`](#quick-setup-with-codegraph-init)
4. [Configuration](#configuration)
5. [Indexing Your Codebase](#indexing-your-codebase)
6. [Running the MCP Server](#running-the-mcp-server)
7. [Daemon Mode](#daemon-mode)
8. [Using the Graph Tools](#using-the-graph-tools)
9. [Optional: Using a SurrealDB Server](#optional-using-a-surrealdb-server)
10. [Troubleshooting](#troubleshooting)

---

## Supported CLI Commands

- `start` / `stop` / `status` — manage the MCP server transports (stdio/http)
- `init` — choose project-local Claude/Codex hooks, merge agent instructions, then index
- `index` — index a project (supports `--force`, language filters, watch mode)
- `query` — fast direct graph query (symbol, callers, callees)
- `plan` — validate a draft plan offline, verify it against the graph, or store it
- `hooks` — install project-local guidance hooks or emit lifecycle context
- `estimate` — estimate indexing time/cost without persisting
- `config` — init/show/set/get/validate configuration; db-check lives here
- `db-check` — opens the project's store (creating it with the bundled schema if new) and reports the target
- `daemon` — (feature-gated) file-watch daemon control

Legacy helper commands (`code`, `test`, `perf`, `stats`, `clean`) are no longer part of the CLI.

For first-time project setup, run `codegraph init /path/to/project`. It offers Claude,
Codex, both or none before updating project `AGENTS.md`/`CLAUDE.md` and indexing.
Scripts should pass `--hooks claude|codex|both|none`; `--no-index` performs setup only.
See [project initialization](USAGE_GUIDE.md#cli-access-for-code-agents) for preservation,
existing-hook detection and provider prerequisites.

For HTTP deployments, MCP 3 validates the request's `Host` header. Loopback hosts
and the configured bind host are accepted by default. When binding to `0.0.0.0`
behind a proxy or serving a public hostname, set an explicit comma-separated
allowlist, for example `CODEGRAPH_HTTP_ALLOWED_HOSTS=localhost,codegraph.example.com`.

---

## Prerequisites

- **Rust 1.95 or newer** - The workspace uses Rust edition 2024. Install from [rustup.rs](https://rustup.rs).
- **A model provider** for embeddings: [Ollama](https://ollama.com) or LM Studio locally, or an API key for OpenAI or Jina. See `docs/AI_PROVIDERS.md`. The graph tools do not require an LLM.
- **macOS** for the installer scripts (Linux users can run the `cargo install` command directly). The scripts use Homebrew.

A shared SurrealDB 3.x server is the supported deployment: run one with Docker or a container
(see [Using a SurrealDB Server](#optional-using-a-surrealdb-server)) and apply
`schema/codegraph_v2.surql` to it. No other database software is needed. An embedded
per-project store is opt-in by setting `CODEGRAPH_SURREALDB_URL` to a `surrealkv://` path,
but it is a compatibility fallback: substantially slower, and it allows one process per project.

Compiler-index enrichment is optional: generate a SCIP index with `scripts/scip-index.sh`
and pass `CODEGRAPH_SCIP_INDEX`. This needs the matching language toolchain (for example
`rust-analyzer` for Rust), but no language server; without an index, indexing is AST-only.

---

## Building CodeGraph

Use the full-features installation script to build CodeGraph with all capabilities:

```bash
cd /path/to/codegraph-rust
./install-codegraph-full-features.sh
```

This script:
- Installs the SurrealDB CLI via Homebrew if not present (only used for the optional server mode)
- Builds CodeGraph with all features enabled (daemon, AI-enhanced, all providers)
- Installs the binary to `~/.cargo/bin/codegraph`

### What Gets Enabled

The full-features build includes:
- All embedding providers (Ollama, LM Studio, Jina AI, OpenAI, ONNX)
- Daemon mode (file watching & auto re-indexing)
- HTTP server with SSE streaming

### Manual Build (Alternative)

If you prefer to build manually:

```bash
cargo install --path crates/codegraph-mcp-server --bin codegraph \
  --all-features --force
```

---

## Quick Setup with `codegraph init`

From a terminal, with a provider configured (see [Configuration](#configuration)):

```bash
codegraph init /path/to/project
```

Init does the per-project setup in one pass:

1. Offers project-local guidance hooks for **Claude Code, Codex, both, or none**, and skips the prompt when CodeGraph hooks are already installed.
2. Adds or refreshes a managed `# codegraph` section in the project's `AGENTS.md` and `CLAUDE.md`, leaving the rest of those files alone.
3. Loads the project's `.env` and configuration and indexes the project recursively. With no `CODEGRAPH_SURREALDB_URL` it connects to the shared server at `ws://127.0.0.1:8000`, which must already run with `schema/codegraph_v2.surql` applied. Point the URL at a `surrealkv://` path to opt into the embedded store instead: first use then creates `<project>/.codegraph/db` with the bundled schema and a `.gitignore` that keeps it out of version control.

For scripts and CI, pass the choices explicitly:

```bash
codegraph init /path/to/project --hooks both --index-tier balanced --workers 4
codegraph init /path/to/project --hooks none --no-index   # hooks/instructions only; no providers, no database
```

After init, connect your client (see [Running the MCP Server](#running-the-mcp-server)) or query the graph from the shell with `codegraph query <symbol>`. Details on hook detection, file preservation and validation are in [the usage guide](USAGE_GUIDE.md#cli-access-for-code-agents).

What init does not do: it does not create global configuration (`codegraph config init` does), choose or install models, or touch user-level harness settings.

---

## Configuration

Settings are resolved in this order, later sources overriding earlier ones:

1. `./.codegraph.toml` in the directory you run from (project-level)
2. `~/.codegraph/config.toml` (user-level; create it with `codegraph config init`)
3. `.env` in the working directory, then `~/.codegraph.env` as a fallback for keys the
   project did not set (put shared defaults in `~/.codegraph.env`, project differences in `.env`)
4. `CODEGRAPH_*` environment variables

For provider-specific examples (Ollama, LM Studio, Jina, OpenAI, xAI, Anthropic, OpenAI-compatible), see `docs/AI_PROVIDERS.md`.

### Config file

```toml
[embedding]
provider = "ollama"                    # ollama | lmstudio | jina | openai | onnx
model = "qwen3-embedding:0.6b"
dimension = 1024                       # 384, 768, 1024, 1536, 2048, 2560, 3072, 3584, 4096
batch_size = 64
ollama_url = "http://localhost:11434"
# lmstudio_url = "http://localhost:1234"

[indexing]
tier = "fast"                          # fast | balanced | full

[performance]
num_threads = 0                        # 0 = auto
max_concurrent_requests = 4

[daemon]
auto_start_with_mcp = true             # start the file watcher with `codegraph start`
debounce_ms = 30
exclude_patterns = ["**/node_modules/**", "**/target/**", "**/.git/**"]

[logging]
level = "info"
```

Storage has no config-file section: `codegraph` connects to a shared server at `ws://127.0.0.1:8000` by default, or to `CODEGRAPH_SURREALDB_URL` (plus the `CODEGRAPH_SURREALDB_*` variables) when set. The embedded per-project store is opt-in through a `surrealkv://` URL; it is a compatibility fallback that is substantially slower and single-writer.

The `[llm]` section was removed in v0.3.0 together with the built-in agent; nothing in
CodeGraph calls a language model. A leftover `[llm]` block is ignored silently (unknown TOML
sections do not raise an error). [AI_PROVIDERS.md](AI_PROVIDERS.md) lists every variable and
key.

### Environment variables and `.env`

```bash
cp .env.example .env    # then edit
```

The variables most setups need:

```bash
# Embeddings
CODEGRAPH_EMBEDDING_PROVIDER=ollama
CODEGRAPH_EMBEDDING_MODEL=qwen3-embedding:0.6b
CODEGRAPH_EMBEDDING_DIMENSION=1024

# API keys (cloud providers)
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
JINA_API_KEY=jina_...
XAI_API_KEY=xai-...

# Agent selection and debugging
CODEGRAPH_AGENT_ARCHITECTURE=react    # react | lats | reflexion
CODEGRAPH_DEBUG=1                     # debug logging
```

`.env.example` lists the rest (chunking, tokenizer, batching, Jina and server-mode settings), each with a comment.

---

## Indexing Your Codebase

`codegraph init` indexes the project as its last step. Use `codegraph index` directly to re-index, to pick languages or a tier, or to index without touching hooks and instruction files.

### Basic Indexing

```bash
# Index current directory recursively
codegraph index . -r

# Index with specific languages
codegraph index . -r -l rust,typescript,python

# Index a specific path
codegraph index /path/to/codebase -r -l rust
```

### Indexing Options

```bash
codegraph index <PATH> [OPTIONS]

Options:
  -r, --recursive           Recursively index subdirectories (default)
      --no-recursive        Index only files directly in the project directory
  -l, --languages <LANGS>   Comma-separated list of languages to index
      --exclude <PATTERN>   Exclude patterns (gitignore format, repeatable)
      --include <PATTERN>   Include only these patterns
      --index-tier <TIER>   fast (default) | balanced | full
                            balanced/full enable more analyzer stages; full
                            keeps references edges (SCIP enrichment is separate)
      --batch-size <N>      Embedding batch size (overrides env and config)
      --force               Re-index even if already indexed
      --complete-deferred   Finish embedding/semantic work deferred by an earlier run
      --stats-json <FILE>   Write indexing metrics and completion status as JSON
```

Run `codegraph index --help` for the full list.

### Examples

```bash
# Index a Rust project
codegraph index /path/to/rust-project -r -l rust

# Index a full-stack project with multiple languages
codegraph index . -r -l rust,typescript,python --exclude "**/node_modules/**" --exclude "**/target/**"

# Force complete re-index
codegraph index . -r -l rust --force
```

### Verify Indexing

The index command ends with a summary of files, nodes and edges. To check the store afterwards:

```bash
cd /path/to/project
codegraph db-check                                             # prints the store it opened
codegraph query <symbol>                                       # confirm the graph answers
codegraph status --detailed                                    # server/provider/index state
```

With the default shared server (the built-in `ws://127.0.0.1:8000` or your `CODEGRAPH_SURREALDB_URL`), reads and indexing run concurrently, so this restriction does not apply. The **embedded** fallback takes an exclusive file lock, so only one process can hold it at a time: if a `codegraph start` server for the same project has already answered a tool call, stop it before running `codegraph index`, otherwise the index command reports that the database is open in another process.

---

## Running the MCP Server

### With OpenCode

Use a **project-level** `<project>/.opencode/opencode.json` with the `mcp.servers` structure.
Do not also register it in the global config: two registrations start two stdio instances.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "codegraph": {
        "type": "local",
        "command": ["/full/path/to/codegraph", "start", "stdio"],
        "cwd": "/path/to/project",
        "enabled": true
      }
    }
  }
}
```

Verify with `opencode mcp list` (expect `codegraph  connected`), then restart OpenCode so the
MCP `initialize` handshake picks up the binary and its instructions.
See [OPENCODE_QUICKSTART](OPENCODE_QUICKSTART.md) for the full new-machine and new-project flow.

### With Claude Code

Add CodeGraph to your Claude Code MCP configuration. In your `~/.claude/claude_desktop_config.json` or MCP settings:

```json
{
  "mcpServers": {
    "codegraph": {
      "command": "/full/path/to/codegraph",
      "args": ["start", "stdio", "--watch"]
    }
  }
}
```

**Important:** Use the **full absolute path** to the binary. Find it with:

```bash
which codegraph
# Usually: /Users/<username>/.cargo/bin/codegraph
```

### STDIO Mode (Recommended)

```bash
# Start MCP server with file watching
/full/path/to/codegraph start stdio --watch

# Without file watching
/full/path/to/codegraph start stdio

# Watch a specific directory
/full/path/to/codegraph start stdio --watch --watch-path /path/to/project
```

### HTTP Mode (For Web Clients)

```bash
# Start HTTP server with SSE streaming
codegraph start http --host 127.0.0.1 --port 3000

# Test the endpoint
curl http://127.0.0.1:3000/health
```

### The `--watch` Flag

When you include `--watch`, the MCP server automatically:
- Monitors your project for file changes
- Re-indexes modified files in the background
- Keeps search results up-to-date

Without `--watch`, you must manually re-run `codegraph index` after making changes.

---

## Daemon Mode

For more control over file watching, use the standalone daemon:

### Starting the Daemon

```bash
# Start watching a project (runs in background)
codegraph daemon start /path/to/project

# Start in foreground (for debugging)
codegraph daemon start /path/to/project --foreground

# Filter by languages
codegraph daemon start /path/to/project --languages rust,typescript

# Exclude patterns
codegraph daemon start /path/to/project \
  --exclude "**/node_modules/**" \
  --exclude "**/target/**"
```

### Managing the Daemon

```bash
# Check daemon status
codegraph daemon status /path/to/project
codegraph daemon status /path/to/project --json

# Stop the daemon
codegraph daemon stop /path/to/project
```

### Daemon vs --watch

| Feature | `--watch` flag | `daemon` command |
|---------|----------------|------------------|
| Runs with MCP server | Yes | Independent |
| Background process | No (inline) | Yes |
| Multiple projects | No | Yes |
| Fine-grained control | Limited | Full |
| Status monitoring | No | Yes |

**Use `--watch`** for single-project setups with Claude Code. With the embedded per-project store this is the mode that works alongside the MCP server, because the watcher runs inside the server process.
**Use `daemon`** when no MCP server is running against the same project, or when you use a SurrealDB server: a standalone daemon is a separate process and cannot share a project's embedded store with a running server.

---

## Using the Graph Tools

CodeGraph exposes 9 direct graph tools. They answer one concrete question each, return
`file:line` evidence in milliseconds, and never call an LLM.

### Available Tools

| Tool | Purpose |
|------|---------|
| `semantic_code_search` | Find a symbol/concept; returns the `node_id` the other tools need |
| `get_reverse_dependencies` | Who calls or depends on a node |
| `get_transitive_dependencies` | Transitive dependency closure |
| `trace_call_chain` | Call path starting at a node |
| `calculate_coupling_metrics` | Coupling and instability for a node/module |
| `detect_circular_dependencies` | Dependency cycles |
| `find_complexity_hotspots` | Complexity/risk ranking |
| `get_hub_nodes` | High-degree hub ranking |
| `plan_check` | Validate a draft nodes/edges/constraints graph offline |

### Example Queries

In Claude Code, simply ask questions — the assistant composes the tools itself:

**Code Search:**
```
"Find all places where authentication is handled in this codebase"
```

**Dependency Analysis:**
```
"What would be affected if I change the UserService class?"
```
(`semantic_code_search` → `get_reverse_dependencies` → `get_transitive_dependencies`)

**Call Chain Analysis:**
```
"Trace the execution path from HTTP request to database query in the payment flow"
```

**Architecture Analysis:**
```
"Give me an overview of this project's architecture and main components"
```
(`get_hub_nodes` + `detect_circular_dependencies` + `find_complexity_hotspots`)

**API Surface:**
```
"What public APIs does the auth module expose?"
```
(`calculate_coupling_metrics`, plus `semantic_code_search` for the public items)

**Change Planning:**
```
"I need to add rate limiting to the API. Validate the plan before I start."
```
(`plan_check`, then `codegraph plan verify`)

Earlier releases shipped a server-side reasoning agent and matching CLI commands that
synthesized these answers. They were removed in v0.3.0; compose the direct tools instead.

---

## Optional: Using a SurrealDB Server

CodeGraph targets a shared SurrealDB server by default (`ws://127.0.0.1:8000`) and reads namespace, database and credentials from `.env`. Indexing and queries are substantially faster than the embedded fallback, and a read-only MCP client can run alongside `codegraph index`. Use the embedded store only when you need a fully self-contained, offline machine, by setting `CODEGRAPH_SURREALDB_URL` to a `surrealkv://` path.

```bash
CODEGRAPH_SURREALDB_URL=ws://localhost:3004
CODEGRAPH_SURREALDB_NAMESPACE=ouroboros
CODEGRAPH_SURREALDB_DATABASE=codegraph
CODEGRAPH_SURREALDB_USERNAME=root
CODEGRAPH_SURREALDB_PASSWORD=root
```

In server mode the schema is not applied automatically and all projects share the database (rows are scoped by project id).

Every command resolves storage the same way: `index`, `query`, `plan`, `status`, and
`codegraph start` all connect to `CODEGRAPH_SURREALDB_URL` when it is set, otherwise to the
default shared server. They never fall back to a local `.codegraph/db` on their own — the
embedded store is used only when `CODEGRAPH_SURREALDB_URL` is itself a `surrealkv://` path.
So if indexing wrote to a server, `codegraph query` reads from that same server even when the
project directory has no `.codegraph` folder.

### Setting Up SurrealDB

Use SurrealDB 3.x (the embedded SDK is 3.3). Follow SurrealDB's upgrade guide before opening an existing 2.x database with a 3.x server. After the server is up, apply the schema (see [Creating the Database Schema](#creating-the-database-schema)).

#### Option 1: Local Server

```bash
# Install SurrealDB CLI
brew install surrealdb/tap/surreal

# Start SurrealDB with persistent storage
surreal start \
  --bind 0.0.0.0:3004 \
  --user root \
  --pass root \
  file://$HOME/.codegraph/surreal.db
```

For in-memory storage (data lost on restart):
```bash
surreal start --bind 0.0.0.0:3004 --user root --pass root memory
```

#### Option 2: Surreal Cloud (Free Tier Available)

1. Sign up at [surrealdb.com/cloud](https://surrealdb.com/cloud)
2. Create a free 1GB instance
3. Note your connection URL, namespace, and credentials

#### Option 3: Surrealist IDE

[Surrealist](https://surrealdb.com/surrealist) provides a graphical IDE for SurrealDB that makes database management easy:

- Visual query editor with syntax highlighting
- Schema visualization
- Data browser and editor
- Easy connection management

Download from [surrealdb.com/surrealist](https://surrealdb.com/surrealist) or use the web version.

---

### Creating the Database Schema

After starting SurrealDB, apply the CodeGraph schema:

#### Using the Apply Script

```bash
cd /path/to/codegraph-rust/schema

# Apply to local database (defaults: localhost:3004, root/root)
./apply-schema.sh

# Apply with custom settings
./apply-schema.sh \
  --endpoint ws://localhost:3004 \
  --namespace ouroboros \
  --database codegraph \
  --username root \
  --password root
```

#### Using SurrealDB CLI Directly

```bash
surreal sql \
  --endpoint ws://localhost:3004 \
  --namespace ouroboros \
  --database codegraph \
  --username root \
  --password root \
  < schema/codegraph_v2.surql
```

#### Using Surrealist IDE

1. Open Surrealist and connect to your database
2. Navigate to the Query tab
3. Open `schema/codegraph_v2.surql`
4. Execute the schema

#### Verify Schema Installation

```bash
surreal sql \
  --endpoint ws://localhost:3004 \
  --namespace ouroboros \
  --database codegraph \
  --username root \
  --password root \
  --command "INFO FOR DB;"
```

You should see tables like `nodes`, `edges`, `chunks`, `symbol_embeddings`, and functions like `fn::semantic_search_chunks_with_context`.

---

## Troubleshooting

### Common Issues

**"The embedded database ... is open in another process"**
- Another `codegraph` process holds this project's embedded store. Stop the running `codegraph start` (or the other client's server) and retry.
- Embedded mode (`surrealkv://`) allows one MCP client per project at a time. If several clients, or a client plus `codegraph index`, must share a database, use the shared server: server mode has no per-client file lock and allows concurrent reads and indexing.

**"Embedded database was created from the ... schema at a different revision"**
- The store keeps the schema it was created with. To move to the current schema, stop all `codegraph` processes, delete `<project>/.codegraph/db` and run `codegraph index` again.

**"No embeddings found" or empty search results**
- Index the project: `codegraph init .` or `codegraph index . -r`
- Check the embedding provider is running and the model is pulled (Ollama, LM Studio, etc.)
- A fast-tier index built without the embeddings feature has graph data but no vectors

**Server mode: "Failed to connect" or missing `fn::` functions**
- Ensure the server is running and `CODEGRAPH_SURREALDB_URL` matches it
- Apply the schema: `cd schema && ./apply-schema.sh`

**MCP server not appearing in Claude Code**
- Use the full absolute path to the binary
- Check Claude Code's MCP logs for errors
- Verify the binary is executable: `chmod +x /path/to/codegraph`

### Getting Help

- Check logs with `CODEGRAPH_DEBUG=1`
- View the README for additional configuration options
- Check `codegraph --help` for CLI documentation
