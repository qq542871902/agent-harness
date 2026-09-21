use crate::context::ContextSettings;
use anyhow::{Context, Result, bail};
use std::{env, fmt};

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub allow_http_loopback: bool,
    pub context: ContextSettings,
}

impl Config {
    /// Loads provider secrets plus optional, validated V7 context limits.
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let api_key = required_env("OPENAI_API_KEY")?;
        let base_url = required_env("OPENAI_BASE_URL")?;
        let model = required_env("OPENAI_MODEL")?;
        let allow_http_loopback = bool_env("ALLOW_HTTP_LOOPBACK")?;
        let token_budget = optional_env("CONTEXT_TOKEN_BUDGET")?;
        let max_output = optional_env("MAX_TOOL_OUTPUT_CHARS")?;
        let context =
            ContextSettings::from_optional_values(token_budget.as_deref(), max_output.as_deref())?;
        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model,
            allow_http_loopback,
            context,
        })
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &"[REDACTED]")
            .field("model", &self.model)
            .field("allow_http_loopback", &self.allow_http_loopback)
            .field("context", &self.context)
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
fn bool_env(name: &str) -> Result<bool> {
    match optional_env(name)?.as_deref().map(str::trim) {
        None | Some("") | Some("0" | "false" | "FALSE") => Ok(false),
        Some("1" | "true" | "TRUE") => Ok(true),
        Some(_) => bail!("{name} must be one of: true, false, 1, 0"),
    }
}
fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{name} is not valid Unicode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_redacts_provider_fields_and_keeps_context_visible() {
        let c = Config {
            api_key: "top-secret".into(),
            base_url: "https://example.test/v1".into(),
            model: "model".into(),
            allow_http_loopback: false,
            context: ContextSettings::default(),
        };
        let d = format!("{c:?}");
        assert!(!d.contains("top-secret") && !d.contains("example.test"));
        assert!(d.contains("[REDACTED]") && d.contains("token_budget"));
    }
}
