//! AnalyzeRelationships pipeline stage (M4-REL-1).
//!
//! Produces a project summary and inter-abstraction relationships via a single
//! LLM call, using budgeted, diverse evidence selection from the crawl
//! inventory.
//!
//! # Flow
//!
//! 1. [`select_evidence_files`] picks a diverse, budget-capped set of file
//!    paths from the abstractions (pure, testable).
//! 2. [`analyze_relationships`] renders `analyze_relationships.md.j2`,
//!    redacts secrets from the file-contents context, calls the LLM, and parses
//!    the YAML output into a [`RelationshipsResult`].
//! 3. [`relationships_and_checkpoint`] wraps the above with checkpoint
//!    save/load and resume semantics, mirroring the identify stage.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use brigid_core::{
    Abstraction, CheckpointV1, ProgressTracker, RelationshipsResult, StageId, extract_yaml_block,
    redact_content,
};
use crate::llm::{LlmClient, bounded_complete_with_budget};
use serde::Deserialize;
use serde_json::json;

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};
use crate::resume;

/// Re-export of the core [`brigid_core::CheckpointError`] for ergonomic matching
/// at call sites that only depend on `brigid-pipeline`.
pub use brigid_core::CheckpointError as CoreCheckpointError;

/// Default context character budget for the relationships prompt evidence.
pub const DEFAULT_RELATIONSHIPS_BUDGET: usize = 80_000;

/// Errors returned by the relationships stage.
#[derive(Debug, thiserror::Error)]
pub enum RelationshipsError {
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
    /// The extracted YAML could not be parsed into a relationships result.
    #[error("failed to parse relationships from LLM output: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
    /// A relationship referenced an abstraction outside the identify result.
    #[error("relationship abstraction index {index} out of range (have {total} abstractions)")]
    EndpointOutOfRange {
        /// The invalid relationship endpoint.
        index: usize,
        /// Number of identified abstractions.
        total: usize,
    },
    /// A checkpoint save/load failed during the relationships stage.
    #[error("checkpoint error during relationships: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
    /// The configured LLM call budget was exceeded.
    #[error("budget exceeded: {0}")]
    Budget(#[from] brigid_core::BudgetExceeded),
}

/// One file in the crawl inventory, with its path and byte size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceFile {
    /// Relative repository path (POSIX `/`).
    pub path: String,
    /// Byte size of the file body.
    pub size: u64,
}

/// Configuration for the relationships stage, mapping directly to the
/// `analyze_relationships.md.j2` template variables.
#[derive(Clone, Debug)]
pub struct RelationshipsConfig {
    /// Project name.
    pub project_name: String,
    /// Language instruction (e.g. `"Use Spanish"` or `""`).
    pub language_instruction: String,
    /// Short language hint suffix appended to labels/summary in the template
    /// (e.g. `" (in Spanish)"` or `""`).
    pub lang_hint: String,
    /// Language note for the abstraction listing header (e.g. `""`).
    pub list_lang_note: String,
    /// Monorepo instruction (non-empty when the repo has multiple apps).
    pub monorepo_instruction: String,
    /// Context character budget for evidence selection.
    pub budget: usize,
}

impl Default for RelationshipsConfig {
    fn default() -> Self {
        Self {
            project_name: String::new(),
            language_instruction: String::new(),
            lang_hint: String::new(),
            list_lang_note: String::new(),
            monorepo_instruction: String::new(),
            budget: DEFAULT_RELATIONSHIPS_BUDGET,
        }
    }
}

/// Select a diverse, budget-capped set of evidence file paths for the
/// relationships prompt.
///
/// # Algorithm
///
/// 1. Each abstraction contributes at least one file (preferring
///    [`Abstraction::entry_files`], then files referenced by
///    [`Abstraction::file_indices`]). This is the minimum guarantee and is
///    honoured even when the budget is tight.
/// 2. Additional candidate files are added round-robin across abstractions
///    (which spreads selection across apps) until the total byte size reaches
///    `budget`.
/// 3. Paths are de-duplicated and returned in selection order.
///
/// # Errors
///
/// Never errors; returns an empty vector when there are no abstractions or no
/// candidates.
#[must_use]
pub fn select_evidence_files(
    abstractions: &[Abstraction],
    inventory: &[EvidenceFile],
    budget: usize,
) -> Vec<String> {
    if abstractions.is_empty() {
        return Vec::new();
    }

    // Build a path -> size lookup map for O(1) size queries during selection.
    let size_map: std::collections::HashMap<&str, u64> = inventory
        .iter()
        .map(|f| (f.path.as_str(), f.size))
        .collect();
    let size_of = |p: &str| -> u64 { size_map.get(p).copied().unwrap_or(0) };

    // Build per-abstraction candidate lists, preferring entry_files then
    // file_indices-mapped paths. De-duplicate within each list.
    let candidates: Vec<Vec<String>> = abstractions
        .iter()
        .map(|a| {
            let mut seen = BTreeSet::new();
            let mut cands: Vec<String> = Vec::new();
            for ef in &a.entry_files {
                if seen.insert(ef.clone()) {
                    cands.push(ef.clone());
                }
            }
            for &idx in &a.file_indices {
                if let Some(f) = inventory.get(idx) {
                    if seen.insert(f.path.clone()) {
                        cands.push(f.path.clone());
                    }
                }
            }
            cands
        })
        .collect();

    let mut selected: Vec<String> = Vec::new();
    let mut selected_set: BTreeSet<String> = BTreeSet::new();
    let mut total: u64 = 0;

    // Phase 1: minimum guarantee — one file per abstraction (round-robin by
    // abstraction index spreads selection across apps). This is honoured even
    // when the budget is tight.
    for cands in &candidates {
        for cand in cands {
            if selected_set.insert(cand.clone()) {
                selected.push(cand.clone());
                total = total.saturating_add(size_of(cand));
                break;
            }
        }
    }

    // Phase 2: fill — round-robin add remaining candidates while under budget.
    // Within each abstraction, candidates are scanned until one that both fits
    // and is unselected is found; later candidates are still considered when
    // an earlier one does not fit.
    let mut more = true;
    while more {
        more = false;
        for cands in &candidates {
            for cand in cands {
                if selected_set.contains(cand) {
                    continue;
                }
                let sz = size_of(cand);
                if total.saturating_add(sz) <= budget as u64 {
                    selected_set.insert(cand.clone());
                    selected.push(cand.clone());
                    total = total.saturating_add(sz);
                    more = true;
                    break;
                }
                // This candidate does not fit; keep scanning the rest of this
                // abstraction's candidates for one that does.
            }
        }
    }

    selected
}

/// Run the AnalyzeRelationships stage: render the prompt, call the LLM, and
/// parse the YAML output into a [`RelationshipsResult`].
///
/// `file_contents` is a slice of `(path, content)` pairs from the crawl
/// inventory. Evidence files are selected via [`select_evidence_files`], their
/// contents are joined into the prompt `context`, and secrets are redacted
/// with [`redact_content`] before rendering.
///
/// # Errors
///
/// Returns [`RelationshipsError`] for prompt render failures, LLM call
/// failures, YAML extraction/parse failures, or budget overruns.
pub async fn analyze_relationships(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    identify: &brigid_core::IdentifyResult,
    file_contents: &[(String, String)],
    config: &RelationshipsConfig,
    progress: &mut ProgressTracker,
) -> Result<RelationshipsResult, RelationshipsError> {
    let abstractions = &identify.abstractions;

    // a. Build the evidence inventory from file contents and select a diverse,
    //    budget-capped set of evidence files.
    let inventory: Vec<EvidenceFile> = file_contents
        .iter()
        .map(|(path, content)| EvidenceFile {
            path: path.clone(),
            size: content.len() as u64,
        })
        .collect();
    let selected = select_evidence_files(abstractions, &inventory, config.budget);

    // b. Build the prompt `context` from the selected evidence files' contents,
    //    redacting secrets from every file body before it enters the prompt.
    let context = build_evidence_context(&selected, file_contents);

    // c. Build the abstraction listing (`idx # Name` per line) and derive the
    //    monorepo instruction when the config does not supply one.
    let abstraction_listing = format_abstraction_listing(abstractions);
    let monorepo_instruction = if config.monorepo_instruction.is_empty() {
        monorepo_instruction_from_abstractions(abstractions)
    } else {
        config.monorepo_instruction.clone()
    };

    // d. Build the render context. Free-text variables are sanitized so
    //    untrusted input cannot execute as Jinja template code. The context
    //    blob is also sanitized: file contents are untrusted, and a value like
    //    `{{ 7 * 7 }}` would be evaluated as a Jinja expression when the outer
    //    template is rendered.
    let render_ctx = json!({
        "project_name": sanitize_template_input(&config.project_name),
        "list_lang_note": sanitize_template_input(&config.list_lang_note),
        "abstraction_listing": sanitize_template_input(&abstraction_listing),
        "context": sanitize_template_input(&context),
        "language_instruction": sanitize_template_input(&config.language_instruction),
        "monorepo_instruction": sanitize_template_input(&monorepo_instruction),
        "lang_hint": sanitize_template_input(&config.lang_hint),
    });

    // e. Render the prompt.
    let prompt = renderer.render(PromptId::AnalyzeRelationships, &render_ctx)?;

    // f. Call the LLM with bounded budget enforcement.
    progress.set_stage("relationships");
    let result = bounded_complete_with_budget(client, vec![prompt], 1, progress).await?;
    let response = result
        .into_iter()
        .next()
        .ok_or(RelationshipsError::EmptyOutput)??;

    // h. Extract the YAML block from the (possibly prose-wrapped) response.
    let yaml_text = extract_yaml_block(&response)?;

    // i. Parse the extracted YAML into a RelationshipsResult.
    let raw: RawRelationships = serde_yaml_ng::from_str(&yaml_text)?;
    let total = abstractions.len();
    let mut relationships = Vec::with_capacity(raw.relationships.len());
    for relationship in raw.relationships {
        for index in [relationship.from_abstraction, relationship.to_abstraction] {
            if index >= total {
                return Err(RelationshipsError::EndpointOutOfRange { index, total });
            }
        }
        relationships.push(brigid_core::Relationship::new(
            relationship.from_abstraction,
            relationship.to_abstraction,
            relationship.label,
            relationship.kind,
        ));
    }

    Ok(RelationshipsResult::new(raw.summary, relationships))
}

/// Verify LLM-produced relationships against structural call-graph data from
/// a graph provider (ADR 0016 T4).
///
/// For each relationship, checks if the graph provider's call graph confirms
/// or contradicts the `from`→`to` edge. Sets
/// [`brigid_core::Relationship::structurally_verified`] to:
/// - `Some(true)` — call graph confirms the edge
/// - `Some(false)` — call graph contradicts the edge
/// - `None` — no structural data for these abstractions
///
/// When the provider is [`brigid_core::NoneProvider`], all relationships stay
/// `None` — the function is a no-op.
///
/// Abstraction names are used as the lookup keys for
/// [`brigid_core::GraphProvider::relationship_exists`].
pub fn verify_relationships_with_graph(
    result: &mut RelationshipsResult,
    identify: &brigid_core::IdentifyResult,
    provider: &dyn brigid_core::GraphProvider,
) {
    for rel in &mut result.relationships {
        let from_name = identify.abstractions.get(rel.from).map(|a| a.name.as_str());
        let to_name = identify.abstractions.get(rel.to).map(|a| a.name.as_str());
        if let (Some(from), Some(to)) = (from_name, to_name) {
            rel.structurally_verified = provider.relationship_exists(from, to);
        }
    }
}

/// Save the relationships result to the checkpoint and mark the Relationships
/// stage complete.
///
/// # Errors
///
/// Returns [`CheckpointStoreError`] if the serialize, load, or atomic write
/// fails.
pub fn save_relationships_result(
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    result: &RelationshipsResult,
) -> Result<(), CheckpointStoreError> {
    let value = result.to_checkpoint_value()?;
    checkpoint.relationships = Some(value);
    checkpoint.mark_stage_complete(StageId::Relationships, now_iso8601_utc());
    let (_, files) = store.load()?;
    store.save(checkpoint.clone(), &files)?;
    Ok(())
}

/// Check if the relationships stage should run based on the checkpoint state.
///
/// Returns `true` if [`StageId::Relationships`] is not in `completed_stages`.
#[must_use]
pub fn should_run_relationships(checkpoint: &CheckpointV1) -> bool {
    resume::should_run(StageId::Relationships, checkpoint)
}

/// Load the relationships result from a checkpoint (if any).
///
/// Returns `None` if no relationships have been saved yet (or if the stored
/// value is corrupt).
#[must_use]
pub fn load_relationships_result(checkpoint: &CheckpointV1) -> Option<RelationshipsResult> {
    checkpoint
        .relationships
        .as_ref()
        .and_then(|v| RelationshipsResult::from_checkpoint_value(v.clone()).ok())
}

/// Run the full relationships stage with checkpoint persistence and resume.
///
/// # Flow
///
/// 1. Check [`should_run_relationships`] — if `false`, load and return the
///    existing result via [`load_relationships_result`].
/// 2. Run [`analyze_relationships`].
/// 3. Save the result via [`save_relationships_result`].
/// 4. Return the [`RelationshipsResult`].
///
/// # Errors
///
/// Returns [`RelationshipsError`] for prompt/LLM/parse failures, budget
/// overruns, or checkpoint persistence failures.
#[allow(clippy::too_many_arguments)]
pub async fn relationships_and_checkpoint(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    identify: &brigid_core::IdentifyResult,
    file_contents: &[(String, String)],
    config: &RelationshipsConfig,
    progress: &mut ProgressTracker,
) -> Result<RelationshipsResult, RelationshipsError> {
    // 1. Resume: skip if the stage is already complete.
    if !should_run_relationships(checkpoint) {
        if let Some(existing) = load_relationships_result(checkpoint) {
            return Ok(existing);
        }
        // Edge case: stage marked complete but no relationships payload. Fall
        // through and re-run.
    }

    // 2. Run the relationships analysis.
    let result =
        analyze_relationships(client, renderer, identify, file_contents, config, progress).await?;

    // 3. Save the result to the checkpoint.
    save_relationships_result(store, checkpoint, &result)?;

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

/// Intermediate deserialization struct for the LLM YAML output.
#[derive(Deserialize)]
struct RawRelationships {
    summary: String,
    #[serde(default)]
    relationships: Vec<RawRelationship>,
}

/// Intermediate deserialization struct for one relationship edge.
#[derive(Deserialize)]
struct RawRelationship {
    from_abstraction: usize,
    to_abstraction: usize,
    label: String,
    kind: String,
}

/// Build the `abstraction_listing` string (`idx # Name` per line).
fn format_abstraction_listing(abstractions: &[Abstraction]) -> String {
    abstractions
        .iter()
        .enumerate()
        .map(|(idx, a)| format!("{idx} # {}", a.name))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Derive a monorepo instruction from the distinct apps across abstractions.
fn monorepo_instruction_from_abstractions(abstractions: &[Abstraction]) -> String {
    let apps: BTreeSet<String> = abstractions
        .iter()
        .flat_map(|a| a.apps.iter().cloned())
        .filter(|a| !a.is_empty())
        .collect();
    if apps.len() > 1 {
        let list = apps.into_iter().collect::<Vec<_>>().join(", ");
        format!("This is a monorepo with apps: {list}. ")
    } else {
        String::new()
    }
}

/// Look up the byte size of a path in the inventory (test helper).
#[cfg(test)]
fn size_of(inventory: &[EvidenceFile], path: &str) -> u64 {
    inventory
        .iter()
        .find(|f| f.path == path)
        .map(|f| f.size)
        .unwrap_or(0)
}

/// Build the prompt `context` blob from the selected evidence files' contents.
///
/// Each file body is secret-redacted via [`redact_content`] before joining.
/// Files are emitted in selection order with a `# File: <path>` header.
fn build_evidence_context(selected: &[String], file_contents: &[(String, String)]) -> String {
    let mut out = String::new();
    for path in selected {
        let Some(content) = file_contents
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.as_str())
        else {
            continue;
        };
        let redacted = redact_content(content);
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("# File: ");
        out.push_str(path);
        out.push('\n');
        out.push_str(&redacted);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_store::records_from_files;
    use brigid_core::{Abstraction, IdentifyResult, RunConfig, Tier};
    use crate::llm::{LlmError, MockClient};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct CapturingClient {
        captured: Arc<Mutex<String>>,
        response: String,
    }
    #[async_trait::async_trait]
    impl llm_kernel::llm::LLMClient for CapturingClient {
        async fn complete(&self, request: llm_kernel::llm::LLMRequest) -> llm_kernel::error::Result<llm_kernel::llm::LLMResponse> {
                let prompt = crate::llm::request_prompt(&request);
                let result: Result<String, crate::llm::LlmError> = async {
            *self.captured.lock().unwrap() = prompt.to_string();
            Ok(self.response.clone())
                }.await;
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

    fn temp_dir() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-rel-ckpt-{n}-{seq}"));
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
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn three_abstractions_two_apps() -> Vec<Abstraction> {
        vec![
            Abstraction {
                name: "Router".into(),
                description: "Routes requests".into(),
                file_indices: vec![0],
                tier: Tier::M,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["apps/web/router.rs".into()],
            },
            Abstraction {
                name: "Store".into(),
                description: "Persistence layer".into(),
                file_indices: vec![1],
                tier: Tier::M,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["apps/web/store.rs".into()],
            },
            Abstraction {
                name: "Worker".into(),
                description: "Background jobs".into(),
                file_indices: vec![2],
                tier: Tier::S,
                kind: brigid_core::AbstractionKind::new("module"),
                apps: vec!["api".into()],
                entry_files: vec!["apps/api/worker.rs".into()],
            },
        ]
    }

    fn inventory_for(abstractions: &[Abstraction]) -> Vec<EvidenceFile> {
        abstractions
            .iter()
            .flat_map(|a| a.entry_files.iter().cloned())
            .map(|p| EvidenceFile { path: p, size: 100 })
            .collect()
    }

    fn canned_three_relationships() -> String {
        let yaml = "\
summary: |
  A small web framework with routing and persistence.
relationships:
  - from_abstraction: 0
    to_abstraction: 1
    label: \"Routes to\"
    kind: calls
  - from_abstraction: 2
    to_abstraction: 0
    label: \"Provides config\"
    kind: configures
  - from_abstraction: 1
    to_abstraction: 2
    label: \"Hands off\"
    kind: publishes
";
        format!("Here is the analysis:\n\n```yaml\n{yaml}```\n")
    }

    fn sample_config() -> RelationshipsConfig {
        RelationshipsConfig {
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang_hint: String::new(),
            list_lang_note: String::new(),
            monorepo_instruction: String::new(),
            budget: 10_000,
        }
    }

    fn file_contents_for(abstractions: &[Abstraction]) -> Vec<(String, String)> {
        abstractions
            .iter()
            .flat_map(|a| a.entry_files.iter().cloned())
            .map(|p| (p, "fn body() {}".to_string()))
            .collect()
    }

    // --- select_evidence_files ---

    #[test]
    fn each_abstraction_contributes_at_least_one_file() {
        let abstractions = three_abstractions_two_apps();
        let inventory = inventory_for(&abstractions);
        let selected = select_evidence_files(&abstractions, &inventory, 100_000);
        assert_eq!(selected.len(), 3, "got: {selected:?}");
        assert!(selected.contains(&"apps/web/router.rs".to_string()));
        assert!(selected.contains(&"apps/web/store.rs".to_string()));
        assert!(selected.contains(&"apps/api/worker.rs".to_string()));
    }

    #[test]
    fn evidence_selection_diversity_across_two_apps() {
        let abstractions = three_abstractions_two_apps();
        let inventory = inventory_for(&abstractions);
        let selected = select_evidence_files(&abstractions, &inventory, 100_000);
        let has_web = selected.iter().any(|p| p.starts_with("apps/web/"));
        let has_api = selected.iter().any(|p| p.starts_with("apps/api/"));
        assert!(has_web, "no web app file selected: {selected:?}");
        assert!(has_api, "no api app file selected: {selected:?}");
    }

    #[test]
    fn budget_cap_enforcement_total_does_not_exceed_budget() {
        let abstractions = three_abstractions_two_apps();
        // Add extra candidate files per abstraction so there is more to select
        // than the minimum.
        let mut abs = abstractions.clone();
        abs[0].entry_files.push("apps/web/router_extra.rs".into());
        abs[1].entry_files.push("apps/web/store_extra.rs".into());
        abs[2].entry_files.push("apps/api/worker_extra.rs".into());
        let mut inventory = inventory_for(&abs);
        // Ensure extras exist in inventory.
        for p in [
            "apps/web/router_extra.rs",
            "apps/web/store_extra.rs",
            "apps/api/worker_extra.rs",
        ] {
            if !inventory.iter().any(|f| f.path == p) {
                inventory.push(EvidenceFile {
                    path: p.to_string(),
                    size: 100,
                });
            }
        }
        // Budget: 3 minimum files (300) + room for 1 extra (100) = 400.
        let budget = 400;
        let selected = select_evidence_files(&abs, &inventory, budget);
        let total: u64 = selected.iter().map(|p| size_of(&inventory, p)).sum();
        assert!(
            total <= budget as u64,
            "total {total} exceeds budget {budget}: {selected:?}"
        );
        // Minimum guarantee still holds.
        assert!(selected.len() >= 3);
    }

    #[test]
    fn select_evidence_files_empty_abstractions_returns_empty() {
        let selected = select_evidence_files(&[], &[], 100_000);
        assert!(selected.is_empty());
    }

    #[test]
    fn select_evidence_files_falls_back_to_file_indices_when_no_entry_files() {
        let mut a = Abstraction::new("Core", "desc", Tier::S, "module");
        a.file_indices = vec![0];
        let inventory = vec![EvidenceFile {
            path: "src/core.rs".into(),
            size: 50,
        }];
        let selected = select_evidence_files(&[a], &inventory, 100_000);
        assert_eq!(selected, vec!["src/core.rs".to_string()]);
    }

    // --- analyze_relationships ---

    #[tokio::test]
    async fn happy_path_parses_summary_and_three_relationships() {
        let client = MockClient::new(canned_three_relationships());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let result =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect("happy path should succeed");
        assert!(!result.project_summary.is_empty());
        assert_eq!(result.relationships.len(), 3);
        assert_eq!(result.relationships[0].from, 0);
        assert_eq!(result.relationships[0].to, 1);
        assert_eq!(result.relationships[0].label, "Routes to");
        assert_eq!(result.relationships[0].kind, "calls");
        assert_eq!(result.relationships[2].kind, "publishes");
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn empty_relationships_list_is_valid_edge_case() {
        let yaml =
            "```yaml\nsummary: |\n  A project with no relationships.\nrelationships: []\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let result =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect("empty relationships should succeed");
        assert!(!result.project_summary.is_empty());
        assert!(result.relationships.is_empty());
    }

    #[tokio::test]
    async fn relationship_endpoint_out_of_range_returns_error() {
        let yaml = "\
```yaml
summary: |-
  A project with an invalid relationship.
relationships:
  - from_abstraction: 0
    to_abstraction: 3
    label: \"Missing target\"
    kind: calls
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let err =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect_err("out-of-range relationship endpoint should error");
        assert!(
            matches!(
                err,
                RelationshipsError::EndpointOutOfRange { index: 3, total: 3 }
            ),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn relationship_from_endpoint_out_of_range_returns_error() {
        let yaml = "\
```yaml
summary: |-
  A project with an invalid relationship source.
relationships:
  - from_abstraction: 3
    to_abstraction: 0
    label: \"Missing source\"
    kind: calls
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let err =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect_err("out-of-range from_abstraction endpoint should error");
        assert!(
            matches!(
                err,
                RelationshipsError::EndpointOutOfRange { index: 3, total: 3 }
            ),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn malformed_yaml_returns_typed_parse_error() {
        let yaml = "```yaml\nsummary: : :\nrelationships: - bad\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let err =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect_err("malformed yaml should error");
        assert!(matches!(err, RelationshipsError::Parse(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn no_yaml_block_returns_extract_error() {
        let client = MockClient::new("just prose, no structure".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let err =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect_err("no block should error");
        assert!(
            matches!(err, RelationshipsError::Extract(_)),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new(canned_three_relationships()).fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let err =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect_err("llm failure should propagate");
        assert!(
            matches!(err, RelationshipsError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn secrets_redacted_before_rendering() {
        // Single-abstraction fixture with a valid 0→0 endpoint. The shared
        // `canned_three_relationships()` references indices 1 and 2, which
        // would trip `EndpointOutOfRange` for a single abstraction. This
        // test verifies secret redaction in the prompt, not parsing.
        let single_abstraction_yaml = "\
summary: |
  Self-referential core.
relationships:
  - from_abstraction: 0
    to_abstraction: 0
    label: \"Internal\"
    kind: calls
";
        let response = format!("Here is the analysis:\n\n```yaml\n{single_abstraction_yaml}```\n");
        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
            response,
        };
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![Abstraction {
            name: "Core".into(),
            description: "desc".into(),
            file_indices: vec![0],
            tier: Tier::S,
            kind: brigid_core::AbstractionKind::new("module"),
            apps: vec![],
            entry_files: vec!["src/config.rs".into()],
        }]);
        let file_contents = vec![(
            "src/config.rs".to_string(),
            "DB_KEY=super-secret\nfn load() {}".to_string(),
        )];
        let config = sample_config();
        let _ = analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        assert!(
            !prompt.contains("super-secret"),
            "secret leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("DB_KEY=****"),
            "secret not redacted in prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn secrets_redacted_with_multiple_valid_relationships() {
        // Three abstractions with secrets in each entry file. The canned
        // response has three relationships referencing indices 0, 1, 2 —
        // all valid here — so redaction and multi-relationship parsing are
        // exercised together.
        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
            response: canned_three_relationships(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = vec![
            (
                "apps/web/router.rs".to_string(),
                "ROUTER_KEY=router-secret\nfn route() {}".to_string(),
            ),
            (
                "apps/web/store.rs".to_string(),
                "STORE_KEY=store-secret\nfn persist() {}".to_string(),
            ),
            (
                "apps/api/worker.rs".to_string(),
                "WORKER_KEY=worker-secret\nfn work() {}".to_string(),
            ),
        ];
        let config = sample_config();
        let result =
            analyze_relationships(&client, &renderer, &identify, &file_contents, &config, &mut ProgressTracker::new(10))
                .await
                .expect("multiple valid relationships should succeed");
        assert_eq!(result.relationships.len(), 3, "got: {result:?}");
        let prompt = captured.lock().unwrap().clone();
        for secret in ["router-secret", "store-secret", "worker-secret"] {
            assert!(
                !prompt.contains(secret),
                "secret leaked into prompt: {prompt}"
            );
        }
        for redacted in ["ROUTER_KEY=****", "STORE_KEY=****", "WORKER_KEY=****"] {
            assert!(
                prompt.contains(redacted),
                "secret not redacted in prompt: {prompt}"
            );
        }
    }

    #[tokio::test]
    async fn progress_tracker_records_the_call() {
        let client = MockClient::new(canned_three_relationships());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = analyze_relationships(
            &client,
            &renderer,
            &identify,
            &file_contents,
            &config,
            &mut progress,
        )
        .await
        .expect("should succeed with progress");
        assert_eq!(result.relationships.len(), 3);
        let snap = progress.snapshot();
        assert_eq!(snap.llm_calls_used, 1);
        assert_eq!(snap.llm_calls_remaining, 9);
    }

    #[tokio::test]
    async fn progress_tracker_budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_three_relationships());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let mut progress = ProgressTracker::new(0);
        let err = analyze_relationships(
            &client,
            &renderer,
            &identify,
            &file_contents,
            &config,
            &mut progress,
        )
        .await
        .expect_err("budget exceeded should error");
        assert!(matches!(err, RelationshipsError::Budget(_)), "got: {err:?}");
        assert_eq!(client.call_count(), 0);
    }

    // --- checkpoint integration ---

    #[test]
    fn save_relationships_result_populates_and_marks_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = RelationshipsResult::new(
            "A summary.",
            vec![brigid_core::Relationship::new(0, 1, "calls", "calls")],
        );
        save_relationships_result(&store, &mut cp, &result).expect("save should succeed");
        assert!(cp.relationships.is_some());
        assert!(cp.is_stage_complete(StageId::Relationships));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_relationships_result_round_trips_via_load() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let result = RelationshipsResult::new(
            "A summary.",
            vec![brigid_core::Relationship::new(0, 1, "calls", "calls")],
        );
        save_relationships_result(&store, &mut cp, &result).expect("save should succeed");
        let (loaded, _) = store.load().expect("load should succeed");
        let loaded_result = load_relationships_result(&loaded).expect("should have relationships");
        assert_eq!(loaded_result, result);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_run_relationships_fresh_returns_true() {
        let cp = fresh_checkpoint();
        assert!(should_run_relationships(&cp));
    }

    #[test]
    fn should_run_relationships_complete_returns_false() {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:05:00Z");
        assert!(!should_run_relationships(&cp));
    }

    #[test]
    fn load_relationships_result_without_data_returns_none() {
        let cp = fresh_checkpoint();
        assert!(load_relationships_result(&cp).is_none());
    }

    #[tokio::test]
    async fn relationships_and_checkpoint_fresh_runs_and_saves() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new(canned_three_relationships());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = relationships_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &file_contents,
            &config,
            &mut progress,
        )
        .await
        .expect("should succeed");
        assert_eq!(result.relationships.len(), 3);
        assert!(cp.is_stage_complete(StageId::Relationships));
        assert!(cp.relationships.is_some());
        assert_eq!(client.call_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn relationships_and_checkpoint_complete_skips_and_loads_existing() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let existing = RelationshipsResult::new(
            "Cached summary.",
            vec![brigid_core::Relationship::new(0, 1, "calls", "calls")],
        );
        save_relationships_result(&store, &mut cp, &existing).expect("seed save should succeed");

        let client = MockClient::new(canned_three_relationships());
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions_two_apps());
        let file_contents = file_contents_for(&identify.abstractions);
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = relationships_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &file_contents,
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
    fn format_abstraction_listing_is_indexed() {
        let abs = three_abstractions_two_apps();
        let listing = format_abstraction_listing(&abs);
        assert!(listing.contains("0 # Router"), "{listing}");
        assert!(listing.contains("1 # Store"), "{listing}");
        assert!(listing.contains("2 # Worker"), "{listing}");
    }

    #[test]
    fn monorepo_instruction_multi_app() {
        let abs = three_abstractions_two_apps();
        let instr = monorepo_instruction_from_abstractions(&abs);
        assert!(instr.contains("monorepo"), "{instr}");
        assert!(instr.contains("api"), "{instr}");
        assert!(instr.contains("web"), "{instr}");
    }

    #[test]
    fn monorepo_instruction_single_app_empty() {
        let abs = vec![Abstraction::new("A", "d", Tier::S, "module")];
        let instr = monorepo_instruction_from_abstractions(&abs);
        assert!(instr.is_empty());
    }

    #[test]
    fn now_iso8601_utc_is_valid_format() {
        let ts = now_iso8601_utc();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
    }

    // -----------------------------------------------------------------------
    // verify_relationships_with_graph (ADR 0016 T4)
    // -----------------------------------------------------------------------

    #[test]
    fn verify_relationships_with_none_provider_leaves_all_none() {
        let identify = IdentifyResult::new(vec![
            Abstraction::new("Auth", "desc", Tier::S, "module"),
            Abstraction::new("DB", "desc", Tier::S, "module"),
        ]);
        let mut result = RelationshipsResult::new(
            "summary".to_string(),
            vec![brigid_core::Relationship::new(0, 1, "uses", "calls")],
        );
        let provider = brigid_core::NoneProvider::new();
        verify_relationships_with_graph(&mut result, &identify, &provider);
        assert_eq!(result.relationships[0].structurally_verified, None);
    }

    #[test]
    fn verify_relationships_with_confirming_provider_sets_true() {
        struct ConfirmingProvider;
        impl brigid_core::GraphProvider for ConfirmingProvider {
            fn call_graph_for_file(&self, _: &str) -> Vec<brigid_core::CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<brigid_core::Community> {
                Vec::new()
            }
            fn hub_concepts(&self) -> Vec<String> {
                Vec::new()
            }
            fn relationship_exists(&self, from: &str, to: &str) -> Option<bool> {
                if from == "Auth" && to == "DB" {
                    Some(true)
                } else {
                    None
                }
            }
            fn multimodal_concepts(&self) -> Vec<brigid_core::MultimodalConcept> {
                Vec::new()
            }
            fn name(&self) -> &str {
                "confirming"
            }
        }

        let identify = IdentifyResult::new(vec![
            Abstraction::new("Auth", "desc", Tier::S, "module"),
            Abstraction::new("DB", "desc", Tier::S, "module"),
        ]);
        let mut result = RelationshipsResult::new(
            "summary".to_string(),
            vec![brigid_core::Relationship::new(0, 1, "uses", "calls")],
        );
        verify_relationships_with_graph(&mut result, &identify, &ConfirmingProvider);
        assert_eq!(result.relationships[0].structurally_verified, Some(true));
    }

    #[test]
    fn verify_relationships_with_contradicting_provider_sets_false() {
        struct ContradictingProvider;
        impl brigid_core::GraphProvider for ContradictingProvider {
            fn call_graph_for_file(&self, _: &str) -> Vec<brigid_core::CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<brigid_core::Community> {
                Vec::new()
            }
            fn hub_concepts(&self) -> Vec<String> {
                Vec::new()
            }
            fn relationship_exists(&self, from: &str, to: &str) -> Option<bool> {
                if from == "Auth" && to == "DB" {
                    Some(false)
                } else {
                    None
                }
            }
            fn multimodal_concepts(&self) -> Vec<brigid_core::MultimodalConcept> {
                Vec::new()
            }
            fn name(&self) -> &str {
                "contradicting"
            }
        }

        let identify = IdentifyResult::new(vec![
            Abstraction::new("Auth", "desc", Tier::S, "module"),
            Abstraction::new("DB", "desc", Tier::S, "module"),
        ]);
        let mut result = RelationshipsResult::new(
            "summary".to_string(),
            vec![brigid_core::Relationship::new(0, 1, "uses", "calls")],
        );
        verify_relationships_with_graph(&mut result, &identify, &ContradictingProvider);
        assert_eq!(result.relationships[0].structurally_verified, Some(false));
    }

    #[test]
    fn verify_relationships_with_no_match_leaves_none() {
        struct EmptyProvider;
        impl brigid_core::GraphProvider for EmptyProvider {
            fn call_graph_for_file(&self, _: &str) -> Vec<brigid_core::CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<brigid_core::Community> {
                Vec::new()
            }
            fn hub_concepts(&self) -> Vec<String> {
                Vec::new()
            }
            fn relationship_exists(&self, _: &str, _: &str) -> Option<bool> {
                None
            }
            fn multimodal_concepts(&self) -> Vec<brigid_core::MultimodalConcept> {
                Vec::new()
            }
            fn name(&self) -> &str {
                "empty"
            }
        }

        let identify = IdentifyResult::new(vec![
            Abstraction::new("Auth", "desc", Tier::S, "module"),
            Abstraction::new("DB", "desc", Tier::S, "module"),
        ]);
        let mut result = RelationshipsResult::new(
            "summary".to_string(),
            vec![brigid_core::Relationship::new(0, 1, "uses", "calls")],
        );
        verify_relationships_with_graph(&mut result, &identify, &EmptyProvider);
        assert_eq!(result.relationships[0].structurally_verified, None);
    }

    #[test]
    fn verify_relationships_with_out_of_range_indices_leaves_none() {
        struct ConfirmingProvider;
        impl brigid_core::GraphProvider for ConfirmingProvider {
            fn call_graph_for_file(&self, _: &str) -> Vec<brigid_core::CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<brigid_core::Community> {
                Vec::new()
            }
            fn hub_concepts(&self) -> Vec<String> {
                Vec::new()
            }
            fn relationship_exists(&self, _: &str, _: &str) -> Option<bool> {
                Some(true)
            }
            fn multimodal_concepts(&self) -> Vec<brigid_core::MultimodalConcept> {
                Vec::new()
            }
            fn name(&self) -> &str {
                "confirming"
            }
        }

        let identify =
            IdentifyResult::new(vec![Abstraction::new("Auth", "desc", Tier::S, "module")]);
        let mut result = RelationshipsResult::new(
            "summary".to_string(),
            vec![brigid_core::Relationship::new(0, 5, "uses", "calls")],
        );
        verify_relationships_with_graph(&mut result, &identify, &ConfirmingProvider);
        // to=5 is out of range, so no verification happens
        assert_eq!(result.relationships[0].structurally_verified, None);
    }

    #[test]
    fn verify_relationships_with_multiple_relationships() {
        struct MultiProvider;
        impl brigid_core::GraphProvider for MultiProvider {
            fn call_graph_for_file(&self, _: &str) -> Vec<brigid_core::CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<brigid_core::Community> {
                Vec::new()
            }
            fn hub_concepts(&self) -> Vec<String> {
                Vec::new()
            }
            fn relationship_exists(&self, from: &str, to: &str) -> Option<bool> {
                match (from, to) {
                    ("Auth", "DB") => Some(true),
                    ("DB", "Cache") => Some(false),
                    _ => None,
                }
            }
            fn multimodal_concepts(&self) -> Vec<brigid_core::MultimodalConcept> {
                Vec::new()
            }
            fn name(&self) -> &str {
                "multi"
            }
        }

        let identify = IdentifyResult::new(vec![
            Abstraction::new("Auth", "desc", Tier::S, "module"),
            Abstraction::new("DB", "desc", Tier::S, "module"),
            Abstraction::new("Cache", "desc", Tier::S, "module"),
        ]);
        let mut result = RelationshipsResult::new(
            "summary".to_string(),
            vec![
                brigid_core::Relationship::new(0, 1, "uses", "calls"),
                brigid_core::Relationship::new(1, 2, "writes", "calls"),
                brigid_core::Relationship::new(0, 2, "invalidates", "related"),
            ],
        );
        verify_relationships_with_graph(&mut result, &identify, &MultiProvider);
        assert_eq!(result.relationships[0].structurally_verified, Some(true));
        assert_eq!(result.relationships[1].structurally_verified, Some(false));
        assert_eq!(result.relationships[2].structurally_verified, None);
    }

    #[test]
    fn relationship_structurally_verified_default_is_none() {
        let rel = brigid_core::Relationship::new(0, 1, "uses", "calls");
        assert_eq!(rel.structurally_verified, None);
    }

    #[test]
    fn relationship_with_structurally_verified_round_trips() {
        let mut rel = brigid_core::Relationship::new(0, 1, "uses", "calls");
        rel.structurally_verified = Some(true);
        let json = serde_json::to_string(&rel).unwrap();
        assert!(json.contains("\"structurally_verified\":true"));
        let back: brigid_core::Relationship = serde_json::from_str(&json).unwrap();
        assert_eq!(back.structurally_verified, Some(true));
    }

    #[test]
    fn relationship_without_structurally_verified_omits_field() {
        let rel = brigid_core::Relationship::new(0, 1, "uses", "calls");
        let json = serde_json::to_string(&rel).unwrap();
        assert!(!json.contains("structurally_verified"));
    }
}
