//! The provider-agnostic `LlmClient` trait.
//!
//! Every concrete provider client (OpenAI-compatible, Anthropic, …) implements
//! [`LlmClient`] so the pipeline can talk to any provider behind a single
//! object-safe interface. The trait is intentionally minimal — just
//! [`LlmClient::complete`] — streaming, tools, and structured output land in
//! later milestones.
//!
//! `async-trait` is used so the trait remains object-safe and `dyn LlmClient`
//! works for dependency injection in tests ([`crate::MockClient`]) and in the
//! pipeline. The `Send + Sync` supertrait is required for bounded concurrency
//! with tokio downstream.

use crate::LlmError;
use async_trait::async_trait;

/// Provider-agnostic LLM completion client.
///
/// Implementations are expected to be cheap to clone (wrap an HTTP client
/// handle) and safe to share across tasks (`Send + Sync`).
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Complete a prompt, returning the full response text.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError`] on network failure, timeout, rate limit,
    /// non-2xx provider response, or response-parsing failure.
    async fn complete(&self, prompt: &str) -> Result<String, LlmError>;
}
