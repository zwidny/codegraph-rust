# CodeGraph (with updates)

## Measured value: CodeGraph vs grep

Measured on two real repositories, against the workflow an agent has without CodeGraph:
grep with line numbers, then read a window around the hit. No chat LLM is involved; the
graph arm does embed the query, which is why its latency is higher on the locate case.

| Question | Repo | grep | CodeGraph | Result |
|---|---|---|---|---|
| Where is X defined? | Rust | 79% hit@1 · 10.2 KB · 8 ms | **93%** · 15.6 KB · 0.75 s | graph is more accurate; grep is cheaper and far faster |
| Where is X defined? | Python | **86%** · 3.9 KB · 8 ms | 64% · 17.3 KB · 0.96 s | grep wins outright |
| Who calls X? | Rust | 94% recall · 70% precision · 8.5 KB · 6 ms | 93% · **100%** · 6.3 KB · 0.12 s | graph trades latency for precision |
| Who calls X? | Python | 100% · 69% · 9.2 KB · 8 ms | 100% · **100%** · 5.7 KB · 0.12 s | same trade-off |
| What does X reach? (depth 2) | Rust | 26% recall · 12% precision · 101 KB · 0.13 s | **100%** · 94% · **10.8 KB** · 0.24 s | graph gives the only usable answer |
| What does X reach? (depth 2) | Python | 15% · 7% · 78 KB · 0.15 s | **100%** · 100% · **18.2 KB** · 0.17 s | same |

`grep`'s caller **recall is not comparable evidence**: it counts every textual mention of the
name (definition, comments, strings), so a true caller file is almost always hit even when
grep misattributes the hit. Read the caller rows by **precision** — 70% vs 100% — not recall.

The honest read: for *"where is this symbol"*, grep is better — it reads less and answers in
milliseconds, and on Python it is also more accurate. The graph earns its cost on
**relationships**: caller lists become precise instead of a pile of textual matches, and a
two-hop question that grep answers at 15–26% recall becomes answerable at all. Reading cost
per query stays between 6 and 18 KB in every case.

CodeGraph bytes are measured with the default `content_chars` cap (code snippets truncated to
2000 chars). Both locate rows are re-measured under that cap: Rust 15.6 KB / 16.3 KB median
(was 18.1 / 19.2 full), Python 17.3 KB / 17.8 KB median (was 19.4 / 19.9 full).

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

Reproduce:

```bash
python3 scripts/benchmarks/mcp_value.py --generate 14       # locate
python3 scripts/benchmarks/mcp_value.py --mode impact       # who calls
python3 scripts/benchmarks/mcp_value.py --mode chain --depth 2
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
> **Prefer shell commands?** Run `codegraph agent context "your question"`. The same four
> agentic tools are available through the [CLI, with project-local Claude Code/Codex hooks](docs/AGENTIC_CLI.md).
> `codegraph init` offers hook setup and adds agent instructions before indexing.

---

## What CodeGraph does

CodeGraph indexes a repository into a symbol/edge knowledge graph plus embeddings, and serves
it to coding agents over MCP, so they can ask relationship questions instead of grepping blind.

- **Graph + embeddings** — semantic search over a real call/definition graph, not just text.
- **Agentic tools** — `semantic_code_search`, `get_reverse_dependencies`, `trace_call_chain`, …
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
- An **embedding provider** for indexing. An **LLM** is only needed for the agentic tools
  (`codegraph agent ...`), not for indexing or the graph MCP tools.

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

# Agent LLM (only for `codegraph agent`, not for the MCP graph tools)
CODEGRAPH_LLM_PROVIDER=anthropic
CODEGRAPH_LLM_MODEL=claude-sonnet-4
CODEGRAPH_CONTEXT_WINDOW=200000
ANTHROPIC_API_KEY=sk-ant-...
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

**Index tiers.** `balanced` and `full` run language servers (`typescript-language-server`,
`clangd`, `gopls`, …) and **fail if those tools are missing**. On a machine without them — or
for a very large monorepo — use `--index-tier fast` and scope what you index:

```bash
codegraph init . --hooks both --index-tier fast
codegraph index server --index-tier fast -l typescript   # one subtree; belongs to the repo project
```

**Faster, reproducible indexing with SCIP.** Instead of driving a language server once per
file, import pre-built SCIP indexes:

```bash
rust-analyzer scip .                          # Rust
NODE_OPTIONS=--max-old-space-size=16384 scip-typescript index --pnpm-workspaces   # TS/JS
scip-go                                       # Go

CODEGRAPH_SCIP_INDEX="/abs/rust.scip,/abs/go.scip@easytier-go" \
  codegraph index . --index-tier balanced
```

Each entry is `<path>[@<subdir>]`, where `subdir` is the index's directory relative to the
project root. Setting the variable disables language servers for the run, so supply one index
per language in a mixed-language project. On a 10k-file TypeScript project this cut the
analysis phase from 21 minutes to 2.4 s plus a 3.9 s import, and contributed 62,711
definition edges the LSP path never produced. Details: [Indexing pipeline](docs/INDEXING_PIPELINE.md).

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
- **From the shell** (agentic tools need the LLM):

```bash
codegraph agent context "how does the request router dispatch"
codegraph agent impact  "if I change RequestRouter, what breaks?"
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

## Agent CLI accuracy by indexing tier

The CLI evaluation uses the same eight questions as the HTTP MCP test, defined in
[agentic_test_cases.py](agentic_test_cases.py). They cover configuration loading,
prompt selection, caching, dependencies, call chains, architecture, public APIs and
complexity across all four agent tools. [test_cli_agentic.py](test_cli_agentic.py)
saves full responses for manual comparison with source.

The fast entry below predates the current model-aware tokenizer/context/chunk policy.
Future tier comparisons should use the same current input policy and model settings;
the historical entry remains a record of that run.

| Indexing tier | LLM / request model | Evaluation date | CLI response checks | Manual accuracy findings |
|---------------|-----|-----------------|---------------------|--------------------------|
| `fast` | `gpt-6-luna` (confirmed by the project owner; not captured by the runner) | 2026-10-06 | 8/8 `OK` | Mixed: useful findings, incomplete answers and at least two source-confirmed incorrect answers; no overall accuracy score assigned |
| `balanced` (original) | `gpt-6-luna` (explicit request model) | 2026-10-06 | 8/8 `OK` | Useful configuration/cache/call-chain answers and public-method list; incorrect direct-caller classification, incomplete hub results and metric caveats; no overall accuracy score assigned |
| `balanced` (refreshed) | `gpt-6-luna` (explicit request model) | 2026-10-06 | 8/8 `OK` | Hub ranking, ratio arithmetic and wrapper explanation fixed; prompt/cache/call-chain coverage incomplete, one API method omitted; API case takes 587.5 seconds; no overall accuracy score assigned |
| `full` (original) | `gpt-6-luna` (explicit request model) | 2026-10-06 | 7/8 `OK`, 1 `TIMEOUT` | Source-aligned configuration, cache, call-chain and public-API answers with verified locations; weak substitute for the missing symbol; hub ranking unavailable and instability values unreliable; the tier-aware prompt case timed out twice; no overall accuracy score assigned |
| `full` (refreshed) | `gpt-6-luna` (explicit request model) | 2026-10-06 | 8/8 `OK` | Prompt answer, hub ranking and instability arithmetic now work; cache details incomplete, two API methods omitted, stale LATS comment repeated; architecture case takes 546.4 seconds; no overall accuracy score assigned |

**`OK` measures command/response success, not factual correctness.** It means the
command returned a valid JSON answer without a reported timeout or partial-result
marker. It does not require every inner graph tool to succeed. All eight fast-tier
responses passed this check despite factual mistakes. The response field
`tier: Massive` describes the agent's context-window budget, independently of the
indexing tier.

Source review of the fast-tier answers found:

- **Call chain (case 5): incorrect active implementation.** The answer selected
  the `#[cfg(not(feature = "ai-enhanced"))]` error stub for `execute_agentic_workflow`
  and concluded the workflow did not reach graph tools. The
  [AI-enabled implementation](crates/codegraph-mcp-server/src/official_server.rs#L559)
  creates `GraphToolExecutor` and invokes the Rig agent executor. The answer did not
  distinguish the two conditional implementations.
- **Public API (case 7): incomplete API and incorrect usage conclusion.** The answer
  described `GraphToolExecutor` as having no direct usage and suggested changes were
  isolated and low-risk. Its [public methods](crates/codegraph-mcp-tools/src/graph_tool_executor.rs#L130)
  include constructors, `execute`, cache controls and tool metadata accessors; it is
  used by both the [server workflow](crates/codegraph-mcp-server/src/official_server.rs#L691)
  and the [Rig tool factory](crates/codegraph-mcp-rig/src/tools/factory.rs#L16).
- **LRU cache (case 3): useful but incomplete.** The answer correctly identified
  tool-result caching by function and parameters, but did not explain the cache-hit,
  miss, eviction or clearing behavior requested by the question.
- **Missing symbol (case 4): appropriately uncertain.** The answer reported that
  `PromptSelector` could not be found rather than inventing its dependencies. No
  Rust definition with that name exists in the reviewed source; this question needs
  that caveat when interpreting the results.

The balanced run `20261006_041913_884481` explicitly selected `gpt-6-luna` through
`CODEGRAPH_LLM_MODEL`. All eight commands returned answers in **462.8 seconds**.
The stored index reported 250 files, 22,496 nodes, 48,224 edges and 22,496 chunks,
with embedding and semantic stages ready. It used Ollama `qwen3-embedding:0.6b`
with 1,024-dimensional vectors and a resolved 32,768-token serving context.
[Full balanced answers, run details and source review](docs/evaluations/balanced-cli-2026-10-06.md)
are retained separately from the README summary.

Source review of the original explicit-model balanced answers found:

- **Configuration, prompts and cache (cases 1–3): useful source-aligned explanations.**
  The answers covered multiple configuration systems, dotenv/TOML precedence, the
  active Rig prompt builder, and cache hits/misses, successful-result insertion,
  eviction, clearing and lack of TTL. The prompt answer also identified separate
  server/Rig tier-resolution paths. Its claim of an eight-round "hard cap" needs
  qualification: [the public builder override](crates/codegraph-mcp-rig/src/agent/builder.rs#L104)
  accepts other values.
- **Missing symbol and call chain (cases 4–5): appropriate caveats and active flow.**
  The answer did not invent `PromptSelector`, stated its alternate target, and
  distinguished the AI-enabled workflow from the feature-disabled stub. It traced
  the active path through Rig tools, `CountingExecutor`, `GraphToolExecutor` and
  `GraphFunctions` to SurrealDB.
- **Public API (case 7): complete method names, incorrect caller classification.**
  The answer listed all ten public inherent methods and recognized real consumers.
  However, it called the eight Rig tool adapters *direct* callers of
  `GraphToolExecutor::execute`. They call
  [`CountingExecutor::execute`](crates/codegraph-mcp-rig/src/tools/counting_executor.rs#L118),
  which then delegates to `GraphToolExecutor`; those adapters are indirect callers.
- **Architecture/metrics (cases 6–7): incomplete evidence despite `OK`.** Hub queries
  failed five times across these cases with SurrealDB's `array::concat()`
  1,048,576-byte limit. The architecture answer disclosed the missing hub ranking
  and flagged inconsistent coupling values and misleading zero struct-level counts.
  For example, reported Ca=19/Ce=66 with instability=0.0 does not match the intended
  Ce/(Ca+Ce) ratio. These metrics cannot support a reliable stability conclusion.
- **Complexity (case 8): useful reported ranking with measurement limits.** The
  answer ranked `reconcile_project` first at complexity 108/risk 1,404. The
  [query](schema/codegraph_v2.surql#L555) computes risk as complexity × (incoming
  dependency-edge count + 1), after preselecting high-complexity candidates. It is
  a heuristic, not a calibrated failure probability or a guaranteed global risk
  ranking; incoming edge counts also need not equal distinct caller counts.

The refreshed balanced run `20261006_151920_418410` again explicitly requested
`gpt-6-luna`. All eight cases returned `OK` in **1,053.3 seconds**, with **131 tool
calls** and **177 summed locations**. It used the newly ingested index (238 files,
21,832 nodes, 43,266 edges, 23,387 chunks), Jina `jina-embeddings-v5-text-small`
(1,024 dimensions), a 512-token AST chunk policy and the current 600-second deadline.
[The refreshed review and all eight new answers](docs/evaluations/balanced-cli-2026-10-06.md#refreshed-balanced-run-1519-utc)
are preserved alongside the original run.

- **Verified improvements:** The architecture case returns hub rankings without
  the former `array::concat` failure; all seven displayed instability ratios match
  Ce/(Ca+Ce) after rounding. The API answer correctly describes Rig adapters as
  indirect consumers through `CountingExecutor`. No graph-tool failures or Jina
  422 errors are logged in the eight commands.
- **Remaining accuracy/coverage gaps:** The prompt answer omits default round
  budgets and prompt-builder details; the cache answer cannot establish successful
  insertion/truncation and omits eviction/clearing; the call chain stops at
  `RigExecutor::new` before tool dispatch. These details were covered by the original
  balanced answers. The API list omits `graph_functions`, and the absent-symbol
  answer has one incorrect import line. Hotspot arithmetic checks out, but the
  answer omits the candidate-scope and incoming-edge-row qualifications.
- **Remaining latency:** The API command takes **587.5 seconds**. A **510.6-second**
  logged gap after its first completed search occurs before the next tool call,
  during the agent/model turn. The provider-side cause is unestablished. Eight
  successful responses do not establish that latency is resolved.
- **Comparison limits:** This run changes embeddings, chunk policy, binary, index
  contents, result limits and deadline. Earlier evaluation documents containing
  answers to these same questions were retrieved in cases 1–4. It is an operational
  smoke test with source review, not a held-out accuracy benchmark or a controlled
  measurement of tier effects.

The original full run `20261006_060847_378162` also selected `gpt-6-luna` explicitly.
Seven commands returned answers and case 2 (tier-aware prompts) hit the 300-second deadline;
the run took **648.4 seconds** including that timeout. The stored index reported
238 files, 34,494 nodes, 104,224 edges and 34,494 chunks, marked complete with
embedding and semantic stages ready, and includes dataflow, LSP, module and
documentation analyzer output. Embedding settings match the original balanced run.
[Full answers, run details and source review](docs/evaluations/full-cli-2026-10-06.md)
are retained separately.

After this evaluation, the default whole-agent CLI deadline and the shared CLI/HTTP
test-case deadlines increased from 300 to **600 seconds**. `--timeout-secs` still
overrides the CLI budget. The original full-tier table entry retains that 300-second
deadline; both complete refreshed runs use 600 seconds. The original recorded stall
occurred while awaiting a model response after graph calls returned.

Source review of the original full-tier answers found:

- **Configuration, cache, call chain and public API (cases 1, 3, 5, 7): source-aligned,
  with every checked location matching.** The configuration answer inventories the
  main loader, dotenv initialization, the agent's `[llm]` overlay, the MCP-core and
  advanced-config loaders, a JSON pipeline loader and direct environment readers;
  its list of callers of the main loader is incomplete. The cache answer covers hits,
  misses, post-truncation insertion, eviction and clearing, with one line range off
  by a function. The call-chain answer follows the AI-enabled workflow rather than
  the feature-disabled stub. The API answer lists all ten public methods and, unlike
  the original balanced answer, correctly describes the Rig adapters as indirect
  callers
  through [`CountingExecutor`](crates/codegraph-mcp-rig/src/tools/counting_executor.rs#L118).
- **Tier-aware prompts (case 2): no answer.** The command timed out in the run and in
  a single-case rerun. In both, the agent finished its tool calls within a minute and
  the remaining time was spent waiting for a model response. The cause was not
  established; the balanced run answered this case in 72 seconds.
- **Missing symbol (case 4): honest about absence, weak substitute.** The answer says
  `PromptSelector` does not exist, then analyzes
  [`prompt_selection`](crates/codegraph-mcp-server/src/project_init.rs#L103), the
  hook-selection prompt of `codegraph init`, which is unrelated to tier prompts. Its
  statements about that function are correct.
- **Architecture (case 6): accurate structure, no hub ranking.** Both hub queries
  failed with the same `array::concat()` 1,048,576-byte limit as in the balanced run,
  and the failure reproduces when the
  [function](schema/codegraph_v2.surql#L513) is called directly on this index. The
  answer discloses the gap and flags that instability is reported as `0.0` for nodes
  with outgoing dependencies. Direct calls for five sampled nodes all returned
  `0.0`: the expression (now [corrected](schema/codegraph_v2.surql#L470)) divided two integer counts,
  and integer division truncates the ratio.
  Coupling counts are unaffected, but stability conclusions cannot be drawn.
- **Complexity (case 8): consistent arithmetic, different ranking.** All 20 risk
  scores match complexity × (incoming dependency-edge count + 1). Parser `walk`
  functions now lead (Rust walker: risk 3,131 from complexity 31 and 100 incoming
  edges) and `reconcile_project` drops to eighth, because the full index has more
  than twice the edges and the query counts edge rows, not distinct callers.

The case 2 timeouts, the hub failures and the `0.0` instability values were traced to
defects that are now fixed: tool results were unbounded (one search could return 228 KB
and a run had no total limit), the hub query exceeded an `array::concat` size limit, and
the instability ratio divided integers. Re-running cases 2 and 6 on the same index with
the fixes returned `OK` in 79 and 106 seconds, with a hub ranking and non-zero
instability; the other cases were not re-run. See the
[follow-up section](docs/evaluations/full-cli-2026-10-06.md#follow-up-causes-found-and-fixed).

The refreshed full run `20261006_162918_871171` explicitly requested `gpt-6-luna`.
All eight cases returned `OK` in **901.8 seconds**, with **99 tool calls** and
**172 summed locations**. The catalog records 239 files, 34,998 nodes, 100,107 edges
and 36,600 chunks, including LSP definitions and dataflow output. It uses the same
installed binary, Jina v5 embedding/chunk policy and agent settings as refreshed
balanced. [Full refreshed review and all eight answers](docs/evaluations/full-cli-2026-10-06.md#refreshed-full-run-1629-utc)
remain alongside the original full run.

- **Verified improvements:** The prompt case returns in 82.9 seconds and now
  covers prompt composition, builder wiring and the 3/5/6/8 default round budgets.
  Hub ranking succeeds, and all eight displayed architecture instability ratios
  match Ce/(Ca+Ce). No graph-tool failures or Jina 422 errors are logged. The
  absent-symbol answer explicitly refuses to substitute the unrelated hook prompt.
- **Remaining accuracy/coverage gaps:** Cache insertion/eviction and post-truncation
  storage remain unestablished. The API list omits `graph_functions` and
  `clear_cache`. The call chain reaches graph dispatch but repeats a stale enum
  comment saying LATS cannot use tools; the current implementation does. That
  comment, environment example and installation guide were corrected after the
  run; the saved answer is unchanged and the index still contains the stale text.
- **Metric qualifications:** Coupling deduplicates neighboring graph nodes; hub
  degrees and hotspot risk count dependency-edge rows. Both include dataflow
  relationships rather than only calls. All twenty displayed risk calculations
  and declaration starts check out, but risk is a heuristic over a high-complexity
  candidate set, not a guaranteed global risk ranking or distinct-caller count.
- **Remaining latency:** Architecture takes **546.4 seconds**, including a
  **479.6-second** gap between completed search and the next tool call during an
  agent/model turn. The provider-side cause is unestablished. The API case returns
  in 69.4 seconds; latency has shifted between cases, not been resolved generally.
- **Comparison limits:** Checkout, graph contents and file count still differ,
  and model reasoning/cache warmth are uncontrolled. Prior evaluation documents
  were retrieved in cases 1–3, so this is not a held-out accuracy benchmark. This
  ReAct run uses a binary predating the LATS fix and does not live-validate LATS.

All seven completed original full-tier cases and all eight original balanced cases
logged typed-answer parse warnings; the server synthesized structured evidence from
tool traces instead. The runner accepted the resulting JSON responses. The refreshed
balanced and full runs also use synthesized highlights for prose answers; the
fallback now logs at `DEBUG`. Neither those fallbacks nor inner-tool errors are a
factual accuracy score. The initial balanced diagnostic run `20261006_041408_086869` is retained
separately because it did not explicitly select the intended request model.

The fast baseline is local run `20261006_013512_392882`. Its indexing tier comes from
the test session and its `gpt-6-luna` model label from the project owner; the runner
does not capture either setting automatically. During balanced evaluation, source review found that the Rig adapter
selected its model from `CODEGRAPH_LLM_MODEL`, then `CODEGRAPH_AGENT_MODEL`; at that time
it did not read the core configuration's `CODEGRAPH_MODEL`, so with only the latter set the
OpenAI Rig path defaulted to requesting `gpt-4o`. (The adapter has since been changed to
fall back to `CODEGRAPH_MODEL` and then `[llm] model`.) The fast run's saved responses do not
record the request model, so that label rests on the owner's confirmation.
Set `CODEGRAPH_LLM_MODEL` explicitly for reproducible agent comparisons. These labels
identify configured/requested models; the runner does not attest provider-side routing.

These observations do not isolate the effect of indexing tier from model reasoning
or establish a numerical accuracy rate. The runs also differ in checkout,
installed binary, file count, embedding/input policies, result limits and deadlines.
For subsequent comparisons, keep questions, model, agent context budget and inference
settings consistent; record the source revision and index configuration, and disclose
changes between runs. Review source locations, active conditional code, completeness
and unsupported conclusions before assigning accuracy scores.

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
│  │                    Agentic Tools Layer                    │  │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────────────┐  │  │
│  │  │  ReAct  │ │  LATS   │ │Reflexion│ │ Tool Execution  │  │  │
│  │  │  (Rig)  │ │  (Rig)  │ │  (Rig)  │ │    Pipeline     │  │  │
│  │  └────┬────┘ └────┬────┘ └────┬────┘ └────────┬────────┘  │  │
│  └───────┼───────────┼───────────┼───────────────┼───────────┘  │
│          └───────────┴───────────┴───────────────┘              │
│                              │                                  │
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

**Key insight:** The agentic tools don't just call one function. They *reason* about which graph operations to perform, chain them together, and synthesize results. A single `agentic_impact` call might:

1. Search for the target component semantically
2. Get its direct dependencies
3. Trace transitive dependencies
4. Check for circular dependencies
5. Calculate coupling metrics
6. Identify hub nodes that might be affected
7. Synthesize all findings into an actionable answer

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

### LLM (for agentic reasoning)
- **Local:** Ollama, LM Studio
- **Cloud:** Anthropic Claude, OpenAI, xAI Grok, OpenAI Compliant

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

The built-in agent resolves each LLM setting in three steps: the environment variable (a project `.env` counts), then the key in the config file's `[llm]` section, then a default. `enabled = false` in `[llm]` makes the agent ignore the section, and API keys are read from the environment only. `codegraph config agent-status` shows the provider, model and tier in effect; [AI_PROVIDERS.md](docs/AI_PROVIDERS.md) lists every variable and key.

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
