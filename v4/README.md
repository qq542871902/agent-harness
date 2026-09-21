# mini-harness V4 — Policy and Approval

[中文文档](README.zh-CN.md)

V4 is an independent snapshot built on V3's bounded coding agent. It retains V3's strict workspace confinement, atomic bounded writes, exact shell allowlist, bounded process I/O, model-step limit, and recoverable tool observations, then adds a policy decision before every tool dispatch and an injectable human-approval port.

## Default policy

| Tool call | Decision |
| --- | --- |
| `read_file`, `list_files` | Allow |
| `write_file` | Ask |
| `shell` with exactly `cargo test`, `cargo check`, or `git diff` | Allow |
| `shell` with exactly `cargo fmt`, `cargo clippy`, or `git status` | Ask |
| Other non-dangerous shell strings | Ask, then the native exact allowlist still applies |
| Dangerous or malformed shell input | Deny |
| Unknown tools | Deny |

Dangerous command classification is based on typed shell arguments and tokenized command structure, not substring matching. Policy approval never expands native capability: even an approved shell request must still be one of V3's six exact commands, runs without a shell, and remains subject to its timeout and output bounds.

For an `Ask` decision, the console displays the tool and pretty JSON arguments, redacts values under common secret-bearing keys, and loops for `y` (grant once), `n` (deny), or `a` (always allow this tool). An always-allow choice is held in a dedicated in-memory approval state for the current agent task only. Every call is policy-checked first, so `Deny` always wins over that override. A rejected or policy-denied request is appended as a same-call-ID tool observation and the model gets another turn.

## Noninteractive safety

One-shot `run` uses the same console approver and may prompt before writes, `cargo fmt`, `cargo clippy`, or `git status`. If stdin is unavailable or reaches EOF, approval resolves to denial; V4 does not hang waiting forever or approve implicitly. Each interactive prompt starts a fresh task and therefore a fresh always-allow state.

## Configure and run

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL` using the same configuration as the earlier snapshots. Run from the intended workspace root:

```bash
cargo run -p mini-harness-v4 -- run "Inspect the project, make the requested change, and test it."

# Interactive mode; each line starts a fresh AgentState and approval state
cargo run -p mini-harness-v4
```

Type `/exit`, `exit`, or `quit` to leave interactive mode.

## Source map

```text
src/
├── main.rs       # CLI composition and console approver
├── agent/        # bounded loop, policy gate, coding prompt, state, outcomes
├── policy/       # typed DefaultPolicy, ApprovalHandler, and task-local overrides
├── config/       # provider configuration with redacted Debug
├── llm/          # OpenAI-compatible protocol and timed URL-safe HTTP client
└── tools/        # V3 strict registry and sandboxed read/list/write/shell tools
```

V4's scope is policy and approval only. It does not add arbitrary shell execution, persistent approvals or sessions, JSONL tracing, context compression, or production-grade OS isolation.
