# Debug Logging for MCP Tool Calls

CodeGraph can record every MCP graph-tool call (input parameters, results and
errors) to a JSONL file. It is off by default and has no effect on the indexed
graph or on query results.

There is no server-side agent any more (removed in v0.3.0), so the log contains
only tool-call events: no reasoning steps and no agent start/finish records.

## Quick Start

### 1. Enable debug logging

The logger is initialized when the CLI starts (before the MCP server runs), so
set the environment variable before launching the server:

```bash
export CODEGRAPH_DEBUG=1
codegraph start stdio
```

`CODEGRAPH_DEBUG` accepts `1`, `true` or `TRUE` (the value is compared
case-insensitively for `true`). Any other value, or leaving it unset, keeps the
logger disabled.

Or add it to your `.env` file:

```bash
CODEGRAPH_DEBUG=1
```

### 2. Call a tool

Once the server is running, each tool invocation appends one or more lines to
the current log file:

```bash
codegraph query <symbol>
```

Note that only calls that go through the MCP tool executor are logged; a CLI
command that answers without invoking a tool writes nothing.

### 3. Read the log

The log is plain JSONL, so `jq` (see [Raw log analysis](#raw-log-analysis)) or
any JSON line reader can inspect it. There is no bundled viewer script.

## What Gets Logged

Three event types are written, one JSON object per line:

1. **`tool_call_start`** — tool name and the parameters passed to it.
2. **`tool_call_finish`** — tool name, the full result, and a summarized result
   (`result_summary`).
3. **`tool_call_error`** — tool name, parameters and the error message.

`result_summary` is derived from the result shape:

- array → `{"type": "array", "count": N, "sample": <first element or null>}`
- object → `{"type": "object", "keys": [...]}`
- anything else → the value itself

## Log File Location

Each process that starts with logging enabled creates one timestamped file:

```
<debug dir>/agentic_debug_YYYYMMDD_HHMMSS.jsonl
```

The `agentic_debug_` prefix is a historical name kept for compatibility; it does
not imply agent output.

- Default debug directory: `.codegraph/debug/` under the process's current
  working directory.
- The timestamp uses UTC (`YYYYMMDD_HHMMSS`).

## Configuration

### Custom debug directory

```bash
export CODEGRAPH_DEBUG_DIR=/path/to/custom/debug/dir
```

If `CODEGRAPH_DEBUG_DIR` is unset, the default `.codegraph/debug/` directory is
used. The directory is created on startup when logging is enabled.

## Log Format

Each log entry is a JSON object on a single line (JSONL format).

Tool call start:

```json
{
  "timestamp": "2025-01-01T12:00:00.123456Z",
  "event": "tool_call_start",
  "tool": "semantic_code_search",
  "parameters": {
    "query": "configuration loading"
  }
}
```

Tool call finish:

```json
{
  "timestamp": "2025-01-01T12:00:05.678910Z",
  "event": "tool_call_finish",
  "tool": "semantic_code_search",
  "result": {
    "tool": "semantic_code_search",
    "result": []
  },
  "result_summary": {
    "type": "array",
    "count": 0,
    "sample": null
  }
}
```

Tool call error:

```json
{
  "timestamp": "2025-01-01T12:00:05.678910Z",
  "event": "tool_call_error",
  "tool": "get_reverse_dependencies",
  "parameters": {
    "node_id": "nodes:...",
    "edge_type": "calls"
  },
  "error": "..."
}
```

## Common Debugging Scenarios

### Tool returns empty results

Filter `tool_call_finish` events where `result_summary.count` is `0`:

```bash
jq 'select(.event == "tool_call_finish" and .result_summary.count == 0) | .tool' \
  .codegraph/debug/agentic_debug_*.jsonl
```

**Possible causes:**

- `project_id` mismatch between server and database
- No embeddings in database
- Query threshold too high
- Full-text search indexes missing

### Tool parameters wrong

Inspect `tool_call_start` events for the exact parameters that reached the
executor:

```bash
jq 'select(.event == "tool_call_start") | {tool, parameters}' \
  .codegraph/debug/agentic_debug_*.jsonl
```

**Check:**

- Are parameters being passed correctly?
- Is `project_id` being inferred from PWD?
- Are embeddings generated with the correct dimension?

## Raw Log Analysis

For advanced debugging, parse the JSONL directly:

```bash
# All tool results
jq 'select(.event == "tool_call_finish")' .codegraph/debug/agentic_debug_*.jsonl

# Count empty results
jq 'select(.event == "tool_call_finish" and .result_summary.count == 0) | .tool' \
  .codegraph/debug/agentic_debug_*.jsonl

# All errors
jq 'select(.event == "tool_call_error") | {tool, error}' \
  .codegraph/debug/agentic_debug_*.jsonl
```

## Performance Impact

Debug logging is cheap and inert when disabled:

- Logging only occurs when `CODEGRAPH_DEBUG` is enabled at startup.
- Each event is written and flushed immediately, so the file is always
  readable while the server runs.
- Entries are serialized JSON lines; no background task or network access is
  involved.

## Troubleshooting

### No debug logs generated

1. Check `CODEGRAPH_DEBUG=1` (or `true`) was set **before** starting the process.
2. Check startup output for:
   ```
   🐛 Debug logging enabled: .codegraph/debug/agentic_debug_20250101_120000.jsonl
   ```
3. Check the directory is writable:
   ```bash
   ls -ld .codegraph/debug/
   ```

### Logs not updating

1. Ensure the server is actually running.
2. Confirm the tool calls under test reach the MCP server (a CLI path that does
   not invoke a tool writes nothing).
3. Tail the active file:
   ```bash
   tail -f .codegraph/debug/agentic_debug_*.jsonl
   ```

### Parse errors

1. Check the log file is not corrupted.
2. Stop the server before copying the file.
3. Find the malformed line with `jq`:
   ```bash
   jq -c . .codegraph/debug/agentic_debug_*.jsonl
   ```

## See Also

- [CLAUDE.md](../CLAUDE.md) - MCP server configuration
- [crates/codegraph-mcp-core/src/debug_logger.rs](../crates/codegraph-mcp-core/src/debug_logger.rs) - Debug logger implementation
