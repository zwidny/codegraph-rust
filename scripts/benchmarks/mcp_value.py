#!/usr/bin/env python3
"""Measure what CodeGraph's MCP tools buy over plain grep when an agent must locate code.

Two arms answer the same questions about an indexed project:

  grep      - iterative ripgrep, i.e. the baseline an agent has without CodeGraph
  codegraph - one `semantic_code_search` call over MCP stdio

Per arm the report carries hit@1, hit@5, rounds/calls, the bytes an agent would
have to read, and latency. Tasks are generated from the live index so a run can be
reproduced after re-indexing; pass `--tasks` to replay a frozen set.

    python3 scripts/benchmarks/mcp_value.py --generate 20 --markdown out.md
    python3 scripts/benchmarks/mcp_value.py --tasks tasks.json
"""
import argparse
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

DEFAULT_COMMAND = ["codegraph", "start", "stdio"]
WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]{3,}")
STOP = {
    "that", "this", "with", "from", "into", "when", "then", "than", "them", "they",
    "have", "has", "been", "were", "will", "would", "should", "could", "about",
    "which", "while", "where", "there", "their", "these", "those", "also", "only",
    "does", "each", "such", "same", "more", "most", "some", "because", "after",
    "before", "under", "over", "through", "without", "against", "between", "using",
    "used", "uses", "call", "calls", "called", "code", "file", "files", "line",
}


# ---------------------------------------------------------------- pure helpers

def normalize_path(path, root):
    """Return a project-relative posix path so both arms compare equal."""
    raw = str(path).replace("\\", "/")
    base = str(root).replace("\\", "/").rstrip("/")
    if raw.startswith(base + "/"):
        raw = raw[len(base) + 1:]
    while raw.startswith("./"):
        raw = raw[2:]
    return raw


def extract_keywords(text, limit=4):
    """Pick the longest content words an agent would plausibly grep for."""
    seen, picked = set(), []
    for word in WORD.findall(text or ""):
        low = word.lower()
        if low in STOP or low in seen:
            continue
        seen.add(low)
        picked.append(word)
    picked.sort(key=lambda w: (-len(w), w))
    return picked[:limit]


def hit_rank(candidates, truth):
    """1-based rank of the ground-truth file in a candidate list, or 0 for a miss."""
    for index, candidate in enumerate(candidates, start=1):
        if candidate == truth:
            return index
    return 0


def summarize(per_task, arm):
    rows = [t[arm] for t in per_task if t.get(arm)]
    if not rows:
        return {"tasks": 0}
    total = len(rows)
    return {
        "tasks": total,
        "hit@1": sum(1 for r in rows if r["rank"] == 1) / total,
        "hit@5": sum(1 for r in rows if 1 <= r["rank"] <= 5) / total,
        "miss": sum(1 for r in rows if r["rank"] == 0) / total,
        "rounds": sum(r["rounds"] for r in rows) / total,
        "bytes": sum(r["bytes"] for r in rows) / total,
        "ms": sum(r["ms"] for r in rows) / total,
    }


def render_markdown(report):
    def pct(value):
        return f"{value * 100:.0f}%"

    lines = [
        f"# CodeGraph MCP value: {report['project']}",
        "",
        f"Tasks: {report['tasks']} · index tasks: {report['generated_from']} · "
        f"limit={report['limit']} · max grep rounds={report['max_rounds']}",
        "",
        "| arm | hit@1 | hit@5 | miss | rounds/calls | bytes read | latency |",
        "|---|---|---|---|---|---|---|",
    ]
    for arm in ("grep", "codegraph"):
        s = report["arms"].get(arm) or {}
        if not s.get("tasks"):
            continue
        lines.append(
            f"| {arm} | {pct(s['hit@1'])} | {pct(s['hit@5'])} | {pct(s['miss'])} | "
            f"{s['rounds']:.1f} | {s['bytes'] / 1024:.1f} KB | {s['ms'] / 1000:.2f} s |"
        )
    lines += ["", "## Per task", "", "| task | kind | question | truth | grep | codegraph |", "|---|---|---|---|---|---|"]
    for task in report["per_task"]:
        g, c = task.get("grep") or {}, task.get("codegraph") or {}
        fmt = lambda r: f"rank {r['rank']} · {r['bytes'] / 1024:.1f} KB · {r['rounds']}" if r else "skipped"
        question = task["question"].replace("|", "\\|")[:70]
        lines.append(f"| {task['id']} | {task['kind']} | {question} | `{task['truth']}` | {fmt(g)} | {fmt(c)} |")
    return "\n".join(lines) + "\n"


# ------------------------------------------------------------- task generation

def read_env(path):
    values = {}
    if not Path(path).is_file():
        return values
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.split("#", 1)[0].strip().strip('"').strip("'")
    return values


def load_env_layers(root):
    """Project .env first, then ~/.codegraph.env as fallback, matching codegraph."""
    merged = read_env(Path.home() / ".codegraph.env")
    merged.update(read_env(Path(root) / ".env"))
    return merged


def sql_query(env, statement, params):
    url = env.get("CODEGRAPH_SURREALDB_URL", "")
    if not url.startswith(("ws://", "wss://", "http://", "https://")):
        raise SystemExit("CODEGRAPH_SURREALDB_URL is not set; cannot sample the index")
    endpoint = url.replace("ws://", "http://").replace("wss://", "https://").rstrip("/")
    if endpoint.endswith("/rpc"):
        endpoint = endpoint[:-4]
    payload = statement.encode()
    for name, value in params.items():
        payload = payload.replace(f"${name}".encode(), json.dumps(value).encode())
    request = urllib.request.Request(endpoint + "/sql", data=payload, method="POST")
    user = env.get("CODEGRAPH_SURREALDB_USERNAME", "")
    password = env.get("CODEGRAPH_SURREALDB_PASSWORD", "")
    if user:
        import base64

        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        request.add_header("Authorization", "Basic " + token)
    request.add_header("surreal-ns", env.get("CODEGRAPH_SURREALDB_NAMESPACE", "codegraph"))
    request.add_header("surreal-db", env.get("CODEGRAPH_SURREALDB_DATABASE", "codegraph"))
    request.add_header("Accept", "application/json")
    with urllib.request.urlopen(request, timeout=30) as response:
        body = json.loads(response.read())
    first = body[0] if body else {}
    if first.get("status") != "OK":
        raise SystemExit(f"index query failed: {first}")
    return first.get("result") or []


COMMENT_LINE = re.compile(r"^\s*(?://[/!]?|#(?!\[))\s*(?P<text>\S.*)$")
CODE_START = re.compile(r"^(fn|pub|struct|enum|impl|use|let|return|if|else|for|while|match|const|static|type|mod)\b")
CODE_SUFFIX = {
    ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".cpp", ".cc",
    ".cxx", ".c", ".h", ".hpp", ".cs", ".rb", ".php", ".kt", ".kts", ".swift", ".dart",
}
MAX_READ_BYTES = 1 << 20  # an experienced agent skips a 7 MB tokenizer table


def natural_comment(content):
    """Return the first line of prose a maintainer would quote, or an empty string."""
    if not content or "zstd:" in content[:200]:
        return ""
    for line in content.splitlines()[:15]:
        if not line.strip().isascii():
            continue  # grep cannot fairly compete on non-ASCII prose
        match = COMMENT_LINE.match(line)
        if not match:
            continue
        text = match.group("text").strip()
        if text.upper().startswith("ABOUTME"):
            text = text.split(":", 1)[-1].strip()
        if len(text) < 25 or CODE_START.match(text):
            continue
        if text.endswith(("{", ";", "(",")")) or "::" in text:
            continue
        if len(extract_keywords(text, 6)) < 3:
            continue
        return text
    return ""


def generate_tasks(root, count, seed, min_name_len=8):
    """Sample symbol and documented-code tasks from the project's own index.

    Only unambiguous symbols are kept: a name that appears once in the project can be
    judged fairly by both arms, while `format` or `info` would only measure noise.
    """
    env = load_env_layers(root)
    project = str(Path(root).resolve())
    counts = sql_query(
        env,
        "SELECT name, count() AS c FROM nodes "
        f"WHERE project_id = {json.dumps(project)} "
        "AND node_type IN ['Function', 'Method', 'Struct', 'Trait'] GROUP BY name LIMIT 5000;",
        {},
    )
    unique = sorted(
        row["name"] for row in counts
        if row.get("name") and len(row["name"]) >= min_name_len and row.get("c") == 1
    )
    if not unique:
        raise SystemExit("no unambiguous symbols in the index; index a larger project")
    random.seed(seed)
    random.shuffle(unique)
    picks = unique[: max(count * 3, 30)]
    quoted = ", ".join(json.dumps(name) for name in picks)
    rows = sql_query(
        env,
        "SELECT name, file_path, start_line, content FROM nodes "
        f"WHERE project_id = {json.dumps(project)} AND name IN [{quoted}];",
        {},
    )
    random.shuffle(rows)

    symbols, concepts = [], []
    for row in rows:
        path = normalize_path(row.get("file_path", ""), project)
        if not path or path.startswith(".codegraph/") or not row.get("name"):
            continue
        symbols.append({
            "kind": "symbol",
            "question": f"Where is `{row['name']}` defined?",
            "keywords": [w for w in re.split(r"[^A-Za-z0-9_]+", row["name"]) if len(w) > 2] or [row["name"]],
            "truth": path,
            "line": row.get("start_line"),
        })
        content = row.get("content") or ""
        comment = natural_comment(content)
        keywords = extract_keywords(comment)
        if len(keywords) >= 3:
            concepts.append({
                "kind": "concept",
                "question": comment[:160],
                "keywords": keywords,
                "truth": path,
                "line": row.get("start_line"),
            })

    # Alternate kinds so a run never degenerates into one question shape.
    tasks, pool = [], {"symbol": symbols, "concept": concepts}
    order = ["symbol", "concept"]
    while len(tasks) < count and (pool["symbol"] or pool["concept"]):
        kind = order[len(tasks) % 2]
        if not pool[kind]:
            kind = "concept" if kind == "symbol" else "symbol"
        if not pool[kind]:
            break
        task = pool[kind].pop()
        if any(t["truth"] == task["truth"] and t["kind"] == kind for t in tasks):
            continue
        task["id"] = f"t{len(tasks) + 1:02d}"
        tasks.append(task)
    return tasks


# ---------------------------------------------------------------------- arms

def read_bytes(paths):
    total = 0
    for path in paths:
        try:
            total += os.path.getsize(path)
        except OSError:
            pass
    return total


def grep_arm(task, root, max_rounds, budget_files):
    """Replay an agent narrowing candidates with ripgrep (or grep), then reading them."""
    rg = shutil.which("rg")
    grep = shutil.which("grep")
    if not rg and not grep:
        return None
    started = time.time()
    scope = [str(root)]
    rounds = 0
    for keyword in task["keywords"][:max_rounds]:
        rounds += 1
        if rg:
            command = [rg, "--files-with-matches", "--no-messages", "-F", keyword] + scope
        else:
            command = [grep, "-rIl", "-F", keyword] + scope
        result = subprocess.run(command, capture_output=True, text=True)
        found = [line for line in result.stdout.splitlines() if line.strip()]
        if not found:
            break
        scope = found
        if len(found) <= budget_files:
            break
    candidates = [normalize_path(p, root) for p in scope if normalize_path(p, root)]
    # An experienced agent skips data files and anything too large to read casually.
    readable = []
    for path in candidates:
        if Path(path).suffix.lower() not in CODE_SUFFIX:
            continue
        try:
            if (root / path).stat().st_size > MAX_READ_BYTES:
                continue
        except OSError:
            continue
        readable.append(path)
    read = readable[:budget_files]
    return {
        "rank": hit_rank(read, task["truth"]),
        "rounds": rounds,
        "bytes": read_bytes([str(root / path) for path in read]),
        "candidates": candidates[:10],
        "ms": int((time.time() - started) * 1000),
    }


class Mcp:
    """Minimal MCP stdio client: initialize, then tools/call."""

    def __init__(self, command, cwd):
        self.process = subprocess.Popen(
            command, cwd=str(cwd), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self.next_id = 1

    def _send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def _wait(self, want_id, timeout):
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.process.stdout.readline()
            if not line:
                raise RuntimeError("MCP server closed stdout")
            line = line.strip()
            if not line.startswith("{"):
                continue
            message = json.loads(line)
            if message.get("id") == want_id:
                if "error" in message:
                    raise RuntimeError(f"MCP error: {message['error']}")
                return message["result"]
        raise TimeoutError(f"no MCP reply for id={want_id}")

    def request(self, method, params, timeout=120.0):
        request_id = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return self._wait(request_id, timeout)

    def initialize(self):
        result = self.request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "mcp-value", "version": "1"},
        })
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
        return result

    def call(self, name, arguments, timeout=180.0):
        result = self.request("tools/call", {"name": name, "arguments": arguments}, timeout)
        blocks = result.get("content") or []
        text = blocks[0].get("text", "") if blocks else ""
        if result.get("isError"):
            raise RuntimeError(f"tool error: {text[:200]}")
        return text

    def close(self):
        for stream in (self.process.stdin, self.process.stdout):
            try:
                stream.close()
            except Exception:
                pass
        self.process.terminate()


def codegraph_arm(task, client, limit, budget_files):
    started = time.time()
    payload = client.call("semantic_code_search", {"query": task["question"], "limit": limit})
    data = json.loads(payload)
    rows = data.get("result", []) if isinstance(data, dict) else data
    candidates = [normalize_path(row.get("file_path", ""), task["root"]) for row in rows]
    candidates = [c for c in candidates if c]
    # Same budget as the grep arm: an agent inspects the first few candidates only.
    return {
        "rank": hit_rank(candidates[:budget_files], task["truth"]),
        "rounds": 1,
        "bytes": len(payload.encode()),
        "candidates": candidates[:10],
        "ms": int((time.time() - started) * 1000),
    }


# ----------------------------------------------------------------------- main

def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--project", type=Path, default=Path.cwd(), help="indexed project root")
    parser.add_argument("--tasks", type=Path, help="frozen task file (JSON)")
    parser.add_argument("--generate", type=int, default=20, help="sample this many tasks from the index")
    parser.add_argument("--seed", type=int, default=7, help="sampling seed for --generate")
    parser.add_argument("--min-name-len", type=int, default=8, help="ignore ambiguous short symbol names")
    parser.add_argument("--limit", type=int, default=10, help="semantic_code_search limit")
    parser.add_argument("--max-rounds", type=int, default=3, help="max grep refinement rounds")
    parser.add_argument("--budget-files", type=int, default=3, help="files an agent reads per question")
    parser.add_argument("--save-tasks", type=Path, help="write the sampled task set here")
    parser.add_argument("--markdown", type=Path, help="write a markdown report here")
    parser.add_argument("--json-out", type=Path, help="write the raw report here")
    parser.add_argument("--command", default=" ".join(DEFAULT_COMMAND), help="MCP server command")
    parser.add_argument("--skip-codegraph", action="store_true", help="run the grep arm only")
    args = parser.parse_args()

    root = args.project.resolve()
    if args.tasks:
        tasks = json.loads(args.tasks.read_text())
    else:
        tasks = generate_tasks(root, args.generate, args.seed, args.min_name_len)
    if not tasks:
        raise SystemExit("no tasks: index the project first (codegraph index .)")
    for task in tasks:
        task.setdefault("root", str(root))

    if args.save_tasks:
        args.save_tasks.write_text(json.dumps(tasks, indent=2))

    client = None
    if not args.skip_codegraph:
        client = Mcp(args.command.split(), root)
        try:
            client.initialize()
        except Exception as error:  # surfaces connection problems instead of silently scoring 0
            client.close()
            raise SystemExit(f"cannot start MCP server ({args.command}): {error}")

    per_task = []
    try:
        for task in tasks:
            record = {"id": task["id"], "kind": task["kind"], "question": task["question"], "truth": task["truth"]}
            record["grep"] = grep_arm(task, root, args.max_rounds, args.budget_files)
            if client:
                record["codegraph"] = codegraph_arm(task, client, args.limit, args.budget_files)
            per_task.append(record)
    finally:
        if client:
            client.close()

    report = {
        "project": str(root),
        "tasks": len(per_task),
        "generated_from": "index" if not args.tasks else "file",
        "limit": args.limit,
        "max_rounds": args.max_rounds,
        "arms": {
            "grep": summarize([t for t in per_task if t.get("grep")], "grep"),
            "codegraph": summarize([t for t in per_task if t.get("codegraph")], "codegraph"),
        },
        "per_task": per_task,
    }
    output = json.dumps(report, indent=2)
    print(output)
    if args.json_out:
        args.json_out.write_text(output)
    if args.markdown:
        args.markdown.write_text(render_markdown(report))


if __name__ == "__main__":
    main()
