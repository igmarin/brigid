use std::collections::BTreeSet;

use decon_core::{
    BudgetConfig, ProgressTracker, capped_file_chars, extract_yaml_block, module_key,
};
use decon_llm::{LlmClient, bounded_complete, bounded_complete_with_budget};
use serde::Deserialize;
use serde_json::json;

use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};

use super::types::{CandidateAbstraction, CandidateBatch, IdentifyError};

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
        let chars = size.min(usize::MAX as u64) as usize;
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
) -> Result<String, crate::prompts::PromptError> {
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
    tier: decon_core::Tier,
    kind: decon_core::AbstractionKind,
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
    let raw: Vec<RawCandidate> = serde_yaml_ng::from_str(yaml_text)?;

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
    async fn batching_handles_file_sizes_exceeding_32bit_usize() {
        // A file size that exceeds 32-bit usize (>4GB) must still convert to a
        // usable usize without panicking. On 64-bit (the target) u64 fits usize
        // so this is a no-op; on 32-bit the value saturates to usize::MAX. In
        // both cases the capped size (max_file_chars) drives batching, so the
        // oversized file lands in its own batch.
        let files = vec!["oversized.rs".to_string(), "small.rs".to_string()];
        // 5 GB — above the 4 GB 32-bit usize ceiling.
        let sizes: Vec<u64> = vec![5_000_000_000, 10];
        let cfg = BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 50,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        };
        let batches = batch_files_by_size(&files, &sizes, &cfg);
        // oversized.rs capped to 1000 > budget 50 -> own batch.
        // small.rs is 10, fits in a new batch.
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
