use anyhow::{Context, Result, bail};
use mini_harness_v8::{
    agent::{AgentOutcome, AgentRunner, AgentState, AgentStatus, RunPersistence},
    config::Config,
    context::ContextBuilder,
    llm::{LlmClient, OpenAiCompatibleClient},
    mcp::{McpConfig, McpManager},
    policy::{ApprovalHandler, ConsoleApprovalHandler, DefaultPolicy, Policy},
    session::{FileSessionRepository, Session, SessionCheckpoint, SessionRepository, SessionSink},
    tools::{ListFilesTool, ReadFileTool, ShellTool, ToolRegistry, WriteFileTool},
    trace::{
        JsonlTraceWriter, TraceEvent, TraceEventKind, TraceSink, parse_session_id,
        resolve_trace_path, safe_error,
    },
};
use serde_json::Value;
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
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
    let client = OpenAiCompatibleClient::new(&config)?;
    let workspace = workspace_directory()?;
    let mut registry = build_native_tool_registry(&workspace)?;
    let mcp_config = McpConfig::from_env()?;
    let mcp_configured = mcp_config.is_some();
    let mut mcp = match mcp_config.as_ref() {
        Some(config) => McpManager::connect(config, &mut registry).await?,
        None => McpManager::default(),
    };
    let policy = DefaultPolicy;
    let approver = ConsoleApprovalHandler;

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
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    workspace: &Path,
    config: &Config,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
) -> Result<()> {
    println!("mini-harness V8 bounded MCP agent — type /exit to quit.");
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

async fn run_task(
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    workspace: &Path,
    config: &Config,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
    prompt: &str,
) -> Result<()> {
    let id = Uuid::new_v4();
    let writer = JsonlTraceWriter::create(workspace, id)?;
    let repository = match FileSessionRepository::new(workspace) {
        Ok(repository) => repository,
        Err(error) => return traced_setup_failure(&writer, 0, error),
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
        Err(error) => return traced_setup_failure(&writer, 0, error),
    };
    if let Err(error) = repository.create(&session) {
        return traced_setup_failure(&writer, 0, error);
    }
    let _lease = match repository.acquire(id) {
        Ok(lease) => lease,
        Err(error) => return traced_setup_failure(&writer, 0, error),
    };
    let checkpoint = SessionCheckpoint::new(&repository, session);
    println!("Session: {id}");
    println!("Trace: {}", writer.path().display());
    let runner = AgentRunner::with_persistence(
        client,
        registry,
        &config.model,
        policy,
        approver,
        ContextBuilder::with_sensitive_values(config.context.clone(), [config.api_key.clone()])?,
        RunPersistence {
            session_id: id,
            trace: &writer,
            sessions: &checkpoint,
        },
    );
    let mut state = state;
    print_outcome(runner.run(&mut state).await?)
}

#[allow(clippy::too_many_arguments)]
async fn resume_task(
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    workspace: &Path,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
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
            "session {id} used MCP tools; set MCP_CONFIG_PATH to a current V8 MCP configuration before resume"
        );
    }
    if session.state.status == AgentStatus::Completed {
        println!("Session {id} is already completed.");
        if let Some(content) = session.state.final_content() {
            println!("{content}");
        }
        return Ok(());
    }
    let writer = JsonlTraceWriter::open_append(workspace, id)?;
    let model = session.model.clone();
    let settings = session.context_settings.clone();
    let mut state = session.state.clone();
    state.prepare_for_resume();
    let checkpoint = SessionCheckpoint::new(&repository, session);
    if let Err(error) = checkpoint.checkpoint(&state) {
        return traced_setup_failure(&writer, state.step, error);
    }
    println!("Resuming session: {id}");
    println!("Trace: {}", writer.path().display());
    let runner = AgentRunner::with_persistence(
        client,
        registry,
        &model,
        policy,
        approver,
        ContextBuilder::with_sensitive_values(settings, [api_key.to_owned()])?,
        RunPersistence {
            session_id: id,
            trace: &writer,
            sessions: &checkpoint,
        },
    );
    print_outcome(runner.resume(&mut state).await?)
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
    "Usage:\n  mini-harness-v8\n  mini-harness-v8 run <prompt>\n  mini-harness-v8 resume <session-id>\n  mini-harness-v8 session <session-id>\n  mini-harness-v8 trace <session-id>\n\nMCP:\n  Set MCP_CONFIG_PATH to an absolute path to a strict V8 MCP config.\n  If absent, only native tools are registered.\n  V8 session schema 3 is strict; earlier schemas are not migrated."
}
fn print_usage() {
    println!("{}", usage());
}

#[cfg(test)]
mod tests {
    use super::*;
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
