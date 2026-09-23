use crate::config::EmbeddingConfig;
use anyhow::{Context, Result, bail};
use reqwest::{Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1_048_576;
const MAX_INPUT_CHARS: usize = 8_000;

#[derive(Clone)]
pub struct EmbeddingClient {
    http: Client,
    api_key: String,
    model: String,
    embeddings_url: Url,
    batch_size: usize,
}

impl EmbeddingClient {
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        let mut base_url = Url::parse(&config.base_url).with_context(|| {
            format!("EMBEDDING_BASE_URL is not a valid URL: {}", config.base_url)
        })?;
        if base_url.scheme() != "https" {
            let loopback = base_url.host_str().is_some_and(is_loopback_host);
            if base_url.scheme() != "http" || !config.allow_http_loopback || !loopback {
                bail!(
                    "EMBEDDING_BASE_URL must use HTTPS; plain HTTP requires ALLOW_HTTP_LOOPBACK=true and an explicit loopback host"
                );
            }
        }
        if !base_url.username().is_empty() || base_url.password().is_some() {
            bail!("EMBEDDING_BASE_URL must not contain credentials");
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let embeddings_url = base_url
            .join("embeddings")
            .context("failed to construct embeddings URL")?;
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to create embedding HTTP client")?;
        Ok(Self {
            http,
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            embeddings_url,
            batch_size: config.batch_size,
        })
    }

    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        for input in inputs {
            if input.chars().count() > MAX_INPUT_CHARS {
                bail!("embedding input exceeds the {MAX_INPUT_CHARS}-character limit");
            }
        }

        let mut embeddings = Vec::with_capacity(inputs.len());
        let mut dimension = None;
        for batch in inputs.chunks(self.batch_size) {
            let response = self
                .http
                .post(self.embeddings_url.clone())
                .bearer_auth(&self.api_key)
                .json(&EmbeddingRequest {
                    model: &self.model,
                    input: batch,
                })
                .send()
                .await
                .context("embedding request failed")?;
            let status = response.status();
            let body = read_response_body(response).await?;
            if status != StatusCode::OK {
                bail!("embedding API returned {status}");
            }
            let response: EmbeddingResponse = serde_json::from_str(&body)
                .context("embedding API returned an invalid response")?;
            let mut ordered = vec![None; batch.len()];
            for item in response.data {
                if item.index >= ordered.len() {
                    bail!("embedding API returned an out-of-range index");
                }
                if ordered[item.index].is_some() {
                    bail!("embedding API returned a duplicate index");
                }
                validate_embedding(&item.embedding, &mut dimension)?;
                ordered[item.index] = Some(item.embedding);
            }
            if ordered.iter().any(Option::is_none) {
                bail!("embedding API returned fewer vectors than requested");
            }
            embeddings.extend(ordered.into_iter().flatten());
        }
        Ok(embeddings)
    }
}

fn validate_embedding(embedding: &[f32], dimension: &mut Option<usize>) -> Result<()> {
    if embedding.is_empty() {
        bail!("embedding API returned an empty vector");
    }
    if embedding.iter().any(|value| !value.is_finite()) {
        bail!("embedding API returned a non-finite vector value");
    }
    if embedding.iter().map(|value| value * value).sum::<f32>() <= f32::EPSILON {
        bail!("embedding API returned a zero vector");
    }
    match *dimension {
        Some(expected) if expected != embedding.len() => {
            bail!("embedding API returned inconsistent vector dimensions")
        }
        None => *dimension = Some(embedding.len()),
        Some(_) => {}
    }
    Ok(())
}

async fn read_response_body(mut response: Response) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        bail!("embedding response exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read embedding response body")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            bail!("embedding response exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).context("embedding response body is not valid UTF-8")
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}
