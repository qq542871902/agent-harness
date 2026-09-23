use anyhow::{Context, Result, bail};
use mini_harness_v9::config::Config;
use std::{env, fmt, str::FromStr};

const DEFAULT_CHUNK_LINES: usize = 40;
const DEFAULT_CHUNK_OVERLAP_LINES: usize = 8;
const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 16;
const MIN_CHUNK_LINES: usize = 4;
const MAX_CHUNK_LINES: usize = 200;
const MAX_EMBEDDING_BATCH_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalMode {
    Lexical,
    Vector,
    Hybrid,
}

impl RetrievalMode {
    pub fn uses_embeddings(self) -> bool {
        !matches!(self, Self::Lexical)
    }
}

impl FromStr for RetrievalMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "lexical" => Ok(Self::Lexical),
            "vector" => Ok(Self::Vector),
            "hybrid" => Ok(Self::Hybrid),
            _ => bail!("RAG_MODE must be one of: lexical, vector, hybrid"),
        }
    }
}

#[derive(Clone)]
pub struct EmbeddingConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub allow_http_loopback: bool,
    pub batch_size: usize,
}

impl fmt::Debug for EmbeddingConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddingConfig")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &"[REDACTED]")
            .field("model", &self.model)
            .field("allow_http_loopback", &self.allow_http_loopback)
            .field("batch_size", &self.batch_size)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RagConfig {
    pub mode: RetrievalMode,
    pub embedding: Option<EmbeddingConfig>,
    pub chunk_lines: usize,
    pub chunk_overlap_lines: usize,
}

impl RagConfig {
    pub fn from_env(runtime: &Config) -> Result<Self> {
        let mode = optional_env("RAG_MODE")?
            .as_deref()
            .unwrap_or("lexical")
            .parse::<RetrievalMode>()?;
        let chunk_lines = usize_env(
            "RAG_CHUNK_LINES",
            DEFAULT_CHUNK_LINES,
            MIN_CHUNK_LINES,
            MAX_CHUNK_LINES,
        )?;
        let chunk_overlap_lines = usize_env(
            "RAG_CHUNK_OVERLAP_LINES",
            DEFAULT_CHUNK_OVERLAP_LINES.min(chunk_lines.saturating_sub(1)),
            0,
            chunk_lines.saturating_sub(1),
        )?;

        let embedding = if mode.uses_embeddings() {
            let model = required_env("EMBEDDING_MODEL")?;
            let explicit_api_key = non_empty_optional_env("EMBEDDING_API_KEY")?;
            let explicit_base_url = non_empty_optional_env("EMBEDDING_BASE_URL")?;
            if explicit_base_url.is_some() && explicit_api_key.is_none() {
                bail!(
                    "EMBEDDING_API_KEY must be set when EMBEDDING_BASE_URL is set; refusing to send the chat API key to an independently configured endpoint"
                );
            }
            let api_key = explicit_api_key.unwrap_or_else(|| runtime.api_key.clone());
            let base_url = explicit_base_url.unwrap_or_else(|| runtime.base_url.clone());
            let batch_size = usize_env(
                "RAG_EMBEDDING_BATCH_SIZE",
                DEFAULT_EMBEDDING_BATCH_SIZE,
                1,
                MAX_EMBEDDING_BATCH_SIZE,
            )?;
            Some(EmbeddingConfig {
                api_key,
                base_url: base_url.trim_end_matches('/').to_owned(),
                model,
                allow_http_loopback: runtime.allow_http_loopback,
                batch_size,
            })
        } else {
            None
        };

        Ok(Self {
            mode,
            embedding,
            chunk_lines,
            chunk_overlap_lines,
        })
    }

    pub fn sensitive_values(&self, chat_api_key: &str) -> Vec<String> {
        let mut values = vec![chat_api_key.to_owned()];
        if let Some(embedding) = &self.embedding
            && embedding.api_key != chat_api_key
        {
            values.push(embedding.api_key.clone());
        }
        values
    }
}

fn required_env(name: &str) -> Result<String> {
    non_empty_optional_env(name)?.with_context(|| format!("{name} is not set"))
}

fn non_empty_optional_env(name: &str) -> Result<Option<String>> {
    match optional_env(name)? {
        Some(value) if value.trim().is_empty() => bail!("{name} must not be empty"),
        Some(value) => Ok(Some(value.trim().to_owned())),
        None => Ok(None),
    }
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{name} is not valid Unicode")),
    }
}

fn usize_env(name: &str, default: usize, minimum: usize, maximum: usize) -> Result<usize> {
    let Some(value) = optional_env(name)? else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<usize>()
        .with_context(|| format!("{name} must be an integer"))?;
    if !(minimum..=maximum).contains(&parsed) {
        bail!("{name} must be between {minimum} and {maximum}");
    }
    Ok(parsed)
}
