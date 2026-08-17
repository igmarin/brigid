//! LLM provider clients and the `LlmClient` trait for `brigid`.
//!
//! # Deprecated
//!
//! This crate is deprecated as of brigid 2.0.0. New code should use
//! [`llm-kernel`](https://docs.rs/llm-kernel) (`LLMClient`, `CacheClient`,
//! `OpenAIClient`). The crate remains in the workspace until a follow-up
//! removal (issue #297 Phase 4). Bug fixes only — no new features.
//!
//! ---
//!
//! Defines a provider-agnostic `LlmClient` trait plus concrete
//! implementations (OpenAI-compatible first, per the project's provider
//! priority), retries/backoff, bounded concurrency, and disk response
//! caching keyed by `hash(prompt) + model + provider`.
//!
//! Milestone 2 delivers [`cache::DiskCache`] (no live network). Milestone 3
//! adds the [`LlmClient`] trait, the [`LlmError`] error enum, and the
//! [`MockClient`] test double. Milestone 4 adds the
//! [`OpenAiCompatibleClient`] HTTP client with retry/backoff/timeout and
//! optional disk caching.

#![deny(missing_docs)]
#![allow(deprecated)]

#[deprecated(since = "2.0.0", note = "use llm_kernel::llm::CacheClient instead")]
pub mod cache;
#[deprecated(since = "2.0.0", note = "use llm_kernel::llm::LLMClient instead")]
pub mod client;
#[deprecated(
    since = "2.0.0",
    note = "use brigid_pipeline::llm::bounded_complete or llm-kernel client wrappers"
)]
pub mod concurrency;
#[deprecated(since = "2.0.0", note = "use llm_kernel::error::KernelError instead")]
pub mod error;
#[deprecated(
    since = "2.0.0",
    note = "use brigid_pipeline::llm::MockClient (implements llm_kernel::llm::LLMClient)"
)]
pub mod mock;
#[deprecated(since = "2.0.0", note = "use llm_kernel::llm::OpenAIClient instead")]
pub mod openai_client;

#[deprecated(
    since = "2.0.0",
    note = "use llm_kernel::llm::{CacheClient, LLMClient} instead"
)]
pub use cache::{CacheError, CacheKeyInput, CacheStats, DiskCache, cache_key};
#[deprecated(since = "2.0.0", note = "use llm_kernel::llm::LLMClient instead")]
pub use client::LlmClient;
#[deprecated(
    since = "2.0.0",
    note = "use brigid_pipeline::llm::bounded_complete or llm-kernel client wrappers"
)]
pub use concurrency::{bounded_complete, bounded_complete_with_budget};
#[deprecated(since = "2.0.0", note = "use llm_kernel::error::KernelError instead")]
pub use error::LlmError;
#[deprecated(
    since = "2.0.0",
    note = "use brigid_pipeline::llm::MockClient (implements llm_kernel::llm::LLMClient)"
)]
pub use mock::MockClient;
#[deprecated(since = "2.0.0", note = "use llm_kernel::llm::OpenAIClient instead")]
pub use openai_client::{OpenAiClientConfig, OpenAiCompatibleClient, ProviderPreset};

/// The version of this crate, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
