//! llm-kernel adapters used by the pipeline.
//!
//! The pipeline talks to [`llm_kernel::llm::LLMClient`] (`complete(LLMRequest)`).
//! This module keeps a small prompt-shaped convenience layer so stage code can
//! still send a single user string, plus a brigid-specific [`MockClient`] that
//! implements the kernel trait.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use brigid_core::ProgressTracker;
use brigid_core::progress::BudgetExceeded;
use futures::future::join_all;
use llm_kernel::error::KernelError;
use llm_kernel::llm::{ChatMessage, LLMClient, LLMRequest, LLMResponse, LLMStream};
use llm_kernel::store::KvStore;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Semaphore;

/// Alias for the kernel client trait used throughout the pipeline.
pub use llm_kernel::llm::LLMClient as LlmClient;

/// Newtype wrapper around `Box<dyn LLMClient>` so it can implement `LLMClient`
/// (needed to wrap a boxed trait object in [`StatsClient`]).
pub struct BoxedLlmClient(pub Box<dyn LLMClient>);

impl BoxedLlmClient {
    /// Wrap a boxed client.
    #[must_use]
    pub fn new(inner: Box<dyn LLMClient>) -> Self {
        Self(inner)
    }
}

#[async_trait]
impl LLMClient for BoxedLlmClient {
    async fn complete(&self, request: LLMRequest) -> Result<LLMResponse, KernelError> {
        self.0.complete(request).await
    }

    fn model_name(&self) -> &str {
        self.0.model_name()
    }

    async fn stream_complete(&self, request: LLMRequest) -> Result<LLMStream, KernelError> {
        self.0.stream_complete(request).await
    }
}

/// Hosts allowed to receive an `Authorization` header.
///
/// Mirrors the `brigid-llm` default allowlist so kernel-constructed clients
/// (live smoke tests, and later Phase 4 CLI construction) refuse to send
/// credentials to an unexpected host.
const DEFAULT_ALLOWED_LLM_HOSTS: &[&str] = &[
    "api.deepseek.com",
    "api.openai.com",
    "openrouter.ai",
    "localhost",
    "127.0.0.1",
];

/// Return `key` from the environment only when it is set to a non-blank value.
///
/// Blank and whitespace-only values are treated as unset (see
/// `docs/move-to-rust.md` §4.3).
#[must_use]
pub fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// [`nonempty_env`] with a fallback when the variable is unset or blank.
#[must_use]
pub fn nonempty_env_or(key: &str, default: impl Into<String>) -> String {
    nonempty_env(key).unwrap_or_else(|| default.into())
}

/// Extract the hostname from an HTTP(S) base URL (port stripped).
///
/// Uses the standards-compliant `url::Url` parser so userinfo tricks like
/// `https://api.openai.com:443@evil.example/v1` are correctly parsed as
/// targeting `evil.example`, not `api.openai.com`. URLs containing userinfo
/// are rejected entirely (return `None`) because no legitimate LLM provider
/// uses userinfo in its base URL. Only `http` and `https` schemes are
/// accepted; other schemes (e.g. `ftp`) never reach credential selection.
#[must_use]
pub fn host_from_base_url(base_url: &str) -> Option<String> {
    let parsed = url::Url::parse(base_url).ok()?;
    // Only HTTP(S) endpoints can serve an LLM API.
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    // Reject userinfo — no legitimate LLM provider endpoint uses it, and
    // its presence is a strong signal of a credential-exfiltration attempt.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.is_empty() { None } else { Some(host) }
}

/// Loopback hosts where cleartext HTTP is permitted (e.g. local LLM servers
/// like Ollama). Non-loopback hosts require HTTPS.
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1"];

/// Reject a base URL whose host is not on the allowed hosts list.
///
/// Also rejects URLs with userinfo (see [`host_from_base_url`]) and non-HTTP(S)
/// schemes. HTTPS is required for all non-loopback hosts — the Authorization
/// header must never be sent over cleartext to a remote provider.
///
/// `extra_hosts` extends the default allowlist with user-configured hosts
/// from `RunConfig.allowed_hosts`.
///
/// # Errors
///
/// Returns [`LlmError::Network`] when the URL cannot be parsed, contains
/// userinfo, uses a non-HTTP(S) scheme, requires HTTPS but uses HTTP, or
/// the host is not allowlisted.
pub fn validate_llm_base_url(base_url: &str) -> Result<(), LlmError> {
    validate_llm_base_url_with(base_url, &[])
}

/// Like [`validate_llm_base_url`] but with an extra hosts allowlist.
///
/// `extra_hosts` are user-configured hosts (from `RunConfig.allowed_hosts`)
/// that are permitted in addition to the default allowlist.
pub fn validate_llm_base_url_with(base_url: &str, extra_hosts: &[String]) -> Result<(), LlmError> {
    let host = host_from_base_url(base_url).ok_or_else(|| {
        LlmError::network(format!(
            "failed to parse base_url host from '{base_url}' \
             (userinfo and non-HTTP(S) schemes are rejected)"
        ))
    })?;

    // HTTPS enforcement: non-loopback hosts must use HTTPS.
    let is_https = base_url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://");
    if !is_https && !LOOPBACK_HOSTS.contains(&host.as_str()) {
        return Err(LlmError::network(format!(
            "host '{host}' requires https; \
             refusing to send Authorization header over cleartext"
        )));
    }

    // Host allowlist: default hosts + user-configured extra hosts.
    let allowed = DEFAULT_ALLOWED_LLM_HOSTS
        .iter()
        .copied()
        .chain(extra_hosts.iter().map(String::as_str))
        .any(|h| h == host.as_str());
    if allowed {
        Ok(())
    } else {
        Err(LlmError::network(format!(
            "host '{host}' is not in the allowed hosts list; \
             refusing to send Authorization header to unapproved host"
        )))
    }
}

/// Resolved LLM client configuration: the env var name holding the API key,
/// the model, and the base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLlmConfig {
    /// Provider name for diagnostics (e.g. `"deepseek"`, `"openai"`, `"openrouter"`).
    pub provider: String,
    /// Model identifier (e.g. `"deepseek-chat"`, `"gpt-4o"`, `"openai/gpt-4o"`).
    pub model: String,
    /// Environment variable name holding the API key.
    pub api_key_env: String,
    /// Base URL for the provider API.
    pub base_url: String,
}

/// Provider preset metadata used during resolution.
struct ProviderPreset {
    base_url: &'static str,
    default_model: Option<&'static str>,
    api_key_env: &'static str,
}

impl ProviderPreset {
    fn for_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "deepseek" => Some(Self {
                base_url: "https://api.deepseek.com/v1",
                default_model: Some("deepseek-chat"),
                api_key_env: "DEEPSEEK_API_KEY",
            }),
            "openai" => Some(Self {
                base_url: "https://api.openai.com/v1",
                default_model: None,
                api_key_env: "OPENAI_API_KEY",
            }),
            "openrouter" => Some(Self {
                base_url: "https://openrouter.ai/api/v1",
                default_model: None,
                api_key_env: "OPENROUTER_API_KEY",
            }),
            _ => None,
        }
    }
}

/// Infer a provider name from a base URL by matching the actual host
/// against known provider hosts. Returns `None` for unrecognized hosts
/// (including localhost), which are treated as custom.
fn infer_provider_from_base_url(base_url: &str) -> Option<&'static str> {
    let host = host_from_base_url(base_url)?;
    match host.as_str() {
        "api.deepseek.com" => Some("deepseek"),
        "api.openai.com" => Some("openai"),
        "openrouter.ai" => Some("openrouter"),
        _ => None,
    }
}

/// Expected host for each known provider preset.
impl ProviderPreset {
    fn expected_host(&self) -> &'static str {
        match self.api_key_env {
            "DEEPSEEK_API_KEY" => "api.deepseek.com",
            "OPENAI_API_KEY" => "api.openai.com",
            "OPENROUTER_API_KEY" => "openrouter.ai",
            _ => "",
        }
    }
}

/// Resolve LLM client configuration from the environment and optional overrides.
///
/// Provider resolution (ADR 0017):
/// 1. Explicit provider override (or `BRIGID_PROVIDER`) → preset defaults
/// 2. Else infer provider from `BRIGID_LLM_BASE_URL`
/// 3. DeepSeek is the default when nothing is specified
///
/// API key chain: `BRIGID_LLM_API_KEY` → provider-specific key
/// (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`, or `DEEPSEEK_API_KEY`).
/// A DeepSeek-scoped key is never sent to OpenRouter/OpenAI.
///
/// **Security**: when a known provider is selected (deepseek/openai/openrouter),
/// the base URL host must match that provider's expected host. This prevents
/// `BRIGID_PROVIDER=openai` + `BRIGID_LLM_BASE_URL=https://openrouter.ai/...`
/// from sending `OPENAI_API_KEY` to `openrouter.ai`. For localhost or custom
/// hosts (not a known provider), `BRIGID_LLM_API_KEY` is required — a
/// provider-specific key is never sent to an unrecognized host.
///
/// `extra_hosts` extends the default host allowlist with user-configured hosts
/// from `RunConfig.allowed_hosts`. HTTPS is required for all non-loopback hosts.
///
/// Blank or whitespace-only env values are treated as unset.
///
/// # Errors
///
/// Returns a human-readable error string when:
/// - The base URL host is not on the allowed hosts list
/// - The base URL uses a non-HTTP(S) scheme or non-HTTPS for a non-loopback host
/// - The base URL contains userinfo (credential exfiltration risk)
/// - A known provider's base URL host doesn't match the expected host
/// - A custom/localhost endpoint has no `BRIGID_LLM_API_KEY`
/// - No model is configured for a provider that requires one
pub fn resolve_llm_config(
    provider_override: Option<&str>,
    model_override: Option<&str>,
    extra_hosts: &[String],
) -> Result<ResolvedLlmConfig, String> {
    let provider_hint = provider_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| nonempty_env("BRIGID_PROVIDER"));

    let mut preset = provider_hint.as_deref().and_then(ProviderPreset::for_name);

    let (provider_name, default_base_url, default_model, api_key_env) = match &preset {
        Some(p) => (
            provider_hint.clone().unwrap_or_default(),
            p.base_url,
            p.default_model,
            p.api_key_env,
        ),
        None => {
            // Unset or custom — infer from base URL.
            let base_url_env = nonempty_env("BRIGID_LLM_BASE_URL");
            let inferred = base_url_env
                .as_deref()
                .and_then(infer_provider_from_base_url);
            match inferred.and_then(ProviderPreset::for_name) {
                Some(p) => {
                    // Inferred a known provider from the base URL host.
                    let name = inferred.unwrap().to_string();
                    let base = p.base_url;
                    let model = p.default_model;
                    let key = p.api_key_env;
                    preset = Some(p);
                    (name, base, model, key)
                }
                None if base_url_env.is_some() => {
                    // Custom or localhost — no preset, no provider-specific
                    // key. BRIGID_LLM_API_KEY will be required.
                    preset = None;
                    (
                        "custom".to_string(),
                        "https://api.deepseek.com/v1",
                        None,
                        "",
                    )
                }
                None => {
                    // No provider, no base URL env — default to DeepSeek.
                    let p = ProviderPreset::for_name("deepseek").unwrap();
                    let base = p.base_url;
                    let model = p.default_model;
                    let key = p.api_key_env;
                    preset = Some(p);
                    ("deepseek".to_string(), base, model, key)
                }
            }
        }
    };

    let base_url = nonempty_env_or("BRIGID_LLM_BASE_URL", default_base_url);

    // Validate the base URL (rejects userinfo, non-HTTP(S) schemes,
    // non-HTTPS for non-loopback, and checks allowlist + extra_hosts).
    validate_llm_base_url_with(&base_url, extra_hosts).map_err(|e| e.to_string())?;

    // Security: when a known provider is selected, verify the base URL host
    // matches the expected provider host. This prevents a provider-scoped key
    // (e.g. OPENAI_API_KEY) from being sent to a different provider's host
    // (e.g. openrouter.ai) via a BRIGID_LLM_BASE_URL override.
    if let Some(p) = &preset {
        let actual_host = host_from_base_url(&base_url)
            .ok_or_else(|| format!("failed to parse host from base_url '{base_url}'"))?;
        let expected = p.expected_host();
        if !expected.is_empty() && actual_host != expected {
            return Err(format!(
                "provider '{provider_name}' expects host '{expected}' but \
                 BRIGID_LLM_BASE_URL points to '{actual_host}'; refusing to send \
                 {api_key_env} to a different provider's host"
            ));
        }
    }

    // Model resolution: override → BRIGID_LLM_MODEL → preset default.
    let model = model_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| nonempty_env("BRIGID_LLM_MODEL"))
        .or_else(|| default_model.map(|s| s.to_string()))
        .ok_or_else(|| {
            "provider requires an explicit model (set model in brigid.toml, \
             BRIGID_MODEL, or BRIGID_LLM_MODEL)"
                .to_string()
        })?;

    // API key resolution: BRIGID_LLM_API_KEY takes precedence over
    // provider-specific key. This prevents a DeepSeek-scoped key from
    // being sent to OpenRouter/OpenAI.
    let api_key_env = if nonempty_env("BRIGID_LLM_API_KEY").is_some() {
        "BRIGID_LLM_API_KEY"
    } else {
        api_key_env
    };

    // Security: for localhost or custom hosts (not a known provider preset),
    // require BRIGID_LLM_API_KEY. A provider-specific key (e.g. DEEPSEEK_API_KEY)
    // must never be sent to an unrecognized host — a local proxy or custom
    // endpoint could forward it anywhere.
    let actual_host = host_from_base_url(&base_url)
        .ok_or_else(|| format!("failed to parse host from base_url '{base_url}'"))?;
    let is_known_provider_host = preset.is_some_and(|p| {
        let expected = p.expected_host();
        !expected.is_empty() && actual_host == expected
    });
    if !is_known_provider_host && api_key_env != "BRIGID_LLM_API_KEY" {
        return Err(format!(
            "host '{actual_host}' is not a known provider endpoint; \
             set BRIGID_LLM_API_KEY explicitly to send credentials to this host \
             (provider-specific keys like {api_key_env} are not sent to \
             unrecognized hosts)"
        ));
    }

    Ok(ResolvedLlmConfig {
        provider: provider_name,
        model,
        api_key_env: api_key_env.to_string(),
        base_url,
    })
}

/// Build a live [`LLMClient`] from the environment, wrapped in
/// `llm_kernel::llm::RetryClient` for bounded exponential backoff on 429/5xx
/// errors, optionally wrapped in `llm_kernel::llm::CacheClient` for response
/// caching.
///
/// Uses [`resolve_llm_config`] for provider/model/key resolution.
///
/// # Errors
///
/// Returns a human-readable error string on configuration or client
/// construction failure.
pub fn build_live_client(
    cache: Option<Arc<llm_kernel::store::kv::SqliteKvStore>>,
    provider: Option<&str>,
    model: Option<&str>,
    extra_hosts: &[String],
) -> Result<Box<dyn LLMClient>, String> {
    use llm_kernel::llm::{CacheClient, ModelConfig, OpenAIClient, RetryClient, RetryConfig};

    let resolved = resolve_llm_config(provider, model, extra_hosts)?;

    let config = ModelConfig {
        provider: resolved.provider.clone(),
        model: resolved.model,
        api_key_env: resolved.api_key_env,
        base_url: Some(resolved.base_url),
        temperature: 0.7,
        max_tokens: Some(4096),
    };

    let client = OpenAIClient::new(&config).map_err(|e| e.to_string())?;
    let client = RetryClient::new(client, RetryConfig::default());

    if let Some(store) = cache {
        Ok(Box::new(CacheClient::new(client, store)))
    } else {
        Ok(Box::new(client))
    }
}

/// Cache hit/miss statistics, queryable after a run.
#[derive(Debug, Default, Clone)]
pub struct CacheStats {
    /// Number of cache hits (responses served from cache).
    pub hits: u64,
    /// Number of cache misses (responses fetched from the upstream client).
    pub misses: u64,
}

impl CacheStats {
    /// Total number of cache lookups (hits + misses).
    #[must_use]
    pub fn total(&self) -> u64 {
        self.hits + self.misses
    }

    /// Hit rate as a percentage (0–100). Returns 0 when there are no lookups.
    #[must_use]
    pub fn hit_rate_percent(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            (self.hits as f64 / self.total() as f64) * 100.0
        }
    }
}

/// Shared atomic counters for cache hit/miss tracking.
///
/// Uses `AtomicU64` so increments are lock-free and never panic (unlike
/// `Mutex::lock().unwrap()` which would violate the library-code
/// no-panic rule).
#[derive(Debug, Default)]
struct AtomicCacheStats {
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl AtomicCacheStats {
    fn snapshot(&self) -> CacheStats {
        use std::sync::atomic::Ordering::Relaxed;
        CacheStats {
            hits: self.hits.load(Relaxed),
            misses: self.misses.load(Relaxed),
        }
    }
}

/// A shared handle to cache statistics that can be queried after the client
/// has been moved or consumed.
///
/// Cloning the handle is cheap (it shares the underlying atomics). Call
/// [`CacheStatsHandle::snapshot`] to read the current hit/miss counts.
#[derive(Debug, Clone)]
pub struct CacheStatsHandle {
    inner: Arc<AtomicCacheStats>,
}

impl CacheStatsHandle {
    /// Create a handle with zero stats — useful as a placeholder when
    /// the client is a mock (no cache to track).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(AtomicCacheStats::default()),
        }
    }

    /// Read the current hit/miss statistics.
    #[must_use]
    pub fn snapshot(&self) -> CacheStats {
        self.inner.snapshot()
    }
}

/// An [`LLMClient`] wrapper that tracks cache hit/miss statistics.
///
/// `StatsClient` probes the `KvStore` **before** delegating to the inner
/// client to determine whether a response will be served from cache. This
/// uses the same cache key derivation as `llm_kernel::llm::CacheClient`, so
/// the probe accurately predicts whether `CacheClient` will find a hit.
///
/// # Concurrency
///
/// The probe-then-delegate approach is **best-effort** under concurrency.
/// When multiple calls are in flight simultaneously (as happens with
/// `bounded_complete`), a concurrent call may store a cache entry between
/// this wrapper's probe and the inner client's lookup, causing a minor
/// undercount of hits and overcount of misses. The counts are approximate
/// but useful for reporting overall cache effectiveness.
///
/// # Key derivation drift
///
/// The cache key derivation is duplicated from `llm_kernel::llm::cache`.
/// If llm-kernel changes the key format, hasher, or key version, stats
/// will silently report all misses (non-fatal — the cache itself still
/// works correctly). The `stats_client_tracks_miss_then_hit` integration
/// test guards against drift today.
pub struct StatsClient<C> {
    inner: C,
    store: Option<Arc<llm_kernel::store::kv::SqliteKvStore>>,
    stats: Arc<AtomicCacheStats>,
}

impl<C> StatsClient<C> {
    /// Wrap `inner` with stats tracking. When `store` is `Some`, cache
    /// hit/miss is tracked by probing the store before each call.
    pub fn new(inner: C, store: Option<Arc<llm_kernel::store::kv::SqliteKvStore>>) -> Self {
        Self {
            inner,
            store,
            stats: Arc::new(AtomicCacheStats::default()),
        }
    }

    /// Read the current hit/miss statistics.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        self.stats.snapshot()
    }

    /// Get a clone of the shared stats handle, useful for collecting stats
    /// after a run when the client has been moved or consumed.
    #[must_use]
    pub fn stats_handle(&self) -> CacheStatsHandle {
        CacheStatsHandle {
            inner: Arc::clone(&self.stats),
        }
    }
}

#[async_trait]
impl<C: LLMClient> LLMClient for StatsClient<C> {
    async fn complete(&self, request: LLMRequest) -> Result<LLMResponse, KernelError> {
        // When a store is present, probe it to determine hit vs miss.
        // This mirrors CacheClient's key derivation but is best-effort:
        // if the probe fails, we count it as a miss.
        if let Some(store) = &self.store {
            let key = cache_key_for_stats(self.inner.model_name(), &request);
            let store = Arc::clone(store);
            let hit = tokio::task::spawn_blocking(move || {
                store.get(&key).map(|v| v.is_some()).unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            use std::sync::atomic::Ordering::Relaxed;
            if hit {
                self.stats.hits.fetch_add(1, Relaxed);
            } else {
                self.stats.misses.fetch_add(1, Relaxed);
            }
        }
        self.inner.complete(request).await
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    async fn stream_complete(&self, request: LLMRequest) -> Result<LLMStream, KernelError> {
        self.inner.stream_complete(request).await
    }
}

/// Derive a cache key matching `llm_kernel::llm::CacheClient`'s key scheme.
///
/// This replicates the key derivation from `llm_kernel::llm::cache::cache_key`
/// so `StatsClient` can probe the store for hit/miss tracking. If the scheme
/// changes upstream, stats will simply report all misses (non-fatal).
fn cache_key_for_stats(model_name: &str, request: &LLMRequest) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    const KEY_VERSION: u8 = 2;
    const KEY_PREFIX: &str = "llm-resp";

    let mut hasher = DefaultHasher::new();
    model_name.hash(&mut hasher);
    serde_json::to_vec(request)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{KEY_PREFIX}:v{KEY_VERSION}:{:016x}", hasher.finish())
}

/// Prompt-shaped errors matching the historical `brigid-llm` surface so
/// existing match arms and `#[from]` conversions stay readable.
#[derive(Clone, Debug, Error)]
pub enum LlmError {
    /// Network or transport failure.
    #[error("network error: {message}")]
    Network {
        /// Human-readable description of the transport failure.
        message: String,
    },
    /// The request timed out before the provider responded.
    #[error("request timed out")]
    Timeout,
    /// The provider returned a 429 rate-limit response.
    #[error("rate limited")]
    RateLimit {
        /// Optional advised wait before retrying.
        retry_after: Option<Duration>,
    },
    /// The provider returned a non-2xx status code (other than 429).
    #[error("provider error: status {status}: {body}")]
    Provider {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Raw response body (truncated by the provider client).
        body: String,
    },
    /// The provider response could not be parsed into completion text.
    #[error("failed to parse provider response: {message}")]
    Parse {
        /// Description of why parsing failed.
        message: String,
    },
}

impl LlmError {
    /// Convenience constructor for [`LlmError::Network`].
    #[must_use]
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network {
            message: message.into(),
        }
    }

    /// Convenience constructor for [`LlmError::Parse`].
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse {
            message: message.into(),
        }
    }

    /// Map a kernel error onto this prompt-shaped enum.
    #[must_use]
    pub fn from_kernel(err: KernelError) -> Self {
        match err {
            KernelError::Timeout(_) => Self::Timeout,
            KernelError::RateLimited(secs) => Self::RateLimit {
                retry_after: Some(Duration::from_secs(secs)),
            },
            KernelError::Http { status, message } => Self::Provider {
                status,
                body: message,
            },
            KernelError::Serialization(e) => Self::parse(e.to_string()),
            other => Self::network(other.to_string()),
        }
    }

    /// Map this enum onto a kernel error (for [`LLMClient`] implementations).
    #[must_use]
    pub fn into_kernel(self) -> KernelError {
        match self {
            Self::Timeout => KernelError::Timeout(0),
            Self::RateLimit { retry_after } => {
                KernelError::RateLimited(retry_after.map(|d| d.as_secs()).unwrap_or(0))
            }
            Self::Provider { status, body } => KernelError::Http {
                status,
                message: body,
            },
            Self::Parse { message } => KernelError::LlmApi(message),
            Self::Network { message } => KernelError::LlmApi(message),
        }
    }
}

/// Build a single-user-message request from a prompt string.
#[must_use]
pub fn prompt_request(prompt: impl Into<String>) -> LLMRequest {
    LLMRequest::builder().user_message(prompt).build()
}

/// Extract concatenated user/assistant text from a request (for test doubles).
#[must_use]
pub fn request_prompt(request: &LLMRequest) -> String {
    request
        .messages
        .iter()
        .map(ChatMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wrap completion text in a kernel response.
#[must_use]
pub fn text_response(content: impl Into<String>) -> LLMResponse {
    LLMResponse {
        content: content.into(),
        ..LLMResponse::default()
    }
}

/// Complete a single prompt string via [`LLMClient::complete`].
///
/// # Errors
///
/// Returns [`LlmError`] when the kernel client fails.
pub async fn complete_text(client: &dyn LLMClient, prompt: &str) -> Result<String, LlmError> {
    let response = client
        .complete(prompt_request(prompt))
        .await
        .map_err(LlmError::from_kernel)?;
    Ok(response.content)
}

/// Streaming is unused by the pipeline; test doubles return this error.
pub fn stream_unsupported() -> llm_kernel::error::Result<LLMStream> {
    Err(KernelError::LlmApi(
        "streaming is not supported by this client".into(),
    ))
}

/// Run prompt completions with a concurrency semaphore.
pub async fn bounded_complete(
    client: &dyn LLMClient,
    prompts: Vec<String>,
    max_concurrency: usize,
) -> Vec<Result<String, LlmError>> {
    let n = prompts.len();
    if n == 0 {
        return Vec::new();
    }
    let max = max_concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(max));
    let futures = prompts.into_iter().map(|prompt| {
        let sem = Arc::clone(&semaphore);
        async move {
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|_| LlmError::network("concurrency semaphore closed unexpectedly"))?;
            complete_text(client, &prompt).await
        }
    });
    join_all(futures).await
}

fn prompt_count(len: usize) -> Result<u32, BudgetExceeded> {
    u32::try_from(len).map_err(|_| BudgetExceeded {
        used: u32::MAX,
        max: u32::MAX,
    })
}

/// Bounded complete with a budget reservation on [`ProgressTracker`].
///
/// # Errors
///
/// Returns [`BudgetExceeded`] when the prompt count would overflow the budget.
pub async fn bounded_complete_with_budget(
    client: &dyn LLMClient,
    prompts: Vec<String>,
    max_concurrency: usize,
    progress: &mut ProgressTracker,
) -> Result<Vec<Result<String, LlmError>>, BudgetExceeded> {
    let n = prompt_count(prompts.len())?;
    progress.reserve_llm_calls(n)?;
    Ok(bounded_complete(client, prompts, max_concurrency).await)
}

struct MockState {
    responses: Vec<String>,
    next: usize,
    calls: usize,
    fail_on: Option<(usize, LlmError)>,
}

impl MockState {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses,
            next: 0,
            calls: 0,
            fail_on: None,
        }
    }
}

fn lock(state: &Mutex<MockState>) -> MutexGuard<'_, MockState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Network-free test double implementing [`LLMClient`].
pub struct MockClient {
    state: Mutex<MockState>,
}

impl MockClient {
    /// Single canned response, repeated for every call.
    #[must_use]
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            state: Mutex::new(MockState::new(vec![response.into()])),
        }
    }

    /// Sequence of responses; the last value is repeated when exhausted.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Parse`] when `responses` is empty.
    pub fn with_responses(responses: Vec<String>) -> Result<Self, LlmError> {
        if responses.is_empty() {
            return Err(LlmError::parse(
                "MockClient::with_responses requires at least one response",
            ));
        }
        Ok(Self {
            state: Mutex::new(MockState::new(responses)),
        })
    }

    /// Fail the `call_index`-th call (0-based) with `error`.
    #[must_use]
    pub fn fail_on(self, call_index: usize, error: LlmError) -> Self {
        {
            let mut state = lock(&self.state);
            state.fail_on = Some((call_index, error));
        }
        self
    }

    /// Number of `complete` calls observed so far.
    #[must_use]
    pub fn call_count(&self) -> usize {
        lock(&self.state).calls
    }

    fn next_response(state: &mut MockState) -> String {
        let idx = state.next.min(state.responses.len().saturating_sub(1));
        let resp = state.responses[idx].clone();
        if state.next < state.responses.len().saturating_sub(1) {
            state.next += 1;
        }
        resp
    }
}

#[async_trait]
impl LLMClient for MockClient {
    async fn complete(&self, _request: LLMRequest) -> llm_kernel::error::Result<LLMResponse> {
        let (response, error) = {
            let mut state = lock(&self.state);
            let call_index = state.calls;
            state.calls += 1;
            let response = Self::next_response(&mut state);
            let error = state
                .fail_on
                .as_ref()
                .and_then(|(idx, err)| (*idx == call_index).then(|| err.clone()));
            (response, error)
        };
        match error {
            Some(err) => Err(err.into_kernel()),
            None => Ok(text_response(response)),
        }
    }

    fn model_name(&self) -> &str {
        "mock"
    }

    async fn stream_complete(&self, _request: LLMRequest) -> llm_kernel::error::Result<LLMStream> {
        stream_unsupported()
    }
}

impl std::fmt::Debug for MockClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("MockClient")
            .field("responses_len", &state.responses.len())
            .field("next", &state.next)
            .field("calls", &state.calls)
            .field("has_fail_on", &state.fail_on.is_some())
            .finish()
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn nonempty_env_treats_blank_as_unset() {
        let key = "BRIGID_TEST_NONEMPTY_ENV";
        // Safety: test-only env key, not read concurrently by other tests.
        unsafe {
            std::env::set_var(key, "  ");
        }
        assert!(nonempty_env(key).is_none());
        assert_eq!(nonempty_env_or(key, "fallback"), "fallback");
        unsafe {
            std::env::set_var(key, "value");
        }
        assert_eq!(nonempty_env(key).as_deref(), Some("value"));
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn validate_llm_base_url_accepts_known_hosts() {
        assert!(validate_llm_base_url("https://api.deepseek.com/v1").is_ok());
        assert!(validate_llm_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_llm_base_url("https://openrouter.ai/api/v1").is_ok());
        assert!(validate_llm_base_url("http://localhost:11434/v1").is_ok());
    }

    #[test]
    fn validate_llm_base_url_rejects_unknown_host() {
        let err = validate_llm_base_url("https://evil.example/v1").unwrap_err();
        assert!(err.to_string().contains("not in the allowed"), "got: {err}");
    }

    #[test]
    fn validate_llm_base_url_rejects_empty() {
        assert!(validate_llm_base_url("").is_err());
        assert!(validate_llm_base_url("not-a-url").is_err());
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use serial_test::serial;

    /// Helper: run a closure with a set of env vars, restoring the previous
    /// state afterwards. Uses `unsafe` because `std::env::set_var` is unsafe
    /// in Rust 2024. Test-only; no concurrent access to these keys.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let mut saved = Vec::new();
        for (key, _) in vars {
            saved.push((*key, std::env::var(key).ok()));
        }
        unsafe {
            for (key, val) in vars {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        f();
        unsafe {
            for (key, val) in &saved {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    // Env keys touched by resolve_llm_config. Listed here so tests can
    // clean up reliably.
    const ENV_KEYS: &[&str] = &[
        "BRIGID_PROVIDER",
        "BRIGID_LLM_BASE_URL",
        "BRIGID_LLM_MODEL",
        "BRIGID_LLM_API_KEY",
        "DEEPSEEK_API_KEY",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
    ];

    fn clear_env() -> Vec<(&'static str, Option<&'static str>)> {
        ENV_KEYS.iter().map(|&k| (k, None)).collect::<Vec<_>>()
    }

    #[test]
    #[serial]
    fn deepseek_default_uses_deepseek_key() {
        let vars = clear_env();
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.provider, "deepseek");
            assert_eq!(cfg.model, "deepseek-chat");
            assert_eq!(cfg.api_key_env, "DEEPSEEK_API_KEY");
            assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
        });
    }

    #[test]
    #[serial]
    fn openai_provider_uses_openai_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.provider, "openai");
            assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
            assert_eq!(cfg.base_url, "https://api.openai.com/v1");
        });
    }

    #[test]
    #[serial]
    fn openrouter_provider_uses_openrouter_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openrouter")));
        vars.push(("BRIGID_LLM_MODEL", Some("openai/gpt-4o")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.provider, "openrouter");
            assert_eq!(cfg.api_key_env, "OPENROUTER_API_KEY");
            assert_eq!(cfg.base_url, "https://openrouter.ai/api/v1");
        });
    }

    /// Security: a DeepSeek-scoped key must never be sent to OpenRouter/OpenAI.
    /// When BRIGID_LLM_API_KEY is set, it takes precedence over all
    /// provider-specific keys — but if it's NOT set, the provider-specific
    /// key is used (never a DeepSeek key for OpenAI/OpenRouter).
    #[test]
    #[serial]
    fn openai_never_uses_deepseek_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        vars.push(("DEEPSEEK_API_KEY", Some("sk-deepseek-only")));
        // No OPENAI_API_KEY, no BRIGID_LLM_API_KEY
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
            // The resolved env var is OPENAI_API_KEY, NOT DEEPSEEK_API_KEY.
            // OpenAIClient::new will fail because OPENAI_API_KEY is unset,
            // but the key isolation is enforced at the config level.
        });
    }

    #[test]
    #[serial]
    fn openrouter_never_uses_deepseek_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openrouter")));
        vars.push(("BRIGID_LLM_MODEL", Some("openai/gpt-4o")));
        vars.push(("DEEPSEEK_API_KEY", Some("sk-deepseek-only")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.api_key_env, "OPENROUTER_API_KEY");
        });
    }

    /// Security: BRIGID_LLM_API_KEY takes precedence over provider-specific
    /// keys, so a user with a single key can use any provider.
    #[test]
    #[serial]
    fn brigid_llm_api_key_takes_precedence() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        vars.push(("BRIGID_LLM_API_KEY", Some("sk-universal")));
        vars.push(("OPENAI_API_KEY", Some("sk-openai-specific")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.api_key_env, "BRIGID_LLM_API_KEY");
        });
    }

    /// Security: blank env vars are treated as unset.
    #[test]
    #[serial]
    fn blank_env_treated_as_unset() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("  ")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            // Blank provider → inferred as deepseek from default base URL.
            assert_eq!(cfg.provider, "deepseek");
        });
    }

    #[test]
    #[serial]
    fn blank_api_key_treated_as_unset() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_API_KEY", Some("  ")));
        vars.push(("DEEPSEEK_API_KEY", Some("sk-deepseek")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            // Blank BRIGID_LLM_API_KEY → falls back to DEEPSEEK_API_KEY.
            assert_eq!(cfg.api_key_env, "DEEPSEEK_API_KEY");
        });
    }

    /// Security: custom base URL host must be on the allowlist.
    #[test]
    #[serial]
    fn custom_base_url_rejected_if_host_not_allowed() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://evil.example/v1")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(err.contains("not in the allowed"), "got: {err}");
        });
    }

    /// Provider inference from base URL: pointing BRIGID_LLM_BASE_URL at
    /// OpenRouter without an explicit provider correctly labels the provider.
    #[test]
    #[serial]
    fn infers_openrouter_from_base_url() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://openrouter.ai/api/v1")));
        vars.push(("BRIGID_LLM_MODEL", Some("openai/gpt-4o")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.provider, "openrouter");
            assert_eq!(cfg.api_key_env, "OPENROUTER_API_KEY");
        });
    }

    #[test]
    #[serial]
    fn infers_openai_from_base_url() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://api.openai.com/v1")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.provider, "openai");
            assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
        });
    }

    /// OpenAI/OpenRouter require an explicit model — no safe default.
    #[test]
    #[serial]
    fn openai_requires_explicit_model() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(err.contains("requires an explicit model"), "got: {err}");
        });
    }

    /// Model override takes precedence over env and preset defaults.
    #[test]
    #[serial]
    fn model_override_takes_precedence() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_MODEL", Some("env-model")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, Some("override-model"), &[]).unwrap();
            assert_eq!(cfg.model, "override-model");
        });
    }

    /// Provider override takes precedence over env.
    #[test]
    #[serial]
    fn provider_override_takes_precedence() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("deepseek")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(Some("openai"), None, &[]).unwrap();
            assert_eq!(cfg.provider, "openai");
            assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
        });
    }

    /// localhost is allowed for local LLM servers (e.g. Ollama), but
    /// requires BRIGID_LLM_API_KEY — a provider-specific key must never
    /// be sent to a local endpoint that could forward it anywhere.
    #[test]
    #[serial]
    fn localhost_requires_brigid_llm_api_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("http://localhost:11434/v1")));
        vars.push(("BRIGID_LLM_MODEL", Some("llama3")));
        vars.push(("BRIGID_LLM_API_KEY", Some("sk-local")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.base_url, "http://localhost:11434/v1");
            assert_eq!(cfg.api_key_env, "BRIGID_LLM_API_KEY");
        });
    }

    /// localhost without BRIGID_LLM_API_KEY is rejected — DEEPSEEK_API_KEY
    /// must not be sent to a local endpoint.
    #[test]
    #[serial]
    fn localhost_rejects_provider_specific_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("http://localhost:11434/v1")));
        vars.push(("BRIGID_LLM_MODEL", Some("llama3")));
        vars.push(("DEEPSEEK_API_KEY", Some("sk-deepseek")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(
                err.contains("BRIGID_LLM_API_KEY"),
                "should require BRIGID_LLM_API_KEY for localhost, got: {err}"
            );
        });
    }

    // --- Security: userinfo URL rejection ---

    /// `https://api.openai.com:443@evil.example/v1` must be rejected.
    /// The naive parser would extract `api.openai.com` (passes allowlist),
    /// but the actual host is `evil.example` (receives the API key).
    #[test]
    #[serial]
    fn userinfo_url_rejected_in_validate() {
        let err = validate_llm_base_url("https://api.openai.com:443@evil.example/v1").unwrap_err();
        assert!(
            err.to_string().contains("userinfo"),
            "should reject userinfo URL, got: {err}"
        );
    }

    #[test]
    #[serial]
    fn userinfo_url_rejected_in_resolve() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        vars.push((
            "BRIGID_LLM_BASE_URL",
            Some("https://api.openai.com:443@evil.example/v1"),
        ));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(
                err.contains("userinfo") || err.contains("failed to parse"),
                "should reject userinfo URL, got: {err}"
            );
        });
    }

    /// `https://api.openai.com@evil.example/v1` must also be rejected.
    #[test]
    #[serial]
    fn userinfo_url_without_port_rejected() {
        let err = validate_llm_base_url("https://api.openai.com@evil.example/v1").unwrap_err();
        assert!(
            err.to_string().contains("userinfo"),
            "should reject userinfo URL, got: {err}"
        );
    }

    // --- Security: provider-to-host mismatch ---

    /// `BRIGID_PROVIDER=openai` + `BRIGID_LLM_BASE_URL=https://openrouter.ai/...`
    /// must be rejected — OPENAI_API_KEY must not be sent to openrouter.ai.
    #[test]
    #[serial]
    fn openai_provider_rejects_openrouter_base_url() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://openrouter.ai/api/v1")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(
                err.contains("refusing to send"),
                "should reject provider-to-host mismatch, got: {err}"
            );
        });
    }

    /// `BRIGID_PROVIDER=deepseek` + `BRIGID_LLM_BASE_URL=https://api.openai.com/...`
    /// must be rejected — DEEPSEEK_API_KEY must not be sent to api.openai.com.
    #[test]
    #[serial]
    fn deepseek_provider_rejects_openai_base_url() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("deepseek")));
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://api.openai.com/v1")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(
                err.contains("refusing to send"),
                "should reject provider-to-host mismatch, got: {err}"
            );
        });
    }

    /// `BRIGID_PROVIDER=openrouter` + `BRIGID_LLM_BASE_URL=https://api.openai.com/...`
    /// must be rejected — OPENROUTER_API_KEY must not be sent to api.openai.com.
    #[test]
    #[serial]
    fn openrouter_provider_rejects_openai_base_url() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openrouter")));
        vars.push(("BRIGID_LLM_MODEL", Some("openai/gpt-4o")));
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://api.openai.com/v1")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(
                err.contains("refusing to send"),
                "should reject provider-to-host mismatch, got: {err}"
            );
        });
    }

    /// When BRIGID_LLM_API_KEY is set, a known provider with matching host
    /// still works (BRIGID_LLM_API_KEY takes precedence).
    #[test]
    #[serial]
    fn known_provider_with_brigid_llm_api_key() {
        let mut vars = clear_env();
        vars.push(("BRIGID_PROVIDER", Some("openai")));
        vars.push(("BRIGID_LLM_MODEL", Some("gpt-4o")));
        vars.push(("BRIGID_LLM_API_KEY", Some("sk-universal")));
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &[]).unwrap();
            assert_eq!(cfg.api_key_env, "BRIGID_LLM_API_KEY");
        });
    }

    // --- Security: HTTPS enforcement ---

    /// `http://api.deepseek.com/v1` must be rejected — cleartext to a
    /// non-loopback host would expose the API key.
    #[test]
    #[serial]
    fn http_rejected_for_non_loopback_host() {
        let err = validate_llm_base_url("http://api.deepseek.com/v1").unwrap_err();
        assert!(
            err.to_string().contains("requires https"),
            "should reject HTTP for non-loopback, got: {err}"
        );
    }

    /// `http://localhost:11434/v1` is allowed — loopback can use cleartext.
    #[test]
    #[serial]
    fn http_allowed_for_loopback() {
        assert!(validate_llm_base_url("http://localhost:11434/v1").is_ok());
        assert!(validate_llm_base_url("http://127.0.0.1:11434/v1").is_ok());
    }

    /// `https://api.deepseek.com/v1` is allowed.
    #[test]
    #[serial]
    fn https_allowed_for_non_loopback() {
        assert!(validate_llm_base_url("https://api.deepseek.com/v1").is_ok());
    }

    /// `ftp://api.deepseek.com/v1` must be rejected — non-HTTP(S) scheme.
    #[test]
    #[serial]
    fn ftp_scheme_rejected() {
        assert!(validate_llm_base_url("ftp://api.deepseek.com/v1").is_err());
    }

    /// HTTP to a non-loopback host is rejected in resolve_llm_config too.
    #[test]
    #[serial]
    fn resolve_rejects_http_for_known_provider() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("http://api.deepseek.com/v1")));
        with_env(&vars, || {
            let err = resolve_llm_config(None, None, &[]).unwrap_err();
            assert!(
                err.contains("requires https"),
                "should reject HTTP for non-loopback, got: {err}"
            );
        });
    }

    // --- Custom host allowlist ---

    /// A custom host not in the default allowlist is rejected by default.
    #[test]
    #[serial]
    fn custom_host_rejected_without_extra_hosts() {
        let err = validate_llm_base_url("https://my-proxy.internal/v1").unwrap_err();
        assert!(err.to_string().contains("not in the allowed"), "got: {err}");
    }

    /// A custom host passes when included in `extra_hosts`.
    #[test]
    #[serial]
    fn custom_host_allowed_with_extra_hosts() {
        let extra = vec!["my-proxy.internal".to_string()];
        assert!(validate_llm_base_url_with("https://my-proxy.internal/v1", &extra).is_ok());
    }

    /// A custom host in `extra_hosts` works end-to-end via `resolve_llm_config`.
    #[test]
    #[serial]
    fn resolve_allows_custom_host_with_extra_hosts() {
        let mut vars = clear_env();
        vars.push(("BRIGID_LLM_BASE_URL", Some("https://my-proxy.internal/v1")));
        vars.push(("BRIGID_LLM_MODEL", Some("custom-model")));
        vars.push(("BRIGID_LLM_API_KEY", Some("sk-custom")));
        let extra = vec!["my-proxy.internal".to_string()];
        with_env(&vars, || {
            let cfg = resolve_llm_config(None, None, &extra).unwrap();
            assert_eq!(cfg.base_url, "https://my-proxy.internal/v1");
            assert_eq!(cfg.api_key_env, "BRIGID_LLM_API_KEY");
        });
    }

    /// `extra_hosts` does not bypass HTTPS enforcement.
    #[test]
    #[serial]
    fn extra_hosts_does_not_bypass_https() {
        let extra = vec!["my-proxy.internal".to_string()];
        let err = validate_llm_base_url_with("http://my-proxy.internal/v1", &extra).unwrap_err();
        assert!(err.to_string().contains("requires https"), "got: {err}");
    }

    // --- CacheStats ---

    #[test]
    fn cache_stats_default_is_zero() {
        let stats = CacheStats::default();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.total(), 0);
        assert_eq!(stats.hit_rate_percent(), 0.0);
    }

    #[test]
    fn cache_stats_hit_rate() {
        let stats = CacheStats { hits: 3, misses: 1 };
        assert_eq!(stats.total(), 4);
        assert_eq!(stats.hit_rate_percent(), 75.0);
    }

    #[test]
    fn cache_stats_all_misses() {
        let stats = CacheStats { hits: 0, misses: 5 };
        assert_eq!(stats.total(), 5);
        assert_eq!(stats.hit_rate_percent(), 0.0);
    }

    #[test]
    fn cache_stats_all_hits() {
        let stats = CacheStats { hits: 5, misses: 0 };
        assert_eq!(stats.total(), 5);
        assert_eq!(stats.hit_rate_percent(), 100.0);
    }

    // --- StatsClient ---

    #[tokio::test]
    async fn stats_client_tracks_miss_then_hit() {
        use llm_kernel::llm::CacheClient;
        use llm_kernel::store::kv::SqliteKvStore;

        let store: Arc<SqliteKvStore> = Arc::new(SqliteKvStore::open_in_memory().unwrap());
        let store_for_client = Arc::clone(&store);

        // Build a CacheClient<MockClient> so the first call stores a response
        // and the second call serves it from cache.
        let mock = MockClient::new("cached response");
        let cached = CacheClient::new(mock, store_for_client as Arc<dyn KvStore>);

        // Wrap in StatsClient with the same store for hit/miss probing.
        let stats_client = StatsClient::new(cached, Some(store));
        let req = LLMRequest::builder().user_message("hello").build();

        // First call: miss (entry not in store yet, CacheClient fetches and stores).
        let _r1 = stats_client.complete(req.clone()).await.unwrap();
        let stats_after_first = stats_client.stats();
        assert_eq!(stats_after_first.misses, 1);
        assert_eq!(stats_after_first.hits, 0);

        // Second call: hit (entry now in store, CacheClient serves from cache).
        let _r2 = stats_client.complete(req).await.unwrap();
        let stats_after_second = stats_client.stats();
        assert_eq!(stats_after_second.misses, 1);
        assert_eq!(stats_after_second.hits, 1);
        assert_eq!(stats_after_second.hit_rate_percent(), 50.0);
    }

    #[tokio::test]
    async fn stats_client_without_store_tracks_nothing() {
        let mock = MockClient::new("response");
        let stats_client = StatsClient::new(mock, None);
        let req = LLMRequest::builder().user_message("hi").build();
        let _ = stats_client.complete(req).await.unwrap();
        let stats = stats_client.stats();
        assert_eq!(stats.total(), 0);
    }

    #[tokio::test]
    async fn stats_handle_survives_after_client_moved() {
        use llm_kernel::llm::CacheClient;
        use llm_kernel::store::kv::SqliteKvStore;

        let store: Arc<SqliteKvStore> = Arc::new(SqliteKvStore::open_in_memory().unwrap());
        let store_for_client = Arc::clone(&store);
        let mock = MockClient::new("cached response");
        let cached = CacheClient::new(mock, store_for_client as Arc<dyn KvStore>);
        let stats_client = StatsClient::new(cached, Some(store));

        // Get the handle before the client is consumed.
        let handle = stats_client.stats_handle();
        let req = LLMRequest::builder().user_message("hello").build();

        // Consume the client by completing a call.
        let _ = stats_client.complete(req.clone()).await.unwrap();
        let _ = stats_client.complete(req).await.unwrap();

        // The handle should still report the correct stats.
        let stats = handle.snapshot();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.hit_rate_percent(), 50.0);
    }

    #[test]
    fn cache_stats_handle_empty_is_zero() {
        let handle = CacheStatsHandle::empty();
        let stats = handle.snapshot();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.total(), 0);
    }

    #[tokio::test]
    async fn boxed_llm_client_delegates() {
        let mock = MockClient::new("boxed response");
        let boxed: Box<dyn LLMClient> = Box::new(mock);
        let wrapper = BoxedLlmClient::new(boxed);
        assert_eq!(wrapper.model_name(), "mock");
        let req = LLMRequest::builder().user_message("hi").build();
        let resp = wrapper.complete(req).await.unwrap();
        assert_eq!(resp.content, "boxed response");
    }

    #[tokio::test]
    async fn stats_client_with_boxed_client() {
        use llm_kernel::llm::CacheClient;
        use llm_kernel::store::kv::SqliteKvStore;

        let store: Arc<SqliteKvStore> = Arc::new(SqliteKvStore::open_in_memory().unwrap());
        let store_for_client = Arc::clone(&store);
        let mock = MockClient::new("boxed cached");
        let cached = CacheClient::new(mock, store_for_client as Arc<dyn KvStore>);
        let boxed: Box<dyn LLMClient> = Box::new(cached);
        let stats_client = StatsClient::new(BoxedLlmClient::new(boxed), Some(store));
        let req = LLMRequest::builder().user_message("hello").build();

        let _ = stats_client.complete(req.clone()).await.unwrap();
        let _ = stats_client.complete(req).await.unwrap();
        let stats = stats_client.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
    }
}
