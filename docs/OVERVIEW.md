# How CodeGraph works

> Overview of the problem CodeGraph solves, what it does differently, and why it
> matters for AI coding. For setup, see the [Quick Start](../README.md#quick-start-add-codegraph-to-a-project).

## The Problem

AI coding assistants are powerful, but they're flying blind. They see files one at a time, grep for patterns, and burn tokens trying to understand your architecture. Every conversation starts from zero.

**What if your AI assistant already knew your codebase?**

---

## What CodeGraph Does Differently

### 1. Graph + Embeddings = True Understanding

Most semantic search tools create embeddings and call it a day. CodeGraph builds a **real knowledge graph**:

```
Your Code → Build Context → AST + FastML → SCIP Resolution → Enrichment → Graph + Embeddings
              ↓               ↓              ↓              ↓              ↓         ↓
          Packages        Nodes/edges    Type-aware     API surface      Graph    Semantic
          Features        Fast patterns  linking        Module graph   traversal   search
          Targets         Spans          Definitions    Dataflow/Docs             (hybrid)
```

When you search, you don't just get "similar code"—you get code with its **relationships intact**. The function that matches your query, plus what calls it, what it depends on, and where it fits in the architecture.

Indexing enrichment adds:
- Module nodes and module-level import/containment edges for cross-file navigation
- Rust-local dataflow edges (`defines`, `uses`, `flows_to`, `returns`, `mutates`) for impact analysis
- Document/spec nodes linked to backticked symbols in `README.md`, `docs/**/*.md`, and `schema/**/*.surql`
- Architecture signals (package cycles + optional boundary violations)

#### Indexing tiers (speed vs richness)

Indexing is tiered so you can choose between speed/storage and graph richness. The default is **fast**.

| Tier | What it enables | Typical use |
|------|-----------------|-------------|
| `fast` | AST nodes + core edges only; SCIP definitions when an index is supplied | Quick indexing, low storage |
| `balanced` | SCIP definitions + docs/enrichment + module linking | Richer navigation and documentation with moderate analyzer cost |
| `full` | All analyzers + SCIP references + dataflow + architecture | Maximum graph richness and analyzer coverage |

Historical agent-CLI accuracy reports are kept in [docs/evaluations](evaluations/); the
agent CLI itself was removed in v0.3.0.

Tier behavior details:
- `fast`: disables build context, enrichment, module linking, dataflow, docs/contracts, and architecture; filters out `Uses`/`References` edges. A supplied SCIP index is still imported.
- `balanced`: enables build context, enrichment, module linking, and docs/contracts; filters out `References` edges.
- `full`: enables all analyzers and keeps `References` edges; no edge filtering.

Configure the tier:
- CLI: `codegraph index /path/to/project --index-tier balanced`
- Env: `CODEGRAPH_INDEX_TIER=balanced`
- Config: `[indexing] tier = "balanced"`

Directory indexing scans subdirectories by default, so
`codegraph index --languages Rust --index-tier balanced .` finds Rust sources in workspace crates. Use
`--no-recursive` for an intentional root-only scan; `-r`/`--recursive` remain accepted.
`codegraph estimate` uses the same traversal defaults.

#### Compiler-index (SCIP) enrichment

Compiler-grade enrichment is optional and comes from a pre-built SCIP index. There is no
language-server process and no per-language tool check at index time:

- Generate an index per language with `scripts/scip-index.sh` (which drives the language's
  own indexer: `rust-analyzer scip` for Rust, `scip-typescript index` for TS/JS,
  `scip-go` for Go, and `scip-python index` for Python — run it from the project root,
  never with `--cwd`, which yields a 72-byte empty index).
- Pass the paths in `CODEGRAPH_SCIP_INDEX` (comma-separated, `path@subdir` for an index
  built inside a subdirectory). The same index is imported on every tier.
- A language without an index stays AST-only; import fails loudly when no document matches
  an indexed source, instead of reporting success with zero enrichment.
- Positions are validated against the captured sources. `CODEGRAPH_SCIP_TRUST_SOURCE=1`
  opts out of hash validation; `CODEGRAPH_SCIP_MAX_BYTES` caps artifact size.
- `CODEGRAPH_ANALYZERS=0` disables the AST analyzer stages independently of tier.

Source validation and sidecar requirements are described in the
[indexing implementation guide](indexing-performance-work.md).

#### Incremental indexing and inference policies

`--batch-size` sets the maximum number of embedding texts per batch for both local
and cloud providers. An explicit value wins over `CODEGRAPH_EMBEDDINGS_BATCH_SIZE`,
its legacy alias `CODEGRAPH_EMBEDDING_BATCH_SIZE`, and `[embedding] batch_size` in
TOML, in that order; the default is 64. Memory-based tuning does not change explicit
values, including `--batch-size 100`.

```bash
codegraph index --languages Rust --index-tier balanced --batch-size 512 .
```

Token/byte budgets, cache hits and provider API limits can produce smaller actual
requests. For larger requests, adjust `CODEGRAPH_EMBEDDING_BATCH_TOKENS` (default at
least the resolved input context, with local/remote floors of 8192/32768) and
`CODEGRAPH_EMBEDDING_BATCH_BYTES` (default 1 MiB) as needed.
Logs show these inference limits separately from database write batches;
`CODEGRAPH_CHUNK_DB_BATCH_SIZE` defaults to at most 32 rows and is capped at 512.
Ollama and LM Studio have no extra fixed 256-text cap.

Full, single-file and watch indexing share complete-project reconciliation. Unchanged
sources reuse cached AST/analyzer artifacts while the full catalog retains callers
across edits, renames and deletions. Only changed graph records and file metadata are
written. Source snapshots, parsing, inference and writer queues have independent
resource bounds; completion follows durable acknowledgements and final input checks.
`--force` prepares and reconciles again without trusting the previous catalog.

Embedding and semantic-resolution work is independent of extraction tier. Each accepts
`sync` (default when its feature is compiled), `deferred` or `off`:

```bash
CODEGRAPH_EMBEDDING_POLICY=deferred CODEGRAPH_SEMANTIC_RESOLUTION=off \
  codegraph index /path/to/project --index-tier fast --stats-json indexing.json
codegraph index /path/to/project --complete-deferred --stats-json completed.json
```

Deferred runs persist a resumable job and distinguish graph readiness from pending
inference. Prepared-text embedding caches include model/task/tokenizer/runtime identity;
chunking preserves Unicode and enforces provider token budgets. Mutable model aliases
expire; `CODEGRAPH_MODEL_REVISION` declares an immutable revision.

### Embedding inputs and chunking

Chunk planning starts from AST node source spans, such as functions and classes.
A unit that fits the complete input budget stays intact. Oversized units split at
Tree-sitter statement/block boundaries, then merge adjacent pieces while they fit.
Oversized leaves and unsupported syntax use UTF-8-safe line/token splitting.
Unicode and structural whitespace are preserved; overlap uses token counts.

For Ollama, the budget follows model metadata and serving context. Qwen3 embedding
models support [32K inputs](https://github.com/QwenLM/Qwen3-Embedding#model-overview);
[nomic-embed-text-v2-moe](https://huggingface.co/nomic-ai/nomic-embed-text-v2-moe)
supports 512 tokens. Counting includes document/query prefixes and special tokens.
Nomic text models receive `search_document: ` / `search_query: `; Qwen3 queries
receive a retrieval instruction. Every Ollama request uses `truncate=false`:
context mismatch fails visibly instead of silently dropping source.

Recognized models automatically load their matching publisher tokenizer, caching
`tokenizer.json` and downloading it on first use if absent. Model weights are not
downloaded. Custom/offline models can supply `CODEGRAPH_TOKENIZER_PATH`; an unknown
model requires that path or `CODEGRAPH_TOKENIZER_REPO`.

| Control | Behavior |
|---------|----------|
| `CODEGRAPH_CHUNK_MAX_TOKENS` | Lower the complete-input target; capped by serving context. Legacy `CODEGRAPH_MAX_CHUNK_TOKENS` has lower precedence. |
| `CODEGRAPH_CHUNK_SMART_SPLIT=0` | Use token splitting instead of AST boundaries for oversized units. Default: AST splitting. |
| `CODEGRAPH_CHUNK_OVERLAP_TOKENS` | Maximum suffix overlap in tokens; default 64, zero disables. Shrunk to fit the next input. |
| `CODEGRAPH_EMBEDDING_SKIP_CHUNKING=1` | Keep nodes intact; fail before inference if any exceeds the configured input limit. |
| `CODEGRAPH_OLLAMA_NUM_CTX` | Set request context, bounded by the model maximum. Otherwise use Modelfile/running context when reported. |
| `CODEGRAPH_MODEL_MAX_TOKENS` | Supply missing model context metadata or lower the advertised maximum. |
| `CODEGRAPH_TOKENIZER_PATH` | Use a matching local `tokenizer.json`, without downloading it. |
| `CODEGRAPH_TOKENIZER_REPO`, `CODEGRAPH_TOKENIZER_REVISION` | Override publisher tokenizer repository/revision; default revision `main`. |
| `CODEGRAPH_EMBEDDING_DOCUMENT_PREFIX`, `CODEGRAPH_EMBEDDING_QUERY_PREFIX` | Override retrieval prefixes for custom models. |

`--batch-size` controls texts per inference request, independently of chunk length.
Default request token budgets grow to accommodate the resolved input context
(local floor 8192; remote floor 32768). Explicit `CODEGRAPH_EMBEDDING_BATCH_TOKENS`
and `CODEGRAPH_EMBEDDING_BATCH_BYTES` remain separate bounds; an input exceeding
one fails with advice to raise that bound or lower the chunk target.

Reindex after upgrading: tokenizer/task/context and chunk-policy changes invalidate
old artifacts and embeddings. Offline mocks verify preparation and request limits;
retrieval quality and indexing speed still need measurements on your project.

`CODEGRAPH_VECTOR_INDEX_MODE=all|selected|deferred|off` controls HNSW construction for
fresh embedded stores. The default retains all schema dimensions; selected builds the
active dimension, deferred builds after durable ingestion, and off reports vector
readiness false. Existing shared indexes are preserved. Alternate splitters, candidate
caps and runtime/precision choices remain opt-in.

See [configuration and invariants](indexing-performance-work.md) and
[reproducible speed/quality benchmarks](indexing-benchmarks.md). The offline debug
fixture results verify behavior; production throughput and model quality require
representative measurements.

Semantic relationship scoring caches vector norms and scores independent unresolved
names in the existing worker-limited CPU pool, outside async runtime workers. Candidate
order, the cosine threshold and ambiguous-tie handling remain unchanged. `--stats-json`
now separates exact/lexical matching, semantic candidate selection, symbol embedding,
CPU scoring, and edge preparation/writes within relationship resolution.

If SCIP import fails with a stale-index diagnostic, regenerate the artifact for the current
sources (`scripts/scip-index.sh` or `rust-analyzer scip .`) and re-run indexing. A language
without an index is not an error: it simply stays AST-only.

#### Optional architecture boundary rules

If you want CodeGraph to flag forbidden package dependencies, add `codegraph.boundaries.toml` at the project root:

```toml
[[deny]]
from = "your_crate"
to = "forbidden_crate"
reason = "explain the boundary"
```

Indexing will emit `violates_boundary` edges when a `depends_on` relationship matches a deny rule.

### 2. Direct Graph Tools, Not Just Search

CodeGraph doesn't return a list of files and wish you luck. It ships **9 direct graph
tools** that answer one concrete question each, in milliseconds and without an LLM:

| Tool | What It Actually Does |
|------|----------------------|
| `semantic_code_search` | Find a symbol or concept; returns the `node_id` the other tools need |
| `get_reverse_dependencies` | Who calls or depends on a node |
| `get_transitive_dependencies` | The dependency closure from a node |
| `trace_call_chain` | The call path starting at a node |
| `calculate_coupling_metrics` | Afferent/efferent coupling and instability for a node |
| `detect_circular_dependencies` | Dependency cycles for an edge type |
| `find_complexity_hotspots` | Complexity/risk ranking |
| `get_hub_nodes` | High-degree hub ranking |
| `plan_check` | Validate a draft nodes/edges/constraints graph offline |

The client composes these itself. A change-impact question becomes:
`semantic_code_search` → `get_reverse_dependencies` → `get_transitive_dependencies` →
read the returned files. An architecture question becomes `get_hub_nodes` +
`detect_circular_dependencies` + `find_complexity_hotspots`. Results carry `file:line`
locations and bounded snippets, so the client can verify against current source.

### 3. Context Protection

Long `content` in a tool result (a whole document, an `impl` block) is cut to a leading
snippet, 2,000 characters by default, and marked `content_truncated` with its original
length. The row keeps its file path and line range, so the client can read the rest.
One tool result is limited to `context_window × 2` bytes, capped at 200 KB however large
the window is; oversized array results keep their leading items and carry `_truncated`
metadata.

**Configure via environment:**
```bash
# Set this to match your agent LLM's real context window
CODEGRAPH_CONTEXT_WINDOW=128000            # default: 128K

# Optional snippet-length override
CODEGRAPH_TOOL_CONTENT_CHARS=2000          # per row; 0 disables shortening
```

### 4. Hybrid Search That Actually Works

We don't pick sides in the "embeddings vs keywords" debate. CodeGraph combines:

- **70% vector similarity** (semantic understanding)
- **30% lexical search** (exact matches matter)
- **Graph traversal** (relationships and context)
- **Optional reranking** (cross-encoder precision)

The result? You find `handleUserAuth` when you search for "login logic"—but also when you search for "handleUserAuth".

---
![Intelligence](assets/mid.png)

## Why This Matters for AI Coding

When you connect CodeGraph to Claude Code, Cursor, or any MCP-compatible agent:

**Before:** Your AI reads files one by one, grepping around, burning tokens on context-gathering.

**After:** Your AI calls `semantic_code_search("UserService")` for the `node_id`, then `get_reverse_dependencies(node_id, "calls")` and instantly sees what breaks if you refactor it.

This isn't incremental improvement. It's the difference between an AI that *searches* your code and one that *understands* it.

### Why this is powerful for code agents

CodeGraph shifts the *cognitive load* (search + relevance + dependency reasoning) into targeted graph queries, so your code agent can spend its context budget on *making the change*, not *discovering what to change*.

#### What a tool result looks like (example)

`get_reverse_dependencies` returns structured rows with file paths, line numbers and bounded snippets:

```json
{
  "node_id": "nodes:`0f0f013b-...`",
  "edge_type": "calls",
  "results": [
    {
      "node_id": "nodes:`a1b2c3d4-...`",
      "name": "IndexStorage",
      "file_path": "crates/codegraph-graph/src/storage.rs",
      "line_number": 48,
      "content": "pub struct ...",
      "content_truncated": false
    }
  ]
}
```

#### What a code agent would otherwise have to do

Without CodeGraph, a code agent typically needs multiple “single-purpose” calls to reach the same confidence:
- search for the symbol (often multiple strategies: text + semantic + ripgrep-style search)
- open and read multiple files (definition + usages + callers + related modules)
- reconstruct dependency/call graphs mentally from partial evidence
- repeat when a guess is wrong (more reads, more tokens)

This burns context quickly: reading “just” a handful of medium-sized files + surrounding context can easily consume tens of thousands of tokens, and larger repos can push into hundreds of thousands depending on how much code gets pulled into context.

With CodeGraph, the agent gets *pinpointed locations and relationships* (plus bounded context) and can keep far more of the context window available for planning and implementing changes.

---
