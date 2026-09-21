# mini-harness V1 — Tool Calling

[English](README.md) · [中文](README.zh-CN.md)

V1 从 V0 的 LLM Client 出发，引入下一个独立能力：

```text
LLM Tool Call → Tool Registry → Rust 函数 → 展示结果
```

新增内容：

- OpenAI-compatible `tools` 请求定义；
- 将 `message.tool_calls` 解析为类型化 `ToolCall`；
- 通用异步 `Tool` Trait 和 `ToolRegistry`；
- 只读 `read_file` 与 `list_files` 工具；
- canonicalize 后的 workspace sandbox 检查。

## 重要边界

V1 **不是** Agent Loop。它执行同一模型响应中的 Tool Call 并展示输出，但不会追加 Observation，也不会再次调用模型。将 Tool Result 回传模型从 V2 开始。

## 配置

从 workspace 根目录执行：

```bash
cp .env.example .env
```

设置 `OPENAI_API_KEY`、`OPENAI_BASE_URL` 与 `OPENAI_MODEL`。

## 运行

从 workspace 根目录执行：

```bash
# 保持纯聊天模式，不发送工具 Schema
cargo run -p mini-harness-v1 -- run "解释 Tool Calling。"

# 启用 V1 的两个只读工具
cargo run -p mini-harness-v1 -- run --tools "列出当前 workspace 的文件。"

# 启用工具的交互模式
cargo run -p mini-harness-v1 -- --tools
```

## 工具与安全性

| 工具 | 功能 | 边界 |
| --- | --- | --- |
| `read_file` | 读取 UTF-8 文本文件。 | 相对路径必须解析到当前工作目录之内。 |
| `list_files` | 列出目录的直接子项。 | 相对路径必须解析到当前工作目录之内。 |

绝对路径、逃出 workspace 的 `..` 路径，以及符号链接逃逸都会被拒绝。使用以上根目录命令启动时，workspace 就是仓库根目录。

## 源码地图

```text
src/
├── main.rs       # 兼容 V0 的 CLI 与 --tools 模式
├── config/       # .env / 环境变量配置
├── llm/          # Tool Definition 协议与 Provider 解码
└── tools/        # Tool Trait、Registry、read_file、list_files
```
