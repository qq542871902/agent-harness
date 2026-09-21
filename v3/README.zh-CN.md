# mini-harness V3 — Coding Agent

[English](README.md) · [中文](README.zh-CN.md)

V3 是独立快照，将 V2 的只读 Agent Loop 扩展为小型、有边界的 Coding Agent。它保留完整的 `模型 → 工具 → Observation` 循环，并增加安全的 workspace 写入与精确白名单命令执行器。

## 能力

- `read_file`：读取最大 1 MiB 的 UTF-8 常规文件；
- `list_files`：最多列出 1,000 个直接子项，输出有大小限制；
- `write_file`：创建或原子替换最大 1 MiB 的 UTF-8 常规文件；
- `shell`：不经过 Shell，直接执行且仅允许 `cargo check`、`cargo test`、`cargo fmt`、`cargo clippy`、`git diff`、`git status`；执行上限 120 秒，stdout/stderr 有界；
- 所有工具使用严格 JSON 参数；未知字段会被拒绝；
- canonical workspace 约束、路径穿越/绝对路径拒绝、符号链接逃逸防护；
- 系统提示要求先检查文件、修改后测试；
- V2 的 20 次模型请求上限与 Tool Failure Observation 语义。

Shell 结果为含 `exit_code`、`stdout`、`stderr` 的结构化 JSON。非零退出码仍是成功完成的工具 Observation，使模型能分析并修复测试失败。

## 安全边界

进程当前工作目录就是 workspace。文件工具不能访问 workspace 外路径；`write_file` 要求父目录已存在，并拒绝符号链接目标。Shell 输入不由 Shell 解释，不允许 flag、命令链或六个精确白名单命令之外的调用。

Provider 默认要求 HTTPS；只有显式的 loopback 本地配置才可使用 HTTP。Shell 子进程清空继承环境后只恢复少量非敏感变量，因此不会继承 Provider API Key。文件工具拒绝 `.env`、私钥、凭据目录及 Harness 的 `.sessions`/`traces` 路径。

仍需注意：Cargo 命令可执行仓库控制的 build script、proc macro、compiler wrapper 和测试二进制。精确白名单并不构成对恶意仓库代码的 OS 级隔离。

## 配置与运行

使用与前序快照相同的 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`OPENAI_MODEL` 配置，并从目标 workspace 根目录运行：

```bash
cargo run -p mini-harness-v3 -- run "检查项目，完成请求的变更，并测试它。"

# 交互模式：每行创建一个新的 AgentState
cargo run -p mini-harness-v3
```

输入 `/exit`、`exit` 或 `quit` 退出。

## 源码地图

```text
src/
├── main.rs       # CLI、四工具 Registry、每个任务一个 AgentState
├── agent/        # 有界循环、Coding System Prompt、State、Outcome
├── config/       # Provider 配置与脱敏 Debug
├── llm/          # OpenAI-compatible 协议和有 timeout 的 URL 安全 HTTP Client
└── tools/        # 严格 Registry 以及受沙箱保护的 read/list/write/shell 工具
```
