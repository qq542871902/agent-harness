# mini-harness V3 — Coding Agent

[中文文档](README.zh-CN.md)

V3 is an independent snapshot that turns V2's read-only agent loop into a small, bounded coding agent. It keeps the full model → tool → observation loop and adds safe workspace writes and an exact-allowlist command runner.

## Capabilities

- `read_file`: reads UTF-8 regular files up to 1 MiB;
- `list_files`: lists at most 1,000 direct entries with bounded output;
- `write_file`: creates or atomically replaces UTF-8 regular files up to 1 MiB;
- `shell`: directly invokes exactly one of `cargo check`, `cargo test`, `cargo fmt`, `cargo clippy`, `git diff`, or `git status` with a 120-second timeout and bounded stdout/stderr;
- strict JSON arguments for every tool; unknown fields are rejected;
- canonical workspace confinement, traversal/absolute-path rejection, and symlink-escape protection;
- a system instruction to inspect relevant files before modifying and test after changes;
- the V2 20-model-request limit and tool-failure-as-observation behavior.

Shell results are structured JSON containing `exit_code`, `stdout`, and `stderr`. A nonzero command exit is still a successful tool observation so the model can inspect and address failures.

## Safety boundary

The process working directory is the workspace. File tools cannot access paths outside it. `write_file` requires an existing parent directory and rejects symlink targets. Shell input is not interpreted by a shell and does not accept flags, command chains, or commands beyond the six exact entries above.

V3 does not add approvals, arbitrary command execution, persistent sessions, context compression, or production-grade isolation. Canonical path checks reduce accidental escape but are not a replacement for OS sandboxing against a hostile concurrent process.

## Configure and run

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL` using the same configuration as the earlier snapshots. Run from the intended workspace root:

```bash
cargo run -p mini-harness-v3 -- run "Inspect the project, make the requested change, and test it."

# Interactive mode; each line starts a fresh AgentState
cargo run -p mini-harness-v3
```

Type `/exit`, `exit`, or `quit` to leave interactive mode.

## Source map

```text
src/
├── main.rs       # CLI, four-tool registry, and one AgentState per task
├── agent/        # bounded loop, coding system prompt, state, outcomes
├── config/       # provider configuration with redacted Debug
├── llm/          # OpenAI-compatible protocol and timed URL-safe HTTP client
└── tools/        # strict registry and sandboxed read/list/write/shell tools
```
