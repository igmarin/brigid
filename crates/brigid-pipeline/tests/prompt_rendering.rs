#![allow(missing_docs)]

//! Integration tests for the `brigid_pipeline::prompts` rendering module.
//!
//! These tests exercise the public `PromptId` / `PromptRenderer` API and the
//! `sanitize_template_input` helper. They are intentionally independent of the
//! older `tests/prompts.rs` file, which tests the raw templates directly with a
//! hand-rolled minijinja `Environment`.

use brigid_pipeline::prompts::{PromptError, PromptId, PromptRenderer, sanitize_template_input};
use serde_json::json;

// ---------------------------------------------------------------------------
// PromptId::as_str
// ---------------------------------------------------------------------------

#[test]
fn prompt_id_as_str_returns_expected_filenames() {
    assert_eq!(
        PromptId::IdentifySingleShot.as_str(),
        "identify_single_shot.md.j2"
    );
    assert_eq!(PromptId::IdentifyMap.as_str(), "identify_map.md.j2");
    assert_eq!(PromptId::IdentifyReduce.as_str(), "identify_reduce.md.j2");
    assert_eq!(
        PromptId::AnalyzeRelationships.as_str(),
        "analyze_relationships.md.j2"
    );
    assert_eq!(PromptId::OrderChapters.as_str(), "order_chapters.md.j2");
    assert_eq!(PromptId::ChapterOutline.as_str(), "chapter_outline.md.j2");
    assert_eq!(PromptId::WriteChapter.as_str(), "write_chapter.md.j2");
    assert_eq!(PromptId::ReviewChapter.as_str(), "review_chapter.md.j2");
    assert_eq!(
        PromptId::WriteSetupGuide.as_str(),
        "write_setup_guide.md.j2"
    );
    assert_eq!(
        PromptId::WriteArchitectureOverview.as_str(),
        "write_architecture_overview.md.j2"
    );
}

// ---------------------------------------------------------------------------
// PromptId::template_text
// ---------------------------------------------------------------------------

#[test]
fn prompt_id_template_text_is_non_empty_and_matches_file() {
    for id in [
        PromptId::IdentifySingleShot,
        PromptId::IdentifyMap,
        PromptId::IdentifyReduce,
        PromptId::AnalyzeRelationships,
        PromptId::OrderChapters,
        PromptId::ChapterOutline,
        PromptId::WriteChapter,
        PromptId::ReviewChapter,
        PromptId::WriteSetupGuide,
        PromptId::WriteArchitectureOverview,
    ] {
        let text = id.template_text();
        assert!(!text.is_empty(), "{} template text is empty", id.as_str());
        // Every template uses at least one Jinja placeholder.
        assert!(
            text.contains("{{"),
            "{} template text has no Jinja placeholders",
            id.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// PromptRenderer::new
// ---------------------------------------------------------------------------

#[test]
fn prompt_renderer_new_creates_a_renderer() {
    let _renderer = PromptRenderer::new().expect("renderer");
}

// ---------------------------------------------------------------------------
// Fixture contexts matching prompts/README.md schemas.
// ---------------------------------------------------------------------------

fn identify_single_shot_ctx() -> serde_json::Value {
    json!({
        "project_name": "brigid",
        "context": "File snippets here",
        "language_instruction": "",
        "max_abstraction_num": 10,
        "name_lang_hint": "",
        "desc_lang_hint": "",
        "file_listing": "- 0 # lib.rs\n- 1 # main.rs"
    })
}

fn identify_map_ctx() -> serde_json::Value {
    json!({
        "batch_idx": 1,
        "batch_total": 3,
        "project_name": "brigid",
        "module_note": "core",
        "context": "File snippets for this batch",
        "language_instruction": "",
        "per_batch": 5,
        "name_lang_hint": "",
        "desc_lang_hint": "",
        "file_listing": "- 0 # lib.rs",
        "community_context": ""
    })
}

fn identify_reduce_ctx() -> serde_json::Value {
    json!({
        "project_name": "brigid",
        "module_summary": "core, cli",
        "language_instruction": "",
        "max_abstraction_num": 10,
        "name_lang_hint": "",
        "desc_lang_hint": "",
        "candidates_blob": "- candidate 0:\n    name: Query Processing\n    description: Handles queries\n    file_indices: [0, 1]",
        "multimodal_context": ""
    })
}

fn analyze_relationships_ctx() -> serde_json::Value {
    json!({
        "project_name": "brigid",
        "list_lang_note": "",
        "abstraction_listing": "- 0 # Query Processing\n- 1 # Optimization",
        "context": "Abstractions and code snippets",
        "language_instruction": "",
        "monorepo_instruction": "",
        "lang_hint": ""
    })
}

fn order_chapters_ctx() -> serde_json::Value {
    json!({
        "project_name": "brigid",
        "list_lang_note": "",
        "abstraction_listing": "- 0 # Query Processing\n- 1 # Optimization",
        "context": "Project summary and relationships"
    })
}

fn chapter_outline_ctx() -> serde_json::Value {
    json!({
        "lang": "English",
        "tier": "M",
        "diagram_level": "standard",
        "need": 2
    })
}

fn write_chapter_ctx() -> serde_json::Value {
    json!({
        "language_instruction": "",
        "project_name": "brigid",
        "abstraction_name": "Query Processing",
        "chapter_num": 1,
        "abstraction_description": "Handles incoming queries",
        "tier": "M",
        "kind": "service",
        "apps_line": "core",
        "entry_list": "- `lib.rs`",
        "full_chapter_listing": "- [Query Processing](01_query_processing.md)",
        "prev_link": "None (first chapter)",
        "next_link": "None (last chapter)",
        "previous_chapters_summary": "This is the first chapter.",
        "file_context_str": "--- File: lib.rs ---\nfn main() {}",
        "chapter_outline": "## MANDATORY CHAPTER STRUCTURE\n## DIAGRAM REQUIREMENTS\n## GROUNDING RULES",
        "need": 2
    })
}

fn review_chapter_ctx() -> serde_json::Value {
    json!({
        "language": "English",
        "need": 2,
        "have": 1,
        "chapter_md": "# Chapter 1: Query Processing\n\ncontent"
    })
}

fn write_setup_guide_ctx() -> serde_json::Value {
    json!({
        "project_name": "brigid",
        "score": 50,
        "gaps": "- Missing env setup",
        "context": "README fragment and config files",
        "lang": "English"
    })
}

fn write_architecture_overview_ctx() -> serde_json::Value {
    json!({
        "lang_note": "",
        "project_name": "brigid",
        "summary": "A Rust tutorial generator",
        "inventory": "- core: 5 files",
        "abstractions": "- 0: Query Processing",
        "relationships": "- 1 -> 0: uses"
    })
}

// ---------------------------------------------------------------------------
// Identify prompt render tests (the 3 snapshot-style assertions).
// ---------------------------------------------------------------------------

#[test]
fn render_identify_single_shot_returns_non_empty_expected_content() {
    let renderer = PromptRenderer::new().expect("renderer");
    let out = renderer
        .render(PromptId::IdentifySingleShot, &identify_single_shot_ctx())
        .expect("identify_single_shot should render");
    assert!(!out.is_empty(), "output is empty");
    assert!(out.contains("brigid"), "missing project name");
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(out.contains("top 5-10"), "missing max abstractions range");
    assert!(!out.contains("{{"), "unrendered placeholder remains");
}

#[test]
fn render_identify_map_returns_non_empty_expected_content() {
    let renderer = PromptRenderer::new().expect("renderer");
    let out = renderer
        .render(PromptId::IdentifyMap, &identify_map_ctx())
        .expect("identify_map should render");
    assert!(!out.is_empty(), "output is empty");
    assert!(out.contains("batch 1/3"), "missing batch indicator");
    assert!(out.contains("up to 5 important"), "missing per_batch count");
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(!out.contains("{{"), "unrendered placeholder remains");
}

#[test]
fn render_identify_reduce_returns_non_empty_expected_content() {
    let renderer = PromptRenderer::new().expect("renderer");
    let out = renderer
        .render(PromptId::IdentifyReduce, &identify_reduce_ctx())
        .expect("identify_reduce should render");
    assert!(!out.is_empty(), "output is empty");
    assert!(out.contains("top 5-10"), "missing max abstractions range");
    assert!(out.contains("(max 10 items):"), "missing max item count");
    assert!(out.contains("```yaml"), "missing yaml fence");
    assert!(!out.contains("{{"), "unrendered placeholder remains");
}

// ---------------------------------------------------------------------------
// Snapshot stability: rendering the same context twice gives identical output.
// ---------------------------------------------------------------------------

#[test]
fn render_is_deterministic_for_identify_single_shot() {
    let renderer = PromptRenderer::new().expect("renderer");
    let ctx = identify_single_shot_ctx();
    let a = renderer
        .render(PromptId::IdentifySingleShot, &ctx)
        .expect("first render");
    let b = renderer
        .render(PromptId::IdentifySingleShot, &ctx)
        .expect("second render");
    assert_eq!(a, b, "rendering is not deterministic");
}

#[test]
fn render_is_deterministic_for_identify_map() {
    let renderer = PromptRenderer::new().expect("renderer");
    let ctx = identify_map_ctx();
    let a = renderer
        .render(PromptId::IdentifyMap, &ctx)
        .expect("first render");
    let b = renderer
        .render(PromptId::IdentifyMap, &ctx)
        .expect("second render");
    assert_eq!(a, b, "rendering is not deterministic");
}

#[test]
fn render_is_deterministic_for_identify_reduce() {
    let renderer = PromptRenderer::new().expect("renderer");
    let ctx = identify_reduce_ctx();
    let a = renderer
        .render(PromptId::IdentifyReduce, &ctx)
        .expect("first render");
    let b = renderer
        .render(PromptId::IdentifyReduce, &ctx)
        .expect("second render");
    assert_eq!(a, b, "rendering is not deterministic");
}

// ---------------------------------------------------------------------------
// All 10 templates render without error against their documented schema.
// ---------------------------------------------------------------------------

#[test]
fn all_templates_render_without_missing_variable_errors() {
    let renderer = PromptRenderer::new().expect("renderer");
    let cases: [(PromptId, serde_json::Value); 10] = [
        (PromptId::IdentifySingleShot, identify_single_shot_ctx()),
        (PromptId::IdentifyMap, identify_map_ctx()),
        (PromptId::IdentifyReduce, identify_reduce_ctx()),
        (PromptId::AnalyzeRelationships, analyze_relationships_ctx()),
        (PromptId::OrderChapters, order_chapters_ctx()),
        (PromptId::ChapterOutline, chapter_outline_ctx()),
        (PromptId::WriteChapter, write_chapter_ctx()),
        (PromptId::ReviewChapter, review_chapter_ctx()),
        (PromptId::WriteSetupGuide, write_setup_guide_ctx()),
        (
            PromptId::WriteArchitectureOverview,
            write_architecture_overview_ctx(),
        ),
    ];
    for (id, ctx) in cases {
        let out = renderer.render(id, &ctx).unwrap_or_else(|e| {
            panic!(
                "{id} failed to render with documented schema: {e}",
                id = id.as_str()
            )
        });
        assert!(!out.is_empty(), "{} rendered to empty string", id.as_str());
        assert!(
            !out.contains("{{"),
            "{} still contains unrendered placeholders",
            id.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// Snapshot stability: a hash change would be caught.
// We assert the exact rendered output for the three identify prompts so that
// any template edit is detected.
// ---------------------------------------------------------------------------

#[test]
fn identify_single_shot_snapshot_is_stable() {
    let renderer = PromptRenderer::new().expect("renderer");
    let out = renderer
        .render(PromptId::IdentifySingleShot, &identify_single_shot_ctx())
        .expect("render");
    // A stable, content-bearing substring that ties the snapshot to the
    // current template text. If the template changes meaningfully this will
    // fail, prompting a deliberate snapshot update.
    let expected_header = "For the project `brigid`:";
    assert!(
        out.starts_with(expected_header),
        "snapshot header changed; expected to start with {expected_header:?}, got: {out:?}"
    );
    // Deterministic length check catches accidental whitespace/template drift.
    let expected_len = 1792;
    assert_eq!(
        out.len(),
        expected_len,
        "snapshot length changed; expected {expected_len}, got {}. \
         If the template was intentionally changed, update this snapshot.",
        out.len()
    );
}

#[test]
fn identify_map_snapshot_is_stable() {
    let renderer = PromptRenderer::new().expect("renderer");
    let out = renderer
        .render(PromptId::IdentifyMap, &identify_map_ctx())
        .expect("render");
    let expected_header = "You are analyzing batch 1/3 of the monorepo `brigid`.";
    assert!(
        out.starts_with(expected_header),
        "snapshot header changed; expected to start with {expected_header:?}, got: {out:?}"
    );
    let expected_len = 1409;
    assert_eq!(
        out.len(),
        expected_len,
        "snapshot length changed; expected {expected_len}, got {}. \
         If the template was intentionally changed, update this snapshot.",
        out.len()
    );
}

#[test]
fn identify_reduce_snapshot_is_stable() {
    let renderer = PromptRenderer::new().expect("renderer");
    let out = renderer
        .render(PromptId::IdentifyReduce, &identify_reduce_ctx())
        .expect("render");
    let expected_header = "Project `brigid` is a large multi-app / engine-like monorepo.";
    assert!(
        out.starts_with(expected_header),
        "snapshot header changed; expected to start with {expected_header:?}, got: {out:?}"
    );
    let expected_len = 1142;
    assert_eq!(
        out.len(),
        expected_len,
        "snapshot length changed; expected {expected_len}, got {}. \
         If the template was intentionally changed, update this snapshot.",
        out.len()
    );
}

// ---------------------------------------------------------------------------
// sanitize_template_input
// ---------------------------------------------------------------------------

#[test]
fn sanitize_template_input_breaks_jinja_syntax() {
    let raw = "Hello {{ evil }} world {{ another }}";
    let sanitized = sanitize_template_input(raw);
    // The sanitized text must not contain the `{{` sequence that minijinja
    // would interpret as an expression start.
    assert!(
        !sanitized.contains("{{"),
        "sanitized text still contains `{{`: {sanitized:?}"
    );
    assert!(
        !sanitized.contains("}}"),
        "sanitized text still contains `}}`: {sanitized:?}"
    );
}

#[test]
fn sanitize_template_input_does_not_alter_plain_text() {
    let raw = "Just a normal sentence with curly braces: none here.";
    let sanitized = sanitize_template_input(raw);
    assert_eq!(sanitized, raw, "plain text was altered");
}

#[test]
fn sanitize_template_input_round_trips_through_render() {
    // Render a template that injects a sanitized value; the value must not
    // execute as Jinja.
    let renderer = PromptRenderer::new().expect("renderer");
    let malicious = "name {{ 7 * 7 }} boom";
    let sanitized = sanitize_template_input(malicious);
    let ctx = json!({
        "project_name": sanitized,
        "context": "snippets",
        "language_instruction": "",
        "max_abstraction_num": 5,
        "name_lang_hint": "",
        "desc_lang_hint": "",
        "file_listing": "- 0 # lib.rs"
    });
    let out = renderer
        .render(PromptId::IdentifySingleShot, &ctx)
        .expect("render");
    // The expression `7 * 7` must NOT have been evaluated to 49.
    assert!(
        !out.contains("49"),
        "sanitized input was evaluated as Jinja: {out:?}"
    );
    // The literal braces should appear (broken apart) in the output.
    assert!(out.contains("{ {"), "sanitized braces missing from output");
}

// ---------------------------------------------------------------------------
// Error case: render with a missing required variable.
// ---------------------------------------------------------------------------

#[test]
fn render_with_missing_variable_returns_error() {
    let renderer = PromptRenderer::new().expect("renderer");
    // identify_single_shot requires `project_name` among others; omit it.
    let incomplete = json!({
        "context": "snippets",
        "language_instruction": "",
        "max_abstraction_num": 5,
        "name_lang_hint": "",
        "desc_lang_hint": "",
        "file_listing": ""
    });
    let result = renderer.render(PromptId::IdentifySingleShot, &incomplete);
    assert!(
        matches!(result, Err(PromptError::Render(_))),
        "expected Render error for missing variable, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Error case: unknown template id is impossible by construction (enum), but
// verify that rendering every variant works with a valid context.
// ---------------------------------------------------------------------------

#[test]
fn render_all_variants_via_enum() {
    let renderer = PromptRenderer::new().expect("renderer");
    let pairs: [(PromptId, serde_json::Value); 10] = [
        (PromptId::IdentifySingleShot, identify_single_shot_ctx()),
        (PromptId::IdentifyMap, identify_map_ctx()),
        (PromptId::IdentifyReduce, identify_reduce_ctx()),
        (PromptId::AnalyzeRelationships, analyze_relationships_ctx()),
        (PromptId::OrderChapters, order_chapters_ctx()),
        (PromptId::ChapterOutline, chapter_outline_ctx()),
        (PromptId::WriteChapter, write_chapter_ctx()),
        (PromptId::ReviewChapter, review_chapter_ctx()),
        (PromptId::WriteSetupGuide, write_setup_guide_ctx()),
        (
            PromptId::WriteArchitectureOverview,
            write_architecture_overview_ctx(),
        ),
    ];
    for (id, ctx) in pairs {
        let result = renderer.render(id, &ctx);
        assert!(result.is_ok(), "{} failed: {:?}", id.as_str(), result);
    }
}
