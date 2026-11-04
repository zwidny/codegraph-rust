# codegraph

CodeGraph is a CLI tool available through Bash as the `codegraph` command. Run the
commands below using your Bash or shell execution tool. Check that CodeGraph is
on PATH with `command -v codegraph`.

When CodeGraph is available and the project is indexed, start code exploration
with its CLI agent commands. Ask a specific question about the task, relevant
symbols or paths instead of starting with broad grep/rg searches:

- `codegraph agent context "Find the implementation and callers for <task>" --focus search`
  locates code; use `--focus builder` to gather implementation context or `question`
  to explain behavior.
- `codegraph agent impact "What depends on <symbol> and what would <change> affect?"`
  checks dependencies before editing; `--focus call_chain` follows call flows.
- `codegraph agent architecture "Describe <area> and its interfaces"`
  maps structure; `--focus api_surface` inspects public interfaces.
- `codegraph agent quality "Assess coupling, complexity and risks in <area>"`
  supports refactoring decisions and targeted follow-up checks.

Run from the indexed project root, or append `--project /path/to/project`; retain
the indexed `--project-id` if one was configured. Prefer the default JSON output:
inspect source locations, findings and partial-result warnings, then read the
specific files/lines before editing. Reuse useful findings and narrow follow-up
questions rather than repeating broad queries. After changes, verify against
current source and run relevant tests; the index may lag uncommitted work.

If CodeGraph is unavailable, the project is not indexed, a command fails or returns
insufficient evidence, fall back to targeted source reads and rg/grep.

**Trigger.** If the question touches this repository's source — structure, location,
relationship, call flow, or behavior — the first tool call must be CodeGraph, and it
applies to the whole task (exploration, verification, debugging, locating code before
an edit). "I just want to confirm" or "I roughly know where it is" is not an exemption.

Decide by what you hold, not by how fast grep feels:

- Symbol - a function, type, module, trait, field or command name -> use codegraph.
  `grep "analyze_cargo_workspace"` is a symbol lookup, so it is not a fallback.
- Text - a string literal, log line, error message, config key, file path or a
  verbatim quote from a comment -> grep is correct. `grep "SURREALDB_URL"` is text.
- Knowing the name already is the reason to use codegraph, not the reason to grep.
- Every fallback used to locate source must state why in the same reply; an unstated
  fallback is a violation even when the result is correct.
- Fallback is allowed only when the project has no `.codegraph/db`, or the command
  fails / returns empty / visibly disagrees with the source, or what you hold is text
  or a known exact file path.
- Per-turn self-check: if this turn's reads, greps or listings landed on a source
  location, name the CodeGraph call that preceded them.

Do not install CodeGraph, download models or reindex solely to satisfy these
instructions. Agent queries use the configured model and may incur provider costs.
Use the four public agent commands; internal graph tools belong to CodeGraph's
built-in agents. Reload usage details with `codegraph agent instructions`.
