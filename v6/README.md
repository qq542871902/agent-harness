# mini-harness V6 — Persistent Sessions

[中文文档](README.zh-CN.md)

V6 is an independent snapshot built from V5. It preserves the sandboxed tools, exact shell allowlist, policy/approval flow, bounded agent loop, and structured JSONL tracing, then adds durable save/resume under the canonical workspace.

## Session model

Each new task receives a UUID and a versioned `.sessions/{session_id}.json` record containing the canonical workspace, model name, complete `AgentState` transcript, status, cumulative step count, current limit, pending tool calls, trace path, and creation/update timestamps. The file repository rejects unsupported versions, malformed or oversized JSON, UUID/workspace mismatches, symlinked directories/files, and paths that do not remain directly beneath the canonical `.sessions` directory. Writes are bounded, permission-protected on Unix, synced, and atomically installed; initial creation never replaces an existing session. An exclusive per-session lease prevents concurrent resumes from replaying pending side effects or overwriting each other’s checkpoints.

The runner checkpoints creation and every meaningful transition: running/approval status, attempted model requests, model tool-call turns, each tool observation, and terminal status. Persistence failure is fatal and emits a redacted `agent_failed` trace event. Tool calls from an interrupted model turn are retained as pending work so resume completes their observations before requesting another model turn. Task-scoped “always allow” approval caches remain transient and are intentionally not serialized.

```bash
# Start and persist a new session
cargo run -p mini-harness-v6 -- run "Inspect the project and summarize it."

# Continue in the same workspace and append to the same trace
cargo run -p mini-harness-v6 -- resume 550e8400-e29b-41d4-a716-446655440000

# Safely validate and display session or trace data
cargo run -p mini-harness-v6 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v6 -- trace 550e8400-e29b-41d4-a716-446655440000
```

Completed sessions are displayed and not rerun. A transient `WaitingApproval` state is reset safely before resume. `Failed` and `MaxStepsReached` sessions retain their cumulative `step` and deterministically receive 20 additional model-request attempts (`max_steps = prior step + 20`). The saved model is reused even if the current default model differs.

## Security and sensitivity

Unlike traces, session JSON necessarily contains user prompts, model text, tool arguments, and tool observations. Treat `.sessions/` as sensitive application data: prompts or tool output may themselves contain credentials. Provider configuration and API-key fields are never part of the session schema. Both `.sessions/` and `traces/` are reserved from `write_file`, and both directories reject symlink redirection. V6 is still a learning harness, not an OS-level sandbox or encrypted secret store.

## Source map

```text
src/
├── agent/       # Serializable state plus checkpointed run/resume loop
├── config/      # Provider environment validation
├── llm/         # Provider-neutral messages/client and OpenAI adapter
├── policy/      # Allow/ask/deny policy and transient approvals
├── session.rs   # Session schema, repository/sink ports, filesystem adapter
├── tools/       # Confined native tools; session/trace trees reserved
├── trace.rs     # Create/append JSONL writer and safe lookup
└── main.rs      # run, resume, session, trace, and interactive CLI
```

V6 deliberately excludes context compression (V7), arbitrary shell execution, encrypted session storage, and production-grade OS isolation.
