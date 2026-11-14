# CodeGraph Usage Guide

## Mandatory usage policy — read this first

These rules apply to **every** agent session that has this MCP server connected,
regardless of which client or model drives it. They are not optional guidance.

**Trigger.** If the question touches this repository's source — structure, location,
relationship, call flow, or "what does X do" — the **first** tool call must be a
CodeGraph tool (direct graph tools first; `agentic_*` only for synthesis). This holds
for the whole task, not just the first turn: exploration, verification, debugging,
locating code before an edit. "I just want to confirm", "I roughly know where it is"
and "grep feels faster" are **not** exemptions.

**One criterion: symbols go to CodeGraph, text goes to grep.**

| In your hand | Use |
|---|---|
| function / type / module / trait / field / command variant name | `semantic_code_search` |
| string literal, log line, error message, config key, file path, verbatim comment | `grep` |

Answering "what is this project / where is the entry point / how is it structured"
with `ls`, `glob`, `cat Cargo.toml` or a hand-written grep is a violation — use
`get_hub_nodes`, `agentic_architecture` or `semantic_code_search` instead.

Knowing the name already is the reason to use CodeGraph, not the reason to grep.

**Fallback is allowed only when** (verify in this order, never decide from memory):

1. the project has no `.codegraph/db` — only then is it "not indexed";
2. or a CodeGraph call returns empty, errors, or visibly disagrees with the sources;
3. or what you hold is **text** (right column above), or you are confirming the
   contents of a **known exact file path**.

**Declare every fallback.** When `read` / `grep` / `ls` / `glob` is used to locate
source, the same reply must contain `fallback: <which rule above>`; batch them into
one line when possible. An unstated fallback is a violation even when the result is
correct. The presence of `.codegraph/` in an `ls` listing is itself evidence that the
project is indexed, and must not be used to justify a fallback.

**Per-turn self-check.** Did this turn's `read` / `grep` / `ls` / `glob` land on a
source location? If yes, which CodeGraph call in this turn preceded it? No answer
means the policy was violated.

---

## Direct graph tools (no LLM, 0.3–3s) — start here

Eight tools query the code graph directly. They are **20–100× faster** than the
`agentic_*` tools (which run a full LLM workflow, 55s–2.5min) and they return
raw structured data you can compose yourself. **Prefer these for concrete
questions; reach for `agentic_*` only when you need a synthesized explanation.**

A ninth direct tool, `plan_check`, validates a **draft** graph you are planning
(nodes/edges/constraints) before any code exists. Use it to externalise reasoning
and catch cycles, uncovered paths, layering violations and dangling references.

| Question | Tool | Required params |
|---|---|---|
| Where is this concept / find related code | `semantic_code_search` | — (`query`, `limit`, `threshold`) |
| Who calls / depends on this node | `get_reverse_dependencies` | `node_id`, `edge_type` |
| What does this node depend on (transitive) | `get_transitive_dependencies` | `node_id`, `edge_type` |
| Trace the call chain starting here | `trace_call_chain` | `node_id` |
| Coupling metrics for a node/module | `calculate_coupling_metrics` | `node_id` |
| Detect circular dependency cycles | `detect_circular_dependencies` | `edge_type` |
| Complexity hotspots | `find_complexity_hotspots` | — (`min_complexity`, `limit`) |
| High-degree hub nodes | `get_hub_nodes` | — (`min_degree`) |
| Validate a planned logic graph | `plan_check` | `plan` (object with `nodes`, `edges`, `constraints`) |

### Getting a `node_id`

You almost always start with a name, not an id:

```json
{"query": "chunk overlap tokens", "limit": 5}
```

`semantic_code_search` returns `result[i].node_id` shaped like
``nodes:`0f0f013b-df13-d632-1366-59ce8ed1c0b5` ``. Pass that **verbatim** to the
node tools — do not strip the backticks or the `nodes:` prefix, and do not
construct ids yourself.

### `edge_type` values

`calls` · `defines` · `uses` · `imports` · `extends` · `implements` ·
`contains` · `references`

For "who invokes this function" use `calls`. For "what pulls this module in"
use `imports`.

---

## Plan before a large change (`plan_check`)

`plan_check` validates a **draft logic graph** (nodes, edges, constraints) offline,
before the code exists. Use it to make implicit dependencies explicit: on a long,
multi-hop task the model does not reliably re-read earlier context, so the same
constraint can be violated several steps later.

**When** (any one of these):

- adding a module, crate-level feature or provider/factory path;
- a cross-layer change (e.g. `cli -> core -> vector`) or reversing that direction;
- a multi-hop dependency (A calls B calls C, or a new type implements a trait elsewhere);
- changing the public interface of 3+ files (signature, trait method, config key, schema).

Skip it for a single-file fix, a docs/text change or a mechanical rename inside one
module.

**How**:

1. Write the plan object: `nodes` = modules/types/traits you intend to add or touch
   (`id` should be the real symbol name you will write), `edges` = `calls` /
   `implements` / `uses`, `constraints` = `acyclic` + `layering` (order low -> high) +
   `coverage` (optionally `no_path`, `max_fan_in`, `max_fan_out`).
2. Call `plan_check` and fix `violations` until `ok: true`. `known_nodes` flags edges to
   undeclared nodes; `layering` / `acyclic` catch design errors.
3. After the code graph is indexed, compare the plan with reality:
   `codegraph plan --project <root> verify plan.json`. `unplanned_code_edges` means the
   code depends on something the plan forgot; `existing_planned_nodes` means you may be
   re-creating an existing symbol.
4. `codegraph plan --project <root> store plan.json --plan-id <id>` keeps the plan and
   its constraints for later sessions; `codegraph plan show <id>` reads it back.

## SCIP on a partial re-index

`codegraph index <scan_root>` updates only that scope. Compiler-grade enrichment comes from
the `CODEGRAPH_SCIP_INDEX` you pass; files the index does not cover keep AST-only results.
No language server is started for the scan.

```bash
CODEGRAPH_SCIP_INDEX=/abs/rust.scip codegraph index crates/codegraph-core \
  --project-root /path/to/repo --index-tier balanced
```

- Pass the **project root** (`--project-root`, or set `CODEGRAPH_PROJECT_ID`) so the index
  and the scan agree on the project. Keep the scan path relative to that root.
- The index is validated against the captured sources; regenerate it when sources change.
- `--index-tier` filters edges, it does not gate enrichment: `fast` drops
  `Uses`/`References`, `balanced` drops `References`, `full` keeps everything.
- For a large repository, run one full `codegraph index <root>` pass with a SCIP index, then
  use partial re-indexes for day-to-day edits.
- **Prefer SCIP over AST-only.** Generate a compiler index first and pass it via
  `CODEGRAPH_SCIP_INDEX` (`rust-analyzer scip .`, `scip-typescript index`, `scip-go`,
  `scip-python index`; comma-separated, `path@subdir` for an index built inside a
  subdirectory). Measured on a 10k-file tree: analysis 21 min -> 2.4 s + 3.9 s import.
  `scripts/scip-index.sh` generates the ones it knows and prints the value.
- **Any language with a SCIP indexer can be enriched, not only the four above.** CodeGraph
  parses Python natively, so a Python SCIP index contributes definition/reference edges like
  any other. Generate it with `scip-python index --project-name <name> --output <path>`
  from the project root and do **not** add `--cwd` — that flag makes scip-python emit a
  72-byte empty index. Check the artifact first (`wc -c <path>`, well above 1 KB); a
  72-byte file holds zero documents and the import aborts. A language with no index stays
  AST-only; there is no language-server fallback.

---

## Recommended workflows

### 1. Change safety check (before editing)

```
1. semantic_code_search("the symbol or concept")     → node_id, file:line
2. get_reverse_dependencies(node_id, "calls", 1)     → who breaks if I change it
3. Read the files listed above                       → confirm current code
4. Edit
```

### 2. Understand a flow (debugging)

```
1. semantic_code_search("symptom or module")         → entry node_id
2. trace_call_chain(node_id, 3)                      → the call path
3. get_transitive_dependencies(node_id, "calls", 2)  → what it reaches
4. Read the specific files identified
```

### 3. Architecture survey

```
1. get_hub_nodes(30)                                 → central modules
2. detect_circular_dependencies("imports")           → layering problems
3. find_complexity_hotspots(10, 20)                  → risky areas
4. Only now, if you need a narrative: agentic_architecture({"query": "..."})
```

### 4. When to use `agentic_*` instead

Use the agentic tools when the question needs **synthesis across evidence** and
you would otherwise chain 4+ graph calls yourself:

- "How does provider selection work end to end?"
- "What are the architectural risks in the indexing pipeline?"
- "Gather everything I need to implement feature X"

Cost: 55s–2.5min per call. Never use them for a single symbol lookup — that is
what the direct tools are for.

---

## Tool Selection (agentic, slower)

## When to Use CodeGraph Tools

Use CodeGraph tools as your **first choice** for understanding code. They return synthesized, relevant information instead of raw file contents, saving context for actual work.

### Tool Selection

| Question | Tool | What You Get |
|----------|------|--------------|
| "What do I need to proceed?" | `agentic_context` | Client-readable context bundle: summary, analysis, highlights (with file:line + snippets), related locations, risks, next steps, confidence |
| "If I change X, what breaks?" | `agentic_impact` | Client-readable impact bundle: summary, analysis, impact highlights, affected locations, risks, next steps, confidence |
| "How is this area structured?" | `agentic_architecture` | Client-readable architecture bundle: summary, analysis, structural highlights, related locations, risks, next steps, confidence |
| "Where are the risks?" | `agentic_quality` | Client-readable quality bundle: summary, analysis, hotspot highlights, risk notes, related locations, next steps, confidence |

### Example Queries

```json
{"query": "gather context for adding rate limiting"}
{"query": "if I change the indexing pipeline, what breaks?"}
{"query": "how is the MCP server structured?"}
{"query": "hotspots and coupling risks in the parser"}
```

---

## Focus Parameter for Precision

Each tool accepts an optional `focus` parameter to narrow analysis to a specific mode:

| Tool | Focus Values | Default Behavior |
|------|-------------|-----------------|
| `agentic_context` | `"search"`, `"builder"`, `"question"` | Auto-selects based on query |
| `agentic_impact` | `"dependencies"`, `"call_chain"` | Analyzes both dependency chains and call flows |
| `agentic_architecture` | `"structure"`, `"api_surface"` | Provides both structural and interface analysis |
| `agentic_quality` | `"complexity"`, `"coupling"`, `"hotspots"` | Comprehensive risk assessment |

Example with focus:
```json
{"query": "trace login flow", "focus": "call_chain"}
{"query": "what public interfaces exist", "focus": "api_surface"}
```

---

## When to Fall Back to File Reading

CodeGraph tools work best for **understanding and exploration**. Use regular file tools when:

### 1. You Need Exact File Contents for Editing

After CodeGraph tells you which file to modify, read that specific file to make edits:

```
1. agentic_context({"query": "where is the login handler?"})
   → Returns: src/auth/handler.rs:45-67
2. Read src/auth/handler.rs to make your edit
```

### 2. CodeGraph Results Are Insufficient

If a CodeGraph tool returns:
- "No relevant code found"
- Results that don't answer your question
- Errors or empty responses

Then fall back to targeted file reading based on what you know.

### 3. You Need Very Recent Changes

If the codebase was modified after the last changes, CodeGraph may not reflect those changes instantly. Use file tools for:
- Files you just created
- Code you modified during your last phase
- Uncommitted changes

### 4. Simple, Known Locations

For quick lookups where you already know the exact file:
- Reading a config file you know exists
- Checking a specific line number from an error
- Viewing a file path from a stack trace

---

## How to Use CodeGraph Results

### Results Include Locations

Every result includes file paths and line numbers. Use these to:
- Navigate to the code
- Read specific files for editing
- Verify the analysis matches current code

### Results Are Synthesized

Results explain relationships and patterns, not just raw code. Use this to:
- Understand how components connect
- Plan changes with full context
- Identify all affected areas before refactoring

### Verify Before Major Changes

For significant modifications:
1. Use CodeGraph to understand scope
2. Read the actual files to confirm
3. Make your changes
4. Use `agentic_impact` to check impact

---

## Recommended Workflows

### Implementing a Feature

```
1. agentic_architecture({"query": "how is [similar feature] structured?"})
2. agentic_context({"query": "context for implementing [feature]"})
3. Read specific files identified → make changes
```

### Debugging

```
1. agentic_context({"query": "[error message or symptom]"})
2. agentic_impact({"query": "what depends on [suspected area]?"})
3. Read identified files → fix issue
```

### Understanding a Codebase

```
1. agentic_architecture({"query": "overall structure"})
2. agentic_context({"query": "main interfaces and entry points"})
```

### Refactoring

```
1. agentic_impact({"query": "what depends on [target]?"})
2. agentic_quality({"query": "risk hotspots around [target]"})
3. Plan changes based on full impact understanding
4. Read and modify files systematically
```

---

## Quick Reference

**Start with CodeGraph when:**
- Exploring unfamiliar code
- Understanding relationships
- Planning changes
- Answering "how" or "why" questions

**Use file tools when:**
- Making actual edits
- CodeGraph didn't find what you need
- Checking very recent changes
- Reading known, specific files

**Always:**
- Read CodeGraph results fully before requesting more
- Use the file locations provided in results
- Verify critical information before major changes

---

## Project-Only Indexing Policy

CodeGraph is project-scoped: it indexes the repository workspace, not third-party dependency source trees (e.g. downloaded crates, `node_modules/`, `site-packages/`).

When a question is about third-party library behavior, prefer library documentation or API references rather than expecting CodeGraph to show vendored source.
