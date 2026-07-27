#![allow(missing_docs)]

//! Criterion benchmark: file context selection algorithm.
//!
//! Measures `select_chapter_file_context` with varying numbers of entry files,
//! file indices, and budget sizes. This is a pure function (no I/O) that
//! collects candidate paths, truncates content, and builds the context string.

use brigid_core::{Abstraction, AbstractionKind, Tier};
use brigid_pipeline::chapters::select_chapter_file_context;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::time::Duration;

/// Build an abstraction with `n` entry files and `n` file indices.
fn make_abstraction(n: usize) -> Abstraction {
    let mut abs = Abstraction::new(
        "Core Engine",
        "The main processing engine of the application.",
        Tier::L,
        AbstractionKind::new("module"),
    );
    abs.entry_files = (0..n).map(|i| format!("src/core/mod_{i}.rs")).collect();
    abs.file_indices = (0..n).collect();
    abs.apps = vec!["engine".to_string()];
    abs
}

/// Build `n` file contents of fixed size.
fn make_file_contents(n: usize, chars_per_file: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| {
            let path = format!("src/core/mod_{i}.rs");
            let content = format!(
                "//! Core module {i}\n\
                 pub fn process_{i}(input: &str) -> String {{\n\
                 \tlet mut result = String::new();\n\
                 \tfor c in input.chars() {{\n\
                 \t\tresult.push(c);\n\
                 \t}}\n\
                 \tresult\n\
                 }}\n"
            );
            // Pad or truncate to target size.
            let content = if content.len() < chars_per_file {
                let padding = "x".repeat(chars_per_file - content.len());
                format!("{content}\n// padding\n{padding}\n")
            } else {
                content
            };
            (path, content)
        })
        .collect()
}

fn bench_file_context_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("file_context_selection");
    group.sample_size(30);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Vary the number of files.
    for n in [5usize, 20, 50] {
        let abs = make_abstraction(n);
        let files = make_file_contents(n, 500);
        group.bench_with_input(BenchmarkId::new("files", n), &n, |b, &_| {
            b.iter(|| {
                let ctx = select_chapter_file_context(
                    black_box(&abs),
                    black_box(&files),
                    black_box(80_000),
                    black_box(12_000),
                );
                let _ = black_box(ctx);
            });
        });
    }

    // Vary the budget (tight budget forces more stubs).
    let abs = make_abstraction(30);
    let files = make_file_contents(30, 2000);
    for budget in [1_000usize, 10_000, 80_000] {
        group.bench_with_input(BenchmarkId::new("budget", budget), &budget, |b, &_| {
            b.iter(|| {
                let ctx = select_chapter_file_context(
                    black_box(&abs),
                    black_box(&files),
                    black_box(budget),
                    black_box(12_000),
                );
                let _ = black_box(ctx);
            });
        });
    }

    // Vary file content size (truncation path).
    let abs_small = make_abstraction(10);
    for chars in [500usize, 5_000, 20_000] {
        let files = make_file_contents(10, chars);
        group.bench_with_input(BenchmarkId::new("file_size", chars), &chars, |b, &_| {
            b.iter(|| {
                let ctx = select_chapter_file_context(
                    black_box(&abs_small),
                    black_box(&files),
                    black_box(80_000),
                    black_box(12_000),
                );
                let _ = black_box(ctx);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_file_context_selection);
criterion_main!(benches);
