# mini-harness V5 — Structured JSONL Trace

[中文文档](README.zh-CN.md)

V5 is an independent snapshot built on V4. It preserves V4's sandboxed tools, exact shell allowlist, bounded I/O, model-step accounting, typed allow/ask/deny policy, task-scoped approval, and recoverable denial/tool-failure observations. V5 adds a structured trace that can reconstruct one complete agent run.

## Trace model

Every CLI `run` and every accepted interactive task creates a UUID session and writes:

```text
traces/{session_id}.jsonl
```

The CLI prints the session ID and trace path before starting the model loop. Each line is one complete JSON object and is flushed immediately. Every record includes an RFC 3339 UTC `timestamp`, `session_id`, and a serde-tagged `type`. Events cover session start, model requests/responses, tool calls/results, approval requests/decisions, completion, fatal failure, and max-step exhaustion.

`AgentRunner` depends only on the `TraceSink` observer port. `JsonlTraceWriter` is the filesystem adapter, and a test-only `NullTraceSink` preserves simple unit-test composition. Policy, approval, and native-tool decisions are unchanged: denied calls and tool failures emit unsuccessful `tool_result` events and remain same-call-ID observations for the model. Fatal runner errors end with `agent_failed`.

Tool arguments and tool output are intentionally omitted from the trace, preventing credentials in payloads from being persisted. Fatal error text is bounded to 2,048 characters and lines carrying common credential markers are replaced with `[REDACTED]`.

## Configure and run

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL` using the shared configuration, then run from the workspace whose files the tools should access:

```bash
cargo run -p mini-harness-v5 -- run "Inspect the project and summarize it."

# Interactive mode; each task gets a fresh UUID, trace, AgentState, and approval state
cargo run -p mini-harness-v5

# Validate a canonical UUID and safely display its JSONL trace
cargo run -p mini-harness-v5 -- trace 550e8400-e29b-41d4-a716-446655440000
```

The trace directory must be a real directory directly beneath the canonical workspace; symlinked trace directories and symlinked session files are rejected. V5 also reserves `traces/` from the native `write_file` adapter, so an approved model write cannot replace the active audit file; the V4 policy decision itself remains unchanged and any blocked attempt is still a recoverable tool observation.

## Source map

```text
src/
├── main.rs       # CLI composition, one trace per task, safe trace display
├── trace.rs      # TraceEvent, TraceSink/NullTraceSink, JsonlTraceWriter
├── agent/        # V4 loop plus ordered observer events
├── policy/       # unchanged V4 policy and approval behavior
├── config/       # provider configuration with redacted Debug
├── llm/          # OpenAI-compatible protocol and HTTP client
└── tools/        # strict sandboxed read/list/write/shell tools
```

V5 does not add persistent/resumable sessions, arbitrary shell execution, context compression, or production-grade OS isolation.
