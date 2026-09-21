# mini-harness V4 — Policy 与 Approval

[English](README.md) · [中文](README.zh-CN.md)

V4 是建立在 V3 有界 Coding Agent 上的独立快照。它保留 V3 的严格 workspace 约束、原子有界写入、精确 Shell 白名单、有界进程 I/O、模型 step 上限和可恢复 Tool Observation，并在每次工具分发前加入 Policy 决策和可注入的人类审批 Port。

## 默认 Policy

| Tool Call | 决策 |
| --- | --- |
| `read_file`、`list_files` | Allow |
| `write_file` | Ask |
| `shell` 且命令精确为 `cargo test`、`cargo check`、`git diff` | Allow |
| `shell` 且命令精确为 `cargo fmt`、`cargo clippy`、`git status` | Ask |
| 其他非危险 Shell 字符串 | Ask，之后原生精确白名单仍会生效 |
| 危险或格式错误的 Shell 输入 | Deny |
| 未知工具 | Deny |

危险命令分类使用类型化 Shell 参数和 token 化命令结构，而不是子串匹配。Policy Approval 不会扩大原生能力：即使已批准，Shell 也只能是 V3 的六个精确命令，不经过 Shell 解析，并继续受 timeout 和输出上限保护。

对 `Ask` 决策，控制台会显示工具和格式化 JSON 参数，并对常见密钥字段的值脱敏。输入 `y` 表示本次批准、`n` 表示拒绝、`a` 表示当前任务内始终批准该工具。always-allow 保存于独立的内存 Approval State，只对当前 Agent Task 生效；每次调用仍先经过 Policy，所以 `Deny` 总是优先。被用户拒绝或被 Policy 拒绝的调用会作为同 ID Tool Observation 追加，模型仍获得下一轮决策机会。

## 非交互安全性

单次 `run` 使用相同 Console Approver；写入、`cargo fmt`、`cargo clippy` 或 `git status` 可能触发提示。若 stdin 不可用或遇到 EOF，审批解析为拒绝；V4 不会无限等待或隐式批准。每条交互输入都会新建任务，因此也会获得新的 always-allow State。

## 配置与运行

使用前序快照相同的 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`OPENAI_MODEL` 配置，并从目标 workspace 根目录运行：

```bash
cargo run -p mini-harness-v4 -- run "检查项目，完成请求的变更，并测试它。"

# 交互模式：每行创建新的 AgentState 与 Approval State
cargo run -p mini-harness-v4
```

输入 `/exit`、`exit` 或 `quit` 退出。

## 源码地图

```text
src/
├── main.rs       # CLI 组合与 Console Approver
├── agent/        # 有界循环、Policy Gate、Coding Prompt、State、Outcome
├── policy/       # 类型化 DefaultPolicy、ApprovalHandler、任务内 Override
├── config/       # Provider 配置与脱敏 Debug
├── llm/          # OpenAI-compatible 协议与有 timeout 的 URL 安全 HTTP Client
└── tools/        # V3 严格 Registry 与受沙箱保护的 read/list/write/shell 工具
```

V4 的范围只包括 Policy 和 Approval；不包括任意 Shell、持久化审批或 Session、JSONL Trace、Context 压缩或生产级 OS 隔离。
