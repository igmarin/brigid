//! llm-kernel adapters used by the pipeline.
//!
//! The pipeline talks to [`llm_kernel::llm::LLMClient`] (`complete(LLMRequest)`).
//! This module keeps a small prompt-shaped convenience layer so stage code can
//! still send a single user string, plus a brigid-specific [`MockClient`] that
//! implements the kernel trait.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use brigid_core::progress::BudgetExceeded;
use brigid_core::ProgressTracker;
use futures::future::join_all;
use llm_kernel::error::KernelError;
use llm_kernel::llm::{ChatMessage, LLMClient, LLMRequest, LLMResponse, LLMStream};
use tokio::sync::Semaphore;
use std::sync::Arc;
use thiserror::Error;

/// Alias for the kernel client trait used throughout the pipeline.
pub use llm_kernel::llm::LLMClient as LlmClient;

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
#[must_use]
pub fn host_from_base_url(base_url: &str) -> Option<String> {
    let rest = base_url.split_once("://")?.1;
    let hostport = rest.split('/').next()?;
    let host = hostport.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Reject a base URL whose host is not on the default provider allowlist.
///
/// # Errors
///
/// Returns [`LlmError::Network`] when the URL cannot be parsed or the host
/// is not allowlisted.
pub fn validate_llm_base_url(base_url: &str) -> Result<(), LlmError> {
    let host = host_from_base_url(base_url).ok_or_else(|| {
        LlmError::network(format!("failed to parse base_url host from '{base_url}'"))
    })?;
    if DEFAULT_ALLOWED_LLM_HOSTS.contains(&host.as_str()) {
        Ok(())
    } else {
        Err(LlmError::network(format!(
            "host '{host}' is not in the allowed hosts list; \
             refusing to send Authorization header to unapproved host"
        )))
    }
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
pub async fn complete_text(
    client: &dyn LLMClient,
    prompt: &str,
) -> Result<String, LlmError> {
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
            let _permit = sem.acquire_owned().await.map_err(|_| {
                LlmError::network("concurrency semaphore closed unexpectedly")
            })?;
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
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        assert!(
            err.to_string().contains("not in the allowed"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_llm_base_url_rejects_empty() {
        assert!(validate_llm_base_url("").is_err());
        assert!(validate_llm_base_url("not-a-url").is_err());
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
