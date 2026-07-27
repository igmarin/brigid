//! Cancellation-aware identify runner (issue #68).
//!
//! Wraps the map + reduce identify stages with a [`CancelToken`] so that a
//! Ctrl+C / SIGTERM between map batches (or before the reduce call) stops
//! dispatching new work and writes a **partial** checkpoint with whatever
//! candidates were collected so far. The [`StageId::Identify`] stage is
//! **not** marked complete on cancellation — the caller should exit with
//! code `5` so the user knows to resume.
//!
//! See [`identify_with_cancellation`].
//!
//! [`StageId::Identify`]: brigid_core::StageId::Identify

use std::time::{SystemTime, UNIX_EPOCH};

use brigid_core::{
    CheckpointError, CheckpointV1, FileBundleRecord, IdentifyResult, ProgressTracker, RunConfig,
    StageId,
};

use crate::cancellation::CancelToken;
use crate::checkpoint_store::CheckpointStore;
use crate::identify::{
    CandidateAbstraction, CandidateBatch, IdentifyError, IdentifyMapInput, IdentifyReduceInput,
    IdentifySingleShotInput, identify_reduce, identify_single_shot,
};
use crate::prompts::PromptRenderer;

/// Outcome of [`identify_with_cancellation`].
///
/// Distinguishes a fully-completed run from a cancelled run that wrote a
/// partial checkpoint. The caller maps these to process exit codes:
///
/// - [`IdentifyRunOutcome::Completed`] → exit `0`.
/// - [`IdentifyRunOutcome::Cancelled`] → exit `5` (partial checkpoint saved).
/// - [`IdentifyError`] (propagated) → exit `1` or `2`.
#[derive(Debug)]
pub enum IdentifyRunOutcome {
    /// The identify stage completed fully; the checkpoint marks
    /// [`StageId::Identify`] as done.
    Completed(IdentifyResult),
    /// The stage was cancelled mid-flight. A partial checkpoint was written
    /// with whatever candidates were collected, but [`StageId::Identify`] is
    /// **not** marked complete. Resume to continue.
    Cancelled {
        /// How many candidate batches were collected before cancellation
        /// (0 if cancelled before any map batch completed).
        batches_completed: usize,
        /// Total number of candidate abstractions collected so far.
        candidates_collected: usize,
    },
}

/// Which identify strategy to use.
#[derive(Clone, Debug)]
pub enum IdentifyStrategy {
    /// Single-shot: one LLM call for the whole repo (small repos).
    SingleShot(IdentifySingleShotInput),
    /// Map + reduce: batched map calls then one reduce call.
    MapReduce(IdentifyMapInput),
}

/// Configuration for a cancellation-aware identify run.
#[derive(Clone, Debug)]
pub struct IdentifyRunConfig {
    /// Which strategy (single-shot or map+reduce) to run.
    pub strategy: IdentifyStrategy,
    /// Reduce input (only used for the map+reduce strategy). The candidates
    /// field is overwritten internally from the map results.
    pub reduce_input: Option<IdentifyReduceInput>,
    /// Unredacted run config (for checkpoint identity hashing).
    pub unredacted_config: RunConfig,
    /// Source revision (git SHA / URL) for checkpoint identity.
    pub source_revision: String,
    /// File-bundle records to persist alongside the checkpoint.
    pub files: Vec<FileBundleRecord>,
}

/// ISO-8601-ish UTC timestamp for checkpoint bookkeeping.
fn now_iso() -> String {
    // We avoid pulling in chrono; the checkpoint tests use fixed strings,
    // but the runner writes a real wall-clock timestamp. Format is
    // seconds-since-epoch suffixed with "Z" — sufficient for bookkeeping and
    // round-trips through serde as a plain string.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}Z")
}

/// Write a partial checkpoint with the collected candidates, **without**
/// marking [`StageId::Identify`] complete.
///
/// The candidates are serialized into the checkpoint's `abstractions` field as
/// a JSON object with a `candidates` array (so a future resume can pick them
/// up). The stage is intentionally left incomplete.
fn write_partial_checkpoint(
    store: &CheckpointStore,
    run_cfg: &IdentifyRunConfig,
    candidates: &[CandidateAbstraction],
) -> Result<(), IdentifyError> {
    let mut meta = CheckpointV1::new_with_repo(
        &run_cfg.unredacted_config,
        run_cfg.unredacted_config.redacted_for_checkpoint(),
        &run_cfg.source_revision,
        now_iso(),
        run_cfg.unredacted_config.root.as_deref(),
        run_cfg.unredacted_config.since.clone(),
    )?;

    // Mark any earlier stages that are implied to be done (fetch/dry-run) so
    // the checkpoint is in a sensible state. We do NOT mark Identify.
    meta.mark_stage_complete(StageId::Fetch, now_iso());

    // Store candidates as a JSON object so the schema is forward-compatible.
    let candidates_json = serde_json::json!({
        "partial": true,
        "candidates": candidates,
    });
    meta.abstractions = Some(candidates_json);

    store.save(meta, &run_cfg.files)?;
    Ok(())
}

/// Write a **completed** checkpoint with the final [`IdentifyResult`],
/// marking [`StageId::Identify`] complete.
fn write_completed_checkpoint(
    store: &CheckpointStore,
    run_cfg: &IdentifyRunConfig,
    result: &IdentifyResult,
) -> Result<(), IdentifyError> {
    let mut meta = CheckpointV1::new_with_repo(
        &run_cfg.unredacted_config,
        run_cfg.unredacted_config.redacted_for_checkpoint(),
        &run_cfg.source_revision,
        now_iso(),
        run_cfg.unredacted_config.root.as_deref(),
        run_cfg.unredacted_config.since.clone(),
    )?;

    meta.mark_stage_complete(StageId::Fetch, now_iso());
    meta.mark_stage_complete(StageId::DryRun, now_iso());
    meta.abstractions = Some(
        result
            .to_checkpoint_value()
            .map_err(CheckpointError::from)?,
    );
    meta.mark_stage_complete(StageId::Identify, now_iso());

    store.save(meta, &run_cfg.files)?;
    Ok(())
}

/// Flatten candidate batches into a single candidate vector.
fn flatten_candidates(batches: &[CandidateBatch]) -> Vec<CandidateAbstraction> {
    batches
        .iter()
        .flat_map(|b| b.candidates.iter().cloned())
        .collect()
}

/// Run the identify stage (single-shot or map+reduce) with cancellation
/// support.
///
/// # Cancellation behavior
///
/// - **Between map batches**: the token is checked before dispatching each
///   batch. If cancelled, in-flight calls are allowed to finish, then a
///   partial checkpoint is written with the candidates collected so far and
///   [`IdentifyRunOutcome::Cancelled`] is returned.
/// - **Before the reduce call**: if the token is cancelled after all map
///   batches complete but before reduce, a partial checkpoint with the map
///   candidates is written and [`IdentifyRunOutcome::Cancelled`] is returned.
/// - **Single-shot**: since this is a single LLM call, cancellation is checked
///   before the call. If cancelled, a partial checkpoint with no abstractions
///   is written and [`IdentifyRunOutcome::Cancelled`] is returned.
///
/// On a full completion, the checkpoint marks [`StageId::Identify`] complete.
///
/// # Errors
///
/// Returns [`IdentifyError`] for LLM / parse / budget failures. On
/// cancellation, returns `Ok(IdentifyRunOutcome::Cancelled)` — the partial
/// checkpoint has already been written.
///
/// [`StageId::Identify`]: brigid_core::StageId::Identify
pub async fn identify_with_cancellation(
    client: &dyn brigid_llm::LlmClient,
    renderer: &PromptRenderer,
    run_cfg: &IdentifyRunConfig,
    progress: &mut ProgressTracker,
    cancel: &CancelToken,
    checkpoint_store: &CheckpointStore,
    registry: Option<&brigid_core::plugin::PluginRegistry>,
) -> Result<IdentifyRunOutcome, IdentifyError> {
    // Extract the file paths from the strategy for kind enrichment.
    let strategy_files: Vec<String> = match &run_cfg.strategy {
        IdentifyStrategy::SingleShot(input) => input.files.clone(),
        IdentifyStrategy::MapReduce(map_input) => map_input.files.clone(),
    };
    match &run_cfg.strategy {
        IdentifyStrategy::SingleShot(input) => {
            // Check cancellation before the single call.
            if cancel.is_cancelled() {
                write_partial_checkpoint(checkpoint_store, run_cfg, &[])?;
                return Ok(IdentifyRunOutcome::Cancelled {
                    batches_completed: 0,
                    candidates_collected: 0,
                });
            }
            let mut result = identify_single_shot(client, renderer, input, Some(progress)).await?;
            // Enrich empty kinds via the plugin registry (issue #228).
            if let Some(reg) = registry {
                let empty_contents: Vec<String> = vec![String::new(); strategy_files.len()];
                crate::identify::enrich_identify_kinds(
                    &mut result,
                    &strategy_files,
                    &empty_contents,
                    reg,
                );
            }
            write_completed_checkpoint(checkpoint_store, run_cfg, &result)?;
            Ok(IdentifyRunOutcome::Completed(result))
        }
        IdentifyStrategy::MapReduce(map_input) => {
            // Compute batches from the full file inventory (global indices
            // preserved). We run batches one at a time, checking cancellation
            // before each batch. This lets us stop dispatching new work on
            // Ctrl+C while letting any in-flight call finish.
            let batch_indices = crate::identify::batch_files_by_size(
                &map_input.files,
                &map_input.sizes,
                &map_input.budget_config,
            );
            let batch_total = batch_indices.len();

            let mut all_batches: Vec<CandidateBatch> = Vec::new();

            for (batch_idx, indices) in batch_indices.into_iter().enumerate() {
                // Check cancellation before dispatching this batch.
                if cancel.is_cancelled() {
                    let candidates = flatten_candidates(&all_batches);
                    write_partial_checkpoint(checkpoint_store, run_cfg, &candidates)?;
                    return Ok(IdentifyRunOutcome::Cancelled {
                        batches_completed: all_batches.len(),
                        candidates_collected: candidates.len(),
                    });
                }
                let batch = crate::identify::run_single_map_batch(
                    client,
                    renderer,
                    map_input,
                    &indices,
                    batch_idx,
                    batch_total,
                    Some(progress),
                )
                .await?;
                all_batches.push(batch);
            }

            // All map batches done. Check cancellation before reduce.
            if cancel.is_cancelled() {
                let candidates = flatten_candidates(&all_batches);
                write_partial_checkpoint(checkpoint_store, run_cfg, &candidates)?;
                return Ok(IdentifyRunOutcome::Cancelled {
                    batches_completed: all_batches.len(),
                    candidates_collected: candidates.len(),
                });
            }

            // Run reduce if we have a reduce input.
            if let Some(mut reduce_input) = run_cfg.reduce_input.clone() {
                let candidates = flatten_candidates(&all_batches);
                reduce_input.candidates = candidates;
                let mut result =
                    identify_reduce(client, renderer, &reduce_input, Some(progress)).await?;
                // Enrich empty kinds via the plugin registry (issue #228).
                if let Some(reg) = registry {
                    let empty_contents: Vec<String> = vec![String::new(); strategy_files.len()];
                    crate::identify::enrich_identify_kinds(
                        &mut result,
                        &strategy_files,
                        &empty_contents,
                        reg,
                    );
                }
                write_completed_checkpoint(checkpoint_store, run_cfg, &result)?;
                Ok(IdentifyRunOutcome::Completed(result))
            } else {
                // No reduce input — treat map completion as the result. This
                // branch is used by tests that only exercise the map stage.
                let candidates = flatten_candidates(&all_batches);
                let mut result = IdentifyResult::new(
                    candidates
                        .iter()
                        .map(|c| {
                            brigid_core::Abstraction::new(
                                c.name.clone(),
                                c.description.clone(),
                                c.tier,
                                c.kind.clone(),
                            )
                        })
                        .collect(),
                );
                // Enrich empty kinds via the plugin registry (issue #228).
                if let Some(reg) = registry {
                    let empty_contents: Vec<String> = vec![String::new(); strategy_files.len()];
                    crate::identify::enrich_identify_kinds(
                        &mut result,
                        &strategy_files,
                        &empty_contents,
                        reg,
                    );
                }
                write_completed_checkpoint(checkpoint_store, run_cfg, &result)?;
                Ok(IdentifyRunOutcome::Completed(result))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::{IdentifyMapInput, IdentifyReduceInput, IdentifySingleShotInput};
    use crate::prompts::PromptRenderer;
    use brigid_core::{BudgetConfig, RunConfig, Tier};
    use brigid_llm::{LlmClient, MockClient};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Monotonic counter to guarantee unique temp dirs across parallel tests.
    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-cancel-identify-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_files() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/config.rs".to_string(),
            "src/api.rs".to_string(),
            "src/db.rs".to_string(),
        ]
    }

    fn canned_candidates() -> String {
        let yaml = "\
- name: \"Module A\"
  description: \"desc a\"
  file_indices: [0, 1]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"Module B\"
  description: \"desc b\"
  file_indices: [2, 3]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("```yaml\n{yaml}```\n")
    }

    fn canned_final() -> String {
        let yaml = "\
- name: \"Core\"
  description: \"core system\"
  file_indices: [0, 1, 2, 3]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"Api\"
  description: \"api layer\"
  file_indices: [4, 5]
  tier: \"M\"
  kind: \"service\"
  apps: []
  entry_files: []
";
        format!("```yaml\n{yaml}```\n")
    }

    /// Budget that splits six 100-char files into three batches of two.
    fn three_batch_config() -> BudgetConfig {
        BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 200,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        }
    }

    fn map_input(files: Vec<String>, sizes: Vec<u64>) -> IdentifyMapInput {
        IdentifyMapInput {
            files,
            sizes,
            project_name: "proj".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
            max_concurrency: 1,
            budget_config: three_batch_config(),
        }
    }

    fn reduce_input(files: Vec<String>) -> IdentifyReduceInput {
        IdentifyReduceInput {
            candidates: Vec::new(),
            files,
            project_name: "proj".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
            module_summary: "core, api".to_string(),
        }
    }

    fn run_cfg_map(files: Vec<String>, sizes: Vec<u64>, reduce: bool) -> IdentifyRunConfig {
        let ri = if reduce {
            Some(reduce_input(files.clone()))
        } else {
            None
        };
        IdentifyRunConfig {
            strategy: IdentifyStrategy::MapReduce(map_input(files, sizes)),
            reduce_input: ri,
            unredacted_config: RunConfig::default(),
            source_revision: "rev-1".to_string(),
            files: checkpoint_files(),
        }
    }

    fn run_cfg_single(files: Vec<String>) -> IdentifyRunConfig {
        IdentifyRunConfig {
            strategy: IdentifyStrategy::SingleShot(IdentifySingleShotInput {
                files: files.clone(),
                project_name: "proj".to_string(),
                language_instruction: String::new(),
                lang_note: String::new(),
                max_abstraction_num: 5,
            }),
            reduce_input: None,
            unredacted_config: RunConfig::default(),
            source_revision: "rev-1".to_string(),
            files: checkpoint_files(),
        }
    }

    /// A single dummy file-bundle record so the checkpoint can be saved and
    /// loaded (the store requires at least one record to produce a valid
    /// gzip member).
    fn checkpoint_files() -> Vec<FileBundleRecord> {
        use crate::checkpoint_store::records_from_files;
        records_from_files(&[("dummy.txt", b"hello" as &[u8])])
    }

    // -----------------------------------------------------------------
    // No cancellation: normal run completes, identify marked complete
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn no_cancellation_map_reduce_completes_and_marks_identify() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100, 100, 100];
        // 3 map batches + 1 reduce = 4 responses. MockClient repeats the last.
        let client = MockClient::with_responses(vec![
            canned_candidates(),
            canned_candidates(),
            canned_candidates(),
            canned_final(),
        ])
        .unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(100);
        let cancel = CancelToken::new();
        let cfg = run_cfg_map(files, sizes, true);

        let outcome = identify_with_cancellation(
            &client,
            &renderer,
            &cfg,
            &mut progress,
            &cancel,
            &store,
            None,
        )
        .await
        .expect("should complete");

        match outcome {
            IdentifyRunOutcome::Completed(result) => {
                assert!(!result.abstractions.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        let (meta, _) = store.load().unwrap();
        assert!(meta.is_stage_complete(StageId::Identify));
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // Cancellation before reduce: map completes, cancel before reduce
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn cancel_before_reduce_writes_partial_no_identify_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100, 100, 100];

        // A client that cancels the token after the 3rd map call (i.e. after
        // all map batches complete, before reduce). This way the map waves
        // run to completion, but the reduce check sees cancellation.
        let cancel = CancelToken::new();
        struct CancelAfterMap {
            inner: MockClient,
            cancel: CancelToken,
            calls: std::sync::atomic::AtomicUsize,
            map_calls: usize,
        }
        #[async_trait::async_trait]
        impl LlmClient for CancelAfterMap {
            async fn complete(&self, prompt: &str) -> Result<String, brigid_llm::LlmError> {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let result = self.inner.complete(prompt).await?;
                // After the last map call, cancel so the reduce check fires.
                if n + 1 == self.map_calls {
                    self.cancel.cancel();
                }
                Ok(result)
            }
        }
        let client = CancelAfterMap {
            inner: MockClient::new(canned_candidates()),
            cancel: cancel.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            map_calls: 3, // three batches
        };

        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(100);
        let cfg = run_cfg_map(files, sizes, true);

        let outcome = identify_with_cancellation(
            &client,
            &renderer,
            &cfg,
            &mut progress,
            &cancel,
            &store,
            None,
        )
        .await
        .expect("cancelled is Ok");

        match outcome {
            IdentifyRunOutcome::Cancelled {
                batches_completed,
                candidates_collected,
            } => {
                // All 3 map batches completed, but reduce did not run.
                assert_eq!(batches_completed, 3);
                // 3 batches * 2 candidates each = 6.
                assert_eq!(candidates_collected, 6);
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }

        let (meta, _) = store.load().unwrap();
        assert!(
            !meta.is_stage_complete(StageId::Identify),
            "identify must NOT be complete on cancellation"
        );
        let abs = meta.abstractions.expect("abstractions set");
        assert_eq!(abs["partial"], serde_json::json!(true));
        let cands = abs["candidates"].as_array().unwrap();
        assert_eq!(cands.len(), 6);
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // Cancellation with no progress: cancel immediately -> no abstractions
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn cancel_immediately_single_shot_writes_empty_checkpoint() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let files = sample_files();
        let client = MockClient::new(canned_final());
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(100);
        let cancel = CancelToken::new();
        cancel.cancel();
        let cfg = run_cfg_single(files);

        let outcome = identify_with_cancellation(
            &client,
            &renderer,
            &cfg,
            &mut progress,
            &cancel,
            &store,
            None,
        )
        .await
        .expect("cancelled is Ok");

        match outcome {
            IdentifyRunOutcome::Cancelled {
                candidates_collected,
                ..
            } => {
                assert_eq!(candidates_collected, 0);
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }

        let (meta, _) = store.load().unwrap();
        assert!(!meta.is_stage_complete(StageId::Identify));
        // The partial checkpoint should have an abstractions payload with
        // partial=true and an empty candidates array.
        let abs = meta.abstractions.expect("abstractions should be set");
        assert_eq!(abs["partial"], serde_json::json!(true));
        assert_eq!(abs["candidates"], serde_json::json!([]));
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // Cancellation during map: cancel after batch 1 of 3
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn cancel_during_map_writes_partial_candidates() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100, 100, 100];
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(100);
        let cancel = CancelToken::new();
        let cfg = run_cfg_map(files, sizes, true);

        // Cancel after the first wave completes. We do this by cancelling
        // in a spawned task with a tiny delay. Since MockClient is instant,
        // we instead rely on the fact that the cancel check happens before
        // each wave. We cancel before calling, so the first wave check
        // triggers cancellation with 0 candidates. To test "after batch 1",
        // we use a custom client that cancels the token after its first call.
        let cancel_for_task = cancel.clone();
        struct CancelAfterFirst {
            inner: MockClient,
            cancel: CancelToken,
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl LlmClient for CancelAfterFirst {
            async fn complete(&self, prompt: &str) -> Result<String, brigid_llm::LlmError> {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // After the first map call returns, cancel so the next
                    // wave check stops.
                    let result = self.inner.complete(prompt).await?;
                    self.cancel.cancel();
                    Ok(result)
                } else {
                    self.inner.complete(prompt).await
                }
            }
        }
        let client = CancelAfterFirst {
            inner: MockClient::new(canned_candidates()),
            cancel: cancel_for_task,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let outcome = identify_with_cancellation(
            &client,
            &renderer,
            &cfg,
            &mut progress,
            &cancel,
            &store,
            None,
        )
        .await
        .expect("should not error");

        match outcome {
            IdentifyRunOutcome::Cancelled {
                batches_completed,
                candidates_collected,
            } => {
                // At least the first wave (1 batch) should have completed.
                assert!(batches_completed >= 1, "batches: {batches_completed}");
                assert!(
                    candidates_collected >= 2,
                    "candidates: {candidates_collected}"
                );
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }

        let (meta, _) = store.load().unwrap();
        assert!(!meta.is_stage_complete(StageId::Identify));
        let abs = meta.abstractions.expect("abstractions set");
        assert_eq!(abs["partial"], serde_json::json!(true));
        let cands = abs["candidates"].as_array().unwrap();
        assert!(!cands.is_empty(), "should have partial candidates");
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // Single-shot no cancellation completes
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn single_shot_no_cancellation_completes() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let files = sample_files();
        let client = MockClient::new(canned_final());
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(100);
        let cancel = CancelToken::new();
        let cfg = run_cfg_single(files);

        let outcome = identify_with_cancellation(
            &client,
            &renderer,
            &cfg,
            &mut progress,
            &cancel,
            &store,
            None,
        )
        .await
        .expect("should complete");

        assert!(matches!(outcome, IdentifyRunOutcome::Completed(_)));
        let (meta, _) = store.load().unwrap();
        assert!(meta.is_stage_complete(StageId::Identify));
        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------
    // batch_files_by_size (re-exported from identify) mirrors batching
    // -----------------------------------------------------------------
    #[test]
    fn batch_files_by_size_three_batches_for_six_files() {
        let files = sample_files();
        let sizes = vec![100, 100, 100, 100, 100, 100];
        let batches = crate::identify::batch_files_by_size(&files, &sizes, &three_batch_config());
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0], vec![0, 1]);
        assert_eq!(batches[1], vec![2, 3]);
        assert_eq!(batches[2], vec![4, 5]);
    }

    #[test]
    fn flatten_candidates_preserves_order() {
        let batches = vec![
            CandidateBatch {
                batch_idx: 0,
                candidates: vec![CandidateAbstraction {
                    name: "A".into(),
                    description: "a".into(),
                    file_indices: vec![0],
                    tier: Tier::S,
                    kind: brigid_core::AbstractionKind::new("module"),
                    apps: vec![],
                    entry_files: vec![],
                    batch_idx: 0,
                }],
            },
            CandidateBatch {
                batch_idx: 1,
                candidates: vec![CandidateAbstraction {
                    name: "B".into(),
                    description: "b".into(),
                    file_indices: vec![1],
                    tier: Tier::M,
                    kind: brigid_core::AbstractionKind::new("util"),
                    apps: vec![],
                    entry_files: vec![],
                    batch_idx: 1,
                }],
            },
        ];
        let flat = flatten_candidates(&batches);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].name, "A");
        assert_eq!(flat[1].name, "B");
    }
}
