#![allow(missing_docs)]

//! Criterion benchmark: greedy batch file packing by size.
//!
//! Measures [`brigid_pipeline::batch_files_by_size`] with varying batch char
//! budgets and file counts. The packer caps each file's size at
//! `max_file_chars`, then greedily fills batches whose total capped size does
//! not exceed `batch_char_budget`.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_core::budget::BudgetConfig;
use brigid_pipeline::batch_files_by_size;
use std::time::Duration;

/// Build `n` synthetic file paths and parallel sizes.
fn make_files(n: usize, size: u64) -> (Vec<String>, Vec<u64>) {
    let files: Vec<String> = (0..n).map(|i| format!("src/file_{i}.rs")).collect();
    let sizes: Vec<u64> = vec![size; n];
    (files, sizes)
}

fn bench_batch_packing(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_packing");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Vary file count and batch char budget. Each file is 4 000 chars (well
    // under the default 12 000 cap), so packing behaviour depends on the
    // budget: small budgets yield many small batches, large budgets yield
    // few large batches.
    let file_size: u64 = 4_000;

    for n in [100usize, 1_000, 10_000] {
        for budget in [8_000usize, 40_000, 80_000] {
            let (files, sizes) = make_files(n, file_size);
            let config = BudgetConfig {
                batch_char_budget: budget,
                ..BudgetConfig::default()
            };
            let label = format!("{n}_files_{budget}_budget");
            group.bench_with_input(
                BenchmarkId::new("batch_files_by_size", label),
                &(files, sizes, config),
                |b, (files, sizes, config)| {
                    b.iter(|| {
                        let batches = batch_files_by_size(
                            black_box(files),
                            black_box(sizes),
                            black_box(config),
                        );
                        black_box(batches);
                    });
                },
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_batch_packing);
criterion_main!(benches);
