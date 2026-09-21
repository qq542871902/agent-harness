# mini-harness V7 — Context Management

[English](README.md) · [中文](README.zh-CN.md)

V7 是基于 V6 的独立快照。它保留受沙箱保护的 Native Tool、Policy/Approval 流程、有界 Loop、结构化 Trace 和可恢复 Session，并新增一个由 `AgentRunner` 在每次请求中使用的 Provider 无关 `ContextBuilder`。

## Context 策略

canonical `AgentState.messages` 历史会保留在 Session 中（只受 Native Tool Output 截断影响）；每次发送模型的只是派生出的有界视图。估算器刻意不依赖额外库且具有确定性：将完整 compact `ChatRequest` JSON（包括模型标识、Tool Definition 与 Message）序列化后，用 UTF-8 字节数除以四并向上取整。这是近似值，不代表 Provider 的计费 Token 用量。

默认值为 `CONTEXT_TOKEN_BUDGET=16384`、`MAX_TOOL_OUTPUT_CHARS=32768` 和 2,048 字符的摘要上限。可选环境变量会被 trim、解析为有界正整数，非法时给出清晰错误。Provider 字段仍在 `Config` Debug 中脱敏。Context 设置不含密钥且会持久化，保证 resume 使用原始策略。

当未超预算时，请求与 canonical 历史完全一致。超预算时，V7 总会保留 Coding System Prompt 和原始用户任务；每条 Assistant `tool_calls` Message 与其后所有同 ID Tool Result 会作为原子组；先移除最旧的完整组，再添加确定性有界 System Summary，并保留能放入预算的最大最近后缀。Summary 总会保留结构化计数和聚合工具名/结果；只有安全摘要片段会被缩短。已知 Provider 密钥只瞬态注入 Builder 用于精确脱敏，不会持久化；带凭据标记的行和类似 Token 的值也会脱敏。若必需 Context 无法容纳，运行会本地明确失败，而不是超出预算或产生非法协议 transcript。

Native Tool Observation 会在插入 canonical 历史和 Session 前截断。截断按 Unicode scalar value 计数，保留平衡的首尾内容，并加入明确的 `original_chars=N` 标记。`context_built` 和增强的 `tool_result` Trace Event 只包含计数、大小和布尔值，绝不记录 Message 或 Tool Output 内容。

## Session 与兼容性

V7 将 Session schema 提升到版本 2，并保存 Context Setting 与最新确定性 Summary Metadata。V7 采用严格兼容策略：schema-1 的 V6 Session 及未知未来版本都会被明确拒绝；不做隐式 migration。V0–V6 保持独立且不变。

```bash
cargo run -p mini-harness-v7 -- run "检查项目并概括它。"
cargo run -p mini-harness-v7 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v7 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v7 -- trace 550e8400-e29b-41d4-a716-446655440000
```

## 源码地图

```text
src/
├── agent/       # Canonical State 与 Context 管理 Runner
├── context/     # 估算器、连贯分组、Summary、Unicode 截断
├── config/      # 脱敏 Provider Config 与受校验 Context 限制
├── llm/         # Provider 无关协议与 OpenAI-compatible Adapter
├── policy/      # Allow/Ask/Deny Policy 与瞬态 Approval
├── session.rs   # 严格 schema-2 的原子 Session 持久化
├── tools/       # workspace 约束的 Native Tool
├── trace.rs     # 无内容的 Context Metadata 与 JSONL Trace
└── main.rs      # 组合与 CLI
```

V7 仍是学习 Harness，不是精确 Tokenizer、加密存储、任意 Shell 或 OS 级沙箱。
