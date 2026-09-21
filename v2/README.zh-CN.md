# mini-harness V2 — Agent Loop

[English](README.md) · [中文](README.zh-CN.md)

V2 将 V1 的单次模型/工具交换变成有边界的多轮 Agent Runtime：

```text
用户任务
  ↓
LLM
  ↓
Tool Call
  ↓
Tool Registry → Native Tool
  ↓
Tool Observation
  ↓
LLM
  ↓
… 最终回答
```

## V2 新增内容

- `AgentState`：保存单个任务的内存消息历史、当前 step、最大 step 和状态；
- `AgentRunner`：每个 step 驱动一次模型请求；
- 协议正确的 OpenAI-compatible transcript：
  - Assistant Tool Call Message 保留 `tool_calls` 与 JSON 字符串化参数；
  - Tool Observation 保留对应的 `tool_call_id`；
- 每个 Tool Error 都转换为 Tool Observation，使模型可以重试或换一种决策；
- 默认 **20 次模型请求** 的 `max_steps`，安全终止循环任务。

V2 继续使用 V1 的 workspace sandbox 只读 `read_file`、`list_files` 工具，并在每次模型请求中发送 Tool Definition。

## 刻意保留的 V2 边界

V2 尚未实现写入、Shell、Policy/Approval、Trace 文件、持久化 Session 或 Context 压缩；这些属于后续学习阶段。Provider Transport/协议错误对此任务是 fatal，Native Tool 失败则会作为 Observation 回传模型。

## 配置与运行

从 workspace 根目录复制配置并设置 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`OPENAI_MODEL`：

```bash
cp .env.example .env

# 单个 Agent 任务
cargo run -p mini-harness-v2 -- run "检查项目并概括 V0、V1、V2 的区别。"

# 交互模式：每项输入使用新的 AgentState
cargo run -p mini-harness-v2
```

输入 `/exit`、`exit` 或 `quit` 退出。

## 停止条件

| 条件 | 结果 |
| --- | --- |
| 模型返回无 Tool Call 的文本 | 打印答案，状态变为 `Completed`。 |
| 模型请求工具 | 按顺序执行调用，并将全部结果作为 Observation 追加到下一轮。 |
| Tool 执行失败 | 追加失败 Observation，Runner 继续。 |
| 20 次模型请求仍无最终文本 | 状态变为 `MaxStepsReached`，CLI 打印停止信息。 |
| API / Transport / 协议失败 | 状态变为 `Failed`，CLI 返回错误。 |

## 源码地图

```text
src/
├── main.rs       # CLI；每个用户任务一个 AgentState
├── agent/        # AgentState、AgentStatus、AgentRunner、AgentOutcome
├── config/       # .env / 环境变量配置
├── llm/          # 完整多轮请求/响应 transcript 协议
└── tools/        # Tool Trait、Registry、read_file、list_files
```
