#!/usr/bin/env python3
"""Measure what CodeGraph's tools buy an actual agent on real, already-merged changes.

Each task is a merged commit: the agent starts from the parent commit in a throwaway git
worktree, gets the commit subject as the problem statement, and must edit the repository.
A task scores by how well the edited files match the files the real commit touched.

Two arms share one model, one prompt skeleton and one step budget:

  baseline   read_file, grep, list_files, apply_patch, done
  codegraph  the same plus semantic_code_search and get_reverse_dependencies over MCP

    python3 scripts/benchmarks/agent_value.py --tasks 4 --max-steps 8
    python3 scripts/benchmarks/agent_value.py --tasks 2 --arm baseline

The index is built from the current checkout, so a worktree at the parent commit may differ
slightly from what the graph knows. Paths are stable; treat file-level scores as the signal.
"""
import argparse
import json
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mcp_value as mv  # noqa: E402  (reuse the env loader and the MCP stdio client)

SYSTEM = """You are a code agent working inside a git worktree. Solve the task by
inspecting files and editing them. Call exactly one tool per turn.

Rules:
- Paths are relative to the repository root.
- `old` in apply_patch must be an exact substring that occurs exactly once in the file.
- Prefer the smallest correct edit. Do not rewrite whole files.
- Keep going until you have edited at least one file; reading alone scores zero.
- Once you have located the code to change, call apply_patch. Do not spend the whole
  budget reading: a partial edit scores better than no edit.
- You have at most {max_steps} tool calls. Call done as soon as the change is complete.
"""

GRAPH_HINT = """
You also have two graph tools. Use them instead of guessing keywords:
- semantic_code_search: searches the indexed code by meaning. Call it whenever you do not
  already know the exact file or symbol name; grep only matches literal strings.
- get_reverse_dependencies: lists the callers of a node_id returned by semantic_code_search.
"""

MAX_OUTPUT_CHARS = 3000
MAX_READ_LINES = 160

TOOL_SCHEMAS = {
    "read_file": {
        "description": "Read a file from the worktree, with 1-based line numbers.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "path relative to the repo root"},
                "offset": {"type": "integer", "description": "first line to return (1-based)"},
                "limit": {"type": "integer", "description": "how many lines to return"},
            },
            "required": ["path"],
        },
    },
    "grep": {
        "description": "Literal text search across source files; returns file:line:content.",
        "parameters": {
            "type": "object",
            "properties": {"pattern": {"type": "string"}},
            "required": ["pattern"],
        },
    },
    "list_files": {
        "description": "List files matching a glob relative to the repo root.",
        "parameters": {
            "type": "object",
            "properties": {"glob": {"type": "string"}},
            "required": ["glob"],
        },
    },
    "apply_patch": {
        "description": "Replace one exact substring in a file. `old` must occur exactly once.",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old": {"type": "string"},
                "new": {"type": "string"},
            },
            "required": ["path", "old", "new"],
        },
    },
    "done": {
        "description": "Finish the task. Call this once the edit is complete.",
        "parameters": {
            "type": "object",
            "properties": {"summary": {"type": "string"}},
            "required": ["summary"],
        },
    },
    "semantic_code_search": {
        "description": "Search the code graph by meaning; returns name, file:line and node_id.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
            },
            "required": ["query"],
        },
    },
    "get_reverse_dependencies": {
        "description": "List who calls a node. Pass a node_id returned by semantic_code_search.",
        "parameters": {
            "type": "object",
            "properties": {
                "node_id": {"type": "string"},
                "depth": {"type": "integer"},
            },
            "required": ["node_id"],
        },
    },
}

BASE_TOOL_NAMES = ["read_file", "grep", "list_files", "apply_patch", "done"]
CODEGRAPH_TOOL_NAMES = BASE_TOOL_NAMES + ["semantic_code_search", "get_reverse_dependencies"]


def tool_schemas(arm):
    names = CODEGRAPH_TOOL_NAMES if arm == "codegraph" else BASE_TOOL_NAMES
    return [
        {"type": "function", "function": {"name": name, **TOOL_SCHEMAS[name]}}
        for name in names
    ]


# --------------------------------------------------------------------- tasks

def git(root, *args):
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)


def pick_tasks(root, count, max_files=1, min_lines=6, max_lines=60):
    """Pick merged commits that changed a small, code-only set of files."""
    log = git(root, "log", "--no-merges", "--diff-filter=M", "--numstat",
              "--pretty=format:COMMIT\t%H\t%s", "-n", "400").stdout
    tasks, current = [], None
    for line in log.splitlines():
        if line.startswith("COMMIT\t"):
            _, sha, subject = line.split("\t", 2)
            current = {"sha": sha, "subject": subject, "files": [], "lines": 0}
            tasks.append(current)
            continue
        if not current or not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) != 3:
            continue
        added, removed, path = parts
        if added == "-" or removed == "-":
            current["lines"] = 10 ** 6  # binary
            continue
        current["files"].append(path)
        current["lines"] += int(added) + int(removed)

    # Small fixes first: they are the shape an agent can actually finish in a few steps.
    tasks.sort(key=lambda task: (0 if task["subject"].startswith("fix") else 1, task["lines"]))
    picked = []
    for task in tasks:
        if not task["files"] or len(task["files"]) > max_files:
            continue
        if not (min_lines <= task["lines"] <= max_lines):
            continue
        if any(not _is_code(path) for path in task["files"]):
            continue
        if any(_too_large(root, path, max_lines=900) for path in task["files"]):
            continue
        if len(task["subject"]) < 20:
            continue
        task["golden"] = sorted(set(task["files"]))
        picked.append(task)
        if len(picked) >= count:
            break
    return picked


def _too_large(root, path, max_lines=900):
    """A 2000-line file is a navigation task of its own; keep tasks tractable."""
    try:
        return sum(1 for _ in (Path(root) / path).open(errors="ignore")) > max_lines
    except OSError:
        return True


def _is_code(path):
    return Path(path).suffix.lower() in mv.CODE_SUFFIX


def make_worktree(root, sha, target):
    if target.exists():
        shutil.rmtree(target, ignore_errors=True)
    target.parent.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        ["git", "-C", str(root), "worktree", "add", "--detach", "--force", str(target), f"{sha}^"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"worktree add failed: {result.stderr.strip()[:300]}")
    return target


def drop_worktree(root, target):
    subprocess.run(["git", "-C", str(root), "worktree", "remove", "--force", str(target)],
                   capture_output=True, text=True)
    shutil.rmtree(target, ignore_errors=True)


# ----------------------------------------------------------------------- llm

class Llm:
    def __init__(self, env, timeout=180):
        base = (env.get("CODEGRAPH_OPENAI_COMPATIBLE_URL") or "").rstrip("/")
        key = env.get("OPENAI_COMPATIBLE_API_KEY") or env.get("OPENAI_API_KEY")
        if not base or not key:
            raise SystemExit("need CODEGRAPH_OPENAI_COMPATIBLE_URL and an API key in the env layers")
        self.url = base + "/chat/completions"
        self.key = key
        self.model = env.get("CODEGRAPH_LLM_MODEL") or "deepseek-flash"
        self.timeout = timeout
        self.prompt_tokens = 0
        self.completion_tokens = 0

    def chat(self, messages, tools=None, max_tokens=1500):
        body = {
            "model": self.model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": 0,
        }
        if tools:
            body["tools"] = tools
            body["tool_choice"] = "auto"
        payload = json.dumps(body).encode()
        request = urllib.request.Request(
            self.url, data=payload, method="POST",
            headers={"Content-Type": "application/json", "Authorization": f"Bearer {self.key}"},
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                body = json.loads(response.read())
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", "replace")[:500]
            raise RuntimeError(f"LLM HTTP {error.code}: {detail}") from error
        usage = body.get("usage") or {}
        self.prompt_tokens += usage.get("prompt_tokens", 0)
        self.completion_tokens += usage.get("completion_tokens", 0)
        return body["choices"][0]["message"]


def parse_action(text):
    """Extract the last JSON object carrying a `tool` key from a reply.

    Reasoning models sometimes wrap the object in prose or emit two of them, so scan with
    raw_decode instead of a greedy regex.
    """
    decoder = json.JSONDecoder()
    found, index, text = [], 0, text or ""
    while index < len(text):
        start = text.find("{", index)
        if start < 0:
            break
        try:
            obj, end = decoder.raw_decode(text, start)
        except ValueError:
            index = start + 1
            continue
        if isinstance(obj, dict) and obj.get("tool"):
            found.append(obj)
        index = end if end > start else start + 1
    return found[-1] if found else None


# --------------------------------------------------------------------- tools

class Workspace:
    def __init__(self, root):
        self.root = Path(root)

    def _resolve(self, path):
        candidate = (self.root / str(path)).resolve()
        if not str(candidate).startswith(str(self.root.resolve())):
            raise ValueError("path escapes the worktree")
        return candidate

    def read_file(self, path, offset=1, limit=MAX_READ_LINES, **kwargs):
        target = self._resolve(path)
        if not target.is_file():
            return f"error: no such file: {path}"
        lines = target.read_text(errors="ignore").splitlines()
        start = max(int(offset) - 1, 0)
        chunk = lines[start:start + min(int(limit), MAX_READ_LINES)]
        body = "\n".join(f"{start + index + 1}: {line}" for index, line in enumerate(chunk))
        return body[:MAX_OUTPUT_CHARS] or "(empty file)"

    def grep(self, pattern, **kwargs):
        if not pattern:
            return "error: empty pattern"
        result = subprocess.run(
            ["grep", "-rIn", "-F", str(pattern), "--include=*.rs", "--include=*.py",
             "--include=*.ts", "--include=*.tsx", "--include=*.js", "--include=*.go",
             ".", "--exclude-dir=.git", "--exclude-dir=.codegraph"],
            cwd=str(self.root), capture_output=True, text=True,
        )
        rows = result.stdout.splitlines()[:60]
        return "\n".join(rows)[:MAX_OUTPUT_CHARS] if rows else "(no matches)"

    def list_files(self, glob="**/*", **kwargs):
        rows = [str(path.relative_to(self.root)) for path in self.root.glob(str(glob)) if path.is_file()]
        rows = [row for row in rows if "/.git/" not in row][:80]
        return "\n".join(rows) if rows else "(no files)"

    def apply_patch(self, path, old, new, **kwargs):
        target = self._resolve(path)
        if not target.is_file():
            return f"error: no such file: {path}"
        text = target.read_text(errors="ignore")
        if not old or text.count(old) != 1:
            return f"error: `old` must occur exactly once, found {text.count(old) if old else 0} time(s)"
        target.write_text(text.replace(old, new, 1))
        return f"patched {path}"

    def changed_files(self):
        result = subprocess.run(["git", "-C", str(self.root), "diff", "--name-only"],
                                capture_output=True, text=True)
        return [line for line in result.stdout.splitlines() if line.strip()]

    def restore(self):
        subprocess.run(["git", "-C", str(self.root), "checkout", "--", "."],
                       capture_output=True, text=True)


def trim_messages(messages, budget_chars=90000, floor=2000):
    """Shrink the oldest tool observations until the transcript fits the budget.

    Both arms get the same treatment, so a long run measures the tool list and not which
    arm happened to overflow first.
    """
    total = sum(len(str(message.get("content") or "")) for message in messages)
    if total <= budget_chars:
        return
    for message in messages:
        if total <= budget_chars:
            break
        if message.get("role") != "tool":
            continue
        text = str(message.get("content") or "")
        if len(text) <= floor:
            continue
        keep = floor
        total -= len(text) - keep
        message["content"] = text[:keep] + "\n[truncated]"


def execute_tool(name, args, workspace, client, arm):
    """Run one tool call and return the observation text."""
    if name == "semantic_code_search" and arm == "codegraph" and client:
        payload = client.call("semantic_code_search",
                              {"query": str(args.get("query", "")),
                               "limit": int(args.get("limit") or 8)})
        rows = json.loads(payload).get("result", [])
        return "\n".join(
            f"{row.get('name')} | {row.get('file_path')}:{row.get('start_line')} | {row.get('node_id', '')}"
            for row in rows[:8]
        ) or "(no results)"
    if name == "get_reverse_dependencies" and arm == "codegraph" and client:
        payload = client.call("get_reverse_dependencies",
                              {"node_id": str(args.get("node_id", "")),
                               "edge_type": "calls",
                               "depth": int(args.get("depth") or 1)})
        rows = json.loads(payload).get("result", [])
        return "\n".join(
            f"{row.get('name')} | {(row.get('location') or {}).get('file_path')}"
            for row in rows[:10]
        ) or "(no callers)"
    if name in {"read_file", "grep", "list_files", "apply_patch"}:
        return getattr(workspace, name)(**args)
    return f"error: unknown tool {name}"


def run_agent(llm, workspace, problem, arm, max_steps, client=None):
    """Drive the model with native tool calling; one tool per turn, capped at max_steps."""
    tools = tool_schemas(arm)
    system = SYSTEM.replace("{max_steps}", str(max_steps))
    if arm == "codegraph":
        system += GRAPH_HINT
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": f"Task: {problem}"},
    ]
    steps, started = [], time.time()
    step, no_tool = 0, 0
    while step < max_steps and no_tool < 2:
        message = llm.chat(messages, tools)
        calls = message.get("tool_calls") or []
        if not calls:
            # Some models still answer with a JSON action in content; accept that shape.
            action = parse_action(message.get("content") or "")
            if action:
                calls = [{
                    "id": f"call_{step}",
                    "type": "function",
                    "function": {"name": action["tool"],
                                 "arguments": json.dumps(action.get("args") or {})},
                }]
            else:
                no_tool += 1
                messages.append({"role": "assistant", "content": message.get("content") or "(no tool call)"})
                messages.append({"role": "user", "content": "Call exactly one of the provided tools."})
                continue
        call = calls[0]
        function = call.get("function") or {}
        name = function.get("name") or ""
        try:
            args = json.loads(function.get("arguments") or "{}")
        except json.JSONDecodeError:
            args = {}
        if not isinstance(args, dict):
            args = {}
        assistant_message = {
            "role": "assistant",
            "content": message.get("content"),
            "tool_calls": [{"id": call.get("id") or f"call_{step}", "type": "function",
                            "function": function}],
        }
        # Thinking models reject a follow-up turn that drops their reasoning trace.
        if message.get("reasoning_content"):
            assistant_message["reasoning_content"] = message["reasoning_content"]
        messages.append(assistant_message)
        if name == "done":
            steps.append({"step": step + 1, "tool": "done",
                          "summary": str(args.get("summary"))[:200]})
            break
        try:
            observation = execute_tool(name, args, workspace, client, arm)
        except Exception as error:  # a tool failure is an observation, not a crash
            observation = f"error: {error}"
        observation = str(observation)
        steps.append({"step": step + 1, "tool": name, "args": args,
                      "observation": observation[:500]})
        messages.append({"role": "tool", "tool_call_id": call.get("id") or f"call_{step}",
                         "content": observation[:MAX_OUTPUT_CHARS]})
        trim_messages(messages)
        step += 1
    elapsed = time.time() - started
    return {"steps": steps, "changed": workspace.changed_files(), "elapsed": elapsed}


# ------------------------------------------------------------------- scoring

def score(edited, golden):
    edited_set, golden_set = set(edited), set(golden)
    hits = edited_set & golden_set
    return {
        "edited": sorted(edited_set),
        "golden": sorted(golden_set),
        "hit": len(hits),
        "precision": len(hits) / len(edited_set) if edited_set else 0.0,
        "recall": len(hits) / len(golden_set) if golden_set else 0.0,
        "solved": len(hits) == len(golden_set),
    }


def summarize(runs, arm):
    rows = [run[arm] for run in runs if run.get(arm)]
    if not rows:
        return {"tasks": 0}
    total = len(rows)
    return {
        "tasks": total,
        "solved": sum(1 for row in rows if row["score"]["solved"]) / total,
        "recall": sum(row["score"]["recall"] for row in rows) / total,
        "precision": sum(row["score"]["precision"] for row in rows) / total,
        "steps": sum(len(row["run"]["steps"]) for row in rows) / total,
        "tokens": sum(row["tokens"] for row in rows) / total,
        "seconds": sum(row["run"]["elapsed"] for row in rows) / total,
    }


def render_markdown(report):
    def pct(value):
        return f"{value * 100:.0f}%"

    lines = [
        f"# CodeGraph agent value: {report['project']}",
        "",
        f"Tasks: {report['tasks']} · max steps: {report['max_steps']} · model: {report['model']}",
        "",
        "| arm | solved | file recall | file precision | steps | tokens | seconds |",
        "|---|---|---|---|---|---|---|",
    ]
    for arm in ("baseline", "codegraph"):
        s = report["arms"].get(arm) or {}
        if not s.get("tasks"):
            continue
        lines.append(
            f"| {arm} | {pct(s['solved'])} | {pct(s['recall'])} | {pct(s['precision'])} | "
            f"{s['steps']:.1f} | {s['tokens']:.0f} | {s['seconds']:.0f} |"
        )
    lines += ["", "## Per task", "", "| task | commit | golden | baseline | codegraph |", "|---|---|---|---|---|"]
    for task in report["per_task"]:
        def cell(arm):
            row = task.get(arm)
            if not row:
                return "skipped"
            score = row["score"]
            return f"{score['hit']}/{len(score['golden'])} files · {len(row['run']['steps'])} steps"
        lines.append(
            f"| {task['id']} | `{task['sha'][:8]}` {task['subject'][:50]} | "
            f"{', '.join(task['golden'])} | {cell('baseline')} | {cell('codegraph')} |"
        )
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------- main

def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--project", type=Path, default=Path.cwd())
    parser.add_argument("--tasks", type=int, default=4, help="how many merged commits to use")
    parser.add_argument("--max-steps", type=int, default=8)
    parser.add_argument("--arm", choices=("both", "baseline", "codegraph"), default="both")
    parser.add_argument("--workdir", type=Path, default=Path("/tmp/opencode/agent-bench"))
    parser.add_argument("--max-tokens", type=int, default=1500, help="per model call")
    parser.add_argument("--markdown", type=Path)
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--command", default=" ".join(mv.DEFAULT_COMMAND))
    parser.add_argument("--keep-worktrees", action="store_true")
    args = parser.parse_args()

    root = args.project.resolve()
    env = mv.load_env_layers(root)
    llm = Llm(env)
    tasks = pick_tasks(root, args.tasks)
    if not tasks:
        raise SystemExit("no small merged commits found to use as tasks")
    print(f"tasks: {len(tasks)}", file=sys.stderr)

    client = None
    if args.arm in ("both", "codegraph"):
        client = mv.Mcp(args.command.split(), root)
        client.initialize()

    runs, per_task = [], []
    try:
        for index, task in enumerate(tasks, start=1):
            task_id = f"t{index:02d}"
            record = {"id": task_id, "sha": task["sha"], "subject": task["subject"],
                      "golden": task["golden"]}
            for arm in (["baseline", "codegraph"] if args.arm == "both" else [args.arm]):
                worktree = args.workdir / f"{task_id}-{arm}"
                make_worktree(root, task["sha"], worktree)
                workspace = Workspace(worktree)
                before = llm.prompt_tokens + llm.completion_tokens
                try:
                    run = run_agent(llm, workspace, task["subject"], arm,
                                    args.max_steps, client if arm == "codegraph" else None)
                    tokens = llm.prompt_tokens + llm.completion_tokens - before
                    result = {"score": score(run["changed"], task["golden"]),
                              "run": run, "tokens": tokens}
                finally:
                    if not args.keep_worktrees:
                        drop_worktree(root, worktree)
                record[arm] = result
                runs.append(record)
                print(f"{task_id} {arm}: {result['score']['hit']}/{len(task['golden'])} files, "
                      f"{len(run['steps'])} steps, {tokens} tokens, {run['elapsed']:.0f}s",
                      file=sys.stderr)
            per_task.append(record)
    finally:
        if client:
            client.close()

    report = {
        "project": str(root),
        "tasks": len(per_task),
        "max_steps": args.max_steps,
        "model": llm.model,
        "arms": {
            "baseline": summarize(per_task, "baseline"),
            "codegraph": summarize(per_task, "codegraph"),
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
