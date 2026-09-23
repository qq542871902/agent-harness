# mini-harness

[中文文档](README.zh-CN.md)

A staged Rust project for learning how an Agent Harness is built. Each directory is a self-contained learning snapshot rather than an ever-growing single application.

```text
agent-harness/
├── mini-harness-spec.md  # Full project specification
├── .env.example          # Shared OpenAI-compatible configuration template
├── v0/                   # User → LLM → Answer
├── v1/                   # LLM Tool Call → Tool Registry → Native Tool
├── v2/                   # LLM → Tool → Observation → LLM → Final Answer
├── v3/                   # Coding agent with sandboxed write and shell tools
├── v4/                   # Policy-checking agent with human approval
├── v5/                   # Complete per-run structured JSONL trace
├── v6/                   # Persistent, resumable versioned sessions
├── v7/                   # Deterministic token-budgeted context management
├── v8/                   # Bounded MCP 2025-06-18 stdio tool adapter
├── v9/                   # Same-process bounded role sub-agents
└── v10/                  # Controlled, cited workspace retrieval
```

## Stages

| Directory | Focus | Includes | Deliberately excludes |
| --- | --- | --- | --- |
| [`v0/`](v0/README.md) | LLM CLI | Config, `LlmClient`, one OpenAI-compatible provider, one-shot and interactive CLI | Tools, agent loop, sessions, policy, tracing |
| [`v1/`](v1/README.md) | Tool Calling | V0 foundation plus tool schemas, Tool Call parsing, registry, `read_file`, `list_files` | Tool results fed back to model, multi-step Agent Loop |
| [`v2/`](v2/README.md) | Agent Loop | V1 tools plus in-memory AgentState, tool observations, multi-turn loop, max-step limit | Write/shell tools, policy, approval, session persistence, tracing |
| [`v3/`](v3/README.md) | Coding Agent | V2 loop plus strict sandboxed `write_file`, exact-allowlist `shell`, bounded I/O, coding system prompt | Arbitrary shell, approval flow, persistent sessions, OS-level isolation |
| [`v4/`](v4/README.md) | Policy / Approval | V3 foundation plus typed allow/ask/deny policy, CLI approval, task-scoped always-allow, denial observations | Arbitrary shell, persistent approval/session state, tracing, OS-level isolation |
| [`v5/`](v5/README.md) | Structured Trace | V4 behavior plus UUID-scoped, flush-on-event JSONL tracing, observer port, safe trace display | Persistent/resumable sessions, context compression, arbitrary shell, OS-level isolation |
| [`v6/`](v6/README.md) | Persistent Sessions | V5 behavior plus versioned atomic session checkpoints, safe resume, cumulative continuation budgets, append-mode tracing | Context compression, encrypted storage, arbitrary shell, OS-level isolation |
| [`v7/`](v7/README.md) | Context Management | V6 behavior plus request token budgets, coherent history compaction, deterministic summaries, Unicode-safe tool-output truncation | Exact provider tokenization, encrypted storage, arbitrary shell, OS-level isolation |
| [`v8/`](v8/README.md) | Bounded MCP stdio | V7 behavior plus strict user-selected config, MCP 2025-06-18 lifecycle, paginated tool discovery, namespaced adapters, Ask policy, bounded process lifecycle | Full/future MCP, HTTP transport, server-initiated capabilities, MCP sandboxing |
| [`v9/`](v9/README.md) | Same-process sub-agents | V8 behavior plus `spawn_agent`, role-filtered registry snapshots, shared runtime ports, bounded delegation, payload-free lineage tracing, schema-5 execution-journal resume | Recursive delegation, concurrent orchestration, separate child sessions/processes |
| [`v10/`](v10/README.md) | Controlled workspace RAG | V9 runtime plus `search_workspace_knowledge`, local lexical retrieval, opt-in OpenAI-compatible vector/hybrid retrieval, in-memory cosine index, path/line citations, bounded scanning, and sensitive-path exclusion | Persistent vector database, entity-relation graph, LightRAG, child retrieval capability |

## Configure once

From the workspace root:

```bash
cp .env.example .env
```

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL`. The same OpenAI-compatible configuration works with providers such as DeepSeek.

## Run a specific stage

```bash
# V0: pure LLM CLI
cargo run -p mini-harness-v0 -- run "Explain an Agent Harness."

# V1: permit the model to select one of the read-only tools once
cargo run -p mini-harness-v1 -- run --tools "List the files in this workspace."

# V2: continue after tool observations until a final answer or max_steps
cargo run -p mini-harness-v2 -- run "Inspect this workspace and summarize its stages."

# V3: inspect, edit within the workspace, and run exact-allowlist checks
cargo run -p mini-harness-v3 -- run "Inspect the project, update its documentation, and test the change."

# V4: apply policy before dispatch and prompt for approval when required
cargo run -p mini-harness-v4 -- run "Inspect the project, update its documentation, and test the change."

# V5: retain V4 decisions and write a complete UUID-scoped JSONL trace
cargo run -p mini-harness-v5 -- run "Inspect the project and summarize its stages."

# Safely validate and display one V5 trace
cargo run -p mini-harness-v5 -- trace <session-id>

# V6: persist every task and resume it with the same transcript and trace
cargo run -p mini-harness-v6 -- run "Inspect the project and summarize its stages."
cargo run -p mini-harness-v6 -- resume <session-id>
cargo run -p mini-harness-v6 -- session <session-id>
# V7: bound complete requests while preserving mandatory prompts and tool protocol
CONTEXT_TOKEN_BUDGET=16384 MAX_TOOL_OUTPUT_CHARS=32768 \
  cargo run -p mini-harness-v7 -- run "Inspect the project and summarize its stages."
cargo run -p mini-harness-v7 -- resume <session-id>

# V8: optionally discover bounded MCP stdio tools from explicit user configuration
MCP_CONFIG_PATH=/absolute/path/to/mcp.json \
  cargo run -p mini-harness-v8 -- run "Use configured tools to inspect the task."
# Without MCP_CONFIG_PATH, V8 runs with native tools only.
cargo run -p mini-harness-v8 -- run "Inspect the project with native tools."

# V9: delegate bounded research/code/test tasks in the same harness runtime
cargo run -p mini-harness-v9 -- run "Delegate repository research, then summarize it."

# V10: local lexical retrieval by default; opt in to vector/hybrid RAG with embeddings
cargo run -p mini-harness-v10 -- run "Find how DefaultPolicy restricts tool calls and cite the relevant code."
RAG_MODE=hybrid EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "Find where tool-call authorization is enforced and cite it."
```

V4 `run` may prompt on stdin before writes and selected shell commands. If stdin is unavailable or reaches EOF, the request is denied and returned to the model as a tool observation rather than hanging or executing silently. V5 preserves these decisions while recording each task under `traces/{session_id}.jsonl`; tool payloads are omitted so credentials are not persisted in traces. V6 additionally stores the complete resumable transcript under `.sessions/{session_id}.json`; this expected model/user/tool content is sensitive, while provider API keys are not part of the schema. V7 additionally derives a budgeted request view while retaining canonical session history, compacts only complete coherent message groups, records deterministic redacted summary metadata, and truncates native observations by Unicode characters before persistence. Its strict schema-2 loader clearly rejects older V6 sessions rather than migrating them implicitly. V8 adds an explicitly configured, bounded MCP 2025-06-18 stdio subset: discovered tools are namespaced into the shared registry and always require approval by default. MCP server processes run outside the native filesystem sandbox; see [`v8/README.md`](v8/README.md) before enabling one. V8 schema-3 sessions persist only non-secret tool identities and require current MCP configuration on resume. V9 adds approval-gated `spawn_agent` delegation to same-process child `AgentRunner`s that share the current LLM, context policy, approval/policy ports, trace, and role-filtered Arc-backed tool snapshots. Child state is in-memory, recursion is structurally absent, and schema 5 persists the non-secret sub-agent capability marker plus a durable in-flight side-effect journal so resume converts interrupted writes, shell calls, MCP calls, and delegations into indeterminate observations rather than silently replaying them. Child-internal trace events carry explicit child lineage. All later snapshots require HTTPS for provider credentials unless an explicit loopback-only local HTTP opt-in is configured; shell children receive a minimal non-secret environment, and model file tools exclude credential/session/trace paths. MCP protocol timeouts or desynchronization terminate and quarantine the configured server. Both harness-owned subtrees are reserved from `write_file`.

The root `Cargo.toml` is a virtual workspace. Select a stage explicitly with Cargo's `-p` option so the command and source code always match the concept being studied.
