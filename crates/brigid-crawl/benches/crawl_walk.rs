#![allow(missing_docs)]

//! Criterion benchmark: local filesystem crawl walk.
//!
//! Measures [`brigid_crawl::crawl_local`] over temp-generated fixture trees
//! with 100, 1 000, and 10 000 files of a fixed size. The walker performs an
//! iterative directory traversal, stats every file, and returns a sorted
//! inventory of relative POSIX paths with parallel byte sizes.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_crawl::crawl_local;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

/// Fixed content written into every fixture file (256 bytes).
const FILE_CONTENT: &[u8] = b"\
//! A fixed-size fixture file for crawl benchmarks.\n\
//! It contains enough bytes to exercise the stat path without\n\
//! being so large that disk I/O dominates the walk. Padding:\n\
//! 0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF\n\
//! 0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF\n\
//! 0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF\n\
//! 0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF\n";

/// Generate a temp directory tree containing `n` files distributed across
/// nested subdirectories (100 files per directory to avoid huge single dirs).
fn generate_fixture(n: usize) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let files_per_dir = 100usize;
    let mut created = 0usize;
    let mut dir_idx = 0usize;
    while created < n {
        let subdir = root.join(format!("d{dir_idx:04}"));
        fs::create_dir_all(&subdir).expect("create subdir");
        for i in 0..files_per_dir {
            if created >= n {
                break;
            }
            let path = subdir.join(format!("file_{i:04}.txt"));
            let mut f = File::create(&path).expect("create file");
            f.write_all(FILE_CONTENT).expect("write file");
            created += 1;
        }
        dir_idx += 1;
    }
    assert_eq!(created, n, "fixture generation mismatch");
    dir
}

/// Verify the crawl found the expected number of files (sanity check only).
fn assert_file_count(root: &Path, expected: usize) {
    let result = crawl_local(root).expect("crawl");
    assert_eq!(
        result.file_count(),
        expected,
        "crawl should find exactly {expected} files"
    );
}

fn bench_crawl_walk(c: &mut Criterion) {
    let mut group = c.benchmark_group("crawl_walk");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for n in [100usize, 1_000, 10_000] {
        // Pre-generate the fixture once per input size; the benchmark only
        // measures the walk, not the tree generation.
        let dir = generate_fixture(n);
        // Sanity check before timing.
        assert_file_count(dir.path(), n);

        group.bench_with_input(BenchmarkId::new("crawl_local", n), &n, |b, &_n| {
            b.iter(|| {
                let result = crawl_local(black_box(dir.path())).expect("crawl");
                black_box(result);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_crawl_walk);
criterion_main!(benches);
