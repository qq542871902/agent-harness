# mini-harness V8 — Bounded MCP stdio

[中文文档](README.zh-CN.md)

V8 is an independent snapshot built from V7. It retains V7’s native tools, policy/approval flow, bounded context, structured traces, and resumable sessions, then adapts a deliberately small MCP client into the same dynamic `Tool` trait. V0–V7 remain independent and unchanged.

## Supported MCP subset

V8 implements the stable **2025-06-18** lifecycle subset needed for stdio tools; it does not claim full MCP support and does not claim compatibility with an unspecified “2026 protocol.” The implementation follows the official [lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle), [stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports), and [tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools) documentation.

Supported behavior is intentionally bounded:

- A user-selected `MCP_CONFIG_PATH` launches enabled servers as direct executable plus argument vectors—never through a shell. Without that variable, V8 registers native tools only.
- Transport is newline-delimited UTF-8 JSON-RPC 2.0 over child stdin/stdout. V8 sends `initialize` with `protocolVersion: "2025-06-18"`, client information, and empty client capabilities, validates the selected version, then sends `notifications/initialized`.
- `tools/list` follows `nextCursor` for at most 100 pages and 256 tools per server. `tools/call` supports text content, `structuredContent`, and `isError`. Other content types and MCP features are rejected as unsupported.
- Client request IDs are increasing numbers and response IDs must match. Server notifications are ignored. Unsupported server-to-client requests receive JSON-RPC `-32601` responses so neither peer deadlocks.
- Startup, requests, shutdown, lines, mapped results, schemas, descriptions, tool counts, and retained stderr are bounded. Production defaults are 10 seconds for each startup stage, 15 seconds per request, 1 second graceful shutdown, 1 MiB per line/result, and 64 KiB retained stderr metadata. Stderr is continuously drained but never logged.
- The client closes stdin for graceful shutdown, then terminates the complete child process group after the shutdown bound. Drop is a kill fallback.
- Remote failures become structured `McpProtocolError` values. MCP `isError` becomes `ToolOutput.success = false`, allowing the normal agent loop to observe the failure.

This is not Streamable HTTP, resources, prompts, roots, elicitation, sampling, completion, subscriptions, authorization, dynamic list-change handling, binary/image/audio content, or general future-version support.

Content based on the linked MCP documentation was rephrased for compliance with licensing restrictions.

## Configuration and security boundary

Copy `mcp.example.json` outside the repository if desired, edit it, and set an absolute path:

```bash
MCP_CONFIG_PATH=/absolute/path/to/mcp.json \
  cargo run -p mini-harness-v8 -- run "Use the configured tools to inspect the task."
```

The strict version-1 JSON shape is:

```json
{
  "version": 1,
  "servers": [
    {
      "name": "local",
      "enabled": true,
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"],
      "working_directory": "/absolute/existing/directory",
      "env_allowlist": ["HOME"]
    }
  ]
}
```

Unknown fields, unsupported versions, files over 64 KiB, symlink config files, duplicate or invalid names, empty/NUL/non-absolute commands, invalid working directories, NUL arguments, and malformed/duplicate environment names are rejected. Server and remote tool components use 1–48 ASCII letters, digits, `_`, or `-`, excluding ambiguous `__`. A command must name an existing absolute file. `working_directory` may be `null`.

`env_allowlist` contains **names only**. The child environment is cleared, then present values for those names are inherited from the harness process. Secret values are never embedded in this config format, logged, traced, or added to the MCP session manifest. Do not place values in `args` if they must remain secret.

**Configured MCP commands are trusted user configuration, never model-generated. They execute outside the native filesystem-tool sandbox and can access anything permitted by the operating-system account.** Review executables, arguments, working directories, and allowlisted environment names before enabling a server.

Discovered tools are exposed deterministically as `mcp__<server>__<tool>`. Invalid/oversized names, non-object or oversized input schemas, oversized descriptions, and registry collisions fail startup. Native tools remain unchanged. `DefaultPolicy` returns `Ask` for every valid `mcp__` tool regardless of annotations; malformed names are denied, and denial wins. The approval prompt shows redacted arguments before execution, and task-scoped “always allow” remains isolated to the full namespaced tool.

## Sessions and traces

V8 uses strict session schema 3. It persists only the sorted namespaced MCP tool manifest, never commands, arguments, configuration paths, environment values, or other MCP secrets. Resuming a session that used MCP requires a current `MCP_CONFIG_PATH`. If a previously available tool is absent from the current configuration, a pending call is converted to the same failed tool observation used for other unavailable tools, allowing the model to continue with the current manifest.

Per-session JSONL traces add `mcp_server_connected`, `mcp_tool_registered`, and `mcp_call_result` metadata with server/name/count/success only. JSON-RPC arguments, schemas, content, structured results, stderr, command configuration, and environment values are omitted.

```bash
cargo run -p mini-harness-v8 -- run "Inspect the project and summarize it."
cargo run -p mini-harness-v8 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v8 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v8 -- trace 550e8400-e29b-41d4-a716-446655440000
```

## Source map and validation

```text
src/
├── agent/       # Source-neutral agent loop and MCP metadata tracing
├── context/     # Deterministic context budgets and output truncation
├── mcp/         # Strict config, bounded JSON-RPC stdio client, Tool adapter
├── policy/      # Native rules plus Ask-by-default MCP policy
├── session.rs   # Strict schema-3 persistence with non-secret MCP manifest
├── tools/       # Dynamic-safe Tool trait, registry, and native tools
├── trace.rs     # Payload-free native/MCP JSONL metadata
└── main.rs      # Connect/register/reuse/shutdown lifecycle
tests/
└── mcp_protocol.rs # Shell-free deterministic mock-server protocol tests
```

Validation:

```bash
cargo fmt --all -- --check
cargo test -p mini-harness-v8 --all-targets
cargo check -p mini-harness-v8 --all-targets
cargo clippy -p mini-harness-v8 --all-targets -- -D warnings
```

V8 remains a learning harness, not a complete MCP host, an encrypted store, or an OS-level sandbox.
