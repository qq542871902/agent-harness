mod agent;
mod config;
mod context;
mod llm;
mod policy;
mod session;
#[cfg(test)]
mod test_support;
mod tools;
mod trace;
use agent::{AgentOutcome, AgentRunner, AgentState, AgentStatus, RunPersistence};
use anyhow::{Context, Result, bail};
use config::Config;
use context::ContextBuilder;
use llm::{LlmClient, OpenAiCompatibleClient};
use policy::{ApprovalHandler, ConsoleApprovalHandler, DefaultPolicy, Policy};
use serde_json::Value;
use session::{FileSessionRepository, Session, SessionCheckpoint, SessionRepository, SessionSink};
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};
use tools::{ListFilesTool, ReadFileTool, ShellTool, ToolRegistry, WriteFileTool};
use trace::{
    JsonlTraceWriter, TraceEvent, TraceEventKind, TraceSink, parse_session_id, resolve_trace_path,
    safe_error,
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
    let registry = build_tool_registry(&workspace)?;
    let policy = DefaultPolicy;
    let approver = ConsoleApprovalHandler;
    match command {
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
                &id,
            )
            .await
        }
        Command::Interactive => {
            run_interactive(&client, &registry, &workspace, &config, &policy, &approver).await
        }
        Command::Trace(_) | Command::Session(_) => unreachable!(),
    }
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
            [c] if c == "--help" || c == "-h" => {
                print_usage();
                std::process::exit(0)
            }
            [c, prompt @ ..] if c == "run" && !prompt.is_empty() => Ok(Self::Run(prompt.join(" "))),
            [c, id] if c == "resume" => Ok(Self::Resume(id.clone())),
            [c, id] if c == "session" => Ok(Self::Session(id.clone())),
            [c, id] if c == "trace" => Ok(Self::Trace(id.clone())),
            [c, ..] if c == "run" => bail!("`run` requires a prompt\n\n{}", usage()),
            [c, ..] if matches!(c.as_str(), "resume" | "session" | "trace") => {
                bail!("`{c}` requires exactly one session ID\n\n{}", usage())
            }
            _ => bail!("unknown command\n\n{}", usage()),
        }
    }
}
fn build_tool_registry(workspace: &Path) -> Result<ToolRegistry> {
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
    println!("mini-harness V7 context-managed agent — type /exit to quit.");
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
        Ok(r) => r,
        Err(e) => return traced_setup_failure(&writer, 0, e),
    };
    let state = AgentState::new(prompt);
    let session = match Session::new(
        id,
        workspace,
        &config.model,
        config.context.clone(),
        state.clone(),
        writer.path().to_path_buf(),
    ) {
        Ok(s) => s,
        Err(e) => return traced_setup_failure(&writer, 0, e),
    };
    if let Err(e) = repository.create(&session) {
        return traced_setup_failure(&writer, 0, e);
    }
    let _lease = match repository.acquire(id) {
        Ok(l) => l,
        Err(e) => return traced_setup_failure(&writer, 0, e),
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
    run_with_sigint(&runner, &mut state, false).await
}
async fn resume_task(
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    workspace: &Path,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
    api_key: &str,
    raw_id: &str,
) -> Result<()> {
    let id = parse_session_id(raw_id)?;
    let repository = FileSessionRepository::new(workspace)?;
    let _lease = repository.acquire(id)?;
    let session = repository.load(id)?;
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
    if let Err(e) = checkpoint.checkpoint(&state) {
        return traced_setup_failure(&writer, state.step, e);
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
    run_with_sigint(&runner, &mut state, true).await
}
async fn run_with_sigint(
    runner: &AgentRunner<'_>,
    state: &mut AgentState,
    resumed: bool,
) -> Result<()> {
    tokio::select! {
        result = async {
            if resumed { runner.resume(state).await } else { runner.run(state).await }
        } => print_outcome(result?),
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for SIGINT")?;
            runner.abort(state)?;
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
            .and_then(|v| v.to_str())
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
    "Usage:\n  mini-harness-v7\n  mini-harness-v7 run <prompt>\n  mini-harness-v7 resume <session-id>\n  mini-harness-v7 session <session-id>\n  mini-harness-v7 trace <session-id>\n\nContext:\n  Requests preserve the coding prompt, original task, and newest coherent groups.\n  CONTEXT_TOKEN_BUDGET and MAX_TOOL_OUTPUT_CHARS override validated defaults.\n  V7 session schema 2 is strict; V6 schema 1 sessions are rejected without migration."
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
