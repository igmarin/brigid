//! Graph provider context formatting for the identify stage (ADR 0016).
//!
//! When a [`brigid_core::GraphProvider`] is present, its structural data
//! (communities, multimodal concepts) is formatted into human-readable strings
//! and injected into the identify prompt templates. The LLM uses this
//! structural context to produce better-informed abstractions — communities
//! suggest file groupings, multimodal concepts surface design docs the LLM
//! cannot read directly.
//!
//! When the provider is [`brigid_core::NoneProvider`] (or any provider that
//! returns empty data), the formatted strings are empty and the template
//! conditional blocks (`{% if community_context %}`) are skipped — the
//! identify stage runs exactly as today (LLM-only).

use brigid_core::{Community, GraphProvider, MultimodalConcept};

/// Format communities as a human-readable string for the map prompt.
///
/// Each community is listed with its label (if any) and file paths:
///
/// ```text
/// Structural clustering hint — these files group together:
///
/// 1. [auth module] src/auth/session.rs, src/auth/token.rs, src/auth/mod.rs
/// 2. [unknown] src/db/connection.rs, src/db/migrations.rs
/// ```
///
/// Returns an empty string when `communities` is empty (NoneProvider case).
#[must_use]
pub fn format_communities(communities: &[Community]) -> String {
    if communities.is_empty() {
        return String::new();
    }
    let mut out = String::from("Structural clustering hint — these files group together:\n\n");
    for (i, community) in communities.iter().enumerate() {
        let label = community.label.as_deref().unwrap_or("unlabeled");
        let files = community.files.join(", ");
        out.push_str(&format!("{}. [{}] {}\n", i + 1, label, files));
    }
    out.push_str(
        "\nConsider these groupings when naming abstractions. You may merge, split, or reject them.\n",
    );
    out
}

/// Format multimodal concepts as a human-readable string for the reduce prompt.
///
/// Each concept is listed with its source file and description:
///
/// ```text
/// Concepts extracted from design documents (non-code sources):
///
/// 1. [microservices] (from docs/architecture.png): Service boundaries diagram
/// 2. [event-driven] (from docs/design.pdf): Event sourcing architecture
/// ```
///
/// Returns an empty string when `concepts` is empty (NoneProvider case).
#[must_use]
pub fn format_multimodal_concepts(concepts: &[MultimodalConcept]) -> String {
    if concepts.is_empty() {
        return String::new();
    }
    let mut out = String::from("Concepts extracted from design documents (non-code sources):\n\n");
    for (i, concept) in concepts.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}] (from {}): {}\n",
            i + 1,
            concept.concept,
            concept.source_file,
            concept.description
        ));
    }
    out.push_str(
        "\nConsider whether any identified abstractions correspond to these documented concepts.\n",
    );
    out
}

/// Extract and format community context from a graph provider for the map prompt.
///
/// Returns an empty string when the provider has no community data (NoneProvider).
#[must_use]
pub fn community_context_from_provider(provider: &dyn GraphProvider) -> String {
    let communities = provider.communities();
    format_communities(&communities)
}

/// Extract and format multimodal concept context from a graph provider for the
/// reduce prompt.
///
/// Returns an empty string when the provider has no multimodal data (NoneProvider).
#[must_use]
pub fn multimodal_context_from_provider(provider: &dyn GraphProvider) -> String {
    let concepts = provider.multimodal_concepts();
    format_multimodal_concepts(&concepts)
}

/// Format hub concepts as a human-readable string for the order prompt.
///
/// Hub concepts are the most-connected concepts in the graph — candidates
/// for early chapter placement:
///
/// ```text
/// High-connectivity hub concepts (consider explaining early):
///
/// 1. auth
/// 2. config
/// 3. router
/// ```
///
/// Returns an empty string when `hubs` is empty (NoneProvider case).
#[must_use]
pub fn format_hub_concepts(hubs: &[String]) -> String {
    if hubs.is_empty() {
        return String::new();
    }
    let mut out = String::from("High-connectivity hub concepts (consider explaining early):\n\n");
    for (i, hub) in hubs.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, hub));
    }
    out
}

/// Extract and format hub concept context from a graph provider for the order
/// prompt.
///
/// Returns an empty string when the provider has no hub concept data
/// (NoneProvider).
#[must_use]
pub fn hub_context_from_provider(provider: &dyn GraphProvider) -> String {
    let hubs = provider.hub_concepts();
    format_hub_concepts(&hubs)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::graph_provider::{CallEdge, NoneProvider};

    // -----------------------------------------------------------------------
    // format_communities
    // -----------------------------------------------------------------------

    #[test]
    fn format_communities_empty_returns_empty_string() {
        assert!(format_communities(&[]).is_empty());
    }

    #[test]
    fn format_communities_with_label() {
        let communities = vec![Community {
            files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            label: Some("auth module".to_string()),
        }];
        let result = format_communities(&communities);
        assert!(result.contains("Structural clustering hint"));
        assert!(result.contains("[auth module]"));
        assert!(result.contains("src/a.rs, src/b.rs"));
        assert!(result.contains("Consider these groupings"));
    }

    #[test]
    fn format_communities_without_label() {
        let communities = vec![Community {
            files: vec!["src/x.rs".to_string()],
            label: None,
        }];
        let result = format_communities(&communities);
        assert!(result.contains("[unlabeled]"));
        assert!(result.contains("src/x.rs"));
    }

    #[test]
    fn format_communities_multiple() {
        let communities = vec![
            Community {
                files: vec!["src/a.rs".to_string()],
                label: Some("auth".to_string()),
            },
            Community {
                files: vec!["src/b.rs".to_string()],
                label: Some("db".to_string()),
            },
        ];
        let result = format_communities(&communities);
        assert!(result.contains("1. [auth]"));
        assert!(result.contains("2. [db]"));
    }

    // -----------------------------------------------------------------------
    // format_multimodal_concepts
    // -----------------------------------------------------------------------

    #[test]
    fn format_multimodal_concepts_empty_returns_empty_string() {
        assert!(format_multimodal_concepts(&[]).is_empty());
    }

    #[test]
    fn format_multimodal_concepts_with_data() {
        let concepts = vec![MultimodalConcept {
            concept: "microservices".to_string(),
            source_file: "docs/architecture.png".to_string(),
            description: "Service boundaries diagram".to_string(),
        }];
        let result = format_multimodal_concepts(&concepts);
        assert!(result.contains("Concepts extracted from design documents"));
        assert!(result.contains("[microservices]"));
        assert!(result.contains("docs/architecture.png"));
        assert!(result.contains("Service boundaries diagram"));
        assert!(result.contains("Consider whether any identified abstractions"));
    }

    #[test]
    fn format_multimodal_concepts_multiple() {
        let concepts = vec![
            MultimodalConcept {
                concept: "microservices".to_string(),
                source_file: "docs/arch.png".to_string(),
                description: "Architecture".to_string(),
            },
            MultimodalConcept {
                concept: "event-driven".to_string(),
                source_file: "docs/design.pdf".to_string(),
                description: "Event sourcing".to_string(),
            },
        ];
        let result = format_multimodal_concepts(&concepts);
        assert!(result.contains("1. [microservices]"));
        assert!(result.contains("2. [event-driven]"));
    }

    // -----------------------------------------------------------------------
    // Provider-based helpers
    // -----------------------------------------------------------------------

    #[test]
    fn community_context_from_none_provider_is_empty() {
        let provider = NoneProvider::new();
        assert!(community_context_from_provider(&provider).is_empty());
    }

    #[test]
    fn multimodal_context_from_none_provider_is_empty() {
        let provider = NoneProvider::new();
        assert!(multimodal_context_from_provider(&provider).is_empty());
    }

    #[test]
    fn community_context_from_custom_provider() {
        struct StubProvider;
        impl GraphProvider for StubProvider {
            fn call_graph_for_file(&self, _file_path: &str) -> Vec<CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<Community> {
                vec![Community {
                    files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
                    label: Some("core".to_string()),
                }]
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
                "stub"
            }
        }

        let provider = StubProvider;
        let context = community_context_from_provider(&provider);
        assert!(context.contains("[core]"));
        assert!(context.contains("src/a.rs, src/b.rs"));
    }

    #[test]
    fn multimodal_context_from_custom_provider() {
        struct StubProvider;
        impl GraphProvider for StubProvider {
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
                vec![MultimodalConcept {
                    concept: "architecture".to_string(),
                    source_file: "docs/arch.png".to_string(),
                    description: "System design".to_string(),
                }]
            }
            fn name(&self) -> &str {
                "stub"
            }
        }

        let provider = StubProvider;
        let context = multimodal_context_from_provider(&provider);
        assert!(context.contains("[architecture]"));
        assert!(context.contains("docs/arch.png"));
    }

    // -----------------------------------------------------------------------
    // format_hub_concepts
    // -----------------------------------------------------------------------

    #[test]
    fn format_hub_concepts_empty_returns_empty_string() {
        assert!(format_hub_concepts(&[]).is_empty());
    }

    #[test]
    fn format_hub_concepts_with_data() {
        let hubs = vec![
            "auth".to_string(),
            "config".to_string(),
            "router".to_string(),
        ];
        let result = format_hub_concepts(&hubs);
        assert!(result.contains("High-connectivity hub concepts"));
        assert!(result.contains("1. auth"));
        assert!(result.contains("2. config"));
        assert!(result.contains("3. router"));
    }

    #[test]
    fn format_hub_concepts_single() {
        let hubs = vec!["auth".to_string()];
        let result = format_hub_concepts(&hubs);
        assert!(result.contains("1. auth"));
    }

    // -----------------------------------------------------------------------
    // hub_context_from_provider
    // -----------------------------------------------------------------------

    #[test]
    fn hub_context_from_none_provider_is_empty() {
        let provider = NoneProvider::new();
        assert!(hub_context_from_provider(&provider).is_empty());
    }

    #[test]
    fn hub_context_from_custom_provider() {
        struct StubProvider;
        impl GraphProvider for StubProvider {
            fn call_graph_for_file(&self, _file_path: &str) -> Vec<CallEdge> {
                Vec::new()
            }
            fn communities(&self) -> Vec<Community> {
                Vec::new()
            }
            fn hub_concepts(&self) -> Vec<String> {
                vec!["auth".to_string(), "config".to_string()]
            }
            fn relationship_exists(&self, _from: &str, _to: &str) -> Option<bool> {
                None
            }
            fn multimodal_concepts(&self) -> Vec<MultimodalConcept> {
                Vec::new()
            }
            fn name(&self) -> &str {
                "stub"
            }
        }

        let provider = StubProvider;
        let context = hub_context_from_provider(&provider);
        assert!(context.contains("1. auth"));
        assert!(context.contains("2. config"));
    }
}
