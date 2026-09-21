# mini-harness V7 — Context Management

[中文文档](README.zh-CN.md)

V7 is an independent snapshot built from V6. It retains the sandboxed native tools, policy/approval flow, bounded loop, structured tracing, and resumable sessions, then adds a provider-neutral `ContextBuilder` used by `AgentRunner` for every request.

## Context policy

The canonical `AgentState.messages` history is retained in the session (subject only to native tool-output truncation); each model request receives a derived bounded view. The estimator is deliberately dependency-free and deterministic: serialize the complete compact `ChatRequest` JSON—including model identifier, tool definitions, and messages—then divide its UTF-8 byte length by four, rounding up. This is an approximation, not provider billing usage.

Defaults are `CONTEXT_TOKEN_BUDGET=16384`, `MAX_TOOL_OUTPUT_CHARS=32768`, and a 2048-character summary bound. Optional environment overrides are trimmed, parsed as bounded positive integers, and fail clearly when invalid. Provider fields remain redacted in `Config` debug output. Context settings are non-secret and persisted so resume uses the original policy.

Under budget, the request is identical to canonical history. Over budget, V7 always keeps the coding system prompt and original user task, groups every assistant `tool_calls` message atomically with all immediately following same-ID tool results, removes the oldest complete groups first, adds a deterministic bounded system summary, and retains the largest fitting recent suffix. Summaries always retain their structural counts and aggregate tool names/outcomes; only the safe-snippet section is shortened. Known provider secrets are injected transiently into the builder for exact redaction and are never persisted; credential-marked lines and token-like values are also redacted. If required context cannot fit, the run fails locally with a clear fatal error rather than exceeding the configured budget or emitting an invalid protocol transcript.

Native tool observations are truncated before insertion into canonical history/session. Truncation counts Unicode scalar values, preserves balanced head/tail content, and inserts an explicit `original_chars=N` marker. `context_built` and enriched `tool_result` trace events contain only counts, sizes, and booleans—never message or tool-output content.

## Sessions and compatibility

V7 increments the session schema to version 2 and persists context settings plus the latest deterministic summary metadata. V7 deliberately uses strict compatibility: schema-1 V6 sessions and unknown future versions are rejected with an explicit message; no implicit migration is attempted. V0–V6 remain independent and unchanged.

```bash
cargo run -p mini-harness-v7 -- run "Inspect the project and summarize it."
cargo run -p mini-harness-v7 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v7 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v7 -- trace 550e8400-e29b-41d4-a716-446655440000
```

## Source map

```text
src/
├── agent/       # Canonical state and context-managed runner
├── context/     # Estimator, coherent grouping, summaries, Unicode truncation
├── config/      # Redacted provider config and validated context limits
├── llm/         # Provider-neutral protocol and OpenAI-compatible adapter
├── policy/      # Allow/ask/deny policy and transient approvals
├── session.rs   # Strict schema-2 atomic session persistence
├── tools/       # Workspace-confined native tools
├── trace.rs     # Content-free context metadata and JSONL tracing
└── main.rs      # Composition and CLI
```

V7 remains a learning harness, not an exact tokenizer, encrypted store, arbitrary shell, or OS-level sandbox.
