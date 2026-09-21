# mini-harness V9 — 同进程 Sub-Agent

[English](README.md) · [中文](README.zh-CN.md)

V9 是基于 V8 的独立快照。它保留 V8 的 Native/MCP Tool、Policy 与 Approval、有界 Context、JSONL Trace 和可恢复 Parent Session，然后通过 `spawn_agent` 增加需要审批的有界委派。V0–V8 保持独立且不变。

## `spawn_agent` 契约

模型可见工具接受一个严格 Object：

```json
{"role":"research","task":"检查相关代码","max_steps":4}
```

- `role` 只能为 `research`、`code`、`test`；
- `task` trim 后不可为空，最多 4,096 个 Unicode 字符；
- `max_steps` 可选，默认 4，必须为 1–8 的整数；
- 未知字段和格式错误值会在 Child 启动前拒绝；
- Child 执行上限 120 秒，返回输出最多 16,384 个 Unicode 字符；最终回答、max-step、timeout 和 fatal failure 都会成为普通、有界的 Parent Tool Observation。

`DefaultPolicy` 对 `spawn_agent` 返回 `Ask`。审批拒绝会阻止 Child 构建/执行，并变成 Parent Observation。Console Approval 可 await，且限制为 60 秒，不能突破 Child deadline。Child 启动后，其每次 Tool Call 仍分别通过相同 Policy 与 Approval Handler；Policy Deny 始终优先。

## Runtime 与能力模型

`SubAgentSpawner` 是执行 Port，`SpawnAgentTool` 是其 Tool Adapter。`SameProcessSubAgentSpawner` 在现有 Tokio 进程内运行 Child `AgentRunner`——Sub-Agent 从不作为 OS 进程启动。若 Child 被允许使用 `shell`，它仍只能运行 V8 的精确白名单验证命令。

每个 Child 共享 Parent Task 的 `Arc<dyn LlmClient>`、模型、`ContextBuilder` 设置与密钥脱敏值、`DefaultPolicy`/`ApprovalHandler`、Parent Trace Sink，以及当前 Native/MCP Registry 的 Arc-backed 快照。Child 拥有独立内存 `AgentState`、Approval State、step budget、按角色定制的 System Prompt，且没有持久化 Child Session。

| 角色 | Native Tool | MCP Tool |
| --- | --- | --- |
| `research` | `read_file`、`list_files` | 当前已注册的所有 MCP Tool |
| `code` | `read_file`、`list_files`、`write_file`、`shell` | 当前已注册的所有 MCP Tool |
| `test` | `read_file`、`list_files`、`shell` | 当前已注册的所有 MCP Tool |

Child Snapshot 永不包含 `spawn_agent`，因此递归委派在结构上不可达。`ToolRegistry` 使用排序的 `BTreeMap<String, Arc<dyn Tool>>`，Clone 和过滤 Snapshot 都轻量且确定。等待 Child 时不持有 Registry Lock 或可变 Registry Borrow。Shell Process Group 带取消守卫，因此外层 Child timeout 不会遗留编译器子进程。V8 的 main 所有 `McpManager` 生命周期，直到 Parent Command 与所有直接 await 的 Child 工作结束。

交互任务会创建新的 task-scoped Registry 和 spawn Adapter，将委派绑定到该任务 Trace。resume 加载 schema 5，并从当前 Native/MCP Snapshot 重建相同能力。

## Session 与 Trace

V9 Session schema 5 明确拒绝所有旧/未来版本。除 V8 字段外，它保存 `sub_agent_enabled: true`（非机密能力标记）和 in-flight side-effect marker。该 marker 阻止 resume 静默重放：中断的写入、Shell、MCP 或 `spawn_agent` 调用会变成同 ID 的 indeterminate Parent Observation，使模型先核对外部状态。Child Prompt、任务、State、Output 和独立 Child Session 都不持久化；有界 Child Result 会自然作为 Parent Tool Observation 持久化。

共享 Parent JSONL Trace 增加 `sub_agent_started`、`sub_agent_completed`、`sub_agent_failed`。这些事件只含唯一 Child UUID、Parent Tool Call ID、角色、step 数和状态。Child 内部 Runner Event 携带相同 lineage 的 `actor: child`，不会被混淆为 Parent 生命周期事件；不会记录委派任务、Prompt 或 Output。现有模型/工具事件继续保持无 Payload。

## 运行与验证

```bash
cargo run -p mini-harness-v9 -- run "委派仓库研究任务，然后概括它。"
cargo run -p mini-harness-v9 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v9 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v9 -- trace 550e8400-e29b-41d4-a716-446655440000

cargo fmt --all -- --check
cargo test -p mini-harness-v9 --all-targets
cargo check -p mini-harness-v9 --all-targets
cargo clippy -p mini-harness-v9 --all-targets -- -D warnings
```

## 源码地图

```text
src/agent/sub_agent.rs    # 同进程 Runtime、角色 Prompt、限制、lineage
src/tools/spawn_agent.rs  # 严格 Adapter 与 SubAgentSpawner Port
src/tools/registry.rs     # 确定性 Arc clone/filter Snapshot
src/session.rs            # 严格 schema-5 Parent 持久化与 execution journal
src/trace.rs              # 无 Payload 的 Sub-Agent 生命周期事件
src/main.rs               # task-scoped run/interactive/resume 组合
```

V9 刻意不包含递归/多层委派、任意并发、持久化 Child Session、任意 Shell、加密 Session 存储和 OS 级隔离。MCP 命令仍是运行在 Native 文件工具沙箱外的受信任用户配置；启用前请阅读 [`v8/README.zh-CN.md`](../v8/README.zh-CN.md)。
