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


def median(values):
    ordered = sorted(values)
    if not ordered:
        return 0.0
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return float(ordered[middle])
    return (ordered[middle - 1] + ordered[middle]) / 2


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
        "bytes_median": median([r["bytes"] for r in rows]),
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


def use_postgres(env):
    backend = (env.get("CODEGRAPH_BACKEND") or "").lower()
    if backend == "postgres":
        return True
    if backend == "surreal":
        return False
    return bool(env.get("CODEGRAPH_PG_URL"))


def surreal_to_pg(statement):
    """Translate the handful of SurrealQL shapes this sampler issues into PostgreSQL."""
    import re as _re

    sql = statement.replace("count()", "count(*)")
    sql = _re.sub(r"\bto AS\b", "to_id AS", sql)
    sql = _re.sub(r"\bfrom AS\b", "from_id AS", sql)
    sql = sql.replace("metadata.source_file", "metadata->>'source_file'")

    def fix_in(match):
        inner = match.group(1).replace('"', "'")
        return f"IN ({inner})"

    sql = _re.sub(r"IN \[(.*?)\]", fix_in, sql, flags=_re.S)
    # Remaining double-quoted tokens are string literals the sampler JSON-encoded (the
    # project id); PostgreSQL needs single quotes. No identifiers are quoted here.
    sql = _re.sub(r'"([^"]*)"', r"'\1'", sql)
    return sql


def _pg_psql(env):
    configured = env.get("CODEGRAPH_PG_PSQL")
    if configured:
        import shlex

        return shlex.split(configured)
    if shutil.which("psql"):
        url = env.get("CODEGRAPH_PG_URL")
        if not url:
            raise SystemExit("CODEGRAPH_PG_URL is not set; cannot sample the Postgres index")
        return ["psql", url]
    # Fall back to the reference container so a host without psql can still sample.
    return ["docker", "exec", "-i", "codegraph-pg", "psql", "-U", "codegraph", "-d", "codegraph"]


def pg_query(env, statement, params):
    for name, value in params.items():
        statement = statement.replace(f"${name}", json.dumps(value))
    wrapped = (
        "SELECT coalesce(json_agg(row_to_json(t)), '[]'::json)::text "
        f"FROM ({statement.rstrip().rstrip(';')}) t;"
    )
    result = subprocess.run(
        _pg_psql(env) + ["-t", "-A", "-c", wrapped],
        capture_output=True, text=True, timeout=60,
    )
    if result.returncode != 0:
        raise SystemExit(f"postgres sample failed: {result.stderr.strip()[:400]}")
    body = result.stdout.strip()
    return json.loads(body) if body else []


def sql_query(env, statement, params):
    if use_postgres(env):
        return pg_query(env, surreal_to_pg(statement), params)
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
# Without ripgrep the fallback grep does not read .gitignore, so prune the usual build output.
GRAPH_EXCLUDES = [
    "target", ".git", ".codegraph", "node_modules", "dist", "build",
    ".venv", "__pycache__", ".pytest_cache", ".ruff_cache", ".next", "coverage",
]
GREP_INCLUDES = [
    "--include=*.rs", "--include=*.py", "--include=*.ts", "--include=*.tsx",
    "--include=*.js", "--include=*.jsx", "--include=*.go", "--include=*.java",
    "--include=*.cpp", "--include=*.cc", "--include=*.c", "--include=*.h",
    "--include=*.cs", "--include=*.rb", "--include=*.php", "--include=*.kt",
    "--include=*.swift", "--include=*.dart",
]
MAX_SECOND_HOP = 12  # files the second hop may add to the candidate set


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


def window_bytes(path, line, window):
    """Bytes an agent pulls when it reads `window` lines around a hit instead of the file."""
    try:
        text = path.read_text(errors="ignore")
    except OSError:
        return 0
    lines = text.splitlines(keepends=True)
    if not lines:
        return 0
    last = len(lines) - 1
    start = min(max(int(line) - 1 - window, 0), last)
    stop = min(start + window * 2 + 1, len(lines))
    return len("".join(lines[start:stop]).encode())


HIT_LINE = re.compile(r"^(?P<file>[^:]+):(?P<line>\d+):")


def parse_hits(output):
    """Parse `path:line:content` rows from grep/rg output into (path, line) pairs."""
    hits = []
    for row in output.splitlines():
        match = HIT_LINE.match(row)
        if not match:
            continue
        hits.append((match.group("file"), int(match.group("line"))))
    return hits


def ordered_files(hits):
    seen, files = set(), []
    for path, _ in hits:
        if path not in seen:
            seen.add(path)
            files.append(path)
    return files


def first_lines(hits):
    lines = {}
    for path, line in hits:
        lines.setdefault(path, line)
    return lines


SEARCH_MAX_PER_FILE = 3
SEARCH_MAX_LINES = 40


def run_search(keyword, scope, rg, grep, max_per_file=SEARCH_MAX_PER_FILE,
               max_lines=SEARCH_MAX_LINES):
    """One grep/rg pass with line numbers, capped like an agent piping through head.

    Returns (hits, output_bytes). Without the cap a common word produces hundreds of rows,
    which no agent reads and which would overstate the baseline.
    """
    if rg:
        command = [rg, "-n", "--no-heading", "--no-messages", "-F",
                   "-m", str(max_per_file), keyword] + scope
    else:
        command = [grep, "-rn", "-m", str(max_per_file), "-F"] + GREP_INCLUDES + [
            f"--exclude-dir={name}" for name in GRAPH_EXCLUDES
        ] + [keyword] + scope
    result = subprocess.run(command, capture_output=True, text=True, timeout=120)
    rows = result.stdout.splitlines()[:max_lines]
    text = "\n".join(rows)
    return parse_hits(text), len(text.encode())


def readable_hits(hits, root, budget_files):
    """Project-relative code files in hit order, capped at the inspection budget."""
    out = []
    for path in ordered_files(hits):
        relative = normalize_path(path, root)
        if not relative or Path(relative).suffix.lower() not in CODE_SUFFIX:
            continue
        try:
            if (root / relative).stat().st_size > MAX_READ_BYTES:
                continue
        except OSError:
            continue
        out.append(relative)
        if len(out) >= budget_files:
            break
    return out


def window_cost(root, files, lines, window):
    """Bytes billed for reading a line window around each hit."""
    total = 0
    for path in files:
        total += window_bytes(root / path, lines.get(path, 1), window)
    return total


def grep_arm(task, root, max_rounds, budget_files, window=40):
    """Replay an agent that greps with line numbers, then reads only around the hit.

    Cost is what enters the context: the grep output the agent scans plus the line window it
    opens per candidate. Reading whole files would overstate what a careful agent does.
    """
    started = time.time()
    rg = shutil.which("rg")
    grep = shutil.which("grep")
    if not rg and not grep:
        return None
    spent = 0
    rounds = 0
    scope = [str(root)]
    hits = []
    for keyword in task["keywords"][:max_rounds]:
        rounds += 1
        if rg:
            command = [rg, "-n", "--no-heading", "--no-messages", "-F", keyword] + scope
        else:
            # `grep -m 5` caps matches per file; without it a common word floods the pipe.
            command = [grep, "-rn", "-m", "5", "-F"] + GREP_INCLUDES + [
                f"--exclude-dir={name}" for name in GRAPH_EXCLUDES
            ] + [keyword] + scope
        result = subprocess.run(command, capture_output=True, text=True, timeout=120)
        spent += len(result.stdout.encode())
        hits = parse_hits(result.stdout)
        files = ordered_files(hits)
        scope = files or [str(root)]
        if len(files) <= budget_files:
            break
    candidates = [normalize_path(path, root) for path in ordered_files(hits)]
    candidates = [c for c in candidates if c and Path(c).suffix.lower() in CODE_SUFFIX]
    read = candidates[:budget_files]
    lines = {normalize_path(path, root): line for path, line in first_lines(hits).items()}
    read_cost = 0
    for path in read:
        target = root / path
        try:
            if target.stat().st_size > MAX_READ_BYTES:
                continue
        except OSError:
            continue
        read_cost += window_bytes(target, lines.get(path, 1), window)
    return {
        "rank": hit_rank(read, task["truth"]),
        "rounds": rounds,
        "bytes": spent + read_cost,
        "grep_bytes": spent,
        "read_bytes": read_cost,
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

def summarize_impact(per_task, arm):
    rows = [t[arm] for t in per_task if t.get(arm)]
    if not rows:
        return {"tasks": 0}
    total = len(rows)
    return {
        "tasks": total,
        "recall": sum(r["recall"] for r in rows) / total,
        "precision": sum(r["precision"] for r in rows) / total,
        "complete": sum(1 for r in rows if r["recall"] >= 1.0) / total,
        "calls": sum(r["rounds"] for r in rows) / total,
        "bytes": sum(r["bytes"] for r in rows) / total,
        "bytes_median": median([r["bytes"] for r in rows]),
        "ms": sum(r["ms"] for r in rows) / total,
    }


def render_impact_markdown(report):
    def pct(value):
        return f"{value * 100:.0f}%"

    lines = [
        f"# CodeGraph MCP value (impact): {report['project']}",
        "",
        f"Tasks: {report['tasks']} · budget={report['budget_files']} files per question",
        "",
        "| arm | caller recall | precision | found all | calls | bytes read | latency |",
        "|---|---|---|---|---|---|---|",
    ]
    for arm in ("grep", "codegraph"):
        s = report["arms"].get(arm) or {}
        if not s.get("tasks"):
            continue
        lines.append(
            f"| {arm} | {pct(s['recall'])} | {pct(s['precision'])} | {pct(s['complete'])} | "
            f"{s['calls']:.1f} | {s['bytes'] / 1024:.1f} KB | {s['ms'] / 1000:.2f} s |"
        )
    lines += ["", "## Per task", "", "| task | target | callers | grep | codegraph |", "|---|---|---|---|---|"]
    for task in report["per_task"]:
        g, c = task.get("grep") or {}, task.get("codegraph") or {}
        fmt = lambda r: f"recall {r['recall']:.0%} · {r['bytes'] / 1024:.1f} KB" if r else "skipped"
        lines.append(f"| {task['id']} | `{task['target_name']}` | {task['caller_count']} | {fmt(g)} | {fmt(c)} |")
    return "\n".join(lines) + "\n"


def overlap(candidates, truth):
    """Recall and precision of candidate files against the ground-truth caller files."""
    found = set(candidates)
    expected = set(truth)
    if not expected:
        return 0.0, 0.0
    hits = len(found & expected)
    return hits / len(expected), (hits / len(found) if found else 0.0)


def generate_impact_tasks(root, count, seed, min_name_len=8, min_callers=2, max_callers=8):
    """Sample targets whose incoming `calls` edges form a small, checkable caller set."""
    env = load_env_layers(root)
    project = str(Path(root).resolve())
    counts = sql_query(
        env,
        "SELECT to AS target, count() AS c FROM edges "
        f"WHERE project_id = {json.dumps(project)} AND edge_type = 'calls' GROUP BY target LIMIT 4000;",
        {},
    )
    wanted = [
        row["target"] for row in counts
        if row.get("target") and min_callers <= (row.get("c") or 0) <= max_callers
    ]
    random.seed(seed)
    random.shuffle(wanted)
    if not wanted:
        raise SystemExit("no targets with a small caller set; index a project with call edges")
    # One pass over the node table beats an `id IN (nodes:`...`)` filter, which compares
    # a RecordId against string literals.
    nodes = sql_query(
        env,
        "SELECT id, name, file_path FROM nodes "
        f"WHERE project_id = {json.dumps(project)} LIMIT 20000;",
        {},
    )
    by_id = {str(row["id"]): row for row in nodes}
    # Caller files per target in one pass: comparing `to` against a RecordId literal is
    # what an `IN (nodes:`...`)` filter gets wrong.
    edge_rows = sql_query(
        env,
        "SELECT to AS target, from AS caller, metadata.source_file AS site FROM edges "
        f"WHERE project_id = {json.dumps(project)} AND edge_type = 'calls' LIMIT 50000;",
        {},
    )
    truth_by_target = {}
    for row in edge_rows:
        target = str(row.get("target") or "")
        caller = by_id.get(str(row.get("caller") or ""))
        # Ground truth is the caller's definition file, which is what the graph reports.
        raw = (caller or {}).get("file_path") or row.get("site") or ""
        path = normalize_path(raw, project)
        if target and path:
            truth_by_target.setdefault(target, set()).add(path)
    tasks = []
    for target_id in wanted:
        if len(tasks) >= count:
            break
        node = by_id.get(target_id)
        if not node or len(node.get("name") or "") < min_name_len:
            continue
        truth = sorted(truth_by_target.get(target_id, set()))
        if len(truth) < min_callers:
            continue
        tasks.append({
            "id": f"i{len(tasks) + 1:02d}",
            "target_id": target_id,
            "target_name": node["name"],
            "target_file": normalize_path(node.get("file_path") or "", project),
            "keywords": [w for w in re.split(r"[^A-Za-z0-9_]+", node["name"]) if len(w) > 2] or [node["name"]],
            "truth": truth,
            "caller_count": len(truth),
            "root": str(root),
        })
    return tasks


def grep_impact_arm(task, root, rounds_budget, budget_files, window=40):
    """Find who calls `target`: grep its name, then read only around each hit."""
    rg = shutil.which("rg")
    grep = shutil.which("grep")
    if not rg and not grep:
        return None
    started = time.time()
    keyword = task["keywords"][0] if task["keywords"] else task["target_name"]
    hits, spent = run_search(keyword, [str(root)], rg, grep)
    candidates = []
    for path in ordered_files(hits):
        relative = normalize_path(path, root)
        if relative and Path(relative).suffix.lower() in CODE_SUFFIX:
            candidates.append(relative)
    read = readable_hits(hits, root, budget_files)
    lines = {normalize_path(path, root): line for path, line in first_lines(hits).items()}
    # An impact answer is a list, so recall scores the whole hit list; bytes bill the grep
    # output plus the windows an agent opens to confirm each caller.
    recall, precision = overlap(candidates, task["truth"])
    return {
        "recall": recall,
        "precision": precision,
        "rounds": 1 if hits else 0,
        "bytes": spent + window_cost(root, read, lines, window),
        "grep_bytes": spent,
        "candidates": candidates[:10],
        "ms": int((time.time() - started) * 1000),
    }


def codegraph_impact_arm(task, client, budget_files):
    started = time.time()
    payload = client.call("get_reverse_dependencies", {
        "node_id": task["target_id"],
        "edge_type": "calls",
        "depth": 1,
    })
    data = json.loads(payload)
    rows = data.get("result", []) if isinstance(data, dict) else data
    candidates = []
    for row in rows:
        location = row.get("location") or {}
        relative = normalize_path(location.get("file_path") or "", task["root"])
        if relative:
            candidates.append(relative)
    recall, precision = overlap(candidates, task["truth"])
    return {
        "recall": recall,
        "precision": precision,
        "rounds": 1,
        "bytes": len(payload.encode()),
        "candidates": candidates[:10],
        "ms": int((time.time() - started) * 1000),
    }


def render_chain_markdown(report):
    def pct(value):
        return f"{value * 100:.0f}%"

    lines = [
        f"# CodeGraph MCP value (call chain): {report['project']}",
        "",
        f"Tasks: {report['tasks']} · depth={report['depth']} · budget={report['budget_files']} files",
        "",
        "| arm | reached recall | precision | found all | calls | bytes read | latency |",
        "|---|---|---|---|---|---|---|",
    ]
    for arm in ("grep", "codegraph"):
        s = report["arms"].get(arm) or {}
        if not s.get("tasks"):
            continue
        lines.append(
            f"| {arm} | {pct(s['recall'])} | {pct(s['precision'])} | {pct(s['complete'])} | "
            f"{s['calls']:.1f} | {s['bytes'] / 1024:.1f} KB | {s['ms'] / 1000:.2f} s |"
        )
    lines += ["", "## Per task", "", "| task | start | reachable | grep | codegraph |", "|---|---|---|---|---|"]
    for task in report["per_task"]:
        g, c = task.get("grep") or {}, task.get("codegraph") or {}
        fmt = lambda r: f"recall {r['recall']:.0%} · {r['bytes'] / 1024:.1f} KB" if r else "skipped"
        lines.append(f"| {task['id']} | `{task['target_name']}` | {task['reach_count']} | {fmt(g)} | {fmt(c)} |")
    return "\n".join(lines) + "\n"


def generate_chain_tasks(root, count, seed, depth=2, min_name_len=8, min_reach=3, max_reach=15):
    """Sample start nodes whose outgoing `calls` closure at `depth` is small enough to check."""
    env = load_env_layers(root)
    project = str(Path(root).resolve())
    nodes = sql_query(
        env,
        "SELECT id, name, file_path FROM nodes "
        f"WHERE project_id = {json.dumps(project)} LIMIT 20000;",
        {},
    )
    by_id = {str(row["id"]): row for row in nodes}
    edge_rows = sql_query(
        env,
        "SELECT from AS src, to AS dst FROM edges "
        f"WHERE project_id = {json.dumps(project)} AND edge_type = 'calls' LIMIT 50000;",
        {},
    )
    adjacency = {}
    for row in edge_rows:
        src, dst = str(row.get("src") or ""), str(row.get("dst") or "")
        if src and dst:
            adjacency.setdefault(src, set()).add(dst)

    roots = [node_id for node_id in adjacency if node_id in by_id]
    random.seed(seed)
    random.shuffle(roots)
    tasks = []
    for target_id in roots:
        if len(tasks) >= count or len(tasks) >= len(roots):
            break
        node = by_id[target_id]
        if len(node.get("name") or "") < min_name_len:
            continue
        seen = {target_id}
        frontier = {target_id}
        for _ in range(depth):
            nxt = set()
            for current in frontier:
                nxt |= adjacency.get(current, set())
            nxt -= seen
            seen |= nxt
            frontier = nxt
            if not frontier:
                break
        reached = seen - {target_id}
        files = sorted({
            normalize_path((by_id.get(item) or {}).get("file_path") or "", project)
            for item in reached
        } - {""})
        if not (min_reach <= len(reached) <= max_reach) or len(files) < min_reach:
            continue
        tasks.append({
            "id": f"c{len(tasks) + 1:02d}",
            "target_id": target_id,
            "target_name": node["name"],
            "target_file": normalize_path(node.get("file_path") or "", project),
            "keywords": [w for w in re.split(r"[^A-Za-z0-9_]+", node["name"]) if len(w) > 2] or [node["name"]],
            "truth": files,
            "reach_count": len(files),
            "root": str(root),
        })
    return tasks


CALL_SITE = re.compile(r"([A-Za-z_][A-Za-z0-9_]{3,})\s*\(")
CONTROL = {
    "if", "for", "while", "match", "return", "fn", "let", "mut", "else", "loop",
    "impl", "struct", "enum", "use", "pub", "self", "this", "new", "unwrap", "expect",
    "Some", "None", "Ok", "Err", "map", "iter", "into", "clone", "format", "println",
}


def called_names(paths, limit=20):
    """Read files and guess which symbols they call, the way a grep-only agent must."""
    names, seen = [], set()
    for path in paths:
        try:
            text = path.read_text(errors="ignore")
        except OSError:
            continue
        for name in CALL_SITE.findall(text):
            if name in CONTROL or name in seen:
                continue
            seen.add(name)
            names.append(name)
            if len(names) >= limit:
                return names
    return names


def grep_chain_arm(task, root, budget_files, window=40):
    """Two-hop baseline: grep the start name, read around the hits, then grep what they call."""
    rg = shutil.which("rg")
    grep = shutil.which("grep")
    if not rg and not grep:
        return None
    started = time.time()
    calls, spent = 0, 0

    def search(keyword):
        nonlocal calls, spent
        calls += 1
        hits, out_bytes = run_search(keyword, [str(root)], rg, grep)
        spent += out_bytes
        return hits

    first_hits = search(task["target_name"])
    read = readable_hits(first_hits, root, budget_files)
    first_lines_map = {normalize_path(p, root): l for p, l in first_lines(first_hits).items()}
    cost = window_cost(root, read, first_lines_map, window)

    # Hop two: resolve each called symbol to its first readable hit. Dumping every hit of
    # twenty names would make the baseline a strawman, not a careful agent.
    second = []
    for name in called_names([root / path for path in read]):
        hits = search(name)
        for hit in readable_hits(hits, root, 1):
            if hit not in second:
                second.append(hit)
                cost += window_cost(root, [hit],
                                    {normalize_path(p, root): l for p, l in first_lines(hits).items()},
                                    window)
        if len(second) >= MAX_SECOND_HOP:
            break

    candidates = list(dict.fromkeys(
        readable_hits(first_hits, root, 10_000) + second
    ))
    recall, precision = overlap(candidates, task["truth"])
    return {
        "recall": recall,
        "precision": precision,
        "rounds": calls,
        "bytes": spent + cost,
        "grep_bytes": spent,
        "candidates": candidates[:10],
        "ms": int((time.time() - started) * 1000),
    }


def codegraph_chain_arm(task, client, depth, budget_files):
    started = time.time()
    payload = client.call("get_transitive_dependencies", {
        "node_id": task["target_id"],
        "edge_type": "calls",
        "depth": depth,
    })
    data = json.loads(payload)
    rows = data.get("result", []) if isinstance(data, dict) else data
    candidates = []
    for row in rows:
        location = row.get("location") or {}
        relative = normalize_path(location.get("file_path") or "", task["root"])
        if relative:
            candidates.append(relative)
    recall, precision = overlap(candidates, task["truth"])
    return {
        "recall": recall,
        "precision": precision,
        "rounds": 1,
        "bytes": len(payload.encode()),
        "candidates": list(dict.fromkeys(candidates))[:10],
        "ms": int((time.time() - started) * 1000),
    }


def run_chain(args, root):
    depth = max(1, min(args.depth, 5))
    tasks = (
        json.loads(args.tasks.read_text())
        if args.tasks else generate_chain_tasks(root, args.generate, args.seed, depth)
    )
    if not tasks:
        raise SystemExit("no call-chain tasks: index the project first (codegraph index .)")
    if args.save_tasks:
        args.save_tasks.write_text(json.dumps(tasks, indent=2))

    client = None
    if not args.skip_codegraph:
        client = Mcp(args.command.split(), root)
        try:
            client.initialize()
        except Exception as error:
            client.close()
            raise SystemExit(f"cannot start MCP server ({args.command}): {error}")
    per_task = []
    try:
        for task in tasks:
            record = {
                "id": task["id"], "target_name": task["target_name"],
                "target_file": task["target_file"], "reach_count": task["reach_count"],
            }
            record["grep"] = grep_chain_arm(task, root, args.budget_files)
            if client:
                record["codegraph"] = codegraph_chain_arm(task, client, depth, args.budget_files)
            per_task.append(record)
    finally:
        if client:
            client.close()

    report = {
        "project": str(root),
        "mode": "chain",
        "tasks": len(per_task),
        "depth": depth,
        "budget_files": args.budget_files,
        "arms": {
            "grep": summarize_impact(per_task, "grep"),
            "codegraph": summarize_impact(per_task, "codegraph"),
        },
        "per_task": per_task,
    }
    output = json.dumps(report, indent=2)
    print(output)
    if args.json_out:
        args.json_out.write_text(output)
    if args.markdown:
        args.markdown.write_text(render_chain_markdown(report))


def run_impact(args, root):
    tasks = (
        json.loads(args.tasks.read_text())
        if args.tasks else generate_impact_tasks(root, args.generate, args.seed)
    )
    if not tasks:
        raise SystemExit("no impact tasks: index the project first (codegraph index .)")
    if args.save_tasks:
        args.save_tasks.write_text(json.dumps(tasks, indent=2))

    client = None
    if not args.skip_codegraph:
        client = Mcp(args.command.split(), root)
        try:
            client.initialize()
        except Exception as error:
            client.close()
            raise SystemExit(f"cannot start MCP server ({args.command}): {error}")
    per_task = []
    try:
        for task in tasks:
            record = {
                "id": task["id"], "target_name": task["target_name"],
                "target_file": task["target_file"], "caller_count": task["caller_count"],
            }
            record["grep"] = grep_impact_arm(task, root, args.max_rounds, args.budget_files)
            if client:
                record["codegraph"] = codegraph_impact_arm(task, client, args.budget_files)
            per_task.append(record)
    finally:
        if client:
            client.close()

    report = {
        "project": str(root),
        "mode": "impact",
        "tasks": len(per_task),
        "limit": args.limit,
        "budget_files": args.budget_files,
        "arms": {
            "grep": summarize_impact(per_task, "grep"),
            "codegraph": summarize_impact(per_task, "codegraph"),
        },
        "per_task": per_task,
    }
    output = json.dumps(report, indent=2)
    print(output)
    if args.json_out:
        args.json_out.write_text(output)
    if args.markdown:
        args.markdown.write_text(render_impact_markdown(report))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--mode", choices=("locate", "impact", "chain"), default="locate",
                        help="locate: where is a symbol; impact: who calls a target; chain: what does it reach")
    parser.add_argument("--depth", type=int, default=2, help="call-chain depth")
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
    if args.mode == "impact":
        return run_impact(args, root)
    if args.mode == "chain":
        return run_chain(args, root)
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
