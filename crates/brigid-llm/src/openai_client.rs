//! OpenAI-compatible LLM provider client.
//!
//! [`OpenAiCompatibleClient`] talks to any OpenAI-compatible chat-completions
//! endpoint (OpenAI, DeepSeek, local servers, …) over HTTP using `reqwest`.
//! It implements [`crate::LlmClient`] with retry/backoff/timeout and optional
//! [`crate::DiskCache`] response caching.
//!
//! Construction via [`OpenAiCompatibleClient::new`] returns a [`Result`];
//! builder errors (e.g. invalid TLS configuration) are propagated as
//! [`LlmError::Network`] rather than silently swallowed.
//!
//! # Data leaves the machine
//!
//! Calling [`OpenAiCompatibleClient::complete`] sends the full prompt text and
//! an `Authorization: Bearer <api_key>` header to the configured `base_url`.
//! Only use providers you trust with the crawled source contents. The API key
//! is read from the environment only and is never logged; error messages
//! redact it to the first four characters.

use crate::cache::{CacheKeyInput, DiskCache};
use crate::{LlmClient, LlmError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

/// Maximum backoff between retries, regardless of the exponential growth.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Read an environment variable, treating blank/whitespace-only values as
/// unset. This mirrors the `nonblank` helper in `brigid-core/src/config.rs`
/// and prevents the historical Make bug where `VAR=""` is accepted as a
/// valid value (see `docs/move-to-rust.md` §4.3).
fn nonblank_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Configuration for an OpenAI-compatible LLM client.
#[derive(Clone, Debug)]
pub struct OpenAiClientConfig {
    /// Base URL (e.g. `https://api.openai.com/v1` or DeepSeek
    /// `https://api.deepseek.com/v1`).
    pub base_url: String,
    /// API key (from env only — never CLI args or config files).
    pub api_key: String,
    /// Model identifier (e.g. `gpt-4o`, `deepseek-chat`, `openai/gpt-4o`).
    pub model: String,
    /// Request timeout.
    pub timeout: Duration,
    /// Max retry attempts for transient errors (429, 5xx, network).
    pub max_retries: u32,
    /// Initial backoff duration (doubles each retry).
    pub initial_backoff: Duration,
    /// Provider name for cache keys (e.g. `openai`, `deepseek`, `openrouter`).
    pub provider_name: String,
    /// Hosts allowed to receive the `Authorization` header. Defaults to
    /// known providers plus loopback for testing. Extend with
    /// [`OpenAiClientConfig::with_allowed_host`].
    pub allowed_hosts: Vec<String>,
    /// Output token cap sent as `max_tokens` in the request body. Without an
    /// explicit cap, providers default to small limits (e.g. DeepSeek ~4096)
    /// and long structured responses are truncated mid-block.
    pub max_tokens: u32,
    /// Optional `HTTP-Referer` header (OpenRouter attribution / rankings).
    pub http_referer: Option<String>,
    /// Optional `X-Title` header (OpenRouter app name for rankings).
    pub app_title: Option<String>,
}

/// Known first-class provider presets (ADR 0017).
///
/// Config files keep `provider` as a free string; this enum is the internal
/// lookup used to pick defaults, cache `provider_name`, and request headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderPreset {
    /// DeepSeek API (`api.deepseek.com`).
    Deepseek,
    /// OpenAI API (`api.openai.com`).
    Openai,
    /// OpenRouter gateway (`openrouter.ai`).
    Openrouter,
    /// Any other provider id or unset — base URL / model come from env.
    Custom {
        /// Cache-key / diagnostics name (may be empty before inference).
        name: String,
    },
}

impl ProviderPreset {
    /// Normalize a free-form provider string into a preset.
    ///
    /// Matching is case-insensitive. Unknown values become
    /// [`ProviderPreset::Custom`] with the lowercased name preserved so
    /// cache keys stay provider-isolated.
    #[must_use]
    pub fn from_name(name: Option<&str>) -> Self {
        match name.map(str::trim).filter(|s| !s.is_empty()) {
            None => Self::Custom {
                name: String::new(),
            },
            Some(raw) => {
                let lower = raw.to_ascii_lowercase();
                match lower.as_str() {
                    "deepseek" => Self::Deepseek,
                    "openai" => Self::Openai,
                    "openrouter" => Self::Openrouter,
                    // Keep the name "custom" non-empty so from_env_with does
                    // not treat an explicit provider = "custom" as "unset" and
                    // overwrite it via the base-URL heuristic.
                    other => Self::Custom {
                        name: other.to_string(),
                    },
                }
            }
        }
    }

    /// Stable provider name for cache keys and diagnostics.
    #[must_use]
    pub fn provider_name(&self) -> &str {
        match self {
            Self::Deepseek => "deepseek",
            Self::Openai => "openai",
            Self::Openrouter => "openrouter",
            Self::Custom { name } if !name.is_empty() => name.as_str(),
            Self::Custom { .. } => "custom",
        }
    }

    /// Whether this preset requires an explicit model (no safe default).
    #[must_use]
    pub fn requires_explicit_model(&self) -> bool {
        match self {
            Self::Openai | Self::Openrouter => true,
            // Named custom providers (including the literal "custom") have no
            // safe default model. The empty-name Custom variant is only used
            // as a temporary "unset" marker before base-URL inference.
            Self::Custom { name } => !name.is_empty(),
            Self::Deepseek => false,
        }
    }

    /// Default base URL for the preset, if any.
    #[must_use]
    pub fn default_base_url(&self) -> Option<&'static str> {
        match self {
            Self::Deepseek => Some("https://api.deepseek.com/v1"),
            Self::Openai => Some("https://api.openai.com/v1"),
            Self::Openrouter => Some("https://openrouter.ai/api/v1"),
            Self::Custom { .. } => None,
        }
    }

    /// Default model for the preset, if a safe default exists.
    #[must_use]
    pub fn default_model(&self) -> Option<&'static str> {
        match self {
            Self::Deepseek => Some("deepseek-chat"),
            Self::Openai | Self::Openrouter | Self::Custom { .. } => None,
        }
    }

    /// Provider-specific API key env var (checked after `BRIGID_LLM_API_KEY`).
    #[must_use]
    pub fn api_key_env(&self) -> Option<&'static str> {
        match self {
            Self::Deepseek => Some("DEEPSEEK_API_KEY"),
            Self::Openai => Some("OPENAI_API_KEY"),
            Self::Openrouter => Some("OPENROUTER_API_KEY"),
            Self::Custom { .. } => None,
        }
    }
}

/// Infer a provider cache name from a base URL when no explicit provider is set.
///
/// Order matters: OpenRouter URLs often contain the substring `openai` in
/// model paths elsewhere, but the host check for `openrouter` must win first.
fn infer_provider_name_from_base_url(base_url: &str) -> &'static str {
    let lower = base_url.to_ascii_lowercase();
    if lower.contains("openrouter") {
        "openrouter"
    } else if lower.contains("openai") {
        "openai"
    } else {
        "deepseek"
    }
}

/// Whether an attribution env value means "disable this header".
///
/// Recognized disable tokens (case-insensitive): `off`, `none`, `0`,
/// `false`, `no`. Any other non-blank value is treated as a custom header
/// value.
fn is_attribution_disabled(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "off" | "none" | "0" | "false" | "no"
    )
}

/// Default output token cap. DeepSeek's default (4096) truncates long
/// structured identify/relationships responses; 8192 is the documented
/// output maximum for `deepseek-chat` and is accepted by OpenAI-compatible
/// providers (they clamp or error per model). Override with
/// `BRIGID_LLM_MAX_TOKENS`.
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Built-in host allowlist: known LLM providers plus loopback for tests.
const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
    "api.openai.com",
    "api.deepseek.com",
    "openrouter.ai",
    "localhost",
    "127.0.0.1",
];

/// Default OpenRouter `HTTP-Referer` when attribution is enabled.
const DEFAULT_OPENROUTER_REFERER: &str = "https://github.com/igmarin/brigid";
/// Default OpenRouter `X-Title` when attribution is enabled.
const DEFAULT_OPENROUTER_TITLE: &str = "brigid";

impl OpenAiClientConfig {
    /// Create a new config with sensible defaults for timeout (120s),
    /// retries (3), and initial backoff (1s).
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            timeout: Duration::from_secs(120),
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            provider_name: "deepseek".to_string(),
            allowed_hosts: DEFAULT_ALLOWED_HOSTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_tokens: DEFAULT_MAX_TOKENS,
            http_referer: None,
            app_title: None,
        }
    }

    /// Build a config from environment variables only.
    ///
    /// Equivalent to [`Self::from_env_with`] with no provider/model overrides.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Provider`] if no API key is present, or if a
    /// provider that requires an explicit model (OpenAI, OpenRouter) has none.
    pub fn from_env() -> Result<Self, LlmError> {
        Self::from_env_with(None, None)
    }

    /// Build a config from environment variables plus optional overrides.
    ///
    /// `provider_override` / `model_override` come from `RunConfig` (CLI /
    /// `brigid.toml` / `BRIGID_PROVIDER` / `BRIGID_MODEL` after merge). They
    /// take precedence over the raw env vars of the same name when set.
    ///
    /// Other LLM knobs (`BRIGID_LLM_BASE_URL`, `BRIGID_LLM_MAX_TOKENS`,
    /// attribution headers, API keys) are **env-only** today — `RunConfig`
    /// has no fields for them. Pass resolved provider/model from the CLI
    /// when available; when calling this from library code without a
    /// `RunConfig`, omit the overrides and `BRIGID_PROVIDER` is still
    /// honoured so a standalone `from_env()` path works.
    ///
    /// Provider resolution (ADR 0017):
    /// 1. Explicit provider override (or `BRIGID_PROVIDER`) → preset defaults
    /// 2. Else infer `provider_name` from `BRIGID_LLM_BASE_URL`
    /// 3. DeepSeek remains the default when nothing is specified
    ///
    /// API key chain: `BRIGID_LLM_API_KEY` → provider-specific key
    /// (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`, or `DEEPSEEK_API_KEY` for
    /// DeepSeek). A DeepSeek-scoped key is never sent to OpenRouter/OpenAI.
    ///
    /// Blank or whitespace-only env values are treated as unset.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Provider`] if no API key is present, or if
    /// OpenAI/OpenRouter is selected without a model.
    pub fn from_env_with(
        provider_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self, LlmError> {
        let provider_hint = provider_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| nonblank_env("BRIGID_PROVIDER"));

        let mut preset = ProviderPreset::from_name(provider_hint.as_deref());

        let base_url = nonblank_env("BRIGID_LLM_BASE_URL")
            .or_else(|| preset.default_base_url().map(str::to_string))
            .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());

        // When no explicit provider was given, refine from the base URL so
        // pointing BRIGID_LLM_BASE_URL at OpenRouter does not mis-label the
        // cache key as "deepseek".
        if matches!(&preset, ProviderPreset::Custom { name } if name.is_empty()) {
            let inferred = infer_provider_name_from_base_url(&base_url);
            preset = ProviderPreset::from_name(Some(inferred));
        }

        let provider_name = preset.provider_name().to_string();

        let model_from_override = model_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let model_from_env = nonblank_env("BRIGID_LLM_MODEL");
        let model = match (
            model_from_override.or(model_from_env),
            preset.default_model(),
            preset.requires_explicit_model(),
        ) {
            (Some(m), _, _) => m,
            (None, Some(default), _) => default.to_string(),
            (None, None, true) => {
                return Err(LlmError::Provider {
                    status: 0,
                    body: format!(
                        "provider '{provider_name}' requires an explicit model (set model in brigid.toml, BRIGID_MODEL, or BRIGID_LLM_MODEL; OpenRouter models look like 'openai/gpt-4o')"
                    ),
                });
            }
            // DeepSeek (or unset→inferred DeepSeek) is the only preset with a
            // safe default model.
            (None, None, false) => "deepseek-chat".to_string(),
        };

        // Key resolution: BRIGID_LLM_API_KEY → provider-specific key only.
        // Do not fall back to DEEPSEEK_API_KEY for OpenRouter/OpenAI — that
        // would send a DeepSeek-scoped credential to another host.
        let api_key = nonblank_env("BRIGID_LLM_API_KEY")
            .or_else(|| preset.api_key_env().and_then(nonblank_env))
            .ok_or_else(|| {
                let hint = match preset.api_key_env() {
                    Some(k) => format!("BRIGID_LLM_API_KEY or {k}"),
                    None => "BRIGID_LLM_API_KEY".to_string(),
                };
                LlmError::Provider {
                    status: 0,
                    body: format!("API key not set for provider '{provider_name}': set {hint}"),
                }
            })?;

        let mut allowed_hosts: Vec<String> = DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(extra) = nonblank_env("BRIGID_LLM_ALLOWED_HOSTS") {
            for h in extra
                .split(',')
                .map(|h| h.trim().to_lowercase())
                .filter(|h| !h.is_empty())
            {
                brigid_core::validate_hostname(&h).map_err(|err| {
                    LlmError::network(format!("invalid BRIGID_LLM_ALLOWED_HOSTS entry: {err}"))
                })?;
                if !allowed_hosts.iter().any(|existing| existing == &h) {
                    allowed_hosts.push(h);
                }
            }
        }
        let max_tokens = match nonblank_env("BRIGID_LLM_MAX_TOKENS") {
            Some(raw) => raw.parse::<u32>().map_err(|_| {
                LlmError::network(format!(
                    "invalid BRIGID_LLM_MAX_TOKENS value {raw:?}: expected a positive integer"
                ))
            })?,
            None => DEFAULT_MAX_TOKENS,
        };

        // Attribution: unset → defaults for OpenRouter only; explicit
        // off/none/0/false/no → disabled; any other nonblank → custom value.
        let http_referer = match nonblank_env("BRIGID_LLM_REFERER") {
            Some(v) if is_attribution_disabled(&v) => None,
            Some(v) => Some(v),
            None if provider_name == "openrouter" => Some(DEFAULT_OPENROUTER_REFERER.to_string()),
            None => None,
        };
        let app_title = match nonblank_env("BRIGID_LLM_APP_TITLE") {
            Some(v) if is_attribution_disabled(&v) => None,
            Some(v) => Some(v),
            None if provider_name == "openrouter" => Some(DEFAULT_OPENROUTER_TITLE.to_string()),
            None => None,
        };

        if provider_name == "openrouter" && !model.contains('/') {
            eprintln!(
                "brigid: warning: OpenRouter model '{model}' has no '/' namespace                  (expected e.g. 'openai/gpt-4o'); continuing anyway"
            );
        }

        Ok(Self {
            base_url,
            api_key,
            model,
            timeout: Duration::from_secs(120),
            max_retries: 3,
            initial_backoff: Duration::from_secs(1),
            provider_name,
            allowed_hosts,
            max_tokens,
            http_referer,
            app_title,
        })
    }

    /// Set the request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the maximum number of retry attempts.
    #[must_use]
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Set the output token cap sent as `max_tokens` in the request body.
    #[must_use]
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Set the provider name used for cache keys.
    #[must_use]
    pub fn with_provider_name(mut self, name: &str) -> Self {
        self.provider_name = name.to_string();
        self
    }

    /// Set the optional `HTTP-Referer` attribution header.
    #[must_use]
    pub fn with_http_referer(mut self, referer: impl Into<String>) -> Self {
        self.http_referer = Some(referer.into());
        self
    }

    /// Set the optional `X-Title` attribution header.
    #[must_use]
    pub fn with_app_title(mut self, title: impl Into<String>) -> Self {
        self.app_title = Some(title.into());
        self
    }

    /// Add a host to the allowlist for `Authorization` header validation.
    ///
    /// Use this when pointing the client at a self-hosted or custom
    /// OpenAI-compatible provider whose host is not in the built-in list
    /// (`api.openai.com`, `api.deepseek.com`, `localhost`, `127.0.0.1`).
    /// The host is compared case-insensitively.
    #[must_use]
    pub fn with_allowed_host(mut self, host: &str) -> Self {
        let lower = host.to_lowercase();
        if !self.allowed_hosts.iter().any(|h| h == &lower) {
            self.allowed_hosts.push(lower);
        }
        self
    }

    /// Validate that the `base_url` host is in the allowlist.
    ///
    /// Returns `Ok(())` if the host is allowed, or a [`LlmError::Network`]
    /// describing the rejected host. This check runs before any HTTP call
    /// so the `Authorization` header is never sent to an unapproved host.
    fn validate_host(&self) -> Result<(), LlmError> {
        let host = reqwest::Url::parse(&self.base_url)
            .map_err(|e| {
                LlmError::network(format!("failed to parse base_url '{}': {e}", self.base_url))
            })?
            .host_str()
            .ok_or_else(|| {
                LlmError::network(format!(
                    "base_url '{}' has no host component; \
                     refusing to send Authorization header to a hostless URL",
                    self.base_url
                ))
            })?
            .to_lowercase();
        if self.allowed_hosts.iter().any(|h| h == &host) {
            Ok(())
        } else {
            Err(LlmError::network(format!(
                "host '{host}' is not in the allowed hosts list; \
                 refusing to send Authorization header to unapproved host"
            )))
        }
    }

    /// Redact the API key for safe inclusion in error/log messages: show only
    /// the first four characters followed by `...`.
    #[must_use]
    fn redact_key(&self) -> String {
        let k = &self.api_key;
        if k.len() <= 4 {
            format!("{}...", k)
        } else {
            format!("{}...", &k[..4])
        }
    }
}

/// OpenAI-compatible LLM client using `reqwest` with retry/backoff/timeout.
pub struct OpenAiCompatibleClient {
    config: OpenAiClientConfig,
    http: reqwest::Client,
    cache: Option<DiskCache>,
}

impl OpenAiCompatibleClient {
    /// Create a new client from the given config.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Network`] if the underlying `reqwest::Client`
    /// cannot be built (e.g. invalid TLS configuration).
    pub fn new(config: OpenAiClientConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| LlmError::network(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            config,
            http,
            cache: None,
        })
    }

    /// Attach a [`DiskCache`] for response caching.
    #[must_use]
    pub fn with_cache(mut self, cache: DiskCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Build the chat-completions request URL.
    fn completions_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }

    /// Compute the backoff for a given 0-indexed attempt, capped at
    /// [`BACKOFF_CAP`].
    fn backoff_for(&self, attempt: u32) -> Duration {
        let mut d = self.config.initial_backoff;
        for _ in 0..attempt {
            d = d.saturating_mul(2);
            if d >= BACKOFF_CAP {
                return BACKOFF_CAP;
            }
        }
        d.min(BACKOFF_CAP)
    }
}

/// Request body for the chat-completions endpoint.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    max_tokens: u32,
}

/// A single chat message.
#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// The `choices[0].message` portion of a chat-completions response.
#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}

/// A single choice in a chat-completions response.
#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

/// The chat-completions response envelope.
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

/// Classify an HTTP status as retryable (429 or 5xx).
#[cfg(test)]
fn classify_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

#[async_trait]
impl LlmClient for OpenAiCompatibleClient {
    async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
        // a. Build cache key. max_tokens is part of the key: a response
        // cached under a smaller output cap (possibly truncated) must not be
        // served to callers with a larger cap.
        let cache_extras = format!("mt={}", self.config.max_tokens);
        let cache_input = CacheKeyInput {
            prompt,
            model: &self.config.model,
            provider: &self.config.provider_name,
            extras: Some(&cache_extras),
        };
        // b. Cache hit short-circuits.
        if let Some(cache) = &self.cache {
            if let Ok(Some(cached)) = cache.get_for(&cache_input).await {
                return Ok(cached);
            }
        }

        // c. Validate host before sending Authorization header.
        self.config.validate_host()?;

        let url = self.completions_url();
        let body = ChatRequest {
            model: &self.config.model,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
            stream: false,
            max_tokens: self.config.max_tokens,
        };

        let mut last_error: Option<LlmError> = None;
        // Initial attempt + max_retries retries.
        for attempt in 0..=self.config.max_retries {
            let mut req = self
                .http
                .post(&url)
                .bearer_auth(&self.config.api_key)
                .json(&body);
            // OpenRouter (and optional other gateways) use attribution headers.
            if let Some(referer) = &self.config.http_referer {
                req = req.header("HTTP-Referer", referer.as_str());
            }
            if let Some(title) = &self.config.app_title {
                req = req.header("X-Title", title.as_str());
            }

            // e. Apply timeout via tokio::time::timeout.
            let send = tokio::time::timeout(self.config.timeout, req.send());
            let response = match send.await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => {
                    // Builder errors are not retryable.
                    if e.is_builder() {
                        return Err(LlmError::network(format!("request builder error: {e}")));
                    }
                    // A timeout (from the client's own deadline) is not
                    // retried — the overall request deadline has elapsed.
                    if e.is_timeout() {
                        return Err(LlmError::Timeout);
                    }
                    // h. Network error: retry with backoff.
                    last_error = Some(LlmError::network(format!(
                        "request failed (key {}): {e}",
                        self.config.redact_key()
                    )));
                    if attempt < self.config.max_retries {
                        let bo = self.backoff_for(attempt);
                        tokio::time::sleep(bo).await;
                        continue;
                    }
                    break;
                }
                Err(_) => {
                    // Timeout is not retried here — the overall request
                    // deadline has elapsed.
                    return Err(LlmError::Timeout);
                }
            };

            let status = response.status();

            if status.is_success() {
                // f. Parse the response.
                let text = response
                    .text()
                    .await
                    .map_err(|e| LlmError::parse(format!("failed to read response body: {e}")))?;
                let parsed: ChatResponse = serde_json::from_str(&text)
                    .map_err(|e| LlmError::parse(format!("invalid response JSON: {e}")))?;
                let content = parsed
                    .choices
                    .into_iter()
                    .next()
                    .and_then(|c| c.message.content)
                    .ok_or_else(|| LlmError::parse("response had no choices[0].message.content"))?;
                // Store in cache.
                if let Some(cache) = &self.cache {
                    let _ = cache.put_for(&cache_input, &content).await;
                }
                return Ok(content);
            }

            // Capture headers before consuming the body.
            let retry_after = parse_retry_after(response.headers());

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                // g. 429 is retryable; carry Retry-After hint.
                last_error = Some(LlmError::RateLimit { retry_after });
                if attempt < self.config.max_retries {
                    let bo = retry_after.unwrap_or(self.backoff_for(attempt));
                    tokio::time::sleep(bo).await;
                    continue;
                }
                break;
            }

            if status.is_server_error() {
                let resp_body = response.text().await.unwrap_or_default();
                last_error = Some(LlmError::Provider {
                    status: status.as_u16(),
                    body: truncate_body(&resp_body),
                });
                if attempt < self.config.max_retries {
                    let bo = self.backoff_for(attempt);
                    tokio::time::sleep(bo).await;
                    continue;
                }
                break;
            }

            // i. 4xx (except 429): no retry.
            let resp_body = response.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: truncate_body(&resp_body),
            });
        }

        // j. After max_retries exhausted.
        Err(last_error
            .unwrap_or_else(|| LlmError::network("retries exhausted with no error captured")))
    }
}

/// Parse the `Retry-After` header (seconds form) into a [`Duration`].
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Truncate a response body for inclusion in error messages to avoid leaking
/// secrets or excessive log volume.
fn truncate_body(body: &str) -> String {
    const MAX: usize = 512;
    if body.len() > MAX {
        format!("{}...(truncated)", &body[..MAX])
    } else {
        body.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn temp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("brigid-llm-openai-{n}"))
    }

    /// Build a client pointed at the given mock server with tiny backoff.
    fn client_for(server: &MockServer) -> OpenAiCompatibleClient {
        let config = OpenAiClientConfig::new(server.uri(), "sk-test-key-1234", "deepseek-chat")
            .with_max_retries(3)
            .with_timeout(Duration::from_secs(10));
        // Override initial_backoff via a tiny hack: build then it's private.
        // We expose backoff through config; set via a small test helper by
        // constructing then mutating is not possible (fields pub). Use pub.
        let mut config = config;
        config.initial_backoff = Duration::from_millis(1);
        OpenAiCompatibleClient::new(config).unwrap()
    }

    fn ok_response(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }]
        }))
    }

    #[tokio::test]
    async fn success_returns_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("hello from llm"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "hello from llm");
    }

    #[tokio::test]
    async fn sends_bearer_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer sk-test-key-1234"))
            .respond_with(ok_response("ok"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    async fn sends_model_and_prompt_in_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [{"role":"user","content":"hi there"}],
                "stream": false,
                "max_tokens": 8192
            })))
            .respond_with(ok_response("ok"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let out = client.complete("hi there").await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    async fn retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("recovered"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "recovered");
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("after rate limit"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "after rate limit");
    }

    #[tokio::test]
    async fn does_not_retry_on_400() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server);
        let err = client.complete("hi").await.unwrap_err();
        match err {
            LlmError::Provider { status, body } => {
                assert_eq!(status, 400);
                assert_eq!(body, "bad request");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn does_not_retry_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server);
        let err = client.complete("hi").await.unwrap_err();
        match err {
            LlmError::Provider { status, body } => {
                assert_eq!(status, 401);
                assert_eq!(body, "unauthorized");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_retries_exhausted_on_500() {
        let server = MockServer::start().await;
        // 1 initial + 3 retries = 4 calls total.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .expect(4)
            .mount(&server)
            .await;
        let client = client_for(&server);
        let err = client.complete("hi").await.unwrap_err();
        match err {
            LlmError::Provider { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "down");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_returns_timeout_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("late").set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let config = OpenAiClientConfig::new(server.uri(), "sk-test-key-1234", "deepseek-chat")
            .with_max_retries(0)
            .with_timeout(Duration::from_millis(100));
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        assert!(matches!(err, LlmError::Timeout), "got: {err:?}");
    }

    #[tokio::test]
    async fn rate_limit_with_retry_after_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "5")
                    .set_body_string("slow down"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let config = OpenAiClientConfig::new(server.uri(), "sk-test-key-1234", "deepseek-chat")
            .with_max_retries(0);
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        match err {
            LlmError::RateLimit { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(5)));
            }
            other => panic!("expected RateLimit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_error_on_malformed_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let err = client.complete("hi").await.unwrap_err();
        assert!(matches!(err, LlmError::Parse { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn parse_error_when_no_choices() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "x",
                "choices": []
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let err = client.complete("hi").await.unwrap_err();
        assert!(matches!(err, LlmError::Parse { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn cache_hit_returns_without_http() {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "cached prompt",
            model: "deepseek-chat",
            provider: "deepseek",
            extras: Some("mt=8192"),
        };
        cache.put_for(&input, "cached response").await.unwrap();

        // Server that would fail if hit.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let config = OpenAiClientConfig::new(server.uri(), "sk-test-key-1234", "deepseek-chat")
            .with_provider_name("deepseek");
        let client = OpenAiCompatibleClient::new(config)
            .unwrap()
            .with_cache(cache);
        let out = client.complete("cached prompt").await.unwrap();
        assert_eq!(out, "cached response");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cache_store_on_success() {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("fresh response"))
            .expect(1)
            .mount(&server)
            .await;
        let config = OpenAiClientConfig::new(server.uri(), "sk-test-key-1234", "deepseek-chat")
            .with_provider_name("deepseek");
        let client = OpenAiCompatibleClient::new(config)
            .unwrap()
            .with_cache(cache.clone());
        let out = client.complete("store me").await.unwrap();
        assert_eq!(out, "fresh response");

        // Verify it was stored.
        let input = CacheKeyInput {
            prompt: "store me",
            model: "deepseek-chat",
            provider: "deepseek",
            extras: Some("mt=8192"),
        };
        assert_eq!(
            cache.get_for(&input).await.unwrap().as_deref(),
            Some("fresh response")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn max_tokens_change_busts_cache() {
        // A response cached under one max_tokens cap must not be served to a
        // client configured with a different cap — a truncated response
        // cached before a cap increase would otherwise poison retries.
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let poisoned = CacheKeyInput {
            prompt: "same prompt",
            model: "deepseek-chat",
            provider: "deepseek",
            extras: Some("mt=4096"),
        };
        cache.put_for(&poisoned, "truncated").await.unwrap();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("full response"))
            .expect(1)
            .mount(&server)
            .await;

        let config = OpenAiClientConfig::new(server.uri(), "sk-test-key-1234", "deepseek-chat")
            .with_provider_name("deepseek")
            .with_max_tokens(8192);
        let client = OpenAiCompatibleClient::new(config)
            .unwrap()
            .with_cache(cache);
        let out = client.complete("same prompt").await.unwrap();
        assert_eq!(out, "full response");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn network_error_retries_then_fails() {
        // Point at a port that refuses connections.
        let config =
            OpenAiClientConfig::new("http://127.0.0.1:1", "sk-test-key-1234", "deepseek-chat")
                .with_max_retries(1);
        let mut config = config;
        config.initial_backoff = Duration::from_millis(1);
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        assert!(matches!(err, LlmError::Network { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn redact_key_shows_prefix_only() {
        let config = OpenAiClientConfig::new("https://x", "sk-secret-abcdef", "m");
        assert_eq!(config.redact_key(), "sk-s...");
    }

    #[test]
    #[serial]
    fn from_env_defaults_when_set() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-env-test");
            env::remove_var("DEEPSEEK_API_KEY");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
            env::remove_var("BRIGID_LLM_MAX_TOKENS");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.api_key, "sk-env-test");
        assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
        assert_eq!(cfg.model, "deepseek-chat");
        assert_eq!(cfg.provider_name, "deepseek");
        assert_eq!(cfg.timeout, Duration::from_secs(120));
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.max_tokens, 8192);
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_max_tokens_override() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-env-test");
            env::set_var("BRIGID_LLM_MAX_TOKENS", "16384");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.max_tokens, 16384);
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_MAX_TOKENS");
        }
    }

    #[test]
    #[serial]
    fn from_env_max_tokens_invalid_errors() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-env-test");
            env::set_var("BRIGID_LLM_MAX_TOKENS", "not-a-number");
        }
        let err = OpenAiClientConfig::from_env().unwrap_err();
        assert!(
            matches!(err, LlmError::Network { .. }),
            "expected Network error for invalid max_tokens, got: {err:?}"
        );
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_MAX_TOKENS");
        }
    }

    #[test]
    #[serial]
    fn from_env_max_tokens_blank_uses_default() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-env-test");
            env::set_var("BRIGID_LLM_MAX_TOKENS", "   ");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.max_tokens, 8192);
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_MAX_TOKENS");
        }
    }

    #[test]
    #[serial]
    fn from_env_uses_deepseek_fallback() {
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::set_var("DEEPSEEK_API_KEY", "sk-fallback");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.api_key, "sk-fallback");
        unsafe {
            env::remove_var("DEEPSEEK_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_openai_provider_name() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::set_var("BRIGID_LLM_BASE_URL", "https://api.openai.com/v1");
            env::set_var("BRIGID_LLM_MODEL", "gpt-4o");
            env::remove_var("BRIGID_PROVIDER");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.provider_name, "openai");
        assert_eq!(cfg.model, "gpt-4o");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
        }
    }

    #[test]
    #[serial]
    fn from_env_errors_when_no_key() {
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("DEEPSEEK_API_KEY");
        }
        let err = OpenAiClientConfig::from_env().unwrap_err();
        assert!(matches!(err, LlmError::Provider { .. }), "got: {err:?}");
    }

    #[test]
    #[serial]
    fn from_env_errors_when_api_key_blank() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "");
            env::remove_var("DEEPSEEK_API_KEY");
        }
        let err = OpenAiClientConfig::from_env().unwrap_err();
        assert!(matches!(err, LlmError::Provider { .. }), "got: {err:?}");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_errors_when_api_key_whitespace_only() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "   \t  ");
            env::remove_var("DEEPSEEK_API_KEY");
        }
        let err = OpenAiClientConfig::from_env().unwrap_err();
        assert!(matches!(err, LlmError::Provider { .. }), "got: {err:?}");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_falls_back_to_deepseek_when_primary_blank() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "");
            env::set_var("DEEPSEEK_API_KEY", "sk-fallback");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.api_key, "sk-fallback");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("DEEPSEEK_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_blank_base_url_uses_default() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::set_var("BRIGID_LLM_BASE_URL", "   ");
            env::remove_var("BRIGID_LLM_MODEL");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
    }

    #[test]
    #[serial]
    fn from_env_blank_model_uses_default() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::set_var("BRIGID_LLM_MODEL", "");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.model, "deepseek-chat");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_MODEL");
        }
    }

    #[tokio::test]
    async fn hostile_host_rejected_before_network_call() {
        let config =
            OpenAiClientConfig::new("https://evil.example.com/v1", "sk-test-key-1234", "m");
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        assert!(
            matches!(err, LlmError::Network { .. }),
            "expected LlmError::Network for hostile host, got: {err:?}"
        );
        assert!(
            err.to_string().contains("host"),
            "error should mention host validation: {err}"
        );
    }

    #[tokio::test]
    async fn allowed_custom_host_proceeds() {
        // Use a port that refuses connections — we just want to confirm
        // the host passes validation and the request is attempted.
        let config = OpenAiClientConfig::new("http://my-custom-llm.local:1", "sk-test", "m")
            .with_allowed_host("my-custom-llm.local");
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        // Should be a network error (connection refused), NOT a host
        // validation error.
        assert!(matches!(err, LlmError::Network { .. }), "got: {err:?}");
        assert!(
            !err.to_string().contains("host"),
            "custom host should pass validation: {err}"
        );
    }

    #[test]
    fn default_allowed_hosts_includes_known_providers() {
        let config = OpenAiClientConfig::new("https://api.deepseek.com/v1", "k", "m");
        assert!(config.allowed_hosts.contains(&"api.openai.com".to_string()));
        assert!(
            config
                .allowed_hosts
                .contains(&"api.deepseek.com".to_string())
        );
        assert!(config.allowed_hosts.contains(&"openrouter.ai".to_string()));
        assert!(config.allowed_hosts.contains(&"localhost".to_string()));
        assert!(config.allowed_hosts.contains(&"127.0.0.1".to_string()));
    }

    #[tokio::test]
    async fn localhost_with_port_passes_validation() {
        // Port is stripped by host_str(); only the host is checked.
        let config = OpenAiClientConfig::new("http://localhost:1", "sk-test", "m");
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let err = client.complete("hi").await.unwrap_err();
        // Should be a connection-refused network error, NOT a host
        // validation error.
        assert!(matches!(err, LlmError::Network { .. }), "got: {err:?}");
        assert!(
            !err.to_string().contains("not in the allowed"),
            "localhost with port should pass validation: {err}"
        );
    }

    #[test]
    #[serial]
    fn from_env_allowed_hosts_env_extends_list() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
            env::set_var(
                "BRIGID_LLM_ALLOWED_HOSTS",
                "my-custom-llm.local, Another.Host.COM ",
            );
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        // Defaults still present.
        assert!(cfg.allowed_hosts.contains(&"api.deepseek.com".to_string()));
        // Extra hosts added, trimmed and lowercased.
        assert!(
            cfg.allowed_hosts
                .contains(&"my-custom-llm.local".to_string())
        );
        assert!(cfg.allowed_hosts.contains(&"another.host.com".to_string()));
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_ALLOWED_HOSTS");
        }
    }

    #[test]
    fn with_allowed_host_dedups_existing_entries() {
        let config = OpenAiClientConfig::new("https://api.deepseek.com/v1", "k", "m")
            .with_allowed_host("api.deepseek.com")
            .with_allowed_host("my-proxy.internal")
            .with_allowed_host("MY-PROXY.INTERNAL");
        let deepseek_count = config
            .allowed_hosts
            .iter()
            .filter(|h| h == &"api.deepseek.com")
            .count();
        assert_eq!(deepseek_count, 1, "default host should not duplicate");
        let proxy_count = config
            .allowed_hosts
            .iter()
            .filter(|h| h == &"my-proxy.internal")
            .count();
        assert_eq!(
            proxy_count, 1,
            "case-variant duplicate host should be deduplicated"
        );
    }

    #[test]
    #[serial]
    fn from_env_allowed_hosts_rejects_wildcard() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
            env::set_var("BRIGID_LLM_ALLOWED_HOSTS", "*.evil.com");
        }
        let err = OpenAiClientConfig::from_env().unwrap_err();
        assert!(
            matches!(err, LlmError::Network { .. }),
            "expected Network error for invalid host, got: {err:?}"
        );
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_ALLOWED_HOSTS");
        }
    }

    #[test]
    #[serial]
    fn from_env_allowed_hosts_rejects_path() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
            env::set_var("BRIGID_LLM_ALLOWED_HOSTS", "evil.com/path");
        }
        let err = OpenAiClientConfig::from_env().unwrap_err();
        assert!(
            matches!(err, LlmError::Network { .. }),
            "expected Network error for path host, got: {err:?}"
        );
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_ALLOWED_HOSTS");
        }
    }

    // ------------------------------------------------------------------
    // Issue #212: validate_host must reject URLs without a host
    // ------------------------------------------------------------------

    /// A `file:///` URL has no host component.  `validate_host` must
    /// reject it rather than defaulting to an empty string (which would
    /// bypass the allowlist and could send the Authorization header to
    /// an unintended target).
    #[test]
    fn validate_host_rejects_url_without_host() {
        let config = OpenAiClientConfig::new("file:///etc/passwd", "sk-test-key-1234", "m");
        let err = config.validate_host().unwrap_err();
        assert!(
            matches!(err, LlmError::Network { .. }),
            "expected Network error for hostless URL, got: {err:?}"
        );
        // The error must explicitly say the URL has no host — not just
        // that an empty-string host is "not in the allowed list".
        assert!(
            err.to_string().contains("no host"),
            "error should mention 'no host', got: {err}"
        );
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let config = OpenAiClientConfig::new("https://x", "k", "m").with_max_retries(5);
        let client = OpenAiCompatibleClient::new(config).unwrap();
        assert_eq!(client.backoff_for(0), Duration::from_secs(1));
        assert_eq!(client.backoff_for(1), Duration::from_secs(2));
        assert_eq!(client.backoff_for(2), Duration::from_secs(4));
        assert_eq!(client.backoff_for(6), Duration::from_secs(60));
    }

    #[test]
    fn new_returns_result_and_ok_variant_works() {
        // Verify new() returns Result (not Self) and the Ok variant
        // produces a usable client. This is the compile-time guarantee
        // that builder errors are propagated, not silently swallowed.
        let config = OpenAiClientConfig::new("https://api.deepseek.com/v1", "sk-test", "m");
        let result = OpenAiCompatibleClient::new(config);
        assert!(result.is_ok(), "new() should return Ok on valid config");
    }

    #[test]
    fn classify_status_correct() {
        assert!(classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(classify_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(classify_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(!classify_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!classify_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!classify_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!classify_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!classify_status(reqwest::StatusCode::OK));
    }
    // ------------------------------------------------------------------
    // ADR 0017: OpenRouter / provider presets
    // ------------------------------------------------------------------

    #[test]
    fn provider_preset_from_name_normalizes_case() {
        assert_eq!(
            ProviderPreset::from_name(Some("OpenRouter")),
            ProviderPreset::Openrouter
        );
        assert_eq!(
            ProviderPreset::from_name(Some("OPENAI")),
            ProviderPreset::Openai
        );
        assert_eq!(
            ProviderPreset::from_name(Some("DeepSeek")),
            ProviderPreset::Deepseek
        );
        assert_eq!(
            ProviderPreset::from_name(Some("my-proxy")),
            ProviderPreset::Custom {
                name: "my-proxy".into()
            }
        );
    }

    #[test]
    #[serial]
    fn from_env_with_openrouter_requires_model() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-or");
            env::remove_var("BRIGID_LLM_MODEL");
            env::remove_var("BRIGID_MODEL");
            env::remove_var("BRIGID_PROVIDER");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("OPENROUTER_API_KEY");
        }
        let err = OpenAiClientConfig::from_env_with(Some("openrouter"), None).unwrap_err();
        match err {
            LlmError::Provider { body, .. } => {
                assert!(body.contains("requires an explicit model"), "got: {body}");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_with_openrouter_defaults() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-or");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
            env::remove_var("BRIGID_PROVIDER");
            env::remove_var("BRIGID_LLM_REFERER");
            env::remove_var("BRIGID_LLM_APP_TITLE");
            env::remove_var("OPENROUTER_API_KEY");
        }
        let cfg =
            OpenAiClientConfig::from_env_with(Some("openrouter"), Some("openai/gpt-4o")).unwrap();
        assert_eq!(cfg.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(cfg.model, "openai/gpt-4o");
        assert_eq!(cfg.provider_name, "openrouter");
        assert!(cfg.allowed_hosts.contains(&"openrouter.ai".to_string()));
        assert_eq!(
            cfg.http_referer.as_deref(),
            Some(DEFAULT_OPENROUTER_REFERER)
        );
        assert_eq!(cfg.app_title.as_deref(), Some(DEFAULT_OPENROUTER_TITLE));
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_openrouter_api_key_fallback() {
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("DEEPSEEK_API_KEY");
            env::set_var("OPENROUTER_API_KEY", "sk-openrouter-only");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_PROVIDER");
        }
        let cfg = OpenAiClientConfig::from_env_with(
            Some("openrouter"),
            Some("anthropic/claude-3.5-sonnet"),
        )
        .unwrap();
        assert_eq!(cfg.api_key, "sk-openrouter-only");
        assert_eq!(cfg.provider_name, "openrouter");
        unsafe {
            env::remove_var("OPENROUTER_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_infers_openrouter_from_base_url() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::set_var("BRIGID_LLM_BASE_URL", "https://openrouter.ai/api/v1");
            env::set_var("BRIGID_LLM_MODEL", "openai/gpt-4o");
            env::remove_var("BRIGID_PROVIDER");
        }
        let cfg = OpenAiClientConfig::from_env().unwrap();
        assert_eq!(cfg.provider_name, "openrouter");
        assert!(cfg.http_referer.is_some());
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
        }
    }

    #[test]
    #[serial]
    fn from_env_with_provider_override_beats_base_url_heuristic() {
        // Explicit deepseek + openrouter URL still labels cache as deepseek
        // (user chose provider; they may be tunneling).
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::set_var("BRIGID_LLM_BASE_URL", "https://openrouter.ai/api/v1");
            env::remove_var("BRIGID_PROVIDER");
        }
        let cfg =
            OpenAiClientConfig::from_env_with(Some("deepseek"), Some("deepseek-chat")).unwrap();
        assert_eq!(cfg.provider_name, "deepseek");
        assert_eq!(cfg.base_url, "https://openrouter.ai/api/v1");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
    }

    #[test]
    fn openrouter_host_is_allowed_by_default() {
        let config =
            OpenAiClientConfig::new("https://openrouter.ai/api/v1", "sk-x", "openai/gpt-4o")
                .with_provider_name("openrouter");
        assert!(config.validate_host().is_ok());
    }

    #[tokio::test]
    async fn openrouter_sends_attribution_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer sk-or-test"))
            .and(header("HTTP-Referer", DEFAULT_OPENROUTER_REFERER))
            .and(header("X-Title", DEFAULT_OPENROUTER_TITLE))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "model": "openai/gpt-4o",
                "stream": false
            })))
            .respond_with(ok_response("via openrouter"))
            .mount(&server)
            .await;

        let config = OpenAiClientConfig::new(server.uri(), "sk-or-test", "openai/gpt-4o")
            .with_provider_name("openrouter")
            .with_http_referer(DEFAULT_OPENROUTER_REFERER)
            .with_app_title(DEFAULT_OPENROUTER_TITLE);
        let mut config = config;
        config.initial_backoff = Duration::from_millis(1);
        let client = OpenAiCompatibleClient::new(config).unwrap();
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "via openrouter");
    }

    #[tokio::test]
    async fn openrouter_cache_key_uses_openrouter_provider() {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_response("or-cached"))
            .expect(1)
            .mount(&server)
            .await;

        let config = OpenAiClientConfig::new(server.uri(), "sk-or", "openai/gpt-4o")
            .with_provider_name("openrouter");
        let client = OpenAiCompatibleClient::new(config)
            .unwrap()
            .with_cache(cache.clone());
        let out = client.complete("openrouter prompt").await.unwrap();
        assert_eq!(out, "or-cached");

        let input = CacheKeyInput {
            prompt: "openrouter prompt",
            model: "openai/gpt-4o",
            provider: "openrouter",
            extras: Some("mt=8192"),
        };
        assert_eq!(
            cache.get_for(&input).await.unwrap().as_deref(),
            Some("or-cached")
        );
        // Must not land under deepseek.
        let wrong = CacheKeyInput {
            prompt: "openrouter prompt",
            model: "openai/gpt-4o",
            provider: "deepseek",
            extras: Some("mt=8192"),
        };
        assert!(cache.get_for(&wrong).await.unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[serial]
    fn from_env_attribution_off_disables_headers() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-or");
            env::set_var("BRIGID_LLM_REFERER", "off");
            env::set_var("BRIGID_LLM_APP_TITLE", "none");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
        let cfg =
            OpenAiClientConfig::from_env_with(Some("openrouter"), Some("openai/gpt-4o")).unwrap();
        assert!(cfg.http_referer.is_none());
        assert!(cfg.app_title.is_none());
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_REFERER");
            env::remove_var("BRIGID_LLM_APP_TITLE");
        }
    }
    #[test]
    fn provider_preset_custom_keyword_keeps_name() {
        assert_eq!(
            ProviderPreset::from_name(Some("custom")),
            ProviderPreset::Custom {
                name: "custom".into()
            }
        );
        assert!(ProviderPreset::from_name(Some("custom")).requires_explicit_model());
    }

    #[test]
    #[serial]
    fn from_env_with_explicit_custom_requires_model() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::remove_var("BRIGID_LLM_MODEL");
            env::remove_var("BRIGID_PROVIDER");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
        let err = OpenAiClientConfig::from_env_with(Some("custom"), None).unwrap_err();
        match err {
            LlmError::Provider { body, .. } => {
                assert!(body.contains("requires an explicit model"), "got: {body}");
                assert!(
                    !body.contains("  "),
                    "error body should not contain double-space from indentation: {body:?}"
                );
            }
            other => panic!("expected Provider, got {other:?}"),
        }
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn from_env_with_explicit_custom_not_overwritten_by_base_url() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::set_var("BRIGID_LLM_BASE_URL", "https://openrouter.ai/api/v1");
            env::remove_var("BRIGID_PROVIDER");
        }
        let cfg =
            OpenAiClientConfig::from_env_with(Some("custom"), Some("my-local-model")).unwrap();
        assert_eq!(cfg.provider_name, "custom");
        assert_eq!(cfg.model, "my-local-model");
        assert_eq!(cfg.base_url, "https://openrouter.ai/api/v1");
        // Attribution headers only for openrouter provider_name.
        assert!(cfg.http_referer.is_none());
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
    }

    #[test]
    #[serial]
    fn from_env_with_named_custom_proxy_requires_model() {
        unsafe {
            env::set_var("BRIGID_LLM_API_KEY", "sk-x");
            env::remove_var("BRIGID_LLM_MODEL");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
        let err = OpenAiClientConfig::from_env_with(Some("my-proxy"), None).unwrap_err();
        assert!(matches!(err, LlmError::Provider { .. }), "got: {err:?}");
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
        }
    }
    #[test]
    #[serial]
    fn openrouter_does_not_use_deepseek_api_key() {
        // Security: a DeepSeek-scoped key must never be sent to OpenRouter.
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("OPENROUTER_API_KEY");
            env::set_var("DEEPSEEK_API_KEY", "sk-deepseek-only");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_PROVIDER");
        }
        let err = OpenAiClientConfig::from_env_with(Some("openrouter"), Some("openai/gpt-4o"))
            .unwrap_err();
        match err {
            LlmError::Provider { body, .. } => {
                assert!(body.contains("OPENROUTER_API_KEY"), "got: {body}");
                assert!(
                    body.contains("openrouter"),
                    "error should name the provider: {body}"
                );
            }
            other => panic!("expected Provider, got {other:?}"),
        }
        unsafe {
            env::remove_var("DEEPSEEK_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn openai_does_not_use_deepseek_api_key() {
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::remove_var("OPENAI_API_KEY");
            env::set_var("DEEPSEEK_API_KEY", "sk-deepseek-only");
            env::remove_var("BRIGID_LLM_BASE_URL");
        }
        let err = OpenAiClientConfig::from_env_with(Some("openai"), Some("gpt-4o")).unwrap_err();
        match err {
            LlmError::Provider { body, .. } => {
                assert!(body.contains("OPENAI_API_KEY"), "got: {body}");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
        unsafe {
            env::remove_var("DEEPSEEK_API_KEY");
        }
    }

    #[test]
    #[serial]
    fn deepseek_still_accepts_deepseek_api_key() {
        unsafe {
            env::remove_var("BRIGID_LLM_API_KEY");
            env::set_var("DEEPSEEK_API_KEY", "sk-deepseek-ok");
            env::remove_var("BRIGID_LLM_BASE_URL");
            env::remove_var("BRIGID_LLM_MODEL");
            env::remove_var("BRIGID_PROVIDER");
        }
        let cfg = OpenAiClientConfig::from_env_with(Some("deepseek"), None).unwrap();
        assert_eq!(cfg.api_key, "sk-deepseek-ok");
        assert_eq!(cfg.provider_name, "deepseek");
        unsafe {
            env::remove_var("DEEPSEEK_API_KEY");
        }
    }
}
