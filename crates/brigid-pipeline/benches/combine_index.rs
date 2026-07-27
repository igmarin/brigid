#![allow(missing_docs)]

//! Criterion benchmark: combine index building.
//!
//! Measures `build_index_markdown` — the pure assembly of `index.md` from
//! abstractions, relationships, chapter order, chapter content, and i18n
//! chrome strings. Includes Mermaid diagram generation and sanitization.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_core::{
    Abstraction, AbstractionKind, Chapter, ChapterOrder, ChapterResult, ChromeStrings, Locale,
    ModuleKey, Relationship, RelationshipsResult, Tier,
};
use brigid_pipeline::combine::build_index_markdown;
use std::time::Duration;

/// Build `n` abstractions with fixed content.
fn make_abstractions(n: usize) -> Vec<Abstraction> {
    (0..n)
        .map(|i| {
            let mut abs = Abstraction::new(
                format!("Concept {i}"),
                format!("Description for concept {i}"),
                match i % 3 {
                    0 => Tier::L,
                    1 => Tier::M,
                    _ => Tier::S,
                },
                AbstractionKind::new("module"),
            );
            abs.apps = vec![if i % 2 == 0 {
                "web".to_string()
            } else {
                "api".to_string()
            }];
            abs.entry_files = vec![format!("src/concept_{i}.rs")];
            abs.file_indices = vec![i];
            abs
        })
        .collect()
}

/// Build relationships between abstractions.
fn make_relationships(n: usize) -> Vec<Relationship> {
    (0..n.saturating_sub(1))
        .map(|i| Relationship::new(i, i + 1, format!("calls {i}"), "calls"))
        .collect()
}

/// Build `n` chapters matching the abstractions.
fn make_chapters(n: usize) -> ChapterResult {
    let chapters: Vec<Chapter> = (0..n)
        .map(|i| {
            Chapter::new(
                i,
                i + 1,
                format!("Concept {i}"),
                format!("# Concept {i}\n\nContent for concept {i}.\n"),
                match i % 3 {
                    0 => Tier::L,
                    1 => Tier::M,
                    _ => Tier::S,
                },
                "module",
                format!("footer {i}"),
            )
        })
        .collect();
    ChapterResult::new(chapters)
}

fn bench_combine_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("combine_index");
    group.sample_size(30);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Vary the number of abstractions/chapters.
    for n in [3usize, 10, 25] {
        let abstractions = make_abstractions(n);
        let relationships = make_relationships(n);
        let order = ChapterOrder::new((0..n).collect());
        let chapters = make_chapters(n);
        let modules = vec![ModuleKey::new("web"), ModuleKey::new("api")];
        let chrome = ChromeStrings::for_locale(Locale::En);
        let rel_result = RelationshipsResult::new("A web app.", relationships.clone());

        group.bench_with_input(BenchmarkId::new("build_index", n), &n, |b, &_| {
            b.iter(|| {
                let md = build_index_markdown(
                    black_box(&abstractions),
                    black_box(&rel_result.relationships),
                    black_box(&order),
                    black_box(&chapters),
                    black_box(None),
                    black_box(None),
                    black_box(&modules),
                    black_box(&chrome),
                );
                let _ = black_box(md);
            });
        });
    }

    // Benchmark with setup guide and overview present.
    for n in [5usize, 15] {
        let abstractions = make_abstractions(n);
        let relationships = make_relationships(n);
        let order = ChapterOrder::new((0..n).collect());
        let chapters = make_chapters(n);
        let modules = vec![ModuleKey::new("_root")];
        let chrome = ChromeStrings::for_locale(Locale::En);
        let rel_result = RelationshipsResult::new("A web app.", relationships.clone());
        let setup = brigid_core::SetupGuide::new("# Setup\n\nInstall deps.", 80, vec![], false);
        let overview = brigid_core::ArchitectureOverview::new(
            "# Architecture\n\nOverview.",
            vec!["web".into()],
        );

        group.bench_with_input(
            BenchmarkId::new("build_index_with_extras", n),
            &n,
            |b, &_| {
                b.iter(|| {
                    let md = build_index_markdown(
                        black_box(&abstractions),
                        black_box(&rel_result.relationships),
                        black_box(&order),
                        black_box(&chapters),
                        black_box(Some(&setup)),
                        black_box(Some(&overview)),
                        black_box(&modules),
                        black_box(&chrome),
                    );
                    let _ = black_box(md);
                });
            },
        );
    }

    // Benchmark with multiple modules (triggers system map diagram).
    for n in [5usize, 15] {
        let abstractions = make_abstractions(n);
        let relationships = make_relationships(n);
        let order = ChapterOrder::new((0..n).collect());
        let chapters = make_chapters(n);
        let modules = vec![
            ModuleKey::new("apps/web"),
            ModuleKey::new("apps/api"),
            ModuleKey::new("apps/worker"),
        ];
        let chrome = ChromeStrings::for_locale(Locale::En);
        let rel_result = RelationshipsResult::new("A monorepo.", relationships.clone());

        group.bench_with_input(BenchmarkId::new("build_index_monorepo", n), &n, |b, &_| {
            b.iter(|| {
                let md = build_index_markdown(
                    black_box(&abstractions),
                    black_box(&rel_result.relationships),
                    black_box(&order),
                    black_box(&chapters),
                    black_box(None),
                    black_box(None),
                    black_box(&modules),
                    black_box(&chrome),
                );
                let _ = black_box(md);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_combine_index);
criterion_main!(benches);
