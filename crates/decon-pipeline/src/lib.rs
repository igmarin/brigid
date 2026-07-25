//! Stage orchestration, checkpoint/resume, and dry-run planning for `decon`.
//!
//! The Python reference implementation's Pocket Flow graph becomes an
//! explicit `Pipeline` state machine here — clearer than a generic node
//! framework for this linear workflow (fetch → identify → relationships →
//! order → chapters → setup → overview → combine). Checkpoint format follows
//! ADR 0001 (content-addressed manifest, not a monolithic JSON blob).
//!
//! Milestone 1 delivers [`dry_run::dry_run`]; Milestone 2 adds
//! [`checkpoint_store::CheckpointStore`]. Later milestones add LLM stages —
//! see `docs/move-to-rust.md` §2.2 and §5.

#![deny(missing_docs)]

pub mod cancellation;
pub mod checkpoint_store;
pub mod dry_run;
pub mod identify;
pub mod identify_checkpoint;
pub mod identify_runner;
pub mod prompts;
pub mod resume;
pub mod setup_guide;

pub use cancellation::{CancelToken, setup_ctrl_c_handler};
pub use checkpoint_store::{CheckpointStore, CheckpointStoreError, records_from_files};
pub use dry_run::{DryRunError, DryRunPlan, dry_run, dry_run_with_budget};
pub use identify::{
    CandidateAbstraction, CandidateBatch, IdentifyError, IdentifyMapInput, IdentifyReduceInput,
    IdentifySingleShotInput, identify_map, identify_reduce, identify_single_shot,
};
pub use identify_checkpoint::{
    DEFAULT_MAX_ABSTRACTIONS, DEFAULT_MAX_CONCURRENCY, SINGLE_SHOT_FILE_THRESHOLD,
    SINGLE_SHOT_SIZE_THRESHOLD, identify_and_checkpoint, load_identify_result,
    save_identify_result, should_run_identify,
};
pub use identify_runner::{
    IdentifyRunConfig, IdentifyRunOutcome, IdentifyStrategy, identify_with_cancellation,
};
pub use prompts::{PromptError, PromptId, PromptRenderer, sanitize_template_input};
pub use resume::{
    ResumeIdentityMismatch, check_identity, invalidate_from, next_stage, pending_stages, should_run,
};

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
