//! Progress tracking and max-LLM-call budget (fail closed).
//!
//! Pure counters for operability before live LLM stages. Exceeding
//! the configured maximum returns [`BudgetExceeded`].
//!
//! Stage timings are recorded with [`std::time::Instant`] so the CLI can
//! report per-stage elapsed time in `--verbose` mode.

use std::time::{Duration, Instant};

use thiserror::Error;

/// Error when the LLM call ceiling is hit.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("LLM call budget exceeded: used {used} of max {max}")]
pub struct BudgetExceeded {
    /// Calls already recorded.
    pub used: u32,
    /// Configured maximum.
    pub max: u32,
}

/// Snapshot of progress for CLI/logging.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressSnapshot {
    /// LLM calls completed (or reserved) so far.
    pub llm_calls_used: u32,
    /// Hard ceiling.
    pub max_llm_calls: u32,
    /// Remaining calls before failure (`max - used`, saturating).
    pub llm_calls_remaining: u32,
    /// Human stage label currently running (if any).
    pub current_stage: Option<String>,
    /// Number of stages marked complete via [`ProgressTracker::complete_stage`].
    pub stages_completed: u32,
}

/// A completed stage's name, elapsed time, and LLM call count, for verbose
/// reporting and JSON output.
#[derive(Clone, Debug)]
pub struct StageTiming {
    /// Human-readable stage label (e.g. `"identify"`).
    pub stage: String,
    /// Elapsed wall-clock time for this stage.
    pub elapsed: Duration,
    /// Number of LLM calls made during this stage.
    pub llm_calls: u32,
}

/// Fail-closed LLM call budget and light progress state.
#[derive(Clone, Debug)]
pub struct ProgressTracker {
    max_llm_calls: u32,
    llm_calls_used: u32,
    current_stage: Option<String>,
    stages_completed: u32,
    stage_start: Option<Instant>,
    stage_llm_calls: u32,
    stage_timings: Vec<StageTiming>,
}

impl ProgressTracker {
    /// Create a tracker with the given hard ceiling (`0` means no calls allowed).
    #[must_use]
    pub fn new(max_llm_calls: u32) -> Self {
        Self {
            max_llm_calls,
            llm_calls_used: 0,
            current_stage: None,
            stages_completed: 0,
            stage_start: None,
            stage_llm_calls: 0,
            stage_timings: Vec::new(),
        }
    }

    /// Record one successful (or attempted) LLM call.
    ///
    /// # Errors
    ///
    /// [`BudgetExceeded`] when the next call would exceed `max_llm_calls`.
    pub fn record_llm_call(&mut self) -> Result<(), BudgetExceeded> {
        if self.llm_calls_used >= self.max_llm_calls {
            return Err(BudgetExceeded {
                used: self.llm_calls_used,
                max: self.max_llm_calls,
            });
        }
        self.llm_calls_used = self.llm_calls_used.saturating_add(1);
        if self.current_stage.is_some() {
            self.stage_llm_calls = self.stage_llm_calls.saturating_add(1);
        }
        Ok(())
    }

    /// Reserve `n` calls up front (e.g. map batch). Fails closed if not enough remain.
    ///
    /// # Errors
    ///
    /// [`BudgetExceeded`] when `used + n > max`.
    pub fn reserve_llm_calls(&mut self, n: u32) -> Result<(), BudgetExceeded> {
        let new_used = self.llm_calls_used.saturating_add(n);
        if new_used > self.max_llm_calls {
            return Err(BudgetExceeded {
                used: self.llm_calls_used,
                max: self.max_llm_calls,
            });
        }
        self.llm_calls_used = new_used;
        if self.current_stage.is_some() {
            self.stage_llm_calls = self.stage_llm_calls.saturating_add(n);
        }
        Ok(())
    }

    /// Set the human-readable current stage label.
    ///
    /// If a stage is already running, its elapsed time is recorded before
    /// switching to the new stage.
    pub fn set_stage(&mut self, stage: impl Into<String>) {
        self.record_stage_timing();
        self.current_stage = Some(stage.into());
        self.stage_start = Some(Instant::now());
        self.stage_llm_calls = 0;
    }

    /// Clear current stage and increment completed stage count.
    ///
    /// The elapsed time for the current stage is recorded for verbose
    /// reporting via [`ProgressTracker::stage_timings`].
    pub fn complete_stage(&mut self) {
        self.record_stage_timing();
        self.current_stage = None;
        self.stages_completed = self.stages_completed.saturating_add(1);
    }

    /// Record the elapsed time for the current stage (if any) into the
    /// timings list and clear the start instant.
    fn record_stage_timing(&mut self) {
        if let (Some(name), Some(start)) = (self.current_stage.as_ref(), self.stage_start.take()) {
            self.stage_timings.push(StageTiming {
                stage: name.clone(),
                elapsed: start.elapsed(),
                llm_calls: self.stage_llm_calls,
            });
        }
        self.stage_llm_calls = 0;
    }

    /// Return per-stage elapsed timings recorded so far.
    ///
    /// Only stages that were started via [`set_stage`](Self::set_stage) and
    /// then completed or switched away from via
    /// [`complete_stage`](Self::complete_stage) or another `set_stage` call
    /// appear here.
    #[must_use]
    pub fn stage_timings(&self) -> &[StageTiming] {
        &self.stage_timings
    }

    /// Number of LLM calls already reserved/used.
    #[must_use]
    pub fn llm_calls_used(&self) -> u32 {
        self.llm_calls_used
    }

    /// Create a tracker for a new run/stage that accounts for calls already
    /// consumed in `checkpoint`.
    ///
    /// `max_llm_calls == None` falls back to [`DEFAULT_MAX_LLM_CALLS`]. The
    /// remaining budget is `max - used`, saturating at zero.
    #[must_use]
    pub fn from_config_and_checkpoint(
        max_llm_calls: Option<u32>,
        checkpoint: &crate::CheckpointV1,
    ) -> Self {
        let max = max_llm_calls.unwrap_or(crate::DEFAULT_MAX_LLM_CALLS);
        Self::new(max.saturating_sub(checkpoint.metadata.llm_calls_used))
    }

    /// Immutable snapshot for display / tests.
    #[must_use]
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            llm_calls_used: self.llm_calls_used,
            max_llm_calls: self.max_llm_calls,
            llm_calls_remaining: self.max_llm_calls.saturating_sub(self.llm_calls_used),
            current_stage: self.current_stage.clone(),
            stages_completed: self.stages_completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckpointV1, DEFAULT_MAX_LLM_CALLS, RunConfig};

    #[test]
    fn records_until_ceiling() {
        let mut t = ProgressTracker::new(2);
        t.record_llm_call().unwrap();
        t.record_llm_call().unwrap();
        let err = t.record_llm_call().unwrap_err();
        assert_eq!(err.used, 2);
        assert_eq!(err.max, 2);
        assert_eq!(t.snapshot().llm_calls_remaining, 0);
    }

    #[test]
    fn zero_max_fails_immediately() {
        let mut t = ProgressTracker::new(0);
        assert!(t.record_llm_call().is_err());
    }

    #[test]
    fn reserve_batch() {
        let mut t = ProgressTracker::new(5);
        t.reserve_llm_calls(3).unwrap();
        assert_eq!(t.snapshot().llm_calls_used, 3);
        assert!(t.reserve_llm_calls(3).is_err());
        t.reserve_llm_calls(2).unwrap();
        assert_eq!(t.snapshot().llm_calls_used, 5);
    }

    #[test]
    fn stage_tracking() {
        let mut t = ProgressTracker::new(10);
        t.set_stage("identify");
        assert_eq!(t.snapshot().current_stage.as_deref(), Some("identify"));
        t.complete_stage();
        assert!(t.snapshot().current_stage.is_none());
        assert_eq!(t.snapshot().stages_completed, 1);
    }

    #[test]
    fn stage_llm_calls_tracked_per_stage() {
        let mut t = ProgressTracker::new(20);
        t.set_stage("identify");
        t.record_llm_call().unwrap();
        t.record_llm_call().unwrap();
        t.complete_stage();
        t.set_stage("relationships");
        t.record_llm_call().unwrap();
        t.complete_stage();
        let timings = t.stage_timings();
        assert_eq!(timings.len(), 2);
        assert_eq!(timings[0].stage, "identify");
        assert_eq!(timings[0].llm_calls, 2);
        assert_eq!(timings[1].stage, "relationships");
        assert_eq!(timings[1].llm_calls, 1);
        assert_eq!(t.snapshot().llm_calls_used, 3);
    }

    #[test]
    fn stage_llm_calls_with_reserve() {
        let mut t = ProgressTracker::new(20);
        t.set_stage("chapters");
        t.reserve_llm_calls(5).unwrap();
        t.complete_stage();
        let timings = t.stage_timings();
        assert_eq!(timings[0].llm_calls, 5);
    }

    #[test]
    fn stage_llm_calls_zero_when_no_calls() {
        let mut t = ProgressTracker::new(20);
        t.set_stage("order");
        t.complete_stage();
        let timings = t.stage_timings();
        assert_eq!(timings[0].llm_calls, 0);
    }

    #[test]
    fn llm_calls_outside_stage_not_counted() {
        let mut t = ProgressTracker::new(20);
        t.record_llm_call().unwrap();
        t.set_stage("identify");
        t.record_llm_call().unwrap();
        t.complete_stage();
        let timings = t.stage_timings();
        assert_eq!(timings[0].llm_calls, 1);
        assert_eq!(t.snapshot().llm_calls_used, 2);
    }

    #[test]
    fn from_config_and_checkpoint_defaults_to_max_llm_calls() {
        let cfg = RunConfig::empty();
        let cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "0Z").unwrap();
        let t = ProgressTracker::from_config_and_checkpoint(None, &cp);
        assert_eq!(t.snapshot().llm_calls_remaining, DEFAULT_MAX_LLM_CALLS);
    }

    #[test]
    fn from_config_and_checkpoint_subtracts_used_calls() {
        let cfg = RunConfig::empty();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "0Z").unwrap();
        cp.record_llm_calls(10);
        let t = ProgressTracker::from_config_and_checkpoint(Some(50), &cp);
        assert_eq!(t.snapshot().llm_calls_remaining, 40);
    }

    #[test]
    fn from_config_and_checkpoint_saturates_at_zero() {
        let cfg = RunConfig::empty();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "0Z").unwrap();
        cp.record_llm_calls(100);
        let t = ProgressTracker::from_config_and_checkpoint(Some(50), &cp);
        assert_eq!(t.snapshot().llm_calls_remaining, 0);
    }

    #[test]
    fn from_config_and_checkpoint_used_equals_max_yields_zero() {
        let cfg = RunConfig::empty();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "0Z").unwrap();
        cp.record_llm_calls(50);
        let t = ProgressTracker::from_config_and_checkpoint(Some(50), &cp);
        assert_eq!(t.snapshot().llm_calls_remaining, 0);
    }
}
