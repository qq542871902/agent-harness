mod agent;
mod config;
mod llm;
mod policy;
mod session;
#[cfg(test)]
mod test_support;
mod tools;
mod trace;

use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use agent::{AgentOutcome, AgentRunner, AgentState, AgentStatus, RunPersistence};
use anyhow::{Context, Result, bail};
use config::Config;
use llm::{LlmClient, OpenAiCompatibleClient};
use policy::{ApprovalHandler, ConsoleApprovalHandler, DefaultPolicy, Policy};
use serde_json::Value;
use session::{FileSessionRepository, Session, SessionCheckpoint, SessionRepository, SessionSink};
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
        Command::Trace(session_id) => return display_trace(session_id),
        Command::Session(session_id) => return display_session(session_id),
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
                &client,
                &registry,
                &workspace,
                &config.model,
                &policy,
                &approver,
                &prompt,
            )
            .await
        }
        Command::Resume(session_id) => {
            resume_task(
                &client,
                &registry,
                &workspace,
                &policy,
                &approver,
                &session_id,
            )
            .await
        }
        Command::Interactive => {
            run_interactive(
                &client,
                &registry,
                &workspace,
                &config.model,
                &policy,
                &approver,
            )
            .await
        }
        Command::Trace(_) | Command::Session(_) => {
            unreachable!("display commands returned before provider setup")
        }
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
        let arguments: Vec<String> = arguments.collect();
        match arguments.as_slice() {
            [] => Ok(Self::Interactive),
            [command] if command == "--help" || command == "-h" => {
                print_usage();
                std::process::exit(0);
            }
            [command, prompt @ ..] if command == "run" && !prompt.is_empty() => {
                Ok(Self::Run(prompt.join(" ")))
            }
            [command, session_id] if command == "resume" => Ok(Self::Resume(session_id.clone())),
            [command, session_id] if command == "session" => Ok(Self::Session(session_id.clone())),
            [command, session_id] if command == "trace" => Ok(Self::Trace(session_id.clone())),
            [command, ..] if command == "run" => bail!("`run` requires a prompt\n\n{}", usage()),
            [command, ..] if matches!(command.as_str(), "resume" | "session" | "trace") => {
                bail!("`{command}` requires exactly one session ID\n\n{}", usage())
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
    model: &str,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
) -> Result<()> {
    println!("mini-harness V6 persistent-session agent — type /exit to quit.");
    let stdin = io::stdin();
    let mut input = String::new();
    loop {
        print!("mini-harness> ");
        io::stdout().flush().context("failed to flush stdout")?;
        input.clear();
        if stdin
            .lock()
            .read_line(&mut input)
            .context("failed to read stdin")?
            == 0
        {
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
        if let Err(error) =
            run_task(client, registry, workspace, model, policy, approver, prompt).await
        {
            eprintln!("Error: {error:#}");
        }
    }
}

async fn run_task(
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    workspace: &Path,
    model: &str,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
    prompt: &str,
) -> Result<()> {
    let session_id = Uuid::new_v4();
    let writer = JsonlTraceWriter::create(workspace, session_id)?;
    let repository = match FileSessionRepository::new(workspace) {
        Ok(repository) => repository,
        Err(error) => return traced_setup_failure(&writer, 0, error),
    };
    let state = AgentState::new(prompt);
    let session = match Session::new(
        session_id,
        workspace,
        model,
        state.clone(),
        writer.path().to_path_buf(),
    ) {
        Ok(session) => session,
        Err(error) => return traced_setup_failure(&writer, 0, error),
    };
    if let Err(error) = repository.create(&session) {
        return traced_setup_failure(&writer, 0, error);
    }
    let _lease = match repository.acquire(session_id) {
        Ok(lease) => lease,
        Err(error) => return traced_setup_failure(&writer, 0, error),
    };
    let checkpoint = SessionCheckpoint::new(&repository, session);

    println!("Session: {session_id}");
    println!("Trace: {}", writer.path().display());
    let runner = AgentRunner::with_persistence(
        client,
        registry,
        model,
        policy,
        approver,
        RunPersistence {
            session_id,
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
    raw_session_id: &str,
) -> Result<()> {
    let session_id = parse_session_id(raw_session_id)?;
    let repository = FileSessionRepository::new(workspace)?;
    let _lease = repository.acquire(session_id)?;
    let session = repository.load(session_id)?;
    if session.state.status == AgentStatus::Completed {
        println!("Session {session_id} is already completed.");
        if let Some(content) = session.state.final_content() {
            println!("{content}");
        }
        return Ok(());
    }

    let writer = JsonlTraceWriter::open_append(workspace, session_id)?;
    let model = session.model.clone();
    let mut state = session.state.clone();
    state.prepare_for_resume();
    let checkpoint = SessionCheckpoint::new(&repository, session);
    if let Err(error) = checkpoint.checkpoint(&state) {
        return traced_setup_failure(&writer, state.step, error);
    }

    println!("Resuming session: {session_id}");
    println!("Trace: {}", writer.path().display());
    let runner = AgentRunner::with_persistence(
        client,
        registry,
        &model,
        policy,
        approver,
        RunPersistence {
            session_id,
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
            if resumed {
                runner.resume(state).await
            } else {
                runner.run(state).await
            }
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
    writer.record(&TraceEvent::new(
        Uuid::parse_str(
            writer
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .context("trace path does not contain a session UUID")?,
        )
        .context("trace path contains an invalid session UUID")?,
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

fn display_trace(raw_session_id: &str) -> Result<()> {
    let session_id = parse_session_id(raw_session_id)?;
    let workspace = workspace_directory()?;
    let canonical_path = resolve_trace_path(&workspace, session_id)?;
    let contents = std::fs::read_to_string(&canonical_path)
        .with_context(|| format!("failed to read trace `{}`", canonical_path.display()))?;
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

fn display_session(raw_session_id: &str) -> Result<()> {
    let session_id = parse_session_id(raw_session_id)?;
    let workspace = workspace_directory()?;
    let repository = FileSessionRepository::new(&workspace)?;
    let session = repository.load(session_id)?;
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
    "Usage:\n  mini-harness-v6\n  mini-harness-v6 run <prompt>\n  mini-harness-v6 resume <session-id>\n  mini-harness-v6 session <session-id>\n  mini-harness-v6 trace <session-id>\n\nBehavior:\n  Persists versioned sessions beneath .sessions and append-only traces beneath\n  traces. Resume preserves cumulative steps; Failed and MaxStepsReached sessions\n  receive a fresh 20-model-request budget. Completed sessions are displayed only.\n  Approval caches are transient and are never persisted.\n\nEnvironment:\n  OPENAI_API_KEY   API key for the OpenAI-compatible provider\n  OPENAI_BASE_URL  Provider API base URL, for example https://api.openai.com/v1\n  OPENAI_MODEL     Model identifier used for new sessions"
}

fn print_usage() {
    println!("{}", usage());
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use llm::{ChatRequest, ModelResponse};
    use test_support::TestWorkspace;

    struct NeverClient;

    #[async_trait]
    impl LlmClient for NeverClient {
        async fn chat(&self, _request: ChatRequest) -> Result<ModelResponse> {
            panic!("completed session must not call the provider")
        }
    }

    struct NeverApprover;

    impl ApprovalHandler for NeverApprover {
        fn request_approval(&self, _call: &llm::ToolCall) -> Result<policy::ApprovalDecision> {
            panic!("completed session must not request approval")
        }
    }

    #[test]
    fn session_commands_require_one_unmodified_identifier() {
        for command in ["trace", "resume", "session"] {
            assert!(Command::parse([command.into(), Uuid::nil().to_string()].into_iter()).is_ok());
            assert!(
                Command::parse([command.into(), "../x".into(), "extra".into()].into_iter())
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn completed_resume_returns_without_provider_or_tool_execution() {
        let workspace = TestWorkspace::new("completed-command");
        let session_id = Uuid::from_u128(25);
        let writer = JsonlTraceWriter::create(workspace.path(), session_id).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let mut state = AgentState::new("task");
        state.messages.push(llm::Message::assistant("already done"));
        state.status = AgentStatus::Completed;
        let session = Session::new(
            session_id,
            workspace.path(),
            "test-model",
            state,
            writer.path().to_path_buf(),
        )
        .unwrap();
        repository.create(&session).unwrap();
        drop(writer);

        resume_task(
            &NeverClient,
            &ToolRegistry::new(),
            workspace.path(),
            &DefaultPolicy,
            &NeverApprover,
            &session_id.to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            repository.load(session_id).unwrap().state.status,
            AgentStatus::Completed
        );
    }

    #[test]
    fn completed_resume_state_is_not_reset_and_failed_gets_fresh_budget() {
        let mut completed = AgentState::new("task");
        completed.status = AgentStatus::Completed;
        completed.prepare_for_resume();
        assert_eq!(completed.status, AgentStatus::Completed);

        let mut failed = AgentState::with_max_steps("task", 2);
        failed.status = AgentStatus::Failed;
        failed.step = 2;
        failed.prepare_for_resume();
        assert_eq!(failed.status, AgentStatus::Ready);
        assert_eq!(failed.max_steps, 22);
    }
}
