//! LLM provider clients and the `LlmClient` trait for `brigid`.
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

pub mod cache;
pub mod client;
pub mod concurrency;
pub mod error;
pub mod mock;
pub mod openai_client;

pub use cache::{CacheError, CacheKeyInput, CacheStats, DiskCache, cache_key};
pub use client::LlmClient;
pub use concurrency::{bounded_complete, bounded_complete_with_budget};
pub use error::LlmError;
pub use mock::MockClient;
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
