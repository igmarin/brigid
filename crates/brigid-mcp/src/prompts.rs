//! MCP prompts — pre-built onboarding workflows for AI assistants.
//!
//! Implements the prompt capability matrix from ADR 0015 §4. Each prompt
//! assembles a multi-message conversation starter from the in-memory
//! [`CheckpointData`], loading the relevant resources (index, setup guide,
//! chapters, relationship graph) so the AI assistant has immediate context.
//!
//! # Prompts
//!
//! | Prompt | Arguments | What it loads |
//! |--------|-----------|---------------|
//! | `onboard_to_codebase` | — | Index + setup guide + top 3 chapters by tier |
//! | `explain_file` | `file_path` | Owning chapter + abstraction dependencies |
//! | `deep_dive_abstraction` | `name` | Chapter + relationship graph + file list |

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{PromptMessage, Role};
use rmcp::{prompt, prompt_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::CheckpointData;
use crate::tools::{
    file_to_abstraction_index, find_abstraction_by_name, relationships_for_abstraction,
};

/// Parameters for [`BrigidPrompts::explain_file`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExplainFileParams {
    /// Relative repository path of the file to explain.
    pub file_path: String,
}

/// Parameters for [`BrigidPrompts::deep_dive_abstraction`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeepDiveAbstractionParams {
    /// Name of the abstraction to deep-dive into.
    pub name: String,
}

/// Prompt handler for the brigid MCP server.
///
/// Holds a reference to the loaded [`CheckpointData`] and implements all
/// ADR 0015 §4 prompts as `#[prompt]`-annotated methods. Each prompt returns
/// a [`Vec<PromptMessage>`] suitable for the MCP `get_prompt` response.
#[derive(Debug, Clone)]
pub struct BrigidPrompts {
    /// The loaded checkpoint data backing all prompt assembly.
    pub data: CheckpointData,
}

/// Build a user-role text message.
fn user_msg(text: impl Into<String>) -> PromptMessage {
    PromptMessage::new_text(Role::User, text)
}

/// Build an assistant-role text message.
fn assistant_msg(text: impl Into<String>) -> PromptMessage {
    PromptMessage::new_text(Role::Assistant, text)
}

/// Get the combined tutorial index markdown, if available.
fn index_markdown(data: &CheckpointData) -> String {
    data.combined
        .as_ref()
        .map(|c| c.index_markdown.clone())
        .unwrap_or_else(|| {
            "(No combined index available — the combine stage has not run.)".to_string()
        })
}

/// Get the setup guide markdown, if available.
fn setup_markdown(data: &CheckpointData) -> String {
    data.setup_guide
        .as_ref()
        .map(|g| g.markdown.clone())
        .unwrap_or_else(|| "(No setup guide available — the setup stage has not run.)".to_string())
}

/// Get the architecture overview markdown, if available.
fn overview_markdown(data: &CheckpointData) -> String {
    data.overview
        .as_ref()
        .map(|o| o.markdown.clone())
        .unwrap_or_else(|| {
            "(No architecture overview available — the overview stage has not run.)".to_string()
        })
}

/// Find the chapter for a given abstraction index.
fn chapter_for_abstraction(data: &CheckpointData, index: usize) -> Option<&brigid_core::Chapter> {
    data.chapters
        .as_ref()
        .and_then(|ch| ch.chapters.iter().find(|c| c.abstraction_index == index))
}

/// Get the list of file paths for an abstraction (from crawl inventory + entry files).
fn files_for_abstraction(data: &CheckpointData, abs: &brigid_core::Abstraction) -> Vec<String> {
    let mut files: Vec<String> = abs
        .file_indices
        .iter()
        .filter_map(|&idx| data.files.get(idx).map(|f| f.path.clone()))
        .collect();
    for ef in &abs.entry_files {
        if !files.contains(ef) {
            files.push(ef.clone());
        }
    }
    files
}

/// The `#[prompt]` macro generates `_prompt_attr()` metadata functions that
/// lack doc comments (unlike `#[tool]`), so we suppress `missing_docs` for
/// the router impl block only.
#[allow(missing_docs)]
#[prompt_router(vis = "pub")]
impl BrigidPrompts {
    /// Create a new prompt handler backed by the given checkpoint data.
    #[must_use]
    pub fn new(data: CheckpointData) -> Self {
        Self { data }
    }

    /// Onboard a newcomer to the codebase.
    ///
    /// Loads the combined tutorial index, the setup guide, the architecture
    /// overview, and the top 3 chapters (by tier: L first, then M, then S).
    /// This gives the AI assistant a complete onboarding starter.
    #[prompt(
        name = "onboard_to_codebase",
        description = "Onboard a newcomer to the codebase. Loads the tutorial index, setup guide, architecture overview, and top chapters by tier."
    )]
    pub fn onboard_to_codebase(&self) -> Vec<PromptMessage> {
        let mut messages = vec![
            user_msg(
                "I'm new to this codebase. Help me get onboarded by walking me through the project structure, setup, and key concepts.",
            ),
            assistant_msg(
                "I'll start by loading the tutorial index, setup guide, and architecture overview to give you a structured onboarding overview.",
            ),
        ];

        // Index.
        messages.push(user_msg(format!(
            "Here is the combined tutorial index:\n\n````markdown\n{}\n````",
            index_markdown(&self.data)
        )));

        // Setup guide.
        messages.push(user_msg(format!(
            "Here is the setup guide:\n\n````markdown\n{}\n````",
            setup_markdown(&self.data)
        )));

        // Architecture overview.
        messages.push(user_msg(format!(
            "Here is the architecture overview:\n\n````markdown\n{}\n````",
            overview_markdown(&self.data)
        )));

        // Top 3 chapters by tier (L > M > S).
        if let Some(chapters) = &self.data.chapters {
            let mut sorted: Vec<&brigid_core::Chapter> = chapters.chapters.iter().collect();
            sorted.sort_by(|a, b| {
                // L > M > S (reverse ordinal since L=2, M=1, S=0 in Ord)
                b.tier.cmp(&a.tier)
            });
            let top: Vec<&brigid_core::Chapter> = sorted.into_iter().take(3).collect();
            for ch in top {
                messages.push(user_msg(format!(
                    "Chapter {}: {}\n\n````markdown\n{}\n````",
                    ch.chapter_num, ch.title, ch.markdown
                )));
            }
        }

        messages.push(assistant_msg(
            "Based on these materials, I can now help you understand the codebase. What would you like to dive deeper into?",
        ));

        messages
    }

    /// Explain a specific source file.
    ///
    /// Loads the chapter that owns the file (via file→abstraction→chapter
    /// lookup) and the abstraction's dependency relationships.
    #[prompt(
        name = "explain_file",
        description = "Explain a specific source file. Loads the owning chapter and the abstraction's dependency relationships."
    )]
    pub fn explain_file(&self, params: Parameters<ExplainFileParams>) -> Vec<PromptMessage> {
        let file_path = &params.0.file_path;
        let mut messages = vec![
            user_msg(format!(
                "Explain the file `{file_path}` in the context of this codebase."
            )),
            assistant_msg(format!(
                "I'll look up which abstraction owns `{file_path}` and load the relevant chapter and dependency information."
            )),
        ];

        let map = file_to_abstraction_index(&self.data);
        match map.get(file_path) {
            Some(&abs_idx) => {
                let abs = self
                    .data
                    .abstractions
                    .as_ref()
                    .and_then(|a| a.abstractions.get(abs_idx));

                if let Some(abs) = abs {
                    messages.push(user_msg(format!(
                        "The file `{file_path}` belongs to the abstraction **{}** (tier: {}, kind: {}).\n\nDescription: {}",
                        abs.name, abs.tier, abs.kind.as_str(), abs.description
                    )));

                    // Load the owning chapter.
                    if let Some(ch) = chapter_for_abstraction(&self.data, abs_idx) {
                        messages.push(user_msg(format!(
                            "Here is the chapter explaining **{}**:\n\n````markdown\n{}\n````",
                            ch.title, ch.markdown
                        )));
                    }

                    // Load dependencies.
                    let deps = relationships_for_abstraction(&self.data, abs_idx);
                    if !deps.is_empty() {
                        let dep_lines: Vec<String> = deps
                            .iter()
                            .map(|d| {
                                format!(
                                    "  - {} {} {} ({})",
                                    d.from_name, d.label, d.to_name, d.direction
                                )
                            })
                            .collect();
                        messages.push(user_msg(format!(
                            "Here are the dependency relationships for **{}**:\n\n{}",
                            abs.name,
                            dep_lines.join("\n")
                        )));
                    }
                }
            }
            None => {
                messages.push(assistant_msg(format!(
                    "The file `{file_path}` is not mapped to any abstraction in the checkpoint. It may be a new file or not part of the analyzed scope."
                )));
            }
        }

        messages
    }

    /// Deep-dive into a specific abstraction.
    ///
    /// Loads the abstraction's chapter, its full relationship graph
    /// (outgoing + incoming edges), and the list of source files.
    #[prompt(
        name = "deep_dive_abstraction",
        description = "Deep-dive into a specific abstraction. Loads its chapter, relationship graph, and file list."
    )]
    pub fn deep_dive_abstraction(
        &self,
        params: Parameters<DeepDiveAbstractionParams>,
    ) -> Vec<PromptMessage> {
        let name = &params.0.name;
        let mut messages = vec![
            user_msg(format!(
                "Give me a deep dive into the `{name}` abstraction in this codebase."
            )),
            assistant_msg(format!(
                "I'll load the chapter, relationship graph, and file list for the `{name}` abstraction."
            )),
        ];

        match find_abstraction_by_name(&self.data, name) {
            Some((idx, abs)) => {
                messages.push(user_msg(format!(
                    "**{}** — {}\n\nTier: {} | Kind: {} | Apps: {}\n\nEntry files: {}",
                    abs.name,
                    abs.description,
                    abs.tier,
                    abs.kind.as_str(),
                    if abs.apps.is_empty() {
                        "(none)".to_string()
                    } else {
                        abs.apps.join(", ")
                    },
                    if abs.entry_files.is_empty() {
                        "(none)".to_string()
                    } else {
                        abs.entry_files.join(", ")
                    },
                )));

                // Chapter.
                if let Some(ch) = chapter_for_abstraction(&self.data, idx) {
                    messages.push(user_msg(format!(
                        "Chapter {}: {}\n\n````markdown\n{}\n````",
                        ch.chapter_num, ch.title, ch.markdown
                    )));
                }

                // Relationship graph.
                let deps = relationships_for_abstraction(&self.data, idx);
                if !deps.is_empty() {
                    let dep_lines: Vec<String> = deps
                        .iter()
                        .map(|d| {
                            format!(
                                "  - [{}] {} {} {}",
                                d.direction, d.from_name, d.label, d.to_name
                            )
                        })
                        .collect();
                    messages.push(user_msg(format!(
                        "Relationship graph for **{}**:\n\n{}",
                        abs.name,
                        dep_lines.join("\n")
                    )));
                } else {
                    messages.push(user_msg(format!(
                        "No dependency relationships recorded for **{}**.",
                        abs.name
                    )));
                }

                // File list.
                let files = files_for_abstraction(&self.data, abs);
                if !files.is_empty() {
                    messages.push(user_msg(format!(
                        "Source files for **{}**:\n\n{}",
                        abs.name,
                        files
                            .iter()
                            .map(|f| format!("  - `{f}`"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )));
                }
            }
            None => {
                messages.push(assistant_msg(format!(
                    "No abstraction named `{name}` was found in the checkpoint. Use the `list_abstractions` tool to see all available abstractions."
                )));
            }
        }

        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::{
        Abstraction, Chapter, ChapterOrder, ChapterResult, CheckpointV1, IdentifyResult,
        Relationship, RelationshipsResult, RunConfig, SetupGuide, StageId, Tier,
    };
    use brigid_core::{ArchitectureOverview, CombinedTutorial};
    use brigid_pipeline::records_from_files;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-mcp-prompts-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn full_data() -> (PathBuf, CheckpointData) {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[
            ("src/core.rs", b"fn core() {}"),
            ("src/router.rs", b"fn route() {}"),
        ]);
        store.save(cp.clone(), &files).unwrap();

        let mut core = Abstraction::new("Core", "The core system", Tier::L, "module");
        core.file_indices = vec![0];
        core.entry_files = vec!["src/core.rs".to_string()];
        let mut routing = Abstraction::new("Routing", "Routes requests", Tier::S, "class");
        routing.file_indices = vec![1];
        routing.entry_files = vec!["src/router.rs".to_string()];

        let identify = IdentifyResult::new(vec![core, routing]);
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());

        let relationships = RelationshipsResult::new(
            "A small web framework.",
            vec![Relationship::new(0, 1, "routes to", "calls")],
        );
        cp.relationships = Some(relationships.to_checkpoint_value().unwrap());

        let order = ChapterOrder::new(vec![0, 1]);
        cp.order = Some(order.to_checkpoint_value().unwrap());

        let chapters = ChapterResult::new(vec![
            Chapter::new(
                0,
                1,
                "Core",
                "# Core\n\nThe core system.",
                Tier::L,
                "module",
                "footer 0",
            ),
            Chapter::new(
                1,
                2,
                "Routing",
                "# Routing\n\nRoutes requests.",
                Tier::S,
                "class",
                "footer 1",
            ),
        ]);
        let chapter_entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, chapter_entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");

        let guide = SetupGuide::new("# Setup\n\nInstall Rust", 42, vec![], true);
        let setup_entry = store.write_setup_guide(&dir, &guide).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Setup, vec![setup_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Setup, "t3");

        let overview = ArchitectureOverview::new("# Architecture\n", vec![]);
        let overview_entry = store.write_architecture_overview(&dir, &overview).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Overview, vec![overview_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Overview, "t4");

        let tutorial = CombinedTutorial::new("# Index\n", 2, true, true, "en");
        let combine_entry = store.write_combined_index(&dir, &tutorial).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Combine, vec![combine_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Combine, "t5");

        store.save(cp, &files).unwrap();

        let loader = crate::CheckpointLoader::new(&dir);
        let data = loader.load().expect("checkpoint should load");
        (dir, data)
    }

    fn cleanup(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn onboard_to_codebase_returns_non_empty_messages() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.onboard_to_codebase();
        assert!(!messages.is_empty());
        // Should have at least: user, assistant, index, setup, overview, chapters, assistant.
        assert!(messages.len() >= 5);
        // First message is from user.
        assert_eq!(messages[0].role, Role::User);
        // Second is from assistant.
        assert_eq!(messages[1].role, Role::Assistant);
        cleanup(&dir);
    }

    #[test]
    fn onboard_to_codebase_includes_index_and_setup() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.onboard_to_codebase();
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("# Index"));
        assert!(all_text.contains("# Setup"));
        assert!(all_text.contains("# Architecture"));
        // Should include chapter content.
        assert!(all_text.contains("# Core"));
        cleanup(&dir);
    }

    #[test]
    fn onboard_to_codebase_top_chapters_by_tier() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.onboard_to_codebase();
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Core (tier L) should appear before Routing (tier S).
        let core_pos = all_text.find("# Core").unwrap();
        let routing_pos = all_text.find("# Routing").unwrap();
        assert!(core_pos < routing_pos);
        cleanup(&dir);
    }

    #[test]
    fn explain_file_returns_messages_with_chapter() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.explain_file(Parameters(ExplainFileParams {
            file_path: "src/core.rs".to_string(),
        }));
        assert!(!messages.is_empty());
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("Core"));
        assert!(all_text.contains("# Core"));
        // Should mention the dependency.
        assert!(all_text.contains("routes to"));
        cleanup(&dir);
    }

    #[test]
    fn explain_file_unknown_file_returns_not_found_message() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.explain_file(Parameters(ExplainFileParams {
            file_path: "nonexistent.rs".to_string(),
        }));
        assert!(!messages.is_empty());
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("not mapped"));
        cleanup(&dir);
    }

    #[test]
    fn deep_dive_abstraction_returns_chapter_and_graph() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.deep_dive_abstraction(Parameters(DeepDiveAbstractionParams {
            name: "Core".to_string(),
        }));
        assert!(!messages.is_empty());
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("The core system"));
        assert!(all_text.contains("# Core"));
        assert!(all_text.contains("routes to"));
        assert!(all_text.contains("src/core.rs"));
        cleanup(&dir);
    }

    #[test]
    fn deep_dive_abstraction_case_insensitive() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.deep_dive_abstraction(Parameters(DeepDiveAbstractionParams {
            name: "routing".to_string(),
        }));
        assert!(!messages.is_empty());
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("Routes requests"));
        assert!(all_text.contains("# Routing"));
        cleanup(&dir);
    }

    #[test]
    fn deep_dive_abstraction_unknown_returns_not_found() {
        let (dir, data) = full_data();
        let prompts = BrigidPrompts::new(data);
        let messages = prompts.deep_dive_abstraction(Parameters(DeepDiveAbstractionParams {
            name: "Nonexistent".to_string(),
        }));
        assert!(!messages.is_empty());
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("No abstraction named"));
        cleanup(&dir);
    }

    #[test]
    fn prompt_router_generates_prompt_definitions() {
        let (dir, data) = full_data();
        let _prompts = BrigidPrompts::new(data);
        let router = BrigidPrompts::prompt_router();
        let all = router.list_all();
        let names: Vec<String> = all.iter().map(|p| p.name.to_string()).collect::<Vec<_>>();
        assert!(names.contains(&"onboard_to_codebase".to_string()));
        assert!(names.contains(&"explain_file".to_string()));
        assert!(names.contains(&"deep_dive_abstraction".to_string()));
        cleanup(&dir);
    }
}
