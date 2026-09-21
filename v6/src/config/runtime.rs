use std::{env, fmt};

use anyhow::{Context, Result, bail};

/// Runtime configuration for the OpenAI-compatible provider.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub allow_insecure_loopback: bool,
}

impl Config {
    /// Loads configuration from the process environment and an optional local `.env` file.
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let api_key = required_env("OPENAI_API_KEY")?;
        let base_url = required_env("OPENAI_BASE_URL")?;
        let model = required_env("OPENAI_MODEL")?;
        let allow_insecure_loopback = parse_bool_env("ALLOW_INSECURE_LOOPBACK")?;

        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model,
            allow_insecure_loopback,
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &"[REDACTED]")
            .field("model", &self.model)
            .field("allow_insecure_loopback", &self.allow_insecure_loopback)
            .finish()
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

fn parse_bool_env(name: &str) -> Result<bool> {
    match env::var(name) {
        Ok(value) if value.trim().eq_ignore_ascii_case("true") => Ok(true),
        Ok(value) if value.trim().eq_ignore_ascii_case("false") => Ok(false),
        Ok(_) => bail!("{name} must be `true` or `false`"),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("{name} is not valid Unicode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_api_key() {
        let config = Config {
            api_key: "top-secret".into(),
            base_url: "https://example.test/v1".into(),
            model: "model".into(),
            allow_insecure_loopback: false,
        };

        let debug = format!("{config:?}");
        assert!(!debug.contains("top-secret"));
        assert!(!debug.contains("example.test"));
        assert!(debug.contains("[REDACTED]"));
    }
}
