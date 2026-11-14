# CodeGraph Usage Guide

How to get the most out of CodeGraph when working with AI coding assistants.

---

## Current tool set

The MCP server exposes **9 tools**: eight direct graph queries plus `plan_check`.
They run without an LLM and return structured evidence in milliseconds.

| Tool | Answers |
|---|---|
| `semantic_code_search` | Where is this concept? Returns the `node_id` every other tool needs |
| `get_reverse_dependencies` | Who calls/depends on this |
| `get_transitive_dependencies` | Transitive dependency closure |
| `trace_call_chain` | Call path from a node |
| `calculate_coupling_metrics` | Coupling for a node/module |
| `detect_circular_dependencies` | Dependency cycles |
| `find_complexity_hotspots` | Complexity hotspots |
| `get_hub_nodes` | High-degree hubs |
| `plan_check` | Validate a draft nodes/edges/constraints graph offline |

The MCP server also exposes the prompt `codegraph:initial_instructions`.

`node_id` must be used verbatim from a prior search result (`nodes:<uuid>`). Filtering is by
`threshold` on `vector_score`; the default is 0.4, lower it to 0.2–0.3 for natural-language
recall.

For new-machine setup and OpenCode MCP wiring, see [OPENCODE_QUICKSTART](OPENCODE_QUICKSTART.md).

---

## CLI access for code agents

The same graph is available from the shell, without an MCP server:

```bash
codegraph query <symbol>              # locate a definition
codegraph query <symbol> --callers    # who calls it
codegraph query <symbol> --callees    # what it calls
codegraph query <symbol> --kind function --file src/auth   # narrow the match
codegraph plan check plan.json        # offline draft-graph validation
codegraph plan verify plan.json       # compare with the indexed code graph
codegraph plan store plan.json --plan-id <id>
codegraph index . --scip-auto         # re-index with compiler enrichment
codegraph status --detailed           # server, provider and database state
```

`codegraph init /path/to/project` offers project-local Claude/Codex hooks, adds
CodeGraph-first guidance to both `AGENTS.md` and `CLAUDE.md`, then indexes recursively.
Use `--hooks claude|codex|both|none` for an explicit choice and `--no-index` for setup only.
Use `codegraph hooks install` for opt-in project-local Claude Code/Codex hooks.

There is no server-side LLM workflow or agent subcommand. For an
"architecture" or "impact" style answer, compose `query` calls yourself and read the
returned `file:line` locations.

## Getting Started with Your AI Assistant

### Step 1: Initialize the Session

When you start a new conversation with your AI assistant (Claude Code, Cursor, etc.), have
it load CodeGraph's MCP prompt:

```
Read the codegraph:initial_instructions prompt
```

Or simply ask:

```
"Read the codegraph instructions so you know how to use it"
```

This loads guidance on when and how to use each direct tool. **Do this once per session.**

### Step 2: Verify Connection

Ask your assistant to check that CodeGraph is working:

```
"Can you verify codegraph is connected and the index is available?"
```

The assistant should be able to call a simple tool like `semantic_code_search` to confirm.

---

## The Graph Tools

The nine tools are composable building blocks. Here is when to use each.

### `semantic_code_search`
**Use when:** Finding code, exploring unfamiliar areas, discovering patterns

**Good prompts:**
- "Find where user authentication is handled"
- "Show me how error handling works in this codebase"
- "Find all API endpoints"
- "Where is the database connection configured?"

**What it does:** Hybrid semantic search over the graph; every result carries a `node_id`,
file and line.

---

### `get_reverse_dependencies`
**Use when:** Before refactoring, understanding impact, checking coupling

**Good prompts:**
- "What depends on the UserService class?"
- "What would break if I change the auth module?"
- "Who calls processOrder()?"

**What it does:** Returns incoming edges (default `calls`) for a `node_id`. Chain it on the
returned nodes for a wider blast radius.

---

### `get_transitive_dependencies`
**Use when:** You need the full dependency closure, not just direct callers/callees

**Good prompts:**
- "Show me the dependency tree for the payment system"
- "How coupled is the API layer to the database?"

**What it does:** Follows the chosen `edge_type` transitively from a `node_id`.

---

### `trace_call_chain`
**Use when:** Tracing execution flow, debugging, understanding data paths

**Good prompts:**
- "Trace the execution from HTTP request to database in the order flow"
- "How does data flow from the API to the cache?"
- "What's the call chain when a user logs in?"

**What it does:** Traces call paths from a node with a configurable depth.

---

### `get_hub_nodes`
**Use when:** Onboarding, architecture reviews, understanding the big picture

**Good prompts:**
- "What are the main components of this project?"
- "Which modules have the most connections?"

**What it does:** Ranks high-degree nodes; combine with
`detect_circular_dependencies` for layering problems.

---

### `detect_circular_dependencies`
**Use when:** Checking layering or import cycles

**Good prompts:**
- "Are there circular imports between these crates?"
- "Does the API layer depend back on the database layer?"

**What it does:** Reports dependency cycles for the chosen `edge_type`.

---

### `find_complexity_hotspots`
**Use when:** Refactoring prioritisation, code health reviews

**Good prompts:**
- "Which functions are the riskiest to change?"
- "Where should we start refactoring?"

**What it does:** Ranks nodes by cyclomatic complexity and incoming-edge risk.

---

### `calculate_coupling_metrics`
**Use when:** API design review, breaking-change assessment, module boundaries

**Good prompts:**
- "What public APIs does the auth module expose?"
- "How coupled is this module to the rest of the workspace?"
- "Would changing this function signature break anything?"

**What it does:** Returns afferent/efferent coupling and instability for a node.

---

### `plan_check`
**Use when:** Designing a change, investigating a multi-module behaviour, or writing code
that crosses layers

**What it does:** Validates a draft `{nodes, edges, constraints}` graph offline — no
database, no LLM — and reports undeclared references, layering violations and cycles.
`codegraph plan verify` then compares the plan with the indexed graph.

---

## Prompt Patterns for Maximum Efficiency

### Be Specific About What You Need

**Less effective:**
```
"Tell me about the auth system"
```

**More effective:**
```
"I need to add OAuth support. Use codegraph to:
1. Find how authentication currently works
2. Identify what I'd need to modify
3. Check for any existing OAuth-related code"
```

### Chain Tools for Complex Tasks

For complex work, guide your assistant to use multiple tools:

```
"I'm going to refactor the UserService. Please:
1. Use semantic_code_search to find it and get its node_id
2. Use get_reverse_dependencies to see what depends on it
3. Use get_transitive_dependencies for the wider impact
4. Read the named files and summarise what I need to change"
```

### Ask for the Right Tool

If your assistant is grepping files manually, redirect it:

```
"Instead of reading files one by one, use codegraph's semantic_code_search
to find what you're looking for"
```

Or:

```
"Use get_reverse_dependencies instead of manually tracing imports"
```

### Reference Previous Results

CodeGraph results can inform follow-up questions:

```
"Based on that dependency list, which of those dependent components
would be most affected by changing the interface?"
```

---

## Working Patterns

### Pattern 1: Exploration First

When starting work on an unfamiliar area:

1. **Architecture overview:** "Use get_hub_nodes to show the central modules"
2. **Find entry points:** "Where does payment processing start?" (`semantic_code_search`)
3. **Trace the flow:** "Trace the execution path for a successful payment"
   (`trace_call_chain`)
4. **Check dependencies:** "What does the payment module depend on?"
   (`get_transitive_dependencies`)

### Pattern 2: Pre-Refactoring

Before making changes:

1. **Impact analysis:** "What would be affected if I change the OrderProcessor?"
   (`get_reverse_dependencies`)
2. **Dependency map:** "Show me everything that depends on this class"
   (`get_transitive_dependencies`)
3. **Find consumers:** "Who calls processOrder()?" (`get_reverse_dependencies` with `calls`)
4. **Risk check:** "Which of these are complexity hotspots?" (`find_complexity_hotspots`)

### Pattern 3: Feature Implementation

When adding new features:

1. **Find patterns:** "How are similar features implemented in this codebase?"
   (`semantic_code_search`)
2. **Model the change:** write a plan and run `plan_check`
3. **Check conventions:** "What conventions should I follow based on existing code?"
4. **Verify integration:** "Where should this new feature integrate?"
   (`get_reverse_dependencies` / `plan verify`)

### Pattern 4: Debugging

When tracking down issues:

1. **Find the code:** "Where is [symptom] likely originating?" (`semantic_code_search`)
2. **Trace execution:** "Trace the call chain that leads to this error"
   (`trace_call_chain`)
3. **Check dependencies:** "What could be affecting this behavior?"
   (`get_reverse_dependencies`)
4. **Find similar patterns:** "Are there similar patterns elsewhere that work correctly?"

---

## Tips for Best Results

### 1. Let CodeGraph Do the Heavy Lifting

Don't let your AI assistant manually grep through files when CodeGraph can search the
symbol graph. If you see it reading files one by one, say:

```
"Use codegraph for this instead of reading files manually"
```

### 2. Read the Evidence

CodeGraph returns graph evidence with `file:line` locations, not a pre-written answer.
Read the files it names and draw the conclusion yourself; do not present a tool result as
if it were reasoning you did not do.

### 3. Use Natural Language

You don't need to know the exact tool names. These all work:

- "Find where X is implemented" → `semantic_code_search`
- "What depends on X?" → `get_reverse_dependencies`
- "How does X flow through the system?" → `trace_call_chain`
- "Give me an overview of X" → `get_hub_nodes` + `detect_circular_dependencies`

### 4. Combine with Your AI's Reasoning

CodeGraph provides context; your AI provides reasoning. A good workflow:

1. Gather evidence with CodeGraph
2. Let your AI analyze and propose solutions
3. Verify proposals with `get_reverse_dependencies`
4. Implement with confidence

### 5. Keep the Index Fresh

If you're making changes, ensure CodeGraph is running with `--watch`:

```bash
codegraph start stdio --watch
```

This keeps the index current as you edit files.

For manual updates, prefer the narrowest path that changed:

```bash
codegraph index packages/core/src/tool.ts   # one file
codegraph index packages/core               # one directory
```

A partial re-index keeps the rest of the project graph intact and reuses the
project's existing symbols to resolve edges. See
[Incremental indexing](INCREMENTAL_INDEXING.md) for project-root resolution,
scope rules and limits.

---

## Troubleshooting

### "CodeGraph isn't finding my recent changes"

The index might be stale. Either:
- Restart with `--watch` flag
- Manually re-index: `codegraph index . -r -l <languages>`

### "The AI keeps reading files manually instead of using CodeGraph"

Explicitly redirect:
```
"Stop reading files manually. Use codegraph's semantic_code_search tool instead."
```

### "Results seem incomplete"

Try being more specific, or use a different tool:
- For broad discovery → `semantic_code_search`
- For relationships → `get_reverse_dependencies`
- For execution paths → `trace_call_chain`

### "The AI doesn't know about CodeGraph tools"

Have it read the instructions:
```
"Read the codegraph:initial_instructions prompt"
```

---

## Quick Reference

| I want to... | Use... |
|--------------|--------|
| Find code | `semantic_code_search` |
| See what depends on X | `get_reverse_dependencies` |
| See the full dependency closure | `get_transitive_dependencies` |
| Trace execution flow | `trace_call_chain` |
| Understand architecture | `get_hub_nodes` + `detect_circular_dependencies` |
| See coupling / public surface impact | `calculate_coupling_metrics` |
| Find risky code | `find_complexity_hotspots` |
| Validate a planned change | `plan_check` |
| Load tool instructions | `codegraph:initial_instructions` prompt |
