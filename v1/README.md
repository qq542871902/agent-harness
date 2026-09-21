# mini-harness V1 — Tool Calling

[中文文档](README.zh-CN.md)

V1 starts from the V0 LLM client and introduces the next isolated capability:

```text
LLM Tool Call → Tool Registry → Rust Function → Displayed Result
```

It adds:

- OpenAI-compatible `tools` request definitions;
- parsing of `message.tool_calls` into typed `ToolCall` values;
- the common asynchronous `Tool` trait and `ToolRegistry`;
- read-only `read_file` and `list_files` tools;
- canonicalized workspace sandbox checks.

## Important boundary

V1 is **not** an Agent Loop. It executes tool calls received from one model response and displays their output, but does not append observations or call the model again. Sending tool results back to the model begins in V2.

## Configure

From the workspace root:

```bash
cp .env.example .env
```

Set `OPENAI_API_KEY`, `OPENAI_BASE_URL`, and `OPENAI_MODEL`.

## Run

Run commands from the workspace root:

```bash
# Keep V1 in plain chat mode (no schemas are sent)
cargo run -p mini-harness-v1 -- run "Explain Tool Calling."

# Enable the two V1 tools
cargo run -p mini-harness-v1 -- run --tools "List the files in this workspace."

# Tool-enabled interactive mode
cargo run -p mini-harness-v1 -- --tools
```

## Tools and safety

| Tool | Function | Boundary |
| --- | --- | --- |
| `read_file` | Reads a UTF-8 text file. | A relative path must canonicalize inside the current working directory. |
| `list_files` | Lists direct children of a directory. | A relative path must canonicalize inside the current working directory. |

Absolute paths, `..` paths that escape the workspace, and symlink escapes are denied. When launching through the workspace-root commands above, the workspace is the repository root.

## Source map

```text
src/
├── main.rs       # V0-compatible CLI plus --tools mode
├── config/       # .env / environment configuration
├── llm/          # tool-definition protocol and provider decoding
└── tools/        # Tool trait, registry, read_file, list_files
```
