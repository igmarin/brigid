//! Single-shot **identify** stage: one LLM call produces the full abstraction
//! list for small repos where map/reduce is unnecessary.
//!
//! This is the Rust port of the Python reference's `_single_shot_identify`
//! node. The function takes a [`decon_llm::LlmClient`] (so it works with
//! [`decon_llm::MockClient`] in tests and a real provider client in
//! production), renders the `identify_single_shot` prompt, calls the LLM,
//! extracts the YAML block, parses it into [`Abstraction`]s, and validates the
//! `file_indices` against the crawl inventory.
//!
//! Caching is intentionally NOT handled here — the caller (or a later ticket)
//! wraps the LLM call with [`decon_llm::DiskCache`]. Likewise, heuristic
//! enrichment of `tier`/`kind`/`apps`/`entry_files` beyond what the LLM
//! returns is a separate concern.

use std::collections::BTreeSet;

use decon_core::progress::BudgetExceeded;
use decon_core::{
    Abstraction, AbstractionKind, BudgetConfig, IdentifyResult, ProgressTracker, Tier,
    capped_file_chars, extract_yaml_block, module_key,
};
use decon_llm::{LlmClient, bounded_complete, bounded_complete_with_budget};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::checkpoint_store::CheckpointStoreError;
use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};

/// Re-export of [`PromptError`] for ergonomic matching at call sites that
/// only depend on `decon-pipeline`.
pub use crate::prompts::PromptError;
/// Re-export of [`decon_core::ExtractError`] for ergonomic matching at call
/// sites that only depend on `decon-pipeline`.
pub use decon_core::ExtractError;
/// Re-export of [`decon_llm::LlmError`] for ergonomic matching at call sites
/// that only depend on `decon-pipeline`.
pub use decon_llm::LlmError;

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
    Parse(#[from] serde_yaml::Error),
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
    /// `decon_core::CheckpointError`) so the orchestration in
    /// [`crate::identify_checkpoint`] can propagate persistence failures
    /// without inventing a separate error enum.
    #[error("checkpoint error during identify: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
}

/// Input to the single-shot identify stage.
#[derive(Clone, Debug)]
pub struct IdentifySingleShotInput {
    /// Relative file paths from the crawl inventory.
    pub files: Vec<String>,
    /// Project name.
    pub project_name: String,
    /// Language instruction (e.g. `"Use Chinese"` or `""`).
    pub language_instruction: String,
    /// Additional lang note (e.g. `"Use 简体中文"` or `""`), mapped to the
    /// `name_lang_hint` and `desc_lang_hint` template variables.
    pub lang_note: String,
    /// Max number of abstractions to return.
    pub max_abstraction_num: usize,
}

/// Run the single-shot identify stage: one LLM call produces the full
/// abstraction list. Used for small repos where map/reduce is unnecessary.
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt render failures, LLM call failures,
/// YAML extraction/parse failures, out-of-range file indices, an empty
/// abstraction list, or a budget overrun.
pub async fn identify_single_shot(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &IdentifySingleShotInput,
    progress: Option<&mut decon_core::ProgressTracker>,
) -> Result<IdentifyResult, IdentifyError> {
    // a. Build the render context. The `identify_single_shot.md.j2` template
    //    expects: project_name, context, language_instruction,
    //    max_abstraction_num, name_lang_hint, desc_lang_hint, file_listing.
    //    Free-text variables are sanitized so untrusted input cannot execute
    //    as Jinja template code (see `prompts/README.md` security note).
    let file_listing = format_file_listing(&input.files);
    let context = json!({
        "project_name": sanitize_template_input(&input.project_name),
        // The single-shot input carries only the file inventory; the joined
        // file *contents* are injected by the caller into a future variant of
        // this input. For now `context` is empty so the template renders.
        "context": "",
        "language_instruction": sanitize_template_input(&input.language_instruction),
        "max_abstraction_num": input.max_abstraction_num,
        "name_lang_hint": sanitize_template_input(&input.lang_note),
        "desc_lang_hint": sanitize_template_input(&input.lang_note),
        "file_listing": file_listing,
    });

    // b. Render the prompt.
    let prompt = renderer.render(PromptId::IdentifySingleShot, &context)?;

    // c. Reserve budget up front (fail closed before spending a network call)
    //    and tag the current stage for observability. `reserve_llm_calls(1)`
    //    is the authoritative counter: it advances `llm_calls_used` by one,
    //    matching the single call we make. We intentionally do NOT also call
    //    `record_llm_call` (that would double-count).
    if let Some(tracker) = progress {
        tracker.reserve_llm_calls(1).map_err(IdentifyError::from)?;
        tracker.set_stage("identify");
    }

    // d. Call the LLM.
    let response = client.complete(&prompt).await?;

    // e. (Budget accounting happened in step c via `reserve_llm_calls`; no
    //    further counter mutation is needed here.)

    // f. Extract the YAML block from the (possibly prose-wrapped) response.
    let yaml_text = extract_yaml_block(&response)?;

    // g. Parse the extracted YAML into a list of abstractions.
    let abstractions: Vec<Abstraction> = serde_yaml::from_str(&yaml_text)?;

    // h. Validate file_indices against the crawl inventory.
    let total = input.files.len();
    for abs in &abstractions {
        for &idx in &abs.file_indices {
            if idx >= total {
                return Err(IdentifyError::FileIndexOutOfRange { index: idx, total });
            }
        }
    }

    // i. Empty result is an error, not a silent success.
    if abstractions.is_empty() {
        return Err(IdentifyError::NoAbstractions);
    }

    // j. Done.
    Ok(IdentifyResult::new(abstractions))
}

// ===========================================================================
// Map stage
// ===========================================================================

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
    pub tier: Tier,
    /// Free-form kind label (see [`AbstractionKind`]).
    pub kind: AbstractionKind,
    /// Monorepo apps this candidate touches (empty for single-app repos).
    pub apps: Vec<String>,
    /// Best real paths to open first when studying this candidate.
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

/// Input to the map stage of identify.
#[derive(Clone, Debug)]
pub struct IdentifyMapInput {
    /// Relative file paths from the crawl inventory (global ordering).
    pub files: Vec<String>,
    /// Per-file byte sizes, parallel to [`Self::files`].
    pub sizes: Vec<u64>,
    /// Project name.
    pub project_name: String,
    /// Language instruction (e.g. `"Use Chinese"` or `""`).
    pub language_instruction: String,
    /// Additional lang note (e.g. `"Use 简体中文"` or `""`), mapped to the
    /// `name_lang_hint` and `desc_lang_hint` template variables.
    pub lang_note: String,
    /// Max number of abstractions per batch (mapped to the template's
    /// `per_batch` variable).
    pub max_abstraction_num: usize,
    /// Maximum number of concurrent in-flight LLM calls.
    pub max_concurrency: usize,
    /// Budget config for batching (char budget per batch).
    pub budget_config: BudgetConfig,
}

/// Run the map stage of identify: split files into batches, render
/// `identify_map.md.j2` per batch, and fire bounded-concurrent LLM calls to
/// produce per-batch candidates.
///
/// # Algorithm
///
/// 1. **Batch** files greedily by size: each file's size is capped at
///    [`BudgetConfig::max_file_chars`], then files are packed into batches
///    whose total capped size does not exceed
///    [`BudgetConfig::batch_char_budget`]. A single oversized file still gets
///    its own batch.
/// 2. **Render** the `identify_map` prompt for each batch with global file
///    indices, batch metadata, and sanitized free-text variables.
/// 3. **Call** the LLM with bounded concurrency via
///    [`bounded_complete`] (or [`bounded_complete_with_budget`] when a
///    [`ProgressTracker`] is supplied).
/// 4. **Parse** each response's YAML block into [`CandidateAbstraction`]s,
///    validate `file_indices` against the global inventory, and fail closed
///    on any error (LLM failure, empty result, out-of-range index).
///
/// # Error strategy — fail closed
///
/// If any batch's LLM call fails, the entire `identify_map` returns
/// [`IdentifyError::LlmBatch`] with the batch index. If any batch returns
/// zero candidates, [`IdentifyError::NoAbstractions`] is returned. We never
/// silently drop a batch's results, because an incomplete candidate set would
/// produce a misleading final abstraction list after the reduce stage.
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt render failures, LLM call failures
/// (with batch index), YAML extraction/parse failures, out-of-range file
/// indices, empty candidate lists, or a budget overrun.
pub async fn identify_map(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &IdentifyMapInput,
    progress: Option<&mut ProgressTracker>,
) -> Result<Vec<CandidateBatch>, IdentifyError> {
    // a. Batch files by size budget.
    let batches = batch_files_by_size(&input.files, &input.sizes, &input.budget_config);
    let batch_total = batches.len();

    // No files → no batches → empty result (not an error).
    if batch_total == 0 {
        return Ok(Vec::new());
    }

    // b. Render one prompt per batch.
    let mut prompts: Vec<String> = Vec::with_capacity(batch_total);
    for (batch_idx, batch_indices) in batches.iter().enumerate() {
        let prompt = render_map_prompt(renderer, input, batch_indices, batch_idx, batch_total)?;
        prompts.push(prompt);
    }

    // c. Fire bounded-concurrent LLM calls. When a progress tracker is
    //    supplied, use bounded_complete_with_budget (which reserves all calls
    //    up front and fails closed on budget exhaustion). Otherwise use
    //    bounded_complete directly.
    let results = match progress {
        Some(tracker) => {
            tracker.set_stage("identify_map");
            bounded_complete_with_budget(client, prompts, input.max_concurrency, tracker)
                .await
                .map_err(IdentifyError::from)?
        }
        None => bounded_complete(client, prompts, input.max_concurrency).await,
    };

    // d. Process each result in order. Fail closed on any error.
    let mut candidate_batches: Vec<CandidateBatch> = Vec::with_capacity(batch_total);
    let total_files = input.files.len();

    for (batch_idx, result) in results.into_iter().enumerate() {
        match result {
            Ok(response) => {
                // Extract the YAML block from the (possibly prose-wrapped) response.
                let yaml_text = extract_yaml_block(&response)?;

                // Parse into candidates (batch_idx is injected after parsing
                // because the LLM does not emit it).
                let candidates = parse_candidates(&yaml_text, batch_idx)?;

                // Validate file_indices against the global inventory.
                for cand in &candidates {
                    for &idx in &cand.file_indices {
                        if idx >= total_files {
                            return Err(IdentifyError::FileIndexOutOfRange {
                                index: idx,
                                total: total_files,
                            });
                        }
                    }
                }

                // Empty result from a batch is an error (fail closed).
                if candidates.is_empty() {
                    return Err(IdentifyError::NoAbstractions);
                }

                candidate_batches.push(CandidateBatch {
                    batch_idx,
                    candidates,
                });
            }
            Err(error) => {
                // FAIL CLOSED: return the error with the batch index so the
                // caller knows which batch failed.
                return Err(IdentifyError::LlmBatch {
                    batch_idx,
                    batch_total,
                    error,
                });
            }
        }
    }

    Ok(candidate_batches)
}

/// Run a **single** map batch: render the prompt for one batch, call the LLM,
/// and parse the candidates. This is the per-batch building block used by
/// [`crate::identify_runner::identify_with_cancellation`] to check
/// cancellation between batches.
///
/// `batch_indices` are global file indices into `input.files`. `batch_idx`
/// is the 0-based batch index (for candidate tagging). `batch_total` is the
/// total number of batches (for prompt display).
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt render, LLM call, extraction, parse,
/// or out-of-range file index failures.
pub(crate) async fn run_single_map_batch(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &IdentifyMapInput,
    batch_indices: &[usize],
    batch_idx: usize,
    batch_total: usize,
    progress: Option<&mut ProgressTracker>,
) -> Result<CandidateBatch, IdentifyError> {
    let prompt = render_map_prompt(renderer, input, batch_indices, batch_idx, batch_total)?;

    if let Some(tracker) = progress {
        tracker.reserve_llm_calls(1).map_err(IdentifyError::from)?;
        tracker.set_stage("identify_map");
    }

    let response = client.complete(&prompt).await?;
    let yaml_text = extract_yaml_block(&response)?;
    let candidates = parse_candidates(&yaml_text, batch_idx)?;

    let total_files = input.files.len();
    for cand in &candidates {
        for &idx in &cand.file_indices {
            if idx >= total_files {
                return Err(IdentifyError::FileIndexOutOfRange {
                    index: idx,
                    total: total_files,
                });
            }
        }
    }

    if candidates.is_empty() {
        return Err(IdentifyError::NoAbstractions);
    }

    Ok(CandidateBatch {
        batch_idx,
        candidates,
    })
}

/// Pack files greedily into batches by capped size.
///
/// Each file's size is capped at `config.max_file_chars` (matching the budget
/// model in [`decon_core::estimate_budget`]). Files are packed in inventory
/// order into batches whose total capped size does not exceed
/// `config.batch_char_budget`. A single file whose capped size exceeds the
/// budget still gets its own batch (an "oversized batch").
///
/// Returns a vector of batches, each a vector of global file indices.
pub(crate) fn batch_files_by_size(
    files: &[String],
    sizes: &[u64],
    config: &BudgetConfig,
) -> Vec<Vec<usize>> {
    if files.is_empty() {
        return Vec::new();
    }

    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current_batch: Vec<usize> = Vec::new();
    let mut current_size: usize = 0;

    for (idx, (_, &size)) in files.iter().zip(sizes.iter()).enumerate() {
        let chars = usize::try_from(size).unwrap_or(usize::MAX);
        let capped = capped_file_chars(chars, config.max_file_chars);

        // Start a new batch if the current one is non-empty and adding this
        // file would exceed the budget.
        if !current_batch.is_empty()
            && current_size.saturating_add(capped) > config.batch_char_budget
        {
            batches.push(std::mem::take(&mut current_batch));
            current_size = 0;
        }

        current_batch.push(idx);
        current_size = current_size.saturating_add(capped);
    }

    // Flush the last batch.
    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    batches
}

/// Render the `identify_map` prompt for one batch.
///
/// The template expects: `batch_idx` (1-based), `batch_total`, `project_name`,
/// `module_note`, `context`, `language_instruction`, `per_batch`,
/// `name_lang_hint`, `desc_lang_hint`, `file_listing`. Free-text variables are
/// sanitized to prevent Jinja injection.
pub(crate) fn render_map_prompt(
    renderer: &PromptRenderer,
    input: &IdentifyMapInput,
    batch_indices: &[usize],
    batch_idx: usize,
    batch_total: usize,
) -> Result<String, PromptError> {
    // Build file_listing with global indices (format: `idx # path`).
    let file_listing: String = batch_indices
        .iter()
        .map(|&i| format!("{i} # {}", input.files[i]))
        .collect::<Vec<_>>()
        .join("\n");

    // Derive a module note from the distinct modules in this batch.
    let module_note = module_note_for_batch(&input.files, batch_indices);

    let context = json!({
        // 1-based batch index for display ("batch 1/2", "batch 2/2").
        "batch_idx": batch_idx + 1,
        "batch_total": batch_total,
        "project_name": sanitize_template_input(&input.project_name),
        "module_note": sanitize_template_input(&module_note),
        // File contents are injected by the caller in a future variant; for
        // now `context` is empty so the template renders.
        "context": "",
        "language_instruction": sanitize_template_input(&input.language_instruction),
        "per_batch": input.max_abstraction_num,
        "name_lang_hint": sanitize_template_input(&input.lang_note),
        "desc_lang_hint": sanitize_template_input(&input.lang_note),
        "file_listing": file_listing,
    });

    renderer.render(PromptId::IdentifyMap, &context)
}

/// Compute a comma-separated list of distinct module keys for a batch.
fn module_note_for_batch(files: &[String], indices: &[usize]) -> String {
    let modules: BTreeSet<String> = indices
        .iter()
        .map(|&i| module_key(&files[i]).to_string())
        .collect();
    modules.into_iter().collect::<Vec<_>>().join(", ")
}

/// Intermediate deserialization struct — the LLM emits YAML without
/// `batch_idx`, so we parse into this and then inject the batch index.
#[derive(Deserialize)]
struct RawCandidate {
    name: String,
    description: String,
    file_indices: Vec<usize>,
    tier: Tier,
    kind: AbstractionKind,
    #[serde(default)]
    apps: Vec<String>,
    #[serde(default)]
    entry_files: Vec<String>,
}

/// Parse a YAML string into a list of [`CandidateAbstraction`]s with the
/// given `batch_idx` injected.
pub(crate) fn parse_candidates(
    yaml_text: &str,
    batch_idx: usize,
) -> Result<Vec<CandidateAbstraction>, IdentifyError> {
    let raw: Vec<RawCandidate> = serde_yaml::from_str(yaml_text)?;

    let candidates = raw
        .into_iter()
        .map(|r| CandidateAbstraction {
            name: r.name,
            description: r.description,
            file_indices: r.file_indices,
            tier: r.tier,
            kind: r.kind,
            apps: r.apps,
            entry_files: r.entry_files,
            batch_idx,
        })
        .collect();

    Ok(candidates)
}

// ===========================================================================
// Reduce stage
// ===========================================================================

/// Input for the identify reduce stage.
#[derive(Clone, Debug)]
pub struct IdentifyReduceInput {
    /// Candidate abstractions from all map batches.
    pub candidates: Vec<CandidateAbstraction>,
    /// The original file inventory (for index validation).
    pub files: Vec<String>,
    /// Project name.
    pub project_name: String,
    /// Language instruction.
    pub language_instruction: String,
    /// Lang note.
    pub lang_note: String,
    /// Maximum number of abstractions in the final list.
    pub max_abstraction_num: usize,
    /// Module summary (optional text describing the module structure).
    pub module_summary: String,
}

/// Run the reduce stage of identify: merge and rank per-batch candidates
/// from the map stage into the final top-N [`Vec<Abstraction>`] via one LLM
/// call.
///
/// This produces the authoritative [`IdentifyResult`] that should be written
/// to the checkpoint.
///
/// # Algorithm
///
/// 1. **Serialize** the candidates into a YAML blob for the prompt.
/// 2. **Render** the `identify_reduce` prompt with the candidates blob,
///    module summary, and sanitized free-text variables.
/// 3. **Reserve** one LLM call against the budget (fail closed before
///    spending a network call).
/// 4. **Call** the LLM.
/// 5. **Extract** the YAML block from the (possibly prose-wrapped) response.
/// 6. **Parse** the extracted YAML into a [`Vec<Abstraction>`].
/// 7. **Cap** the result at `max_abstraction_num` (truncate, keeping the
///    top-ranked items as emitted by the LLM).
/// 8. **Validate** `file_indices` against the crawl inventory.
/// 9. **Fail closed** on an empty result.
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt render failures, LLM call failures,
/// YAML extraction/parse failures, out-of-range file indices, an empty
/// abstraction list, or a budget overrun.
pub async fn identify_reduce(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &IdentifyReduceInput,
    progress: Option<&mut ProgressTracker>,
) -> Result<IdentifyResult, IdentifyError> {
    // a. Build the render context for identify_reduce.md.j2. The template
    //    expects: project_name, module_summary, language_instruction,
    //    max_abstraction_num, name_lang_hint, desc_lang_hint, candidates_blob.
    //    Free-text variables are sanitized so untrusted input cannot execute
    //    as Jinja template code (see `prompts/README.md` security note).
    //    The candidates_blob is also sanitized: candidate names/descriptions
    //    are derived from crawled file content (untrusted), and a value like
    //    `{{ 7 * 7 }}` would be evaluated as a Jinja expression when the
    //    outer template is rendered. `sanitize_template_input` breaks the
    //    `{{`/`}}` delimiters by inserting a space, which is safe for YAML
    //    (YAML does not use `{{`/`}}` as syntax) and invisible to an LLM.
    let candidates_blob = sanitize_template_input(&candidates_to_yaml(&input.candidates)?);
    let context = json!({
        "project_name": sanitize_template_input(&input.project_name),
        "module_summary": sanitize_template_input(&input.module_summary),
        "language_instruction": sanitize_template_input(&input.language_instruction),
        "max_abstraction_num": input.max_abstraction_num,
        "name_lang_hint": sanitize_template_input(&input.lang_note),
        "desc_lang_hint": sanitize_template_input(&input.lang_note),
        "candidates_blob": candidates_blob,
    });

    // b. Render the prompt.
    let prompt = renderer.render(PromptId::IdentifyReduce, &context)?;

    // c. Reserve budget up front (fail closed before spending a network call)
    //    and tag the current stage for observability. `reserve_llm_calls(1)`
    //    is the authoritative counter: it advances `llm_calls_used` by one,
    //    matching the single call we make. We intentionally do NOT also call
    //    `record_llm_call` (that would double-count).
    if let Some(tracker) = progress {
        tracker.reserve_llm_calls(1).map_err(IdentifyError::from)?;
        tracker.set_stage("identify_reduce");
    }

    // d. Call the LLM.
    let response = client.complete(&prompt).await?;

    // e. Extract the YAML block from the (possibly prose-wrapped) response.
    let yaml_text = extract_yaml_block(&response)?;

    // f. Parse the extracted YAML into a list of abstractions.
    let mut abstractions: Vec<Abstraction> = serde_yaml::from_str(&yaml_text)?;

    // g. Enforce max_abstraction_num cap: if the LLM returned more than the
    //    requested maximum, truncate to the top-N (the LLM is instructed to
    //    rank, so the leading items are the highest-priority). We truncate
    //    rather than error so a slightly-over-long response is still usable.
    if abstractions.len() > input.max_abstraction_num {
        abstractions.truncate(input.max_abstraction_num);
    }

    // h. Validate file_indices against the crawl inventory.
    let total = input.files.len();
    for abs in &abstractions {
        for &idx in &abs.file_indices {
            if idx >= total {
                return Err(IdentifyError::FileIndexOutOfRange { index: idx, total });
            }
        }
    }

    // i. Empty result is an error, not a silent success.
    if abstractions.is_empty() {
        return Err(IdentifyError::NoAbstractions);
    }

    // j. Done.
    Ok(IdentifyResult::new(abstractions))
}

/// Serialize candidates to a YAML string for inclusion in the reduce prompt.
///
/// Produces a clean YAML list of [`CandidateAbstraction`]s (including
/// `batch_idx` so the LLM can trace each candidate back to its originating
/// batch). An empty candidate list serializes to `[]`.
///
/// # Errors
///
/// Returns [`IdentifyError::Parse`] if `serde_yaml` fails to serialize the
/// candidates. In practice [`CandidateAbstraction`] is a plain serializable
/// struct so this never fails, but the `Result` return avoids silently
/// swallowing a serialization error (which would send an empty blob to the
/// LLM and produce misleading results).
fn candidates_to_yaml(candidates: &[CandidateAbstraction]) -> Result<String, IdentifyError> {
    if candidates.is_empty() {
        return Ok("[]".to_string());
    }
    Ok(serde_yaml::to_string(candidates)?)
}

/// Format the crawl inventory as the `file_listing` the template expects:
/// `idx # path` per line, matching the `idx # path/comment` format documented
/// in `prompts/identify_single_shot.md.j2`.
fn format_file_listing(files: &[String]) -> String {
    files
        .iter()
        .enumerate()
        .map(|(idx, path)| format!("{idx} # {path}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use decon_core::{AbstractionKind, ProgressTracker, Tier};
    use decon_llm::{LlmClient, LlmError, MockClient};

    /// Two-file inventory used across happy-path tests.
    fn sample_files() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/config.rs".to_string(),
        ]
    }

    /// A canned LLM response wrapping a YAML list of two abstractions in a
    /// fenced block, plus some surrounding prose (as a real LLM would emit).
    fn canned_two_abstractions() -> String {
        let yaml = "\
- name: \"Core Module\"
  description: \"The main module\"
  file_indices: [0, 1, 2]
  tier: \"S\"
  kind: \"module\"
  apps: [\"app1\"]
  entry_files: [\"src/main.rs\"]
- name: \"Utils\"
  description: \"Utility functions\"
  file_indices: [3]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("Here are the abstractions:\n\n```yaml\n{yaml}```\n")
    }

    fn sample_input() -> IdentifySingleShotInput {
        IdentifySingleShotInput {
            files: sample_files(),
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
        }
    }

    #[tokio::test]
    async fn happy_path_returns_two_abstractions() {
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect("happy path should succeed");
        assert_eq!(result.abstractions.len(), 2);
        assert_eq!(result.abstractions[0].name, "Core Module");
        assert_eq!(result.abstractions[0].tier, Tier::S);
        assert_eq!(result.abstractions[0].kind, AbstractionKind::new("module"));
        assert_eq!(result.abstractions[0].file_indices, vec![0, 1, 2]);
        assert_eq!(result.abstractions[1].name, "Utils");
        assert_eq!(result.abstractions[1].file_indices, vec![3]);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn empty_abstraction_list_returns_no_abstractions() {
        let yaml = "```yaml\n[]\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect_err("empty list should error");
        assert!(matches!(err, IdentifyError::NoAbstractions), "got: {err:?}");
    }

    #[tokio::test]
    async fn file_index_out_of_range_returns_error() {
        // file_indices: [0, 99] — 99 is out of range for a 4-file inventory.
        let yaml = "\
```yaml
- name: \"Bad\"
  description: \"oob\"
  file_indices: [0, 99]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect_err("oob index should error");
        match err {
            IdentifyError::FileIndexOutOfRange { index, total } => {
                assert_eq!(index, 99);
                assert_eq!(total, 4);
            }
            other => panic!("expected FileIndexOutOfRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_yaml_returns_parse_error() {
        let yaml = "```yaml\n- name: \"Broken\n  description: : :\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect_err("malformed yaml should error");
        assert!(matches!(err, IdentifyError::Parse(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn malformed_yaml_preserves_typed_serde_yaml_error() {
        let yaml = "```yaml\n- name: \"Broken\n  description: : :\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect_err("malformed yaml should error");
        // The Parse variant must hold a typed serde_yaml::Error, not a String,
        // so callers can inspect the error kind and line number. This fails to
        // compile if Parse holds String instead of serde_yaml::Error.
        match err {
            IdentifyError::Parse(inner) => {
                let _: serde_yaml::Error = inner;
                assert!(!inner.to_string().is_empty());
            }
            other => panic!("expected IdentifyError::Parse(serde_yaml::Error), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_yaml_block_returns_extract_error() {
        let client = MockClient::new("just prose, no structured output here".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect_err("no block should error");
        assert!(matches!(err, IdentifyError::Extract(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new("ignored").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect_err("llm failure should propagate");
        assert!(
            matches!(err, IdentifyError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn progress_tracker_records_the_call() {
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let mut progress = ProgressTracker::new(10);
        let result = identify_single_shot(&client, &renderer, &input, Some(&mut progress))
            .await
            .expect("should succeed with progress");
        assert_eq!(result.abstractions.len(), 2);
        let snap = progress.snapshot();
        assert_eq!(snap.llm_calls_used, 1);
        assert_eq!(snap.llm_calls_remaining, 9);
    }

    #[tokio::test]
    async fn progress_tracker_budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        // max=0 means no calls allowed.
        let mut progress = ProgressTracker::new(0);
        let err = identify_single_shot(&client, &renderer, &input, Some(&mut progress))
            .await
            .expect_err("budget exceeded should error");
        assert!(matches!(err, IdentifyError::Budget(_)), "got: {err:?}");
        // The LLM must NOT have been called when the budget is exhausted up
        // front.
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn rendered_prompt_contains_expected_variables() {
        // Use a capturing client that records the prompt it received.
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_two_abstractions())
            }
        }

        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let input = IdentifySingleShotInput {
            files: sample_files(),
            project_name: "AcmeCorp/{{ evil }}".to_string(),
            language_instruction: "Use Spanish".to_string(),
            lang_note: "Use 简体中文".to_string(),
            max_abstraction_num: 7,
        };
        let _ = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        // Project name present and sanitized (no raw `{{`).
        assert!(prompt.contains("AcmeCorp"), "prompt: {prompt}");
        assert!(
            !prompt.contains("{{ evil }}"),
            "prompt not sanitized: {prompt}"
        );
        // Language instruction present.
        assert!(prompt.contains("Use Spanish"), "prompt: {prompt}");
        // max_abstraction_num present.
        assert!(prompt.contains("top 5-7"), "prompt: {prompt}");
        // File listing present with indexed paths.
        assert!(prompt.contains("0 # src/main.rs"), "prompt: {prompt}");
        assert!(prompt.contains("3 # src/config.rs"), "prompt: {prompt}");
        // lang_note propagated to the name/desc hints.
        assert!(prompt.contains("简体中文"), "prompt: {prompt}");
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_two_abstractions()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&*client, &renderer, &input, None)
            .await
            .expect("dyn client should work");
        assert_eq!(result.abstractions.len(), 2);
    }

    #[tokio::test]
    async fn single_abstraction_round_trips() {
        let yaml = "\
```yaml
- name: \"Solo\"
  description: \"only one\"
  file_indices: [0]
  tier: \"L\"
  kind: \"class\"
  apps: [\"web\", \"api\"]
  entry_files: [\"src/main.rs\", \"src/lib.rs\"]
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&client, &renderer, &input, None)
            .await
            .expect("single abstraction should succeed");
        assert_eq!(result.abstractions.len(), 1);
        let a = &result.abstractions[0];
        assert_eq!(a.name, "Solo");
        assert_eq!(a.tier, Tier::L);
        assert_eq!(a.apps, vec!["web", "api"]);
        assert_eq!(a.entry_files, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn identify_error_display_is_sensible() {
        let e = IdentifyError::NoAbstractions;
        assert_eq!(e.to_string(), "no abstractions found in LLM output");
        let yaml_err = serde_yaml::from_str::<Vec<Abstraction>>("bad: : :\n").unwrap_err();
        let e = IdentifyError::Parse(yaml_err);
        assert!(
            e.to_string()
                .contains("failed to parse abstractions from LLM output")
        );
        let e = IdentifyError::FileIndexOutOfRange { index: 5, total: 3 };
        assert!(e.to_string().contains("5"));
        assert!(e.to_string().contains("3"));
    }
}

// ===========================================================================
// Map stage tests
// ===========================================================================

#[cfg(test)]
mod map_tests {
    use super::*;
    use decon_core::{BudgetConfig, ProgressTracker, Tier};
    use decon_llm::{LlmClient, LlmError, MockClient};

    /// Four-file inventory used across map-stage tests.
    fn sample_files() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/config.rs".to_string(),
        ]
    }

    /// A canned LLM response wrapping a YAML list of two candidate abstractions
    /// in a fenced block, plus some surrounding prose.
    fn canned_candidates_yaml() -> String {
        let yaml = "\
- name: \"Module A\"
  description: \"Module A desc\"
  file_indices: [0, 1]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"Module B\"
  description: \"Module B desc\"
  file_indices: [2, 3]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("Here are the abstractions:\n\n```yaml\n{yaml}```\n")
    }

    /// Budget config that splits four 100-char files into two batches of two.
    fn two_batch_config() -> BudgetConfig {
        BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 200,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        }
    }

    fn sample_map_input(
        files: Vec<String>,
        sizes: Vec<u64>,
        budget: BudgetConfig,
    ) -> IdentifyMapInput {
        IdentifyMapInput {
            files,
            sizes,
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
            max_concurrency: 2,
            budget_config: budget,
        }
    }

    #[tokio::test]
    async fn two_batches_both_succeed() {
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100];
        let client = MockClient::new(canned_candidates_yaml());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, two_batch_config());

        let result = identify_map(&client, &renderer, &input, None)
            .await
            .expect("two batches should succeed");

        assert_eq!(result.len(), 2, "should produce 2 candidate batches");
        // Batch 0
        assert_eq!(result[0].batch_idx, 0);
        assert_eq!(result[0].candidates.len(), 2);
        assert_eq!(result[0].candidates[0].name, "Module A");
        assert_eq!(result[0].candidates[0].batch_idx, 0);
        assert_eq!(result[0].candidates[0].tier, Tier::S);
        // Batch 1
        assert_eq!(result[1].batch_idx, 1);
        assert_eq!(result[1].candidates.len(), 2);
        assert_eq!(result[1].candidates[0].batch_idx, 1);
        assert_eq!(result[1].candidates[1].name, "Module B");
        assert_eq!(client.call_count(), 2);
    }

    #[tokio::test]
    async fn single_batch_small_repo() {
        let files = sample_files();
        let sizes = vec![10, 10, 10, 10];
        let client = MockClient::new(canned_candidates_yaml());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, BudgetConfig::default());

        let result = identify_map(&client, &renderer, &input, None)
            .await
            .expect("single batch should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].batch_idx, 0);
        assert_eq!(result[0].candidates.len(), 2);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn file_index_out_of_range_returns_error() {
        let yaml = "\
```yaml
- name: \"Bad\"
  description: \"oob\"
  file_indices: [0, 99]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
```
";
        let files = sample_files();
        let sizes = vec![10, 10, 10, 10];
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, BudgetConfig::default());

        let err = identify_map(&client, &renderer, &input, None)
            .await
            .expect_err("oob index should error");

        match err {
            IdentifyError::FileIndexOutOfRange { index, total } => {
                assert_eq!(index, 99);
                assert_eq!(total, 4);
            }
            other => panic!("expected FileIndexOutOfRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn batch_failure_fails_closed() {
        // Two batches; fail on the second call (batch index 1).
        // Use max_concurrency=1 for deterministic call ordering.
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100];
        let client = MockClient::new(canned_candidates_yaml()).fail_on(1, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let mut input = sample_map_input(files, sizes, two_batch_config());
        input.max_concurrency = 1;

        let err = identify_map(&client, &renderer, &input, None)
            .await
            .expect_err("batch failure should error");

        match err {
            IdentifyError::LlmBatch {
                batch_idx, error, ..
            } => {
                assert_eq!(batch_idx, 1);
                assert!(matches!(error, LlmError::Timeout), "got: {error:?}");
            }
            other => panic!("expected LlmBatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_result_from_batch_returns_no_abstractions() {
        let yaml = "```yaml\n[]\n```\n";
        let files = sample_files();
        let sizes = vec![10, 10, 10, 10];
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, BudgetConfig::default());

        let err = identify_map(&client, &renderer, &input, None)
            .await
            .expect_err("empty result should error");

        assert!(matches!(err, IdentifyError::NoAbstractions), "got: {err:?}");
    }

    #[tokio::test]
    async fn budget_exceeded_returns_budget_error() {
        let files = sample_files();
        let sizes = vec![10, 10, 10, 10];
        let client = MockClient::new(canned_candidates_yaml());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, BudgetConfig::default());

        let mut progress = ProgressTracker::new(0); // no calls allowed
        let err = identify_map(&client, &renderer, &input, Some(&mut progress))
            .await
            .expect_err("budget exceeded should error");

        assert!(matches!(err, IdentifyError::Budget(_)), "got: {err:?}");
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn progress_tracker_records_calls() {
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100];
        let client = MockClient::new(canned_candidates_yaml());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, two_batch_config());

        let mut progress = ProgressTracker::new(10);
        let result = identify_map(&client, &renderer, &input, Some(&mut progress))
            .await
            .expect("should succeed with progress");

        assert_eq!(result.len(), 2);
        assert_eq!(progress.snapshot().llm_calls_used, 2);
        assert_eq!(progress.snapshot().llm_calls_remaining, 8);
    }

    #[tokio::test]
    async fn batching_splits_files_correctly() {
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100];
        let batches = batch_files_by_size(&files, &sizes, &two_batch_config());
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec![0, 1]);
        assert_eq!(batches[1], vec![2, 3]);
    }

    #[tokio::test]
    async fn batching_single_batch_when_all_fit() {
        let files = sample_files();
        let sizes = vec![10, 10, 10, 10];
        let batches = batch_files_by_size(&files, &sizes, &BudgetConfig::default());
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn batching_empty_files_returns_empty() {
        let batches = batch_files_by_size(&[], &[], &BudgetConfig::default());
        assert!(batches.is_empty());
    }

    #[tokio::test]
    async fn batching_oversized_file_gets_own_batch() {
        // A single file larger than the batch budget still gets its own batch.
        let files = vec!["huge.rs".to_string(), "tiny.rs".to_string()];
        let sizes = vec![10_000, 10];
        let cfg = BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 50,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        };
        let batches = batch_files_by_size(&files, &sizes, &cfg);
        // huge.rs is capped to 1000, which exceeds budget 50 -> own batch.
        // tiny.rs is 10, fits in a new batch.
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec![0]);
        assert_eq!(batches[1], vec![1]);
    }

    #[tokio::test]
    async fn max_concurrency_one_processes_all_batches() {
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100];
        let client = MockClient::new(canned_candidates_yaml());
        let renderer = PromptRenderer::new().unwrap();
        let mut input = sample_map_input(files, sizes, two_batch_config());
        input.max_concurrency = 1;

        let result = identify_map(&client, &renderer, &input, None)
            .await
            .expect("should succeed with concurrency=1");

        assert_eq!(result.len(), 2);
        assert_eq!(client.call_count(), 2);
    }

    #[tokio::test]
    async fn empty_files_returns_empty_result() {
        let client = MockClient::new(canned_candidates_yaml());
        let renderer = PromptRenderer::new().unwrap();
        let input = IdentifyMapInput {
            files: vec![],
            sizes: vec![],
            project_name: "empty".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
            max_concurrency: 2,
            budget_config: BudgetConfig::default(),
        };

        let result = identify_map(&client, &renderer, &input, None)
            .await
            .expect("empty files should succeed with empty result");

        assert!(result.is_empty());
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let files = sample_files();
        let sizes = vec![10, 10, 10, 10];
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_candidates_yaml()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_map_input(files, sizes, BudgetConfig::default());

        let result = identify_map(&*client, &renderer, &input, None)
            .await
            .expect("dyn client should work");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].candidates.len(), 2);
    }

    #[tokio::test]
    async fn rendered_prompt_contains_expected_variables() {
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                self.captured.lock().unwrap().push(prompt.to_string());
                Ok(canned_candidates_yaml())
            }
        }

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100];
        let mut input = sample_map_input(files, sizes, two_batch_config());
        input.project_name = "AcmeCorp/{{ evil }}".to_string();
        input.max_concurrency = 1; // deterministic order

        let _ = identify_map(&client, &renderer, &input, None)
            .await
            .expect("should succeed");

        let prompts = captured.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2);
        // Batch indices (1-based in the template).
        assert!(prompts[0].contains("batch 1/2"), "prompt: {}", prompts[0]);
        assert!(prompts[1].contains("batch 2/2"), "prompt: {}", prompts[1]);
        // Project name present and sanitized (no raw `{{`).
        assert!(prompts[0].contains("AcmeCorp"), "prompt: {}", prompts[0]);
        assert!(
            !prompts[0].contains("{{ evil }}"),
            "prompt not sanitized: {}",
            prompts[0]
        );
        // File listing with global indices.
        assert!(
            prompts[0].contains("0 # src/main.rs"),
            "prompt: {}",
            prompts[0]
        );
        assert!(
            prompts[0].contains("1 # src/lib.rs"),
            "prompt: {}",
            prompts[0]
        );
        assert!(
            prompts[1].contains("2 # src/utils.rs"),
            "prompt: {}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("3 # src/config.rs"),
            "prompt: {}",
            prompts[1]
        );
    }

    #[tokio::test]
    async fn candidate_abstraction_serde_round_trip() {
        let cand = CandidateAbstraction {
            name: "Test".to_string(),
            description: "desc".to_string(),
            file_indices: vec![0, 2],
            tier: Tier::S,
            kind: decon_core::AbstractionKind::new("module"),
            apps: vec!["app1".to_string()],
            entry_files: vec!["src/main.rs".to_string()],
            batch_idx: 3,
        };
        let json = serde_json::to_string(&cand).unwrap();
        let back: CandidateAbstraction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cand);
        // batch_idx is preserved.
        assert_eq!(back.batch_idx, 3);
    }

    #[test]
    fn llm_batch_error_display_is_sensible() {
        let e = IdentifyError::LlmBatch {
            batch_idx: 2,
            batch_total: 5,
            error: LlmError::Timeout,
        };
        let msg = e.to_string();
        assert!(msg.contains("batch 2"), "msg: {msg}");
        assert!(msg.contains("5"), "msg: {msg}");
        assert!(msg.contains("timed out"), "msg: {msg}");
    }
}

// ===========================================================================
// Reduce stage tests
// ===========================================================================

#[cfg(test)]
mod reduce_tests {
    use super::*;
    use decon_core::{AbstractionKind, ProgressTracker, Tier};
    use decon_llm::{LlmClient, LlmError, MockClient};

    /// Five-file inventory used across reduce-stage tests.
    fn sample_files() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/config.rs".to_string(),
            "src/api.rs".to_string(),
        ]
    }

    /// Build a single [`CandidateAbstraction`].
    fn cand(
        name: &str,
        desc: &str,
        indices: Vec<usize>,
        tier: Tier,
        kind: &str,
        batch_idx: usize,
    ) -> CandidateAbstraction {
        CandidateAbstraction {
            name: name.to_string(),
            description: desc.to_string(),
            file_indices: indices,
            tier,
            kind: AbstractionKind::new(kind),
            apps: Vec::new(),
            entry_files: Vec::new(),
            batch_idx,
        }
    }

    /// Three candidates from two batches (2 from batch 0, 1 from batch 1).
    fn sample_candidates() -> Vec<CandidateAbstraction> {
        vec![
            cand("Module A", "desc a", vec![0, 1], Tier::S, "module", 0),
            cand("Module B", "desc b", vec![2, 3], Tier::M, "utility", 0),
            cand("Module C", "desc c", vec![4], Tier::L, "service", 1),
        ]
    }

    /// A canned LLM response wrapping a YAML list of two final abstractions in
    /// a fenced block, plus some surrounding prose (as a real LLM would emit).
    fn canned_two_final_abstractions() -> String {
        let yaml = "\
- name: \"Core System\"
  description: \"The main system\"
  file_indices: [0, 1, 2, 3]
  tier: \"S\"
  kind: \"module\"
  apps: [\"app1\"]
  entry_files: [\"src/main.rs\"]
- name: \"Utils\"
  description: \"Utilities\"
  file_indices: [4]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("Here are the final abstractions:\n\n```yaml\n{yaml}```\n")
    }

    fn sample_reduce_input(candidates: Vec<CandidateAbstraction>) -> IdentifyReduceInput {
        IdentifyReduceInput {
            candidates,
            files: sample_files(),
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
            module_summary: "core, utils, api".to_string(),
        }
    }

    #[tokio::test]
    async fn happy_path_returns_two_final_abstractions() {
        let client = MockClient::new(canned_two_final_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let result = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect("happy path should succeed");
        assert_eq!(result.abstractions.len(), 2);
        assert_eq!(result.abstractions[0].name, "Core System");
        assert_eq!(result.abstractions[0].tier, Tier::S);
        assert_eq!(result.abstractions[0].kind, AbstractionKind::new("module"));
        assert_eq!(result.abstractions[0].file_indices, vec![0, 1, 2, 3]);
        assert_eq!(result.abstractions[1].name, "Utils");
        assert_eq!(result.abstractions[1].file_indices, vec![4]);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn max_abstraction_cap_truncates_to_limit() {
        // LLM returns 5 abstractions but max_abstraction_num=3 -> truncated to 3.
        let yaml = "\
```yaml
- name: \"A\"
  description: \"a\"
  file_indices: [0]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"B\"
  description: \"b\"
  file_indices: [1]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"C\"
  description: \"c\"
  file_indices: [2]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"D\"
  description: \"d\"
  file_indices: [3]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"E\"
  description: \"e\"
  file_indices: [4]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let mut input = sample_reduce_input(sample_candidates());
        input.max_abstraction_num = 3;
        let result = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect("cap should succeed with truncation");
        assert_eq!(result.abstractions.len(), 3);
        assert_eq!(result.abstractions[0].name, "A");
        assert_eq!(result.abstractions[2].name, "C");
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn empty_result_returns_no_abstractions() {
        let yaml = "```yaml\n[]\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect_err("empty list should error");
        assert!(matches!(err, IdentifyError::NoAbstractions), "got: {err:?}");
    }

    #[tokio::test]
    async fn file_index_out_of_range_returns_error() {
        // file_indices: [0, 99] — 99 is out of range for a 5-file inventory.
        let yaml = "\
```yaml
- name: \"Bad\"
  description: \"oob\"
  file_indices: [0, 99]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect_err("oob index should error");
        match err {
            IdentifyError::FileIndexOutOfRange { index, total } => {
                assert_eq!(index, 99);
                assert_eq!(total, 5);
            }
            other => panic!("expected FileIndexOutOfRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_yaml_returns_parse_error() {
        let yaml = "```yaml\n- name: \"Broken\n  description: : :\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect_err("malformed yaml should error");
        assert!(matches!(err, IdentifyError::Parse(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn no_yaml_block_returns_extract_error() {
        let client = MockClient::new("just prose, no structured output here".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect_err("no block should error");
        assert!(matches!(err, IdentifyError::Extract(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new("ignored").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect_err("llm failure should propagate");
        assert!(
            matches!(err, IdentifyError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn progress_tracker_records_the_call() {
        let client = MockClient::new(canned_two_final_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let mut progress = ProgressTracker::new(10);
        let result = identify_reduce(&client, &renderer, &input, Some(&mut progress))
            .await
            .expect("should succeed with progress");
        assert_eq!(result.abstractions.len(), 2);
        let snap = progress.snapshot();
        assert_eq!(snap.llm_calls_used, 1);
        assert_eq!(snap.llm_calls_remaining, 9);
    }

    #[tokio::test]
    async fn progress_tracker_budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_two_final_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        // max=0 means no calls allowed.
        let mut progress = ProgressTracker::new(0);
        let err = identify_reduce(&client, &renderer, &input, Some(&mut progress))
            .await
            .expect_err("budget exceeded should error");
        assert!(matches!(err, IdentifyError::Budget(_)), "got: {err:?}");
        // The LLM must NOT have been called when the budget is exhausted up
        // front.
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_two_final_abstractions()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let result = identify_reduce(&*client, &renderer, &input, None)
            .await
            .expect("dyn client should work");
        assert_eq!(result.abstractions.len(), 2);
    }

    #[tokio::test]
    async fn rendered_prompt_contains_expected_variables() {
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_two_final_abstractions())
            }
        }

        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let input = IdentifyReduceInput {
            candidates: sample_candidates(),
            files: sample_files(),
            project_name: "AcmeCorp/{{ evil }}".to_string(),
            language_instruction: "Use Spanish".to_string(),
            lang_note: "Use 简体中文".to_string(),
            max_abstraction_num: 7,
            module_summary: "core, utils".to_string(),
        };
        let _ = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        // Project name present and sanitized (no raw `{{`).
        assert!(prompt.contains("AcmeCorp"), "prompt: {prompt}");
        assert!(
            !prompt.contains("{{ evil }}"),
            "prompt not sanitized: {prompt}"
        );
        // Language instruction present.
        assert!(prompt.contains("Use Spanish"), "prompt: {prompt}");
        // max_abstraction_num present.
        assert!(prompt.contains("top 5-7"), "prompt: {prompt}");
        // module_summary present.
        assert!(prompt.contains("core, utils"), "prompt: {prompt}");
        // lang_note propagated to the name/desc hints.
        assert!(prompt.contains("简体中文"), "prompt: {prompt}");
        // candidates_blob present (candidate names appear in the YAML blob).
        assert!(prompt.contains("Module A"), "prompt: {prompt}");
        assert!(prompt.contains("Module C"), "prompt: {prompt}");
    }

    #[test]
    fn candidates_to_yaml_produces_valid_yaml() {
        let candidates = sample_candidates();
        let yaml = candidates_to_yaml(&candidates).expect("should serialize");
        // Should parse back into the same number of candidates.
        let back: Vec<CandidateAbstraction> =
            serde_yaml::from_str(&yaml).expect("should parse back");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].name, "Module A");
        assert_eq!(back[2].batch_idx, 1);
    }

    #[test]
    fn candidates_to_yaml_empty_produces_empty_list() {
        let yaml = candidates_to_yaml(&[]).expect("should serialize empty");
        assert_eq!(yaml.trim(), "[]");
    }

    #[test]
    fn candidates_to_yaml_round_trips_special_yaml_chars() {
        // Candidate names with special YAML characters should round-trip
        // through serde_yaml without corruption.
        let candidates = vec![
            cand(
                "name: with colon",
                "desc # hash",
                vec![0],
                Tier::S,
                "module",
                0,
            ),
            cand("[bracketed]", "desc {braces}", vec![1], Tier::M, "class", 1),
        ];
        let yaml = candidates_to_yaml(&candidates).expect("should serialize");
        let back: Vec<CandidateAbstraction> =
            serde_yaml::from_str(&yaml).expect("should parse back");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "name: with colon");
        assert_eq!(back[0].description, "desc # hash");
        assert_eq!(back[1].name, "[bracketed]");
        assert_eq!(back[1].description, "desc {braces}");
    }

    #[tokio::test]
    async fn candidates_blob_template_injection_is_sanitized() {
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_two_final_abstractions())
            }
        }

        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        // A candidate with a Jinja injection payload in its name/description.
        let candidates = vec![cand(
            "{{ 7 * 7 }}",
            "desc {{ evil }}",
            vec![0],
            Tier::S,
            "module",
            0,
        )];
        let input = sample_reduce_input(candidates);
        let _ = identify_reduce(&client, &renderer, &input, None)
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        // The raw `{{` / `}}` Jinja delimiters must NOT appear in the
        // candidates_blob portion of the rendered prompt.
        assert!(
            !prompt.contains("{{ 7 * 7 }}"),
            "candidate name not sanitized in prompt: {prompt}"
        );
        assert!(
            !prompt.contains("{{ evil }}"),
            "candidate description not sanitized in prompt: {prompt}"
        );
    }
}
