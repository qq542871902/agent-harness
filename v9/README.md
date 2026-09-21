# mini-harness V9 — Same-process sub-agents

[中文文档](README.zh-CN.md)

V9 is an independent snapshot built from V8. It preserves V8’s native/MCP tools, policy and approval flow, bounded context, JSONL traces, and resumable parent sessions, then adds approval-gated bounded delegation through `spawn_agent`. V0–V8 remain independent and unchanged.

## `spawn_agent` contract

The model-visible tool accepts one strict object:

```json
{"role":"research","task":"Inspect the relevant code","max_steps":4}
```

- `role` is exactly `research`, `code`, or `test`.
- `task` is non-empty after trimming and at most 4,096 Unicode characters.
- `max_steps` is optional (default 4) and must be an integer from 1 through 8.
- Unknown fields and malformed values are rejected before a child starts.
- Child execution is bounded to 120 seconds and returned output is bounded to 16,384 Unicode characters. Final answers, maximum-step outcomes, timeouts, and fatal failures all become ordinary bounded parent tool observations.

`DefaultPolicy` returns `Ask` for `spawn_agent`. Approval denial prevents child construction/execution and becomes a parent observation. Console approvals are awaitable and bounded to 60 seconds, so they cannot defeat the child deadline. Once started, every child tool call independently passes through the same policy and approval handler; policy denial always wins.

## Runtime and capability model

`SubAgentSpawner` is the execution port and `SpawnAgentTool` is its tool adapter. `SameProcessSubAgentSpawner` runs a child `AgentRunner` in the existing Tokio process—sub-agents are never launched as operating-system processes. A permitted child `shell` tool may still launch one of the same exact-allowlist validation commands inherited from V8.

Each child shares the parent task’s `Arc<dyn LlmClient>`, model, `ContextBuilder` settings and secret-redaction values, `DefaultPolicy`/`ApprovalHandler`, parent trace sink, and an Arc-backed snapshot of the current native/MCP registry. It receives its own in-memory `AgentState`, approval state, step budget, role-specific bounded system prompt, and no persistent child session.

Capabilities are deterministic:

| Role | Native tools | MCP tools |
| --- | --- | --- |
| `research` | `read_file`, `list_files` | All currently registered MCP tools |
| `code` | `read_file`, `list_files`, `write_file`, `shell` | All currently registered MCP tools |
| `test` | `read_file`, `list_files`, `shell` | All currently registered MCP tools |

Child snapshots never contain `spawn_agent`, making recursive delegation structurally impossible. `ToolRegistry` uses sorted `BTreeMap<String, Arc<dyn Tool>>` storage, so clones and filtered snapshots are cheap and deterministic. No registry lock or mutable registry borrow is held while awaiting a child. Shell process groups have a cancellation guard, so an outer child timeout cannot strand compiler subprocesses. V8’s main-owned `McpManager` still owns server lifecycle and remains alive until the parent command and all directly awaited child work finish.

Interactive prompts create a fresh task-scoped registry and spawn adapter, binding delegation to that prompt’s trace. Resume loads schema 5 and reconstructs the same capability from the current native/MCP snapshot.

## Sessions and traces

V9 session schema 5 explicitly rejects every older/future schema. In addition to V8 fields it persists `sub_agent_enabled: true`, a non-secret capability marker, and an in-flight side-effect marker. This marker prevents silent automatic replay on resume: an interrupted write, shell, MCP, or `spawn_agent` call becomes a same-ID indeterminate parent observation so the model can reconcile external state. Child prompts, tasks, state, output, and separate session records are not persisted. The bounded child result is persisted naturally as the parent tool observation.

The shared parent JSONL trace adds `sub_agent_started`, `sub_agent_completed`, and `sub_agent_failed`. These events contain only a unique child UUID, parent tool-call ID, role, step count, and status. Child-internal runner events carry `actor: child` with the same lineage, so they cannot be confused with parent lifecycle events. They never contain delegated tasks, prompts, or output. Existing model/tool events remain payload-free.

## Run and validate

```bash
cargo run -p mini-harness-v9 -- run "Delegate repository research and summarize it."
cargo run -p mini-harness-v9 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v9 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v9 -- trace 550e8400-e29b-41d4-a716-446655440000

cargo fmt --all -- --check
cargo test -p mini-harness-v9 --all-targets
cargo check -p mini-harness-v9 --all-targets
cargo clippy -p mini-harness-v9 --all-targets -- -D warnings
```

## Source map

```text
src/agent/sub_agent.rs    # Same-process runtime, role prompts, limits, lineage
src/tools/spawn_agent.rs  # Strict adapter and SubAgentSpawner port
src/tools/registry.rs     # Deterministic Arc clone/filter snapshots
src/session.rs            # Strict schema-5 parent persistence and execution journal
src/trace.rs              # Payload-free sub-agent lifecycle events
src/main.rs               # Task-scoped run/interactive/resume composition
```

V9 deliberately excludes recursive/multi-level delegation, arbitrary concurrency, persistent child sessions, arbitrary shell, encrypted session storage, and OS-level sandboxing. MCP commands remain trusted user configuration running outside the native filesystem sandbox; review [`v8/README.md`](../v8/README.md) before enabling them.
