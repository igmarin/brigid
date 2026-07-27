//! Graceful cancellation for pipeline stages (issue #68).
//!
//! When the user presses Ctrl+C (or the process receives SIGTERM), we want
//! long-running stages to **stop dispatching new work**, let in-flight LLM
//! calls finish, and then write a checkpoint with whatever progress was made
//! so far. The process exits with code **5** to signal "partial success with
//! checkpoint" — distinct from `0` (full success) and `2` (config / input
//! error).
//!
//! # Exit-code semantics
//!
//! | Code | Meaning |
//! |------|---------|
//! | `0`  | Stage completed fully; checkpoint marks the stage done. |
//! | `5`  | Stage was cancelled mid-flight; a **partial** checkpoint was written and the stage is **not** marked complete. Resume to continue. |
//! | `2`  | Config / path / I/O input error (no checkpoint written). |
//! | `1`  | Generic failure (LLM error, parse error, etc.). |
//!
//! The [`CancelToken`] type wraps [`tokio_util::sync::CancellationToken`] so
//! the rest of the pipeline does not depend on `tokio-util` directly. A
//! cancelled token propagates to child tokens, so a single root token can
//! gate an entire stage tree.

use std::future::Future;

/// Cancellation token for graceful shutdown of pipeline stages.
///
/// On Ctrl+C (or SIGTERM), the token is cancelled, signaling long-running
/// stages to stop dispatching new work and save a checkpoint with whatever
/// progress was made so far.
///
/// Create a root token with [`CancelToken::new`], spawn the signal handler via
/// [`setup_ctrl_c_handler`], and pass the token (or a [`CancelToken::child`])
/// into stage runners like [`identify_with_cancellation`].
///
/// [`identify_with_cancellation`]: crate::identify_runner::identify_with_cancellation
#[derive(Clone, Debug)]
pub struct CancelToken {
    /// The underlying tokio-util token.
    inner: tokio_util::sync::CancellationToken,
}

impl CancelToken {
    /// Create a new, un-cancelled root token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Cancel the token (and all children). Idempotent.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Whether the token has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Returns a future that completes when the token is cancelled.
    ///
    /// The future is `Send` so it can be awaited inside `tokio::select!` or
    /// joined with other tasks.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send {
        self.inner.cancelled()
    }

    /// Create a child token that is cancelled when this token is cancelled.
    ///
    /// Cancelling the child does **not** cancel the parent.
    #[must_use]
    pub fn child(&self) -> CancelToken {
        CancelToken {
            inner: self.inner.child_token(),
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Set up a Ctrl+C (and, on Unix, SIGTERM) listener that cancels the returned
/// [`CancelToken`] when either signal arrives.
///
/// Spawns a background tokio task that awaits `tokio::signal::ctrl_c()` (and
/// `unix::signal(SIGTERM)` on Unix). When a signal is received the token is
/// cancelled. The task exits after the first signal.
///
/// This must be called from within a tokio runtime context (the CLI's
/// `#[tokio::main]` or a `Runtime::block_on`).
///
/// # Errors
///
/// Returns an error string if the signal handler could not be installed
/// (e.g. the runtime is not available or signal registration fails).
pub fn setup_ctrl_c_handler() -> Result<CancelToken, String> {
    let token = CancelToken::new();
    let cancel = token.clone();
    // Spawn the listener task. We use a separate clone for the SIGTERM branch
    // so both paths cancel the same root token.
    let cancel_term = token.clone();
    tokio::spawn(async move {
        // Wait for Ctrl+C. Ignore the error (e.g. signal not supported) —
        // the token simply never fires in that case.
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    });

    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            // SIGTERM is the default signal sent by `kill` and container
            // orchestrators. We best-effort install it; if registration
            // fails we simply rely on Ctrl+C alone.
            if let Ok(mut term) = signal(SignalKind::terminate()) {
                term.recv().await;
                cancel_term.cancel();
            }
        });
    }
    // Keep the compiler happy on non-unix: the clones above are consumed by
    // the spawned tasks on unix; on non-unix `cancel_term` is unused.
    #[cfg(not(unix))]
    {
        let _ = cancel_term;
    }

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_token_is_not_cancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_makes_is_cancelled_true() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_future_completes_after_cancel() {
        let token = CancelToken::new();
        // Not cancelled yet — the future should not complete immediately.
        let fut = token.cancelled();
        tokio::pin!(fut);
        // Race against a short timeout: the future should still be pending.
        let pending = tokio::time::timeout(std::time::Duration::from_millis(10), &mut fut).await;
        assert!(pending.is_err(), "future should not complete before cancel");

        token.cancel();
        // Now the future should complete promptly.
        let done = tokio::time::timeout(std::time::Duration::from_millis(100), fut).await;
        assert!(done.is_ok(), "future should complete after cancel");
    }

    #[tokio::test]
    async fn already_cancelled_future_completes_immediately() {
        let token = CancelToken::new();
        token.cancel();
        let done =
            tokio::time::timeout(std::time::Duration::from_millis(100), token.cancelled()).await;
        assert!(done.is_ok(), "cancelled future should complete immediately");
    }

    #[test]
    fn child_is_cancelled_when_parent_cancelled() {
        let parent = CancelToken::new();
        let child = parent.child();
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(
            child.is_cancelled(),
            "child should be cancelled with parent"
        );
    }

    #[test]
    fn cancelling_child_does_not_cancel_parent() {
        let parent = CancelToken::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "cancelling child must not cancel parent"
        );
    }

    #[test]
    fn grandchild_is_cancelled_with_grandparent() {
        let root = CancelToken::new();
        let child = root.child();
        let grandchild = child.child();
        root.cancel();
        assert!(grandchild.is_cancelled());
    }

    #[test]
    fn default_is_not_cancelled() {
        let token = CancelToken::default();
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn setup_ctrl_c_handler_returns_token() {
        // We cannot easily send a real Ctrl+C in a unit test, but we can
        // verify the handler returns a usable (un-cancelled) token.
        let token = setup_ctrl_c_handler().expect("handler should install");
        assert!(!token.is_cancelled());
        // Manually cancelling the returned token works normally.
        token.cancel();
        assert!(token.is_cancelled());
    }
}
