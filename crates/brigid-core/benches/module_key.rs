#![allow(missing_docs)]

//! Criterion benchmark: module key computation.
//!
//! Measures [`brigid_core::module_key`] with varying path depths. The key
//! derivation splits a relative POSIX path on `/` and applies umbrella
//! (`apps/<name>/…`) and root-level rules to produce a coarse module key.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_core::module_key;
use std::time::Duration;

/// Build a relative POSIX path with `depth` components.
///
/// The final component is a filename; intermediate components are directories.
fn make_path(depth: usize) -> String {
    let mut parts: Vec<String> = (0..depth.saturating_sub(1))
        .map(|i| format!("dir{i}"))
        .collect();
    parts.push("file.rs".to_owned());
    parts.join("/")
}

fn bench_module_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("module_key");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for depth in [1usize, 5, 10, 50] {
        let path = make_path(depth);
        group.bench_with_input(
            BenchmarkId::new("module_key", format!("depth_{depth}")),
            &path,
            |b, path| {
                b.iter(|| {
                    let key = module_key(black_box(path));
                    black_box(key);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_module_key);
criterion_main!(benches);
