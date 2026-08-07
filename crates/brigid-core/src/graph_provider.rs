//! Graph provider trait for structural ground truth from external tools.
//!
//! This module defines the extension point that lets `brigid` use structural
//! data from external graph tools (codegraph, Graphify) to improve abstraction
//! identification and relationship verification on large codebases. See ADR
//! 0016 for the full architecture rationale.
//!
//! # Design
//!
//! - [`GraphProvider`] is an **object-safe** trait (no generics, no `Self` in
//!   return position, `Send + Sync` supertrait) so the pipeline can hold
//!   `Option<Box<dyn GraphProvider>>` and dispatch dynamically across async
//!   stages. This follows the same pattern as [`crate::plugin::KindDetector`]
//!   in ADR 0014.
//! - [`NoneProvider`] implements every method as a no-op (empty vectors,
//!   `None` for [`GraphProvider::relationship_exists`]). This is the default
//!   when no external graph tool is configured — `brigid` works exactly as
//!   today (LLM-only).
//! - [`none()`] returns a boxed [`NoneProvider`] for convenient default
//!   construction.
//!
//! Dynamic loading from external tools happens at construction time (reading
//! `.codegraph/graph.db` or `graphify-out/graph.json`), not at query time.
//! The trait methods are synchronous, in-memory lookups over pre-loaded data.

use serde::{Deserialize, Serialize};

/// A directed call-graph edge from one symbol to another.
///
/// Produced by symbol-level analysis tools (codegraph). The `caller` and
/// `callee` are symbol names (function/method names); `callee_file` is the
/// file path where the callee is defined.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallEdge {
    /// The calling symbol (function/method name).
    pub caller: String,
    /// The called symbol (function/method name).
    pub callee: String,
    /// File path where the callee is defined.
    pub callee_file: String,
}

/// A community-detected file grouping.
///
/// Produced by clustering algorithms (Graphify's Leiden community detection).
/// Each community is a set of file paths that cluster together structurally.
/// The label is optional — Graphify may provide a human-readable label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Community {
    /// File paths that belong to this community.
    pub files: Vec<String>,
    /// Optional human-readable label for the community.
    pub label: Option<String>,
}

/// A concept extracted from a non-code file (diagram, PDF, image).
///
/// Produced by multimodal analysis (Graphify's vision model). These concepts
/// let `brigid` reference architecture diagrams and design docs that the LLM
/// has never seen — the most genuinely novel contribution of the graph
/// provider integration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultimodalConcept {
    /// The concept name or summary.
    pub concept: String,
    /// Source file path (e.g. `"docs/architecture.png"`).
    pub source_file: String,
    /// Description of the concept extracted from the source file.
    pub description: String,
}

/// Structural ground truth from an external graph tool.
///
/// Implementations: [`NoneProvider`] (default), `CodegraphProvider`,
/// `GraphifyProvider`, `ComposedProvider`. When present, the identify and
/// relationships stages use this data to inform and verify LLM output. When
/// absent ([`NoneProvider`]), `brigid` works exactly as today — LLM-only.
///
/// The trait is **object-safe**: no generics, no `Self` in return position,
/// `Send + Sync` supertrait, so it can be stored as `Box<dyn GraphProvider>`.
///
/// # Example
///
/// ```
/// use brigid_core::graph_provider::{GraphProvider, NoneProvider, CallEdge};
///
/// let provider = NoneProvider;
/// assert!(provider.call_graph_for_file("src/main.rs").is_empty());
/// assert_eq!(provider.name(), "none");
/// ```
pub trait GraphProvider: Send + Sync {
    /// Symbol-level call graph for a file (codegraph).
    ///
    /// Returns edges: `(caller_symbol, callee_symbol, callee_file)`. When the
    /// provider has no structural data for this file, returns an empty vec.
    fn call_graph_for_file(&self, file_path: &str) -> Vec<CallEdge>;

    /// Community-detected file groupings (Graphify Leiden).
    ///
    /// Each community is a set of file paths that cluster together. When the
    /// provider has no community data, returns an empty vec.
    fn communities(&self) -> Vec<Community>;

    /// High-degree "god node" concepts (Graphify).
    ///
    /// The most-connected concepts in the graph — candidates for early chapter
    /// placement. When the provider has no hub concept data, returns an empty
    /// vec.
    fn hub_concepts(&self) -> Vec<String>;

    /// Verify a claimed relationship exists structurally.
    ///
    /// Returns `Some(true)` if the call graph confirms `from`→`to`,
    /// `Some(false)` if the call graph contradicts it, `None` if the provider
    /// has no structural data for these nodes.
    fn relationship_exists(&self, from: &str, to: &str) -> Option<bool>;

    /// Multimodal concepts extracted from non-code files.
    ///
    /// Each concept has a source file (e.g. `"docs/architecture.png"`) and a
    /// description. When the provider has no multimodal data, returns an empty
    /// vec.
    fn multimodal_concepts(&self) -> Vec<MultimodalConcept>;

    /// Provider name for diagnostics.
    fn name(&self) -> &str;
}

/// Default no-op graph provider.
///
/// Implements every [`GraphProvider`] method as a no-op: empty vectors for
/// data-returning methods, `None` for [`GraphProvider::relationship_exists`].
/// This is the default when no external graph tool is configured. The pipeline
/// behavior is identical to LLM-only identification and relationships.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoneProvider;

impl NoneProvider {
    /// Create a new [`NoneProvider`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl GraphProvider for NoneProvider {
    fn call_graph_for_file(&self, _file_path: &str) -> Vec<CallEdge> {
        Vec::new()
    }

    fn communities(&self) -> Vec<Community> {
        Vec::new()
    }

    fn hub_concepts(&self) -> Vec<String> {
        Vec::new()
    }

    fn relationship_exists(&self, _from: &str, _to: &str) -> Option<bool> {
        None
    }

    fn multimodal_concepts(&self) -> Vec<MultimodalConcept> {
        Vec::new()
    }

    fn name(&self) -> &str {
        "none"
    }
}

/// Create a boxed [`NoneProvider`] as the default graph provider.
///
/// Convenience constructor for `Box::new(NoneProvider::new())`. Use this when
/// the pipeline needs `Option<Box<dyn GraphProvider>>` and no external graph
/// tool is configured.
#[must_use]
pub fn none() -> Box<dyn GraphProvider> {
    Box::new(NoneProvider::new())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Supporting types
    // -----------------------------------------------------------------------

    #[test]
    fn call_edge_serializes_with_snake_case_fields() {
        let edge = CallEdge {
            caller: "fn_a".to_string(),
            callee: "fn_b".to_string(),
            callee_file: "src/b.rs".to_string(),
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"caller\""));
        assert!(json.contains("\"callee\""));
        assert!(json.contains("\"callee_file\""));
    }

    #[test]
    fn call_edge_round_trips() {
        let edge = CallEdge {
            caller: "fn_a".to_string(),
            callee: "fn_b".to_string(),
            callee_file: "src/b.rs".to_string(),
        };
        let json = serde_json::to_string(&edge).unwrap();
        let back: CallEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, back);
    }

    #[test]
    fn community_with_label_round_trips() {
        let community = Community {
            files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            label: Some("auth module".to_string()),
        };
        let json = serde_json::to_string(&community).unwrap();
        let back: Community = serde_json::from_str(&json).unwrap();
        assert_eq!(community, back);
    }

    #[test]
    fn community_without_label_round_trips() {
        let community = Community {
            files: vec!["src/a.rs".to_string()],
            label: None,
        };
        let json = serde_json::to_string(&community).unwrap();
        let back: Community = serde_json::from_str(&json).unwrap();
        assert_eq!(community, back);
    }

    #[test]
    fn multimodal_concept_round_trips() {
        let concept = MultimodalConcept {
            concept: "microservices".to_string(),
            source_file: "docs/arch.png".to_string(),
            description: "Architecture diagram showing service boundaries".to_string(),
        };
        let json = serde_json::to_string(&concept).unwrap();
        let back: MultimodalConcept = serde_json::from_str(&json).unwrap();
        assert_eq!(concept, back);
    }

    // -----------------------------------------------------------------------
    // NoneProvider
    // -----------------------------------------------------------------------

    #[test]
    fn none_provider_call_graph_returns_empty() {
        let provider = NoneProvider::new();
        assert!(provider.call_graph_for_file("src/main.rs").is_empty());
    }

    #[test]
    fn none_provider_communities_returns_empty() {
        let provider = NoneProvider::new();
        assert!(provider.communities().is_empty());
    }

    #[test]
    fn none_provider_hub_concepts_returns_empty() {
        let provider = NoneProvider::new();
        assert!(provider.hub_concepts().is_empty());
    }

    #[test]
    fn none_provider_relationship_exists_returns_none() {
        let provider = NoneProvider::new();
        assert_eq!(provider.relationship_exists("A", "B"), None);
    }

    #[test]
    fn none_provider_multimodal_concepts_returns_empty() {
        let provider = NoneProvider::new();
        assert!(provider.multimodal_concepts().is_empty());
    }

    #[test]
    fn none_provider_name_is_stable() {
        let provider = NoneProvider::new();
        assert_eq!(provider.name(), "none");
    }

    // -----------------------------------------------------------------------
    // none() constructor
    // -----------------------------------------------------------------------

    #[test]
    fn none_returns_boxed_provider() {
        let provider = none();
        assert_eq!(provider.name(), "none");
        assert!(provider.communities().is_empty());
    }

    // -----------------------------------------------------------------------
    // Object safety smoke test
    // -----------------------------------------------------------------------

    #[test]
    fn graph_provider_can_be_used_as_dyn_trait_object() {
        let provider: Box<dyn GraphProvider> = Box::new(NoneProvider::new());
        assert_eq!(provider.name(), "none");
        assert!(provider.call_graph_for_file("src/lib.rs").is_empty());
        assert_eq!(provider.relationship_exists("A", "B"), None);
    }

    // -----------------------------------------------------------------------
    // Custom provider impl (for testing trait dispatch)
    // -----------------------------------------------------------------------

    #[test]
    fn custom_provider_returns_data() {
        struct StubProvider;

        impl GraphProvider for StubProvider {
            fn call_graph_for_file(&self, file_path: &str) -> Vec<CallEdge> {
                if file_path == "src/a.rs" {
                    vec![CallEdge {
                        caller: "fn_a".to_string(),
                        callee: "fn_b".to_string(),
                        callee_file: "src/b.rs".to_string(),
                    }]
                } else {
                    Vec::new()
                }
            }
            fn communities(&self) -> Vec<Community> {
                vec![Community {
                    files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
                    label: Some("core".to_string()),
                }]
            }
            fn hub_concepts(&self) -> Vec<String> {
                vec!["auth".to_string(), "config".to_string()]
            }
            fn relationship_exists(&self, from: &str, to: &str) -> Option<bool> {
                if from == "A" && to == "B" {
                    Some(true)
                } else if from == "A" && to == "C" {
                    Some(false)
                } else {
                    None
                }
            }
            fn multimodal_concepts(&self) -> Vec<MultimodalConcept> {
                vec![MultimodalConcept {
                    concept: "architecture".to_string(),
                    source_file: "docs/arch.png".to_string(),
                    description: "System architecture diagram".to_string(),
                }]
            }
            fn name(&self) -> &str {
                "stub"
            }
        }

        let provider: Box<dyn GraphProvider> = Box::new(StubProvider);
        assert_eq!(provider.name(), "stub");

        let edges = provider.call_graph_for_file("src/a.rs");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].caller, "fn_a");

        let communities = provider.communities();
        assert_eq!(communities.len(), 1);
        assert_eq!(communities[0].files.len(), 2);

        let hubs = provider.hub_concepts();
        assert_eq!(hubs, vec!["auth".to_string(), "config".to_string()]);

        assert_eq!(provider.relationship_exists("A", "B"), Some(true));
        assert_eq!(provider.relationship_exists("A", "C"), Some(false));
        assert_eq!(provider.relationship_exists("X", "Y"), None);

        let concepts = provider.multimodal_concepts();
        assert_eq!(concepts.len(), 1);
        assert_eq!(concepts[0].concept, "architecture");
    }
}
