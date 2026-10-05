//! LLM client abstraction for the resolution cascade's step-3 fallback (P3-T2,
//! issue #26).
//!
//! When the Phase-0 cascade ([`buywise_core::matching::resolve`]) returns
//! [`buywise_core::matching::ResolutionOutcome::NoMatch`] for a raw term, the
//! resolution task ([`crate::resolution`]) asks an LLM which household canonical
//! item the term most likely refers to. The LLM is a *cheap, fast* model reached
//! over an OpenAI-compatible chat-completions endpoint; the model name and
//! credentials come from the environment so they are never baked into the binary.
//!
//! ## Design
//!
//! The seam is the [`LlmClient`] trait: one async method that, given a raw term
//! and the household's candidate `name_en` list, returns the chosen item name (or
//! `None` for "no plausible match"). Production uses [`ReqwestLlmClient`]; unit
//! tests inject a fake so they never touch the network. [`crate::AppState`] holds
//! an `Arc<dyn LlmClient>`, populated from [`LlmConfig::from_env`] at startup.
//!
//! ## Gating and cost
//!
//! The fallback is OFF unless `LLM_FALLBACK_ENABLED=true` (default `false`, so dev
//! and tests never spend tokens by accident). When enabled, every outbound call is
//! recorded in `api_usage_events` with `source='llm'` for cost tracking — that
//! bookkeeping lives in [`crate::resolution`] alongside the rest of the cascade's
//! DB writes, so the client stays a pure transport.
//!
//! ## Error discipline
//!
//! Any failure — disabled, missing config, HTTP/transport error, malformed
//! response — surfaces as an [`LlmError`] (or, for the "no match" case, `Ok(None)`).
//! The caller logs it and leaves the entry `unresolved`; it never retries inline
//! and never panics (the issue's "no retry storm" requirement).

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

/// How long to wait on the LLM HTTP call before giving up. A fallback that blocks
/// the resolution task indefinitely is worse than leaving the entry `unresolved`,
/// so the timeout is deliberately short.
const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The sentinel the prompt asks the model to return when nothing matches. Compared
/// case-insensitively after trimming, so `"none"`, `"None"`, `"NONE."` all count.
const NONE_SENTINEL: &str = "none";

/// Configuration for the LLM fallback, read from the environment.
///
/// All four variables are read by [`from_env`](LlmConfig::from_env); only
/// `LLM_FALLBACK_ENABLED` has a default (`false`). A config with
/// `enabled == false`, or with any of `provider` / `api_key` / `model` missing,
/// builds a client that short-circuits to [`LlmError::Disabled`] without ever
/// making a request.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Master switch. `LLM_FALLBACK_ENABLED=true` (case-insensitive) turns the
    /// fallback on; anything else (including unset) leaves it off.
    pub enabled: bool,
    /// Provider identifier (`LLM_PROVIDER`). Currently unused at runtime: every
    /// provider is assumed OpenAI-chat-completions-compatible, so the default base
    /// URL ignores it (see [`default_base_url`]). Kept on purpose so per-provider
    /// defaults can be added later without touching call sites or the env contract.
    pub provider: Option<String>,
    /// API key (`LLM_API_KEY`) sent as a bearer token.
    pub api_key: Option<String>,
    /// Model name (`LLM_MODEL`). NEVER hardcoded — providers rotate cheap models
    /// frequently, so the operator picks one at deploy time.
    pub model: Option<String>,
    /// Base URL of the chat-completions endpoint. Defaults per provider; overridable
    /// via `LLM_BASE_URL` for self-hosted / proxy deployments.
    pub base_url: String,
}

impl LlmConfig {
    /// Read the LLM configuration from the process environment.
    ///
    /// `LLM_FALLBACK_ENABLED` defaults to `false`. The remaining variables are
    /// optional here (a missing one simply yields a disabled client at call time),
    /// so a partially-configured environment never panics at startup.
    pub fn from_env() -> Self {
        let enabled = std::env::var("LLM_FALLBACK_ENABLED")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let provider = non_empty_env("LLM_PROVIDER");
        let api_key = non_empty_env("LLM_API_KEY");
        let model = non_empty_env("LLM_MODEL");
        let base_url =
            non_empty_env("LLM_BASE_URL").unwrap_or_else(|| default_base_url(provider.as_deref()));
        LlmConfig {
            enabled,
            provider,
            api_key,
            model,
            base_url,
        }
    }

    /// Whether a real request can be made: the fallback is on AND every required
    /// credential is present. A `false` here means the client returns
    /// [`LlmError::Disabled`] before touching the network.
    fn is_operational(&self) -> bool {
        self.enabled && self.api_key.is_some() && self.model.is_some()
    }
}

/// Read an env var, treating empty/whitespace-only as absent.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The default chat-completions URL when `LLM_BASE_URL` is unset.
///
/// Every provider is assumed OpenAI-chat-completions-compatible, so the default
/// is OpenAI's endpoint regardless of `provider`; operators of non-OpenAI hosts
/// set `LLM_BASE_URL` explicitly. `provider` is accepted (not used) so the default
/// can grow per-provider URLs later without touching call sites.
fn default_base_url(_provider: Option<&str>) -> String {
    "https://api.openai.com/v1/chat/completions".to_string()
}

/// Errors the LLM fallback can produce. All are non-fatal: the caller logs and
/// leaves the entry `unresolved`.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// The fallback is disabled or not fully configured — no request was made.
    #[error("LLM fallback disabled or not configured")]
    Disabled,
    /// The HTTP request failed to send or returned a transport-level error.
    #[error("LLM request failed: {0}")]
    Request(String),
    /// The endpoint returned a non-success HTTP status.
    #[error("LLM endpoint returned status {0}")]
    Status(u16),
    /// The response body could not be parsed into the expected shape.
    #[error("LLM response malformed: {0}")]
    Malformed(String),
}

/// The pluggable LLM transport. One method: given the raw term and the
/// household's candidate item names, return the chosen name, `None` for "no
/// plausible match", or an [`LlmError`].
///
/// `dyn`-compatible via [`async_trait`] so [`crate::AppState`] can hold an
/// `Arc<dyn LlmClient>` and tests can swap in a fake.
#[allow(
    clippy::double_must_use,
    reason = "async_trait generates a must_use future for async trait methods"
)]
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Whether this client can actually make a call — i.e. the fallback is enabled
    /// AND fully configured. A cheap, synchronous, side-effect-free check the caller
    /// uses to short-circuit BEFORE doing any work (e.g. loading candidates / a DB
    /// round-trip) when the fallback is off (the production default). When this
    /// returns `false`, [`resolve_item`](Self::resolve_item) is guaranteed to return
    /// [`LlmError::Disabled`].
    fn is_enabled(&self) -> bool;

    /// Ask the model which of `candidate_names` `raw_text` most likely refers to.
    ///
    /// Returns `Ok(Some(name))` for a chosen candidate, `Ok(None)` for the "none"
    /// sentinel, or `Err` on any failure. Implementations MUST NOT retry on
    /// failure (the cascade's no-retry-storm contract).
    async fn resolve_item(
        &self,
        raw_text: &str,
        candidate_names: &[String],
    ) -> Result<Option<String>, LlmError>;

    /// Optionally reorder candidate retailer products for one canonical item
    /// (issue #108).
    ///
    /// Given a human description of the item and the exact candidate labels
    /// (product name + retailer), return those labels best-first. This is
    /// advisory only: the caller keeps its deterministic order for any candidate
    /// the model omits, and falls back entirely on `Ok(None)` or an error, so a
    /// bad answer can never lose a candidate or invent one. The default
    /// implementation opts out (`Ok(None)`); [`ReqwestLlmClient`] implements it.
    /// Callers gate on [`is_enabled`](Self::is_enabled) before calling, so an
    /// off fallback costs nothing.
    async fn rank_candidates(
        &self,
        _item_description: &str,
        _candidate_labels: &[String],
    ) -> Result<Option<Vec<String>>, LlmError> {
        Ok(None)
    }
}

/// Production [`LlmClient`] backed by `reqwest`, targeting an OpenAI-compatible
/// `/chat/completions` endpoint.
pub struct ReqwestLlmClient {
    config: LlmConfig,
    /// The HTTP client carrying [`LLM_REQUEST_TIMEOUT`]. `None` when the builder
    /// failed: we fail CLOSED (disable the fallback) rather than fall back to a
    /// default client WITHOUT the configured timeout, which could hang calls.
    http: Option<reqwest::Client>,
}

impl ReqwestLlmClient {
    /// Build a client from `config`. The underlying `reqwest::Client` carries the
    /// short [`LLM_REQUEST_TIMEOUT`]; if it cannot be constructed we fail CLOSED —
    /// the fallback is disabled (logged once) so every call short-circuits to
    /// [`LlmError::Disabled`] rather than running with an un-timed-out client.
    pub fn new(config: LlmConfig) -> Self {
        let http = match reqwest::Client::builder()
            .timeout(LLM_REQUEST_TIMEOUT)
            .build()
        {
            Ok(client) => Some(client),
            Err(e) => {
                // Fail closed: a client without the timeout could hang the
                // resolution task indefinitely. Log once at startup; every call
                // then returns Disabled.
                tracing::warn!(
                    error = %e,
                    "llm: reqwest client build failed; disabling the LLM fallback"
                );
                None
            }
        };
        ReqwestLlmClient { config, http }
    }

    /// Build the user prompt: the raw term plus the candidate list, asking for a
    /// single item name or `none`. The instruction matches `docs/implementation-plan.md`
    /// P3-T2 verbatim so the contract is auditable.
    fn build_prompt(raw_text: &str, candidate_names: &[String]) -> String {
        let list = candidate_names
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "A user added \"{raw_text}\" to their grocery list. Here are the items in \
             their household catalogue:\n{list}\n\nWhich of these grocery items is this \
             most likely referring to? Return only the exact item name from the list, or \
             'none' if there is no plausible match."
        )
    }

    /// Build the ranking prompt: the item being shopped for and a numbered list of
    /// candidate products (label + retailer), asking for those labels best-first.
    fn build_ranking_prompt(item_description: &str, candidate_labels: &[String]) -> String {
        let list = candidate_labels
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "A shopper wants to buy \"{item_description}\". Here are candidate products \
             found in their stores:\n{list}\n\nRank the candidates by how likely each is \
             the product the shopper means. Return a JSON array of the exact candidate \
             strings (as written after \"- \"), best first. Include only plausible \
             candidates; omit any that do not match."
        )
    }
}

/// Minimal subset of the OpenAI chat-completions response we read: the first
/// choice's message content.
#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

#[allow(
    clippy::double_must_use,
    reason = "async_trait generates a must_use future for async trait methods"
)]
#[async_trait]
impl LlmClient for ReqwestLlmClient {
    fn is_enabled(&self) -> bool {
        // Operational config AND a successfully-built HTTP client (we fail closed
        // when the builder failed, see [`ReqwestLlmClient::new`]).
        self.config.is_operational() && self.http.is_some()
    }

    async fn resolve_item(
        &self,
        raw_text: &str,
        candidate_names: &[String],
    ) -> Result<Option<String>, LlmError> {
        if !self.is_enabled() {
            return Err(LlmError::Disabled);
        }
        // No candidates → nothing the model could pick; skip the call entirely.
        if candidate_names.is_empty() {
            return Ok(None);
        }
        let http = self.http.as_ref().ok_or(LlmError::Disabled)?;
        let api_key = self.config.api_key.as_deref().ok_or(LlmError::Disabled)?;
        let model = self.config.model.as_deref().ok_or(LlmError::Disabled)?;

        let prompt = Self::build_prompt(raw_text, candidate_names);
        // OpenAI-compatible chat-completions request. `temperature: 0` keeps the
        // choice deterministic; a tiny `max_tokens` bounds cost (we only need a name).
        let body = serde_json::json!({
            "model": model,
            "temperature": 0,
            "max_tokens": 32,
            "messages": [
                { "role": "system", "content": "You map free-text grocery entries to a fixed catalogue. Answer with exactly one catalogue item name, or 'none'." },
                { "role": "user", "content": prompt }
            ]
        });

        let resp = http
            .post(&self.config.base_url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Request(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LlmError::Status(resp.status().as_u16()));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Malformed(e.to_string()))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| LlmError::Malformed("no choices in response".to_string()))?;

        Ok(interpret_answer(&content))
    }

    async fn rank_candidates(
        &self,
        item_description: &str,
        candidate_labels: &[String],
    ) -> Result<Option<Vec<String>>, LlmError> {
        if !self.is_enabled() {
            return Err(LlmError::Disabled);
        }
        // Nothing to order → no call.
        if candidate_labels.is_empty() {
            return Ok(None);
        }
        let http = self.http.as_ref().ok_or(LlmError::Disabled)?;
        let api_key = self.config.api_key.as_deref().ok_or(LlmError::Disabled)?;
        let model = self.config.model.as_deref().ok_or(LlmError::Disabled)?;

        let prompt = Self::build_ranking_prompt(item_description, candidate_labels);
        // `temperature: 0` keeps the ordering deterministic; the token budget is
        // larger than resolve_item's because the array can hold several labels.
        let body = serde_json::json!({
            "model": model,
            "temperature": 0,
            "max_tokens": 512,
            "messages": [
                { "role": "system", "content": "You rank grocery products by how likely they are the same item a shopper means. Answer with a JSON array of the exact candidate strings, best first." },
                { "role": "user", "content": prompt }
            ]
        });

        let resp = http
            .post(&self.config.base_url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Request(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LlmError::Status(resp.status().as_u16()));
        }

        let parsed: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Malformed(e.to_string()))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| LlmError::Malformed("no choices in response".to_string()))?;

        Ok(interpret_ranking(&content, candidate_labels))
    }
}

/// Interpret the model's raw answer: trim it, treat the `none` sentinel (and an
/// empty answer) as "no match", otherwise return the cleaned string. Validation
/// that the answer actually names a candidate happens in [`crate::resolution`]
/// (exact-match against the canonical list), so a hallucinated name simply falls
/// through to user clarification rather than binding a wrong item.
fn interpret_answer(content: &str) -> Option<String> {
    // Strip surrounding quotes/whitespace and a trailing period the model may add.
    let cleaned = content
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.')
        .trim();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case(NONE_SENTINEL) {
        return None;
    }
    Some(cleaned.to_string())
}

/// Interpret the model's ranking answer: extract a JSON array of candidate
/// labels, keep only labels that were actually offered (exact match), drop
/// duplicates, and preserve the model's order. Anything unparseable or naming no
/// known candidate yields `None`, so the caller keeps its deterministic order.
/// This is the safety boundary: the model can reorder the candidates it was
/// given, but can never add one.
fn interpret_ranking(content: &str, candidate_labels: &[String]) -> Option<Vec<String>> {
    let start = content.find('[')?;
    let end = content.rfind(']')?;
    if end < start {
        return None;
    }
    let parsed: Vec<String> = serde_json::from_str(&content[start..=end]).ok()?;

    let mut ordered: Vec<String> = Vec::new();
    for label in parsed {
        if candidate_labels.iter().any(|c| c == &label) && !ordered.contains(&label) {
            ordered.push(label);
        }
    }
    if ordered.is_empty() {
        None
    } else {
        Some(ordered)
    }
}

/// Build the application's [`LlmClient`] from the environment. Always returns a
/// usable client: when the fallback is disabled or unconfigured, the client's
/// calls short-circuit to [`LlmError::Disabled`].
pub fn client_from_env() -> std::sync::Arc<dyn LlmClient> {
    std::sync::Arc::new(ReqwestLlmClient::new(LlmConfig::from_env()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a one-shot HTTP/1.1 server that replies to the first connection with
    /// `status`/`body`, then returns its `http://127.0.0.1:<port>/` URL. Lets the
    /// real `ReqwestLlmClient` request path run against a controlled response with
    /// no network dependency (mirrors the ingest crate's `one_shot_server`).
    async fn one_shot_server(status_line: &'static str, body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://{addr}/")
    }

    /// An operational config pointed at `base_url` (a mock server).
    fn operational_config(base_url: String) -> LlmConfig {
        LlmConfig {
            enabled: true,
            provider: Some("openai".to_string()),
            api_key: Some("test-key".to_string()),
            model: Some("test-model".to_string()),
            base_url,
        }
    }

    /// A well-formed chat-completions body whose single choice's content is `answer`.
    fn completion_body(answer: &str) -> String {
        serde_json::json!({
            "choices": [ { "message": { "role": "assistant", "content": answer } } ]
        })
        .to_string()
    }

    #[tokio::test]
    async fn happy_path_returns_chosen_item_name() {
        let url = one_shot_server("200 OK", completion_body("Apple Juice")).await;
        let client = ReqwestLlmClient::new(operational_config(url));
        let answer = client
            .resolve_item(
                "zumo de manzana",
                &["Milk".to_string(), "Apple Juice".to_string()],
            )
            .await
            .expect("operational call succeeds");
        assert_eq!(answer, Some("Apple Juice".to_string()));
    }

    #[tokio::test]
    async fn none_response_maps_to_no_match() {
        let url = one_shot_server("200 OK", completion_body("none")).await;
        let client = ReqwestLlmClient::new(operational_config(url));
        let answer = client
            .resolve_item("widget", &["Milk".to_string()])
            .await
            .expect("call succeeds");
        assert_eq!(answer, None, "'none' answer maps to Ok(None)");
    }

    #[tokio::test]
    async fn api_error_status_is_an_error() {
        let url = one_shot_server("429 Too Many Requests", "{}".to_string()).await;
        let client = ReqwestLlmClient::new(operational_config(url));
        let err = client
            .resolve_item("zumo", &["Milk".to_string()])
            .await
            .expect_err("a non-2xx status is an error");
        assert!(matches!(err, LlmError::Status(429)), "got {err:?}");
    }

    #[tokio::test]
    async fn malformed_body_is_an_error() {
        let url = one_shot_server("200 OK", "not json at all".to_string()).await;
        let client = ReqwestLlmClient::new(operational_config(url));
        let err = client
            .resolve_item("zumo", &["Milk".to_string()])
            .await
            .expect_err("an unparseable body is an error");
        assert!(matches!(err, LlmError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn interpret_answer_handles_none_and_quotes() {
        assert_eq!(interpret_answer("none"), None);
        assert_eq!(interpret_answer("  None.  "), None);
        assert_eq!(interpret_answer("\"NONE\""), None);
        assert_eq!(interpret_answer(""), None);
        assert_eq!(
            interpret_answer("Apple Juice"),
            Some("Apple Juice".to_string())
        );
        assert_eq!(
            interpret_answer("  \"Apple Juice\".  "),
            Some("Apple Juice".to_string())
        );
    }

    #[test]
    fn interpret_ranking_keeps_only_offered_labels_in_order() {
        let offered = vec![
            "Heinz Baked Beans | tesco".to_string(),
            "Own Baked Beans | aldi".to_string(),
        ];
        // Model returns offered labels out of order, one hallucinated, one dup.
        let content = "Here you go:\n[\"Own Baked Beans | aldi\", \"Ghost Product | lidl\", \
                       \"Heinz Baked Beans | tesco\", \"Own Baked Beans | aldi\"]";
        assert_eq!(
            interpret_ranking(content, &offered),
            Some(vec![
                "Own Baked Beans | aldi".to_string(),
                "Heinz Baked Beans | tesco".to_string(),
            ])
        );
    }

    #[test]
    fn interpret_ranking_rejects_unparseable_or_unknown_only() {
        let offered = vec!["Milk | tesco".to_string()];
        assert_eq!(interpret_ranking("no array here", &offered), None);
        assert_eq!(interpret_ranking("[\"Unknown | lidl\"]", &offered), None);
        assert_eq!(interpret_ranking("[]", &offered), None);
    }

    #[tokio::test]
    async fn default_rank_candidates_opts_out() {
        struct Noop;
        #[async_trait]
        impl LlmClient for Noop {
            fn is_enabled(&self) -> bool {
                true
            }
            async fn resolve_item(
                &self,
                _raw_text: &str,
                _candidate_names: &[String],
            ) -> Result<Option<String>, LlmError> {
                Ok(None)
            }
        }
        assert_eq!(
            Noop.rank_candidates("Milk", &["Milk | tesco".to_string()])
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn disabled_rank_candidates_short_circuits() {
        let client = ReqwestLlmClient::new(LlmConfig {
            enabled: false,
            provider: None,
            api_key: Some("k".to_string()),
            model: Some("m".to_string()),
            base_url: "http://127.0.0.1:1/unused".to_string(),
        });
        let err = client
            .rank_candidates("Milk", &["Milk | tesco".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Disabled));
    }

    #[tokio::test]
    async fn disabled_config_short_circuits_without_network() {
        // Default-built config is disabled (enabled=false), so resolve_item must
        // return Disabled without attempting any HTTP request.
        let client = ReqwestLlmClient::new(LlmConfig {
            enabled: false,
            provider: None,
            api_key: Some("k".to_string()),
            model: Some("m".to_string()),
            base_url: "http://127.0.0.1:1/unused".to_string(),
        });
        let err = client
            .resolve_item("zumo", &["Milk".to_string()])
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Disabled));
    }

    #[tokio::test]
    async fn operational_but_empty_candidates_returns_none() {
        let client = ReqwestLlmClient::new(LlmConfig {
            enabled: true,
            provider: Some("openai".to_string()),
            api_key: Some("k".to_string()),
            model: Some("m".to_string()),
            base_url: "http://127.0.0.1:1/unused".to_string(),
        });
        // Empty candidate list short-circuits to Ok(None) before any network call.
        assert_eq!(client.resolve_item("zumo", &[]).await.unwrap(), None);
    }

    fn build(provider: Option<&str>) -> String {
        default_base_url(provider)
    }

    #[test]
    fn default_base_url_is_openai_compatible() {
        assert!(build(None).contains("chat/completions"));
        assert!(build(Some("openai")).contains("api.openai.com"));
        assert!(build(Some("anything")).contains("chat/completions"));
    }
}
