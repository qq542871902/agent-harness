# mini-harness V8 — Bounded MCP stdio

[English](README.md) · [中文](README.zh-CN.md)

V8 是基于 V7 的独立快照。它保留 V7 的 Native Tool、Policy/Approval、有界 Context、结构化 Trace 和可恢复 Session，并将刻意受限的 MCP Client 适配到同一个动态 `Tool` Trait。V0–V7 保持独立且不变。

## 支持的 MCP 子集

V8 实现 stdio 工具所需的稳定 **2025-06-18** lifecycle 子集；它不声称支持完整 MCP，也不声称兼容未指定的“2026 协议”。实现参考官方 [lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)、[stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports) 和 [tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools) 文档。

支持行为刻意有边界：

- 用户通过 `MCP_CONFIG_PATH` 选择 server；启用的 server 以直接可执行文件和参数向量启动，绝不经过 Shell。变量缺失时，V8 仅注册 Native Tool。
- Transport 为子进程 stdin/stdout 上按行分隔的 UTF-8 JSON-RPC 2.0。V8 发送带 `protocolVersion: "2025-06-18"`、Client 信息和空 Client Capability 的 `initialize`，校验协商版本，然后发送 `notifications/initialized`。
- `tools/list` 最多遍历 100 页、每 server 最多 256 个工具。`tools/call` 支持 Text Content、`structuredContent` 与 `isError`；其他 Content Type 和 MCP 能力会被拒绝为不支持。
- Client Request ID 递增且 Response ID 必须匹配。Server Notification 被忽略；不支持的 server-to-client Request 会收到 JSON-RPC `-32601`，避免双方死锁。
- 启动、请求、关闭、单行、映射结果、Schema、Description、工具数量和保留 stderr 都有边界。默认：每个启动阶段 10 秒、每请求 15 秒、优雅关闭 1 秒、每行/结果 1 MiB、保留 stderr metadata 64 KiB；stderr 被持续 drain 但不会记录。
- Client 优先关闭 stdin 做优雅结束，超过关闭上限后终止完整 Child Process Group；Drop 是兜底 kill。
- 普通远端失败映射为结构化 `McpProtocolError`；MCP `isError` 映射为 `ToolOutput.success = false`，使普通 Agent Loop 能观察失败。
- timeout、malformed framing、超长协议行、ID 不匹配或其他协议失同步会关闭并隔离 Session，终止 server 进程组；后续调用 fail closed，不会消费迟到 Response。

它不支持 Streamable HTTP、resources、prompts、roots、elicitation、sampling、completion、subscriptions、authorization、动态 list-change、二进制图片/音频 Content 或未来版本通用支持。

基于链接 MCP 文档的内容已为许可合规而改写。

## 配置与安全边界

可将 `mcp.example.json` 复制到仓库外，编辑后设置绝对路径：

```bash
MCP_CONFIG_PATH=/absolute/path/to/mcp.json \
  cargo run -p mini-harness-v8 -- run "使用已配置工具检查任务。"
```

严格版本 1 JSON 形状：

```json
{
  "version": 1,
  "servers": [
    {
      "name": "local",
      "enabled": true,
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"],
      "working_directory": "/absolute/existing/directory",
      "env_allowlist": ["HOME"]
    }
  ]
}
```

未知字段、不支持版本、超过 64 KiB 的文件、符号链接 Config、重复/非法名称、空/NUL/非绝对命令、非法工作目录、NUL 参数、畸形/重复环境变量名都会被拒绝。Server 和 Remote Tool component 只能使用 1–48 个 ASCII 字母、数字、`_`、`-`，并排除含糊的 `__`。命令必须指向存在的绝对常规文件；`working_directory` 可以为 `null`。

`env_allowlist` 只包含**变量名**。Child 环境会清空，只继承该白名单中当前存在的变量值。密钥值永不写入此 Config 格式、日志、Trace 或 MCP Session Manifest；不要把机密放进 `args`。

**配置的 MCP 命令是受信任的用户配置，绝非模型生成。它们运行在 Native 文件工具沙箱之外，可访问 OS 账户允许的一切内容。** 启用前请审查可执行文件、参数、工作目录和允许继承的环境变量。

发现到的工具以确定性 `mcp__<server>__<tool>` 暴露。非法/过大名称、非 Object 或过大 Input Schema、过大 Description 和 Registry Collision 会使启动失败。Native Tool 保持不变。每个有效 `mcp__` Tool 的 `DefaultPolicy` 为 `Ask`；畸形名称被拒绝，且 Deny 优先。审批提示会显示脱敏参数，任务内 always-allow 只作用于完整 namespaced Tool 名称。

## Session 与 Trace

V8 使用严格 Session schema 3，只保存排序后的 namespaced MCP Tool Manifest；永不保存命令、参数、Config 路径、环境变量值或其他 MCP 机密。恢复使用过 MCP 的 Session 时必须提供当前 `MCP_CONFIG_PATH`。若当前配置缺少此前工具，待处理调用会变成失败 Tool Observation，模型可继续使用当前 Manifest。

每个 Session 的 JSONL Trace 新增只含 metadata 的 `mcp_server_connected`、`mcp_tool_registered`、`mcp_call_result`；JSON-RPC 参数、Schema、Content、结构化结果、stderr、命令配置和环境变量值均被省略。

```bash
cargo run -p mini-harness-v8 -- run "检查项目并概括它。"
cargo run -p mini-harness-v8 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v8 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v8 -- trace 550e8400-e29b-41d4-a716-446655440000
```

## 源码地图与验证

```text
src/
├── agent/       # Source-neutral Agent Loop 与 MCP Metadata Trace
├── context/     # 确定性 Context Budget 与 Output 截断
├── mcp/         # 严格 Config、有界 JSON-RPC stdio Client、Tool Adapter
├── policy/      # Native Rule 与 MCP 默认 Ask Policy
├── session.rs   # 带非机密 MCP Manifest 的严格 schema-3 持久化
├── tools/       # 动态安全 Tool Trait、Registry 与 Native Tool
├── trace.rs     # 无 Payload 的 Native/MCP JSONL Metadata
└── main.rs      # connect/register/reuse/shutdown 生命周期
tests/
└── mcp_protocol.rs # 不依赖 Shell 的确定性 Mock Server 协议测试
```

```bash
cargo fmt --all -- --check
cargo test -p mini-harness-v8 --all-targets
cargo check -p mini-harness-v8 --all-targets
cargo clippy -p mini-harness-v8 --all-targets -- -D warnings
```

V8 仍是学习 Harness，不是完整 MCP Host、加密存储或 OS 级沙箱。
