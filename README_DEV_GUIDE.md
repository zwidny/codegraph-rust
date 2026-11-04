# CodeGraph 开发指南（本项目实测版）

> 本文档记录 **codegraph-rust 仓库自身**接入 CodeGraph 的完整流程、调优结论和踩坑记录。
> 所有性能数字来自本机实测（RTX 4060 Laptop 8GB / 16 核 / SurrealKV 嵌入式存储）。
> 最后更新：2026-11-03

---

## 1. 它是什么

CodeGraph 把仓库解析成 **符号 + 边的知识图谱**，再给每个符号和 chunk 生成向量，最后通过 4 个高层工具提供给编码 agent：

| 工具 | 回答什么问题 |
|---|---|
| `agent context` | 实现位置、调用者、解释行为、为任务收集上下文 |
| `agent impact` | 改动影响面、调用链 |
| `agent architecture` | 模块边界、公开 API 面 |
| `agent quality` | 耦合、复杂度、风险热点 |

数据流：

```
源码 → 文件收集 → LSP 符号解析 → 富化(rustdoc/API) → 模块/文档链接
     → tree-sitter AST → 节点+边持久化 → 节点向量 → chunk 向量
     → HNSW 索引构建 → 语义消解
```

本仓库规模：**238 个 Rust 文件 / 9.2 万行 / 26,009 节点 / 51,892 边**。

---

## 2. 环境依赖

### 2.1 必须运行的服务

| 服务 | 地址 | 用途 |
|---|---|---|
| **SurrealDB server** | `ws://127.0.0.1:8010` | 存储（支持并发读写，**推荐**） |
| infinity embedding | `http://127.0.0.1:8766/v1` | 索引期和查询期的向量化 |
| DeepSeek API | `https://api.deepseek.com/v1` | agent 的推理后端 |

启动 SurrealDB server（**推荐**，嵌入式是排他锁，无法并发）：

```bash
docker run -d --name codegraph-db -u 0:0 --restart unless-stopped \
  -p 8010:8000 -v cg-server-data:/data \
  surrealdb/surrealdb:v3.3.0 \
  start --user root --pass root --bind 0.0.0.0:8000 surrealkv:/data

# 首次/重建后应用 schema
curl -s -X POST http://127.0.0.1:8010/sql -u root:root \
  -H "surreal-ns: codegraph" -H "surreal-db: codegraph" \
  --data-binary @schema/codegraph_v2.surql
```

启动 embedding 服务：

```bash
docker compose -f /home/zhao/repos/mth/mempalace_mth/docker/f2llm-infinity/compose.yaml up -d
curl -s http://127.0.0.1:8766/v1/models   # 期望：F2LLM-v2-0.6B
```

服务端硬约束（务必了解）：

```
--max-sequence-length 8192     单条上限，超出直接报错
--max-batch-tokens 8192        单请求总 token 上限
--auto-truncate false          不静默截断，超限即失败
--max-client-batch-size 64     单请求最多 64 条
```

### 2.2 LSP（`balanced` / `full` tier 必需）

| 语言 | 依赖 |
|---|---|
| Rust | `rust-analyzer`（必须在项目目录内能 `rust-analyzer --version`） |
| Python | `pyright-langserver` + `node` |
| TS/JS | `typescript-language-server` + `node` |

安装：

```bash
rustup component add rust-analyzer
npm i -g pyright
```

缺工具时 `balanced`/`full` 会 **fail fast**（不是降级）。

### 2.3 构建

项目 MSRV 是 **1.95**，本机默认 stable 是 1.93，必须显式指定：

```bash
cargo +1.97.1 build -p codegraph-mcp-server --bin codegraph --features full
ln -sf "$PWD/target/debug/codegraph" ~/.local/bin/codegraph
```

系统需要 `libssl-dev pkg-config`（`reqwest` + `tokio-tungstenite` 走 native-tls）。

---

## 3. 配置

### 3.1 文件优先级

```
进程环境变量  >  项目 .env  >  ~/.codegraph.env  >  .codegraph.toml
```

项目 `.env` 先加载，`~/.codegraph.env` 只补项目没写的键（不会覆盖）。共享配置放
`~/.codegraph.env`，项目级只写差异项；进程里已导出的同名变量优先级最高。

### 3.2 当前生效配置（本仓库 `.env`）

```bash
# ---- Embedding ----
CODEGRAPH_EMBEDDING_PROVIDER=openai
OPENAI_API_BASE=http://127.0.0.1:8766/v1
OPENAI_API_KEY=local-gateway-no-key
CODEGRAPH_EMBEDDING_MODEL=F2LLM-v2-0.6B     # 注意：无前导斜杠
CODEGRAPH_EMBEDDING_DIMENSION=1024
CODEGRAPH_CHUNK_MAX_TOKENS=8000             # 留余量，服务端上限 8192
CODEGRAPH_EMBEDDINGS_BATCH_SIZE=64          # 对齐服务端 max-client-batch-size
CODEGRAPH_EMBEDDING_BATCH_TOKENS=8192
CODEGRAPH_PROVIDER_CONCURRENCY=2

# ---- 存储：共享 SurrealDB server（强烈推荐）----
# 嵌入式 SurrealKV 是排他文件锁：MCP/只读进程占着库时无法索引。
# server 模式同时解决「只读不独占」和「写路径 fsync 开销」：
#   首次索引 604s → 164s，稳态 7s → 1.3s，且索引运行中可并发查询。
CODEGRAPH_SURREALDB_URL=ws://127.0.0.1:8010
CODEGRAPH_SURREALDB_NAMESPACE=codegraph
CODEGRAPH_SURREALDB_DATABASE=codegraph
CODEGRAPH_SURREALDB_USERNAME=root
CODEGRAPH_SURREALDB_PASSWORD=root

# ---- Agent LLM ----
CODEGRAPH_LLM_PROVIDER=openai-compatible
CODEGRAPH_OPENAI_COMPATIBLE_URL=https://api.deepseek.com/v1
CODEGRAPH_LLM_MODEL=deepseek-flash
CODEGRAPH_CONTEXT_WINDOW=1048576            # >500k → Massive tier

# ---- 索引策略 ----
CODEGRAPH_INDEX_TIER=balanced
CODEGRAPH_VECTOR_INDEX_MODE=deferred        # 写入零索引维护，末尾批量建
CODEGRAPH_SEMANTIC_RESOLUTION=deferred      # 符号消解延后
CODEGRAPH_HNSW_EFC=64                       # 150→64，构建快 2.3×
# ⭐ 最重要的单项配置：默认 epoch = now/3600，跨小时会让全部 chunk hash 失效，
# 导致每次索引都重算 2.6 万向量（embedding 固定 15 分钟）。设置后缓存 key 恒定，
# 只有内容真正变化的节点会重算。首次设置会因 namespace 变化触发一次全量重算
#（约 16 分钟），属于一次性成本；之后稳态见 §7。
CODEGRAPH_MODEL_REVISION=1
CODEGRAPH_VECTOR_INDEX_TIMEOUT_SECS=1800    # compaction 可能很久
CODEGRAPH_CHUNK_DB_BATCH_SIZE=256           # 32→256，事务/fsync 减 8×
CODEGRAPH_SYMBOL_DB_BATCH_SIZE=128
# 节点持久化批（默认=embedding 批 64 → 26k 节点=406 个事务=406 次 fsync，实测
# 137 秒）。加到 512 后实测 5 秒（27×）。UPSERT 走 array::map+INSERT，无深度限制。
CODEGRAPH_NODE_DB_BATCH_SIZE=512
```

### 3.3 改配置的代价（重要）

以下变量在**项目指纹清单**里（`indexer.rs` 的 `policy` map），改动会触发**全量重建**：

```
CODEGRAPH_CHUNK_MAX_TOKENS / CHUNK_OVERLAP_TOKENS / CHUNK_SMART_SPLIT / CHUNK_SPLITTER
CODEGRAPH_EMBEDDING_SKIP_CHUNKING / *_PREFIX / TOKENIZER_*
CODEGRAPH_ANALYZERS / ANALYZERS_REQUIRE_TOOLS
CODEGRAPH_VECTOR_INDEX_MODE          ← 踩过
CODEGRAPH_PROJECT_ID / ORGANIZATION_ID / REPOSITORY_URL / DOMAIN
RUSTFLAGS / CARGO_BUILD_TARGET / RUSTUP_TOOLCHAIN
```

以下**安全**（可随时改）：

```
CODEGRAPH_EMBEDDINGS_BATCH_SIZE / EMBEDDING_BATCH_TOKENS / PROVIDER_CONCURRENCY
CODEGRAPH_SEMANTIC_RESOLUTION / VECTOR_INDEX_TIMEOUT_SECS
CODEGRAPH_CHUNK_DB_BATCH_SIZE / SYMBOL_DB_BATCH_SIZE / NODE_DB_BATCH_SIZE
CODEGRAPH_HNSW_EFC / HNSW_M
CODEGRAPH_MODEL_REVISION（注意：首次设置触发一次全量重算）
CODEGRAPH_LLM_* / OPENAI_* / CONTEXT_WINDOW
```

---

## 4. 索引流程

### 4.1 首次索引

```bash
# 1. 本项目用共享 SurrealDB server（§3.2），索引与 MCP 可并发，无需停任何进程
#    （仅当退回嵌入式 SurrealKV 时才必须先停掉占用库的进程）
pgrep -x codegraph && echo "INFO: 有 codegraph 进程在跑（server 模式下可共存）"

# 2. 确认 LSP 可用
rust-analyzer --version

# 3. 估算（不写库、不调 LLM）
codegraph estimate . -r --languages rust --index-tier balanced

# 4. 索引（日志落文件，不要用 | tail，会缓冲到结束）
codegraph index . -r --index-tier balanced --languages rust \
  --stats-json /tmp/codegraph-stats.json > /tmp/codegraph-index.log 2>&1 &

# 5. 观察进度
tail -f /tmp/codegraph-index.log
docker logs --since 60s f2llm-infinity 2>&1 | grep -c Success   # embedding 批次
```

### 4.2 增量索引（日常）

改了代码后直接重跑同一条命令。缓存命中，通常 **2–5 分钟**。

### 4.3 全量重建

```bash
# 只删数据库，保留向量缓存（关键：index-cache 里有几万个向量）
rm -rf .codegraph/db

codegraph index . -r --index-tier balanced --languages rust
```

**不要删 `.codegraph/index-cache/`** —— 那是向量缓存（本仓库约 780MB），删了要重算全部向量。

需要删全部时才：

```bash
rm -rf .codegraph    # 连缓存一起删，仅当换 embedding 模型/维度时
```

### 4.4 deferred 补跑

`SEMANTIC_RESOLUTION=deferred` 会让索引标记 `complete=false`，语义消解留待补跑：

```bash
codegraph index . --complete-deferred
```

### 4.5 排除第三方代码

`vendor/`、`node_modules/`、`target/` 必须排除，否则会被当成本项目代码索引：

```bash
# 优先写进 .gitignore（索引器尊重它）
echo "vendor/" >> .gitignore

# 或命令行临时排除
codegraph index . -r --exclude vendor --exclude target
```

### 4.6 索引阶段与耗时（本仓库实测）

分两种情况（26k 节点规模实测）：

**首次 / 全量（约 20 分钟）**：

| 阶段 | 耗时 | 说明 |
|---|---|---|
| 文件收集 | <1s | |
| rust-analyzer 启动 | 3–36s | 冷启动 36s，热启动仅 3s |
| LSP + 富化 + 链接 | ~3s | |
| tree-sitter AST | 2–37s | 有缓存 2s |
| 节点持久化 | **5s** | `NODE_DB_BATCH_SIZE=512` |
| **节点+chunk 向量** | **~7 min** | 26k × 1024 维 F2LLM-0.6B 本地推理（1.7B 时约 16 min） |
| symbol 向量 + 关系解析 | ~2.5 min | `deferred`，解析已 rayon 并行（197s→110s） |
| HNSW 构建（deferred 末尾） | 1–3 min | `EFC=64` |
| reconcile | **2.2s** | 单次扫描 |

**稳态（无文件变化，约 7 秒）**：

| 阶段 | 耗时 |
|---|---|
| input_validation | 743ms |
| cache_eviction | 604ms |
| source_capture | 105ms |
| embedding / reconcile | **0（全缓存命中）** |

---

## 5. 查询

### 5.1 三层入口（按延迟排序）

| 入口 | 延迟 | 适用 |
|---|---|---|
| **MCP 直连图工具（8 个）** | **0.3–1.3s** | 日常：找代码、调用者、依赖、热点 |
| `codegraph query`（CLI） | 3–5s | 脚本、无 MCP 时 |
| `codegraph agent *` / MCP `agentic_*` | **55s–2.5min** | 需要跨证据综合解释的问题 |

### 5.2 `codegraph query`（CLI 直连）

```bash
codegraph query build_chunk_plan                # 符号定位（精确 + 前缀）
codegraph query build_chunk_plan --callers      # 谁调用它
codegraph query EmbeddingPayload --callees      # 它调用谁
codegraph query reconcile_catalog --callers --limit 50
```

### 5.3 MCP 直连图工具（推荐）

8 个工具见 §6.2。典型编排：

```
semantic_code_search("概念")                    → node_id + file:line
get_reverse_dependencies(node_id,"calls",1)     → 谁依赖它
get_transitive_dependencies(node_id,"calls",2)  → 传递闭包
trace_call_chain(node_id,3)                     → 调用链
```

`node_id` 必须原样使用 `semantic_code_search` 返回的 ``nodes:`uuid` ``（含反引号与前缀）。
`edge_type` 取值：`calls` `defines` `uses` `imports` `extends` `implements` `contains` `references`。

### 5.4 agent 命令（慢，但能综合）

```bash
codegraph agent context "Where is build_chunk_plan implemented?" --focus search
codegraph agent impact  "What breaks if config precedence changes?" --focus call_chain
codegraph agent architecture "Describe the indexer interfaces"    --focus api_surface
codegraph agent quality "Coupling risks in the parser"            --focus coupling
```

实测成本：

```
--focus search       ≈ 9 步   22 秒（CLI）/ 55 秒（MCP，进程常驻但仍走 LLM）
--focus dependencies ≈ 35 步  38 秒
--focus question     可能触发 MaxTurnsError(8 轮上限)，REFLEXION 重试 ×3
```

### 5.5 输出约定

默认 JSON：`analysis_type` / `answer` / `findings` / `steps_taken` / `tool_use_count`。

`OK` 只表示拿到了答案，**不代表准确**——必须打开 `findings` 里的位置核对源码。
行号通常有 1–2 行偏差。

---

## 6. 与编码 agent 集成（MCP）

### 6.1 配置（项目级，唯一可用组合）

```json
// <project>/.opencode/opencode.json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "codegraph": {
        "type": "local",
        "command": ["/home/zhao/.local/bin/codegraph", "start", "stdio"],
        "enabled": true
      }
    }
  }
}
```

**四条硬性要求**（每一条都曾导致加载失败）：

| 要求 | 错误做法 | 症状 |
|---|---|---|
| 用 `mcp.servers.<name>` | 写成 `mcp.<name>` | local 类型不识别，工具不出现 |
| 必须有 `"enabled": true` | 省略 | 静默不加载 |
| 放在**项目级**配置 | 放全局 | 为每个打开的项目起实例，未索引的失败导致整体下架 |
| `"command"` 是数组 | 写成字符串 | 解析失败 |

验证：

```bash
opencode mcp list            # 期望 codegraph  connected
```

> 注意：`opencode mcp add --global` 写入的是 `mcp.servers`（正确结构），但写到全局会触发多实例问题。用项目级文件手写最稳。

### 6.2 12 个工具

**8 个直连图工具（无 LLM，0.3–1.3s）**

| 工具 | 必填参数 | 用途 |
|---|---|---|
| `semantic_code_search` | — | 向量检索，返回 `node_id` + `file:line` |
| `get_reverse_dependencies` | `node_id`,`edge_type` | 谁调用/依赖它 |
| `get_transitive_dependencies` | `node_id`,`edge_type` | 传递依赖闭包 |
| `trace_call_chain` | `node_id` | 调用链 |
| `calculate_coupling_metrics` | `node_id` | 耦合指标 |
| `detect_circular_dependencies` | `edge_type` | 循环依赖 |
| `find_complexity_hotspots` | — | 复杂度热点 |
| `get_hub_nodes` | — | 高中心度节点 |

**4 个 agentic 工具（LLM 工作流，55s–2.5min）**

`agentic_context` / `agentic_impact` / `agentic_architecture` / `agentic_quality`
—— 只在需要「跨证据综合解释」时使用。

### 6.3 内置使用说明（instructions）

MCP 在 `initialize` 时下发 `initial_instructions.md`（约 8.7KB），包含：

- 直连工具表 + 参数
- **`node_id` 从哪来**（必须原样用 ``nodes:`uuid` ``）
- `edge_type` 的 8 个合法值
- 4 个推荐 workflow（改前影响面、调试、架构巡览、何时用 agentic）
- 何时回退到文件读取

更新该文件后需重新编译并重启 opencode（MCP 每次 initialize 读取）。

### 6.4 并发（server 模式的关键收益）

用共享 SurrealDB server 后：

- **索引运行时可以同时查询**，不需要停 MCP
- 反过来也一样

嵌入式模式做不到（排他文件锁，只读也不行）。

### 6.5 斜杠命令

`/cg <问题>` —— `~/.config/opencode/command/cg.md`
`/cg-init [路径]` —— 新项目接入，`~/.config/opencode/command/cg-init.md`

---

## 7. 性能实测与优化

### 7.1 优化前后对比（全部实测）

| 轮次 | 总耗时 | 关键变化 |
|---|---|---|
| 最初 | **47 分钟** | 全默认配置，F2LLM-1.7B（2048 维），串行关系解析 |
| 中期 | 22 分钟 | deferred + EFC64 + COSINE_NORMALIZED + 批大小 |
| 首次应用 MODEL_REVISION | 19.5 分钟 | namespace 变化触发**最后一次**全量重算 |
| **换 F2LLM-0.6B + 关系解析并行** | **~9.5 分钟** | 模型小 2.3×（1024 维），rayon 并行 197s→110s |
| **切换共享 SurrealDB server** | **2.7 分钟** | 写路径摆脱每事务 fsync（604s→164s） |
| **稳态（无文件变化）** | **1.3 秒** | 0 向量重算，全部缓存命中 |

**⭐ 首次运行须知（重要）**：

新项目（或删除 `.codegraph/` 后、或**更换 embedding 模型**）的**第一次索引是
全量**，耗时约 **9.5–10 分钟**（26k 节点 / 1024 维 F2LLM-0.6B，本地 GPU 推理 +
写库）；**首次设置
`CODEGRAPH_MODEL_REVISION=1` 也会再触发一次全量**（约 16 分钟，因为缓存
namespace 改变）。这两次一次性成本之后：

| 场景 | 耗时 |
|---|---|
| 无变化重建 | **7 秒**（input 校验 0.7s + 缓存驱逐 0.6s + 源捕获 0.1s） |
| 改几个文件 | 约 1–2 分钟（只有变化文件的节点重算） |
| 跨小时重建 | **同样命中**（MODEL_REVISION 已消灭时间分桶失效） |
| 换 embedding 模型 | 全量 ~10 分钟（一次性，缓存 namespace 随模型变；换回旧模型同样触发） |

逐阶段实测（26k 节点规模）：

| 阶段 | 优化前 | 优化后 | 手段 |
|---|---|---|---|
| 节点持久化 | 137s | **5s** | `NODE_DB_BATCH_SIZE=512`（事务 406→51） |
| embedding | 每次 ~15min | 首次后 **0** | `MODEL_REVISION=1`（缓存 key 恒定） |
| reconcile | 24 min | **2.2s** | 单次扫描重写（原为 5 条全表 `NOT IN`） |
| 关系解析 | 28 min | 5 min | `SEMANTIC_RESOLUTION=deferred` |
| HNSW 构建 | 分散 | 集中且更短 | `deferred` + `EFC=64` + `COSINE_NORMALIZED` |

对应的改动：

| 措施 | 收益 |
|---|---|
| ⭐ `CODEGRAPH_MODEL_REVISION=1` | 消灭跨小时缓存失效（15 min → 0） |
| `CODEGRAPH_NODE_DB_BATCH_SIZE=512` | 持久化 137s → 5s |
| `reconcile_catalog_by_file`（单次扫描） | 24 min → 2.2s |
| `SEMANTIC_RESOLUTION=deferred` | 关系解析 −82%（代价：semantic 边 2592→0） |
| `VECTOR_INDEX_MODE=deferred` | 写入零索引维护 |
| `HNSW_EFC=64` | HNSW 构建 −2.3× |
| `CHUNK_DB_BATCH_SIZE=256` | 事务/fsync 减 8× |
| schema `COSINE_NORMALIZED` + 出口归一化 | 距离计算 −66% |
| base64 响应编码 | 砍 ~10% JSON 解析 |
| 保留 `index-cache` | 避免重算数万向量 |

### 7.2 为什么 SurrealDB 的 HNSW 偏慢

| 原因 | 细节 |
|---|---|
| 延迟合并 | 写入只进 pending 队列，compaction 时才建图，等于建两遍 |
| 每事务 fsync | SurrealDB 3.x 默认 `SyncMode::Every`，SDK 无法覆盖 |
| 写放大 20× | 实测 256 个向量（2MB）→ 磁盘写 41.8MB |
| 构建并行度低 | 实测只用 1–2 个线程（16 核） |

注：`surrealdb-idx` 的 `unrolled_dot` / `unrolled_fold` 已经做了 8 路展开供 LLVM 向量化，
问题不在缺少 SIMD，而在**重复计算**（`COSINE` 每次要 1 次 dot + 2 次 norm）。

### 7.3 无法优化的部分

| 项 | 原因 |
|---|---|
| `sync=never` | `surrealdb::opt::Config` 无此字段；`ConfigMap` 只在 core 内部可用 |
| HNSW 并行度 | SurrealDB 内部实现 |
| DISKANN 替换 | 需改 schema 且有功能差异 |

---

## 8. 本地改动清单

本项目对 codegraph 及其依赖做了以下改动，**升级依赖时需重新应用**：

| 文件 | 改动 | 原因 |
|---|---|---|
| `crates/codegraph-graph/src/surrealdb_storage.rs` | `upsert_symbol_embeddings_batch` 增加写冲突重试（6 次退避） | SurrealKV 的 `Transaction write conflict` 让整轮索引失败 |
| `crates/codegraph-graph/src/vector_indexes.rs` | `INFO FOR INDEX` 探测加 30s 超时 + 连续 2 次超时降级 | 该查询曾卡死 22 分钟 |
| `crates/codegraph-graph/src/surrealdb_storage.rs` | 新增 `reconcile_catalog_by_file`，按文件维度清理孤儿；`indexer.rs` 改用它 | 原实现 5 条 `NOT IN` 全表扫描，参数 2.6–5.2 万元素，每次索引多耗 10+ 分钟 |
| `crates/codegraph-vector/src/embedding.rs` | 出口归一化（`generate_embeddings` / `embed_texts_batched`） | 配合 `COSINE_NORMALIZED` |
| `crates/codegraph-vector/src/simd_ops.rs` | 修复测试变量名 `_scalar_result` → `scalar_result` | 原有编译错误 |
| `schema/codegraph*.surql` | 54 处 `DIST COSINE` → `DIST COSINE_NORMALIZED` | 省掉 2 次 norm 计算（server 端生效） |
| `crates/codegraph-vector/src/openai_provider.rs` | embedding 响应支持 base64（little-endian f32，3 测试） | 砍 ~10% JSON 解析（perf 实测） |
| `crates/codegraph-vector/src/submitted_cache.rs` | 缓存 key 每文本只算一次 + rayon 并行 | 原来每文本算两次指纹 |
| `crates/codegraph-vector/src/embedding.rs` | chunk 指纹计算改 `cpu_pool` 并行 | 原为单线程 for 循环 |
| `crates/codegraph-mcp/src/indexer.rs` | 节点持久化批独立配置（`CODEGRAPH_NODE_DB_BATCH_SIZE`） | 默认跟随 embedding 批 64 导致 406 次事务 |
| `crates/codegraph-mcp/src/indexer.rs` | 关系解析改 rayon 并行 + DashMap 记忆化 | 96k 边串行循环实测 197s → 110s |
| `schema/codegraph_v2.surql` | `array::range(1, $safe_depth)` → `array::range(1, $safe_depth + 1)`（2 处） | depth=1 时范围为空，`fn::expand`/`fn::trace_call_chain` 静默返回空 |
| `crates/codegraph-mcp-server/src/official_server.rs` | 暴露 8 个直连图工具为 MCP tools + 懒加载 executor | 客户端可自行编排，0.3–5s vs agent 55s |
| `crates/codegraph-vector/src/openai_provider.rs` | 未知模型维度改用配置值（原硬编码 1536） | 自托管模型被当成 OpenAI 默认模型，检索失准 |

回归测试：

```bash
cargo +1.97.1 test -p codegraph-vector --lib normalize
cargo +1.97.1 test -p codegraph-graph --features surrealdb --lib
cargo +1.97.1 test -p codegraph-graph --features surrealdb --test ingestion_write_test
```

---

## 9. 故障排查

| 症状 | 原因 | 处理 |
|---|---|---|
| `Missing pyright-langserver` | balanced tier 需要 Python LSP | `npm i -g pyright` |
| `requires rustc 1.95.0` | 默认工具链太旧 | `cargo +1.97.1 build ...` |
| `openssl.pc not found` | 缺开发包 | `sudo apt install -y libssl-dev pkg-config` |
| 索引卡住无输出 20+ 分钟 | `INFO FOR INDEX` 或 compaction | 已加超时；确认 `VECTOR_INDEX_TIMEOUT_SECS` |
| `Transaction write conflict` | SurrealKV 并发写冲突 | 已加重试；仍失败则重跑（缓存命中很快） |
| `HTTP 500` from 8766 | 容器 OOM 崩溃 | 减 `PROVIDER_CONCURRENCY` / `BATCH_SIZE` |
| `413` from 8766 | 单条超 8192 token | 降 `CHUNK_MAX_TOKENS` |
| `MaxTurnsError: 8` | agent 问题太宽泛 | 缩小问题范围或换 `--focus` |
| 索引报 `not ready` 但数据完整 | compaction 未完成 | 数据可用；下次打开不阻塞 |
| `INDEXING COMPLETE` 后卡 10+ 分钟才退出 | 旧版 `reconcile_catalog` 全表 `NOT IN` | 已改 per-file 版本；旧库需重跑一次 |
| 查询命中第三方代码 | `vendor/` 被索引 | 加进 `.gitignore` 后重建 |
| 索引中改了源码 → `Source changed during indexing` | 索引会校验输入是否变化 | 等索引跑完再改代码；或改完重跑（缓存会命中） |
| 每次索引都要 15 分钟 embedding | 未设 `CODEGRAPH_MODEL_REVISION`，跨小时缓存失效 | 设 `CODEGRAPH_MODEL_REVISION=1`（首次会全量一次） |
| 索引报 "database is locked" / MCP 占库无法索引 | 嵌入式 SurrealKV 是**排他文件锁**，只读进程也不例外 | 用共享 server（见 §2/§3），或索引前停 MCP |
| MCP 图工具返回空数组（`get_reverse_dependencies` 等 depth=1） | **SurrealQL 陷阱**：`array::range(1, 1)` 返回 `NONE` 而非空数组，`array::fold(NONE)` 整条链断掉 | schema 已修为 `array::range(1, $safe_depth + 1)`；老库需重新 apply 函数定义 |

### 观察索引进度的可靠方法

```bash
# embedding 进度（最直接）
docker logs f2llm-infinity 2>&1 | grep -c "Success"

# 是否在写库
du -sk .codegraph/db

# 进程真实 CPU（1 核 = 100 ticks/秒）
awk '{print $14+$15}' /proc/$(pgrep -x codegraph|head -1)/stat
```

---

## 10. 一句话总结

对 9 万行的 Rust 仓库：**首次索引约 20 分钟**（全量，不可避免），
**稳态重建只要 7 秒**（改代码后约 1–2 分钟），查询约 20–40 秒/次。
它的价值在"一次提问代替十几轮 grep + 读文件"，但**简单符号查找仍应优先用
grep**——省钱、省时、结果确定。首次部署时把 20 分钟的一次性成本计入预期。
