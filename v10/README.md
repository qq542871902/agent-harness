# mini-harness V10 — Controlled Workspace RAG

[English](README.md) · [中文](README.zh-CN.md)

V10 adds the read-only `search_workspace_knowledge` tool to the V9 agent runtime. It supports deterministic local lexical retrieval plus opt-in vector and hybrid RAG while preserving workspace-relative path and line citations.

## Retrieval modes

Select one process-wide mode with `RAG_MODE`:

- `lexical` (default): local substring matching; workspace content never leaves the process.
- `vector`: line chunks are embedded by an OpenAI-compatible `/embeddings` endpoint and ranked by cosine similarity.
- `hybrid`: vector and lexical chunk rankings are fused with reciprocal rank fusion (RRF). If embedding/index construction fails, the tool returns lexical results with a warning.

Vector data is held only in memory. The first semantic search embeds the current workspace chunks; later searches reuse that index until a source content hash changes. Query embeddings are generated for every semantic search. No vector database or on-disk embedding cache is used.

## Configuration

The existing chat variables remain required:

```text
OPENAI_API_KEY=...
OPENAI_BASE_URL=https://provider.example/v1
OPENAI_MODEL=...
```

Lexical mode needs no additional configuration. To enable vector or hybrid retrieval:

```bash
RAG_MODE=hybrid \
EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "Find the authorization flow and cite its implementation."
```

`EMBEDDING_API_KEY` and `EMBEDDING_BASE_URL` are optional when both fall back to the chat provider. If `EMBEDDING_BASE_URL` is set explicitly, `EMBEDDING_API_KEY` is also required; V10 refuses to send the chat credential to an independently configured endpoint. Optional bounded tuning variables are:

```text
RAG_CHUNK_LINES=40                 # 4..200
RAG_CHUNK_OVERLAP_LINES=8          # 0..chunk_lines-1
RAG_EMBEDDING_BATCH_SIZE=16        # 1..64
```

Provider URLs require HTTPS. Plain HTTP is accepted only for an explicit loopback host when `ALLOW_HTTP_LOOPBACK=true`, matching the V9 chat-client policy.

**Data boundary:** `vector` and `hybrid` send non-sensitive workspace chunk text, relative paths, and the search query to the configured embedding provider. Use only a provider authorized to receive that source. `lexical` remains entirely local.

## Tool contract

The model-facing input remains compatible with the lexical implementation:

```json
{
  "query": "DefaultPolicy ToolPermission",
  "top_k": 5
}
```

`query` is required, non-empty after trimming, and bounded to 512 Unicode characters. `top_k` defaults to 5 and must be from 1 through 10. Unknown fields and malformed values are rejected.

A result contains relative paths, 1-based line ranges, bounded snippets, and a mode-specific score:

```json
{
  "query": "where are tool calls authorized",
  "result_count": 1,
  "results": [
    {
      "path": "v9/src/policy/mod.rs",
      "start_line": 1,
      "end_line": 40,
      "score": 32258,
      "lexical_score": 220,
      "vector_similarity": 0.81,
      "snippet": "..."
    }
  ],
  "retrieval": "hybrid_lexical_vector",
  "warning": null,
  "fallback_reason": null
}
```

Lexical scores preserve the original exact-phrase/term-frequency heuristic. Vector scores are non-negative cosine similarity scaled to `0..1000`. Hybrid scores are scaled RRF values and should only be compared within one response. Vector fields are omitted in lexical mode. Retrieval labels are `local_lexical`, `remote_vector`, `hybrid_lexical_vector`, or `local_lexical_fallback`. A fallback also returns a sanitized `fallback_reason` category such as `embedding_authentication`, `embedding_transport`, `embedding_response`, `embedding_dimension`, or `local_index`; it never includes provider response bodies or source text.

Results are navigation hints; the model should use `read_file` to verify complete source context.

## Boundaries

The tool canonicalizes the workspace, never follows symlinks, excludes sensitive paths (`.env` except `.env.example`, `.sessions/`, `traces/`, common package/cloud/container credential files, private keys, and credential directories), and skips `.git/`, `target/`, and `node_modules/`. It scans at most 500 regular files, reads at most 1 MiB per file and 4 MiB in total, and returns at most 10 results with snippets bounded to 600 Unicode characters. `.env.example` remains searchable for documentation compatibility; verify that it contains placeholders rather than usable secrets before enabling a remote mode.

Semantic modes create at most 2,048 overlapping chunks. Each embedding input is bounded to 8,000 Unicode characters, batches contain at most 64 inputs, embedding responses are bounded to 8 MiB, redirects are disabled, and malformed, zero, non-finite, or dimensionally inconsistent vectors are rejected.

`V10Policy` auto-allows only this read-only retrieval tool; V9 policy, approval, context, trace, session, MCP, and native-tool semantics remain in force. Traces retain only generic payload-free tool metadata. The optional embedding API key is included in context redaction. V9 schema-5 sessions persist the normal transcript, so do not resume an incomplete V10 session with V9.

V9 sub-agent role filtering has a closed native-tool allowlist, so retrieval remains available to the parent agent only.

## Run

```bash
# Local lexical retrieval (default)
cargo run -p mini-harness-v10 -- run "Find how DefaultPolicy restricts tool calls and cite the relevant code."

# Semantic retrieval
RAG_MODE=vector EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "Where is authorization enforced?"

# Hybrid retrieval
RAG_MODE=hybrid EMBEDDING_MODEL=text-embedding-3-small \
  cargo run -p mini-harness-v10 -- run "Find authorization checks and cite them."

cargo run -p mini-harness-v10 -- resume 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v10 -- session 550e8400-e29b-41d4-a716-446655440000
cargo run -p mini-harness-v10 -- trace 550e8400-e29b-41d4-a716-446655440000
```

V10 is still not LightRAG: it does not extract entities or relations and has no graph retrieval. Those capabilities can be added behind the same controlled tool boundary without replacing V9's `AgentRunner`, `Policy`, `Session`, or `Trace` layers.
