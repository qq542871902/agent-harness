mod agent;
mod config;
mod llm;
mod tools;

use std::io::{self, BufRead, Write};

use agent::{AgentOutcome, AgentRunner, AgentState};
use anyhow::{Context, Result, bail};
use config::Config;
use llm::OpenAiCompatibleClient;
use tools::{ListFilesTool, ReadFileTool, ToolRegistry};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let command = Command::parse(std::env::args().skip(1))?;
    let config = Config::from_env()?;
    let client = OpenAiCompatibleClient::new(&config)?;
    let registry = build_tool_registry()?;
    let runner = AgentRunner::new(&client, &registry, &config.model);

    match command {
        Command::Run(prompt) => run_task(&runner, &prompt).await,
        Command::Interactive => run_interactive(&runner).await,
    }
}

enum Command {
    Interactive,
    Run(String),
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
            [command, ..] if command == "run" => bail!("`run` requires a prompt\n\n{}", usage()),
            _ => bail!("unknown command\n\n{}", usage()),
        }
    }
}

fn build_tool_registry() -> Result<ToolRegistry> {
    let workspace =
        std::env::current_dir().context("failed to determine the workspace directory")?;
    let mut registry = ToolRegistry::new();
    registry.register(ReadFileTool::new(&workspace)?);
    registry.register(ListFilesTool::new(&workspace)?);
    Ok(registry)
}

async fn run_interactive(runner: &AgentRunner<'_>) -> Result<()> {
    println!("mini-harness V2 agent — type /exit to quit.");
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

        if let Err(error) = run_task(runner, prompt).await {
            eprintln!("Error: {error:#}");
        }
    }
}

async fn run_task(runner: &AgentRunner<'_>, prompt: &str) -> Result<()> {
    let mut state = AgentState::new(prompt);

    match runner.run(&mut state).await? {
        AgentOutcome::Completed { content } => println!("{content}"),
        AgentOutcome::MaxStepsReached { steps } => {
            println!("Agent stopped after reaching its {steps}-step limit without a final answer.")
        }
    }

    Ok(())
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
    "Usage:\n  mini-harness-v2\n  mini-harness-v2 run <prompt>\n\nBehavior:\n  Runs a bounded multi-turn agent loop with read_file and list_files enabled.\n  Tool observations are sent back to the model until it gives a final answer or\n  the default 20-model-request limit is reached.\n\nEnvironment:\n  OPENAI_API_KEY   API key for the OpenAI-compatible provider\n  OPENAI_BASE_URL  Provider API base URL, for example https://api.openai.com/v1\n  OPENAI_MODEL     Model identifier"
}

fn print_usage() {
    println!("{}", usage());
}
