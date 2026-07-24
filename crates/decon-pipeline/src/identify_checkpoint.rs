//! Checkpoint glue for the identify stage.
//!
//! This module is the bridge between the identify stage functions
//! ([`identify_single_shot`], [`identify_map`], [`identify_reduce`]) and the
//! checkpoint lifecycle ([`CheckpointStore`], [`CheckpointV1`]). The identify
//! LLM logic lives in [`crate::identify`]; this module wires it together with
//! checkpoint persistence and resume.
//!
//! # Resume semantics (M3)
//!
//! For M3 simplicity, **resume mid-identify does a full re-run** — there is no
//! batch-level resume. If the identify stage was interrupted (e.g. a map batch
//! LLM call failed, or the process was killed), the next run re-executes the
//! entire identify stage from scratch. Batch-level checkpointing is a future
//! enhancement.
//!
//! # Single-shot vs map+reduce threshold
//!
//! [`identify_and_checkpoint`] chooses single-shot when the repo has at most
//! [`SINGLE_SHOT_FILE_THRESHOLD`] files **and** the total byte size is under
//! [`SINGLE_SHOT_SIZE_THRESHOLD`]. Otherwise it runs map+reduce. The size
//! threshold matches [`decon_core::DEFAULT_BATCH_CHAR_BUDGET`] (80 000 bytes)
//! — if all file bodies fit comfortably in one LLM context window, a single
//! call is cheaper and avoids the reduce round-trip.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use decon_core::{
    BudgetConfig, CheckpointError, CheckpointV1, IdentifyResult, ProgressTracker, RunConfig,
    StageId, config_hash, module_key,
};
use decon_llm::LlmClient;

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::identify::{
    IdentifyError, IdentifyMapInput, IdentifyReduceInput, IdentifySingleShotInput, identify_map,
    identify_reduce, identify_single_shot,
};
use crate::prompts::PromptRenderer;
use crate::resume;

/// Re-export of the core [`CheckpointError`] for ergonomic matching at call
/// sites that only depend on `decon-pipeline`.
pub use decon_core::CheckpointError as CoreCheckpointError;

/// Maximum file count for single-shot identify (repos with at most this many
/// files use single-shot, subject to the size threshold too).
pub const SINGLE_SHOT_FILE_THRESHOLD: usize = 20;

/// Maximum total byte size for single-shot identify. Matches
/// [`decon_core::DEFAULT_BATCH_CHAR_BUDGET`] — if all file bodies fit in one
/// LLM context window, a single call suffices.
pub const SINGLE_SHOT_SIZE_THRESHOLD: u64 = 80_000;

/// Default maximum number of abstractions to request from the LLM when the
/// run config does not specify a value.
pub const DEFAULT_MAX_ABSTRACTIONS: usize = 20;

/// Default maximum concurrency for map-stage LLM calls.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

// ---------------------------------------------------------------------------
// Error conversions
// ---------------------------------------------------------------------------

impl From<CheckpointStoreError> for IdentifyError {
    fn from(e: CheckpointStoreError) -> Self {
        Self::Checkpoint(e.to_string())
    }
}

impl From<CheckpointError> for IdentifyError {
    fn from(e: CheckpointError) -> Self {
        Self::Checkpoint(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Save the identify result to the checkpoint and mark the Identify stage
/// complete.
///
/// After a successful identify run (single-shot or map+reduce), this:
/// 1. Populates [`CheckpointV1::abstractions`] with the [`IdentifyResult`]
///    (via [`IdentifyResult::to_checkpoint_value`]).
/// 2. Marks [`StageId::Identify`] as complete (with an ISO 8601 UTC timestamp).
/// 3. Writes the checkpoint atomically via [`CheckpointStore::save`].
///
/// The existing file bundle is re-loaded from disk so the manifest is
/// preserved unchanged — only the metadata (`checkpoint.json`) is updated
/// with the new abstractions and stage completion. This reuses the M2 atomic
/// write path and does not invent a new one.
///
/// # Errors
///
/// Returns [`CheckpointStoreError`] if the serialize, load, or atomic write
/// fails.
pub fn save_identify_result(
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    result: &IdentifyResult,
) -> Result<(), CheckpointStoreError> {
    // 1. Populate abstractions with the typed IdentifyResult.
    let value = result
        .to_checkpoint_value()
        .map_err(|e| CheckpointStoreError::Checkpoint(CheckpointError::Json(e.to_string())))?;
    checkpoint.abstractions = Some(value);

    // 2. Mark the Identify stage complete with an ISO 8601 UTC timestamp.
    checkpoint.mark_stage_complete(StageId::Identify, now_iso8601_utc());

    // 3. Re-load the existing file bundle from disk (so the manifest is
    //    preserved unchanged) and write the updated metadata atomically via
    //    the M2 CheckpointStore::save path.
    let (_, files) = store.load()?;
    store.save(checkpoint.clone(), &files)?;
    Ok(())
}

/// Check if the identify stage should run based on the checkpoint state.
///
/// Returns `true` if:
/// - [`StageId::Identify`] is **not** in `completed_stages`, **or**
/// - The `config_hash` has changed (invalidating the identify result).
///
/// This wraps [`resume::should_run`] with an additional config-hash identity
/// check: even if Identify is marked complete, a changed config means the
/// cached abstractions are stale and must be regenerated.
///
/// # Panics
///
/// Never panics.
#[must_use]
pub fn should_run_identify(checkpoint: &CheckpointV1, current_config_hash: &str) -> bool {
    // A config-hash mismatch invalidates the cached identify result — even if
    // the stage is marked complete, a different config means the abstractions
    // are stale and must be regenerated.
    if checkpoint.config_hash != current_config_hash {
        return true;
    }
    // Otherwise defer to the standard resume check (stage not in
    // completed_stages).
    resume::should_run(StageId::Identify, checkpoint)
}

/// Load partial identify results from a checkpoint (if any).
///
/// Returns `None` if no abstractions have been saved yet (or if the stored
/// value is corrupt and cannot be deserialized).
/// Returns `Some(IdentifyResult)` if abstractions exist in the checkpoint.
///
/// # Panics
///
/// Never panics; deserialization failures are mapped to `None`.
#[must_use]
pub fn load_identify_result(checkpoint: &CheckpointV1) -> Option<IdentifyResult> {
    checkpoint
        .abstractions
        .as_ref()
        .and_then(|v| IdentifyResult::from_checkpoint_value(v.clone()).ok())
}

/// Run the full identify stage (choosing single-shot or map+reduce based on
/// file count / total size) and checkpoint the result.
///
/// This is the main entry point for the identify stage in the pipeline.
///
/// # Flow
///
/// 1. Check [`should_run_identify`] — if `false`, load and return the existing
///    result via [`load_identify_result`].
/// 2. Choose single-shot vs map+reduce based on [`SINGLE_SHOT_FILE_THRESHOLD`]
///    and [`SINGLE_SHOT_SIZE_THRESHOLD`].
/// 3. Run the appropriate identify function.
/// 4. Save the result via [`save_identify_result`].
/// 5. Return the [`IdentifyResult`].
///
/// # Resume (M3 simplicity)
///
/// If the identify stage was interrupted, the next run re-executes the entire
/// stage from scratch (no batch-level resume).
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt/LLM/parse failures, budget overruns,
/// or checkpoint persistence failures.
#[allow(clippy::too_many_arguments)] // signature dictated by issue #72 orchestration API
pub async fn identify_and_checkpoint(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    files: Vec<String>,
    sizes: Vec<u64>,
    config: &RunConfig,
    progress: &mut ProgressTracker,
) -> Result<IdentifyResult, IdentifyError> {
    // a. Compute the current config hash and check whether identify should run.
    let current_hash = config_hash(config)?;
    if !should_run_identify(checkpoint, &current_hash) {
        // Skip: load and return the existing result from the checkpoint.
        if let Some(existing) = load_identify_result(checkpoint) {
            return Ok(existing);
        }
        // Edge case: stage marked complete but no abstractions (should not
        // happen in normal operation). Fall through and re-run.
    }

    // b. Choose single-shot vs map+reduce based on file count / total size.
    let result = if use_single_shot(&files, &sizes) {
        // Single-shot: one LLM call for the whole repo.
        let input = IdentifySingleShotInput {
            files: files.clone(),
            project_name: project_name_from_config(config),
            language_instruction: language_instruction_from_config(config),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions_from_config(config),
        };
        identify_single_shot(client, renderer, &input, Some(progress)).await?
    } else {
        // Map + reduce: batch files, call LLM per batch, then reduce.
        let map_input = IdentifyMapInput {
            files: files.clone(),
            sizes: sizes.clone(),
            project_name: project_name_from_config(config),
            language_instruction: language_instruction_from_config(config),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions_from_config(config),
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            budget_config: budget_config_from_run(config),
        };
        let candidate_batches = identify_map(client, renderer, &map_input, Some(progress)).await?;

        // Flatten candidate batches into a single candidate list.
        let candidates: Vec<_> = candidate_batches
            .into_iter()
            .flat_map(|b| b.candidates)
            .collect();

        let reduce_input = IdentifyReduceInput {
            candidates,
            files: files.clone(),
            project_name: project_name_from_config(config),
            language_instruction: language_instruction_from_config(config),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions_from_config(config),
            module_summary: module_summary_from_files(&files),
        };
        identify_reduce(client, renderer, &reduce_input, Some(progress)).await?
    };

    // d. Save the result to the checkpoint.
    save_identify_result(store, checkpoint, &result)?;

    // e. Return the IdentifyResult.
    Ok(result)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Generate an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) from
/// [`SystemTime::now`].
///
/// Uses the Howard Hinnant `civil_from_days` algorithm to convert Unix
/// seconds to a proleptic Gregorian date without pulling in a date crate.
fn now_iso8601_utc() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert days since the Unix epoch (1970-01-01) to a proleptic Gregorian
/// `(year, month, day)` tuple.
///
/// Implements the Howard Hinnant `civil_from_days` algorithm — see
/// <https://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Build a [`BudgetConfig`] from a [`RunConfig`], falling back to defaults
/// for unset fields.
fn budget_config_from_run(config: &RunConfig) -> BudgetConfig {
    let mut bc = BudgetConfig::default();
    if let Some(b) = config.batch_char_budget {
        bc.batch_char_budget = b;
    }
    if let Some(c) = config.chars_per_token {
        bc.chars_per_token = c;
    }
    bc
}

/// Derive a project name from the run config root, falling back to
/// `"project"`.
fn project_name_from_config(config: &RunConfig) -> String {
    config
        .root
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
}

/// Derive a language instruction string from the run config (e.g.
/// `"Use English"`), or empty if unset.
fn language_instruction_from_config(config: &RunConfig) -> String {
    match config.language.as_deref() {
        Some(lang) if !lang.is_empty() => format!("Use {lang}"),
        _ => String::new(),
    }
}

/// Derive `max_abstraction_num` from the run config, defaulting to
/// [`DEFAULT_MAX_ABSTRACTIONS`].
///
/// Uses [`RunConfig::max_llm_calls`] as a rough upper bound when set (a run
/// capped at N LLM calls should not request more than N abstractions), falling
/// back to [`DEFAULT_MAX_ABSTRACTIONS`] when unset.
fn max_abstractions_from_config(config: &RunConfig) -> usize {
    config
        .max_llm_calls
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_ABSTRACTIONS)
}

/// Build a comma-separated module summary from the file inventory (for the
/// reduce prompt).
fn module_summary_from_files(files: &[String]) -> String {
    let modules: BTreeSet<String> = files.iter().map(|f| module_key(f).to_string()).collect();
    modules.into_iter().collect::<Vec<_>>().join(", ")
}

/// Decide whether to use single-shot identify based on file count and total
/// size.
fn use_single_shot(files: &[String], sizes: &[u64]) -> bool {
    let total_size: u64 = sizes.iter().sum();
    files.len() <= SINGLE_SHOT_FILE_THRESHOLD && total_size < SINGLE_SHOT_SIZE_THRESHOLD
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_store::records_from_files;
    use decon_core::{Abstraction, RunConfig, StageId, Tier};
    use decon_llm::MockClient;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Monotonic counter to guarantee unique temp dirs across parallel tests.
    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Create a temp directory for a checkpoint store.
    fn temp_dir() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-identify-ckpt-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a fresh [`CheckpointV1`] with a default config and a known
    /// source revision.
    fn fresh_checkpoint() -> CheckpointV1 {
        let cfg = RunConfig::default();
        CheckpointV1::new(
            &cfg,
            cfg.redacted_for_checkpoint(),
            "rev-abc",
            "2026-07-24T00:00:00Z",
        )
        .unwrap()
    }

    /// Save an initial checkpoint (with Fetch + DryRun complete) and a small
    /// file bundle to the store so identify can run / re-save.
    fn seed_store(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
        cp.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    /// A sample [`IdentifyResult`] with two abstractions.
    fn sample_result() -> IdentifyResult {
        IdentifyResult::new(vec![
            Abstraction::new("Core", "core module", Tier::S, "module"),
            Abstraction::new("Utils", "utilities", Tier::M, "utility"),
        ])
    }

    // --- save_identify_result ---

    #[test]
    fn save_identify_result_populates_abstractions_and_marks_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = sample_result();

        save_identify_result(&store, &mut cp, &result).expect("save should succeed");

        assert!(cp.abstractions.is_some());
        assert!(cp.is_stage_complete(StageId::Identify));
        assert!(cp.stage_timestamps.contains_key("identify"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_identify_result_round_trips_via_from_checkpoint_value() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = sample_result();

        save_identify_result(&store, &mut cp, &result).expect("save should succeed");

        // Reload from disk and verify the abstractions round-trip.
        let (loaded, _) = store.load().expect("load should succeed");
        let loaded_result =
            load_identify_result(&loaded).expect("should have abstractions after save");
        assert_eq!(loaded_result, result);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_identify_result_persists_to_disk() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = sample_result();

        save_identify_result(&store, &mut cp, &result).expect("save should succeed");

        // The on-disk checkpoint must reflect the identify completion.
        let (loaded, _) = store.load().expect("load should succeed");
        assert!(loaded.is_stage_complete(StageId::Identify));
        assert!(loaded.abstractions.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    // --- should_run_identify ---

    #[test]
    fn should_run_identify_fresh_checkpoint_returns_true() {
        let cp = fresh_checkpoint();
        let hash = cp.config_hash.clone();
        assert!(should_run_identify(&cp, &hash));
    }

    #[test]
    fn should_run_identify_complete_same_hash_returns_false() {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:05:00Z");
        let hash = cp.config_hash.clone();
        assert!(!should_run_identify(&cp, &hash));
    }

    #[test]
    fn should_run_identify_complete_different_hash_returns_true() {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:05:00Z");
        // A different config hash invalidates the cached result.
        assert!(should_run_identify(&cp, "sha256:different"));
    }

    #[test]
    fn should_run_identify_not_complete_different_hash_returns_true() {
        let cp = fresh_checkpoint();
        assert!(should_run_identify(&cp, "sha256:different"));
    }

    // --- load_identify_result ---

    #[test]
    fn load_identify_result_with_abstractions_returns_some() {
        let mut cp = fresh_checkpoint();
        let result = sample_result();
        cp.abstractions = Some(result.to_checkpoint_value().unwrap());

        let loaded = load_identify_result(&cp).expect("should return Some");
        assert_eq!(loaded, result);
    }

    #[test]
    fn load_identify_result_without_abstractions_returns_none() {
        let cp = fresh_checkpoint();
        assert!(load_identify_result(&cp).is_none());
    }

    #[test]
    fn load_identify_result_corrupt_value_returns_none() {
        let mut cp = fresh_checkpoint();
        cp.abstractions = Some(serde_json::json!({"not_valid": true}));
        assert!(load_identify_result(&cp).is_none());
    }

    // --- identify_and_checkpoint ---

    /// A canned LLM response with two abstractions valid for a 2-file repo.
    fn canned_two_abstractions() -> String {
        let yaml = "\
- name: \"Core\"
  description: \"core module\"
  file_indices: [0, 1]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"Utils\"
  description: \"utilities\"
  file_indices: [0]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("Here are the abstractions:\n\n```yaml\n{yaml}```\n")
    }

    #[tokio::test]
    async fn identify_and_checkpoint_fresh_runs_single_shot_and_saves() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let sizes = vec![10_u64, 10];
        let config = RunConfig::default();
        let mut progress = ProgressTracker::new(10);

        let result = identify_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            files,
            sizes,
            &config,
            &mut progress,
        )
        .await
        .expect("identify should succeed");

        assert_eq!(result.abstractions.len(), 2);
        assert!(cp.is_stage_complete(StageId::Identify));
        assert!(cp.abstractions.is_some());
        // Single-shot uses exactly one LLM call.
        assert_eq!(client.call_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn identify_and_checkpoint_complete_skips_and_loads_existing() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        // Pre-populate the checkpoint with a completed identify result.
        let existing = sample_result();
        save_identify_result(&store, &mut cp, &existing).expect("seed save should succeed");

        // Now run again — should skip and return the existing result.
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let sizes = vec![10_u64, 10];
        let config = RunConfig::default();
        let mut progress = ProgressTracker::new(10);

        let result = identify_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            files,
            sizes,
            &config,
            &mut progress,
        )
        .await
        .expect("skip should succeed");

        // The LLM must NOT have been called (skipped).
        assert_eq!(client.call_count(), 0);
        assert_eq!(result, existing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn identify_and_checkpoint_partial_abstractions_reruns_from_scratch() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        // Simulate a partial / interrupted identify: abstractions present but
        // Identify NOT marked complete.
        let partial = sample_result();
        cp.abstractions = Some(partial.to_checkpoint_value().unwrap());
        // Persist the partial state to disk.
        let (_, files_records) = store.load().unwrap();
        store.save(cp.clone(), &files_records).unwrap();

        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let sizes = vec![10_u64, 10];
        let config = RunConfig::default();
        let mut progress = ProgressTracker::new(10);

        let result = identify_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            files,
            sizes,
            &config,
            &mut progress,
        )
        .await
        .expect("re-run should succeed");

        // Must have re-run (LLM called) and overwritten the partial result.
        assert_eq!(client.call_count(), 1);
        assert!(cp.is_stage_complete(StageId::Identify));
        assert_eq!(result.abstractions.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    // --- invalidate_from with real typed data ---

    #[test]
    fn invalidate_from_identify_clears_typed_abstractions() {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        cp.mark_stage_complete(StageId::DryRun, "t2");
        cp.mark_stage_complete(StageId::Identify, "t3");

        // Store real typed abstractions (not just a raw JSON literal).
        let result = sample_result();
        cp.abstractions = Some(result.to_checkpoint_value().unwrap());

        // Verify the typed data round-trips before invalidation.
        let loaded = load_identify_result(&cp).expect("should load before invalidate");
        assert_eq!(loaded, result);

        // Invalidate from Identify — must clear abstractions.
        resume::invalidate_from(&mut cp, StageId::Identify);

        assert!(!cp.is_stage_complete(StageId::Identify));
        assert!(cp.abstractions.is_none());
        assert!(load_identify_result(&cp).is_none());
    }

    // --- Integration: identify -> checkpoint -> load -> match ---

    #[tokio::test]
    async fn integration_identify_checkpoint_load_match() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let sizes = vec![10_u64, 10];
        let config = RunConfig::default();
        let mut progress = ProgressTracker::new(10);

        let result = identify_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            files,
            sizes,
            &config,
            &mut progress,
        )
        .await
        .expect("identify should succeed");

        // Reload the checkpoint from disk and verify abstractions match.
        let (loaded_cp, _) = store.load().expect("load should succeed");
        let loaded_result =
            load_identify_result(&loaded_cp).expect("loaded checkpoint should have abstractions");
        assert_eq!(loaded_result, result);
        assert!(loaded_cp.is_stage_complete(StageId::Identify));
        let _ = fs::remove_dir_all(&dir);
    }

    // --- Helper tests ---

    #[test]
    fn use_single_shot_small_repo() {
        let files: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
        let sizes: Vec<u64> = vec![100; 10];
        assert!(use_single_shot(&files, &sizes));
    }

    #[test]
    fn use_single_shot_too_many_files() {
        let files: Vec<String> = (0..25).map(|i| format!("f{i}.rs")).collect();
        let sizes: Vec<u64> = vec![100; 25];
        assert!(!use_single_shot(&files, &sizes));
    }

    #[test]
    fn use_single_shot_too_much_size() {
        let files: Vec<String> = (0..5).map(|i| format!("f{i}.rs")).collect();
        let sizes: Vec<u64> = vec![20_000; 5]; // 100k total > 80k threshold
        assert!(!use_single_shot(&files, &sizes));
    }

    #[test]
    fn now_iso8601_utc_is_valid_format() {
        let ts = now_iso8601_utc();
        // YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.as_bytes()[4], b'-');
        assert_eq!(ts.as_bytes()[10], b'T');
        assert_eq!(ts.as_bytes()[13], b':');
        assert_eq!(ts.as_bytes()[16], b':');
    }

    #[test]
    fn civil_from_days_epoch() {
        // 1970-01-01 is day 0.
        let (y, m, d) = civil_from_days(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_known_date() {
        // 2026-07-24 — 20 658 days from 1970-01-01.
        // (56 years * 365 + 14 leap days = 20 454 to Jan 1 2026, + 204 days
        //  through Jul 24.)
        let (y, m, d) = civil_from_days(20_658);
        assert_eq!((y, m, d), (2026, 7, 24));
    }

    #[test]
    fn project_name_from_config_uses_root_basename() {
        let cfg = RunConfig {
            root: Some(std::path::PathBuf::from("/tmp/my-repo")),
            ..RunConfig::default()
        };
        assert_eq!(project_name_from_config(&cfg), "my-repo");
    }

    #[test]
    fn project_name_from_config_fallback() {
        let cfg = RunConfig::empty();
        assert_eq!(project_name_from_config(&cfg), "project");
    }

    #[test]
    fn language_instruction_from_config_populated() {
        let cfg = RunConfig {
            language: Some("es".into()),
            ..RunConfig::default()
        };
        assert_eq!(language_instruction_from_config(&cfg), "Use es");
    }

    #[test]
    fn language_instruction_from_config_empty() {
        let cfg = RunConfig::empty();
        assert_eq!(language_instruction_from_config(&cfg), "");
    }

    #[test]
    fn module_summary_from_files_distinct() {
        let files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "tests/t.rs".to_string(),
        ];
        let summary = module_summary_from_files(&files);
        // src and tests modules (module_key groups by top dir).
        assert!(!summary.is_empty());
    }

    #[test]
    fn budget_config_from_run_uses_overrides() {
        let cfg = RunConfig {
            batch_char_budget: Some(1_000),
            chars_per_token: Some(2),
            ..RunConfig::default()
        };
        let bc = budget_config_from_run(&cfg);
        assert_eq!(bc.batch_char_budget, 1_000);
        assert_eq!(bc.chars_per_token, 2);
    }

    #[test]
    fn budget_config_from_run_defaults() {
        let cfg = RunConfig::empty();
        let bc = budget_config_from_run(&cfg);
        assert_eq!(bc, BudgetConfig::default());
    }

    #[test]
    fn identify_error_from_checkpoint_store_error() {
        let err = CheckpointStoreError::NotFound(std::path::PathBuf::from("/x"));
        let id_err: IdentifyError = err.into();
        assert!(matches!(id_err, IdentifyError::Checkpoint(_)));
    }

    #[test]
    fn identify_error_from_core_checkpoint_error() {
        let err = CheckpointError::UnsupportedVersion(99);
        let id_err: IdentifyError = err.into();
        assert!(matches!(id_err, IdentifyError::Checkpoint(_)));
    }
}
