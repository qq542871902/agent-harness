use std::env;

use anyhow::{Context, Result, bail};

/// Runtime configuration for the OpenAI-compatible provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl Config {
    /// Loads configuration from the process environment and an optional local `.env` file.
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let api_key = required_env("OPENAI_API_KEY")?;
        let base_url = required_env("OPENAI_BASE_URL")?;
        let model = required_env("OPENAI_MODEL")?;

        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model,
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("{name} is not set"))?;
    let value = value.trim();

    if value.is_empty() {
        bail!("{name} must not be empty");
    }

    Ok(value.to_owned())
}
