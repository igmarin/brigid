//! A thread-safe, network-free mock LLM client for tests.
//!
//! [`MockClient`] implements [`crate::LlmClient`] and is designed for use in
//! downstream crate test suites as well as this crate's own tests. It can:
//!
//! - Return a single canned response for every call.
//! - Return a sequence of responses, advancing one per call.
//! - Fail on the Nth call with a specific [`crate::LlmError`].
//! - Track how many times [`crate::LlmClient::complete`] was called.
//!
//! All interior state is guarded by a [`std::sync::Mutex`], so `MockClient`
//! is `Send + Sync` and safe to share across async tasks. Mutex poisoning is
//! handled gracefully by recovering the inner data — a poisoned lock only
//! happens when a test panics while holding the lock, in which case the test
//! has already failed.

use crate::{LlmClient, LlmError};
use async_trait::async_trait;
use std::sync::{Mutex, MutexGuard};

/// Internal state for [`MockClient`], guarded by a `Mutex`.
#[derive(Debug)]
struct MockState {
    /// Responses to return in order. When exhausted, the last element is
    /// repeated forever (so single-response mode is just a one-element vec).
    responses: Vec<String>,
    /// Index of the next response to return.
    next: usize,
    /// Number of calls received so far.
    calls: usize,
    /// If set, fail the call at this 0-based index with the given error.
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

/// A network-free mock implementation of [`LlmClient`].
#[deprecated(
    since = "2.0.0",
    note = "use brigid_pipeline::llm::MockClient (implements llm_kernel::llm::LLMClient)"
)]
///
/// Construct with [`MockClient::new`] (single canned response) or
/// [`MockClient::with_responses`] (a sequence). Chain
/// [`MockClient::fail_on`] to inject a failure on a specific call.
///
/// # Examples
///
/// ```no_run
/// use brigid_llm::{LlmClient, MockClient};
///
/// # async fn run() {
/// let client = MockClient::new("hello");
/// assert_eq!(client.complete("hi").await.unwrap(), "hello");
/// # }
/// ```
pub struct MockClient {
    state: Mutex<MockState>,
}

/// Lock the mock's internal state, recovering from poisoning gracefully.
///
/// A poisoned mutex only occurs when a test panics while holding the lock,
/// which already fails the test. Recovering the inner data lets subsequent
/// assertions (e.g. `call_count`) still work in tear-down.
fn lock(state: &Mutex<MockState>) -> MutexGuard<'_, MockState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl MockClient {
    /// Create a mock that always returns `response` for every call.
    #[must_use]
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            state: Mutex::new(MockState::new(vec![response.into()])),
        }
    }

    /// Create a mock that returns `responses` in order, one per call.
    ///
    /// After the sequence is exhausted, the last response is repeated for
    /// any further calls.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Parse`] if `responses` is empty — a mock must
    /// have at least one response to return.
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

    /// Configure the mock to fail on the `call_index`-th call (0-based),
    /// returning `error` instead of a response.
    ///
    /// This consumes and returns `self` for builder-style chaining.
    #[must_use]
    pub fn fail_on(self, call_index: usize, error: LlmError) -> Self {
        {
            let mut state = lock(&self.state);
            state.fail_on = Some((call_index, error));
        }
        self
    }

    /// Number of times [`LlmClient::complete`] has been called so far.
    pub fn call_count(&self) -> usize {
        lock(&self.state).calls
    }

    /// Advance to the next response, repeating the last one if exhausted.
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
impl LlmClient for MockClient {
    async fn complete(&self, _prompt: &str) -> Result<String, LlmError> {
        // Collect everything we need in one short critical section, then
        // drop the lock before returning. No await is held across the lock.
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
            Some(err) => Err(err),
            None => Ok(response),
        }
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
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn returns_canned_response() {
        let client = MockClient::new("hello world");
        let out = client.complete("anything").await.unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn repeats_canned_response_on_multiple_calls() {
        let client = MockClient::new("same");
        assert_eq!(client.complete("a").await.unwrap(), "same");
        assert_eq!(client.complete("b").await.unwrap(), "same");
        assert_eq!(client.complete("c").await.unwrap(), "same");
        assert_eq!(client.call_count(), 3);
    }

    #[tokio::test]
    async fn returns_sequence_of_responses() {
        let client = MockClient::with_responses(vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ])
        .unwrap();
        assert_eq!(client.complete("1").await.unwrap(), "first");
        assert_eq!(client.complete("2").await.unwrap(), "second");
        assert_eq!(client.complete("3").await.unwrap(), "third");
        // After exhaustion, the last response repeats.
        assert_eq!(client.complete("4").await.unwrap(), "third");
        assert_eq!(client.call_count(), 4);
    }

    #[tokio::test]
    async fn with_responses_empty_returns_error() {
        let err = MockClient::with_responses(vec![]).unwrap_err();
        assert!(matches!(err, LlmError::Parse { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn fails_on_nth_call_with_configured_error() {
        let client = MockClient::new("ok").fail_on(1, LlmError::Timeout);
        // Call 0: ok
        assert_eq!(client.complete("a").await.unwrap(), "ok");
        // Call 1: fails with Timeout
        let err = client.complete("b").await.unwrap_err();
        assert!(matches!(err, LlmError::Timeout), "got: {err:?}");
        // Call 2: ok again
        assert_eq!(client.complete("c").await.unwrap(), "ok");
        assert_eq!(client.call_count(), 3);
    }

    #[tokio::test]
    async fn fails_on_first_call() {
        let client = MockClient::new("ok").fail_on(0, LlmError::network("boom"));
        let err = client.complete("x").await.unwrap_err();
        assert!(matches!(err, LlmError::Network { .. }), "got: {err:?}");
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn fail_on_with_rate_limit_preserves_retry_after() {
        let client = MockClient::new("ok").fail_on(
            0,
            LlmError::RateLimit {
                retry_after: Some(Duration::from_secs(2)),
            },
        );
        let err = client.complete("x").await.unwrap_err();
        match err {
            LlmError::RateLimit { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(2)));
            }
            other => panic!("expected RateLimit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fail_on_with_provider_error_preserves_status_and_body() {
        let client = MockClient::new("ok").fail_on(
            0,
            LlmError::Provider {
                status: 502,
                body: "bad gateway".to_string(),
            },
        );
        let err = client.complete("x").await.unwrap_err();
        match err {
            LlmError::Provider { status, body } => {
                assert_eq!(status, 502);
                assert_eq!(body, "bad gateway");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_count_starts_at_zero() {
        let client = MockClient::new("ok");
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn sequence_with_failure_interleaved() {
        let client =
            MockClient::with_responses(vec!["a".to_string(), "b".to_string(), "c".to_string()])
                .unwrap()
                .fail_on(1, LlmError::parse("bad json"));

        assert_eq!(client.complete("1").await.unwrap(), "a");
        let err = client.complete("2").await.unwrap_err();
        assert!(matches!(err, LlmError::Parse { .. }), "got: {err:?}");
        // After a failure, the sequence still advances.
        assert_eq!(client.complete("3").await.unwrap(), "c");
        assert_eq!(client.call_count(), 3);
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let client: Box<dyn LlmClient> = Box::new(MockClient::new("dyn"));
        let out = client.complete("hi").await.unwrap();
        assert_eq!(out, "dyn");
    }
}
