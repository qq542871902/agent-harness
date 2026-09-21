mod config;
mod llm;
mod tools;

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use config::Config;
use llm::{ChatRequest, LlmClient, ModelResponse, OpenAiCompatibleClient};
use tools::{ListFilesTool, ReadFileTool, ToolRegistry};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let command = Command::parse(std::env::args().skip(1))?;
    let config = Config::from_env()?;
    let client = OpenAiCompatibleClient::new(&config)?;
    let registry = command
        .tools_enabled()
        .then(build_tool_registry)
        .transpose()?;

    match command {
        Command::Run {
            prompt,
            tools_enabled: _,
        } => run_prompt(&client, &config.model, &prompt, registry.as_ref()).await,
        Command::Interactive { tools_enabled } => {
            run_interactive(&client, &config.model, registry.as_ref(), tools_enabled).await
        }
    }
}

enum Command {
    Interactive { tools_enabled: bool },
    Run { prompt: String, tools_enabled: bool },
}

impl Command {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let arguments: Vec<String> = arguments.collect();

        match arguments.as_slice() {
            [] => Ok(Self::Interactive {
                tools_enabled: false,
            }),
            [flag] if flag == "--tools" => Ok(Self::Interactive {
                tools_enabled: true,
            }),
            [command] if command == "--help" || command == "-h" => {
                print_usage();
                std::process::exit(0);
            }
            [command, flag, prompt @ ..]
                if command == "run" && flag == "--tools" && !prompt.is_empty() =>
            {
                Ok(Self::Run {
                    prompt: prompt.join(" "),
                    tools_enabled: true,
                })
            }
            [command, flag] if command == "run" && flag == "--tools" => {
                bail!("`run --tools` requires a prompt\n\n{}", usage())
            }
            [command, prompt @ ..] if command == "run" && !prompt.is_empty() => Ok(Self::Run {
                prompt: prompt.join(" "),
                tools_enabled: false,
            }),
            [command, ..] if command == "run" => bail!("`run` requires a prompt\n\n{}", usage()),
            _ => bail!("unknown command\n\n{}", usage()),
        }
    }

    fn tools_enabled(&self) -> bool {
        match self {
            Self::Interactive { tools_enabled } | Self::Run { tools_enabled, .. } => *tools_enabled,
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

async fn run_interactive(
    client: &impl LlmClient,
    model: &str,
    registry: Option<&ToolRegistry>,
    tools_enabled: bool,
) -> Result<()> {
    let mode = if tools_enabled { "V1 tools" } else { "V0 chat" };
    println!("mini-harness {mode} — type /exit to quit.");
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

        if let Err(error) = run_prompt(client, model, prompt, registry).await {
            eprintln!("Error: {error:#}");
        }
    }
}

async fn run_prompt(
    client: &impl LlmClient,
    model: &str,
    prompt: &str,
    registry: Option<&ToolRegistry>,
) -> Result<()> {
    info!(
        model,
        tools_enabled = registry.is_some(),
        "sending chat request"
    );

    let mut request = ChatRequest::from_user_prompt(model, prompt);
    if let Some(registry) = registry {
        request = request.with_tools(registry.definitions());
    }

    match client.chat(request).await? {
        ModelResponse::Final { content } => println!("{content}"),
        ModelResponse::ToolCalls { calls } => {
            let registry =
                registry.context("model returned tool calls while tool mode is disabled")?;
            println!("Model requested {} tool call(s):", calls.len());

            for call in calls {
                println!("\nTool: {}", call.name);
                match registry.execute(&call).await {
                    Ok(output) => println!("Success: {}\n{}", output.success, output.content),
                    Err(error) => println!("Success: false\nTool execution failed: {error:#}"),
                }
            }
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
    "Usage:\n  mini-harness-v1\n  mini-harness-v1 --tools\n  mini-harness-v1 run <prompt>\n  mini-harness-v1 run --tools <prompt>\n\nOptions:\n  --tools  Enable V1 read-only workspace tools for this request or interactive session.\n\nEnvironment:\n  OPENAI_API_KEY   API key for the OpenAI-compatible provider\n  OPENAI_BASE_URL  Provider API base URL, for example https://api.openai.com/v1\n  OPENAI_MODEL     Model identifier"
}

fn print_usage() {
    println!("{}", usage());
}
