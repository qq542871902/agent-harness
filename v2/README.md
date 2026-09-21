# mini-harness V2 — Agent Loop

[中文文档](README.zh-CN.md)

V2 converts V1's single model/tool exchange into a bounded multi-turn agent runtime:

```text
User task
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
… Final Answer
```

## What V2 adds

- `AgentState`: one task's in-memory message history, current step, maximum steps, and status;
- `AgentRunner`: drives one model request per step;
- transcript-correct OpenAI-compatible messages:
  - assistant tool-call messages retain `tool_calls` and JSON-stringified arguments;
  - tool observations retain the matching `tool_call_id`;
- every tool error becomes a Tool observation so the model can retry or choose another action;
- `max_steps`, defaulting to **20 model requests**, stops a looping task safely.

V2 retains V1's workspace-sandboxed, read-only `read_file` and `list_files` tools. The tool definitions are sent with every model request.

## Deliberate V2 boundary

V2 does not yet provide write operations, shell execution, policies/approval, trace files, persistent sessions, or context compression. Those are later learning stages. A transport or model-protocol error is fatal for this task; native tool failures are observations returned to the model.

## Configure

From the workspace root:

```bash
cp .env.example .env
```

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL`. An OpenAI-compatible provider such as DeepSeek can be used by setting its base URL and model name.

## Run

Run from the workspace root so the repository becomes the read-only tool workspace:

```bash
# One agent task
cargo run -p mini-harness-v2 -- run "Inspect the project and summarize the differences between V0, V1, and V2."

# Interactive mode; each entered task gets a fresh AgentState
cargo run -p mini-harness-v2
```

Type `/exit`, `exit`, or `quit` to leave interactive mode.

## Stop conditions

| Condition | Outcome |
| --- | --- |
| Model returns text without tool calls | The answer is printed and state becomes `Completed`. |
| Model requests tools | Calls execute sequentially, and all results are appended as tool observations for the next step. |
| Tool execution fails | A failure observation is appended; the runner continues. |
| 20 model requests complete without final text | State becomes `MaxStepsReached`; the CLI prints a stop message. |
| API/transport/protocol failure | State becomes `Failed`; the CLI returns the error. |

## Source map

```text
src/
├── main.rs       # CLI and one AgentState per user task
├── agent/        # AgentState, AgentStatus, AgentRunner, AgentOutcome
├── config/       # .env / environment configuration
├── llm/          # complete multi-turn request/response transcript protocol
└── tools/        # Tool trait, registry, read_file, list_files
```
