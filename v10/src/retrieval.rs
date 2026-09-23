use crate::{
    config::{RagConfig, RetrievalMode},
    embedding::EmbeddingClient,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use mini_harness_v9::{
    llm::ToolCall,
    policy::{DefaultPolicy, Policy, ToolPermission},
    tools::{Tool, ToolOutput},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::{Ordering, Reverse},
    collections::{BTreeSet, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use tokio::sync::Mutex;

const MAX_QUERY_CHARS: usize = 512;
const DEFAULT_TOP_K: usize = 5;
const MAX_TOP_K: usize = 10;
const MAX_SCANNED_FILES: usize = 500;
const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_TOTAL_BYTES: u64 = 4 * 1_048_576;
const MAX_SNIPPET_CHARS: usize = 600;
const MAX_EMBEDDING_INPUT_CHARS: usize = 8_000;
const MAX_VECTOR_CHUNKS: usize = 2_048;
const RRF_K: f32 = 60.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct V10Policy;

impl Policy for V10Policy {
    fn permission(&self, call: &ToolCall) -> ToolPermission {
        if call.name == "search_workspace_knowledge" {
            ToolPermission::Allow
        } else {
            DefaultPolicy.permission(call)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

#[derive(Debug, Clone, Serialize)]
struct SearchResult {
    path: String,
    start_line: usize,
    end_line: usize,
    score: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    lexical_score: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vector_similarity: Option<f32>,
    snippet: String,
}

#[derive(Debug)]
struct LexicalCandidate {
    result: SearchResult,
    term_matches: usize,
    occurrences: usize,
}

struct Document {
    path: String,
    content: String,
    fingerprint: u64,
}

struct VectorChunk {
    path: String,
    start_line: usize,
    end_line: usize,
    content: String,
    snippet: String,
    embedding: Vec<f32>,
}

struct VectorIndex {
    fingerprint: Vec<(String, u64)>,
    chunks: Vec<VectorChunk>,
}

struct SearchOutcome {
    results: Vec<SearchResult>,
    retrieval: &'static str,
    warning: Option<&'static str>,
    fallback_reason: Option<&'static str>,
}

/// Bounded workspace retrieval with local lexical search and opt-in remote
/// embeddings. Vector data is cached only in memory for the current process.
pub struct WorkspaceSearchTool {
    workspace: PathBuf,
    mode: RetrievalMode,
    embedding: Option<EmbeddingClient>,
    chunk_lines: usize,
    chunk_overlap_lines: usize,
    vector_index: Mutex<Option<VectorIndex>>,
}

impl WorkspaceSearchTool {
    pub fn with_config(workspace: impl AsRef<Path>, config: RagConfig) -> Result<Self> {
        let workspace = workspace.as_ref().canonicalize().with_context(|| {
            format!(
                "failed to resolve workspace `{}`",
                workspace.as_ref().display()
            )
        })?;
        if !workspace.is_dir() {
            bail!("workspace is not a directory: {}", workspace.display());
        }
        let embedding = config
            .embedding
            .as_ref()
            .map(EmbeddingClient::new)
            .transpose()?;
        if config.mode.uses_embeddings() && embedding.is_none() {
            bail!("vector retrieval requires an embedding configuration");
        }
        Ok(Self {
            workspace,
            mode: config.mode,
            embedding,
            chunk_lines: config.chunk_lines,
            chunk_overlap_lines: config.chunk_overlap_lines,
            vector_index: Mutex::new(None),
        })
    }

    fn lexical_search(&self, query: &str, top_k: usize) -> Result<Vec<SearchResult>> {
        let normalized_query = query.to_lowercase();
        let terms = query_terms(&normalized_query);
        if terms.is_empty() {
            bail!("query must contain at least one letter, number, or underscore");
        }
        let documents = self.load_documents()?;
        let mut candidates = Vec::new();
        for document in documents {
            candidates.extend(matching_lines(
                &document.path,
                &document.content,
                &normalized_query,
                &terms,
            ));
        }
        candidates.sort_by_key(|candidate| {
            (
                Reverse(candidate.result.score),
                Reverse(candidate.term_matches),
                Reverse(candidate.occurrences),
                candidate.result.path.clone(),
                candidate.result.start_line,
            )
        });
        candidates.truncate(top_k);
        Ok(candidates
            .into_iter()
            .map(|candidate| candidate.result)
            .collect())
    }

    async fn semantic_search(
        &self,
        query: &str,
        top_k: usize,
        hybrid: bool,
    ) -> Result<Vec<SearchResult>> {
        let documents = self.load_documents()?;
        let fingerprint = documents
            .iter()
            .map(|document| (document.path.clone(), document.fingerprint))
            .collect::<Vec<_>>();
        let embedding = self
            .embedding
            .as_ref()
            .context("embedding client is not configured")?;
        let mut index = self.vector_index.lock().await;
        if index
            .as_ref()
            .is_none_or(|current| current.fingerprint != fingerprint)
        {
            let mut chunks =
                chunk_documents(&documents, self.chunk_lines, self.chunk_overlap_lines)?;
            if chunks.is_empty() {
                return Ok(Vec::new());
            }
            let inputs = chunks.iter().map(embedding_input).collect::<Vec<String>>();
            let vectors = embedding.embed(&inputs).await?;
            if vectors.len() != chunks.len() {
                bail!("embedding client returned an unexpected vector count");
            }
            for (chunk, vector) in chunks.iter_mut().zip(vectors) {
                chunk.embedding = vector;
            }
            *index = Some(VectorIndex {
                fingerprint: fingerprint.clone(),
                chunks,
            });
        }

        let query_vector = embedding.embed(&[query.to_owned()]).await?;
        let query_vector = query_vector
            .first()
            .context("embedding client did not return the query vector")?;
        let index = index.as_ref().context("vector index was not initialized")?;
        let mut similarities = index
            .chunks
            .iter()
            .enumerate()
            .map(|(position, chunk)| {
                cosine_similarity(query_vector, &chunk.embedding).map(|score| (position, score))
            })
            .collect::<Result<Vec<_>>>()?;
        similarities.sort_by(|left, right| {
            descending_f32(left.1, right.1)
                .then_with(|| chunk_order(&index.chunks, left.0, right.0))
        });

        let ranked = if hybrid {
            hybrid_ranks(query, &index.chunks, &similarities)
        } else {
            similarities
                .iter()
                .enumerate()
                .map(|(rank, (position, similarity))| RankedChunk {
                    position: *position,
                    score: vector_display_score(*similarity),
                    lexical_score: None,
                    vector_similarity: *similarity,
                    vector_rank: rank,
                })
                .collect()
        };

        Ok(ranked
            .into_iter()
            .take(top_k)
            .map(|ranked| {
                let chunk = &index.chunks[ranked.position];
                SearchResult {
                    path: chunk.path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score: ranked.score,
                    lexical_score: ranked.lexical_score,
                    vector_similarity: Some(ranked.vector_similarity),
                    snippet: chunk.snippet.clone(),
                }
            })
            .collect())
    }

    fn load_documents(&self) -> Result<Vec<Document>> {
        let mut files = Vec::new();
        collect_text_files(&self.workspace, &self.workspace, &mut files)?;
        let mut total_bytes = 0_u64;
        let mut documents = Vec::new();
        for path in files {
            let metadata = fs::metadata(&path)
                .with_context(|| format!("failed to inspect `{}`", path.display()))?;
            if metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > MAX_TOTAL_BYTES {
                bail!("workspace retrieval exceeds the {MAX_TOTAL_BYTES}-byte read limit");
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let relative = path
                .strip_prefix(&self.workspace)
                .context("retrieval path escaped the workspace")?
                .display()
                .to_string();
            let mut hasher = DefaultHasher::new();
            content.hash(&mut hasher);
            documents.push(Document {
                path: relative,
                content,
                fingerprint: hasher.finish(),
            });
        }
        Ok(documents)
    }
}

#[async_trait]
impl Tool for WorkspaceSearchTool {
    fn name(&self) -> &str {
        "search_workspace_knowledge"
    }

    fn description(&self) -> &str {
        match self.mode {
            RetrievalMode::Lexical => {
                "Search non-sensitive UTF-8 workspace files using deterministic local lexical matching. Returns cited paths, line ranges, scores, and snippets. Use read_file to verify cited source."
            }
            RetrievalMode::Vector => {
                "Search non-sensitive UTF-8 workspace files by semantic vector similarity using configured embeddings. Returns cited paths, line ranges, scores, and snippets. Use read_file to verify cited source."
            }
            RetrievalMode::Hybrid => {
                "Search non-sensitive UTF-8 workspace files using fused lexical and semantic vector ranking. Returns cited paths, line ranges, scores, and snippets. Use read_file to verify cited source."
            }
        }
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Terms, symbol names, or a natural-language description to find in non-sensitive workspace text files"
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TOP_K,
                    "default": DEFAULT_TOP_K,
                    "description": "Maximum number of cited matching regions to return"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: SearchArguments = serde_json::from_value(arguments)
            .context("invalid arguments for search_workspace_knowledge")?;
        let query = arguments.query.trim();
        if query.is_empty() {
            bail!("query must not be empty");
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            bail!("query exceeds the {MAX_QUERY_CHARS}-character limit");
        }
        if !(1..=MAX_TOP_K).contains(&arguments.top_k) {
            bail!("top_k must be between 1 and {MAX_TOP_K}");
        }
        if query_terms(&query.to_lowercase()).is_empty() {
            bail!("query must contain at least one letter, number, or underscore");
        }

        let outcome = match self.mode {
            RetrievalMode::Lexical => SearchOutcome {
                results: self.lexical_search(query, arguments.top_k)?,
                retrieval: "local_lexical",
                warning: None,
                fallback_reason: None,
            },
            RetrievalMode::Vector => SearchOutcome {
                results: self.semantic_search(query, arguments.top_k, false).await?,
                retrieval: "remote_vector",
                warning: None,
                fallback_reason: None,
            },
            RetrievalMode::Hybrid => match self.semantic_search(query, arguments.top_k, true).await
            {
                Ok(results) => SearchOutcome {
                    results,
                    retrieval: "hybrid_lexical_vector",
                    warning: None,
                    fallback_reason: None,
                },
                Err(error) => SearchOutcome {
                    results: self.lexical_search(query, arguments.top_k)?,
                    retrieval: "local_lexical_fallback",
                    warning: Some(
                        "vector retrieval was unavailable; results use local lexical fallback",
                    ),
                    fallback_reason: Some(classify_vector_failure(&error)),
                },
            },
        };
        let content = serde_json::to_string_pretty(&json!({
            "query": query,
            "result_count": outcome.results.len(),
            "results": outcome.results,
            "retrieval": outcome.retrieval,
            "warning": outcome.warning,
            "fallback_reason": outcome.fallback_reason,
        }))
        .context("failed to serialize workspace search results")?;
        Ok(ToolOutput {
            content,
            success: true,
        })
    }
}

fn classify_vector_failure(error: &anyhow::Error) -> &'static str {
    let diagnostic = format!("{error:#}").to_ascii_lowercase();
    if diagnostic.contains("401") || diagnostic.contains("403") {
        "embedding_authentication"
    } else if diagnostic.contains("embedding request failed") {
        "embedding_transport"
    } else if diagnostic.contains("dimension") {
        "embedding_dimension"
    } else if diagnostic.contains("embedding api") || diagnostic.contains("embedding response") {
        "embedding_response"
    } else if diagnostic.contains("chunk") || diagnostic.contains("workspace") {
        "local_index"
    } else {
        "vector_unavailable"
    }
}

struct RankedChunk {
    position: usize,
    score: u32,
    lexical_score: Option<u32>,
    vector_similarity: f32,
    vector_rank: usize,
}

fn hybrid_ranks(
    query: &str,
    chunks: &[VectorChunk],
    similarities: &[(usize, f32)],
) -> Vec<RankedChunk> {
    let normalized_query = query.to_lowercase();
    let terms = query_terms(&normalized_query);
    let mut lexical = chunks
        .iter()
        .enumerate()
        .filter_map(|(position, chunk)| {
            lexical_components(&chunk.content.to_lowercase(), &normalized_query, &terms)
                .map(|(score, _, _)| (position, score))
        })
        .collect::<Vec<_>>();
    lexical.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| chunk_order(chunks, left.0, right.0))
    });
    let mut lexical_ranks = vec![None; chunks.len()];
    for (rank, (position, score)) in lexical.into_iter().enumerate() {
        lexical_ranks[position] = Some((rank, score));
    }

    let mut ranked = similarities
        .iter()
        .enumerate()
        .map(|(vector_rank, (position, similarity))| {
            let lexical = lexical_ranks[*position];
            let lexical_component = lexical
                .map(|(rank, _)| 1.0 / (RRF_K + rank as f32 + 1.0))
                .unwrap_or_default();
            let fused = 1.0 / (RRF_K + vector_rank as f32 + 1.0) + lexical_component;
            RankedChunk {
                position: *position,
                score: (fused * 1_000_000.0).round() as u32,
                lexical_score: lexical.map(|(_, score)| score),
                vector_similarity: *similarity,
                vector_rank,
            }
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.vector_rank.cmp(&right.vector_rank))
            .then_with(|| chunk_order(chunks, left.position, right.position))
    });
    ranked
}

fn chunk_documents(
    documents: &[Document],
    chunk_lines: usize,
    overlap_lines: usize,
) -> Result<Vec<VectorChunk>> {
    let mut chunks = Vec::new();
    for document in documents {
        let lines = document.content.lines().collect::<Vec<_>>();
        let mut start = 0;
        while start < lines.len() {
            let end = (start + chunk_lines).min(lines.len());
            let content = lines[start..end].join("\n");
            if !content.trim().is_empty() {
                chunks.push(VectorChunk {
                    path: document.path.clone(),
                    start_line: start + 1,
                    end_line: end,
                    snippet: truncate_chars(&content, MAX_SNIPPET_CHARS),
                    content,
                    embedding: Vec::new(),
                });
                if chunks.len() > MAX_VECTOR_CHUNKS {
                    bail!("workspace exceeds the {MAX_VECTOR_CHUNKS}-chunk vector index limit");
                }
            }
            if end == lines.len() {
                break;
            }
            start = end.saturating_sub(overlap_lines);
        }
    }
    Ok(chunks)
}

fn embedding_input(chunk: &VectorChunk) -> String {
    format!(
        "Path: {}\nLines: {}-{}\n{}",
        chunk.path, chunk.start_line, chunk.end_line, chunk.content
    )
    .chars()
    .take(MAX_EMBEDDING_INPUT_CHARS)
    .collect()
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f32> {
    if left.len() != right.len() {
        bail!("query and workspace embeddings have different dimensions");
    }
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    let denominator = left_norm * right_norm;
    if !denominator.is_finite() || denominator <= f32::EPSILON {
        bail!("cannot compare zero or non-finite embedding vectors");
    }
    let similarity = dot / denominator;
    if !similarity.is_finite() {
        bail!("embedding similarity is not finite");
    }
    Ok(similarity.clamp(-1.0, 1.0))
}

fn descending_f32(left: f32, right: f32) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

fn vector_display_score(similarity: f32) -> u32 {
    (similarity.max(0.0) * 1_000.0).round() as u32
}

fn chunk_order(chunks: &[VectorChunk], left: usize, right: usize) -> Ordering {
    chunks[left]
        .path
        .cmp(&chunks[right].path)
        .then_with(|| chunks[left].start_line.cmp(&chunks[right].start_line))
}

fn collect_text_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to list `{}`", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read an entry under `{}`", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .context("retrieval entry escaped the workspace")?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect `{}`", path.display()))?;
        if metadata.file_type().is_symlink() || is_sensitive_path(relative) {
            continue;
        }
        if metadata.is_dir() {
            if !is_excluded_directory(relative) {
                collect_text_files(root, &path, files)?;
            }
        } else if metadata.is_file() {
            files.push(path);
            if files.len() > MAX_SCANNED_FILES {
                bail!("workspace exceeds the {MAX_SCANNED_FILES}-file retrieval limit");
            }
        }
    }
    Ok(())
}

fn matching_lines(
    path: &str,
    content: &str,
    normalized_query: &str,
    terms: &[String],
) -> Vec<LexicalCandidate> {
    let lines = content.lines().collect::<Vec<_>>();
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let normalized_line = line.to_lowercase();
            let (score, term_matches, occurrences) =
                lexical_components(&normalized_line, normalized_query, terms)?;
            let start = index.saturating_sub(1);
            let end = (index + 1).min(lines.len().saturating_sub(1));
            let snippet = truncate_chars(&lines[start..=end].join("\n"), MAX_SNIPPET_CHARS);
            Some(LexicalCandidate {
                result: SearchResult {
                    path: path.to_owned(),
                    start_line: start + 1,
                    end_line: end + 1,
                    score,
                    lexical_score: None,
                    vector_similarity: None,
                    snippet,
                },
                term_matches,
                occurrences,
            })
        })
        .collect()
}

fn lexical_components(
    normalized_text: &str,
    normalized_query: &str,
    terms: &[String],
) -> Option<(u32, usize, usize)> {
    let term_matches = terms
        .iter()
        .filter(|term| normalized_text.contains(term.as_str()))
        .count();
    if term_matches == 0 {
        return None;
    }
    let occurrences = terms
        .iter()
        .map(|term| normalized_text.match_indices(term.as_str()).count())
        .sum::<usize>();
    let exact_phrase = normalized_text.contains(normalized_query);
    let score = (term_matches as u32 * 100)
        .saturating_add(occurrences as u32 * 10)
        .saturating_add(if exact_phrase { 1_000 } else { 0 });
    Some((score, term_matches, occurrences))
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = BTreeSet::new();
    let mut current = String::new();
    for character in query.chars() {
        if character.is_alphanumeric() || character == '_' {
            current.push(character);
        } else if !current.is_empty() {
            terms.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.insert(current);
    }
    terms.into_iter().take(12).collect()
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut characters = value.chars();
    let retained = characters.by_ref().take(limit).collect::<String>();
    if characters.next().is_some() {
        format!("{retained}…[truncated]")
    } else {
        retained
    }
}

fn is_excluded_directory(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component
                .as_os_str()
                .to_string_lossy()
                .to_ascii_lowercase()
                .as_str(),
            ".git" | "target" | "node_modules"
        )
    })
}

fn is_sensitive_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        (name.starts_with(".env") && name != ".env.example")
            || matches!(
                name.as_str(),
                ".sessions"
                    | "traces"
                    | ".ssh"
                    | ".gnupg"
                    | ".aws"
                    | ".azure"
                    | ".kube"
                    | ".docker"
                    | ".password-store"
                    | ".npmrc"
                    | ".yarnrc"
                    | ".netrc"
                    | ".pypirc"
                    | ".git-credentials"
                    | ".htpasswd"
                    | "auth.json"
                    | "credentials.toml"
                    | "secrets.toml"
                    | "credentials"
                    | "credential"
                    | "secrets"
                    | "private_keys"
                    | "id_rsa"
                    | "id_dsa"
                    | "id_ecdsa"
                    | "id_ed25519"
            )
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.ends_with(".p12")
            || name.ends_with(".pfx")
            || name.ends_with("credentials.json")
            || name.ends_with("service-account.json")
    })
}
