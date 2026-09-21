mod config;
mod llm;

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use config::Config;
use llm::{ChatRequest, LlmClient, ModelResponse, OpenAiCompatibleClient};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let command = Command::parse(std::env::args().skip(1))?;
    let config = Config::from_env()?;
    let client = OpenAiCompatibleClient::new(&config)?;

    match command {
        Command::Run(prompt) => run_prompt(&client, &config.model, &prompt).await,
        Command::Interactive => run_interactive(&client, &config.model).await,
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

async fn run_interactive(client: &impl LlmClient, model: &str) -> Result<()> {
    println!("mini-harness V0 — type /exit to quit.");
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

        if let Err(error) = run_prompt(client, model, prompt).await {
            eprintln!("Error: {error:#}");
        }
    }
}

async fn run_prompt(client: &impl LlmClient, model: &str, prompt: &str) -> Result<()> {
    info!(model, "sending chat request");

    match client
        .chat(ChatRequest::from_user_prompt(model, prompt))
        .await?
    {
        ModelResponse::Final { content } => println!("{content}"),
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
    "Usage:\n  mini-harness-v0\n  mini-harness-v0 run <prompt>\n\nEnvironment:\n  OPENAI_API_KEY   API key for the OpenAI-compatible provider\n  OPENAI_BASE_URL  Provider API base URL, for example https://api.openai.com/v1\n  OPENAI_MODEL     Model identifier"
}

fn print_usage() {
    println!("{}", usage());
}
