//! Pure domain models and pipeline traits for `decon`.
//!
//! This crate intentionally performs no network or filesystem I/O so it stays
//! trivially unit-testable. Crawling lives in `decon-crawl`, provider clients
//! in `decon-llm`, and stage orchestration in `decon-pipeline`.
//!
//! Domain types (`FileBlob`, `ModuleKey`, `Abstraction`, `Relationship`,
//! `Chapter`, `RunConfig`, `Checkpoint`, ...) and pure helpers (context
//! budgeting, monorepo scope, mermaid sanitize/validate, setup-doc scoring)
//! land here incrementally, starting with Milestone 1 — see
//! `docs/move-to-rust.md` §2.3 and §4.1 for the full domain model.

#![deny(missing_docs)]

pub mod abstraction;
pub mod budget;
pub mod chapter;
pub mod checkpoint;
pub mod config;
pub mod diagrams;
pub mod eval;
pub mod extract;
pub mod generate;
pub mod i18n;
pub mod mermaid;
pub mod module;
pub mod progress;
pub mod scope;
pub mod secrets;
pub mod setup;

// Re-exports inherit rustdoc from `budget` (no extra crate-root docs required).
pub use abstraction::{
    Abstraction, AbstractionKind, IdentifyResult, Relationship, RelationshipsResult, Tier,
    TierParseError,
};
pub use budget::{
    BudgetConfig, BudgetEstimate, DEFAULT_BATCH_CHAR_BUDGET, DEFAULT_CHARS_PER_TOKEN,
    DEFAULT_MAX_FILE_CHARS, DEFAULT_MAX_FULL_FILES_PER_MODULE, FileSize, PATH_STUB_PREFIX,
    TRUNCATION_MARKER, TruncateResult, capped_file_chars, estimate_budget, path_stub,
    path_stub_chars, truncate_content,
};
pub use chapter::{Chapter, ChapterOrder, ChapterOrderError, ChapterResult};
pub use checkpoint::{
    CHECKPOINT_SCHEMA_VERSION, CheckpointError, CheckpointMeta, CheckpointV1,
    DEFAULT_MANIFEST_REL_PATH, ENCODING_BASE64, FileBundleRecord, ManifestPointer, StageId,
    StageOutputEntry, StageOutputs, config_hash, sha256_hex_prefixed,
};
pub use config::{
    ConfigError, DEFAULT_CONFIG_CHARS_PER_TOKEN, DEFAULT_MAX_LLM_CALLS, RunConfig,
    canonical_config_json, config_from_env_map, custom_host_warning, merge_allowed_hosts,
    parse_toml_config, parse_yaml_config, resolve_config, validate_hostname,
};
pub use diagrams::{
    DiagramEdge, learning_path_flowchart, module_inventory_flowchart, module_inventory_from_counts,
    system_map_flowchart,
};
pub use eval::{
    DEFAULT_EVAL_PASS_THRESHOLD, EvalChecks, EvalReport, TutorialFile, WEIGHT_EVIDENCE_FOOTER,
    WEIGHT_INDEX_MERMAID, WEIGHT_INDEX_PRESENT, WEIGHT_LINKS_RESOLVE, WEIGHT_MERMAID_VALID,
    WEIGHT_PATH_CITATIONS, WEIGHT_SETUP_OR_OVERVIEW, evaluate_tutorial,
};
pub use extract::{ExtractError, extract_json_block, extract_yaml_block};
pub use generate::{ArchitectureOverview, CombinedTutorial, SetupGuide};
pub use i18n::{ChromeStrings, Locale};
pub use mermaid::{
    MAX_LABEL_CHARS, MAX_SEQUENCE_PARTICIPANTS, ValidateResult, participant_line, sanitize_label,
    sanitize_markdown_mermaid_blocks, sanitize_mermaid, sequence_participant_lines, stable_node_id,
    validate_mermaid,
};
pub use module::{ModuleCount, ModuleKey, ROOT_MODULE, discover_modules, module_key};
pub use progress::{BudgetExceeded, ProgressSnapshot, ProgressTracker, StageTiming};
pub use scope::{
    FilterStats, ScopeFilterResult, filter_files_by_scope, is_shared_module, unscoped_filter_stats,
};
pub use secrets::{
    REDACTED_PLACEHOLDER, content_for_checkpoint, is_sensitive_path, path_is_sensitive,
    redact_content,
};
pub use setup::{
    CONFIG_FILE_NAMES, MIN_README_LEN, SHORT_README_PENALTY, SIGNAL_POINTS, SetupAssessment,
    SetupSignals, assess_setup,
};

/// The version of this crate, as declared in `Cargo.toml`.
///
/// Exposed for diagnostics (e.g. `decon --version` reporting per-crate
/// versions) without callers needing to parse `Cargo.toml` themselves.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
