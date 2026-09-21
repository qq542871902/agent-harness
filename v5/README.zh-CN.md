# mini-harness V5 — Structured JSONL Trace

[English](README.md) · [中文](README.zh-CN.md)

V5 是基于 V4 的独立快照。它保留 V4 的受沙箱保护工具、精确 Shell 白名单、有界 I/O、模型 step 计数、类型化 Allow/Ask/Deny Policy、任务内审批和可恢复的拒绝/工具失败 Observation，并新增可重建一次完整 Agent Run 的结构化 Trace。

## Trace 模型

每个 CLI `run` 以及每个已接受的交互任务都会创建 UUID Session，并写入：

```text
traces/{session_id}.jsonl
```

模型循环启动前，CLI 会打印 Session ID 和 Trace 路径。每一行是一个完整 JSON Object，并在写入后立即 flush。每条记录包含 RFC 3339 UTC `timestamp`、`session_id` 和 serde tag 的 `type`。事件覆盖 Session 开始、Model Request/Response、Tool Call/Result、Approval Request/Decision、完成、fatal failure 和 max-step 耗尽。

`AgentRunner` 只依赖 `TraceSink` Observer Port；`JsonlTraceWriter` 是文件系统 Adapter；仅用于测试的 `NullTraceSink` 保持简单的单元测试组合。Policy、Approval 与 Native Tool 决策保持不变：被拒调用和工具失败会产生不成功的 `tool_result` Event，并继续作为同 ID Observation 回传模型；fatal Runner Error 最终写为 `agent_failed`。

Trace 刻意省略 Tool Arguments 和 Tool Output，避免将 payload 中的凭据持久化。fatal Error 文本限制为 2,048 个字符，包含常见凭据标记的行会被替换为 `[REDACTED]`。

## 配置与运行

使用共享配置设置 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`OPENAI_MODEL`，然后从工具应访问的 workspace 运行：

```bash
cargo run -p mini-harness-v5 -- run "检查项目并概括它。"

# 交互模式：每个任务拥有新的 UUID、Trace、AgentState 与 Approval State
cargo run -p mini-harness-v5

# 校验 canonical UUID 并安全展示 JSONL Trace
cargo run -p mini-harness-v5 -- trace 550e8400-e29b-41d4-a716-446655440000
```

Trace 目录必须是 canonical workspace 下直接存在的真实目录；符号链接 Trace 目录或符号链接 Session 文件会被拒绝。V5 还会从原生 `write_file` 中保留 `traces/`，因此即使模型写入已获批准，也不能替换正在使用的审计文件；V4 的 Policy 决策不变，被阻止的尝试仍是可恢复 Tool Observation。

## 源码地图

```text
src/
├── main.rs       # CLI 组合、每任务一个 Trace、安全展示 Trace
├── trace.rs      # TraceEvent、TraceSink/NullTraceSink、JsonlTraceWriter
├── agent/        # V4 Loop 加有序 Observer Event
├── policy/       # 未改变的 V4 Policy 与 Approval 行为
├── config/       # Provider 配置与脱敏 Debug
├── llm/          # OpenAI-compatible 协议与 HTTP Client
└── tools/        # 严格受沙箱保护的 read/list/write/shell 工具
```

V5 不包含持久化/可恢复 Session、任意 Shell、Context 压缩或生产级 OS 隔离。
