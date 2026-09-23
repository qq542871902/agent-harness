
# mini-harness 项目规格书

## 1. 项目名称

**mini-harness**

一个使用 Rust 实现的最小可用 Agent Harness，用于理解和实践 Agent Runtime 的核心机制，包括：

- LLM 调用
- Tool Calling
- Agent Loop
- Session / State
- Tool Registry
- Policy / Approval
- Trace / Replay
- Context 管理
- MCP 扩展
- Sub-Agent 扩展

---

## 2. 项目目标

本项目的目标不是构建完整的 AI Agent 框架，而是通过一个小型、可运行、可调试、可扩展的 Rust 项目，理解 Agent Harness 的核心运行机制。

最终实现一个简单的 Coding Agent：

> 用户输入任务后，Agent 可以读取项目文件、分析代码、调用工具、运行 `cargo test`，根据执行结果继续迭代，并最终给出结果。

典型流程：

```text
User Task
   ↓
Context Builder
   ↓
LLM
   ↓
Tool Call
   ↓
Tool Registry
   ↓
Policy Check
   ↓
Tool Executor
   ↓
Observation
   ↓
Update State
   ↓
LLM
   ↓
...
   ↓
Final Answer
```

---

## 3. 学习目标

完成项目后，应能够解释并实现以下概念：

1. Agent 与普通 LLM Chat 的区别
2. Agent Harness 的职责
3. Tool Calling 的真实执行机制
4. Agent Loop 如何驱动多步任务
5. Session 与 Agent State 如何管理
6. Tool Registry 如何进行工具分发
7. Policy 如何限制危险操作
8. Trace 如何记录 Agent 行为
9. Context 如何控制输入长度
10. MCP 如何接入外部工具
11. Sub-Agent 如何在 Harness 中调度

---

## 4. 非目标

第一阶段不实现以下功能：

- Web UI
- 分布式 Agent
- 多租户
- 向量数据库
- RAG
- 长期 Memory
- 复杂工作流 DAG
- Kubernetes 部署
- 高并发 Agent Server
- 完整 IDE 集成

这些功能不属于理解 Agent Harness 核心机制所必需的内容。

---

# 5. 技术栈

## 5.1 编程语言

```text
Rust 2024 Edition
```

---

## 5.2 Async Runtime

```text
tokio
```

用于：

- LLM HTTP 请求
- Tool 异步执行
- Shell 子进程
- 并发扩展

---

## 5.3 HTTP

```text
reqwest
```

用于调用 LLM API。

---

## 5.4 Serialization

```text
serde
serde_json
```

用于：

- LLM Request / Response
- Tool Arguments
- Tool Schema
- Trace Event
- Session 持久化

---

## 5.5 Error Handling

```text
anyhow
thiserror
```

---

## 5.6 Logging / Trace

```text
tracing
tracing-subscriber
```

---

## 5.7 Environment

```text
dotenvy
```

---

## 5.8 初始 Cargo.toml

```toml
[package]
name = "mini-harness"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "1", features = ["full"] }

reqwest = { version = "0.12", features = ["json"] }

serde = { version = "1", features = ["derive"] }
serde_json = "1"

async-trait = "0.1"

anyhow = "1"
thiserror = "2"

tracing = "0.1"
tracing-subscriber = "0.3"

dotenvy = "0.15"

uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
```

---

# 6. 项目目录结构

```text
mini-harness/
├── Cargo.toml
├── README.md
├── .env.example
├── .gitignore
│
├── src/
│   ├── main.rs
│   │
│   ├── agent/
│   │   ├── mod.rs
│   │   ├── agent.rs
│   │   ├── runner.rs
│   │   └── state.rs
│   │
│   ├── llm/
│   │   ├── mod.rs
│   │   ├── client.rs
│   │   ├── openai.rs
│   │   └── types.rs
│   │
│   ├── tools/
│   │   ├── mod.rs
│   │   ├── tool.rs
│   │   ├── registry.rs
│   │   ├── read_file.rs
│   │   ├── write_file.rs
│   │   ├── list_files.rs
│   │   └── shell.rs
│   │
│   ├── context/
│   │   ├── mod.rs
│   │   ├── builder.rs
│   │   └── message.rs
│   │
│   ├── session/
│   │   ├── mod.rs
│   │   └── session.rs
│   │
│   ├── policy/
│   │   ├── mod.rs
│   │   └── approval.rs
│   │
│   ├── trace/
│   │   ├── mod.rs
│   │   ├── event.rs
│   │   └── writer.rs
│   │
│   └── config/
│       ├── mod.rs
│       └── config.rs
│
├── traces/
│   └── .gitkeep
│
└── tests/
    ├── agent_loop.rs
    ├── tool_registry.rs
    └── policy.rs
```

---

# 7. 核心架构

```text
                ┌─────────────┐
                │    User     │
                └──────┬──────┘
                       │
                       ▼
                ┌─────────────┐
                │   Session   │
                └──────┬──────┘
                       │
                       ▼
              ┌─────────────────┐
              │ Context Builder │
              └────────┬────────┘
                       │
                       ▼
                 ┌──────────┐
                 │   LLM    │
                 └────┬─────┘
                      │
          ┌───────────┴───────────┐
          │                       │
          ▼                       ▼
   Final Response             Tool Call
                                  │
                                  ▼
                         ┌────────────────┐
                         │ Policy Checker │
                         └───────┬────────┘
                                 │
                                 ▼
                         ┌────────────────┐
                         │ Tool Registry  │
                         └───────┬────────┘
                                 │
                                 ▼
                         ┌────────────────┐
                         │ Tool Executor  │
                         └───────┬────────┘
                                 │
                                 ▼
                           Observation
                                 │
                                 ▼
                            Agent State
                                 │
                                 └──────────→ LLM
```

---

# 8. 核心模块规格

# 8.1 LLM Client

目标：

对具体模型供应商进行抽象。

接口：

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(
        &self,
        request: ChatRequest,
    ) -> anyhow::Result<ModelResponse>;
}
```

第一版只实现一个 Provider。

建议：

```text
OpenAI-compatible API
```

Provider 不应该负责：

- Tool 执行
- Session
- Policy
- Agent Loop

Provider 只负责：

```text
ChatRequest → HTTP → ModelResponse
```

---

# 8.2 Message

统一定义上下文中的消息。

```rust
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}
```

建议结构：

```rust
pub struct Message {
    pub role: Role,
    pub content: String,
    pub tool_call_id: Option<String>,
}
```

---

# 8.3 ModelResponse

模型响应至少支持两种情况：

```rust
pub enum ModelResponse {
    Final {
        content: String,
    },

    ToolCalls {
        calls: Vec<ToolCall>,
    },
}
```

ToolCall：

```rust
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}
```

---

# 8.4 Tool Trait

所有工具实现统一接口。

```rust
#[async_trait]
pub trait Tool: Send + Sync {

    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    fn schema(&self) -> serde_json::Value;

    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> anyhow::Result<ToolOutput>;
}
```

ToolOutput：

```rust
pub struct ToolOutput {
    pub content: String,
    pub success: bool,
}
```

---

# 8.5 Tool Registry

职责：

- 注册 Tool
- 查询 Tool
- 返回 Tool Schema
- 根据 Tool 名称执行

建议：

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}
```

主要接口：

```rust
impl ToolRegistry {

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static;

    pub fn definitions(&self) -> Vec<ToolDefinition>;

    pub async fn execute(
        &self,
        call: &ToolCall,
    ) -> anyhow::Result<ToolOutput>;
}
```

---

# 9. 第一版工具

## 9.1 read_file

输入：

```json
{
  "path": "src/main.rs"
}
```

输出：

```text
文件文本内容
```

安全等级：

```text
ALLOW
```

限制：

- 只能读取 workspace 内文件
- 禁止读取 workspace 外路径

---

## 9.2 list_files

输入：

```json
{
  "path": "src"
}
```

输出：

```text
src/main.rs
src/agent.rs
src/tools.rs
```

安全等级：

```text
ALLOW
```

---

## 9.3 write_file

输入：

```json
{
  "path": "src/main.rs",
  "content": "..."
}
```

安全等级：

```text
ASK
```

限制：

- 只能修改 workspace
- 禁止 `../`
- 禁止绝对路径

---

## 9.4 shell

输入：

```json
{
  "command": "cargo test"
}
```

输出：

```text
stdout
stderr
exit_code
```

安全等级：

```text
ASK
```

第一版只允许白名单命令：

```text
cargo check
cargo test
cargo fmt
cargo clippy
git diff
git status
```

禁止：

```text
sudo
rm
shutdown
reboot
curl | sh
wget | sh
```

---

# 10. Agent State

```rust
pub struct AgentState {

    pub session_id: Uuid,

    pub status: AgentStatus,

    pub messages: Vec<Message>,

    pub step: usize,

    pub max_steps: usize,
}
```

状态：

```rust
pub enum AgentStatus {
    Ready,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    MaxStepsReached,
}
```

默认：

```text
max_steps = 20
```

---

# 11. Agent Loop

Agent Loop 是整个 Harness 的核心。

伪代码：

```rust
while state.step < state.max_steps {

    state.step += 1;

    let request =
        context_builder.build(&state, &tools);

    trace.model_request(&request);

    let response =
        llm.chat(request).await?;

    trace.model_response(&response);

    match response {

        ModelResponse::Final { content } => {

            state.status =
                AgentStatus::Completed;

            return Ok(content);
        }

        ModelResponse::ToolCalls { calls } => {

            for call in calls {

                let decision =
                    policy.check(&call);

                match decision {

                    Allow => {
                        execute_tool(call);
                    }

                    Ask => {
                        ask_user_approval(call);
                    }

                    Deny => {
                        append_denied_result(call);
                    }
                }
            }
        }
    }
}
```

超过：

```text
max_steps
```

必须终止。

---

# 12. Context Builder

职责：

构建每次发送给模型的上下文。

输入：

```text
System Prompt
+
Conversation Messages
+
Tool Definitions
+
Agent State
```

第一版不实现复杂压缩。

只需要：

```text
System Prompt
User / Assistant History
Tool Results
Tool Schemas
```

---

# 13. System Prompt

默认 Prompt：

```text
You are a coding agent running inside a restricted workspace.

You may inspect files and run approved development tools.

Rules:

1. Inspect relevant files before modifying them.
2. Do not access files outside the workspace.
3. Use tools when necessary.
4. After modifying code, run appropriate tests.
5. Do not repeatedly call the same tool without progress.
6. When the task is complete, return a concise final answer.
```

---

# 14. Policy System

Policy 将 Tool 分为三类：

```rust
pub enum ToolPermission {
    Allow,
    Ask,
    Deny,
}
```

默认规则：

| Tool | Permission |
|---|---|
| read_file | Allow |
| list_files | Allow |
| write_file | Ask |
| shell:cargo test | Allow |
| shell:cargo check | Allow |
| shell:cargo fmt | Ask |
| shell:git diff | Allow |
| shell:其他命令 | Ask |
| 危险命令 | Deny |

---

# 15. Approval

当 Tool Permission 为：

```text
ASK
```

CLI 显示：

```text
Agent wants to execute:

Tool:
write_file

Arguments:
{
  "path": "src/main.rs"
}

Approve?

[y] yes
[n] no
[a] always allow this tool
```

用户选择结果必须写入 Trace。

---

# 16. Trace

所有 Agent 行为写入：

```text
traces/{session_id}.jsonl
```

每行一个 JSON。

例如：

```json
{"type":"session_started","session_id":"xxx"}

{"type":"model_request","step":1}

{"type":"model_response","step":1}

{"type":"tool_call","tool":"read_file"}

{"type":"tool_result","tool":"read_file","success":true}

{"type":"approval_requested","tool":"write_file"}

{"type":"approval_granted","tool":"write_file"}

{"type":"session_completed","step":6}
```

---

# 17. Trace Event

建议定义：

```rust
pub enum TraceEvent {

    SessionStarted,

    ModelRequest,

    ModelResponse,

    ToolCall,

    ToolResult,

    ApprovalRequested,

    ApprovalGranted,

    ApprovalDenied,

    AgentCompleted,

    AgentFailed,
}
```

---

# 18. Session

Session 保存：

```text
Session ID
Messages
Agent State
Trace Path
Workspace
```

第一版可以只保存在内存。

第二阶段增加：

```text
.sessions/{session_id}.json
```

支持：

```bash
mini-harness resume <session-id>
```

---

# 19. CLI

第一版接口：

```bash
mini-harness
```

进入交互模式：

```text
mini-harness>

> fix the failing unit tests

Agent:
...
```

支持：

```bash
mini-harness run "fix failing tests"
```

后续支持：

```bash
mini-harness resume <session-id>
```

以及：

```bash
mini-harness trace <session-id>
```

---

# 20. 配置

`.env`

```text
OPENAI_API_KEY=

OPENAI_BASE_URL=

OPENAI_MODEL=
```

配置结构：

```rust
pub struct Config {

    pub api_key: String,

    pub base_url: String,

    pub model: String,

    pub max_steps: usize,

    pub workspace: PathBuf,
}
```

---

# 21. 安全边界

Harness 必须对文件工具执行 Workspace Sandbox。

例如 workspace：

```text
/home/user/project
```

允许：

```text
/home/user/project/src/main.rs
```

禁止：

```text
/etc/passwd

/home/user/.ssh/id_rsa

../../secret
```

实现时必须：

```text
canonicalize(path)
```

并验证：

```text
path.starts_with(workspace)
```

---

# 22. 错误处理

Tool 错误不能直接导致 Agent Crash。

例如：

```text
read_file:
File not found
```

应该作为 Observation 返回模型：

```text
Tool execution failed:

File not found: src/foo.rs
```

然后允许模型继续决策。

---

# 23. 停止条件

Agent 在以下情况停止：

### 23.1 Final Response

模型明确返回最终结果。

### 23.2 Max Steps

```text
step >= max_steps
```

### 23.3 Fatal Error

例如：

```text
LLM authentication failure
```

### 23.4 User Abort

用户输入：

```text
Ctrl+C
```

---

# 24. V1 验收场景

准备测试项目：

```text
examples/broken-calculator/
```

存在一个失败测试。

用户执行：

```bash
mini-harness run "fix the failing test"
```

Agent 应该能够：

```text
1. list_files
2. read_file
3. read_file
4. cargo test
5. 分析失败
6. 修改代码
7. cargo test
8. 最终回答
```

最终：

```text
cargo test
```

成功。

---

# 25. V1 验收标准

必须完成：

- [ ] 可以调用 LLM
- [ ] 可以发送 Tool Schema
- [ ] 可以解析 Tool Call
- [ ] 可以执行 Tool
- [ ] Tool Result 可以重新发送给模型
- [ ] Agent 可以连续执行多步任务
- [ ] 支持 read_file
- [ ] 支持 list_files
- [ ] 支持 write_file
- [ ] 支持 shell
- [ ] 支持 Tool Registry
- [ ] 支持 Agent State
- [ ] 支持 max_steps
- [ ] 支持 Tool Policy
- [ ] 支持人工 Approval
- [ ] 支持 JSONL Trace
- [ ] 支持 Workspace Sandbox
- [ ] Tool Failure 不导致 Agent 直接崩溃
- [ ] Agent 可以修复一个简单 Rust Bug

---

# 26. 开发阶段

## V0 — LLM CLI

目标：

```text
User → LLM → Answer
```

完成：

- Config
- LLM Client
- CLI

---

## V1 — Tool Calling

目标：

```text
LLM → Tool Call → Rust Function
```

完成：

- Tool Trait
- Tool Registry
- read_file
- list_files

---

## V2 — Agent Loop

目标：

```text
LLM
 ↓
Tool
 ↓
Observation
 ↓
LLM
```

完成：

- 多轮执行
- Tool Result
- max_steps

---

## V3 — Coding Agent

增加：

```text
write_file
shell
```

Agent 可以：

```text
read → edit → test
```

---

## V4 — Policy

增加：

- Allow
- Ask
- Deny
- CLI Approval

---

## V5 — Trace

增加：

```text
JSONL Event Log
```

要求能完整查看一次 Agent Run。

---

## V6 — Session

增加：

```text
save
resume
```

---

## V7 — Context Management

实现：

- Complete-request approximate token budgets including model, tool schemas, and messages
- Oldest coherent-group truncation that preserves assistant tool calls with same-ID results
- Deterministic bounded redacted context summaries
- Unicode-safe native tool-output head/tail truncation
- Strict version-2 resumable sessions with persisted context settings and summary metadata

---

## V8 — MCP

实现：

```text
Bounded MCP 2025-06-18 stdio Client
```

- Explicit user-selected strict config through `MCP_CONFIG_PATH`; native-only when absent
- JSON-RPC 2.0 initialize/initialized, paginated `tools/list`, and `tools/call`
- Numeric request correlation, bounded I/O/time/process lifecycle, and unsupported request denial
- Deterministic `mcp__<server>__<tool>` names with Ask-by-default policy
- Non-secret schema-3 session manifests and payload-free MCP trace metadata

将 MCP Tool 转换为：

```text
Tool Trait
```

使 Agent 不关心 Tool 来源。

---

## V9 — Sub-Agent

增加：

```text
spawn_agent
```

主 Agent：

```text
Main Agent
   │
   ├── Research Agent
   ├── Code Agent
   └── Test Agent
```

Sub-Agent 仍然运行于同一个 Harness Runtime。

---

## V10 — Controlled Workspace RAG

增加只读工具：

```text
search_workspace_knowledge
```

- 默认对受控 workspace 内的非敏感 UTF-8 文本执行有界、本地词法检索；
- 可通过 `RAG_MODE=vector|hybrid` 显式启用 OpenAI-compatible Embedding、内存余弦向量检索及 RRF 词法/向量融合；
- 结果必须附带相对路径、行范围、分数与截断片段，供 `read_file` 精读；
- 不能遍历符号链接、`.env`、`.sessions/`、`traces/`、常见凭据路径、`.git/`、`target/` 或 `node_modules/`；
- `lexical` 模式不依赖外部服务；`vector`/`hybrid` 会将有界 chunk、相对路径和查询发送给显式授权的 Embedding Provider；自定义 Embedding Endpoint 必须使用独立显式 Key；
- 向量索引只缓存在进程内存中，源内容哈希变化时重建，不写入长期 Memory 或持久化向量数据库；
- Tool Call 继续经过 Policy、Context、Trace 与 Session；Trace 不记录 query、chunk 或结果 payload。

V10 不实现 LightRAG 的实体关系图或图检索；这些能力可在后续阶段继续扩展。

---

# 27. 最终架构目标

```text
                     mini-harness
                         │
          ┌──────────────┼──────────────┐
          │              │              │
        Agent          Session        Trace
          │
          ▼
      Agent Loop
          │
    ┌─────┴───────┐
    │             │
   LLM           Tools
                  │
          ┌───────┼─────────┐
          │       │         │
       Native    MCP     Sub-Agent
       Tools     Tools      Tool
          │
          ▼
        Policy
          │
          ▼
       Executor
```

---

# 28. 推荐实现顺序

不要一次搭完整架构。

推荐顺序：

```text
01 main.rs
02 llm/client.rs
03 llm/openai.rs
04 llm/types.rs
05 tools/tool.rs
06 tools/read_file.rs
07 tools/registry.rs
08 agent/state.rs
09 agent/runner.rs
10 tools/list_files.rs
11 tools/write_file.rs
12 tools/shell.rs
13 policy/approval.rs
14 trace/event.rs
15 trace/writer.rs
16 session/session.rs
17 context/builder.rs
```

---

# 29. 第一阶段代码量目标

建议：

```text
V0        200 行
V1        400 行
V2        600 行
V3        800 行
V4/V5    1200～1500 行
```

第一版尽量控制在：

```text
1500 LOC
```

以内。

---

# 30. 项目完成标志

当以下命令成立时，可以认为 mini-harness 第一阶段完成：

```bash
mini-harness run \
  "inspect this Rust project, run the tests, fix the bug, and verify the fix"
```

Agent 能自主完成：

```text
理解任务
→ 查看文件
→ 调用测试
→ 分析结果
→ 修改文件
→ 再次测试
→ 输出最终答案
```

并且整个过程：

```text
可限制
可观察
可追踪
可终止
可审计
```

这五点就是该项目对 **Agent Harness** 最核心的学习目标。
