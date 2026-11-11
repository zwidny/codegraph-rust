# Indexing is not deterministic when LSP enrichment is enabled

Status: open issue, pre-existing (not introduced by the `fn::*` → Rust migration).

## Symptom

Three `codegraph index` runs over the same unchanged EasyTier tree produce different
edge counts. Nodes and chunks are identical every time.

| Field | run A (09:23) | run B (09:37, `--force`) | run C (10:48, `--force`) |
|---|---|---|---|
| nodes | 38057 | 38057 | 38057 |
| chunks | 38080 | 38080 | 38080 |
| **edges (stats)** | 72939 | 72866 | 72892 |
| **edges (DB)** | 72940 | 72940 | 72996 |
| **resolved_edges** | 73858 | 73785 | 73811 |
| **lsp_nodes_enriched** | 5935 | 7383 | 5470 |
| errors | 0 | 0 | 0 |

Runs A and B used the same binary, so the drift is not caused by code changes.

## Causal chain

```
language server readiness differs between runs
      (kotlin-language-server initialize: 89.1s vs 21.5s)
  → fewer/more documentSymbol results        (lsp_nodes_enriched 5935 / 7383 / 5470)
  → some nodes get metadata.attributes["qualified_name"], others do not
      (crates/codegraph-mcp/src/analyzers/lsp.rs:1015)
  → the resolution catalog is built from qualified_name
      (crates/codegraph-mcp/src/resolution.rs:47 and :117)
  → name-based edge resolution differs       (resolved_edges ±26)
  → persisted edge count differs             (edges ±26, ~0.04%)
```

The enrichment stage itself is not waiting for the server to finish indexing:
`crates/codegraph-mcp/src/analyzers/lsp.rs:495` answers
`window/workDoneProgress/create` with `null` and never tracks the matching
`$/progress` end, so `documentSymbol` requests are issued as soon as `initialize`
returns (`lsp.rs:574-578`).

## Why it matters

`AGENTS.md` requires deterministic analyzer output. Two consequences today:

1. Re-indexing the same commit can change graph contents, which makes
   "did my code change affect the graph?" a noisy question.
2. Differential tests and any before/after comparison must tolerate ~0.1% edge drift
   or pin the tier.

## Measurement: does LSP `qualified_name` help edge resolution?

`CODEGRAPH_LSP_QUALIFIED_NAMES=0` skips only the `qualified_name` metadata write
(`lsp.rs`, `apply_symbol_metadata`); enrichment counts, definitions and every other
analyzer stay untouched. With the variable unset (or any value other than `0` /
`false`) behaviour is unchanged.

Method: EasyTier at `f07d8d1`, `--index-tier balanced --force`, fresh isolated
database per run (`easytier_lspnames`, v2 schema re-applied each time),
`CODEGRAPH_EMBEDDING_POLICY=off CODEGRAPH_SEMANTIC_RESOLUTION=off`.
`CODEGRAPH_NODE_DB_BATCH_SIZE=64` was used in **both** arms because the default 512
hit SurrealKV write-write conflicts on a fresh database; it changes write batching
only, not graph contents. Two independent A/B pairs were run.

| run | `CODEGRAPH_LSP_QUALIFIED_NAMES` | nodes | edges | resolved_edges | unresolved_edges | resolution_rate | lsp_nodes_enriched | index_ms |
|---|---|---|---|---|---|---|---|---|
| A  | unset | 38057 | 72953 | 73872 | 100979 | 42.249% | 3801 | 82246 |
| B  | `0`   | 38057 | 73117 | 74037 | 100814 | 42.343% | 3300 | 85386 |
| A2 | unset | 38057 | 72933 | 73849 | 101002 | 42.235% | 5360 | 77435 |
| B2 | `0`   | 38057 | 73137 | 74057 | 100794 | 42.354% | 9536 | 80151 |

Same-switch replicate drift: A vs A2 = 23 resolved edges (−0.014pp), B vs B2 = 20
(+0.011pp). Switching the variable changes resolved edges by +165 and +208
(+0.094pp and +0.119pp) — i.e. *fewer* names slightly *raise* resolution, probably
because extra aliases add ambiguity that leaves candidates unresolved. The effect is
an order of magnitude below the 1pp materiality bar and points the wrong way for the
"LSP names improve resolution" hypothesis; `lsp_nodes_enriched` itself swung between
3300 and 9536 across runs, confirming that readiness noise dominates.

Conclusion: unless qualified names are shown to improve *edge correctness* (not
measured here), option 3 is the right trade: dropping `qualified_name` from the
resolution catalog costs ~0.1pp of resolution rate while removing the only
LSP-dependent input to edge identity.

## Options

### 1. Document `--index-tier fast` as the reproducible path (cheap, immediate)

`fast` skips language servers entirely, so the AST-only graph is deterministic.
No code change; documents the trade-off (no LSP enrichment).

### 2. Wait for LSP progress before requesting symbols (correct, medium cost)

Track `window/workDoneProgress/create` tokens and wait for the matching
`$/progress` `end` before the first `documentSymbol`, bounded by the existing
initialize timeout (`CODEGRAPH_LSP_INITIALIZE_TIMEOUT_SECS`, default 180s).

- Pros: keeps enrichment quality and makes it reproducible.
- Cons: needs a per-server readiness signal. rust-analyzer/gopls/kotlin emit
  `$/progress`, but not all servers (or all versions) do; when absent the wait
  degrades to a timeout, which adds latency without guaranteeing determinism.
- Cost: message-loop change plus mock-server tests.

### 3. Stop feeding LSP data into edge resolution (decisive, quality trade-off)

Keep enrichment for display only, or drop `qualified_name` from the resolution
catalog so parsing stays AST-pure. Determinism becomes structural rather than
timing-dependent.

- Pros: fully deterministic regardless of server behaviour.
- Cons: loses the more accurate qualified names LSP supplies, which likely reduces
  `resolved_edges` quality. Needs a measured comparison before adopting.

## Recommendation

1. Now: document option 1 (`fast` = reproducible) and keep `balanced` as the
   quality-first default.
2. Next: quantify option 3 — run `fast` vs `balanced` and compare `resolved_edges`
   quality. If LSP's extra names do not meaningfully raise resolution, adopt option
   3 (decisive and cheaper than option 2).
3. Only if both fail: implement option 2.

## Reproducing

```bash
# same tree, twice
codegraph index . --index-tier balanced --force --stats-json /tmp/a.json
codegraph index . --index-tier balanced --force --stats-json /tmp/b.json
jq '{nodes,edges,resolved_edges,lsp_nodes_enriched}' /tmp/a.json /tmp/b.json
# lsp_nodes_enriched and edges will differ
```
