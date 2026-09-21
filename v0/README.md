# mini-harness V0 — LLM CLI

[中文文档](README.zh-CN.md)

V0 is the smallest runnable harness boundary:

```text
User prompt → OpenAI-compatible Chat Completions API → Answer
```

It contains only:

- dotenv-backed configuration;
- a provider-neutral asynchronous `LlmClient` trait;
- one OpenAI-compatible Chat Completions implementation;
- a one-shot and interactive CLI.

It intentionally has **no** tools, Tool Registry, Agent Loop, policy, session, trace, or workspace access. This makes the distinction between a normal LLM chat client and later Agent capabilities explicit.

## Configure

From the workspace root, copy the shared example:

```bash
cp .env.example .env
```

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL`. For DeepSeek, use `https://api.deepseek.com/v1` as the base URL and a supported model such as `deepseek-chat`.

## Run

Run these commands from the workspace root:

```bash
# One request
cargo run -p mini-harness-v0 -- run "What is an Agent Harness?"

# Interactive mode
cargo run -p mini-harness-v0
```

Type `/exit`, `exit`, or `quit` in interactive mode to leave.

## Source map

```text
src/
├── main.rs       # CLI and request dispatch
├── config/       # .env / environment configuration
└── llm/          # client abstraction, wire types, OpenAI-compatible provider
```
