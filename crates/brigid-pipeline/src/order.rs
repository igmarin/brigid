//! OrderChapters pipeline stage (M4-ORD-1).
//!
//! Produces a pedagogical ordering of abstraction indices via a single LLM
//! call, using the identify result and relationships context.
//!
//! # Flow
//!
//! 1. [`order_chapters`] renders `order_chapters.md.j2`, redacts secrets from
//!    the context, calls the LLM, and parses the YAML output into a
//!    [`ChapterOrder`].
//! 2. [`order_and_checkpoint`] wraps the above with checkpoint save/load and
//!    resume semantics, mirroring the relationships stage.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::llm::{LlmClient, bounded_complete_with_budget};
use brigid_core::{
    ChapterOrder, ChapterOrderError, CheckpointV1, IdentifyResult, ProgressTracker,
    RelationshipsResult, StageId, extract_yaml_block, redact_content,
};
use serde_json::json;

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};
use crate::resume;

/// Re-export of the core [`brigid_core::CheckpointError`] for ergonomic matching
/// at call sites that only depend on `brigid-pipeline`.
pub use brigid_core::CheckpointError as CoreCheckpointError;

/// Errors returned by the order stage.
#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    /// The prompt template failed to render (missing/invalid variable).
    #[error("prompt rendering failed: {0}")]
    Prompt(#[from] crate::prompts::PromptError),
    /// The LLM call failed (network, timeout, rate limit, provider error).
    #[error("LLM call failed: {0}")]
    Llm(#[from] crate::llm::LlmError),
    /// The LLM returned empty output.
    #[error("LLM returned empty output")]
    EmptyOutput,
    /// No YAML block could be extracted from the LLM response.
    #[error("YAML/JSON block extraction failed: {0}")]
    Extract(#[from] brigid_core::ExtractError),
    /// The extracted YAML could not be parsed into an ordered index list.
    #[error("failed to parse chapter order from LLM output: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
    /// The parsed order failed validation (missing/duplicate/out-of-bounds).
    #[error("chapter order validation failed: {0}")]
    Validation(#[from] ChapterOrderError),
    /// A checkpoint save/load failed during the order stage.
    #[error("checkpoint error during order: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
    /// The configured LLM call budget was exceeded.
    #[error("budget exceeded: {0}")]
    Budget(#[from] brigid_core::BudgetExceeded),
}

/// Configuration for the order stage, mapping directly to the
/// `order_chapters.md.j2` template variables.
#[derive(Clone, Debug, Default)]
pub struct OrderConfig {
    /// Project name.
    pub project_name: String,
    /// Language instruction (e.g. `"Use Spanish"` or `""`).
    pub language_instruction: String,
    /// Short language hint suffix appended to the abstraction listing header
    /// (e.g. `" (in Spanish)"` or `""`).
    pub list_lang_note: String,
    /// Hub concept context from a graph provider (ADR 0016 T5).
    /// Empty string when no provider is configured (NoneProvider).
    pub hub_context: String,
}

/// Run the OrderChapters stage: render the prompt, call the LLM, parse the
/// YAML output into a [`ChapterOrder`], and validate it.
///
/// The `abstraction_listing` includes each abstraction's index, name,
/// description, tier, and kind. The `context` includes the project summary
/// and relationship edges from [`RelationshipsResult`], providing dependency
/// information for pedagogical ordering. Secrets are redacted from the context
/// before rendering.
///
/// # Errors
///
/// Returns [`OrderError`] for prompt render failures, LLM call failures, YAML
/// extraction/parse failures, validation failures, or budget overruns.
pub async fn order_chapters(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    identify: &IdentifyResult,
    relationships: &RelationshipsResult,
    config: &OrderConfig,
    progress: &mut ProgressTracker,
) -> Result<ChapterOrder, OrderError> {
    let abstractions = &identify.abstractions;
    let abstraction_count = abstractions.len();

    let abstraction_listing = format_abstraction_listing(abstractions);
    let context = build_order_context(relationships);

    let render_ctx = json!({
        "project_name": sanitize_template_input(&config.project_name),
        "list_lang_note": sanitize_template_input(&config.list_lang_note),
        "abstraction_listing": sanitize_template_input(&abstraction_listing),
        "context": sanitize_template_input(&context),
        "language_instruction": sanitize_template_input(&config.language_instruction),
        // Hub concept context from a graph provider (ADR 0016 T5). Empty
        // string when no provider is configured — the template conditional
        // skips it.
        "hub_context": sanitize_template_input(&config.hub_context),
    });

    let prompt = renderer.render(PromptId::OrderChapters, &render_ctx)?;

    progress.set_stage("order");
    let result = bounded_complete_with_budget(client, vec![prompt], 1, progress).await?;
    let response = result.into_iter().next().ok_or(OrderError::EmptyOutput)??;

    let yaml_text = extract_yaml_block(&response)?;

    let ordered_indices: Vec<usize> = serde_yaml_ng::from_str(&yaml_text)?;

    let order = ChapterOrder::new(ordered_indices);
    order.validate(abstraction_count)?;

    Ok(order)
}

/// Save the chapter order to the checkpoint and mark the Order stage complete.
///
/// # Errors
///
/// Returns [`CheckpointStoreError`] if the serialize, load, or atomic write
/// fails.
pub fn save_order_result(
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    result: &ChapterOrder,
) -> Result<(), CheckpointStoreError> {
    let value = result.to_checkpoint_value()?;
    checkpoint.order = Some(value);
    checkpoint.mark_stage_complete(StageId::Order, now_iso8601_utc());
    let (_, files) = store.load()?;
    store.save(checkpoint.clone(), &files)?;
    Ok(())
}

/// Check if the order stage should run based on the checkpoint state.
///
/// Returns `true` if [`StageId::Order`] is not in `completed_stages`.
#[must_use]
pub fn should_run_order(checkpoint: &CheckpointV1) -> bool {
    resume::should_run(StageId::Order, checkpoint)
}

/// Load the chapter order from a checkpoint (if any).
///
/// Returns `None` if no order has been saved yet (or if the stored value is
/// corrupt).
#[must_use]
pub fn load_order_result(checkpoint: &CheckpointV1) -> Option<ChapterOrder> {
    checkpoint
        .order
        .as_ref()
        .and_then(|v| ChapterOrder::from_checkpoint_value(v.clone()).ok())
}

/// Run the full order stage with checkpoint persistence and resume.
///
/// # Flow
///
/// 1. Check [`should_run_order`] — if `false`, load and return the existing
///    result via [`load_order_result`].
/// 2. Run [`order_chapters`].
/// 3. Save the result via [`save_order_result`].
/// 4. Return the [`ChapterOrder`].
///
/// # Errors
///
/// Returns [`OrderError`] for prompt/LLM/parse/validation failures, budget
/// overruns, or checkpoint persistence failures.
#[allow(clippy::too_many_arguments)]
pub async fn order_and_checkpoint(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    identify: &IdentifyResult,
    relationships: &RelationshipsResult,
    config: &OrderConfig,
    progress: &mut ProgressTracker,
) -> Result<ChapterOrder, OrderError> {
    if !should_run_order(checkpoint) {
        if let Some(existing) = load_order_result(checkpoint) {
            return Ok(existing);
        }
    }

    let result =
        order_chapters(client, renderer, identify, relationships, config, progress).await?;

    save_order_result(store, checkpoint, &result)?;

    Ok(result)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Generate an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
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

/// Convert days since the Unix epoch to a proleptic Gregorian `(year, month,
/// day)` tuple (Howard Hinnant `civil_from_days`).
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

/// Build the `abstraction_listing` string with index, name, tier, kind, and
/// description (`idx # Name (tier=S, kind=module) — description` per line).
///
/// Names and descriptions are secret-redacted via [`redact_content`] before
/// entering the prompt, providing defense-in-depth even if the identify stage
/// did not fully redact upstream content.
fn format_abstraction_listing(abstractions: &[brigid_core::Abstraction]) -> String {
    abstractions
        .iter()
        .enumerate()
        .map(|(idx, a)| {
            let name = redact_content(&a.name);
            let description = redact_content(&a.description);
            format!(
                "{idx} # {} (tier={}, kind={}) — {}",
                name.trim(),
                a.tier.as_str(),
                a.kind.as_str(),
                description.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the prompt `context` blob from the relationships result, redacting
/// secrets from the project summary before it enters the prompt.
///
/// The context includes the project summary followed by the relationship
/// edges listed as `from -> to: label (kind)`.
fn build_order_context(relationships: &RelationshipsResult) -> String {
    let summary = redact_content(&relationships.project_summary);
    if relationships.relationships.is_empty() {
        return format!("Project summary:\n{summary}");
    }
    let edges: Vec<String> = relationships
        .relationships
        .iter()
        .map(|r| {
            let label = redact_content(&r.label);
            let kind = redact_content(&r.kind);
            format!("{} -> {}: {} ({})", r.from, r.to, label.trim(), kind.trim())
        })
        .collect();
    format!(
        "Project summary:\n{summary}\n\nRelationships:\n{}",
        edges.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_store::records_from_files;
    use crate::llm::{LlmError, MockClient};
    use brigid_core::{Abstraction, Relationship, RunConfig, Tier};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-ord-ckpt-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    fn seed_store(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
        cp.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
        cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:04:00Z");
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn five_abstractions() -> Vec<Abstraction> {
        vec![
            Abstraction {
                name: "Router".into(),
                description: "Routes requests".into(),
                file_indices: vec![0],
                tier: Tier::M,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/router.rs".into()],
            },
            Abstraction {
                name: "Store".into(),
                description: "Persistence layer".into(),
                file_indices: vec![1],
                tier: Tier::M,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/store.rs".into()],
            },
            Abstraction {
                name: "Worker".into(),
                description: "Background jobs".into(),
                file_indices: vec![2],
                tier: Tier::S,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["api".into()],
                entry_files: vec!["src/worker.rs".into()],
            },
            Abstraction {
                name: "Config".into(),
                description: "App configuration".into(),
                file_indices: vec![3],
                tier: Tier::S,
                kind: brigid_core::AbstractionKind::new("config"),
                apps: vec![],
                entry_files: vec!["src/config.rs".into()],
            },
            Abstraction {
                name: "Auth".into(),
                description: "Authentication middleware".into(),
                file_indices: vec![4],
                tier: Tier::L,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["web".into(), "api".into()],
                entry_files: vec!["src/auth.rs".into()],
            },
        ]
    }

    fn sample_relationships() -> RelationshipsResult {
        RelationshipsResult::new(
            "A web framework with routing, persistence, and auth.",
            vec![
                Relationship::new(0, 1, "routes to", "calls"),
                Relationship::new(4, 0, "guards", "configures"),
                Relationship::new(1, 2, "hands off", "publishes"),
            ],
        )
    }

    fn sample_config() -> OrderConfig {
        OrderConfig {
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            list_lang_note: String::new(),
            hub_context: String::new(),
        }
    }

    fn canned_order(indices: &[usize]) -> String {
        let names = ["Router", "Store", "Worker", "Config", "Auth"];
        let lines: Vec<String> = indices
            .iter()
            .map(|&i| format!("- {i} # {}", names.get(i).unwrap_or(&"Unknown")))
            .collect();
        let yaml = lines.join("\n");
        format!("Here is the order:\n\n```yaml\n{yaml}\n```\n")
    }

    // --- order_chapters ---

    #[tokio::test]
    async fn happy_path_five_abstractions_valid_order() {
        let client = MockClient::new(canned_order(&[3, 1, 0, 4, 2]));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let result = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("happy path should succeed");
        assert_eq!(result.ordered_indices, vec![3, 1, 0, 4, 2]);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn missing_abstraction_returns_validation_error() {
        let client = MockClient::new(canned_order(&[0, 1, 3]));
        let renderer = PromptRenderer::new().unwrap();
        let abs = five_abstractions();
        let identify = IdentifyResult::new(abs[..4].to_vec());
        let relationships = sample_relationships();
        let config = sample_config();
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect_err("missing abstraction should error");
        assert!(
            matches!(
                err,
                OrderError::Validation(ChapterOrderError::MissingAbstraction { index: 2 })
            ),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn duplicate_index_returns_validation_error() {
        let client = MockClient::new(canned_order(&[0, 1, 1, 3]));
        let renderer = PromptRenderer::new().unwrap();
        let abs = five_abstractions();
        let identify = IdentifyResult::new(abs[..4].to_vec());
        let relationships = sample_relationships();
        let config = sample_config();
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect_err("duplicate index should error");
        assert!(
            matches!(
                err,
                OrderError::Validation(ChapterOrderError::DuplicateIndex { index: 1 })
            ),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn out_of_bounds_returns_validation_error() {
        let client = MockClient::new(canned_order(&[0, 1, 2, 5]));
        let renderer = PromptRenderer::new().unwrap();
        let abs = five_abstractions();
        let identify = IdentifyResult::new(abs[..4].to_vec());
        let relationships = sample_relationships();
        let config = sample_config();
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect_err("out of bounds should error");
        assert!(
            matches!(
                err,
                OrderError::Validation(ChapterOrderError::OutOfBounds { index: 5, count: 4 })
            ),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn single_abstraction_valid() {
        let client = MockClient::new(canned_order(&[0]));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![Abstraction::new(
            "Solo",
            "The only one",
            Tier::S,
            "module",
        )]);
        let relationships = RelationshipsResult::new("Solo project.", vec![]);
        let config = sample_config();
        let result = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("single abstraction should succeed");
        assert_eq!(result.ordered_indices, vec![0]);
    }

    #[tokio::test]
    async fn empty_abstraction_list_valid_edge_case() {
        let client = MockClient::new("```yaml\n[]\n```\n".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![]);
        let relationships = RelationshipsResult::new(String::new(), vec![]);
        let config = sample_config();
        let result = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("empty abstraction list should succeed");
        assert!(result.ordered_indices.is_empty());
    }

    #[tokio::test]
    async fn malformed_yaml_returns_typed_parse_error() {
        let client = MockClient::new("```yaml\n- [unclosed\n```".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect_err("malformed yaml should error");
        assert!(matches!(err, OrderError::Parse(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn no_yaml_block_returns_extract_error() {
        let client = MockClient::new("just prose, no structure".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect_err("no block should error");
        assert!(matches!(err, OrderError::Extract(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new(canned_order(&[3, 1, 0, 4, 2])).fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect_err("llm failure should propagate");
        assert!(
            matches!(err, OrderError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn secrets_redacted_before_rendering() {
        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl llm_kernel::llm::LLMClient for CapturingClient {
            async fn complete(
                &self,
                request: llm_kernel::llm::LLMRequest,
            ) -> llm_kernel::error::Result<llm_kernel::llm::LLMResponse> {
                let prompt = crate::llm::request_prompt(&request);
                let result: Result<String, crate::llm::LlmError> = async {
                    *self.captured.lock().unwrap() = prompt.to_string();
                    Ok(canned_order(&[0]))
                }
                .await;
                match result {
                    Ok(s) => Ok(crate::llm::text_response(s)),
                    Err(e) => Err(e.into_kernel()),
                }
            }
            fn model_name(&self) -> &str {
                "mock"
            }
            async fn stream_complete(
                &self,
                _request: llm_kernel::llm::LLMRequest,
            ) -> llm_kernel::error::Result<llm_kernel::llm::LLMStream> {
                crate::llm::stream_unsupported()
            }
        }
        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let identify =
            IdentifyResult::new(vec![Abstraction::new("Core", "desc", Tier::S, "module")]);
        let relationships = RelationshipsResult::new(
            "DB_KEY=super-secret summary",
            vec![Relationship::new(0, 0, "API_KEY=hush", "calls")],
        );
        let config = sample_config();
        let _ = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        assert!(
            !prompt.contains("super-secret"),
            "secret leaked into prompt: {prompt}"
        );
        assert!(
            !prompt.contains("hush"),
            "secret leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("DB_KEY=****"),
            "secret not redacted in prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn progress_tracker_budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_order(&[3, 1, 0, 4, 2]));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let mut progress = ProgressTracker::new(0);
        let err = order_chapters(
            &client,
            &renderer,
            &identify,
            &relationships,
            &config,
            &mut progress,
        )
        .await
        .expect_err("budget exceeded should error");
        assert!(matches!(err, OrderError::Budget(_)), "got: {err:?}");
        assert_eq!(client.call_count(), 0);
    }

    // --- checkpoint integration ---

    #[test]
    fn save_order_result_populates_and_marks_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = ChapterOrder::new(vec![2, 0, 1, 4, 3]);
        save_order_result(&store, &mut cp, &result).expect("save should succeed");
        assert!(cp.order.is_some());
        assert!(cp.is_stage_complete(StageId::Order));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_order_result_round_trips_via_load() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = ChapterOrder::new(vec![2, 0, 1, 4, 3]);
        save_order_result(&store, &mut cp, &result).expect("save should succeed");
        let (loaded, _) = store.load().expect("load should succeed");
        let loaded_result = load_order_result(&loaded).expect("should have order");
        assert_eq!(loaded_result, result);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_run_order_fresh_returns_true() {
        let cp = fresh_checkpoint();
        assert!(should_run_order(&cp));
    }

    #[test]
    fn should_run_order_complete_returns_false() {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Order, "2026-07-24T00:05:00Z");
        assert!(!should_run_order(&cp));
    }

    #[test]
    fn load_order_result_without_data_returns_none() {
        let cp = fresh_checkpoint();
        assert!(load_order_result(&cp).is_none());
    }

    #[tokio::test]
    async fn order_and_checkpoint_fresh_runs_and_saves() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new(canned_order(&[3, 1, 0, 4, 2]));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = order_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &relationships,
            &config,
            &mut progress,
        )
        .await
        .expect("should succeed");
        assert_eq!(result.ordered_indices, vec![3, 1, 0, 4, 2]);
        assert!(cp.is_stage_complete(StageId::Order));
        assert!(cp.order.is_some());
        assert_eq!(client.call_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn order_and_checkpoint_complete_skips_and_loads_existing() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let existing = ChapterOrder::new(vec![0, 1, 2, 3, 4]);
        save_order_result(&store, &mut cp, &existing).expect("seed save should succeed");

        let client = MockClient::new(canned_order(&[3, 1, 0, 4, 2]));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(five_abstractions());
        let relationships = sample_relationships();
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = order_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &relationships,
            &config,
            &mut progress,
        )
        .await
        .expect("skip should succeed");
        assert_eq!(client.call_count(), 0);
        assert_eq!(result, existing);
        let _ = fs::remove_dir_all(&dir);
    }

    // --- helper tests ---

    #[test]
    fn format_abstraction_listing_includes_tier_kind_description() {
        let abs = five_abstractions();
        let listing = format_abstraction_listing(&abs);
        assert!(
            listing.contains("0 # Router (tier=M, kind=module)"),
            "{listing}"
        );
        assert!(listing.contains("Routes requests"), "{listing}");
        assert!(
            listing.contains("3 # Config (tier=S, kind=config)"),
            "{listing}"
        );
    }

    #[test]
    fn format_abstraction_listing_redacts_secrets() {
        let abs = vec![Abstraction {
            name: "DB_KEY=super-secret".into(),
            description: "API_KEY=hush config".into(),
            file_indices: vec![],
            tier: Tier::S,
            kind: brigid_core::AbstractionKind::new("config"),
            apps: vec![],
            entry_files: vec![],
        }];
        let listing = format_abstraction_listing(&abs);
        assert!(
            !listing.contains("super-secret"),
            "secret leaked into listing: {listing}"
        );
        assert!(
            !listing.contains("hush"),
            "secret leaked into listing: {listing}"
        );
        assert!(
            listing.contains("DB_KEY=****"),
            "secret not redacted in listing: {listing}"
        );
    }

    #[test]
    fn build_order_context_includes_summary_and_relationships() {
        let rel = sample_relationships();
        let ctx = build_order_context(&rel);
        assert!(ctx.contains("Project summary:"), "{ctx}");
        assert!(ctx.contains("web framework"), "{ctx}");
        assert!(ctx.contains("0 -> 1"), "{ctx}");
        assert!(ctx.contains("routes to"), "{ctx}");
        assert!(ctx.contains("calls"), "{ctx}");
    }

    #[test]
    fn build_order_context_no_relationships_omits_section() {
        let rel = RelationshipsResult::new("Just a summary.", vec![]);
        let ctx = build_order_context(&rel);
        assert!(ctx.contains("Just a summary."), "{ctx}");
        assert!(!ctx.contains("Relationships:"), "{ctx}");
    }

    #[test]
    fn now_iso8601_utc_is_valid_format() {
        let ts = now_iso8601_utc();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
    }
}
