//! Embedding-based suggestions for unmatched imported lines (follow-up of the
//! Tesco import, issue #107).
//!
//! The deterministic cascade ([`crate::matching::resolve`]) is lexical: a branded
//! retailer SKU rarely contains a catalogue staple verbatim, so thousands of
//! imported lines stay `unmatched`. The embedding bake-off measured that a text
//! embedding model ranks the correct staple in its top five for ~79.5% of the
//! residual names, and that this is best used to **rank a clarification
//! shortlist**, never to auto-merge.
//!
//! This module is that ranking half:
//!
//!   * [`Embedder`] is the transport seam. It turns text into unit vectors; the
//!     production [`CommandEmbedder`] runs a configurable external command (the
//!     bundled `scripts/embed_names.py`, or any equivalent) so the Rust binary
//!     carries **no** ML runtime and no native dependency. Unit tests inject a
//!     fake via [`Embedder`], exactly like the LLM fallback's seam.
//!   * The batch steps [`embed_catalogue`] and [`embed_unmatched_names`] fill the
//!     two vector stores from migration 022: one vector per catalogue string
//!     (`name_en` / `name_es` / alias) and one per distinct unmatched line name.
//!     Both are idempotent — a string already embedded for the configured model
//!     is skipped — so re-running the batch only embeds what is new.
//!   * [`rank_candidates`] is the pure ranking kernel: cosine similarity of a
//!     query vector against the catalogue, best string per item, top `k`. It is
//!     deterministic (score descending, canonical-item id ascending on ties) and
//!     has no database or model dependency, so it is unit-tested with invented
//!     fixture vectors.
//!   * [`unmatched_suggestions`] and [`confirm_suggestion`] are the review and
//!     confirmation half used by the API: the first lists unmatched lines with
//!     their top-5 candidates and scores; the second binds the chosen lines and
//!     learns the mapping as an `accepted_match` alias through the single alias
//!     write path ([`crate::matching::commit_alias`]).
//!
//! **Nothing is ever matched without an explicit confirmation.** Filling the
//! vector stores, computing suggestions and reading them back never write
//! `purchase_lines.canonical_item_id`; only [`confirm_suggestion`] does, and only
//! for the item the caller names.
//!
//! ## Vectors
//!
//! A vector is stored as a BLOB of little-endian `f32` components (4 bytes each)
//! with its component count in `dim`, so a reader can reject a vector written by
//! an incompatible model. Every query filters on the configured `model` id, so
//! switching models leaves the previous vectors inert rather than silently
//! mixing vector spaces.
//!
//! ## Configuration
//!
//! [`EmbeddingConfig::from_env`] reads the process environment; the feature is
//! OFF unless `EMBEDDING_ENABLED=true` **and** `EMBEDDING_COMMAND` is set. The
//! command receives a JSON request on stdin and must print a JSON response on
//! stdout (see [`CommandEmbedder`]). `EMBEDDING_MODEL` tags the stored vectors,
//! `EMBEDDING_MODEL_PATH` is forwarded to the child as
//! `BUYWISE_EMBED_MODEL_PATH` (the weights live outside the repository), and
//! `EMBEDDING_DIM` / `EMBEDDING_TIMEOUT_SECS` bound the expected shape and the
//! wall-clock budget.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{Sqlite, SqliteConnection, SqlitePool};
use sqlx::{QueryBuilder, Row};

use crate::db::{self, DbError};
use crate::matching::{self, AliasSource};

/// The model id used when `EMBEDDING_MODEL` is unset: EmbeddingGemma 2's text
/// part (Apache 2.0, 768 dimensions), the bake-off's winner.
pub const DEFAULT_MODEL: &str = "google/embeddinggemma-2";

/// The expected vector dimension when `EMBEDDING_DIM` is unset.
pub const DEFAULT_DIM: usize = 768;

/// Default wall-clock budget for one embedding command invocation, in seconds.
/// Generous because the child loads the model on first use (a few seconds) and
/// embeds the whole batch in one call.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// How many candidates a suggestion list carries by default.
pub const DEFAULT_TOP_K: usize = 5;

/// Which prompt the embedded text is for.
///
/// The bake-off's model uses distinct query and document prompts; keeping the
/// distinction in the seam lets the child apply the model's own prompts without
/// the Rust side knowing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedKind {
    /// A catalogue string (a canonical name or an alias).
    Document,
    /// An imported line name (the search query).
    Query,
}

impl EmbedKind {
    /// The wire/prompt token for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedKind::Document => "document",
            EmbedKind::Query => "query",
        }
    }
}

/// Errors the embedding seam can produce.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// The feature is disabled or not fully configured — no command was run.
    #[error("embedding disabled or not configured")]
    Disabled,
    /// The command could not be started.
    #[error("embedding command failed to start: {0}")]
    Spawn(String),
    /// The command exited non-zero.
    #[error("embedding command exited with status {0}")]
    Status(i32),
    /// The command's stdout was not the expected JSON shape.
    #[error("embedding command output malformed: {0}")]
    Malformed(String),
    /// The command did not finish inside the configured timeout.
    #[error("embedding command timed out after {0}s")]
    Timeout(u64),
    /// The command returned a different number of vectors or a different
    /// dimension than requested, so the results cannot be trusted.
    #[error("embedding shape mismatch: expected {want} vectors of dim {want_dim}, got {got} of dim {got_dim}")]
    Shape {
        /// Requested vector count.
        want: usize,
        /// Requested dimension.
        want_dim: usize,
        /// Returned vector count.
        got: usize,
        /// Returned dimension (0 when the batch was empty).
        got_dim: usize,
    },
    /// A database read or write failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for EmbedError {
    fn from(error: sqlx::Error) -> Self {
        EmbedError::Db(DbError::from(error))
    }
}

/// The pluggable embedding transport. `dyn`-compatible via [`async_trait`] so
/// [`EmbeddingConfig`]-built state and tests can hold an `Arc<dyn Embedder>`.
#[allow(
    clippy::double_must_use,
    reason = "async_trait generates a must_use future for async trait methods"
)]
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Whether this embedder can actually run — a cheap, synchronous,
    /// side-effect-free check the caller uses to short-circuit before doing any
    /// work when the feature is off (the production default).
    fn is_enabled(&self) -> bool;

    /// The model id tagged on every stored vector.
    fn model_id(&self) -> String;

    /// The vector dimension this embedder produces.
    fn dim(&self) -> usize;

    /// Embed `texts` for `kind`, returning one vector per input in order.
    ///
    /// MUST NOT retry on failure, and MUST return exactly `texts.len()` vectors
    /// (the caller validates the shape and refuses a mismatch).
    async fn embed(&self, kind: EmbedKind, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Configuration for the external-command embedder, read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfig {
    /// Master switch (`EMBEDDING_ENABLED`, default `false`).
    pub enabled: bool,
    /// The external command, whitespace-separated (`EMBEDDING_COMMAND`). Its
    /// first token is the program; the rest are arguments. Quoting is not
    /// interpreted, so an argument must not contain whitespace.
    pub command: Option<String>,
    /// Model id tagged on stored vectors (`EMBEDDING_MODEL`, default
    /// [`DEFAULT_MODEL`]).
    pub model: String,
    /// Filesystem path to the weights (`EMBEDDING_MODEL_PATH`), forwarded to the
    /// child as `BUYWISE_EMBED_MODEL_PATH`. Optional: a command that resolves
    /// its own weights (a cache, a default) needs no path.
    pub model_path: Option<String>,
    /// Expected vector dimension (`EMBEDDING_DIM`, default [`DEFAULT_DIM`]).
    pub dim: usize,
    /// Wall-clock budget per invocation (`EMBEDDING_TIMEOUT_SECS`, default
    /// [`DEFAULT_TIMEOUT_SECS`]).
    pub timeout: Duration,
}

impl EmbeddingConfig {
    /// Read the configuration from the process environment.
    pub fn from_env() -> Self {
        let enabled = std::env::var("EMBEDDING_ENABLED")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let command = non_empty_env("EMBEDDING_COMMAND");
        let model = non_empty_env("EMBEDDING_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let model_path = non_empty_env("EMBEDDING_MODEL_PATH");
        let dim = std::env::var("EMBEDDING_DIM")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|d| *d > 0)
            .unwrap_or(DEFAULT_DIM);
        let timeout = std::env::var("EMBEDDING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        EmbeddingConfig {
            enabled,
            command,
            model,
            model_path,
            dim,
            timeout,
        }
    }

    /// Whether the command can be run: the feature is on AND a command is set.
    fn is_operational(&self) -> bool {
        self.enabled
            && self
                .command
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
    }
}

/// Read an env var, treating empty/whitespace-only as absent.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The JSON request written to the child command's stdin.
#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    /// `document` or `query`.
    kind: &'a str,
    /// Model id the child should use.
    model: &'a str,
    /// Optional weights path (`null` when unset).
    model_path: Option<&'a str>,
    /// Expected dimension, as a hint.
    dim: usize,
    /// The texts to embed, in order.
    texts: &'a [String],
}

/// The JSON response the child command must print on stdout.
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    /// Dimension of every returned vector. Optional: when absent the first
    /// vector's length is used.
    #[serde(default)]
    dim: Option<usize>,
    /// One vector per requested text, in order.
    vectors: Vec<Vec<f32>>,
}

/// Production [`Embedder`] backed by a configurable external command.
///
/// The command is run once per [`Embedder::embed`] call: the request is written
/// to its stdin as JSON, and it must print a JSON response (`{"vectors": [...]}`)
/// on stdout. A non-zero exit, a timeout, unparseable stdout or a shape mismatch
/// is an error — the caller never guesses.
pub struct CommandEmbedder {
    config: EmbeddingConfig,
}

impl CommandEmbedder {
    /// Build a command embedder from `config`.
    pub fn new(config: EmbeddingConfig) -> Self {
        CommandEmbedder { config }
    }
}

#[async_trait]
impl Embedder for CommandEmbedder {
    fn is_enabled(&self) -> bool {
        self.config.is_operational()
    }

    fn model_id(&self) -> String {
        self.config.model.clone()
    }

    fn dim(&self) -> usize {
        self.config.dim
    }

    async fn embed(&self, kind: EmbedKind, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if !self.is_enabled() {
            return Err(EmbedError::Disabled);
        }
        // Nothing to embed: skip the process entirely.
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let command = self.config.command.as_deref().ok_or(EmbedError::Disabled)?;
        let mut parts = command.split_whitespace();
        let program = parts.next().ok_or(EmbedError::Disabled)?;
        let args: Vec<&str> = parts.collect();

        let request = EmbedRequest {
            kind: kind.as_str(),
            model: &self.config.model,
            model_path: self.config.model_path.as_deref(),
            dim: self.config.dim,
            texts,
        };
        let payload =
            serde_json::to_vec(&request).map_err(|e| EmbedError::Malformed(e.to_string()))?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A killed/timed-out embed must not leave an orphaned child behind.
            .kill_on_drop(true);
        if let Some(path) = self.config.model_path.as_deref() {
            cmd.env("BUYWISE_EMBED_MODEL_PATH", path);
        }

        let mut child = cmd.spawn().map_err(|e| EmbedError::Spawn(e.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| EmbedError::Spawn("child stdin unavailable".to_string()))?;

        // Write stdin on its own task: a payload larger than the pipe buffer must
        // not deadlock against a child that is (also) filling its stdout pipe.
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&payload).await;
            let _ = stdin.shutdown().await;
        });

        let output = match tokio::time::timeout(self.config.timeout, child.wait_with_output()).await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(EmbedError::Spawn(e.to_string())),
            Err(_) => return Err(EmbedError::Timeout(self.config.timeout.as_secs())),
        };
        // The writer finishes once the child has consumed stdin (or the pipe
        // broke); awaiting it here keeps the task from leaking past this call.
        let _ = writer.await;

        if !output.status.success() {
            return Err(EmbedError::Status(output.status.code().unwrap_or(-1)));
        }

        let response: EmbedResponse = serde_json::from_slice(&output.stdout)
            .map_err(|e| EmbedError::Malformed(e.to_string()))?;
        let got_dim = response
            .dim
            .unwrap_or_else(|| response.vectors.first().map(|v| v.len()).unwrap_or(0));
        let invalid_dim = response
            .vectors
            .iter()
            .find(|vector| vector.len() != self.config.dim)
            .map(Vec::len);
        if response.vectors.len() != texts.len()
            || got_dim != self.config.dim
            || invalid_dim.is_some()
        {
            return Err(EmbedError::Shape {
                want: texts.len(),
                want_dim: self.config.dim,
                got: response.vectors.len(),
                got_dim: invalid_dim.unwrap_or(got_dim),
            });
        }
        Ok(response.vectors)
    }
}

/// Encode a vector as little-endian `f32` bytes.
pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for component in vector {
        bytes.extend_from_slice(&component.to_le_bytes());
    }
    bytes
}

/// Decode a little-endian `f32` vector, or `None` when the byte length is not a
/// multiple of four.
pub fn decode_vector(bytes: &[u8]) -> Option<Vec<f32>> {
    let (chunks, remainder) = bytes.as_chunks::<4>();
    if !remainder.is_empty() {
        return None;
    }
    Some(
        chunks
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
    )
}

/// One ranked candidate: a canonical item and its cosine score.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    /// `canonical_items.id`.
    pub canonical_item_id: String,
    /// Cosine similarity of the query against the item's best catalogue string.
    pub score: f32,
}

/// Cosine similarity of two vectors, or `None` when their dimensions differ or
/// either is a zero vector (no direction to compare).
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// Rank `catalogue` (item id, vector) against `query`, returning the best `k`
/// items by cosine similarity.
///
/// An item contributes its **best** string (max cosine), so a name hit and an
/// alias hit for the same item collapse to one candidate. Items whose vector has
/// a different dimension than the query, or a zero vector, are skipped. The order
/// is deterministic: score descending, then canonical-item id ascending.
pub fn rank_candidates(
    query: &[f32],
    catalogue: &[(String, Vec<f32>)],
    k: usize,
) -> Vec<Candidate> {
    let mut best: BTreeMap<&str, f32> = BTreeMap::new();
    for (item, vector) in catalogue {
        let Some(score) = cosine(query, vector) else {
            continue;
        };
        best.entry(item.as_str())
            .and_modify(|current| {
                if score > *current {
                    *current = score;
                }
            })
            .or_insert(score);
    }
    let mut ranked: Vec<Candidate> = best
        .into_iter()
        .map(|(canonical_item_id, score)| Candidate {
            canonical_item_id: canonical_item_id.to_string(),
            score,
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.canonical_item_id.cmp(&b.canonical_item_id))
    });
    ranked.truncate(k);
    ranked
}

/// How many strings a batch step considered versus embedded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmbedCounts {
    /// Distinct strings considered for the configured model.
    pub total: usize,
    /// Vectors actually written; `total - embedded` were already present.
    pub embedded: usize,
}

/// One canonical item's display names, for labelling a suggestion.
#[derive(Debug, Clone, Serialize)]
pub struct SuggestionCandidate {
    /// `canonical_items.id`.
    pub canonical_item_id: String,
    /// `canonical_items.name_en`.
    pub name_en: String,
    /// `canonical_items.name_es`.
    pub name_es: String,
    /// Cosine score of the query against this item's best catalogue string.
    pub score: f32,
}

/// One unmatched imported line with its ranked candidates.
#[derive(Debug, Clone, Serialize)]
pub struct LineSuggestion {
    /// `purchase_lines.id`.
    pub line_id: String,
    /// `purchase_transactions.id` the line belongs to.
    pub transaction_id: String,
    /// Purchase date (`YYYY-MM-DD`).
    pub purchase_date: String,
    /// The line's name, as printed by the retailer.
    pub name: String,
    /// Quantity bought.
    pub quantity: Option<f64>,
    /// Line total in integer cents.
    pub line_total_cents: i64,
    /// Top candidates, best first. Empty when the line has no vector yet.
    pub candidates: Vec<SuggestionCandidate>,
}

/// How much of the unmatched-line queue the suggestion flow covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Coverage {
    /// Unmatched imported lines (with a name) in the household.
    pub unmatched_lines: usize,
    /// Those whose name has a stored vector, so a shortlist can be produced.
    pub covered_lines: usize,
    /// Distinct unmatched line names.
    pub distinct_names: usize,
    /// Those with a stored vector.
    pub covered_names: usize,
}

/// Outcome of a confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfirmOutcome {
    /// The canonical item the caller chose.
    pub canonical_item_id: String,
    /// How many previously unmatched lines were bound to it.
    pub bound_lines: usize,
}

/// Why a confirmation could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum ConfirmError {
    /// No such line belongs to the household.
    #[error("no such purchase line")]
    NoSuchLine,
    /// The named canonical item is not in the household's catalogue.
    #[error("no such canonical item")]
    NoSuchItem,
    /// The line has no name to learn an alias for.
    #[error("the purchase line has no name")]
    UnnamedLine,
    /// The selected line is stale or its alias already belongs to another item.
    #[error("the line or alias conflicts with the current clarification state")]
    Conflict,
    /// A database read or write failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for ConfirmError {
    fn from(error: sqlx::Error) -> Self {
        ConfirmError::Db(DbError::from(error))
    }
}

/// Embed every catalogue string (names and aliases) that has no vector yet for
/// the configured model, and store the vectors.
///
/// Idempotent: a string already embedded for this model is skipped, so a
/// re-run after a catalogue change only embeds the changed strings. Returns the
/// number of strings embedded.
pub async fn embed_catalogue(
    pool: &SqlitePool,
    household_id: &str,
    embedder: &dyn Embedder,
) -> Result<EmbedCounts, EmbedError> {
    if !embedder.is_enabled() {
        return Err(EmbedError::Disabled);
    }
    let model = embedder.model_id();
    // The same (item, text) pair can appear twice (a name that is also an alias,
    // or one alias seeded under both language buckets), and the unique index
    // collapses those rows; dedup first so the batch embeds each string once and
    // the reported count matches the rows written.
    let mut strings = catalogue_strings(pool, household_id).await?;
    strings.sort();
    strings.dedup();
    let total = strings.len();
    let existing = existing_catalogue_texts(pool, household_id, &model).await?;
    let pending: Vec<(String, String)> = strings
        .into_iter()
        .filter(|(item, text)| !existing.contains(&(item.clone(), text.clone())))
        .collect();
    if pending.is_empty() {
        return Ok(EmbedCounts { total, embedded: 0 });
    }

    let texts: Vec<String> = pending.iter().map(|(_, text)| text.clone()).collect();
    let vectors = embedder.embed(EmbedKind::Document, &texts).await?;
    validate_shape(texts.len(), embedder.dim(), &vectors)?;

    let mut tx = pool.begin().await?;
    for ((item, text), vector) in pending.iter().zip(vectors.iter()) {
        insert_catalogue_vector(&mut tx, household_id, item, text, &model, vector).await?;
    }
    tx.commit().await?;

    Ok(EmbedCounts {
        total,
        embedded: pending.len(),
    })
}

/// Embed every distinct unmatched imported line name that has no vector yet for
/// the configured model, and store the vectors.
///
/// Idempotent in the same way as [`embed_catalogue`]. This is the on-demand path
/// for names that appear after an earlier batch: re-running embeds only the new
/// names.
pub async fn embed_unmatched_names(
    pool: &SqlitePool,
    household_id: &str,
    embedder: &dyn Embedder,
) -> Result<EmbedCounts, EmbedError> {
    if !embedder.is_enabled() {
        return Err(EmbedError::Disabled);
    }
    let model = embedder.model_id();
    let names = unmatched_names(pool, household_id).await?;
    let total = names.len();
    let existing = existing_line_names(pool, household_id, &model).await?;
    let pending: Vec<String> = names
        .into_iter()
        .filter(|name| !existing.contains(name))
        .collect();
    if pending.is_empty() {
        return Ok(EmbedCounts { total, embedded: 0 });
    }

    let vectors = embedder.embed(EmbedKind::Query, &pending).await?;
    validate_shape(pending.len(), embedder.dim(), &vectors)?;

    let mut tx = pool.begin().await?;
    for (name, vector) in pending.iter().zip(vectors.iter()) {
        insert_line_vector(&mut tx, household_id, name, &model, vector).await?;
    }
    tx.commit().await?;

    Ok(EmbedCounts {
        total,
        embedded: pending.len(),
    })
}

/// List the household's unmatched imported lines with their top-`k` candidates.
///
/// The catalogue vectors and the line vectors are loaded once and ranked in
/// memory, so the whole page costs a fixed number of queries. A line with no
/// stored vector yet (the batch step has not run for its name) carries an empty
/// candidate list — the caller shows it as "no suggestions yet", never as a
/// match.
pub async fn unmatched_suggestions(
    pool: &SqlitePool,
    household_id: &str,
    model: &str,
    k: usize,
    limit: i64,
    offset: i64,
) -> Result<Vec<LineSuggestion>, DbError> {
    let rows = sqlx::query(
        "SELECT pl.id, pl.transaction_id, pt.purchase_date, pl.name, pl.quantity, \
                pl.line_total_cents \
         FROM purchase_lines pl \
         JOIN purchase_transactions pt ON pt.id = pl.transaction_id \
         WHERE pt.household_id = ?1 AND pl.match_status = 'unmatched' \
               AND NULLIF(trim(pl.name), '') IS NOT NULL \
         ORDER BY pt.purchase_date DESC, pl.id ASC \
         LIMIT ?2 OFFSET ?3",
    )
    .bind(household_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let page_names: std::collections::HashSet<String> = rows
        .iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
    let catalogue = load_catalogue_vectors(pool, household_id, model).await?;
    let names = load_line_vectors(pool, household_id, model, &page_names).await?;
    let labels = load_item_labels(pool, household_id).await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row.get("name");
        let candidates = match names.get(&name) {
            Some(query) if !catalogue.is_empty() => rank_candidates(query, &catalogue, k)
                .into_iter()
                .map(|candidate| {
                    let (name_en, name_es) = labels
                        .get(&candidate.canonical_item_id)
                        .cloned()
                        .unwrap_or_default();
                    SuggestionCandidate {
                        canonical_item_id: candidate.canonical_item_id,
                        name_en,
                        name_es,
                        score: candidate.score,
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        out.push(LineSuggestion {
            line_id: row.get("id"),
            transaction_id: row.get("transaction_id"),
            purchase_date: row.get("purchase_date"),
            name,
            quantity: row.get("quantity"),
            line_total_cents: row.get("line_total_cents"),
            candidates,
        });
    }
    Ok(out)
}

/// Confirm one suggested mapping: learn the line name as an `accepted_match`
/// alias and bind every unmatched line of the household that shares the name.
///
/// This is the ONLY write path that links an imported line to a canonical item
/// outside the deterministic import, and it runs only when a caller names both
/// the line and the item. The alias write goes through
/// [`matching::commit_alias`], the single alias path shared with list-entry
/// clarification, so the mapping resolves via step 1 on the next import.
pub async fn confirm_suggestion(
    pool: &SqlitePool,
    household_id: &str,
    line_id: &str,
    canonical_item_id: &str,
) -> Result<ConfirmOutcome, ConfirmError> {
    let mut tx = pool.begin().await?;

    let row = sqlx::query(
        "SELECT pl.name, pl.match_status FROM purchase_lines pl \
         JOIN purchase_transactions pt ON pt.id = pl.transaction_id \
         WHERE pl.id = ?1 AND pt.household_id = ?2",
    )
    .bind(line_id)
    .bind(household_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(ConfirmError::NoSuchLine);
    };
    let name: Option<String> = row.get("name");
    let match_status: String = row.get("match_status");
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
        return Err(ConfirmError::UnnamedLine);
    };
    if match_status != "unmatched" {
        return Err(ConfirmError::Conflict);
    }

    let item_exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM canonical_items WHERE id = ?1 AND household_id = ?2")
            .bind(canonical_item_id)
            .bind(household_id)
            .fetch_optional(&mut *tx)
            .await?;
    if item_exists.is_none() {
        return Err(ConfirmError::NoSuchItem);
    }

    let normalised_name = crate::normalise::normalize(&name);
    let alias_conflict: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM item_aliases \
         WHERE household_id = ?1 AND lower(alias) = ?2 \
               AND canonical_item_id <> ?3 LIMIT 1",
    )
    .bind(household_id)
    .bind(&normalised_name)
    .bind(canonical_item_id)
    .fetch_optional(&mut *tx)
    .await?;
    if alias_conflict.is_some() {
        return Err(ConfirmError::Conflict);
    }

    matching::commit_alias(
        &mut tx,
        household_id,
        canonical_item_id,
        &name,
        AliasSource::AcceptedMatch,
    )
    .await?;

    let bound = sqlx::query(
        "UPDATE purchase_lines SET canonical_item_id = ?1, match_status = 'matched' \
         WHERE match_status = 'unmatched' AND name = ?2 AND transaction_id IN ( \
             SELECT id FROM purchase_transactions WHERE household_id = ?3 \
         )",
    )
    .bind(canonical_item_id)
    .bind(&name)
    .bind(household_id)
    .execute(&mut *tx)
    .await?
    .rows_affected() as usize;

    tx.commit().await?;
    Ok(ConfirmOutcome {
        canonical_item_id: canonical_item_id.to_string(),
        bound_lines: bound,
    })
}

/// Count how many unmatched imported lines (and distinct names) the suggestion
/// flow covers for `model`, i.e. how many have a stored vector.
pub async fn coverage(
    pool: &SqlitePool,
    household_id: &str,
    model: &str,
) -> Result<Coverage, DbError> {
    let row = sqlx::query(
        "SELECT \
             COUNT(*) AS unmatched_lines, \
             COALESCE(SUM(CASE WHEN e.name IS NOT NULL THEN 1 ELSE 0 END), 0) AS covered_lines, \
             COUNT(DISTINCT pl.name) AS distinct_names, \
             COUNT(DISTINCT CASE WHEN e.name IS NOT NULL THEN pl.name END) AS covered_names \
         FROM purchase_lines pl \
         JOIN purchase_transactions pt ON pt.id = pl.transaction_id \
         LEFT JOIN line_name_embeddings e \
             ON e.household_id = pt.household_id AND e.name = pl.name AND e.model = ?2 \
         WHERE pt.household_id = ?1 AND pl.match_status = 'unmatched' \
               AND NULLIF(trim(pl.name), '') IS NOT NULL",
    )
    .bind(household_id)
    .bind(model)
    .fetch_one(pool)
    .await?;
    Ok(Coverage {
        unmatched_lines: row.get::<i64, _>("unmatched_lines") as usize,
        covered_lines: row.get::<i64, _>("covered_lines") as usize,
        distinct_names: row.get::<i64, _>("distinct_names") as usize,
        covered_names: row.get::<i64, _>("covered_names") as usize,
    })
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

/// Every catalogue string for the household: both canonical names and every
/// alias, each tagged with its canonical item.
async fn catalogue_strings(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<Vec<(String, String)>, DbError> {
    let rows = sqlx::query(
        "SELECT id AS canonical_item_id, name_en AS text FROM canonical_items WHERE household_id = ?1 \
         UNION ALL \
         SELECT id AS canonical_item_id, name_es AS text FROM canonical_items WHERE household_id = ?1 \
         UNION ALL \
         SELECT canonical_item_id, alias AS text FROM item_aliases WHERE household_id = ?1",
    )
    .bind(household_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("canonical_item_id"), row.get("text")))
        .collect())
}

/// The `(item, text)` catalogue pairs already embedded for `model`.
async fn existing_catalogue_texts(
    pool: &SqlitePool,
    household_id: &str,
    model: &str,
) -> Result<std::collections::HashSet<(String, String)>, DbError> {
    let rows = sqlx::query(
        "SELECT canonical_item_id, text FROM catalogue_embeddings \
         WHERE household_id = ?1 AND model = ?2",
    )
    .bind(household_id)
    .bind(model)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get("canonical_item_id"), row.get("text")))
        .collect())
}

/// The distinct unmatched imported line names for the household.
async fn unmatched_names(pool: &SqlitePool, household_id: &str) -> Result<Vec<String>, DbError> {
    let rows = sqlx::query(
        "SELECT DISTINCT pl.name FROM purchase_lines pl \
         JOIN purchase_transactions pt ON pt.id = pl.transaction_id \
         WHERE pt.household_id = ?1 AND pl.match_status = 'unmatched' \
               AND NULLIF(trim(pl.name), '') IS NOT NULL \
         ORDER BY pl.name ASC",
    )
    .bind(household_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| row.get("name")).collect())
}

/// The line names already embedded for `model`.
async fn existing_line_names(
    pool: &SqlitePool,
    household_id: &str,
    model: &str,
) -> Result<std::collections::HashSet<String>, DbError> {
    let rows =
        sqlx::query("SELECT name FROM line_name_embeddings WHERE household_id = ?1 AND model = ?2")
            .bind(household_id)
            .bind(model)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|row| row.get("name")).collect())
}

async fn insert_catalogue_vector(
    conn: &mut SqliteConnection,
    household_id: &str,
    canonical_item_id: &str,
    text: &str,
    model: &str,
    vector: &[f32],
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT OR IGNORE INTO catalogue_embeddings \
             (id, household_id, canonical_item_id, text, model, dim, vector, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(household_id)
    .bind(canonical_item_id)
    .bind(text)
    .bind(model)
    .bind(vector.len() as i64)
    .bind(encode_vector(vector))
    .bind(db::now_iso())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn insert_line_vector(
    conn: &mut SqliteConnection,
    household_id: &str,
    name: &str,
    model: &str,
    vector: &[f32],
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT OR IGNORE INTO line_name_embeddings \
             (id, household_id, name, model, dim, vector, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(household_id)
    .bind(name)
    .bind(model)
    .bind(vector.len() as i64)
    .bind(encode_vector(vector))
    .bind(db::now_iso())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The household's catalogue vectors, one entry per embedded string.
async fn load_catalogue_vectors(
    pool: &SqlitePool,
    household_id: &str,
    model: &str,
) -> Result<Vec<(String, Vec<f32>)>, DbError> {
    let rows = sqlx::query(
        "SELECT canonical_item_id, vector FROM catalogue_embeddings \
         WHERE household_id = ?1 AND model = ?2",
    )
    .bind(household_id)
    .bind(model)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let item: String = row.get("canonical_item_id");
            let bytes: Vec<u8> = row.get("vector");
            decode_vector(&bytes).map(|vector| (item, vector))
        })
        .collect())
}

/// The household's line-name vectors, keyed by name.
async fn load_line_vectors(
    pool: &SqlitePool,
    household_id: &str,
    model: &str,
    names: &std::collections::HashSet<String>,
) -> Result<HashMap<String, Vec<f32>>, DbError> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT name, vector FROM line_name_embeddings \
         WHERE household_id = ",
    );
    query
        .push_bind(household_id)
        .push(" AND model = ")
        .push_bind(model);
    query.push(" AND name IN (");
    let mut separated = query.separated(", ");
    for name in names {
        separated.push_bind(name);
    }
    separated.push_unseparated(")");
    let rows = query.build().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let name: String = row.get("name");
            let bytes: Vec<u8> = row.get("vector");
            decode_vector(&bytes).map(|vector| (name, vector))
        })
        .collect())
}

/// `(name_en, name_es)` for every canonical item, keyed by id.
async fn load_item_labels(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<HashMap<String, (String, String)>, DbError> {
    let rows =
        sqlx::query("SELECT id, name_en, name_es FROM canonical_items WHERE household_id = ?1")
            .bind(household_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("id"),
                (
                    row.get::<String, _>("name_en"),
                    row.get::<String, _>("name_es"),
                ),
            )
        })
        .collect())
}

/// Refuse a batch whose returned shape does not match the request.
fn validate_shape(count: usize, dim: usize, vectors: &[Vec<f32>]) -> Result<(), EmbedError> {
    let got_dim = vectors
        .iter()
        .find(|vector| vector.len() != dim)
        .map(Vec::len)
        .unwrap_or_else(|| vectors.first().map(|v| v.len()).unwrap_or(dim));
    if vectors.len() != count || vectors.iter().any(|vector| vector.len() != dim) {
        return Err(EmbedError::Shape {
            want: count,
            want_dim: dim,
            got: vectors.len(),
            got_dim,
        });
    }
    Ok(())
}
