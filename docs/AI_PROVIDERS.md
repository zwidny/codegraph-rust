# CodeGraph AI providers: embeddings + reranking

This document explains how to configure CodeGraph’s embedding providers (for indexing/search)
and its optional reranking providers. CodeGraph ships no model of its own: the former `[llm]`
configuration and the built-in agent that consumed it were removed in v0.3.0, so nothing here
calls a language model. The 9 graph tools and `codegraph query` / `codegraph plan` query the
graph directly.

It is based on the runtime configuration loader in `crates/codegraph-core/src/config_manager.rs` and provider implementations in:

- `crates/codegraph-vector/src/*` (embeddings)

## Configuration sources and precedence

When CodeGraph starts, it loads configuration in this order:

1. `.env` in the current directory (if present)
2. `~/.codegraph.env` as a fallback for keys the project `.env` did not set (shared defaults
   belong here; both files layer, they do not replace each other)
3. A TOML config file:
   - `./.codegraph.toml` (project-local), else
   - `~/.codegraph/config.toml` (user-global)
4. Environment variable overrides (e.g. `CODEGRAPH_EMBEDDING_PROVIDER`, `OPENAI_API_KEY`)

If you use `.env`, it’s loaded automatically at startup (you do not need `direnv`).

## Minimal setup checklist

1. The project has been indexed. A shared SurrealDB server is the default target and needs the schema applied (see `docs/INSTALLATION_GUIDE.md`); an opt-in embedded store (`surrealkv://`) creates its schema automatically.
2. You have a `./.codegraph.toml` or `~/.codegraph/config.toml` with at least:
   - `[embedding] provider = ...`
3. Secrets are present via `.env` or your shell environment (recommended).

## `.env` examples

### Local (Ollama embeddings)

```bash
# SurrealDB shared server. Default target is ws://127.0.0.1:8000; a surrealkv:// URL opts into the slower embedded store
CODEGRAPH_SURREALDB_URL=ws://localhost:3004
CODEGRAPH_SURREALDB_NAMESPACE=ouroboros
CODEGRAPH_SURREALDB_DATABASE=codegraph
CODEGRAPH_SURREALDB_USERNAME=root
CODEGRAPH_SURREALDB_PASSWORD=root

# Embeddings
CODEGRAPH_EMBEDDING_PROVIDER=ollama
CODEGRAPH_OLLAMA_URL=http://localhost:11434
CODEGRAPH_EMBEDDING_MODEL=hf.co/nomic-ai/nomic-embed-code-GGUF:Q4_K_M
```

### Local (LM Studio embeddings)

```bash
CODEGRAPH_EMBEDDING_PROVIDER=lmstudio
CODEGRAPH_EMBEDDING_MODEL=jinaai/jina-embeddings-v3
```

Note: the LM Studio endpoint is `embedding.lmstudio_url` in TOML; the environment only
overrides it through `CODEGRAPH_LMSTUDIO_URL`.

### Remote (Jina embeddings)

```bash
CODEGRAPH_EMBEDDING_PROVIDER=jina
JINA_API_KEY=...
CODEGRAPH_EMBEDDING_MODEL=jina-embeddings-v4
JINA_API_BASE=https://api.jina.ai/v1
```

Notes:

- `.env` secrets should never be committed. Prefer `~/.codegraph.env` for global secrets.
- You can also put many non-secret defaults into the TOML config file and keep only keys in `.env`.

## TOML config file examples

Create either `./.codegraph.toml` (project-local) or `~/.codegraph/config.toml` (global).

### Example: Jina embeddings

```toml
[embedding]
provider = "jina"
model = "jina-embeddings-v4"
jina_api_base = "https://api.jina.ai/v1"
jina_task = "code.passage"
jina_late_chunking = true
dimension = 2048
batch_size = 64

```

`JINA_API_KEY` goes in the environment.

## Embedding providers (indexing + search)

The embedding provider is configured under `[embedding]` (or via env).

Valid values:

- `auto`
- `onnx`
- `ollama`
- `lmstudio` (sometimes spelled “mlstudio” colloquially, but the config value is `lmstudio`)
- `openai`
- `jina`

### `embedding.provider = "auto"`

“Auto” tries to find a reasonable local option:

- If an Ollama server is reachable, it prefers Ollama.
- Otherwise it tries to find an ONNX model in the local HuggingFace cache.

For reproducibility, prefer setting an explicit provider instead of `auto`.

### `embedding.provider = "ollama"` (local)

Requirements:

- Ollama running (default `http://localhost:11434`)
- An embedding model pulled into Ollama

Config inputs:

- `embedding.ollama_url` (or `CODEGRAPH_OLLAMA_URL`)
- `embedding.model` (or `CODEGRAPH_EMBEDDING_MODEL`)

### `embedding.provider = "lmstudio"` (local)

Requirements:

- LM Studio server running with an embedding model loaded
- The server exposes an OpenAI-compatible embeddings endpoint at:
  - `http://localhost:1234/v1/embeddings` (default)

Config inputs:

- `embedding.lmstudio_url` (TOML only; no env override)
- `embedding.model` (or `CODEGRAPH_LMSTUDIO_MODEL` / `CODEGRAPH_EMBEDDING_MODEL`)

### `embedding.provider = "jina"` (remote)

Requirements:

- `JINA_API_KEY` (or `embedding.jina_api_key`)

Config inputs:

- `embedding.model` (e.g. `jina-embeddings-v4` or `jina-embeddings-v5-text-small`)
- `embedding.jina_api_base` (default `https://api.jina.ai/v1`)
- `embedding.jina_task` (default `auto`: v4 uses `code.passage`, v3/v5 use `retrieval.passage`)
- `embedding.jina_late_chunking` (default is provider-dependent; set explicitly if you care)

Optional env tuning (provider-specific):

- `JINA_MAX_TOKENS`, `JINA_MAX_TEXTS`, `JINA_REQUEST_DELAY_MS`
- `JINA_API_TASK` overrides legacy `JINA_TASK`, then TOML `embedding.jina_task`.
  Explicit tasks are preserved. Search queries pair `code.passage` with `code.query`
  and `retrieval.passage` with `retrieval.query`; symmetric tasks keep their task.
- `JINA_LATE_CHUNKING`, `JINA_TRUNCATE`, `JINA_NORMALIZED` (normalization defaults to
  true for v3/v5/CLIP v2; the request omits it for v4)

For v5 text models, use `retrieval.query`, `retrieval.passage`, `text-matching`,
`classification` or `clustering`; `code.query` / `code.passage` belong to v4.
The v5 small model returns 1024 dimensions, and nano returns 768. See
[Jina's API model schemas](https://github.com/jina-ai/meta-prompt/blob/main/v12.txt).
Known unsupported task/model pairs fail during provider initialization. Embedding
HTTP 400/401/403/422 errors fail without retries and retain the response detail;
408/429, server failures and transport errors retain bounded retries.

```bash
CODEGRAPH_EMBEDDING_PROVIDER=jina
CODEGRAPH_EMBEDDING_MODEL=jina-embeddings-v5-text-small
CODEGRAPH_EMBEDDING_DIMENSION=1024
JINA_API_KEY=...
JINA_API_TASK=retrieval.passage
JINA_TRUNCATE=true
JINA_NORMALIZED=true
```

`JINA_API_TASK=retrieval.query` is also honored for indexing if explicitly selected.
Task, normalization, truncation and late-chunking changes invalidate cached Jina
vectors; reindex to replace vectors generated with a previous request policy.

### `embedding.provider = "openai"` (remote)

Requirements:

- `OPENAI_API_KEY` (or `embedding.openai_api_key`)

Config inputs:

- `embedding.model` (e.g. `text-embedding-3-small`)

### `embedding.provider = "onnx"` (local)

This uses a local ONNX embedding engine (no external service), typically pointing at a model directory.

Common env inputs:

- `CODEGRAPH_LOCAL_MODEL` (model directory / repo path)

### Chunking controls (provider-agnostic)

Chunking and batching can be tuned via environment variables:

- `CODEGRAPH_CHUNK_MAX_TOKENS`
- `CODEGRAPH_CHUNK_OVERLAP_TOKENS`
- `CODEGRAPH_CHUNK_SMART_SPLIT` (`true`/`false`)
- `CODEGRAPH_EMBEDDING_SKIP_CHUNKING` (`true`/`false`)
- `CODEGRAPH_EMBEDDING_BATCH_SIZE` (config loader) and `CODEGRAPH_EMBEDDINGS_BATCH_SIZE` (embedding engine)

If you change embedding dimensions/models, ensure your SurrealDB schema supports the chosen dimension fields.

## Providers after the agent removal

CodeGraph's shipped surface — indexing, the 9 MCP graph tools, `codegraph query` and
`codegraph plan` — never calls a language model. Provider configuration therefore exists
only for embeddings and reranking.

The `[llm]` section and the `CODEGRAPH_LLM_*` / `MCP_CODE_AGENT_MAX_OUTPUT_TOKENS`
variables were removed in v0.3.0 together with the built-in agent. Configuration files and
`.env` files may still carry them: unknown TOML sections are ignored silently (no
`deny_unknown_fields`) and unknown environment variables are simply never read, so an old
file keeps loading — delete the leftovers at your convenience.

`CODEGRAPH_CONTEXT_WINDOW` survives with a narrower meaning: it sizes the tool-result limit
handed back to the MCP client; it does not influence any model call.

## Graph schema selection (optional)

To use the experimental graph schema, set `CODEGRAPH_USE_GRAPH_SCHEMA=true` before the project's embedded store is created (or delete `<project>/.codegraph/db` and re-index).

With a SurrealDB server, also set `CODEGRAPH_GRAPH_DB_DATABASE=codegraph_experimental` (or your chosen database name) and apply `schema/codegraph_graph_experimental.surql` to that database yourself (see `docs/INSTALLATION_GUIDE.md`).
