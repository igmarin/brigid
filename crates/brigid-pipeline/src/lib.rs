//! Stage orchestration, checkpoint/resume, and dry-run planning for `brigid`.
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
pub mod chapters;
pub mod checkpoint_store;
pub mod combine;
pub mod dry_run;
pub mod generate;
pub mod identify;
pub mod identify_checkpoint;
pub mod identify_runner;
pub mod llm;
pub mod order;
pub mod overview;
pub mod prompts;
pub mod relationships;
pub mod resume;
pub mod review;
pub mod setup_guide;

pub use cancellation::{CancelToken, setup_ctrl_c_handler};
pub use chapters::{
    ChaptersConfig, ChaptersError, DEFAULT_CHAPTER_MAX_FILE_CHARS, DEFAULT_CHAPTERS_BUDGET,
    DEFAULT_CHAPTERS_CONCURRENCY, DiagramLevel, chapters_and_checkpoint, count_mermaid_blocks,
    diagram_quota_for_tier, extract_chapter_summary, select_chapter_file_context,
    should_run_chapters, write_chapters, write_single_chapter,
};
pub use checkpoint_store::{CheckpointStore, CheckpointStoreError, records_from_files};
pub use combine::{
    CombineError, build_index_markdown, combine_and_checkpoint, combine_tutorial,
    slugify_chapter_filename, write_output_directory,
};
pub use dry_run::{DryRunError, DryRunPlan, dry_run, dry_run_with_budget, dry_run_with_options};
pub use generate::{
    EachAppOutcome, EachAppSummary, GenerateConfig, GenerateError, GenerateOutcome,
    run_chapters_stage, run_combine_stage, run_generate, run_generate_each_app, run_order_stage,
    run_overview_stage, run_relationships_stage, run_setup_stage,
};
pub use identify::{
    CandidateAbstraction, CandidateBatch, IdentifyError, IdentifyMapInput, IdentifyReduceInput,
    IdentifySingleShotInput, batch_files_by_size, identify_map, identify_reduce,
    identify_single_shot, incremental_identify,
};
pub use identify_checkpoint::{
    DEFAULT_MAX_ABSTRACTIONS, DEFAULT_MAX_CONCURRENCY, SINGLE_SHOT_FILE_THRESHOLD,
    SINGLE_SHOT_SIZE_THRESHOLD, identify_and_checkpoint, load_identify_result,
    save_identify_result, should_run_identify,
};
pub use identify_runner::{
    IdentifyRunConfig, IdentifyRunOutcome, IdentifyStrategy, identify_with_cancellation,
};
pub use llm::{
    BoxedLlmClient, CacheStats, CacheStatsHandle, LlmClient, LlmError, MockClient,
    ResolvedLlmConfig, StatsClient, bounded_complete, bounded_complete_with_budget,
    build_live_client, complete_text, resolve_llm_config,
};
pub use prompts::{PromptError, PromptId, PromptRenderer, sanitize_template_input};
pub use resume::{
    ResumeIdentityMismatch, check_identity, invalidate_from, is_checkpoint_stale, next_stage,
    pending_stages, should_run,
};
pub use review::{
    ReviewError, ReviewOutcome, ReviewSummary, review_chapter, review_chapters,
    validate_reviewed_chapter,
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
