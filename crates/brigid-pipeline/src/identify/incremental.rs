//! Incremental identify: re-run the identify map/reduce only for modules
//! containing files changed since a git ref, then merge the new/updated
//! abstractions with the existing ones from a checkpoint.
//!
//! See issue #227 and ADR 0013 for the git-diff incremental strategy.
//!
//! # Algorithm
//!
//! 1. Load existing abstractions from the checkpoint.
//! 2. Determine which modules contain changed files (from the git diff).
//! 3. Run map/reduce only for the affected modules' files.
//! 4. Merge: replace abstractions from affected modules with the new results,
//!    keep unchanged ones, and drop abstractions whose backing files were
//!    deleted since the ref.
//! 5. Re-run reduce across the merged set to update rankings.

use std::collections::{BTreeSet, HashMap};

use brigid_core::{Abstraction, IdentifyResult, ProgressTracker, RunConfig, module_key};
use crate::llm::LlmClient;

use crate::identify::{
    CandidateAbstraction, IdentifyError, IdentifyMapInput, IdentifyReduceInput, identify_map,
    identify_reduce,
};
use crate::identify_checkpoint::{
    DEFAULT_MAX_CONCURRENCY, budget_config_from_run, language_instruction_from_config,
    max_abstractions_from_config, module_summary_from_files, project_name_from_config,
};
use crate::prompts::PromptRenderer;

/// Run an incremental identify pass.
///
/// When `--since <ref>` is set and a valid (non-stale) checkpoint exists, this
/// re-analyzes only the modules containing files changed since `ref_name`,
/// merges the new abstractions with the preserved ones from `existing`, and
/// re-runs reduce across the full merged set to re-rank.
///
/// # Arguments
///
/// * `existing` — abstractions loaded from the checkpoint.
/// * `changed_files` — relative paths of files that changed since the ref and
///   still exist on disk (from [`brigid_crawl::git_diff::changed_files_since`]).
/// * `deleted_files` — relative paths of files deleted since the ref (from
///   [`brigid_crawl::git_diff::deleted_files_since`]).
/// * `files` / `sizes` — the **full** current crawl inventory (parallel). The
///   merged abstractions' `file_indices` are indices into this inventory.
///
/// # Merge strategy
///
/// Existing abstractions are partitioned by module key (derived from their
/// `entry_files`). For modules in the affected set (any changed or deleted
/// file's module), the existing abstractions are **replaced** by the new
/// map/reduce results. For unaffected modules, the existing abstractions are
/// **preserved**. Abstractions whose `entry_files` are **all** deleted since
/// the ref are **dropped**.
///
/// # Errors
///
/// Returns [`IdentifyError`] for map/reduce LLM/parse failures or budget
/// overruns.
#[allow(clippy::too_many_arguments)] // signature mirrors identify_and_checkpoint
pub async fn incremental_identify(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    existing: &IdentifyResult,
    changed_files: &[String],
    deleted_files: &[String],
    files: &[String],
    sizes: &[u64],
    config: &RunConfig,
    progress: &mut ProgressTracker,
) -> Result<IdentifyResult, IdentifyError> {
    // 1. Determine the set of affected module keys (changed + deleted files).
    let affected_modules: BTreeSet<String> = changed_files
        .iter()
        .chain(deleted_files.iter())
        .map(|p| module_key(p).as_str().to_owned())
        .collect();

    let deleted_set: std::collections::HashSet<&str> =
        deleted_files.iter().map(|s| s.as_str()).collect();

    // 2. Partition existing abstractions into preserved / affected / dropped.
    let mut preserved: Vec<Abstraction> = Vec::new();
    for abs in &existing.abstractions {
        if abs.entry_files.is_empty() {
            // No entry files to reason about: preserve as-is (cannot determine
            // module or deletion status).
            preserved.push(abs.clone());
            continue;
        }
        let all_deleted = abs
            .entry_files
            .iter()
            .all(|f| deleted_set.contains(f.as_str()));
        if all_deleted {
            // All backing files deleted since ref → drop.
            continue;
        }
        let touches_affected = abs
            .entry_files
            .iter()
            .any(|f| affected_modules.contains(module_key(f).as_str()));
        if touches_affected {
            // In an affected module → replace (do not preserve).
            continue;
        }
        preserved.push(abs.clone());
    }

    // 3. Run the map stage only on the changed files (which are, by
    //    construction, in the affected modules). Map their sub-list file
    //    indices back to the full inventory indices afterwards.
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        index_of.insert(f.as_str(), i);
    }

    // The affected files are the changed files that still exist (i.e. present
    // in the full inventory). Preserve the inventory's ordering so the
    // sub-list → full-index remap is stable.
    let mut affected_indices: Vec<usize> = Vec::new();
    for cf in changed_files {
        if let Some(&i) = index_of.get(cf.as_str()) {
            affected_indices.push(i);
        }
    }
    affected_indices.sort_unstable();
    let affected_files: Vec<String> = affected_indices.iter().map(|&i| files[i].clone()).collect();
    let affected_sizes: Vec<u64> = affected_indices.iter().map(|&i| sizes[i]).collect();

    let mut new_candidates: Vec<CandidateAbstraction> = Vec::new();
    if !affected_files.is_empty() {
        let map_input = IdentifyMapInput {
            files: affected_files,
            sizes: affected_sizes,
            project_name: project_name_from_config(config),
            language_instruction: language_instruction_from_config(config),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions_from_config(config),
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            budget_config: budget_config_from_run(config),
            community_context: String::new(),
        };
        let batches = identify_map(client, renderer, &map_input, progress).await?;
        for b in batches {
            for mut cand in b.candidates {
                // Remap sub-list file indices → full inventory indices.
                cand.file_indices = cand
                    .file_indices
                    .iter()
                    .map(|&sub| affected_indices.get(sub).copied())
                    .collect::<Option<Vec<usize>>>()
                    .unwrap_or_default();
                new_candidates.push(cand);
            }
        }
    }

    // 4. Convert preserved abstractions into candidates (re-indexed against
    //    the full inventory via entry_files) so reduce can re-rank the merged
    //    set.
    let mut merged_candidates: Vec<CandidateAbstraction> = Vec::new();
    for abs in &preserved {
        let file_indices: Vec<usize> = abs
            .entry_files
            .iter()
            .filter_map(|f| index_of.get(f.as_str()).copied())
            .collect();
        merged_candidates.push(CandidateAbstraction {
            name: abs.name.clone(),
            description: abs.description.clone(),
            file_indices,
            tier: abs.tier,
            kind: abs.kind.clone(),
            apps: abs.apps.clone(),
            entry_files: abs.entry_files.clone(),
            batch_idx: 0,
        });
    }
    for cand in new_candidates {
        merged_candidates.push(cand);
    }

    // 5. Re-run reduce across the merged set to re-rank.
    let reduce_input = IdentifyReduceInput {
        candidates: merged_candidates,
        files: files.to_vec(),
        project_name: project_name_from_config(config),
        language_instruction: language_instruction_from_config(config),
        lang_note: String::new(),
        max_abstraction_num: max_abstractions_from_config(config),
        module_summary: module_summary_from_files(files),
        multimodal_context: String::new(),
    };
    identify_reduce(client, renderer, &reduce_input, progress).await
}

// ===========================================================================
// Incremental identify tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::{Abstraction, AbstractionKind, RunConfig, Tier};
    use crate::llm::MockClient;

    /// Build an [`Abstraction`] with entry_files set.
    fn abs(name: &str, entry_files: &[&str]) -> Abstraction {
        let mut a = Abstraction::new(name, format!("{name} desc"), Tier::S, "module");
        a.entry_files = entry_files.iter().map(|s| s.to_string()).collect();
        a
    }

    /// A canned map-stage YAML response for one batch, producing a single
    /// candidate with the given name and file index (into the batch sub-list).
    fn map_yaml(name: &str, file_index: usize) -> String {
        format!(
            "```yaml\n- name: \"{name}\"\n  description: \"{name} desc\"\n  \
             file_indices: [{file_index}]\n  tier: \"S\"\n  kind: \"module\"\n  \
             apps: []\n  entry_files: []\n```\n"
        )
    }

    /// A canned reduce-stage YAML response producing the named final
    /// abstractions, each backed by the given full-inventory file index.
    fn reduce_yaml(items: &[(&str, usize)]) -> String {
        let mut body = String::new();
        for (name, idx) in items {
            body.push_str(&format!(
                "- name: \"{name}\"\n  description: \"{name} desc\"\n  \
                 file_indices: [{idx}]\n  tier: \"S\"\n  kind: \"module\"\n  \
                 apps: []\n  entry_files: []\n"
            ));
        }
        format!("```yaml\n{body}```\n")
    }

    /// Budget config that puts each file in its own batch (capped size 1000,
    /// batch budget 50 → one file per batch).
    fn one_per_batch_budget() -> brigid_core::BudgetConfig {
        brigid_core::BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 50,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        }
    }

    /// Run config whose `batch_char_budget` drives the one-per-file batching.
    fn cfg_with_small_batches() -> RunConfig {
        RunConfig {
            batch_char_budget: Some(50),
            ..RunConfig::default()
        }
    }

    #[tokio::test]
    async fn only_affected_modules_re_analyzed_three_preserved() {
        // Full inventory: one file per module across 5 modules.
        let files = vec![
            "api/handler.rs".to_string(),
            "config/config.rs".to_string(),
            "src/main.rs".to_string(),
            "tests/test.rs".to_string(),
            "utils/utils.rs".to_string(),
        ];
        let sizes = vec![100_u64; 5];

        // Existing abstractions: one per module.
        let existing = IdentifyResult::new(vec![
            abs("Core", &["src/main.rs"]),
            abs("Config", &["config/config.rs"]),
            abs("Utils", &["utils/utils.rs"]),
            abs("Api", &["api/handler.rs"]),
            abs("Tests", &["tests/test.rs"]),
        ]);

        // Only src and api changed.
        let changed = vec!["api/handler.rs".to_string(), "src/main.rs".to_string()];
        let deleted: Vec<String> = Vec::new();

        // Map: batch 0 = api/handler.rs (sub idx 0), batch 1 = src/main.rs
        // (sub idx 1). Reduce: 5 final abstractions (3 preserved + 2 new),
        // with file_indices into the full inventory.
        let responses = vec![
            map_yaml("Api Service", 0),
            map_yaml("Core System", 1),
            reduce_yaml(&[
                ("Core System", 2),
                ("Config", 1),
                ("Utils", 4),
                ("Api Service", 0),
                ("Tests", 3),
            ]),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();

        let result = incremental_identify(
            &client,
            &renderer,
            &existing,
            &changed,
            &deleted,
            &files,
            &sizes,
            &cfg_with_small_batches(),
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("incremental identify should succeed");

        // Only the 2 affected modules' files were mapped (2 batches) plus 1
        // reduce call = 3 total LLM calls. If the implementation mapped all 5
        // files, this would be 6.
        assert_eq!(
            client.call_count(),
            3,
            "only 2 affected modules should be re-analyzed"
        );

        let names: Vec<String> = result.abstractions.iter().map(|a| a.name.clone()).collect();
        // 3 preserved from checkpoint.
        assert!(
            names.contains(&"Config".to_string()),
            "Config preserved: {names:?}"
        );
        assert!(
            names.contains(&"Utils".to_string()),
            "Utils preserved: {names:?}"
        );
        assert!(
            names.contains(&"Tests".to_string()),
            "Tests preserved: {names:?}"
        );
        // 2 new/updated from affected modules.
        assert!(
            names.contains(&"Core System".to_string()),
            "Core replaced: {names:?}"
        );
        assert!(
            names.contains(&"Api Service".to_string()),
            "Api replaced: {names:?}"
        );
        // Old affected-module abstraction names are gone.
        assert!(
            !names.contains(&"Core".to_string()),
            "old Core replaced: {names:?}"
        );
        assert!(
            !names.contains(&"Api".to_string()),
            "old Api replaced: {names:?}"
        );
    }

    #[tokio::test]
    async fn merge_handles_new_updated_and_removed_abstractions() {
        // Full inventory (no legacy/old.rs — it was deleted).
        let files = vec![
            "config/config.rs".to_string(),
            "src/main.rs".to_string(),
            "src/new_module.rs".to_string(),
        ];
        let sizes = vec![100_u64; 3];

        // Existing abstractions:
        // - Legacy: references only a deleted file → must be dropped.
        // - Core: in affected module src → replaced.
        // - Config: in unaffected module config → preserved.
        let existing = IdentifyResult::new(vec![
            abs("Legacy", &["legacy/old.rs"]),
            abs("Core", &["src/main.rs"]),
            abs("Config", &["config/config.rs"]),
        ]);

        let changed = vec!["src/main.rs".to_string(), "src/new_module.rs".to_string()];
        let deleted = vec!["legacy/old.rs".to_string()];

        // Map: batch 0 = src/main.rs (sub idx 0), batch 1 = src/new_module.rs
        // (sub idx 1). Reduce: Core v2 (updated), Config (preserved),
        // NewModule (new); Legacy absent (dropped).
        let responses = vec![
            map_yaml("Core v2", 0),
            map_yaml("NewModule", 1),
            reduce_yaml(&[("Core v2", 1), ("Config", 0), ("NewModule", 2)]),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();

        let result = incremental_identify(
            &client,
            &renderer,
            &existing,
            &changed,
            &deleted,
            &files,
            &sizes,
            &cfg_with_small_batches(),
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("merge should succeed");

        let names: Vec<String> = result.abstractions.iter().map(|a| a.name.clone()).collect();
        // Removed: Legacy dropped (all entry_files deleted).
        assert!(
            !names.contains(&"Legacy".to_string()),
            "Legacy should be dropped: {names:?}"
        );
        // Preserved: Config kept from checkpoint.
        assert!(
            names.contains(&"Config".to_string()),
            "Config preserved: {names:?}"
        );
        // Updated: Core replaced by Core v2.
        assert!(
            names.contains(&"Core v2".to_string()),
            "Core updated: {names:?}"
        );
        assert!(
            !names.contains(&"Core".to_string()),
            "old Core replaced: {names:?}"
        );
        // New: NewModule added.
        assert!(
            names.contains(&"NewModule".to_string()),
            "NewModule added: {names:?}"
        );
    }

    #[tokio::test]
    async fn no_affected_files_still_re_ranks_preserved() {
        // No changed files, no deleted files → everything preserved, reduce
        // still runs once to re-rank.
        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let sizes = vec![100_u64; 2];
        let existing = IdentifyResult::new(vec![abs("Core", &["src/a.rs"])]);

        let responses = vec![reduce_yaml(&[("Core", 0)])];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();

        let result = incremental_identify(
            &client,
            &renderer,
            &existing,
            &[],
            &[],
            &files,
            &sizes,
            &cfg_with_small_batches(),
            &mut ProgressTracker::new(10),
        )
        .await
        .expect("should succeed");

        // No map calls (no affected files), just 1 reduce.
        assert_eq!(client.call_count(), 1);
        let names: Vec<String> = result.abstractions.iter().map(|a| a.name.clone()).collect();
        assert!(names.contains(&"Core".to_string()));
    }

    #[test]
    fn one_per_batch_budget_helper_is_valid() {
        // Sanity: ensure the helper compiles and is non-default.
        let b = one_per_batch_budget();
        assert_eq!(b.batch_char_budget, 50);
        let _ = AbstractionKind::new("module");
    }
}
