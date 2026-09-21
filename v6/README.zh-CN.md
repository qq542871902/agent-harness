# mini-harness V6 — Persistent Sessions

[English](README.md) · [中文](README.zh-CN.md)

V6 是从 V5 演进来的独立快照。它保留沙箱工具、精确 Shell 白名单、Policy/Approval 流程、有界 Agent Loop 和结构化 JSONL Trace，并在 canonical workspace 内新增持久化 save/resume。

## Session 模型

每个新任务拥有 UUID，并写入带版本的 `.sessions/{session_id}.json`。记录包含 canonical workspace、模型名、完整 `AgentState` transcript、状态、累计 step、当前上限、待执行 Tool Call、Trace 路径和创建/更新时间。

File Repository 会拒绝不支持版本、损坏或过大的 JSON、UUID/workspace 不匹配、符号链接目录/文件，以及不直接位于 canonical `.sessions` 下的路径。写入有大小限制，Unix 下使用受保护权限、同步和原子安装；首次创建永不替换已有 Session。每个 Session 有独占 lease，避免并发 resume 重放待执行副作用或覆盖 checkpoint。

Runner 会在创建和每个重要转换时 checkpoint：运行/审批状态、尝试的 Model Request、模型 Tool Call Turn、每个 Tool Observation 和终止状态。持久化失败是 fatal，并写入脱敏的 `agent_failed` Trace。任务内 always-allow Approval Cache 只存在内存中，刻意不序列化。

V6 还保存 in-flight side-effect 调度标记。若写入或 Shell 调用在执行期间中断，resume 不会静默重放它；而是追加同 ID 的 indeterminate Observation，要求模型先核对外部状态。任意外部副作用无法普遍保证 exactly-once，但该机制避免默认的 at-least-once 隐式重放。

```bash
# 启动并持久化新 Session
cargo run -p mini-harness-v6 -- run "检查项目并概括它。"

# 在同一 workspace 中恢复，并追加到相同 Trace
cargo run -p mini-harness-v6 -- resume 550e8400-e29b-41d4-a716-446655440000

# 安全校验并展示 Session 或 Trace 数据
cargo run -p mini-harness-v6 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v6 -- trace 550e8400-e29b-41d4-a716-446655440000
```

已完成 Session 只展示、不重跑。临时 `WaitingApproval` 状态会在 resume 前安全重置。`Failed`、`Aborted` 和 `MaxStepsReached` Session 保留累计 `step`，并确定性获得 20 次额外模型请求额度：`max_steps = prior step + 20`。即使当前默认模型不同，也复用保存的模型。

## 安全性与敏感数据

与 Trace 不同，Session JSON 必然包含用户 Prompt、模型文本、工具参数和工具 Observation。请将 `.sessions/` 视为敏感应用数据，因为 Prompt 或 Tool Output 本身可能携带凭据。Provider 配置和 API Key 不属于 Session schema。`.sessions/` 和 `traces/` 都从 `write_file` 中保留，且两者均拒绝符号链接重定向。V6 仍是学习 Harness，不是 OS 级沙箱或加密 Secret Store。

## 源码地图

```text
src/
├── agent/       # 可序列化 State、checkpointed run/resume Loop
├── config/      # Provider 环境校验
├── llm/         # Provider 无关 Message/Client 与 OpenAI Adapter
├── policy/      # Allow/Ask/Deny Policy 与瞬态 Approval
├── session.rs   # Session schema、Repository/Sink Port、文件系统 Adapter
├── tools/       # 受约束 Native Tool；Session/Trace 树被保留
├── trace.rs     # 创建/追加 JSONL Writer 与安全查找
└── main.rs      # run、resume、session、trace 与交互 CLI
```

V6 刻意不包含 Context 压缩（V7）、任意 Shell、加密 Session 存储或生产级 OS 隔离。
