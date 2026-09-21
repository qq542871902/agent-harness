use super::{
    config::{McpConfig, McpServerConfig},
    tool::McpTool,
};
use crate::tools::{ToolOutput, ToolRegistry};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    error::Error,
    fmt,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    task::JoinHandle,
    time::timeout,
};

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_PAGES: usize = 100;
const MAX_TOOLS_PER_SERVER: usize = 256;

#[derive(Debug, Clone)]
pub struct McpLimits {
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_line_bytes: usize,
    pub max_result_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(15),
            shutdown_timeout: Duration::from_secs(1),
            max_line_bytes: 1024 * 1024,
            max_result_bytes: 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
        }
    }
}

impl McpLimits {
    fn validate(&self) -> Result<()> {
        if self.startup_timeout.is_zero()
            || self.request_timeout.is_zero()
            || self.shutdown_timeout.is_zero()
            || self.max_line_bytes == 0
            || self.max_result_bytes == 0
            || self.max_stderr_bytes == 0
        {
            bail!("MCP limits must all be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProtocolError {
    pub server: String,
    pub category: &'static str,
    pub code: Option<i64>,
    pub message: String,
}

impl fmt::Display for McpProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "MCP {} error from `{}`",
            self.category, self.server
        )?;
        if let Some(code) = self.code {
            write!(formatter, " (code {code})")?;
        }
        write!(formatter, ": {}", self.message)
    }
}

impl Error for McpProtocolError {}

fn protocol_error(
    server: &str,
    category: &'static str,
    code: Option<i64>,
    message: impl Into<String>,
) -> anyhow::Error {
    anyhow!(McpProtocolError {
        server: server.to_owned(),
        category,
        code,
        message: message.into(),
    })
}

#[derive(Debug)]
struct DiscoveredTool {
    name: String,
    description: String,
    schema: Value,
}

#[derive(Default)]
pub struct McpManager {
    sessions: Vec<Arc<McpSession>>,
}

impl McpManager {
    pub async fn connect(config: &McpConfig, registry: &mut ToolRegistry) -> Result<Self> {
        Self::connect_with_limits(config, registry, McpLimits::default()).await
    }

    pub async fn connect_with_limits(
        config: &McpConfig,
        registry: &mut ToolRegistry,
        limits: McpLimits,
    ) -> Result<Self> {
        config.validate()?;
        limits.validate()?;
        let mut manager = Self {
            sessions: Vec::new(),
        };
        let mut tools = Vec::new();
        let startup = async {
            for server in config.servers.iter().filter(|server| server.enabled) {
                let session = Arc::new(McpSession::connect(server, limits.clone()).await?);
                manager.sessions.push(Arc::clone(&session));
                let discovered = timeout(limits.startup_timeout, session.list_tools())
                    .await
                    .map_err(|_| {
                        protocol_error(&server.name, "startup", None, "tool discovery timed out")
                    })??;
                for tool in discovered {
                    tools.push(McpTool::new(
                        Arc::clone(&session),
                        tool.name,
                        tool.description,
                        tool.schema,
                    )?);
                }
            }
            registry.register_batch(tools)
        }
        .await;
        if let Err(error) = startup {
            if let Err(cleanup) = manager.shutdown().await {
                return Err(error).context(format!("MCP startup cleanup also failed: {cleanup:#}"));
            }
            return Err(error);
        }
        Ok(manager)
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let mut first_error = None;
        for session in &self.sessions {
            if let Err(error) = session.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.sessions.clear();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        for session in &self.sessions {
            session.force_terminate();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let session = Arc::clone(session);
                drop(runtime.spawn(async move {
                    let _ = session.shutdown().await;
                }));
            }
        }
    }
}

pub(crate) struct McpSession {
    server_name: String,
    process_id: u32,
    closed: AtomicBool,
    limits: McpLimits,
    connection: Mutex<Connection>,
}

impl McpSession {
    async fn connect(config: &McpServerConfig, limits: McpLimits) -> Result<Self> {
        let connection = timeout(limits.startup_timeout, Connection::spawn(config, &limits))
            .await
            .map_err(|_| {
                protocol_error(&config.name, "startup", None, "process launch timed out")
            })??;
        let process_id = connection
            .child
            .id()
            .context("spawned MCP server did not have a process ID")?;
        let session = Self {
            server_name: config.name.clone(),
            process_id,
            closed: AtomicBool::new(false),
            limits,
            connection: Mutex::new(connection),
        };
        let initialized = timeout(session.limits.startup_timeout, session.initialize()).await;
        match initialized {
            Ok(Ok(())) => Ok(session),
            Ok(Err(error)) => {
                session.poison("initialize failed").await;
                Err(error)
            }
            Err(_) => {
                session.poison("initialize timed out").await;
                Err(protocol_error(
                    &session.server_name,
                    "startup",
                    None,
                    "initialize timed out",
                ))
            }
        }
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn initialize(&self) -> Result<()> {
        let mut connection = self.connection.lock().await;
        let result = connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mini-harness-v9", "version": "0.1.0"}
                }),
                &self.limits,
                &self.server_name,
            )
            .await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                protocol_error(
                    &self.server_name,
                    "initialize",
                    None,
                    "missing protocolVersion",
                )
            })?;
        if version != MCP_PROTOCOL_VERSION {
            return Err(protocol_error(
                &self.server_name,
                "initialize",
                None,
                format!("server selected unsupported protocol version `{version}`"),
            ));
        }
        connection
            .notify("notifications/initialized", json!({}))
            .await
            .context("failed to send MCP initialized notification")
    }

    async fn list_tools(&self) -> Result<Vec<DiscoveredTool>> {
        let mut connection = self.connection.lock().await;
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut tools = Vec::new();
        for _ in 0..MAX_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor: &String| json!({"cursor": cursor}));
            let result = connection
                .request("tools/list", params, &self.limits, &self.server_name)
                .await?;
            let object = result.as_object().ok_or_else(|| {
                protocol_error(
                    &self.server_name,
                    "tools/list",
                    None,
                    "result must be an object",
                )
            })?;
            let page = object
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    protocol_error(
                        &self.server_name,
                        "tools/list",
                        None,
                        "result.tools must be an array",
                    )
                })?;
            for value in page {
                tools.push(parse_tool(&self.server_name, value)?);
                if tools.len() > MAX_TOOLS_PER_SERVER {
                    return Err(protocol_error(
                        &self.server_name,
                        "tools/list",
                        None,
                        format!("more than {MAX_TOOLS_PER_SERVER} tools were returned"),
                    ));
                }
            }
            cursor = match object.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
                _ => {
                    return Err(protocol_error(
                        &self.server_name,
                        "tools/list",
                        None,
                        "nextCursor must be a non-empty string or null",
                    ));
                }
            };
            if !seen_cursors.insert(cursor.clone().expect("cursor was set")) {
                return Err(protocol_error(
                    &self.server_name,
                    "tools/list",
                    None,
                    "server repeated a pagination cursor",
                ));
            }
        }
        Err(protocol_error(
            &self.server_name,
            "tools/list",
            None,
            format!("pagination exceeded {MAX_PAGES} pages"),
        ))
    }

    pub(crate) async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutput> {
        if !arguments.is_object() {
            bail!("MCP tool arguments must be a JSON object");
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(protocol_error(
                &self.server_name,
                "closed",
                None,
                "session is closed or poisoned",
            ));
        }
        let mut connection = self.connection.lock().await;
        let result = match connection
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                &self.limits,
                &self.server_name,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                if !is_correlated_remote_error(&error) {
                    self.closed.store(true, Ordering::Release);
                }
                return Err(error);
            }
        };
        match map_call_result(&self.server_name, result, self.limits.max_result_bytes) {
            Ok(output) => Ok(output),
            Err(error) => {
                self.closed.store(true, Ordering::Release);
                connection
                    .poison(
                        self.limits.shutdown_timeout,
                        "tools/call returned a malformed result",
                    )
                    .await;
                Err(error)
            }
        }
    }

    async fn poison(&self, reason: &str) {
        self.closed.store(true, Ordering::Release);
        self.connection
            .lock()
            .await
            .poison(self.limits.shutdown_timeout, reason)
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        let result = self
            .connection
            .lock()
            .await
            .shutdown(self.limits.shutdown_timeout)
            .await;
        self.closed.store(true, Ordering::Release);
        result
    }

    fn force_terminate(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.process_id as i32), libc::SIGKILL);
        }
        if let Ok(mut connection) = self.connection.try_lock() {
            terminate_process_group(&mut connection.child);
        }
    }
}

fn parse_tool(server: &str, value: &Value) -> Result<DiscoveredTool> {
    let object = value.as_object().ok_or_else(|| {
        protocol_error(server, "tools/list", None, "tool entry must be an object")
    })?;
    let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
        protocol_error(
            server,
            "tools/list",
            None,
            "tool entry is missing a string name",
        )
    })?;
    let description = match object.get("description") {
        None => String::new(),
        Some(Value::String(value)) => value.clone(),
        _ => {
            return Err(protocol_error(
                server,
                "tools/list",
                None,
                "tool description must be a string",
            ));
        }
    };
    let schema = object.get("inputSchema").cloned().ok_or_else(|| {
        protocol_error(
            server,
            "tools/list",
            None,
            "tool entry is missing inputSchema",
        )
    })?;
    Ok(DiscoveredTool {
        name: name.to_owned(),
        description,
        schema,
    })
}

fn map_call_result(server: &str, result: Value, max_bytes: usize) -> Result<ToolOutput> {
    let object = result
        .as_object()
        .ok_or_else(|| protocol_error(server, "tools/call", None, "result must be an object"))?;
    let mut parts = Vec::new();
    if let Some(content) = object.get("content") {
        let content = content.as_array().ok_or_else(|| {
            protocol_error(server, "tools/call", None, "content must be an array")
        })?;
        for item in content {
            let item = item.as_object().ok_or_else(|| {
                protocol_error(server, "tools/call", None, "content item must be an object")
            })?;
            match item.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    item.get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            protocol_error(
                                server,
                                "tools/call",
                                None,
                                "text content is missing text",
                            )
                        })?
                        .to_owned(),
                ),
                Some(kind) => {
                    return Err(protocol_error(
                        server,
                        "tools/call",
                        None,
                        format!("unsupported content type `{kind}`"),
                    ));
                }
                None => {
                    return Err(protocol_error(
                        server,
                        "tools/call",
                        None,
                        "content item is missing type",
                    ));
                }
            }
        }
    }
    if let Some(structured) = object.get("structuredContent") {
        parts
            .push(serde_json::to_string(structured).context("failed to encode structuredContent")?);
    }
    let content = parts.join("\n");
    if content.len() > max_bytes {
        return Err(protocol_error(
            server,
            "tools/call",
            None,
            format!("mapped result exceeds {max_bytes} bytes"),
        ));
    }
    let success = match object.get("isError") {
        None => true,
        Some(Value::Bool(value)) => !value,
        _ => {
            return Err(protocol_error(
                server,
                "tools/call",
                None,
                "isError must be a boolean",
            ));
        }
    };
    Ok(ToolOutput { content, success })
}

fn is_correlated_remote_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<McpProtocolError>()
        .is_some_and(|error| error.category == "remote")
}

struct Connection {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_task: Option<JoinHandle<usize>>,
    next_id: u64,
    closed: bool,
    poison_reason: Option<String>,
}

impl Connection {
    async fn spawn(config: &McpServerConfig, limits: &McpLimits) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear();
        if let Some(directory) = &config.working_directory {
            command.current_dir(directory);
        }
        for name in &config.env_allowlist {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch configured MCP server `{}`", config.name))?;
        let stdin = child
            .stdin
            .take()
            .context("MCP child stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("MCP child stdout was unavailable")?;
        let mut stderr = child
            .stderr
            .take()
            .context("MCP child stderr was unavailable")?;
        let max_stderr = limits.max_stderr_bytes;
        let stderr_task = tokio::spawn(async move {
            let mut retained = 0usize;
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => return retained,
                    Ok(count) => retained = retained.saturating_add(count).min(max_stderr),
                }
            }
        });
        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            stderr_task: Some(stderr_task),
            next_id: 1,
            closed: false,
            poison_reason: None,
        })
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_json(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        limits: &McpLimits,
        server: &str,
    ) -> Result<Value> {
        if self.closed {
            let state = if self.poison_reason.is_some() {
                "poisoned"
            } else {
                "closed"
            };
            return Err(protocol_error(
                server,
                "closed",
                None,
                format!("session is {state}"),
            ));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("MCP request ID overflow")?;
        let request = timeout(limits.request_timeout, async {
            self.write_json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            loop {
                let line = read_bounded_line(&mut self.stdout, limits.max_line_bytes)
                    .await
                    .map_err(|error| {
                        protocol_error(server, "transport", None, error.to_string())
                    })?;
                let message: Value = serde_json::from_slice(&line).map_err(|_| {
                    protocol_error(server, "transport", None, "received invalid JSON")
                })?;
                let object = message.as_object().ok_or_else(|| {
                    protocol_error(server, "protocol", None, "message must be a JSON object")
                })?;
                if object.get("jsonrpc") != Some(&Value::String("2.0".into())) {
                    return Err(protocol_error(
                        server,
                        "protocol",
                        None,
                        "message is not JSON-RPC 2.0",
                    ));
                }
                if object.contains_key("method") {
                    if let Some(request_id) = object.get("id") {
                        self.deny_server_request(request_id.clone()).await?;
                    }
                    continue;
                }
                let response_id = object.get("id").and_then(Value::as_u64).ok_or_else(|| {
                    protocol_error(server, "protocol", None, "response ID must be numeric")
                })?;
                if response_id != id {
                    return Err(protocol_error(
                        server,
                        "protocol",
                        None,
                        format!("response ID {response_id} does not match request ID {id}"),
                    ));
                }
                return match (object.get("result"), object.get("error")) {
                    (Some(result), None) => Ok(result.clone()),
                    (None, Some(error)) => Err(parse_remote_error(server, error)),
                    (Some(_), Some(_)) => Err(protocol_error(
                        server,
                        "protocol",
                        None,
                        "response contains both result and error",
                    )),
                    (None, None) => Err(protocol_error(
                        server,
                        "protocol",
                        None,
                        "response has neither result nor error",
                    )),
                };
            }
        })
        .await;
        let result = match request {
            Ok(result) => result,
            Err(_) => Err(protocol_error(
                server,
                "timeout",
                None,
                format!("request `{method}` timed out"),
            )),
        };
        if let Err(error) = &result
            && !is_correlated_remote_error(error)
        {
            self.poison(
                limits.shutdown_timeout,
                &format!("request `{method}` lost protocol synchronization"),
            )
            .await;
        }
        result
    }

    async fn poison(&mut self, wait: Duration, reason: &str) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.poison_reason = Some(reason.to_owned());
        let _ = self.stdin.shutdown().await;
        terminate_process_group(&mut self.child);
        let _ = timeout(wait, self.child.wait()).await;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }

    async fn deny_server_request(&mut self, id: Value) -> Result<()> {
        self.write_json(&json!({
            "jsonrpc":"2.0",
            "id":id,
            "error":{"code":-32601,"message":"Server-to-client requests are not supported"}
        }))
        .await
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let mut bytes =
            serde_json::to_vec(value).context("failed to encode MCP JSON-RPC message")?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .context("failed to write MCP message")?;
        self.stdin
            .flush()
            .await
            .context("failed to flush MCP message")
    }

    async fn shutdown(&mut self, wait: Duration) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let _ = self.stdin.shutdown().await;
        if timeout(wait, self.child.wait()).await.is_err() {
            terminate_process_group(&mut self.child);
            let _ = self.child.wait().await;
        }
        if let Some(task) = self.stderr_task.take() {
            let _ = timeout(wait, task).await;
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.closed {
            terminate_process_group(&mut self.child);
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

fn terminate_process_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

fn parse_remote_error(server: &str, value: &Value) -> anyhow::Error {
    let object = value.as_object();
    let code = object
        .and_then(|object| object.get("code"))
        .and_then(Value::as_i64);
    let message = object
        .and_then(|object| object.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("server returned an invalid JSON-RPC error");
    protocol_error(server, "remote", code, message)
}

async fn read_bounded_line(
    reader: &mut BufReader<ChildStdout>,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "MCP server closed stdout",
            ));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if output.len().saturating_add(newline) > max_bytes {
                reader.consume(newline + 1);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("MCP line exceeds {max_bytes} bytes"),
                ));
            }
            output.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if output.last() == Some(&b'\r') {
                output.pop();
            }
            return Ok(output);
        }
        let count = available.len();
        if output.len().saturating_add(count) > max_bytes {
            reader.consume(count);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MCP line exceeds {max_bytes} bytes"),
            ));
        }
        output.extend_from_slice(available);
        reader.consume(count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_text_and_structured_content_with_is_error() {
        let output = map_call_result(
            "server",
            json!({"content":[{"type":"text","text":"hello"}],"structuredContent":{"x":1},"isError":true}),
            1024,
        ).unwrap();
        assert_eq!(output.content, "hello\n{\"x\":1}");
        assert!(!output.success);
        assert!(map_call_result("server", json!({"content":[{"type":"image"}]}), 1024).is_err());
        assert!(map_call_result("server", json!({"structuredContent":"too long"}), 3).is_err());
    }
}
