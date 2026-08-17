//! Single-shot **identify** stage: one LLM call produces the full abstraction
//! list for small repos where map/reduce is unnecessary.
//!
//! This is the Rust port of the Python reference's `_single_shot_identify`
//! node. The function takes a [`crate::llm::LlmClient`] (so it works with
//! [`crate::llm::MockClient`] in tests and a real provider client in
//! production), renders the `identify_single_shot` prompt, calls the LLM,
//! extracts the YAML block, parses it into [`Abstraction`]s, and validates the
//! `file_indices` against the crawl inventory.
//!
//! Caching is intentionally NOT handled here — the caller (or a later ticket)
//! wraps the LLM call with [`llm_kernel::llm::CacheClient`]. Likewise, heuristic
//! enrichment of `tier`/`kind`/`apps`/`entry_files` beyond what the LLM
//! returns is a separate concern.

#[allow(unused_imports)]
use brigid_core::Abstraction;

mod incremental;
mod map;
mod reduce;
mod single_shot;
mod types;

pub mod graph_context;

pub use incremental::incremental_identify;
pub use map::{IdentifyMapInput, identify_map};
pub use reduce::{IdentifyReduceInput, identify_reduce};
pub use single_shot::{IdentifySingleShotInput, identify_single_shot};
pub use types::{CandidateAbstraction, CandidateBatch, IdentifyError};

/// Enrich empty `kind` fields in an [`brigid_core::IdentifyResult`] using a
/// [`brigid_core::plugin::PluginRegistry`].
///
/// This is the integration point where custom [`KindDetector`] plugins
/// extend the identify stage: after the LLM produces its abstraction list,
/// any abstraction whose `kind` is empty is classified by the registry
/// (falling back to the built-in [`DefaultKindDetector`] when the registry
/// was constructed with [`PluginRegistry::with_default`]).
///
/// Abstractions that already have a non-empty `kind` (the normal case — the
/// LLM sets it) are left untouched, so plugins only **fill gaps** rather
/// than overriding LLM output.
///
/// `files` is the crawl inventory (parallel to `contents`); `contents` may
/// be empty strings when content is unavailable — extension-based
/// detectors still work on the path alone.
///
/// [`KindDetector`]: brigid_core::plugin::KindDetector
/// [`DefaultKindDetector`]: brigid_core::plugin::DefaultKindDetector
/// [`PluginRegistry::with_default`]: brigid_core::plugin::PluginRegistry::with_default
pub fn enrich_identify_kinds(
    result: &mut brigid_core::IdentifyResult,
    files: &[String],
    contents: &[String],
    registry: &brigid_core::plugin::PluginRegistry,
) {
    brigid_core::plugin::enrich_abstraction_kinds(
        &mut result.abstractions,
        files,
        contents,
        registry,
    );
}

/// Re-export of [`crate::llm::LlmError`] for ergonomic matching at call sites
/// that only depend on `brigid-pipeline`.
pub use crate::llm::LlmError;
/// Re-export of [`PromptError`] for ergonomic matching at call sites that
/// only depend on `brigid-pipeline`.
pub use crate::prompts::PromptError;
/// Re-export of [`brigid_core::ExtractError`] for ergonomic matching at call
/// sites that only depend on `brigid-pipeline`.
pub use brigid_core::ExtractError;

pub use map::batch_files_by_size;
pub(crate) use map::run_single_map_batch;
#[allow(unused_imports)]
pub(crate) use map::{parse_candidates, render_map_prompt};

#[cfg(test)]
mod enrich_tests {
    use super::*;
    use brigid_core::plugin::{KindDetector, PluginRegistry};
    use brigid_core::{Abstraction, AbstractionKind, IdentifyResult, Tier};

    /// A custom detector that classifies `.rs` files as "rust module".
    struct RustModuleDetector;
    impl KindDetector for RustModuleDetector {
        fn detect_kind(&self, file_path: &str, _content: &str) -> Option<AbstractionKind> {
            if file_path.ends_with(".rs") {
                Some(AbstractionKind::new("rust module"))
            } else {
                None
            }
        }
        fn name(&self) -> &str {
            "rust-module-detector"
        }
    }

    #[test]
    fn enrich_identify_kinds_fills_empty_kinds() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));

        let mut abs = Abstraction::new("A", "desc", Tier::S, "");
        abs.file_indices = vec![0];
        let mut result = IdentifyResult::new(vec![abs]);
        let files = vec!["src/lib.rs".to_string()];
        let contents = vec!["pub mod foo;".to_string()];

        enrich_identify_kinds(&mut result, &files, &contents, &registry);

        assert_eq!(
            result.abstractions[0].kind,
            AbstractionKind::new("rust module")
        );
    }

    #[test]
    fn enrich_identify_kinds_falls_back_to_default() {
        // Registry with default fallback — no custom plugin matches .md,
        // but the default detector classifies it as "documentation".
        let registry = PluginRegistry::with_default();

        let mut abs = Abstraction::new("Docs", "desc", Tier::S, "");
        abs.entry_files = vec!["README.md".to_string()];
        let mut result = IdentifyResult::new(vec![abs]);
        let files = vec!["README.md".to_string()];
        let contents = vec!["# Title".to_string()];

        enrich_identify_kinds(&mut result, &files, &contents, &registry);

        assert_eq!(
            result.abstractions[0].kind,
            AbstractionKind::new("documentation")
        );
    }

    #[test]
    fn enrich_identify_kinds_no_plugin_fallback_returns_unchanged() {
        // Empty registry — no detector matches, kind stays empty.
        let registry = PluginRegistry::new();

        let mut abs = Abstraction::new("A", "desc", Tier::S, "");
        abs.file_indices = vec![0];
        let mut result = IdentifyResult::new(vec![abs]);
        let files = vec!["src/lib.rs".to_string()];
        let contents = vec!["pub mod foo;".to_string()];

        enrich_identify_kinds(&mut result, &files, &contents, &registry);

        assert_eq!(result.abstractions[0].kind.as_str(), "");
    }

    #[test]
    fn enrich_identify_kinds_does_not_override_llm_kinds() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));

        // The LLM already set kind="class" — must NOT be overridden.
        let mut abs = Abstraction::new("A", "desc", Tier::S, "class");
        abs.file_indices = vec![0];
        let mut result = IdentifyResult::new(vec![abs]);
        let files = vec!["src/lib.rs".to_string()];
        let contents = vec!["pub mod foo;".to_string()];

        enrich_identify_kinds(&mut result, &files, &contents, &registry);

        assert_eq!(result.abstractions[0].kind, AbstractionKind::new("class"));
    }
}
