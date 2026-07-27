#![allow(missing_docs)]

//! Criterion benchmark: prompt template rendering via minijinja.
//!
//! Measures `PromptRenderer::render` for each of the ten embedded templates
//! with fixed contexts. This exercises template parsing (cached in the
//! Environment) and variable substitution.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_pipeline::prompts::{PromptId, PromptRenderer, sanitize_template_input};
use serde_json::json;
use std::time::Duration;

fn bench_template_rendering(c: &mut Criterion) {
    let renderer = PromptRenderer::new().expect("renderer");

    let mut group = c.benchmark_group("template_rendering");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Benchmark rendering each template with a representative context.
    let contexts: Vec<(PromptId, serde_json::Value)> = vec![
        (
            PromptId::IdentifySingleShot,
            json!({
                "project_name": "TestProject",
                "context": "src/main.rs\nsrc/lib.rs\nsrc/utils.rs\n",
                "language_instruction": "Use English.",
                "max_abstraction_num": 10,
                "name_lang_hint": "",
                "desc_lang_hint": "",
                "file_listing": "src/main.rs\nsrc/lib.rs\nsrc/utils.rs\nsrc/config.rs\nsrc/db.rs\n",
            }),
        ),
        (
            PromptId::IdentifyMap,
            json!({
                "project_name": "TestProject",
                "batch_num": 1,
                "batch_total": 3,
                "module_label": "src",
                "file_context": "src/main.rs\nsrc/lib.rs\n",
                "language_instruction": "Use English.",
                "max_abstraction_num": 8,
                "name_lang_hint": "",
                "desc_lang_hint": "",
            }),
        ),
        (
            PromptId::IdentifyReduce,
            json!({
                "project_name": "TestProject",
                "candidate_json": "[]",
                "max_abstraction_num": 10,
                "language_instruction": "Use English.",
                "name_lang_hint": "",
                "desc_lang_hint": "",
            }),
        ),
        (
            PromptId::AnalyzeRelationships,
            json!({
                "project_name": "TestProject",
                "abstraction_json": "[]",
                "language_instruction": "Use English.",
            }),
        ),
        (
            PromptId::OrderChapters,
            json!({
                "project_name": "TestProject",
                "abstraction_json": "[]",
                "language_instruction": "Use English.",
            }),
        ),
        (
            PromptId::ChapterOutline,
            json!({
                "lang": "English",
                "tier": "M",
                "diagram_level": "standard",
                "need": 1,
            }),
        ),
        (
            PromptId::WriteChapter,
            json!({
                "project_name": "TestProject",
                "abstraction_name": "Authentication Service",
                "chapter_num": 1,
                "abstraction_description": "Handles user authentication.",
                "tier": "M",
                "kind": "module",
                "apps_line": "web",
                "entry_list": "- src/auth/mod.rs",
                "full_chapter_listing": "Chapter 1: Authentication Service",
                "prev_link": "None",
                "next_link": "None",
                "previous_chapters_summary": "",
                "file_context_str": "# File: src/auth/mod.rs\nfn auth() {}",
                "chapter_outline": "## Outline\n...",
                "need": 1,
                "language_instruction": "Use English.",
            }),
        ),
        (
            PromptId::ReviewChapter,
            json!({
                "project_name": "TestProject",
                "chapter_markdown": "# Authentication\n\nContent here.",
                "language_instruction": "Use English.",
            }),
        ),
        (
            PromptId::WriteSetupGuide,
            json!({
                "project_name": "TestProject",
                "language_instruction": "Use English.",
                "setup_signals": "README found, no Dockerfile",
                "file_listing": "README.md\nDockerfile\nMakefile\n",
            }),
        ),
        (
            PromptId::WriteArchitectureOverview,
            json!({
                "project_name": "TestProject",
                "language_instruction": "Use English.",
                "project_summary": "A web application with auth and API layers.",
                "app_inventory": "web\napi\nworker",
            }),
        ),
    ];

    for (id, ctx) in &contexts {
        group.bench_with_input(BenchmarkId::new("render", id.as_str()), id, |b, _id| {
            b.iter(|| {
                let result = renderer.render(black_box(*id), black_box(ctx));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark renderer construction (template parsing).
    group.bench_function("renderer_new", |b| {
        b.iter(|| {
            let r = PromptRenderer::new();
            let _ = black_box(r);
        });
    });

    // Benchmark sanitize_template_input with varying input sizes.
    for size in [100usize, 1_000, 5_000] {
        // The literal string we want is:
        //   Hello {{ name }}! {{ value }} {% if x %}bad{% endif %} <padding>
        // In Rust format strings, `{{` → `{` and `}}` → `}`, so we double
        // every brace that should appear literally.
        let input = format!(
            "Hello {{{{ name }}}}! {{{{ value }}}} {{% if x %}}bad{{% endif %}} {}",
            "x".repeat(size)
        );
        group.bench_with_input(BenchmarkId::new("sanitize", size), &size, |b, &_| {
            b.iter(|| {
                let safe = sanitize_template_input(black_box(&input));
                let _ = black_box(safe);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_template_rendering);
criterion_main!(benches);
