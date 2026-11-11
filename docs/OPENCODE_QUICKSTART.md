# CodeGraph × OpenCode 快速接入指南

面向「新电脑 / 新项目」的端到端流程：装好 CodeGraph，索引一个项目，并让 OpenCode
通过 MCP 用上 13 个图工具。

> 本文只讲接入与验证。性能调优见 [README_DEV_GUIDE](../README_DEV_GUIDE.md)，
> 完整参数见 [INSTALLATION_GUIDE](INSTALLATION_GUIDE.md)。

---

## 1. 前置服务

CodeGraph 需要两样外部能力：

| 能力 | 用途 | 配置键 |
|---|---|---|
| Embedding 服务（本地或云端） | 索引期与查询期的向量化 | `CODEGRAPH_EMBEDDING_PROVIDER`、`OPENAI_API_BASE`、`CODEGRAPH_EMBEDDING_MODEL` |
| SurrealDB | 存储图谱；**推荐 server 模式** | `CODEGRAPH_SURREALDB_URL` 等 |

嵌入式 SurrealKV 是排他文件锁：MCP 运行时无法索引。server 模式可以并发读写，
也能让索引期间的查询照常工作。

```bash
docker run -d --name codegraph-db -u 0:0 --restart unless-stopped \
  -p 8010:8000 -v cg-server-data:/data \
  surrealdb/surrealdb:v3.3.0 \
  start --user root --pass root --bind 0.0.0.0:8000 surrealkv:/data

# 首次或重建后应用 schema（v2 是当前主 schema）
curl -s -X POST http://127.0.0.1:8010/sql -u root:root \
  -H "surreal-ns: codegraph" -H "surreal-db: codegraph" \
  --data-binary @schema/codegraph_v2.surql
```

配置优先级：

```
进程环境变量 > 项目 .env > ~/.codegraph.env > .codegraph.toml
```

共享项（embedding / LLM / 资源约束）放 `~/.codegraph.env`，项目级只写差异项。

---

## 2. 构建与安装

MSRV 是 1.95；系统需要 `libssl-dev pkg-config`。

```bash
# 默认本地构建：thin LTO，去掉 local/onnx embedding（比 fat LTO 快约 3 倍）
cargo +1.97.1 build --profile release-dist -p codegraph-mcp-server --bin codegraph \
  --features "daemon,ai-enhanced,server-http,embeddings-openai,embeddings-ollama,embeddings-jina,embeddings-lmstudio"
ln -sf "$PWD/target/release-dist/codegraph" ~/.local/bin/codegraph
command -v codegraph
```

发布产物用 `cargo build --release --features full`（fat LTO），由 `.github/workflows/release.yml` 执行。

`--features full` 才有 agent、embedding 与 HTTP 能力。

LSP 依赖按语言安装；`balanced` / `full` tier 缺工具会 **fail fast**（不是降级）：

| 语言 | 需要 |
|---|---|
| Rust | `rustup component add rust-analyzer` |
| Python | `npm i -g pyright` |
| TS/JS | `typescript-language-server` + `node` |
| Go / Java / C-C++ | `gopls` / `jdtls` / `clangd` |

---

## 3. 新项目接入

两种方式，任选其一。

**方式 A：一体化**

```bash
cd /path/to/project
codegraph init . --hooks none --index-tier balanced
```

`init` 会先装项目级 harness（hooks + 往 `AGENTS.md`/`CLAUDE.md` 加 CodeGraph 指引），
再索引。`--hooks claude|codex|both|none` 显式选择，`--no-index` 只做准备工作。

**方式 B：先看规模再索引**

```bash
codegraph estimate . -r --languages rust --index-tier balanced
```

`estimate` 同时给出 remote / local 两种吞吐场景，并按 `CODEGRAPH_EMBEDDING_PROVIDER`
标注哪条对应你的配置；它只做预估，不会切换或选择 provider。

```bash
codegraph index . -r --index-tier balanced --languages rust \
  > /tmp/cg-index.log 2>&1 &
tail -f /tmp/cg-index.log
```

日志要落文件，不要管道接 `tail`（会缓冲到结束）。

---

## 4. OpenCode 接入（MCP）

写在**项目级** `<project>/.opencode/opencode.json`：

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "codegraph": {
        "type": "local",
        "command": ["/home/<you>/.local/bin/codegraph", "start", "stdio"],
        "cwd": "/path/to/project",
        "enabled": true
      }
    }
  }
}
```

四条硬性要求，任何一条写错都会静默失败：

| 要求 | 错误做法 | 症状 |
|---|---|---|
| 用 `mcp.servers.<name>` | 写成 `mcp.<name>` | local 类型不识别，工具不出现 |
| 必须有 `"enabled": true` | 省略 | 静默不加载 |
| 放**项目级**配置 | 放 `~/.config/opencode/opencode.json` | 每个项目各起实例，未索引项目连累整体 |
| `"command"` 是数组 | 写成字符串 | 解析失败 |

> 全局配置里再写一份 `mcp.servers.codegraph` 会让同一个项目起两个 stdio 实例。
> 项目级 + 全局同时存在时，删掉全局那一份。

验证：

```bash
opencode mcp list        # 期望 codegraph  connected
```

连上后，MCP 在 `initialize` 时下发使用说明（`initial_instructions.md`），
前提是**重启 OpenCode**——说明只在建立连接时读取。

---

## 5. 验证清单

```bash
# 1) 确认存储目标：server 模式（推荐）写在 CODEGRAPH_SURREALDB_URL，
#    本地 .codegraph/db 只在显式 surrealkv:// 的 embedded 模式下才存在
echo "${CODEGRAPH_SURREALDB_URL:-ws://127.0.0.1:8000 (default)}"

# 2) 直连查询（无 LLM，秒级）；与 index/agent 读同一套存储，不会私自退回本地 .codegraph/db
codegraph query <真实符号> --callers

# 3) agent 查询（20–60 秒，需 LLM 配置）
codegraph agent context "Where is configuration loaded?" --focus search

# 4) OpenCode 的 MCP 连接
opencode mcp list        # 期望 codegraph  connected
#    然后在 OpenCode 里让 agent 调一次 semantic_code_search，能返回 node_id 即通
#    可用工具应为 13 个：8 个直连图工具 + 4 个 agentic_* + plan_check（校验草稿逻辑图）
```

`semantic_code_search` 返回的 `node_id` 要原样使用（`nodes:<uuid>`，含反引号与前缀）。
`threshold` 默认 0.4；自然语言查询召回为空就先降到 0.2–0.3，或改用符号名查询。

---

## 6. 故障对照

| 现象 | 原因 | 处理 |
|---|---|---|
| OpenCode 里没有 codegraph 工具 | 配置结构错 / 没重启 | 对照第 4 节四条要求，重启 OpenCode |
| 同一个项目有两个 `codegraph start stdio` | 全局 + 项目级各配一份 | 删全局 `mcp.servers.codegraph` |
| 语义检索返回 `~/.cargo/registry` 内容 | 旧版未过滤外部依赖节点 | 重建二进制并重启 MCP |
| 自然语言查询返回空 | `threshold` 过高 | 显式传 `threshold: 0.25` |
| 索引报“database locked” | 嵌入式模式下有进程占用 | 改用 SurrealDB server，或先停 MCP |
| `balanced`/`full` 直接失败 | 缺 LSP 工具 | 装工具，或 `--index-tier fast` |
| 每次索引都重算全部向量 | `CODEGRAPH_MODEL_REVISION` 未固定 | 设为固定值（如 `1`），一次性全量后稳定 |
