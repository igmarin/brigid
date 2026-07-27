#![allow(missing_docs)]

//! Criterion benchmark: single-chapter generation via `MockClient`.
//!
//! Measures the full `write_single_chapter` path: prompt rendering (outline +
//! chapter templates), secret redaction, MockClient LLM call, mermaid
//! sanitization, and evidence-footer attachment. No network or real LLM calls.

use brigid_core::{Abstraction, AbstractionKind, IdentifyResult, Tier};
use brigid_llm::MockClient;
use brigid_pipeline::chapters::{DiagramLevel, write_single_chapter};
use brigid_pipeline::prompts::PromptRenderer;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::time::Duration;
use tokio::runtime::Runtime;

/// Fixed mock chapter markdown returned by the MockClient.
///
/// Includes two mermaid blocks so the diagram quota for Tier L / Standard is
/// satisfied and no warnings are emitted.
const MOCK_CHAPTER_MD: &str = "\
# Authentication Service

## Motivation

This chapter covers the authentication service.

```mermaid
flowchart TD
    A[Client] --> B[Auth Service]
    B --> C[Token Store]
```

## Architecture

```mermaid
sequenceDiagram
    participant C as Client
    participant S as Auth Service
    C->>S: login(user, pass)
    S-->>C: token
```

## Summary

The auth service validates credentials and issues tokens.
";

/// Build a fixed abstraction with `n` entry files and file indices.
fn make_abstraction(n: usize) -> Abstraction {
    let mut abs = Abstraction::new(
        "Authentication Service",
        "Handles user authentication and token issuance.",
        Tier::M,
        AbstractionKind::new("module"),
    );
    abs.entry_files = (0..n).map(|i| format!("src/auth/mod_{i}.rs")).collect();
    abs.file_indices = (0..n).collect();
    abs.apps = vec!["web".to_string()];
    abs
}

/// Build fixed file contents for the context.
fn make_file_contents(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| {
            let path = format!("src/auth/mod_{i}.rs");
            let content = format!(
                "//! Auth module {i}\n\
                 pub fn authenticate(user: &str, pass: &str) -> bool {{\n\
                 \t// validate credentials\n\
                 \t!user.is_empty() && !pass.is_empty()\n\
                 }}\n"
            );
            (path, content)
        })
        .collect()
}

/// Build a mock chapter response with enough mermaid blocks for the tier.
fn mock_chapter_md(i: usize) -> String {
    let tier = if i % 3 == 0 {
        Tier::L
    } else if i % 3 == 1 {
        Tier::M
    } else {
        Tier::S
    };
    let blocks = match tier {
        Tier::L => 3,
        Tier::M => 2,
        Tier::S => 1,
    };
    let mut md = format!("# Concept {i}\n\nContent for concept {i}.\n\n");
    for b in 0..blocks {
        md.push_str(&format!(
            "```mermaid\nflowchart TD\n  N{b}A[Node {b}A] --> N{b}B[Node {b}B]\n```\n\n"
        ));
    }
    md
}

fn bench_write_single_chapter(c: &mut Criterion) {
    let renderer = PromptRenderer::new().expect("renderer");
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("chapter_generation");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for n in [1usize, 4, 8] {
        let abs = make_abstraction(n);
        let file_contents = make_file_contents(n);
        let full_listing = brigid_pipeline::chapters::select_chapter_file_context(
            &abs,
            &file_contents,
            80_000,
            12_000,
        );

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &_| {
            b.iter(|| {
                let client = MockClient::new(MOCK_CHAPTER_MD);
                let result = rt.block_on(write_single_chapter(
                    black_box(&client),
                    black_box(&renderer),
                    black_box(&abs),
                    black_box(0),
                    black_box(1),
                    black_box("None"),
                    black_box("None"),
                    black_box("Chapter 1: Authentication Service"),
                    black_box(""),
                    black_box(&full_listing),
                    black_box("TestProject"),
                    black_box("Use English."),
                    black_box("English"),
                    black_box(DiagramLevel::Standard),
                ));
                let _ = black_box(result);
            });
        });
    }

    // Also benchmark the full `write_chapters` with multiple abstractions.
    for n in [1usize, 3, 5] {
        let abstractions: Vec<Abstraction> = (0..n)
            .map(|i| {
                let mut a = Abstraction::new(
                    format!("Concept {i}"),
                    format!("Description for concept {i}"),
                    if i % 3 == 0 {
                        Tier::L
                    } else if i % 3 == 1 {
                        Tier::M
                    } else {
                        Tier::S
                    },
                    AbstractionKind::new("module"),
                );
                a.entry_files = vec![format!("src/concept_{i}.rs")];
                a.file_indices = vec![i];
                a.apps = vec!["web".to_string()];
                a
            })
            .collect();
        let file_contents: Vec<(String, String)> = (0..n)
            .map(|i| {
                (
                    format!("src/concept_{i}.rs"),
                    format!("//! concept {i}\npub fn f() -> i32 {{ {i} }}\n"),
                )
            })
            .collect();
        let identify = IdentifyResult::new(abstractions.clone());
        let order = brigid_core::ChapterOrder::new((0..n).collect());

        group.bench_with_input(BenchmarkId::new("write_chapters", n), &n, |b, &_| {
            b.iter(|| {
                let responses: Vec<String> = (0..n).map(mock_chapter_md).collect();
                let client = MockClient::with_responses(responses).expect("mock");
                let config = brigid_pipeline::chapters::ChaptersConfig::default();
                let result = rt.block_on(brigid_pipeline::chapters::write_chapters(
                    black_box(&client),
                    black_box(&renderer),
                    black_box(&identify),
                    black_box(&order),
                    black_box(&file_contents),
                    black_box(&config),
                    black_box(None),
                ));
                let _ = black_box(result);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_write_single_chapter);
criterion_main!(benches);
