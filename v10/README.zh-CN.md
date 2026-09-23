# mini-harness V10 — 受控 Workspace RAG

[English](README.md) · [中文](README.zh-CN.md)

V10 在 V9 Agent Runtime 上新增只读工具 `search_workspace_knowledge`。它在保留 workspace 相对路径和行号引用的同时，支持确定性本地词法检索，以及显式启用的向量与混合 RAG。

## 检索模式

通过进程级环境变量 `RAG_MODE` 选择模式：

- `lexical`（默认）：本地子串匹配，workspace 内容不会离开当前进程；
- `vector`：按行分块后调用 OpenAI-compatible `/embeddings`，使用余弦相似度排序；
- `hybrid`：使用 reciprocal rank fusion（RRF）融合词法与向量 chunk 排名；若 Embedding 或索引构建失败，会带 warning 回退到词法结果。

向量只缓存在当前进程内存中。第一次语义检索会为 workspace 分块生成向量；后续查询复用索引，源文件内容哈希变化时自动重建。每次语义检索都会生成查询向量。不使用向量数据库，也不把 Embedding 缓存写入磁盘。

## 配置

已有 Chat 配置仍然必填：

```text
OPENAI_API_KEY=...
OPENAI_BASE_URL=https://provider.example/v1
OPENAI_MODEL=...
```

默认词法模式不需要新增配置。启用向量或混合检索：

```bash
RAG_MODE=hybrid \
EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "查找鉴权流程并引用实现。"
```

当两者都复用 Chat Provider 时，`EMBEDDING_API_KEY` 和 `EMBEDDING_BASE_URL` 可以不设置。如果显式设置了 `EMBEDDING_BASE_URL`，则也必须显式设置 `EMBEDDING_API_KEY`；V10 会拒绝把 Chat 凭据发送到独立配置的 Endpoint。以下参数可选且有严格范围：

```text
RAG_CHUNK_LINES=40                 # 4..200
RAG_CHUNK_OVERLAP_LINES=8          # 0..chunk_lines-1
RAG_EMBEDDING_BATCH_SIZE=16        # 1..64
```

Provider URL 必须使用 HTTPS。只有设置 `ALLOW_HTTP_LOOPBACK=true` 时，才允许显式 loopback host 使用 HTTP，与 V9 Chat Client 的策略一致。

**数据边界：**`vector` 和 `hybrid` 会把排除敏感路径后的 workspace chunk 文本、相对路径及查询发送给 Embedding Provider。只能使用已获准接收源码的 Provider。`lexical` 始终完全在本地执行。

## 工具契约

模型侧输入与原词法实现兼容：

```json
{
  "query": "DefaultPolicy ToolPermission",
  "top_k": 5
}
```

- `query`：必填，trim 后不可为空，最多 512 个 Unicode 字符；
- `top_k`：可选，默认 `5`，范围 `1–10`；
- 未知字段和错误类型会被拒绝。

结果包含 workspace 相对路径、1-based 行范围、有界 snippet 以及与模式对应的分数：

```json
{
  "query": "工具调用在哪里鉴权",
  "result_count": 1,
  "results": [
    {
      "path": "v9/src/policy/mod.rs",
      "start_line": 1,
      "end_line": 40,
      "score": 32258,
      "lexical_score": 220,
      "vector_similarity": 0.81,
      "snippet": "..."
    }
  ],
  "retrieval": "hybrid_lexical_vector",
  "warning": null,
  "fallback_reason": null
}
```

词法分数继续使用原来的完整短语/关键词频次启发式算法；向量分数是非负余弦相似度放大到 `0..1000`；混合分数是放大后的 RRF 值，只应在同一次响应内比较。词法模式不会输出向量字段。`retrieval` 可能是 `local_lexical`、`remote_vector`、`hybrid_lexical_vector` 或 `local_lexical_fallback`。发生回退时还会输出经过清洗的 `fallback_reason` 分类，例如 `embedding_authentication`、`embedding_transport`、`embedding_response`、`embedding_dimension` 或 `local_index`，不会包含 Provider Response Body 或源码。

检索结果只是定位线索，模型应使用 `read_file` 核验完整源码上下文。

## 检索与安全边界

工具会 canonicalize workspace，不跟随符号链接，排除 `.env`（保留 `.env.example`）、`.sessions/`、`traces/`、常见包管理器/云服务/容器凭据文件、私钥及凭据目录，并跳过 `.git/`、`target/`、`node_modules/`。最多扫描 500 个常规文件；单文件最多 1 MiB，累计最多 4 MiB；最多返回 10 个结果，每个 snippet 最多 600 个 Unicode 字符。为了兼容文档检索，`.env.example` 仍可被检索；启用远程模式前应确认其中只有占位符，没有可用秘密。

语义模式最多创建 2,048 个重叠 chunk；每个 Embedding 输入最多 8,000 个 Unicode 字符；每批最多 64 条；响应最多 8 MiB；禁止 HTTP redirect；空向量、零向量、非有限数值、维度不一致和错误索引都会被拒绝。

`V10Policy` 只额外自动允许这个只读检索工具；V9 的 Policy、Approval、Context、Trace、Session、MCP 与原生工具语义保持不变。Trace 只记录无 payload 的工具元数据，独立 Embedding API Key 也会加入 Context redaction。V10 继续使用 V9 schema-5 Session，因此不要用 V9 恢复未完成的 V10 Session。

V9 子代理使用封闭的原生工具白名单，所以检索工具仍只提供给主 Agent。

## 运行

```bash
# 默认：本地词法检索
cargo run -p mini-harness-v10 -- run "查找 DefaultPolicy 如何限制工具调用，并引用相关代码。"

# 向量检索
RAG_MODE=vector EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "鉴权在哪里执行？"

# 混合检索
RAG_MODE=hybrid EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "查找鉴权检查并引用代码。"

cargo run -p mini-harness-v10 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v10 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v10 -- trace 550e8400-e29b-41d4-a716-446655440000
```

V10 仍不是 LightRAG：它不抽取实体或关系，也不包含图检索。后续可以在相同受控工具边界后增加这些能力，而无需替换 V9 的 `AgentRunner`、`Policy`、`Session` 或 `Trace`。
