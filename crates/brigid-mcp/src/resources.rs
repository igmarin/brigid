//! MCP resources exposing the checkpoint's structured data.
//!
//! Implements the resource capability matrix from ADR 0015 §4. Each resource
//! is identified by a `checkpoint://` URI and backed by the in-memory
//! [`CheckpointData`] struct loaded at startup.
//!
//! # Resource URIs
//!
//! | URI | MIME type | Content |
//! |-----|-----------|---------|
//! | `checkpoint://metadata` | `application/json` | [`CheckpointV1`] metadata |
//! | `checkpoint://abstractions` | `application/json` | [`IdentifyResult`] |
//! | `checkpoint://relationships` | `application/json` | [`RelationshipsResult`] |
//! | `checkpoint://chapter-order` | `application/json` | [`ChapterOrder`] |
//! | `checkpoint://files` | `application/json` | File inventory |
//! | `checkpoint://chapter/{index}` | `text/markdown` | Chapter content |
//! | `checkpoint://setup-guide` | `text/markdown` | Setup guide |
//! | `checkpoint://architecture-overview` | `text/markdown` | Architecture overview |
//! | `checkpoint://index` | `text/markdown` | Combined tutorial index |
//!
//! Resources for stages that have not completed are omitted from the list and
//! return a not-found error when read.

use std::borrow::Cow;

use brigid_core::StageId;
use rmcp::model::{
    Annotations, ListResourcesResult, ListResourceTemplatesResult, ReadResourceResult,
    Resource, ResourceContents, ResourceTemplate,
};

use crate::CheckpointData;

/// MIME type for JSON resources.
const MIME_JSON: &str = "application/json";
/// MIME type for Markdown resources.
const MIME_MARKDOWN: &str = "text/markdown";

/// URI scheme prefix for all checkpoint resources.
const URI_SCHEME: &str = "checkpoint://";

/// The `checkpoint://chapter/{index}` URI template string.
const CHAPTER_TEMPLATE_URI: &str = "checkpoint://chapter/{index}";

/// Build the list of static resources (non-templated) available for `data`.
///
/// Resources for stages that have not completed are excluded so the client
/// only sees what is actually readable.
#[must_use]
pub fn list_resources(data: &CheckpointData) -> Vec<Resource> {
    let mut resources = vec![
        Resource::new(
            format!("{URI_SCHEME}metadata"),
            "Checkpoint Metadata",
        )
        .with_description("Checkpoint metadata: config, completed stages, git commit.")
        .with_mime_type(MIME_JSON),
    ];

    if data.abstractions.is_some() {
        resources.push(
            Resource::new(
                format!("{URI_SCHEME}abstractions"),
                "Abstractions",
            )
            .with_description("Full IdentifyResult — all abstractions with file indices, kinds, tiers.")
            .with_mime_type(MIME_JSON),
        );
    }

    if data.relationships.is_some() {
        resources.push(
            Resource::new(
                format!("{URI_SCHEME}relationships"),
                "Relationships",
            )
            .with_description("Full RelationshipsResult — relationship edges with labels and kinds.")
            .with_mime_type(MIME_JSON),
        );
    }

    if data.chapter_order.is_some() {
        resources.push(
            Resource::new(
                format!("{URI_SCHEME}chapter-order"),
                "Chapter Order",
            )
            .with_description("ChapterOrder — the ordered list of abstraction indices.")
            .with_mime_type(MIME_JSON),
        );
    }

    resources.push(
        Resource::new(format!("{URI_SCHEME}files"), "File Inventory")
            .with_description("File inventory (path, size) from the crawl.")
            .with_mime_type(MIME_JSON),
    );

    // Chapter resources — one per written chapter.
    if let Some(chapters) = &data.chapters {
        for ch in &chapters.chapters {
            let uri = chapter_uri(ch.abstraction_index);
            let name = format!("Chapter {}: {}", ch.chapter_num, ch.title);
            resources.push(
                Resource::new(uri, name)
                    .with_description(format!("Chapter for abstraction #{}.", ch.abstraction_index))
                    .with_mime_type(MIME_MARKDOWN),
            );
        }
    }

    if data.setup_guide.is_some() {
        resources.push(
            Resource::new(format!("{URI_SCHEME}setup-guide"), "Setup Guide")
                .with_description("The generated setup guide.")
                .with_mime_type(MIME_MARKDOWN),
        );
    }

    if data.overview.is_some() {
        resources.push(
            Resource::new(
                format!("{URI_SCHEME}architecture-overview"),
                "Architecture Overview",
            )
            .with_description("The generated architecture overview.")
            .with_mime_type(MIME_MARKDOWN),
        );
    }

    if data.combined.is_some() {
        resources.push(
            Resource::new(format!("{URI_SCHEME}index"), "Tutorial Index")
                .with_description("The combined tutorial index.")
                .with_mime_type(MIME_MARKDOWN),
        );
    }

    resources
}

/// Build the list of resource templates (parameterised URIs).
///
/// Currently only the `checkpoint://chapter/{index}` template is exposed.
#[must_use]
pub fn list_resource_templates() -> Vec<ResourceTemplate> {
    vec![ResourceTemplate::new(CHAPTER_TEMPLATE_URI, "Chapter by Index")
        .with_description("The chapter content for the abstraction at position {index}.")
        .with_mime_type(MIME_MARKDOWN)]
}

/// Build the `checkpoint://chapter/{index}` URI for a given abstraction index.
#[must_use]
pub fn chapter_uri(index: usize) -> String {
    format!("{URI_SCHEME}chapter/{index}")
}

/// The content returned by [`read_resource`] for a given URI.
///
/// On success this is a [`ReadResourceResult`] with one [`ResourceContents`]
/// entry. On failure it is a human-readable error message suitable for
/// returning to the MCP client as a not-found error.
pub enum ReadOutcome {
    /// The resource was found and read successfully.
    Found(ReadResourceResult),
    /// The URI does not correspond to any known resource.
    NotFound(String),
}

/// Read a resource by its `checkpoint://` URI.
///
/// Parses the URI, looks up the corresponding data in `data`, and returns
/// the content as a [`ReadOutcome`].
///
/// # Errors
///
/// Returns [`ReadOutcome::NotFound`] when the URI is not recognised or the
/// backing stage has not completed.
#[must_use]
pub fn read_resource(uri: &str, data: &CheckpointData) -> ReadOutcome {
    let path = uri.strip_prefix(URI_SCHEME).unwrap_or(uri);

    match path {
        "metadata" => json_content(uri, &data.checkpoint),
        "abstractions" => match &data.abstractions {
            Some(abs) => json_content(uri, abs),
            None => not_found_stage(uri, StageId::Identify),
        },
        "relationships" => match &data.relationships {
            Some(rel) => json_content(uri, rel),
            None => not_found_stage(uri, StageId::Relationships),
        },
        "chapter-order" => match &data.chapter_order {
            Some(order) => json_content(uri, order),
            None => not_found_stage(uri, StageId::Order),
        },
        "files" => json_content(uri, &data.files),
        "setup-guide" => match &data.setup_guide {
            Some(guide) => markdown_content(uri, &guide.markdown),
            None => not_found_stage(uri, StageId::Setup),
        },
        "architecture-overview" => match &data.overview {
            Some(overview) => markdown_content(uri, &overview.markdown),
            None => not_found_stage(uri, StageId::Overview),
        },
        "index" => match &data.combined {
            Some(combined) => markdown_content(uri, &combined.index_markdown),
            None => not_found_stage(uri, StageId::Combine),
        },
        rest if rest.starts_with("chapter/") => {
            let index_str = &rest["chapter/".len()..];
            let index: usize = match index_str.parse() {
                Ok(i) => i,
                Err(_) => return not_found(uri),
            };
            match &data.chapters {
                Some(chapters) => {
                    let chapter = chapters
                        .chapters
                        .iter()
                        .find(|c| c.abstraction_index == index);
                    match chapter {
                        Some(ch) => markdown_content(uri, &ch.markdown),
                        None => not_found(uri),
                    }
                }
                None => not_found_stage(uri, StageId::Chapters),
            }
        }
        _ => not_found(uri),
    }
}

/// Wrap a serialisable value as a JSON resource content entry.
fn json_content<T: serde::Serialize>(uri: &str, value: &T) -> ReadOutcome {
    match serde_json::to_string_pretty(value) {
        Ok(text) => ReadOutcome::Found(text_resource(uri, text)),
        Err(e) => ReadOutcome::NotFound(format!("failed to serialize resource {uri}: {e}")),
    }
}

/// Wrap a Markdown string as a Markdown resource content entry.
fn markdown_content(uri: &str, markdown: &str) -> ReadOutcome {
    ReadOutcome::Found(text_resource_with_mime(uri, markdown.to_string(), MIME_MARKDOWN))
}

/// Create a [`ReadResourceResult`] with `application/json` text content.
fn text_resource(uri: &str, text: String) -> ReadResourceResult {
    text_resource_with_mime(uri, text, MIME_JSON)
}

/// Create a [`ReadResourceResult`] with a specific MIME type.
fn text_resource_with_mime(uri: &str, text: String, mime: &str) -> ReadResourceResult {
    let contents = vec![ResourceContents::text(text, uri).with_mime_type(mime)];
    ReadResourceResult::new(contents)
}

/// Build a not-found outcome with a stage-specific hint.
fn not_found_stage(uri: &str, stage: StageId) -> ReadOutcome {
    ReadOutcome::NotFound(format!(
        "resource {uri} is not available: stage '{}' has not completed",
        stage.as_str()
    ))
}

/// Build a generic not-found outcome.
fn not_found(uri: &str) -> ReadOutcome {
    ReadOutcome::NotFound(format!("unknown resource URI: {uri}"))
}

/// Convert a [`ReadOutcome`] into a [`ListResourcesResult`] (for convenience
/// in tests and handlers that want the full result envelope).
#[must_use]
pub fn to_list_resources_result(resources: Vec<Resource>) -> ListResourcesResult {
    ListResourcesResult {
        resources,
        ..Default::default()
    }
}

/// Convert a list of templates into a [`ListResourceTemplatesResult`].
#[must_use]
pub fn to_list_resource_templates_result(
    templates: Vec<ResourceTemplate>,
) -> ListResourceTemplatesResult {
    ListResourceTemplatesResult {
        resource_templates: templates,
        ..Default::default()
    }
}

/// Borrowed display name for a resource URI (used in annotations).
#[allow(dead_code)]
fn resource_name(uri: &str) -> Cow<'_, str> {
    let path = uri.strip_prefix(URI_SCHEME).unwrap_or(uri);
    Cow::Borrowed(path)
}

/// Default annotations for read-only resources (priority 0.5, no audience).
#[allow(dead_code)]
fn default_annotations() -> Annotations {
    Annotations::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::{
        Abstraction, Chapter, ChapterOrder, ChapterResult, CombinedTutorial, Relationship,
        RelationshipsResult, RunConfig, SetupGuide, Tier,
    };
    use brigid_core::{ArchitectureOverview, CheckpointV1, IdentifyResult, StageId};
    use brigid_pipeline::records_from_files;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Monotonic counter for unique temp dirs.
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-mcp-res-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a fully populated `CheckpointData` for testing.
    fn full_checkpoint_data() -> (PathBuf, CheckpointData) {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.txt", b"hello")]);
        store.save(cp.clone(), &files).unwrap();

        let identify = IdentifyResult::new(vec![
            Abstraction::new("Core", "The core system", Tier::M, "module"),
            Abstraction::new("Routing", "Routes requests", Tier::S, "class"),
        ]);
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());

        let relationships = RelationshipsResult::new(
            "A small web framework.",
            vec![Relationship::new(0, 1, "routes to", "calls")],
        );
        cp.relationships = Some(relationships.to_checkpoint_value().unwrap());

        let order = ChapterOrder::new(vec![1, 0]);
        cp.order = Some(order.to_checkpoint_value().unwrap());

        let chapters = ChapterResult::new(vec![
            Chapter::new(0, 1, "Core", "# Core\n\nThe core.", Tier::M, "module", "footer 0"),
            Chapter::new(1, 2, "Routing", "# Routing\n\nRoutes.", Tier::S, "class", "footer 1"),
        ]);
        let chapter_entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, chapter_entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");

        let guide = SetupGuide::new("# Setup\n\nInstall Rust", 42, vec!["gap".into()], true);
        let setup_entry = store.write_setup_guide(&dir, &guide).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Setup, vec![setup_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Setup, "t3");

        let overview = ArchitectureOverview::new("# Architecture\n", vec!["app1".into()]);
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

    /// Build a minimal `CheckpointData` (only fetch stage complete).
    fn minimal_checkpoint_data() -> (PathBuf, CheckpointData) {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}")]);
        store.save(cp, &files).unwrap();

        let loader = crate::CheckpointLoader::new(&dir);
        let data = loader.load().expect("checkpoint should load");
        (dir, data)
    }

    fn cleanup(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    // --- List resources tests ---

    #[test]
    fn list_resources_full_checkpoint_includes_all_static_resources() {
        let (dir, data) = full_checkpoint_data();
        let resources = list_resources(&data);

        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert!(uris.contains(&"checkpoint://metadata"));
        assert!(uris.contains(&"checkpoint://abstractions"));
        assert!(uris.contains(&"checkpoint://relationships"));
        assert!(uris.contains(&"checkpoint://chapter-order"));
        assert!(uris.contains(&"checkpoint://files"));
        assert!(uris.contains(&"checkpoint://setup-guide"));
        assert!(uris.contains(&"checkpoint://architecture-overview"));
        assert!(uris.contains(&"checkpoint://index"));

        // Two chapter resources (one per abstraction).
        let chapter_uris: Vec<&str> = uris
            .iter()
            .copied()
            .filter(|u| u.starts_with("checkpoint://chapter/"))
            .collect();
        assert_eq!(chapter_uris.len(), 2);

        cleanup(&dir);
    }

    #[test]
    fn list_resources_minimal_checkpoint_excludes_missing_stages() {
        let (dir, data) = minimal_checkpoint_data();
        let resources = list_resources(&data);

        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert!(uris.contains(&"checkpoint://metadata"));
        assert!(uris.contains(&"checkpoint://files"));
        // These should NOT be present since stages haven't completed.
        assert!(!uris.contains(&"checkpoint://abstractions"));
        assert!(!uris.contains(&"checkpoint://relationships"));
        assert!(!uris.contains(&"checkpoint://setup-guide"));
        assert!(!uris.contains(&"checkpoint://index"));

        cleanup(&dir);
    }

    #[test]
    fn list_resource_templates_has_chapter_template() {
        let templates = list_resource_templates();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].uri_template, CHAPTER_TEMPLATE_URI);
        assert_eq!(templates[0].mime_type.as_deref(), Some(MIME_MARKDOWN));
    }

    // --- Read resource tests ---

    #[test]
    fn read_metadata_returns_json() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://metadata", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                assert_eq!(result.contents.len(), 1);
                match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, mime_type, .. } => {
                        assert!(mime_type.as_deref() == Some(MIME_JSON));
                        assert!(!text.is_empty());
                        assert!(text.contains("\"version\""));
                    }
                    _ => panic!("expected text content"),
                }
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_abstractions_returns_json() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://abstractions", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                let text = match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("Core"));
                assert!(text.contains("Routing"));
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_relationships_returns_json() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://relationships", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                let text = match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("routes to"));
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_chapter_order_returns_json() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://chapter-order", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                let text = match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("ordered_indices"));
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_files_returns_json() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://files", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                let text = match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("a.rs"));
                assert!(text.contains("b.txt"));
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_chapter_by_index_returns_markdown() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://chapter/0", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, mime_type, .. } => {
                        assert_eq!(mime_type.as_deref(), Some(MIME_MARKDOWN));
                        assert!(text.contains("# Core"));
                    }
                    _ => panic!("expected text"),
                }
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_chapter_invalid_index_returns_not_found() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://chapter/99", &data);
        assert!(matches!(outcome, ReadOutcome::NotFound(_)));
        cleanup(&dir);
    }

    #[test]
    fn read_chapter_non_numeric_index_returns_not_found() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://chapter/abc", &data);
        assert!(matches!(outcome, ReadOutcome::NotFound(_)));
        cleanup(&dir);
    }

    #[test]
    fn read_setup_guide_returns_markdown() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://setup-guide", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, mime_type, .. } => {
                        assert_eq!(mime_type.as_deref(), Some(MIME_MARKDOWN));
                        assert!(text.contains("# Setup"));
                    }
                    _ => panic!("expected text"),
                }
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_architecture_overview_returns_markdown() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://architecture-overview", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                let text = match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("# Architecture"));
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_index_returns_markdown() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://index", &data);
        match outcome {
            ReadOutcome::Found(result) => {
                let text = match &result.contents[0] {
                    ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("# Index"));
            }
            ReadOutcome::NotFound(msg) => panic!("expected found, got: {msg}"),
        }
        cleanup(&dir);
    }

    #[test]
    fn read_unknown_uri_returns_not_found() {
        let (dir, data) = full_checkpoint_data();
        let outcome = read_resource("checkpoint://nonexistent", &data);
        assert!(matches!(outcome, ReadOutcome::NotFound(_)));
        cleanup(&dir);
    }

    #[test]
    fn read_missing_stage_returns_not_found_with_hint() {
        let (dir, data) = minimal_checkpoint_data();
        let outcome = read_resource("checkpoint://abstractions", &data);
        match outcome {
            ReadOutcome::NotFound(msg) => {
                assert!(msg.contains("identify"));
            }
            ReadOutcome::Found(_) => panic!("expected not found"),
        }
        cleanup(&dir);
    }

    #[test]
    fn chapter_uri_formats_correctly() {
        assert_eq!(chapter_uri(0), "checkpoint://chapter/0");
        assert_eq!(chapter_uri(42), "checkpoint://chapter/42");
    }
}
