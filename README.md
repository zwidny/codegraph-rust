# CodeGraph (with updates)

## Measured value: CodeGraph vs grep

Measured on two real repositories, against the workflow an agent has without CodeGraph:
grep with line numbers, then read a window around the hit. No chat LLM is involved; the
graph arm does embed the query, which is why its latency is higher on the locate case.

Both rows were re-measured **2026-11-14 on the PostgreSQL backend** with
`scripts/benchmarks/mcp_value.py` (deterministic sampling; n differs per row):

- **Rust** — this repository, 265 files, `--generate 40/15/12`.
- **Python** — a private `stock_a` project, 53 files, `--generate 20/12/12`; only 3 chain
  tasks qualify at depth 2, so that row is n=3 and noisier than the rest.

The repositories changed since 2026-11-04, so these are fresh task samples, not the same
symbols. Responses are **compact JSON**, and one-hop edges are opt-in
(`include_graph_context`, default false), both of which cut bytes.

Real agents mostly send **keyword phrases**, not full questions: across 338 sampled
`semantic_code_search` calls, 90% were phrases, 7% bare identifiers, 2% sentences. The locate
rows use the harder `--codegraph-query question` arm; on the Rust re-run the phrase arm
measured 62% hit@1 / 68% hit@5 for the graph.

| Question | Repo | grep | CodeGraph | Result |
|---|---|---|---|---|
| Where is X defined? | Rust | **62%** hit@1 · **90%** hit@5 · **16.2 KB** · **0.01 s** | 68% · 72% · **10.8 KB** (2.9 KB top-3) · 0.18 s | better hit@1, worse hit@5, fewer bytes, slower |
| Where is X defined? | Python | 95% hit@1 · 95% hit@5 · **3.4 KB** · **0.00 s** | **100%** · **100%** · 10.3 KB (2.9 KB top-3) · 0.08 s | graph more accurate; grep reads less on the full payload |
| Who calls X? | Rust | **100%** recall · 77% precision · 8.7 KB · **0.01 s** | 97% · **100%** · **4.8 KB** · 0.06 s | graph trades a little recall for precision + fewer bytes |
| Who calls X? | Python | 100% recall · 79% precision · 9.8 KB · **0.00 s** | 100% · **100%** · **4.5 KB** · 0.03 s | same trade-off |
| What does X reach? (depth 2) | Rust | 27% recall · 7% precision · 99.1 KB · 0.13 s | **100%** · **78%** · **8.0 KB** · 0.08 s | graph gives the only usable answer |
| What does X reach? (depth 2) | Python | 72% recall · 25% precision · 73.7 KB · 0.06 s | **100%** · **100%** · **8.4 KB** · 0.11 s | graph wins (n=3) |

`grep`'s caller **recall is not comparable evidence**: it counts every textual mention of the
name (definition, comments, strings), so a true caller file is almost always hit even when
grep misattributes the hit. Read the caller rows by **precision** — 77% vs 100% (Rust), 79% vs
100% (Python) — not recall.

The honest read: locate is **not** a clear win for the graph. On this Rust repository it is
slightly better at hit@1 (68% vs 62%) but worse at hit@5 (72% vs 90%); on the smaller Python
project it is better on both (100% vs 95%). With compact JSON and one-hop edges off by default
the full payload is 10.8 KB (Rust) / 10.3 KB (Python), below grep's 16.2 / 3.4 KB only on
Rust — grep reads less on the small Python repo — and the graph is slower in both cases. The
graph earns its cost on **relationships**: caller lists become precise instead of a pile of
textual matches (precision 77–79% -> 100%), and a two-hop question that grep answers at 27%
recall / 7% precision / 99 KB (Rust) becomes answerable at all (100% / 78% / 8.0 KB).
Doc-comment "concept" questions remain the weak spot.

CodeGraph bytes are the compact JSON payload, mean over the sample. Locate is 10.8 KB (Rust) /
10.3 KB (Python) for the full 10-row response, or 2.9 KB if only the first `budget_files` = 3
inspected rows are counted; impact 4.8 / 4.5 KB, chain 8.0 / 8.4 KB. Snippets are capped at the
default `content_chars` (2000 chars); one-hop edges are excluded unless
`include_graph_context: true` is passed.

### On a real agent (opencode + SWE-bench)

The rows above are deterministic retrieval. To check the agent layer, a real SWE-bench
Multilingual Rust instance was run by **opencode itself** (`opencode run --standalone`, its
own agent loop and MCP calls, `deepseek-flash`) — not a purpose-built harness.

- **Graph tools are used without extra prompting**, and dropping them made the agent slower,
  not cheaper: **58 steps / 5.0M prompt tokens** without CodeGraph vs **42 / 3.6M** with it.
  Without it the agent fell back to shell/grep trial-and-error and hung on `bat` stdin.
- **The patch shape was set by process rules, not the tool.** The same agent over-engineered
  the `-pp` fix (a 63-line restructure) with no TDD/minimal-change instruction, and matched
  the one-line upstream fix when TDD + minimal change were required.
- **The upstream fix only covered `-P`**; `-pp` stayed broken, and completing both takes a
  21-line `match` guard — not a reimplementation.

Full method, per-task traces and limitations:
[docs/evaluations/mcp-value-2026-11-04.md](docs/evaluations/mcp-value-2026-11-04.md).

Reproduce (deterministic sampling — same `--generate` and `--seed` give the same tasks):

```bash
python3 scripts/benchmarks/mcp_value.py --generate 40                  # locate (sentence arm)
python3 scripts/benchmarks/mcp_value.py --generate 40 --codegraph-query phrase
python3 scripts/benchmarks/mcp_value.py --mode impact --generate 15
python3 scripts/benchmarks/mcp_value.py --mode chain --depth 2 --generate 12
```

---

**Your codebase, understood.**

> [!TIP]
> **Give your coding agents lasting project memory.**
>
> Meet [**CodeGraph Agent Memory**](https://github.com/Jakedismo/codegraph-code-agent-memory-rust),
> our sibling project that builds on CodeGraph with persistent memory for architectural
> decisions, findings, failed approaches, and reusable procedures. Enable memory
> retrieval to bring that knowledge into agent workflows alongside current code
> evidence across sessions.
>
> [Explore the agent memory guide →](https://github.com/Jakedismo/codegraph-code-agent-memory-rust/blob/main/docs/AGENT_MEMORY.md)

CodeGraph transforms your entire codebase into a semantically searchable knowledge graph that AI agents can actually *reason* about—not just grep through.

> **Ready to get started?** Jump to the [Installation Guide](docs/INSTALLATION_GUIDE.md) for step-by-step setup instructions.
>
> **Already set up?** See the [Usage Guide](docs/USAGE_GUIDE.md) for tips on getting the most out of CodeGraph with your AI assistant.
>
> **Prefer shell commands?** Run `codegraph query <symbol>` to locate a definition and
> add `--callers` for callers. Pair it with `codegraph plan check|verify|store` for
> draft graphs. `codegraph init` offers project-local Claude Code/Codex hooks and adds
> agent instructions before indexing.

---

## What CodeGraph does

CodeGraph indexes a repository into a symbol/edge knowledge graph plus embeddings, and serves
it to coding agents over MCP, so they can ask relationship questions instead of grepping blind.

- **Graph + embeddings** — semantic search over a real call/definition graph, not just text.
- **Direct graph tools** — `semantic_code_search`, `get_reverse_dependencies`, `trace_call_chain`, … (9 in total, no LLM)
- **Tier-aware prompts and context protection** — fit the agent's real context window.
- **Tiered indexing** — fast / balanced / full, with incremental re-indexing.
- **Hybrid search** — vector + BM25 fused for retrieval.
- **Draft plan checks** — `codegraph plan check` validates a planned graph (entities, relations, constraints) before the code exists, and `codegraph plan store` keeps it as an isolated `plan:<id>` project; see [docs/PLAN_CHECK.md](docs/PLAN_CHECK.md).

For the problem, how it works, and why it matters, see
[docs/OVERVIEW.md](docs/OVERVIEW.md).

## Quick Start: add CodeGraph to a project

CodeGraph indexes a project once and then serves that graph to coding agents over MCP. The
walkthrough uses a real local project, `~/repos/github.com/paperclip` (TypeScript); substitute
your own path.

```bash
PROJECT=~/repos/github.com/paperclip
```

### 0. What you need (once)

- `codegraph` on your `PATH` (install in step 1).
- A **shared SurrealDB server** — the examples use `ws://127.0.0.1:8010`.
- An **embedding provider** for indexing. No LLM is needed: the MCP graph tools and the
  CLI query directly against the graph.

### 1. Install

**Prebuilt binary (recommended).** Download the archive for your platform from the
[latest release](https://github.com/zwidny/codegraph-rust/releases/latest), verify it, and
put `codegraph` on your `PATH`:

```bash
PLATFORM=linux-x86_64    # linux-aarch64 | macos-x86_64 | macos-aarch64
ASSET=codegraph-$PLATFORM.tar.gz
BASE=https://github.com/zwidny/codegraph-rust/releases/latest/download
curl -fLO "$BASE/$ASSET" "$BASE/$ASSET.sha256"
(shasum -a 256 -c "$ASSET.sha256") || (sha256sum -c "$ASSET.sha256")
tar xzf "$ASSET"
sudo install -m 0755 codegraph /usr/local/bin/codegraph
codegraph --version
```

Windows: download `codegraph-windows-x86_64.zip`, extract `codegraph.exe`, and add its folder
to `PATH`.

**From source (optional).** Requires Rust 1.95+; builds with all features:

```bash
git clone https://github.com/zwidny/codegraph-rust
cd codegraph-rust
./install-codegraph-full-features.sh
```

macOS faster builds (LLVM lld):

```bash
brew install llvm
make build-llvm
```

### 2. Configure providers

Put these in a `.env` in the project, or in `~/.codegraph.env` (read for every project).
`codegraph init` loads the project `.env`.

```bash
# Embeddings (required to index)
CODEGRAPH_EMBEDDING_PROVIDER=ollama          # ollama | lmstudio | jina | openai | onnx
CODEGRAPH_EMBEDDING_MODEL=qwen3-embedding:0.6b
CODEGRAPH_EMBEDDING_DIMENSION=1024

# Optional legacy settings: the shipped graph tools do not call an LLM
# CODEGRAPH_LLM_PROVIDER=anthropic
# CODEGRAPH_LLM_MODEL=claude-sonnet-4
# CODEGRAPH_CONTEXT_WINDOW=200000
```

Every variable is listed in `.env.example`; per-provider examples are in
[AI_PROVIDERS.md](docs/AI_PROVIDERS.md). Environment variables override any config file, and
the same settings can live in a `[llm]` / `[embedding]` config section (see
[Configuration](https://github.com/zwidny/codegraph-rust#configuration)).

### 3. Database (shared server, the default)

Run one SurrealDB server and point the client at it:

```bash
docker run -d --name cg-server -p 8010:8000 \
  -v cg-server-data:/data surrealdb/surrealdb:v3.3.0 \
  start --user root --pass root --bind 0.0.0.0:8000 surrealkv:/data
```

```bash
# .env (or ~/.codegraph.env)
CODEGRAPH_SURREALDB_URL=ws://127.0.0.1:8010
CODEGRAPH_SURREALDB_NAMESPACE=codegraph
CODEGRAPH_SURREALDB_DATABASE=codegraph
CODEGRAPH_SURREALDB_USERNAME=root
CODEGRAPH_SURREALDB_PASSWORD=root
```

Apply `schema/codegraph_v2.surql` once per database (see
[Setting Up SurrealDB](docs/INSTALLATION_GUIDE.md#setting-up-surrealdb)). One server serves
many projects; a read-only MCP server and `codegraph index` can run at the same time.

### 4. Index the new project

```bash
cd "$PROJECT"
codegraph init . --hooks both        # choose claude | codex | both | none
```

`init` adds a managed `# codegraph` section to `AGENTS.md` and `CLAUDE.md` (preserving other
content), then recursively indexes the project. Non-interactive forms:

```bash
codegraph init . --hooks none                    # instructions only
codegraph init . --hooks both --index-tier balanced
codegraph index . -r -l typescript,javascript    # index only, no hooks
```

**Index tiers.** Tiers control AST extraction, edge filtering and which analyzer stages run.
They no longer require external language servers: compiler-grade enrichment comes from a
SCIP index (below) and applies on every tier when one is supplied. For a very large
monorepo, use `--index-tier fast` and scope what you index:

```bash
codegraph init . --hooks both --index-tier fast
codegraph index server --index-tier fast -l typescript   # one subtree; belongs to the repo project
```

**Faster, reproducible indexing with SCIP.** Instead of relying on AST-only parsing,
import pre-built compiler indexes:

```bash
rust-analyzer scip .                          # Rust
NODE_OPTIONS=--max-old-space-size=16384 scip-typescript index --pnpm-workspaces   # TS/JS
scip-go                                       # Go
scip-python index --project-name <name> --output python.scip   # Python (run from the project root; do NOT pass --cwd)

CODEGRAPH_SCIP_INDEX="/abs/rust.scip,/abs/go.scip@easytier-go" \
  codegraph index . --index-tier balanced
```

Each entry is `<path>[@<subdir>]`, where `subdir` is the index's directory relative to the
project root. Supply one index per language in a mixed-language project; a language without
an index stays AST-only. On a 10k-file TypeScript project this cut the analysis phase from
21 minutes to 2.4 s plus a 3.9 s import, and contributed 62,711 definition edges that
AST-only parsing never produced. Details: [Indexing pipeline](docs/INDEXING_PIPELINE.md).

Indexing a subdirectory or a single file updates only that scope and leaves the rest
of the project graph intact; see
[incremental indexing](docs/INCREMENTAL_INDEXING.md) for project-root resolution and limits.

Verify the index:

```bash
codegraph status --detailed
codegraph query "RequestRouter" --callers        # fast, no LLM; reads the same store as `index` (CODEGRAPH_SURREALDB_URL or the default server)
```

### 5. Connect your agent (MCP)

For **opencode** (`~/.config/opencode/opencode.json`):

```json
{
  "mcp": {
    "servers": {
      "codegraph": {
        "type": "local",
        "command": ["/full/path/to/codegraph", "start", "stdio", "--watch"]
      }
    }
  }
}
```

For **Claude Code** (MCP config):

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

`--watch` keeps the index fresh as files change. The MCP server exposes the graph tools
(`semantic_code_search`, `get_reverse_dependencies`, `trace_call_chain`, …); they do not use
an LLM.

### 6. Use it

- **From an agent**: ask a code question; the agent calls the graph tools. Caller lists and
  two-hop chains ("who calls X", "what reaches X") are where the graph beats grep.
- **From the shell** (direct graph queries, no LLM):

```bash
codegraph query RequestRouter --callers
codegraph query handle_request --file src/router.rs
codegraph plan verify plan.json
```

- **Keep it fresh.** The MCP tools are read-only, so code changes reach the graph only after a
  re-index. Pick one:
  - `codegraph start stdio --watch` — the MCP server watches files and re-indexes
    incrementally (scope it with `--watch-path <dir>`; `--no-watch` disables it).
  - `codegraph daemon start .` — a separate background watcher (`daemon status` / `daemon stop`).
  - `codegraph index .` — manual **incremental** re-index (detects changed files, reuses the
    cache); add `--force` for a full rebuild, or `--complete-deferred` to finish deferred
    semantic inference. `codegraph index <dir|file>` narrows the update to that scope.
  - Set `[daemon] auto_start_with_mcp = true` to make plain `codegraph start` watch by default.

That's it — `~/repos/github.com/paperclip` is indexed and your agent can reason over its graph.

---

## Historical agent-CLI evaluation

The former server-side agent and its CLI commands were removed in v0.3.0. Their
evaluation records are kept as history only:

- [Balanced CLI run (2026-10-06)](docs/evaluations/balanced-cli-2026-10-06.md)
- [Full CLI run (2026-10-06)](docs/evaluations/full-cli-2026-10-06.md)

---

## The Architecture

> **[View Interactive Architecture Diagram](docs/architecture/codegraph-architecture.html)** - Explore the full workspace structure with clickable components and layer filtering.

```
┌─────────────────────────────────────────────────────────────────┐
│                         Claude Code / MCP Client                │
└─────────────────────────────────┬───────────────────────────────┘
                                  │ MCP Protocol
                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│                        CodeGraph MCP Server                     │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                  Direct Graph Tools (9)                   │  │
│  │  semantic search · reverse/transitive deps · call chains  │  │
│  │  coupling · cycles · hotspots · hubs · plan_check         │  │
│  └───────────────────────────────────────────────────────────┘  │
│  ┌───────────────────────────┼───────────────────────────────┐  │
│  │                  Inner Graph Tools                        │  │
│  │  ┌──────────────┐ ┌──────────────┐ ┌──────────────────┐   │  │
│  │  │ Transitive   │ │    Call      │ │     Coupling     │   │  │
│  │  │ Dependencies │ │   Chains     │ │     Metrics      │   │  │
│  │  └──────────────┘ └──────────────┘ └──────────────────┘   │  │
│  │  ┌──────────────┐ ┌──────────────┐ ┌──────────────────┐   │  │
│  │  │   Reverse    │ │    Cycle     │ │       Hub        │   │  │
│  │  │    Deps      │ │  Detection   │ │      Nodes       │   │  │
│  │  └──────────────┘ └──────────────┘ └──────────────────┘   │  │
│  └───────────────────────────┬───────────────────────────────┘  │
└──────────────────────────────┼──────────────────────────────────┘
                               │
┌──────────────────────────────┼──────────────────────────────────┐
│                         SurrealDB                               │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │   Nodes     │  │    Edges    │  │   Chunks + Embeddings   │  │
│  │  (AST +     │  │  (calls,    │  │   (HNSW vector index)   │  │
│  │   FastML)   │  │   imports)  │  │                         │  │
│  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
│                                                                 │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │              SurrealQL Graph Functions                     │ │
│  │   fn::semantic_search_nodes_via_chunks                     │ │
│  │   fn::semantic_search_chunks_with_context                  │ │
│  │   fn::get_transitive_dependencies                          │ │
│  │   fn::trace_call_chain                                     │ │
│  │   fn::calculate_coupling_metrics                           │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

**Key insight:** The graph tools are composable primitives. The client chains them and
reads the evidence. Answering "what breaks if I change this?" might chain:

1. `semantic_code_search` to find the target and its `node_id`
2. `get_reverse_dependencies` for direct callers
3. `get_transitive_dependencies` for the wider blast radius
4. `detect_circular_dependencies` if the change crosses module boundaries
5. `calculate_coupling_metrics` on the affected hubs
6. `find_complexity_hotspots` to prioritise the risky areas
7. Read the named files and draw the conclusion

---

## Supported Languages

CodeGraph uses tree-sitter for initial parsing and enhances results with FastML algorithms and supports:

Rust • Python • TypeScript • JavaScript • Go • Java • C++ • C • Swift • Kotlin • C# • Ruby • PHP • Dart

---

## Provider Flexibility

### Embeddings
Use any model with dimensions 384-4096:
- **Local:** Ollama, LM Studio, ONNX Runtime
- **Cloud:** OpenAI, Jina AI

### LLM
Not required. The graph tools and CLI queries run without a model; the `[llm]` config
section is parsed for compatibility only.

### Database
- **SurrealDB 3.x shared server or container** at `ws://127.0.0.1:8000` by default, or via `CODEGRAPH_SURREALDB_URL`, with HNSW vector indexes - concurrent MCP reads and `codegraph index` against the same project, and multiple clients; also [Surreal Cloud](https://surrealdb.com/cloud)
- **Embedded SurrealKV** opt-in through a `surrealkv://` URL, for offline or single-shot use - compatibility only, substantially slower, single writer per project

---

## Configuration

Building requires **Rust 1.95 or newer** and uses edition **2024**. Direct registry
dependencies target current stable releases; `Cargo.lock` records the resolved graph. Bincode stays on **2.0.1**, its last
functional release; 3.0.0 deliberately fails compilation. ONNX Runtime bindings use
the latest release candidate, **2.0.0-rc.13**, because no stable 2.0 release exists.

The CLI loads project or user dotenv configuration before starting worker threads.
Library callers should initialize their environment at process startup; `ConfigManager::load()`
only reads configuration and never changes the process environment.

`codegraph init` configures the selected project and loads its environment before
indexing. `codegraph config init` is the separate command for creating global
application configuration; project init does not configure model providers.

Config files are read from `./.codegraph.toml` (project) and then `~/.codegraph/config.toml`
(user); a `.env` in the working directory and `CODEGRAPH_*` environment variables override
them:

```toml
[embedding]
provider = "ollama"
model = "qwen3-embedding:0.6b"
dimension = 1024

[llm]
provider = "anthropic"
model = "claude-sonnet-4"
context_window = 200000

[indexing]
tier = "fast"
```

The `[llm]` section is parsed for configuration compatibility but the shipped graph tools do not call an LLM. [AI_PROVIDERS.md](docs/AI_PROVIDERS.md) lists every variable and key.

Storage: `codegraph` connects to a shared SurrealDB server at `ws://127.0.0.1:8000` by
default, or to `CODEGRAPH_SURREALDB_URL` (plus the other `CODEGRAPH_SURREALDB_*` variables)
when set. The embedded per-project store is opt-in through a `surrealkv://` URL: it exists
for compatibility, is substantially slower, and lets only one process hold the project
directory. `CODEGRAPH_SCHEMA=v1` selects the original `schema/codegraph.surql` for new
stores.

See [INSTALLATION_GUIDE.md](docs/INSTALLATION_GUIDE.md) for complete configuration options.

### Experimental graph schema (optional)

CodeGraph can run against an experimental SurrealDB **graphdb-style schema** (`schema/codegraph_graph_experimental.surql`) that is interoperable with the existing CodeGraph tools and indexing pipeline.

Compared to the default schema (`schema/codegraph_v2.surql`), the experimental schema is designed for faster and more efficient graph-query operations (traversals, neighborhood expansion, and tool-driven graph analytics) on large codebases.

To use it with the embedded store, set the flag before the project's store is first created
(or delete `<project>/.codegraph/db` and re-index):

```bash
CODEGRAPH_USE_GRAPH_SCHEMA=true
```

With a SurrealDB server, load the schema into a dedicated database once and point CodeGraph at it:

```bash
surreal sql --conn ws://localhost:3004 --ns ouroboros --db codegraph_experimental < schema/codegraph_graph_experimental.surql
CODEGRAPH_USE_GRAPH_SCHEMA=true
CODEGRAPH_GRAPH_DB_DATABASE=codegraph_experimental
```

Notes:
- The schema file defines HNSW indexes for multiple embedding dimensions (384–4096) so you can switch embedding models without reworking the DB.
- An existing store keeps the schema it was created with; switching schemas means a fresh store.
- `CODEGRAPH_GRAPH_DB_DATABASE` only applies in server mode.

---

## Daemon Mode

Keep your index fresh automatically:

```bash
# With MCP server (recommended)
codegraph start stdio --watch

# Standalone daemon
codegraph daemon start /path/to/project --languages rust,typescript
```

Changes are detected, debounced, and re-indexed in the background.

With the embedded per-project store, use `--watch`: the watcher then runs inside the MCP
server process. A standalone daemon is a separate process and cannot share a project's store
with a running server, so use it only when no server is running for that project or when you
use a SurrealDB server.

---

## What's Next

- [ ] More language support
- [ ] Cross-repository analysis
- [ ] Custom graph schemas
- [ ] Plugin system for custom analyzers

---

## Philosophy

CodeGraph exists because we believe AI coding assistants should be *augmented*, not replaced. The best AI-human collaboration happens when the AI has deep context about what you're working with.

We're not trying to replace your IDE, your type checker, or your tests. We're giving your AI the context it needs to actually help.

**Your codebase is a graph. Let your AI see it that way.**

---

## License

MIT

---

## Links

- [Installation Guide](docs/INSTALLATION_GUIDE.md)
- [SurrealDB Cloud](https://surrealdb.com/cloud) (free tier)
- [Jina AI](https://jina.ai) (free API tokens)
- [Ollama](https://ollama.com) (local models)

---

![CodeGraph](docs/assets/footer.jpg)
