#![allow(missing_docs)]

//! Criterion benchmark: Mermaid sanitization + validation.
//!
//! Measures `sanitize_mermaid`, `validate_mermaid`, and
//! `sanitize_markdown_mermaid_blocks` with fixed inputs of varying complexity.
//! These are pure functions (no I/O) that scrub labels, cap participants, and
//! validate diagram syntax.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use decon_core::{sanitize_markdown_mermaid_blocks, sanitize_mermaid, validate_mermaid};
use std::time::Duration;

/// A simple flowchart with `n` nodes.
fn make_flowchart(n: usize) -> String {
    let mut body = String::from("flowchart TD\n");
    for i in 0..n {
        body.push_str(&format!("  N{i}[Node {i}]\n"));
    }
    for i in 0..n.saturating_sub(1) {
        body.push_str(&format!("  N{i} --> N{}\n", i + 1));
    }
    body
}

/// A sequence diagram with `n` participants and messages.
fn make_sequence(n: usize) -> String {
    let mut body = String::from("sequenceDiagram\n");
    for i in 0..n {
        body.push_str(&format!("  participant P{i} as Participant {i}\n"));
    }
    for i in 0..n.saturating_sub(1) {
        body.push_str(&format!("  P{i} ->> P{}: message {i}\n", i + 1));
        body.push_str(&format!("  P{} -->> P{i}: reply {i}\n", i + 1));
    }
    body
}

/// A markdown string with `n` mermaid blocks.
fn make_markdown_with_mermaid(n: usize) -> String {
    let mut md = String::new();
    for i in 0..n {
        md.push_str(&format!("# Chapter {i}\n\n"));
        md.push_str("```mermaid\n");
        md.push_str(&make_flowchart(5));
        md.push_str("```\n\n");
        md.push_str("Some text content here.\n\n");
    }
    md
}

/// A flowchart with dangerous characters that need sanitization.
fn make_dirty_flowchart(n: usize) -> String {
    let mut body = String::from("flowchart TD\n");
    for i in 0..n {
        body.push_str(&format!("  N{i}[\"Node {i} #bad; \\\"quoted\\\"\"]\n"));
    }
    for i in 0..n.saturating_sub(1) {
        body.push_str(&format!("  N{i} -->|\"calls #x\"| N{}\n", i + 1));
    }
    body
}

fn bench_mermaid_sanitization(c: &mut Criterion) {
    let mut group = c.benchmark_group("mermaid_sanitization");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Benchmark sanitize_mermaid with varying node counts.
    for n in [5usize, 20, 50] {
        let flowchart = make_flowchart(n);
        group.bench_with_input(BenchmarkId::new("sanitize_flowchart", n), &n, |b, &_| {
            b.iter(|| {
                let result = sanitize_mermaid(black_box(&flowchart));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark validate_mermaid with varying node counts.
    for n in [5usize, 20, 50] {
        let flowchart = make_flowchart(n);
        group.bench_with_input(BenchmarkId::new("validate_flowchart", n), &n, |b, &_| {
            b.iter(|| {
                let result = validate_mermaid(black_box(&flowchart));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark sanitize + validate combined (the real pipeline path).
    for n in [5usize, 20, 50] {
        let flowchart = make_flowchart(n);
        group.bench_with_input(BenchmarkId::new("sanitize_validate", n), &n, |b, &_| {
            b.iter(|| {
                let sanitized = sanitize_mermaid(black_box(&flowchart));
                let result = validate_mermaid(black_box(&sanitized));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark sequence diagram sanitization.
    for n in [3usize, 6, 10] {
        let seq = make_sequence(n);
        group.bench_with_input(BenchmarkId::new("sanitize_sequence", n), &n, |b, &_| {
            b.iter(|| {
                let result = sanitize_mermaid(black_box(&seq));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark dirty flowchart sanitization (characters to scrub).
    for n in [5usize, 20, 50] {
        let dirty = make_dirty_flowchart(n);
        group.bench_with_input(BenchmarkId::new("sanitize_dirty", n), &n, |b, &_| {
            b.iter(|| {
                let result = sanitize_mermaid(black_box(&dirty));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark sanitize_markdown_mermaid_blocks with multiple blocks.
    for n in [1usize, 5, 10] {
        let md = make_markdown_with_mermaid(n);
        group.bench_with_input(BenchmarkId::new("sanitize_md_blocks", n), &n, |b, &_| {
            b.iter(|| {
                let result = sanitize_markdown_mermaid_blocks(black_box(&md));
                let _ = black_box(result);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_mermaid_sanitization);
criterion_main!(benches);
