#![allow(missing_docs)]

//! Criterion benchmark: budget estimation.
//!
//! Measures `estimate_budget` — the pure function that groups files by module,
//! applies per-file caps, computes path stubs for overflow, packs modules into
//! batches, and estimates token counts.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_core::budget::{BudgetConfig, FileSize, estimate_budget, truncate_content};
use brigid_core::module::ModuleKey;
use std::time::Duration;

/// Build `n` files spread across `m` modules with `chars_per_file` each.
fn make_files(n: usize, m: usize, chars_per_file: usize) -> Vec<FileSize> {
    let module_names: Vec<String> = (0..m)
        .map(|i| {
            if i == 0 {
                "_root".to_string()
            } else {
                format!("apps/app_{i}")
            }
        })
        .collect();
    (0..n)
        .map(|i| {
            let module = &module_names[i % m];
            FileSize {
                path: format!("{module}/file_{i}.rs"),
                chars: chars_per_file,
                module: ModuleKey::new(module),
            }
        })
        .collect()
}

fn bench_budget_estimation(c: &mut Criterion) {
    let mut group = c.benchmark_group("budget_estimation");
    group.sample_size(30);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Vary the number of files with default config.
    for n in [10usize, 50, 100, 200] {
        let files = make_files(n, 3, 500);
        group.bench_with_input(BenchmarkId::new("files", n), &n, |b, &_| {
            b.iter(|| {
                let est = estimate_budget(black_box(&files), black_box(&BudgetConfig::default()));
                let _ = black_box(est);
            });
        });
    }

    // Vary the number of modules.
    for m in [1usize, 5, 10] {
        let files = make_files(50, m, 500);
        group.bench_with_input(BenchmarkId::new("modules", m), &m, |b, &_| {
            b.iter(|| {
                let est = estimate_budget(black_box(&files), black_box(&BudgetConfig::default()));
                let _ = black_box(est);
            });
        });
    }

    // Vary file size (truncation path).
    for chars in [500usize, 5_000, 15_000] {
        let files = make_files(50, 3, chars);
        group.bench_with_input(BenchmarkId::new("file_size", chars), &chars, |b, &_| {
            b.iter(|| {
                let est = estimate_budget(black_box(&files), black_box(&BudgetConfig::default()));
                let _ = black_box(est);
            });
        });
    }

    // Benchmark truncate_content directly.
    for chars in [100usize, 1_000, 15_000] {
        let content = "x".repeat(chars);
        group.bench_with_input(BenchmarkId::new("truncate", chars), &chars, |b, &_| {
            b.iter(|| {
                let result = truncate_content(black_box(&content), black_box(12_000));
                let _ = black_box(result);
            });
        });
    }

    // Benchmark with a large file inventory (stress test).
    for n in [500usize, 1_000] {
        let files = make_files(n, 5, 300);
        group.bench_with_input(BenchmarkId::new("large_inventory", n), &n, |b, &_| {
            b.iter(|| {
                let est = estimate_budget(black_box(&files), black_box(&BudgetConfig::default()));
                let _ = black_box(est);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_budget_estimation);
criterion_main!(benches);
