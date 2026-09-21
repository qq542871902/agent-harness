mod agent;
mod config;
mod llm;
mod policy;
#[cfg(test)]
mod test_support;
mod tools;
mod trace;

use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};

use agent::{AgentOutcome, AgentRunner, AgentState};
use anyhow::{Context, Result, bail};
use config::Config;
use llm::{LlmClient, OpenAiCompatibleClient};
use policy::{ApprovalHandler, ConsoleApprovalHandler, DefaultPolicy, Policy};
use serde_json::Value;
use tools::{ListFilesTool, ReadFileTool, ShellTool, ToolRegistry, WriteFileTool};
use trace::{JsonlTraceWriter, parse_session_id, resolve_trace_path};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let command = Command::parse(std::env::args().skip(1))?;
    if let Command::Trace(session_id) = command {
        return display_trace(&session_id);
    }

    let config = Config::from_env()?;
    let client = OpenAiCompatibleClient::new(&config)?;
    let registry = build_tool_registry()?;
    let policy = DefaultPolicy;
    let approver = ConsoleApprovalHandler;

    match command {
        Command::Run(prompt) => {
            run_task(
                &client,
                &registry,
                &config.model,
                &policy,
                &approver,
                &prompt,
            )
            .await
        }
        Command::Interactive => {
            run_interactive(&client, &registry, &config.model, &policy, &approver).await
        }
        Command::Trace(_) => unreachable!("trace command returned before provider setup"),
    }
}

enum Command {
    Interactive,
    Run(String),
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
            [command, session_id] if command == "trace" => Ok(Self::Trace(session_id.clone())),
            [command, ..] if command == "run" => bail!("`run` requires a prompt\n\n{}", usage()),
            [command, ..] if command == "trace" => {
                bail!("`trace` requires exactly one session ID\n\n{}", usage())
            }
            _ => bail!("unknown command\n\n{}", usage()),
        }
    }
}

fn build_tool_registry() -> Result<ToolRegistry> {
    let workspace =
        std::env::current_dir().context("failed to determine the workspace directory")?;
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(&workspace)?)?;
    registry.register(ListFilesTool::new(&workspace)?)?;
    registry.register(WriteFileTool::new(&workspace)?.with_reserved_subtree("traces")?)?;
    registry.register(ShellTool::new(&workspace)?)?;
    Ok(registry)
}

async fn run_interactive(
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    model: &str,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
) -> Result<()> {
    println!("mini-harness V5 traced policy/approval agent — type /exit to quit.");
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

        if let Err(error) = run_task(client, registry, model, policy, approver, prompt).await {
            eprintln!("Error: {error:#}");
        }
    }
}

async fn run_task(
    client: &dyn LlmClient,
    registry: &ToolRegistry,
    model: &str,
    policy: &dyn Policy,
    approver: &dyn ApprovalHandler,
    prompt: &str,
) -> Result<()> {
    let session_id = Uuid::new_v4();
    let workspace = workspace_directory()?;
    let writer = JsonlTraceWriter::create(&workspace, session_id)?;
    println!("Session: {session_id}");
    println!("Trace: {}", writer.path().display());

    let runner = AgentRunner::with_trace(
        client, registry, model, policy, approver, session_id, &writer,
    );
    let mut state = AgentState::new(prompt);

    match runner.run(&mut state).await? {
        AgentOutcome::Completed { content } => println!("{content}"),
        AgentOutcome::MaxStepsReached { steps } => {
            println!("Agent stopped after reaching its {steps}-step limit without a final answer.")
        }
    }

    Ok(())
}

fn display_trace(session_id: &str) -> Result<()> {
    let session_id = parse_session_id(session_id)?;
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
    "Usage:\n  mini-harness-v5\n  mini-harness-v5 run <prompt>\n  mini-harness-v5 trace <session-id>\n\nBehavior:\n  Runs the V4 policy-checked coding-agent loop and flushes a structured trace\n  to traces/{session_id}.jsonl. Each interactive task gets a new session.\n  Trace arguments omit tool payloads, and fatal diagnostics are bounded and\n  credential-redacted. The default limit is 20 model requests.\n\nEnvironment:\n  OPENAI_API_KEY   API key for the OpenAI-compatible provider\n  OPENAI_BASE_URL  Provider API base URL, for example https://api.openai.com/v1\n  OPENAI_MODEL     Model identifier"
}

fn print_usage() {
    println!("{}", usage());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_command_requires_one_unmodified_identifier() {
        assert!(matches!(
            Command::parse(["trace".into(), Uuid::nil().to_string()].into_iter()).unwrap(),
            Command::Trace(_)
        ));
        assert!(
            Command::parse(["trace".into(), "../x".into(), "extra".into()].into_iter()).is_err()
        );
    }
}
