# Repository Guidelines

## 探索协议（强制，先读这一节）

**只要问题涉及本项目源码的结构 / 位置 / 关系 / 调用，第一个工具调用必须是 codegraph（MCP 优先，CLI 兜底）。**
在任何 `ls` / `find` / `glob` / `cat` / `grep` / `read` **之前**执行。

适用范围是**整个任务**，不限于第一轮：探索、验证、调试、修改前定位、排查行为，都算。
「我只是确认一下」「这个我大概知道在哪」**不构成豁免**。

| 问题类型 | 首个调用 |
|---|---|
| 这项目 / 仓库是干什么的 | `agentic_architecture` 或 `agentic_context` |
| 入口在哪 / `main` 在哪 | `semantic_code_search` |
| 架构如何 / 有哪些模块 / 怎么分层 | `get_hub_nodes` + `agentic_architecture` |
| 某功能 / 概念在哪实现 | `semantic_code_search` |
| 谁调用 X / 改 X 影响什么 | `get_reverse_dependencies` |
| 从 X 出发的调用链 | `trace_call_chain` |
| 复杂度 / 耦合 / 风险 | `find_complexity_hotspots` / `agentic_quality` |

**违规示例**（出现即视为违反本节）：

- 用 `ls -la` 或 `ls crates/` 回答「项目结构」
- 用 `glob **/bin/*.rs` 找「入口」
- 用 `cat Cargo.toml` 回答「项目是做什么的」
- 用 `grep -rn "handle_index"` 找函数定义 / 调用点
- 用 `grep` 找 `struct X` / `enum Y` / `impl Z` / `pub fn f` 回答「它在哪、谁用它」

**一条判据：搜符号用 codegraph，搜文本才 grep。**

| 你手里的是 | 用什么 |
|---|---|
| 函数名 / 类型名 / 模块名 / trait 名 / 字段名 / 命令枚举变体 | `semantic_code_search` |
| 字符串字面量 / 日志文案 / 错误信息 / 配置键值 / 文件路径 / 注释原话 | `grep` |

边界例子：

- `grep -rn "CODEGRAPH_SURREALDB_URL"` → 配置键，**合法**
- `grep -rn "analyze_cargo_workspace"` → 函数名，**违规**，应走 `semantic_code_search`
- `grep -rn "exclude_patterns"` → 字段名，**违规**

**「我知道它叫什么」恰恰是应该用 codegraph 的理由，不是用 grep 的理由。**
知道名字只省了「怎么问」，并不改变「这是符号定位」的性质。

**降级到 read / grep 的唯一合法路径**（按顺序核验，不得凭印象判断）：

1. `test -d .codegraph/db` 不存在 → 才算「未索引」
2. 或 codegraph 调用返回空 / 报错 / 与源码明显不符
3. 或确实是**文本**（上表右侧一列）、或**已知精确文件路径**的内容确认

**每条豁免都要申报。** 用 `read` / `grep` / `ls` / `glob` 定位源码时，
同一轮回答里必须写一句 `降级理由: <命中第几条>`。允许把一批合并申报一次，
**不得静默降级**。没有申报的定位调用视为违规，即使结果正确。

`ls` 输出中出现 `.codegraph/` 本身就是「已索引」的证据，不得据此降级。索引滞后只影响刚编辑的文件，
不影响入口 / 架构类问题。

**每轮自检**：本轮有没有 `read` / `grep` / `ls` / `glob` 命中源码位置？
如果有，本轮的 codegraph 调用是哪一次？答不上来即为违规。

### 工具选择（13 个）

**直连图工具（0.3–1.3 秒，无 LLM）** —— 优先用这一层：

| 问题 | 工具 | 必填参数 |
|---|---|---|
| 这个符号 / 概念在哪 | `semantic_code_search` | — |
| 谁调用 / 依赖它 | `get_reverse_dependencies` | `node_id`, `edge_type` |
| 它传递依赖了什么 | `get_transitive_dependencies` | `node_id`, `edge_type` |
| 从它出发的调用链 | `trace_call_chain` | `node_id` |
| 耦合度 | `calculate_coupling_metrics` | `node_id` |
| 循环依赖 | `detect_circular_dependencies` | `edge_type` |
| 复杂度热点 | `find_complexity_hotspots` | — |
| 中心模块 | `get_hub_nodes` | — |
| 校验草稿逻辑图 | `plan_check` | — (`plan` 对象) |

**两条硬规则**：

1. `node_id` 必须原样使用 `semantic_code_search` 返回的 ``nodes:`uuid` ``——保留反引号和前缀，不要自己拼
2. `edge_type` 只能是：`calls` `defines` `uses` `imports` `extends` `implements` `contains` `references`

**`agentic_*`（55 秒–2.5 分钟）** —— 只在需要「跨证据综合解释」时用，不要拿它做单符号查找：
`agentic_context` · `agentic_impact` · `agentic_architecture` · `agentic_quality`

**CLI 等价物（MCP 不可用时）**：

```bash
codegraph query <symbol> --callers              # 对应直连工具，3–5s
codegraph agent context "..." --focus search    # 对应 agentic，22s+
codegraph index . -r --index-tier balanced --languages rust
```

索引稳态 1.3 秒（无变化）至 2.7 分钟（全量 / 换模型）；用共享 SurrealDB server 后，
**索引运行时可以同时查询**，不需要停 MCP。

---

## 环境状态核查（强制）

适用于任何关于「当前配置 / 运行模式 / 进程 / 存储 / 端口 / 生效参数」的回答。
上一节讲「怎么找代码」，本节讲「怎么断言状态」——**两者互不替代**。

### 三条铁律

**1. 缺失 ≠ 不存在。**

从空输出 / 找不到 / `No such file` / 无匹配推出任何结论前，先问：
「采集渠道本身会不会漏？」然后换一条独立路径复核。

典型陷阱：`env | grep CODEGRAPH` 为空，但配置写在 `.env` 里——
`codegraph-core` 自己会加载它（`config_manager.rs` 的 `load_dotenv`），
它**从不进入 shell 环境**。渠道为空不等于事实为空。

**2. 条件句必须验证条件。**

引用注释 / README / `config/example.toml` 作依据时，若原句含
`unless` / `if` / `when` / `except` / `除非` / `如果`，
必须**显式写出该条件成立与否**，不得只引前半句。

典型陷阱：文档写「默认连 A，**除非**设置了 Y」，你只引了「默认连 A」，
却没回答 Y 是否设置——等同于没验证。条件句的措辞会随文档改版变化，
不要背某一句原文，要看**当前**文件里那句是否带条件。

**3. 副作用指令前先看运行态。**

输出 `pkill` / `kill` / `rm` / `dd` / `codegraph index --force` /
`codegraph stop` / 任何写库或改配置的指令前，必须先有**运行态证据**：
`ps aux | grep` / `codegraph status --detailed` / `docker ps`。
配置推断不足以支撑破坏性操作。

本协议自身已有现成依据：见上文「用共享 SurrealDB server 后，
索引运行时可以同时查询，不需要停 MCP」——**下停机指令前先读这一句**。

### 强制输出格式

涉及环境状态的回答，首段必须把三层分离：

```text
观察: <实际执行的命令 + 真实输出>
推断: <由观察推出、但未验证的结论，逐条标 [未验证]>
动作: <指令；仅在观察充分时给出>
```

**推断不得写成指令的形态。** 把未验证假设直接写成 `pkill ...`，即违规。

### 停点

- **矛盾信号**：同一轮内两个源互相打架（例如 `.gitignore` 里列了 `.env`，
  而回答里说「没有本地配置」）时，**停下报告矛盾**，不许静默择一。
- **结论依赖开关**（embedded / server、`index-tier`、feature flag、env 覆盖顺序）时，
  把开关值列为**待验证项**，不许默认取一侧就往下推。

### 本仓库存储模式三连（缺一条不算结论）

```bash
grep -E '^CODEGRAPH_SURREALDB_URL' .env ~/.codegraph.env     # 配置源（项目 .env 覆盖用户文件）
codegraph status --detailed                                  # 运行态
docker ps --filter name=codegraph-db --format '{{.Status}}'   # 服务态
```

---

## Project Structure & Module Organization

CodeGraph is a Rust 2024 workspace (Rust 1.95 or newer) that indexes code into a SurrealDB knowledge graph and exposes MCP and CLI agent tools. Source lives in `crates/codegraph-*/src/`: `core` holds shared types/configuration and artifact caching, `parser` captures sources and extracts syntax, `mcp` reconciles indexing and runs analyzers, `graph` handles storage, and `vector` provides embeddings/search. The CLI is `crates/codegraph-mcp-server/src/bin/codegraph.rs`; the agent backend lives in `codegraph-mcp-rig`.

Unit tests sit alongside modules; integration tests use each crate's `tests/`. Schemas live in `schema/`, configuration examples in `config/`, documentation in `docs/`, and images in `docs/assets/`.

## Build, Test, and Development Commands

Bare Cargo commands target only `codegraph-core`; specify `--workspace` or `-p <crate>`.

**Build through the Makefile targets.** They are the project's single source of truth for
profile, crate, binary and feature flags. Do not re-assemble the underlying `cargo` command
by hand: doing so has already lost the `install` step and left a stale `codegraph` on PATH.
Fall back to a bare `cargo` command only when no target exists, and state why in the reply.

- `make build-dist`: **default local build** (thin LTO, no local/onnx embeddings; output
  `target/release-dist/codegraph`, about 3x faster than fat LTO). Ships every remote
  embedding provider; drop unused ones with
  `DIST_FEATURES="daemon,ai-enhanced,server-http,embeddings-openai"`.
- `make build-dist-openai` / `make install-dist-openai`: shortcut for OpenAI-compatible
  embeddings only (no jina/ollama/lmstudio). Not for `cloud` embeddings, which need
  `embeddings-jina`.
- `make install-dist`: runs `build-dist`, then copies the binary to
  `$(HOME)/.cargo/bin/codegraph`. Use this whenever the `codegraph` on `PATH` must carry
  your changes.
- `make check` (format + lint + tests) / `make dev` (same, for local loops) / `make test`
  (tests only).
- `make lint`: checks only core and suppresses warnings. For CI parity use
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`; strict workspace
  Clippy currently reports pre-existing core/zerocopy diagnostics, so disclose that
  limitation rather than calling a non-strict run lint-clean.

Underlying commands, for reference only (all of the above already wrap them):

- `cargo build --profile release-dist -p codegraph-mcp-server --bin codegraph --features "daemon,ai-enhanced,server-http,embeddings-openai,embeddings-ollama,embeddings-jina,embeddings-lmstudio"`
- `cargo check --workspace`: check all crates without producing binaries.
- `cargo build --release --features full`: published-artifact profile only (fat LTO); `.github/workflows/release.yml` uses it.
- `./target/debug/codegraph start stdio`: run the built MCP server after configuring providers. A shared server is the default target (`ws://127.0.0.1:8000`) and needs `schema/codegraph_v2.surql` applied; only an explicit `surrealkv://` URL opens an embedded per-project store, which applies the bundled v2 schema automatically. Respect `CARGO_TARGET_DIR` when locating binaries.
- `cargo test -p codegraph-mcp`: test the indexing crate; use `cargo test --workspace` for all crates.
- `cargo test -p codegraph-mcp-server --lib` and `cargo test -p codegraph-mcp-server --test project_init_integration --test agent_cli_integration`: verify project setup, hook preservation and CLI contracts without live providers.
- `cargo fmt --all -- --check`: verify formatting; omit `-- --check` to format. `make fmt-check` / `make fmt` wrap these.

## Coding Style & Naming Conventions

Follow `rustfmt.toml`: four spaces, no tabs, 100-column width, and Unix newlines. Use `snake_case` for modules/functions, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Follow existing `ABOUTME` file comments. Keep analyzer output deterministic, scoped by `project_id`, and annotated with provenance.

## Testing Guidelines

Use `#[test]` and `#[tokio::test]` with descriptive snake_case names. Add regression tests for changed behavior. After schema edits, run `cargo test -p codegraph-graph --test schema_indexes_test` and keep both main schemas consistent.

Service tests can return successfully without executing when configuration is absent; `graph_tools_smoke` requires `CODEGRAPH_SURREALDB_URL`. Report these skips. CI collects coverage with `cargo llvm-cov`; no numeric minimum is configured.

## Indexing Invariants & Performance Checks

Full, single-file, delete and watch paths share complete-project reconciliation. Capture immutable source snapshots once, reuse versioned AST/analyzer artifacts and resolve against the full catalog, including unchanged callers. `--force` reconciles without trusting the previous catalog; it does not wipe the project before preparation.

Keep SHA node identities compatible, distinguish real same-line collisions and retain stable occurrence-aware edge IDs. Resolve definitions/exact/contextual and lexical targets before semantic inference; leave ambiguity unresolved and retain resolution provenance. Chunking must preserve Unicode/structural whitespace and enforce the provider's actual tokenizer budget. Cache keys must include source, extraction policy, model/task/revision/tokenizer/runtime identity and relevant build/doc inputs.

Keep fitting AST embedding units intact; split oversized units at real syntax
boundaries and use UTF-8-safe fallback cuts for oversized leaves. Recheck complete
inputs, including task prefixes and special tokens; count overlap with the actual
tokenizer. `CODEGRAPH_EMBEDDING_SKIP_CHUNKING=1` must reject oversized nodes, not
silently split/truncate them. Ollama resolves model/serving context and matching
publisher tokenizers in `codegraph-vector::input_policy`; every request disables
truncation. Unknown tokenizers require an explicit path/repository. Changes to this
policy must invalidate chunk, prepared-vector and reconciliation caches. Provider
initialization/chunk-planning APIs return `Result`; preserve error propagation.
Use isolated fixtures and mock HTTP services to validate these contracts.

Jina indexing resolves `JINA_API_TASK`, legacy `JINA_TASK`, TOML `jina_task`, then
the model-aware `auto` passage default, in that order. Preserve explicit tasks;
pair passage/query tasks without hardcoding v4's `code.query` for v5. Batch, query
and health requests share the payload builder. Send `normalized` only for models
that support it; include actual task/request options in cache identities. Known
invalid task/model pairs fail initialization; permanent embedding HTTP errors
(including 422) must not retry. Offline Jina provider tests and
`index_cli_integration` cover payloads, dotenv precedence and transient retries.

Semantic scoring caches raw-vector norms and parallelizes independent targets in the shared worker-limited CPU pool through `spawn_blocking`. Preserve candidate order, scalar floating-point summation/division, the 0.75 threshold and 1e-6 tie rule; vector normalization or parallel dot-product reductions can change decisions. `semantic_scoring` tests and the offline `semantic_scoring_benchmark` compare node IDs and score bits against the frozen scalar reference. Resolution timings in `phase_ms` separate matching, inference, scoring and writes.

Bound parser workers, retained source bytes, queued writer payloads and provider concurrency independently. Database errors and flush acknowledgements must propagate. Reconcile stale project-scoped records and validate final inputs before persisting readiness. Graph completion, pending/off inference and vector-index readiness are separate states; do not mark a failed or deferred stage complete.

For indexing batches, explicit `--batch-size` overrides `CODEGRAPH_EMBEDDINGS_BATCH_SIZE`, legacy `CODEGRAPH_EMBEDDING_BATCH_SIZE`, TOML `[embedding] batch_size`, then the default 64. Resolve environment defaults once, preserve explicit positive values (including 100), and pass the same row limit to the indexer, provider and submitted-text cache. Do not silently cap Ollama/LM Studio at 256. Token/byte/provider limits and smaller DB write batches remain independent; report their limits accurately. Verify actual HTTP request sizes with the loopback mock in `index_cli_integration`, without querying live providers or the working index.

Rust LSP preflight must verify `rust-analyzer --version` in the target project, including rustup shims; presence on PATH is insufficient. Start language servers in that project directory, preserve a bounded stderr tail in failure reports and replace stopped pooled sessions. Retry only `ContentModified` (`-32801`) for symbol/definition reads, with fresh IDs, unchanged document versions and at most five retries within one 30-second deadline. Propagate cancellation, other errors, changed versions and exhausted retries. Test startup/crash/retry handling with isolated mock servers and `index_cli_integration` without installing toolchains or indexing the working repository.

`CODEGRAPH_EMBEDDING_POLICY` and `CODEGRAPH_SEMANTIC_RESOLUTION` accept `sync|deferred|off` independently of tier. Resume persisted inference with `codegraph index <root> --complete-deferred`. Precision, candidate limits, text-splitter and SCIP choices remain opt-in. See [implementation controls](docs/indexing-performance-work.md) and [speed/quality probes](docs/indexing-benchmarks.md); debug fixture timings are not production speedup claims.

Useful offline regressions: `cargo test -p codegraph-mcp --lib`, `cargo test -p codegraph-mcp --test reconciliation_test --test daemon_watch`, `cargo test -p codegraph-mcp --features embeddings --test deferred_policy_test`, `cargo test -p codegraph-graph --features surrealdb --test ingestion_write_test`, and `python3 -m unittest discover -s scripts/benchmarks -p 'test_*.py'`. Check the full CLI features after provider/runtime changes. Strict workspace Clippy currently reports existing core/zerocopy diagnostics; disclose that limitation instead of calling a non-strict run lint-clean.

### Generate SCIP before indexing (do not just run `codegraph index`)

`codegraph index` drives one language-server request per file by default: ~21 minutes on a
10k-file project. Generate a SCIP index first and import it instead:

```bash
export CODEGRAPH_SCIP_INDEX="$(scripts/scip-index.sh --export)"   # rust / TS / Go, per language
codegraph index . --index-tier balanced
```

Measured on a 10k-file TypeScript project: analysis 21 min → **2.4 s + 3.9 s import**, plus
62,711 definition edges the LSP path never produced. `scripts/scip-index.sh` picks the
indexer per language and writes `.codegraph/scip/`; for an index built inside a
subdirectory, use the `@subdir` form (`go.scip@easytier-go`). A missing index for a language
means that language gets **no** enrichment, because setting `CODEGRAPH_SCIP_INDEX` disables
language servers for the whole run. See `docs/INDEXING_PIPELINE.md` and `TODO.md`.

## Commit & Pull Request Guidelines

Follow the history's Conventional Commit pattern, e.g. `fix(lsp): prevent premature process kill`. Use semantic PR titles and `.github/PULL_REQUEST_TEMPLATE.md`. Explain motivation, link issues, report validation and performance impact, and update affected documentation. Include screenshots when relevant.

## Security & Configuration

Use `.env.example` and `config/example.toml` as references. Keep credentials in ignored local configuration; never commit API keys or database passwords.

`codegraph init [project]` selects project-local Claude/Codex hooks before merging both agent instruction files and indexing. `--hooks claude|codex|both|none` supports scripts; `--no-index` performs provider-independent setup. Keep user-level settings untouched, preserve unrelated hooks/instructions, and retain idempotent managed blocks. See [init and CLI usage](docs/AGENTIC_CLI.md).

The four CLI agent commands have a 600-second whole-workflow deadline, including setup, graph calls and model responses; preserve explicit `--timeout-secs` overrides. CLI/HTTP evaluation cases share that budget, and the HTTP stream-read limit must accommodate it. Keep historical accuracy entries tied to their recorded deadlines; offline fixtures do not establish live answer accuracy.

ReAct and LATS share the eight-tool registration in
`codegraph-mcp-rig/src/tools/factory.rs`. LATS candidates execute native tool loops,
retain branch-local transcripts and supply actual observations to the critic.
Never treat candidate prose, failed tools or exhausted-budget notes as graph
grounding, or return a root-level intermediate step in place of the selected
answer. Preserve shared result budgets, project-scoped caching, tool counts and
traces across candidates. The tier's turn budget bounds search expansions/depth
and each candidate loop, rather than the whole tree's model-call count. Verify
with `cargo test -p codegraph-mcp-rig --all-features --lib` and the full-feature
`agent_cli_integration` mock tests before making runtime accuracy claims.

> **以下 `codegraph:begin/end` 块由 `codegraph init` 自动维护**，只讲 CLI 用法。
> 它与上文「探索协议」重复，**冲突时以本节上方的探索协议为准**（那里是 MCP 工具，更快）。
> 该提示刻意放在块**外**：块内内容每次 `init` 都会被模板整块替换掉。

<!-- codegraph:begin -->
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
<!-- codegraph:end -->

### 何时必须先用 `plan`（规划校验）

写代码**之前**，只要命中下面任一条，就先外化节点/边/约束并调用 `plan_check`（离线）或
`codegraph plan check`：

- 新增模块 / crate 级特性 / provider-factory 路径；
- 跨层改动（例如 `cli -> core -> vector`），或要改变这个方向；
- 多跳依赖（A 调 B、B 调 C，或新类型在别处实现某个 trait）；
- 同时改动 ≥3 个文件的公共接口（签名、trait 方法、配置键、schema）。

单文件 bug fix、改文案、单模块内机械重命名**不用** plan——那只是复述代码图里已有的东西。

调用要点：节点 = 模块 / 类型 / trait（`id` 用你打算写的真实符号名，`plan verify` 按名字比对），
边 = `calls` / `implements` / `uses`，约束 = `acyclic` + `layering` + `coverage`（必要时
`no_path` / `max_fan_*`）。`known_nodes` 抓未声明的引用，`layering` / `acyclic` 抓设计错误。

MCP 的 `plan_check` 是**离线**校验，不读代码图；要和已有代码对照，用 CLI：
`codegraph plan --project <已索引的项目根> verify plan.json`（看 `unplanned_code_edges`
和 `existing_planned_nodes`）。详细说明见 `docs/PLAN_CHECK.md`。

### 部分索引与 LSP（默认关闭，按需打开）

`codegraph index <scan_root>` 只更新该范围，**默认不启动语言服务器**（LSP 会加载整个工程，
单文件更新不值得付全工程导入的代价）。

需要跨文件符号 / 定义时，显式打开并固定项目根：

```bash
CODEGRAPH_LSP_ON_PARTIAL=1 codegraph index packages/happy-android \
  --project-root . --index-tier balanced --languages kotlin
```

- `--project-root`（或 `CODEGRAPH_PROJECT_ID`）必须与索引时的项目一致；扫描路径（如
  `packages/happy-android`）相对该根书写。
- LSP 以**项目根**初始化，不是扫描目录；大 Android / Java / TS 仓库首次导入可能 1–5 分钟。
  `initialize` 默认等待 180s（`CODEGRAPH_LSP_INITIALIZE_TIMEOUT_SECS`），普通请求 30s
  （`CODEGRAPH_LSP_REQUEST_TIMEOUT_SECS`）；超时会降级为 warning，索引继续。
- 需要的工具：Kotlin=`kotlin-language-server`，Java=`jdtls`，TS/JS=`typescript-language-server`，
  Rust=`rust-analyzer`，Go=`gopls`。
- 大仓建议先用全量 `codegraph index <root>` 跑一次带 LSP 的索引，之后日常编辑用不带 LSP 的部分索引。
- **tier 决定有没有 LSP 边**：`balanced` = `SymbolsOnly`，只做 `documentSymbol`，**不解析 definition，LSP 贡献 0 条边**；
  只有 `full` = `SymbolsAndDefinitions` 才会产生 definition 边。balanced 日志里的
  `N nodes enriched + 0 edges resolved` 是预期行为，不是失败。需要 balanced 也有边时设
  `CODEGRAPH_LSP_DEFINITIONS=1`（更慢）。

### 共享 server 的数据库隔离

共享 SurrealDB 默认**每个项目一个 database**（`<目录名>_<路径短哈希>`，如 `happy_3f9c2a1b7e40`），
避免所有项目的 chunks 共用一个 HNSW 索引（构建会随全库增长）。首次 index/query/MCP
连接会自动建库并应用 schema。`CODEGRAPH_DATABASE_PER_PROJECT=0` 可回退到共享
`CODEGRAPH_SURREALDB_DATABASE`。**旧共享库的数据不会自动迁移**，需要重新索引各项目；
确认不再需要旧库前不要删除它。嵌入式 `surrealkv://` 本身按项目一个文件，不受影响。

