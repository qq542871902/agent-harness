use anyhow::{Context, Result, bail};
use mini_harness_v9::{
    agent::{
        AgentOutcome, AgentRunner, AgentState, AgentStatus, RunPersistence,
        SameProcessSubAgentSpawner,
    },
    cancel::CancellationToken,
    config::Config,
    context::ContextBuilder,
    llm::{LlmClient, OpenAiCompatibleClient},
    mcp::{McpConfig, McpManager},
    policy::{ApprovalHandler, ConsoleApprovalHandler, DefaultPolicy, Policy},
    session::{FileSessionRepository, Session, SessionCheckpoint, SessionRepository, SessionSink},
    tools::{
        ListFilesTool, ReadFileTool, ShellTool, SpawnAgentTool, SubAgentSpawner, ToolRegistry,
        WriteFileTool,
    },
    trace::{
        JsonlTraceWriter, TraceEvent, TraceEventKind, TraceSink, parse_session_id,
        resolve_trace_path, safe_error,
    },
};
use serde_json::Value;
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let command = Command::parse(std::env::args().skip(1))?;
    match &command {
        Command::Trace(id) => return display_trace(id),
        Command::Session(id) => return display_session(id),
        _ => {}
    }

    let config = Config::from_env()?;
    let client: Arc<dyn LlmClient> = Arc::new(OpenAiCompatibleClient::new(&config)?);
    let workspace = workspace_directory()?;
    let mut registry = build_native_tool_registry(&workspace)?;
    let mcp_config = McpConfig::from_env()?;
    let mcp_configured = mcp_config.is_some();
    let mut mcp = match mcp_config.as_ref() {
        Some(config) => McpManager::connect(config, &mut registry).await?,
        None => McpManager::default(),
    };
    let policy: Arc<dyn Policy> = Arc::new(DefaultPolicy);
    let approver: Arc<dyn ApprovalHandler> = Arc::new(ConsoleApprovalHandler);

    let result = match command {
        Command::Run(prompt) => {
            run_task(
                &client, &registry, &workspace, &config, &policy, &approver, &prompt,
            )
            .await
        }
        Command::Resume(id) => {
            resume_task(
                &client,
                &registry,
                &workspace,
                &policy,
                &approver,
                &config.api_key,
                mcp_configured,
                &id,
            )
            .await
        }
        Command::Interactive => {
            run_interactive(&client, &registry, &workspace, &config, &policy, &approver).await
        }
        Command::Trace(_) | Command::Session(_) => unreachable!(),
    };
    let shutdown = mcp.shutdown().await;
    result.and(shutdown)
}

enum Command {
    Interactive,
    Run(String),
    Resume(String),
    Session(String),
    Trace(String),
}
impl Command {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let args = arguments.collect::<Vec<_>>();
        match args.as_slice() {
            [] => Ok(Self::Interactive),
            [command] if command == "--help" || command == "-h" => {
                print_usage();
                std::process::exit(0)
            }
            [command, prompt @ ..] if command == "run" && !prompt.is_empty() => {
                Ok(Self::Run(prompt.join(" ")))
            }
            [command, id] if command == "resume" => Ok(Self::Resume(id.clone())),
            [command, id] if command == "session" => Ok(Self::Session(id.clone())),
            [command, id] if command == "trace" => Ok(Self::Trace(id.clone())),
            [command, ..] if command == "run" => {
                bail!("`run` requires a prompt\n\n{}", usage())
            }
            [command, ..] if matches!(command.as_str(), "resume" | "session" | "trace") => {
                bail!("`{command}` requires exactly one session ID\n\n{}", usage())
            }
            _ => bail!("unknown command\n\n{}", usage()),
        }
    }
}

fn build_native_tool_registry(workspace: &Path) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(workspace)?)?;
    registry.register(ListFilesTool::new(workspace)?)?;
    registry.register(
        WriteFileTool::new(workspace)?
            .with_reserved_subtree("traces")?
            .with_reserved_subtree(".sessions")?,
    )?;
    registry.register(ShellTool::new(workspace)?)?;
    Ok(registry)
}

async fn run_interactive(
    client: &Arc<dyn LlmClient>,
    registry: &ToolRegistry,
    workspace: &Path,
    config: &Config,
    policy: &Arc<dyn Policy>,
    approver: &Arc<dyn ApprovalHandler>,
) -> Result<()> {
    println!("mini-harness V9 bounded sub-agent runtime — type /exit to quit.");
    let stdin = io::stdin();
    let mut input = String::new();
    loop {
        print!("mini-harness> ");
        io::stdout().flush()?;
        input.clear();
        if stdin.lock().read_line(&mut input)? == 0 {
            println!();
            return Ok(());
        }
        let prompt = input.trim();
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "/exit" | "exit" | "quit") {
            return Ok(());
        }
        if let Err(error) = run_task(
            client, registry, workspace, config, policy, approver, prompt,
        )
        .await
        {
            eprintln!("Error: {error:#}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_task_registry(
    client: &Arc<dyn LlmClient>,
    base_registry: &ToolRegistry,
    model: &str,
    policy: &Arc<dyn Policy>,
    approver: &Arc<dyn ApprovalHandler>,
    context: mini_harness_v9::context::ContextSettings,
    sensitive_values: Vec<String>,
    session_id: Uuid,
    trace: Arc<dyn TraceSink>,
) -> Result<ToolRegistry> {
    let spawner: Arc<dyn SubAgentSpawner> = Arc::new(SameProcessSubAgentSpawner::new(
        Arc::clone(client),
        base_registry.clone(),
        model,
        Arc::clone(policy),
        Arc::clone(approver),
        context,
        sensitive_values,
        session_id,
        trace,
    ));
    let mut registry = base_registry.clone();
    registry.register(SpawnAgentTool::new(spawner))?;
    Ok(registry)
}

async fn run_task(
    client: &Arc<dyn LlmClient>,
    base_registry: &ToolRegistry,
    workspace: &Path,
    config: &Config,
    policy: &Arc<dyn Policy>,
    approver: &Arc<dyn ApprovalHandler>,
    prompt: &str,
) -> Result<()> {
    let id = Uuid::new_v4();
    let writer = Arc::new(JsonlTraceWriter::create(workspace, id)?);
    let trace: Arc<dyn TraceSink> = writer.clone();
    let registry = build_task_registry(
        client,
        base_registry,
        &config.model,
        policy,
        approver,
        config.context.clone(),
        vec![config.api_key.clone()],
        id,
        trace,
    )?;
    let repository = match FileSessionRepository::new(workspace) {
        Ok(repository) => repository,
        Err(error) => return traced_setup_failure(writer.as_ref(), 0, error),
    };
    let state = AgentState::new(prompt);
    let session = match Session::new(
        id,
        workspace,
        &config.model,
        config.context.clone(),
        registry.mcp_tool_names(),
        state.clone(),
        writer.path().to_path_buf(),
    ) {
        Ok(session) => session,
        Err(error) => return traced_setup_failure(writer.as_ref(), 0, error),
    };
    if let Err(error) = repository.create(&session) {
        return traced_setup_failure(writer.as_ref(), 0, error);
    }
    let _lease = match repository.acquire(id) {
        Ok(lease) => lease,
        Err(error) => return traced_setup_failure(writer.as_ref(), 0, error),
    };
    let checkpoint = SessionCheckpoint::new(&repository, session);
    println!("Session: {id}");
    println!("Trace: {}", writer.path().display());
    let runner = AgentRunner::with_persistence(
        client.as_ref(),
        &registry,
        &config.model,
        policy.as_ref(),
        approver.as_ref(),
        ContextBuilder::with_sensitive_values(config.context.clone(), [config.api_key.clone()])?,
        RunPersistence {
            session_id: id,
            trace: writer.as_ref(),
            sessions: &checkpoint,
        },
    );
    let mut state = state;
    run_with_sigint(runner, &mut state, false).await
}

#[allow(clippy::too_many_arguments)]
async fn resume_task(
    client: &Arc<dyn LlmClient>,
    base_registry: &ToolRegistry,
    workspace: &Path,
    policy: &Arc<dyn Policy>,
    approver: &Arc<dyn ApprovalHandler>,
    api_key: &str,
    mcp_configured: bool,
    raw_id: &str,
) -> Result<()> {
    let id = parse_session_id(raw_id)?;
    let repository = FileSessionRepository::new(workspace)?;
    let _lease = repository.acquire(id)?;
    let session = repository.load(id)?;
    if !session.mcp_tools.is_empty() && !mcp_configured {
        bail!(
            "session {id} used MCP tools; set MCP_CONFIG_PATH to a current V9 MCP configuration before resume"
        );
    }
    if session.state.status == AgentStatus::Completed {
        println!("Session {id} is already completed.");
        if let Some(content) = session.state.final_content() {
            println!("{content}");
        }
        return Ok(());
    }
    let writer = Arc::new(JsonlTraceWriter::open_append(workspace, id)?);
    let model = session.model.clone();
    let settings = session.context_settings.clone();
    let trace: Arc<dyn TraceSink> = writer.clone();
    let registry = build_task_registry(
        client,
        base_registry,
        &model,
        policy,
        approver,
        settings.clone(),
        vec![api_key.to_owned()],
        id,
        trace,
    )?;
    let mut state = session.state.clone();
    state.prepare_for_resume();
    let checkpoint = SessionCheckpoint::new(&repository, session);
    if let Err(error) = checkpoint.checkpoint(&state) {
        return traced_setup_failure(writer.as_ref(), state.step, error);
    }
    println!("Resuming session: {id}");
    println!("Trace: {}", writer.path().display());
    let runner = AgentRunner::with_persistence(
        client.as_ref(),
        &registry,
        &model,
        policy.as_ref(),
        approver.as_ref(),
        ContextBuilder::with_sensitive_values(settings, [api_key.to_owned()])?,
        RunPersistence {
            session_id: id,
            trace: writer.as_ref(),
            sessions: &checkpoint,
        },
    );
    run_with_sigint(runner, &mut state, true).await
}

async fn run_with_sigint(
    runner: AgentRunner<'_>,
    state: &mut AgentState,
    resumed: bool,
) -> Result<()> {
    let cancellation = CancellationToken::new();
    let runner = runner.with_cancellation(cancellation.clone());
    tokio::select! {
        result = async {
            if resumed {
                runner.resume(state).await
            } else {
                runner.run(state).await
            }
        } => print_outcome(result?),
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for SIGINT")?;
            cancellation.cancel();
            let _ = runner.abort(state)?;
            println!("Agent aborted by SIGINT; session checkpointed for safe resume.");
            Ok(())
        }
    }
}

fn traced_setup_failure<T>(
    writer: &JsonlTraceWriter,
    step: usize,
    error: anyhow::Error,
) -> Result<T> {
    let id = Uuid::parse_str(
        writer
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .context("trace path does not contain a session UUID")?,
    )
    .context("trace path contains an invalid session UUID")?;
    writer.record(&TraceEvent::new(
        id,
        TraceEventKind::AgentFailed {
            step,
            error: safe_error(&format!("session persistence failed: {error:#}")),
        },
    ))?;
    Err(error)
}

fn print_outcome(outcome: AgentOutcome) -> Result<()> {
    match outcome {
        AgentOutcome::Completed { content } => println!("{content}"),
        AgentOutcome::MaxStepsReached { steps } => {
            println!("Agent stopped after reaching step {steps} without a final answer.")
        }
    }
    Ok(())
}

fn display_trace(raw: &str) -> Result<()> {
    let id = parse_session_id(raw)?;
    let workspace = workspace_directory()?;
    let path = resolve_trace_path(&workspace, id)?;
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read trace `{}`", path.display()))?;
    for (index, line) in contents.lines().enumerate() {
        if line.is_empty() {
            bail!("trace contains an empty line at {}", index + 1);
        }
        serde_json::from_str::<Value>(line)
            .with_context(|| format!("trace contains invalid JSON at line {}", index + 1))?;
    }
    print!("{contents}");
    io::stdout().flush().context("failed to flush trace output")
}

fn display_session(raw: &str) -> Result<()> {
    let id = parse_session_id(raw)?;
    let workspace = workspace_directory()?;
    let session = FileSessionRepository::new(&workspace)?.load(id)?;
    eprintln!(
        "Warning: session data contains model, user, and tool content; handle it as sensitive."
    );
    println!("{}", serde_json::to_string_pretty(&session)?);
    Ok(())
}

fn workspace_directory() -> Result<PathBuf> {
    std::env::current_dir().context("failed to determine the workspace directory")
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .without_time()
        .init();
}

fn usage() -> &'static str {
    "Usage:\n  mini-harness-v9\n  mini-harness-v9 run <prompt>\n  mini-harness-v9 resume <session-id>\n  mini-harness-v9 session <session-id>\n  mini-harness-v9 trace <session-id>\n\nMCP:\n  Set MCP_CONFIG_PATH to an absolute path to a strict V9 MCP config.\n  If absent, native tools and spawn_agent are registered.\n  V9 session schema 5 is strict; earlier schemas are not migrated."
}
fn print_usage() {
    println!("{}", usage());
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mini_harness_v9::llm::{ChatRequest, ModelResponse};

    struct NoClient;
    #[async_trait]
    impl LlmClient for NoClient {
        async fn chat(&self, _: ChatRequest) -> Result<ModelResponse> {
            bail!("not called")
        }
    }
    struct NoTrace;
    impl TraceSink for NoTrace {
        fn record(&self, _: &TraceEvent) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn task_scoped_registry_reconstructs_spawn_capability() {
        let client: Arc<dyn LlmClient> = Arc::new(NoClient);
        let policy: Arc<dyn Policy> = Arc::new(DefaultPolicy);
        let approver: Arc<dyn ApprovalHandler> = Arc::new(ConsoleApprovalHandler);
        let trace: Arc<dyn TraceSink> = Arc::new(NoTrace);
        let registry = build_task_registry(
            &client,
            &ToolRegistry::new(),
            "model",
            &policy,
            &approver,
            mini_harness_v9::context::ContextSettings::default(),
            vec![],
            Uuid::nil(),
            trace,
        )
        .unwrap();
        assert_eq!(registry.names(), ["spawn_agent"]);
    }

    #[test]
    fn commands_require_one_identifier() {
        for command in ["trace", "resume", "session"] {
            assert!(Command::parse([command.into(), Uuid::nil().to_string()].into_iter()).is_ok());
            assert!(
                Command::parse([command.into(), "../x".into(), "extra".into()].into_iter())
                    .is_err()
            );
        }
    }
}
