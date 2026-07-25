#![allow(missing_docs)]

//! Criterion benchmark: checkpoint save/load round-trip.
//!
//! Measures the full ADR 0001 persistence path: gzip-encode file bundle,
//! atomic write, SHA-256 manifest, JSON metadata, then load + verify + decode.
//! Uses `tempfile` for isolation and fixed inputs for determinism.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use decon_core::{Chapter, ChapterResult, CheckpointV1, RunConfig, StageId, Tier};
use decon_pipeline::checkpoint_store::{CheckpointStore, records_from_files};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a unique temp directory for this benchmark iteration.
fn temp_dir() -> std::path::PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("decon-bench-cp-{n}-{seq}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a fresh checkpoint with prerequisite stages marked complete.
fn fresh_checkpoint() -> CheckpointV1 {
    let cfg = RunConfig::default();
    CheckpointV1::new(
        &cfg,
        cfg.redacted_for_checkpoint(),
        "rev-bench-abc",
        "2026-07-24T00:00:00Z",
    )
    .expect("checkpoint")
}

/// Build `n` chapters with fixed content.
fn make_chapters(n: usize) -> ChapterResult {
    let chapters: Vec<Chapter> = (0..n)
        .map(|i| {
            Chapter::new(
                i,
                i + 1,
                format!("Concept {i}"),
                format!(
                    "# Concept {i}\n\nThis is chapter {}.\n\n```mermaid\nflowchart TD\n  A[Node] --> B[Node]\n```\n",
                    i + 1
                ),
                if i % 3 == 0 { Tier::L } else if i % 3 == 1 { Tier::M } else { Tier::S },
                "module",
                format!(
                    "tier: {} | kind: module",
                    if i % 3 == 0 { "L" } else if i % 3 == 1 { "M" } else { "S" }
                ),
            )
        })
        .collect();
    ChapterResult::new(chapters)
}

/// Build fixed file bundle records for the checkpoint.
fn make_file_records(n: usize) -> Vec<decon_core::FileBundleRecord> {
    let raw_contents: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("//! file {i}\nfn f_{i}() -> i32 {{ {i} }}\n").into_bytes())
        .collect();
    let paths: Vec<String> = (0..n).map(|i| format!("src/file_{i}.rs")).collect();
    let entries: Vec<(&str, &[u8])> = paths
        .iter()
        .zip(raw_contents.iter())
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();
    records_from_files(&entries)
}

fn bench_checkpoint_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("checkpoint_roundtrip");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // Benchmark save + load round-trip with varying file/chapter counts.
    for n in [2usize, 10] {
        group.bench_with_input(BenchmarkId::new("save_load", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let dir = temp_dir();
                    let store = CheckpointStore::new(&dir);
                    let mut cp = fresh_checkpoint();
                    cp.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
                    cp.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");
                    cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
                    cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:04:00Z");
                    cp.mark_stage_complete(StageId::Order, "2026-07-24T00:05:00Z");
                    let files = make_file_records(n);
                    (store, cp, files)
                },
                |(store, cp, files)| {
                    let result = store.save(black_box(cp.clone()), black_box(&files));
                    let _ = black_box(result);

                    let loaded = store.load();
                    let _ = black_box(loaded);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    // Benchmark chapter write + read round-trip.
    for n in [2usize, 10] {
        group.bench_with_input(BenchmarkId::new("chapters_write_read", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let dir = temp_dir();
                    let store = CheckpointStore::new(&dir);
                    let mut cp = fresh_checkpoint();
                    cp.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
                    cp.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");
                    cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
                    cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:04:00Z");
                    cp.mark_stage_complete(StageId::Order, "2026-07-24T00:05:00Z");
                    let files = make_file_records(n);
                    store.save(cp.clone(), &files).unwrap();
                    let chapters = make_chapters(n);
                    (store, cp, chapters)
                },
                |(store, cp, chapters)| {
                    let entries = store.write_chapters(&store.dir, black_box(&chapters));
                    let _ = black_box(&entries);

                    let read = store.read_chapters(&store.dir, black_box(&cp));
                    let _ = black_box(&read);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_checkpoint_roundtrip);
criterion_main!(benches);
