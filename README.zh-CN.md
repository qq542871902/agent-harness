# mini-harness

[English](README.md)

一个用于学习 Agent Harness 构建方式的分阶段 Rust 项目。每个目录都是可独立运行的学习快照，而不是持续膨胀的单一应用。

```text
agent-harness/
├── mini-harness-spec.md  # 完整项目规格
├── .env.example          # 共享的 OpenAI-compatible 配置模板
├── v0/                   # 用户 → LLM → 回答
├── v1/                   # LLM Tool Call → Tool Registry → 原生工具
├── v2/                   # LLM → 工具 → Observation → LLM → 最终回答
├── v3/                   # 带受限写入与 Shell 工具的 Coding Agent
├── v4/                   # 带人工审批的 Policy Agent
├── v5/                   # 单次运行的完整 JSONL Trace
├── v6/                   # 可恢复的持久化版本化 Session
├── v7/                   # 确定性的 Token 预算 Context Management
├── v8/                   # 有边界的 MCP 2025-06-18 stdio 工具适配器
└── v9/                   # 同进程、按角色受限的 Sub-Agent
```

## 阶段

| 目录 | 重点 | 包含能力 | 明确不包含 |
| --- | --- | --- | --- |
| [`v0/`](v0/README.zh-CN.md) | LLM CLI | Config、`LlmClient`、一个 OpenAI-compatible Provider、单次/交互式 CLI | 工具、Agent Loop、Session、Policy、Trace |
| [`v1/`](v1/README.zh-CN.md) | Tool Calling | V0 基础、Tool Schema、Tool Call 解析、Registry、`read_file`、`list_files` | Tool Result 回传、多步 Agent Loop |
| [`v2/`](v2/README.zh-CN.md) | Agent Loop | V1 工具、内存 AgentState、Tool Observation、多轮循环、max-step 上限 | 写入/Shell 工具、审批、持久化 Session、Trace |
| [`v3/`](v3/README.zh-CN.md) | Coding Agent | V2 循环、严格受限的 `write_file`、精确白名单 `shell`、有界 I/O、Coding Prompt | 任意 Shell、审批、持久化 Session、OS 级隔离 |
| [`v4/`](v4/README.zh-CN.md) | Policy / Approval | V3 基础、Allow/Ask/Deny Policy、CLI 审批、任务内 always-allow、拒绝 Observation | 任意 Shell、持久化审批/Session、Trace、OS 级隔离 |
| [`v5/`](v5/README.zh-CN.md) | Structured Trace | V4 行为、按 UUID 的 flush-on-event JSONL Trace、Observer Port、安全查看 Trace | 可恢复 Session、Context 压缩、任意 Shell、OS 级隔离 |
| [`v6/`](v6/README.zh-CN.md) | Persistent Sessions | V5 行为、原子 Session checkpoint、安全 resume、累计 continuation budget、append Trace | Context 压缩、加密存储、任意 Shell、OS 级隔离 |
| [`v7/`](v7/README.zh-CN.md) | Context Management | V6 行为、请求 Token 预算、连贯历史压缩、确定性摘要、Unicode 安全工具输出截断 | 精确 Provider Tokenization、加密存储、任意 Shell、OS 级隔离 |
| [`v8/`](v8/README.zh-CN.md) | Bounded MCP stdio | V7 行为、严格用户配置、MCP 2025-06-18 生命周期、分页发现、命名空间适配器、Ask Policy、有界进程生命周期 | 完整/未来 MCP、HTTP Transport、Server 发起能力、MCP 沙箱 |
| [`v9/`](v9/README.zh-CN.md) | 同进程 Sub-Agent | V8 行为、`spawn_agent`、角色过滤 Registry 快照、共享 Runtime Port、有界委派、无载荷 lineage Trace、schema-5 execution journal resume | 递归委派、并发编排、独立 Child Session/进程 |

## 一次配置

在 workspace 根目录执行：

```bash
cp .env.example .env
```

设置 `OPENAI_API_KEY`、`OPENAI_BASE_URL` 与 `OPENAI_MODEL`。同一 OpenAI-compatible 配置也可用于 DeepSeek 等 Provider。

## 运行指定阶段

```bash
# V0：纯 LLM CLI
cargo run -p mini-harness-v0 -- run "解释什么是 Agent Harness。"

# V1：让模型单次选择只读工具
cargo run -p mini-harness-v1 -- run --tools "列出当前 workspace 中的文件。"

# V2：将工具 Observation 回传模型，直至最终回答或 max_steps
cargo run -p mini-harness-v2 -- run "检查当前 workspace 并概括其阶段。"

# V3：检查、在 workspace 内编辑，并运行精确白名单检查
cargo run -p mini-harness-v3 -- run "检查项目，更新文档，并测试变更。"

# V4：每次工具调用先经过 Policy；需要时提示人工审批
cargo run -p mini-harness-v4 -- run "检查项目，更新文档，并测试变更。"

# V5：保留 V4 决策，并写入完整 UUID JSONL Trace
cargo run -p mini-harness-v5 -- run "检查项目并概括其阶段。"
cargo run -p mini-harness-v5 -- trace <session-id>

# V6：持久化任务，并以相同 transcript 和 Trace 恢复
cargo run -p mini-harness-v6 -- run "检查项目并概括其阶段。"
cargo run -p mini-harness-v6 -- resume <session-id>

# V7：在保持必要 Prompt 和工具协议的前提下限制请求 Context
CONTEXT_TOKEN_BUDGET=16384 MAX_TOOL_OUTPUT_CHARS=32768 \
  cargo run -p mini-harness-v7 -- run "检查项目并概括其阶段。"

# V8：从显式用户 MCP 配置中发现有边界的 stdio 工具
MCP_CONFIG_PATH=/absolute/path/to/mcp.json \
  cargo run -p mini-harness-v8 -- run "使用已配置工具检查任务。"

# V9：在同一个 Harness Runtime 内委派受限研究/编码/测试任务
cargo run -p mini-harness-v9 -- run "委派仓库研究任务，然后概括结果。"
```

## 重要安全边界

- V4 以后某些调用会在 stdin 上等待审批；EOF 或不可用 stdin 会安全地拒绝调用，不会隐式批准。
- 后续阶段的 Provider 凭据默认要求 HTTPS；只有显式配置时才允许 loopback HTTP。
- Shell 子进程使用最小非敏感环境，但 Cargo 命令仍可执行仓库控制的 build script、proc macro 与测试代码；这不是 OS 级隔离。
- 模型文件工具会屏蔽 `.env`、`.sessions`、`traces`、私钥与凭据路径；`.sessions` 仍应视为敏感数据，因为它保存任务 transcript。
- V8 MCP server 是用户信任的本地命令，在原生文件工具沙箱之外运行；协议 timeout 或失同步会关闭并隔离该 server。
- V9 的 execution journal 防止 resume 静默重放中断的写入、Shell、MCP 或 Sub-Agent 副作用；它们会变为 indeterminate Observation。

根 `Cargo.toml` 是 virtual workspace。请始终使用 Cargo 的 `-p` 明确选择学习阶段，使命令与所学习的源码快照一致。
