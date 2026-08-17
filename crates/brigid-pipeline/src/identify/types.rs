use brigid_core::progress::BudgetExceeded;
use brigid_core::{AbstractionKind, Tier};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::checkpoint_store::CheckpointStoreError;
use crate::prompts::PromptError;
use brigid_core::ExtractError;
use crate::llm::LlmError;

// Doc-link only imports — these items are referenced in rustdoc intra-doc
// links but not used in code. `#[allow(unused_imports)]` suppresses the
// unused-import warning while keeping the links resolvable.
#[allow(unused_imports)]
use super::single_shot::identify_single_shot;
#[allow(unused_imports)]
use brigid_core::Abstraction;

/// Errors returned by [`identify_single_shot`].
#[derive(Debug, Error)]
pub enum IdentifyError {
    /// The prompt template failed to render (missing/invalid variable).
    #[error("prompt rendering failed: {0}")]
    Prompt(#[from] PromptError),
    /// The LLM call failed (network, timeout, rate limit, provider error).
    #[error("LLM call failed: {0}")]
    Llm(#[from] LlmError),
    /// No YAML/JSON block could be extracted from the LLM response.
    #[error("YAML/JSON block extraction failed: {0}")]
    Extract(#[from] ExtractError),
    /// The extracted YAML could not be parsed into a list of [`Abstraction`]s.
    #[error("failed to parse abstractions from LLM output: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
    /// An abstraction referenced a file index outside the crawl inventory.
    #[error("abstraction file index {index} out of range (have {total} files)")]
    FileIndexOutOfRange {
        /// The offending index.
        index: usize,
        /// Number of files in the inventory.
        total: usize,
    },
    /// The LLM returned no abstractions.
    #[error("no abstractions found in LLM output")]
    NoAbstractions,
    /// The configured LLM call budget was exceeded.
    #[error("budget exceeded: {0}")]
    Budget(#[from] BudgetExceeded),
    /// An LLM call failed for a specific map batch.
    ///
    /// We **fail closed**: rather than silently dropping the failed batch's
    /// candidates (which would produce an incomplete abstraction set), the
    /// entire `identify_map` call returns this error. The caller can retry
    /// the whole stage or surface the failure to the user.
    #[error("LLM call failed for batch {batch_idx}/{batch_total}: {error}")]
    LlmBatch {
        /// The 0-based batch index that failed.
        batch_idx: usize,
        /// The total number of batches in this map pass.
        batch_total: usize,
        /// The underlying LLM error.
        error: LlmError,
    },
    /// A checkpoint save/load failed during the identify stage.
    ///
    /// Wraps [`crate::CheckpointStoreError`] (and core
    /// `brigid_core::CheckpointError`) so the orchestration in
    /// [`crate::identify_checkpoint`] can propagate persistence failures
    /// without inventing a separate error enum.
    #[error("checkpoint error during identify: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
}

/// A candidate abstraction from the map stage, before reduce
/// deduplication/ranking.
///
/// Like [`Abstraction`] but carries an extra [`CandidateAbstraction::batch_idx`]
/// so the reduce stage (#71) can trace each candidate back to its originating
/// batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateAbstraction {
    /// Human-readable name (e.g. `"Query Processing"`).
    pub name: String,
    /// One-or-two sentence description of the concept.
    pub description: String,
    /// Indices into the **global** crawled file inventory backing this
    /// candidate. The map prompt instructs the LLM to use global indices from
    /// the file listing.
    pub file_indices: Vec<usize>,
    /// Complexity tier controlling tutorial depth and diagram requirements.
    /// Defaults to [`Tier::M`] when omitted from LLM output.
    #[serde(default)]
    pub tier: Tier,
    /// Free-form kind label (see [`AbstractionKind`]).
    /// Defaults to an empty string so plugins can enrich a missing kind.
    #[serde(default)]
    pub kind: AbstractionKind,
    /// Monorepo apps this candidate touches (empty for single-app repos).
    #[serde(default)]
    pub apps: Vec<String>,
    /// Best real paths to open first when studying this candidate.
    #[serde(default)]
    pub entry_files: Vec<String>,
    /// Which batch (0-based) this candidate came from.
    pub batch_idx: usize,
}

/// Results from one map batch.
#[derive(Clone, Debug)]
pub struct CandidateBatch {
    /// The 0-based batch index.
    pub batch_idx: usize,
    /// Candidate abstractions produced by this batch's LLM call.
    pub candidates: Vec<CandidateAbstraction>,
}
