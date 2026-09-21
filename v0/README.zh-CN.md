# mini-harness V0 — LLM CLI

[English](README.md) · [中文](README.zh-CN.md)

V0 是最小、可运行的 Harness 边界：

```text
用户 Prompt → OpenAI-compatible Chat Completions API → 回答
```

它只包含：

- 基于 dotenv 的配置；
- Provider 无关的异步 `LlmClient` Trait；
- 一个 OpenAI-compatible Chat Completions 实现；
- 单次调用和交互式 CLI。

它刻意**不包含**工具、Tool Registry、Agent Loop、Policy、Session、Trace 或 workspace 文件访问。这让普通 LLM Chat Client 与后续 Agent 能力之间的区别保持清晰。

## 配置

从 workspace 根目录复制共享模板：

```bash
cp .env.example .env
```

设置 `OPENAI_API_KEY`、`OPENAI_BASE_URL` 和 `OPENAI_MODEL`。使用 DeepSeek 时，可将 Base URL 设为 `https://api.deepseek.com/v1`，模型设为 `deepseek-chat` 等已支持模型。

## 运行

请从 workspace 根目录执行：

```bash
# 单次请求
cargo run -p mini-harness-v0 -- run "什么是 Agent Harness？"

# 交互模式
cargo run -p mini-harness-v0
```

在交互模式中输入 `/exit`、`exit` 或 `quit` 退出。每条输入都是独立的单轮请求，不会保留对话历史。

## 源码地图

```text
src/
├── main.rs       # CLI 与请求分发
├── config/       # .env / 环境变量配置
└── llm/          # Client 抽象、Wire Type、OpenAI-compatible Provider
```
