//! Bounded concurrency for batched LLM calls.
//!
//! When a pipeline stage needs to call [`crate::LlmClient::complete`] on many
//! prompts (e.g. a map batch), running all calls at once can exhaust rate
//! limits or connection pools. The functions in this module use a
//! [`tokio::sync::Semaphore`] to cap the number of concurrent in-flight
//! requests.
//!
//! - [`bounded_complete`] runs N completion calls with at most
//!   `max_concurrency` calls in flight. Results are returned in input order;
//!   a failure in one call does not cancel the others.
//! - [`bounded_complete_with_budget`] additionally enforces the
//!   [`brigid_core::ProgressTracker`] LLM-call budget before fanning out.

use crate::{LlmClient, LlmError};
use brigid_core::{BudgetExceeded, ProgressTracker};
use futures::future::join_all;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Run LLM completion calls with bounded concurrency.
///
/// Each prompt is sent to `client.complete()` with at most `max_concurrency`
/// calls in flight at once. Results are returned in the same order as the
/// input `prompts`. A failure in one call does **not** abort the others — all
/// calls are attempted, and errors are returned in the corresponding position.
///
/// If `max_concurrency` is `0` it is treated as `1` (at least one call at a
/// time). An empty `prompts` vec returns an empty result vec immediately.
///
/// # Concurrency model
///
/// A [`tokio::sync::Semaphore`] with `max_concurrency` permits is created.
/// Each call acquires a permit before invoking `complete` and releases it when
/// done. Calls that cannot immediately acquire a permit wait until one is
/// freed.
pub async fn bounded_complete(
    client: &dyn LlmClient,
    prompts: Vec<String>,
    max_concurrency: usize,
) -> Vec<Result<String, LlmError>> {
    let n = prompts.len();
    if n == 0 {
        return Vec::new();
    }

    // A semaphore with 0 permits would deadlock; treat 0 as 1.
    let max = max_concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(max));

    // Build one future per prompt. Each acquires a permit before calling
    // `complete`, so at most `max` calls are in flight simultaneously.
    // `join_all` preserves input order in the output vector.
    let futures = prompts.into_iter().map(|prompt| {
        let sem = Arc::clone(&semaphore);
        async move {
            // The semaphore is never closed, so acquisition cannot fail in
            // practice. We still handle the error gracefully rather than
            // panicking, since this is library code.
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|_| LlmError::network("concurrency semaphore closed unexpectedly"))?;
            client.complete(&prompt).await
        }
    });

    join_all(futures).await
}

/// Convert a `usize` prompt count to `u32`, returning [`BudgetExceeded`]
/// when the count exceeds `u32::MAX` (which would silently overflow the
/// budget tracker).
///
/// This is extracted as a separate function so the overflow path can be
/// unit-tested without allocating a multi-billion-element vector.
fn prompt_count(len: usize) -> Result<u32, BudgetExceeded> {
    u32::try_from(len).map_err(|_| BudgetExceeded {
        used: u32::MAX,
        max: u32::MAX,
    })
}

/// Run LLM completion calls with bounded concurrency and budget enforcement.
///
/// Before fanning out, this calls
/// [`ProgressTracker::reserve_llm_calls`] with `prompts.len()`. If the budget
/// is exceeded, a single [`BudgetExceeded`] error is returned immediately
/// (no calls are made). Otherwise the calls run with bounded concurrency via
/// [`bounded_complete`].
///
/// Reserving up front both checks the budget *and* increments the used count,
/// so callers should not additionally call [`ProgressTracker::record_llm_call`]
/// for the same batch — that would double-count.
///
/// # Errors
///
/// Returns [`BudgetExceeded`] when `prompts.len()` would exceed the remaining
/// budget, or when `prompts.len()` exceeds `u32::MAX` (the budget tracker's
/// counter width).
pub async fn bounded_complete_with_budget(
    client: &dyn LlmClient,
    prompts: Vec<String>,
    max_concurrency: usize,
    progress: &mut ProgressTracker,
) -> Result<Vec<Result<String, LlmError>>, BudgetExceeded> {
    let n = prompt_count(prompts.len())?;
    progress.reserve_llm_calls(n)?;
    Ok(bounded_complete(client, prompts, max_concurrency).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockClient;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // ------------------------------------------------------------------
    // Test helper: a custom LlmClient that tracks concurrent calls.
    // ------------------------------------------------------------------

    /// Inner state for the concurrency-tracking mock.
    struct TrackerInner {
        current: AtomicUsize,
        max_seen: AtomicUsize,
    }

    /// A mock [`LlmClient`] that tracks how many calls are in flight
    /// simultaneously, sleeping briefly so that concurrency is observable.
    struct ConcurrencyTracker {
        inner: Arc<TrackerInner>,
        delay: Duration,
    }

    impl ConcurrencyTracker {
        fn new(delay: Duration) -> Self {
            Self {
                inner: Arc::new(TrackerInner {
                    current: AtomicUsize::new(0),
                    max_seen: AtomicUsize::new(0),
                }),
                delay,
            }
        }

        /// The maximum number of concurrent in-flight calls observed.
        fn max_concurrent(&self) -> usize {
            self.inner.max_seen.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmClient for ConcurrencyTracker {
        async fn complete(&self, _prompt: &str) -> Result<String, LlmError> {
            let current = self.inner.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.inner.max_seen.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.inner.current.fetch_sub(1, Ordering::SeqCst);
            Ok("ok".to_string())
        }
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn basic_three_prompts_concurrency_two() {
        let client =
            MockClient::with_responses(vec!["a".to_string(), "b".to_string(), "c".to_string()])
                .unwrap();
        let prompts = vec!["p1".to_string(), "p2".to_string(), "p3".to_string()];
        let results = bounded_complete(&client, prompts, 2).await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap(), "a");
        assert_eq!(results[1].as_ref().unwrap(), "b");
        assert_eq!(results[2].as_ref().unwrap(), "c");
    }

    #[tokio::test]
    async fn concurrency_limit_respected() {
        // 5 prompts, concurrency=2. The tracker sleeps so calls overlap.
        let tracker = ConcurrencyTracker::new(Duration::from_millis(50));
        let prompts: Vec<String> = (0..5).map(|i| format!("p{i}")).collect();
        let results = bounded_complete(&tracker, prompts, 2).await;
        assert_eq!(results.len(), 5);
        for r in &results {
            assert!(r.is_ok(), "all calls should succeed");
        }
        assert_eq!(
            tracker.max_concurrent(),
            2,
            "max concurrent calls should not exceed the limit"
        );
    }

    #[tokio::test]
    async fn all_succeed_concurrency_four() {
        let client = MockClient::new("ok");
        let prompts: Vec<String> = (0..4).map(|i| format!("p{i}")).collect();
        let results = bounded_complete(&client, prompts, 4).await;
        assert_eq!(results.len(), 4);
        for r in &results {
            assert_eq!(r.as_ref().unwrap(), "ok");
        }
    }

    #[tokio::test]
    async fn one_failure_does_not_abort_others() {
        let client =
            MockClient::with_responses(vec!["a".to_string(), "b".to_string(), "c".to_string()])
                .unwrap()
                .fail_on(1, LlmError::Timeout);
        let prompts = vec!["p1".to_string(), "p2".to_string(), "p3".to_string()];
        let results = bounded_complete(&client, prompts, 4).await;
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert_eq!(results[0].as_ref().unwrap(), "a");
        assert!(
            matches!(results[1], Err(LlmError::Timeout)),
            "index 1 should be Timeout"
        );
        assert!(results[2].is_ok());
        assert_eq!(results[2].as_ref().unwrap(), "c");
    }

    #[tokio::test]
    async fn empty_prompts_returns_empty() {
        let client = MockClient::new("ok");
        let results = bounded_complete(&client, vec![], 4).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn single_prompt_succeeds() {
        let client = MockClient::new("solo");
        let results = bounded_complete(&client, vec!["only".to_string()], 4).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap(), "solo");
    }

    #[tokio::test]
    async fn max_concurrency_zero_treated_as_one() {
        let tracker = ConcurrencyTracker::new(Duration::from_millis(50));
        let prompts: Vec<String> = (0..3).map(|i| format!("p{i}")).collect();
        let results = bounded_complete(&tracker, prompts, 0).await;
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.is_ok());
        }
        // With max_concurrency=0 treated as 1, at most 1 concurrent call.
        assert_eq!(
            tracker.max_concurrent(),
            1,
            "max_concurrency=0 should be treated as 1"
        );
    }

    // ------------------------------------------------------------------
    // Budget tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn budget_exceeded_returns_error() {
        let client = MockClient::new("ok");
        let prompts: Vec<String> = (0..3).map(|i| format!("p{i}")).collect();
        let mut progress = ProgressTracker::new(2); // only 2 calls allowed
        let result = bounded_complete_with_budget(&client, prompts, 4, &mut progress).await;
        assert!(result.is_err(), "should return BudgetExceeded");
        let err = result.unwrap_err();
        assert_eq!(err.used, 0);
        assert_eq!(err.max, 2);
        // No calls should have been made.
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn budget_ok_runs_calls_and_reserves() {
        let client = MockClient::new("ok");
        let prompts: Vec<String> = (0..3).map(|i| format!("p{i}")).collect();
        let mut progress = ProgressTracker::new(10);
        let results = bounded_complete_with_budget(&client, prompts, 4, &mut progress)
            .await
            .expect("budget should allow 3 calls");
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.is_ok());
        }
        // reserve_llm_calls should have incremented used by 3.
        assert_eq!(progress.snapshot().llm_calls_used, 3);
        assert_eq!(client.call_count(), 3);
    }

    #[tokio::test]
    async fn budget_empty_prompts_succeeds() {
        let client = MockClient::new("ok");
        let mut progress = ProgressTracker::new(0); // even with 0 budget
        let results = bounded_complete_with_budget(&client, vec![], 4, &mut progress)
            .await
            .expect("empty batch should not exceed budget");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn budget_exact_fit_succeeds() {
        let client = MockClient::new("ok");
        let prompts: Vec<String> = (0..5).map(|i| format!("p{i}")).collect();
        let mut progress = ProgressTracker::new(5); // exactly enough
        let results = bounded_complete_with_budget(&client, prompts, 2, &mut progress)
            .await
            .expect("exact fit should succeed");
        assert_eq!(results.len(), 5);
        assert_eq!(progress.snapshot().llm_calls_used, 5);
        assert_eq!(progress.snapshot().llm_calls_remaining, 0);
    }

    // ------------------------------------------------------------------
    // Issue #212: overflow in prompt count must return an error
    // ------------------------------------------------------------------

    /// When the prompt count exceeds `u32::MAX`, `prompt_count` must return
    /// a [`BudgetExceeded`] error rather than silently clamping to
    /// `u32::MAX` (which would allow exceeding the LLM call budget).
    ///
    /// This test runs only on 64-bit platforms where `usize` can exceed
    /// `u32::MAX`.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn prompt_count_overflow_returns_budget_exceeded() {
        let overflow_len = u32::MAX as usize + 1;
        let result = prompt_count(overflow_len);
        assert!(result.is_err(), "overflow should return Err");
        let err = result.unwrap_err();
        assert_eq!(err.used, u32::MAX);
        assert_eq!(err.max, u32::MAX);
    }

    /// On any platform, a count within `u32::MAX` must convert successfully.
    #[test]
    fn prompt_count_within_range_succeeds() {
        assert_eq!(prompt_count(0).unwrap(), 0);
        assert_eq!(prompt_count(42).unwrap(), 42);
        assert_eq!(prompt_count(u32::MAX as usize).unwrap(), u32::MAX);
    }
}
