//! Typed errors for LLM provider interactions.
//!
//! [`LlmError`] is the single error type returned by [`crate::LlmClient`]
//! implementations. It covers the failure modes that any provider client
//! (OpenAI-compatible, Anthropic, …) can produce without leaking
//! provider-specific transport types: network failures, timeouts, rate
//! limits, non-2xx provider responses, and response-parsing failures.
//!
//! Real HTTP clients land in a separate ticket (#66); this module only
//! defines the error surface so the trait and [`crate::MockClient`] can be
//! built and tested with zero network access.

use std::time::Duration;
use thiserror::Error;

/// Errors returned by [`crate::LlmClient`] implementations.
#[derive(Clone, Debug, Error)]
pub enum LlmError {
    /// Network or transport failure (DNS, connection refused, TLS, …).
    ///
    /// The message keeps the variant provider-agnostic — concrete clients
    /// wrap their transport error's `Display` here.
    #[error("network error: {message}")]
    Network {
        /// Human-readable description of the transport failure.
        message: String,
    },
    /// The request timed out before the provider responded.
    #[error("request timed out")]
    Timeout,
    /// The provider returned a 429 rate-limit response.
    ///
    /// `retry_after` carries the provider-advised wait time when present;
    /// callers read it programmatically to decide backoff. The `Display`
    /// rendering stays constant so log lines are greppable.
    #[error("rate limited")]
    RateLimit {
        /// Optional advised wait before retrying, if the provider supplied one.
        retry_after: Option<Duration>,
    },
    /// The provider returned a non-2xx status code (other than 429).
    #[error("provider error: status {status}: {body}")]
    Provider {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Raw response body. Real clients should truncate this to avoid leaking
        /// secrets or excessive log volume in error messages.
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
    /// Convenience constructor for [`LlmError::Network`] from any message.
    #[must_use]
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network {
            message: message.into(),
        }
    }

    /// Convenience constructor for [`LlmError::Parse`] from any message.
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse {
            message: message.into(),
        }
    }

    /// Return the rate-limit `retry_after` hint, if this is a
    /// [`LlmError::RateLimit`] carrying one. Returns `None` for all other
    /// variants (or when the provider omitted the hint).
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimit { retry_after } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_display_is_sensible() {
        let e = LlmError::network("connection refused");
        assert_eq!(e.to_string(), "network error: connection refused");
    }

    #[test]
    fn timeout_display_is_sensible() {
        let e = LlmError::Timeout;
        assert_eq!(e.to_string(), "request timed out");
    }

    #[test]
    fn rate_limit_display_without_retry_after() {
        let e = LlmError::RateLimit { retry_after: None };
        assert_eq!(e.to_string(), "rate limited");
    }

    #[test]
    fn rate_limit_display_with_retry_after() {
        let e = LlmError::RateLimit {
            retry_after: Some(Duration::from_millis(500)),
        };
        // Display stays constant; retry_after is read as a field.
        assert_eq!(e.to_string(), "rate limited");
        assert_eq!(
            e.retry_after(),
            Some(Duration::from_millis(500)),
            "retry_after should be accessible as a field",
        );
    }

    #[test]
    fn provider_display_is_sensible() {
        let e = LlmError::Provider {
            status: 503,
            body: "upstream down".to_string(),
        };
        assert_eq!(e.to_string(), "provider error: status 503: upstream down");
    }

    #[test]
    fn parse_display_is_sensible() {
        let e = LlmError::parse("missing choices field");
        assert_eq!(
            e.to_string(),
            "failed to parse provider response: missing choices field",
        );
    }
}
