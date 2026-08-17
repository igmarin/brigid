#![allow(missing_docs)]
#![allow(deprecated)]

//! Criterion benchmark: `DiskCache` get/put operations.
//!
//! Measures [`brigid_llm::DiskCache::get`] for both cache hits and misses, and
//! [`brigid_llm::DiskCache::put`] with varying payload sizes. The cache uses
//! async `tokio::fs` I/O (M6-PERF-3), so each iteration drives a tokio runtime
//! via `block_on`.

use brigid_llm::{CacheKeyInput, DiskCache, cache_key};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::Runtime;

/// Create a unique temp cache root for this benchmark run.
fn temp_root() -> std::path::PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("brigid-bench-cache-{n}"))
}

/// Build a payload of approximately `bytes` length.
fn make_payload(bytes: usize) -> String {
    let unit = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
    let mut out = Vec::with_capacity(bytes);
    while out.len() + unit.len() <= bytes {
        out.extend_from_slice(unit);
    }
    // Pad the remainder with 'x' to reach the target size.
    while out.len() < bytes {
        out.push(b'x');
    }
    String::from_utf8(out).expect("payload is valid UTF-8")
}

/// Owned prompt strings used to back `CacheKeyInput` borrows.
struct PromptStore {
    hit_prompts: Vec<String>,
    miss_prompts: Vec<String>,
}

impl PromptStore {
    fn new(sizes: &[(usize, &str)]) -> Self {
        let hit_prompts = sizes
            .iter()
            .map(|(sz, _)| format!("hit-prompt-{sz}"))
            .collect();
        let miss_prompts = sizes
            .iter()
            .map(|(sz, _)| format!("miss-prompt-{sz}"))
            .collect();
        Self {
            hit_prompts,
            miss_prompts,
        }
    }
}

const MODEL: &str = "bench-model";
const PROVIDER: &str = "bench-provider";

fn bench_cache_get(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("cache_get");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let payload_sizes: &[(usize, &str)] = &[(1_024, "1KB"), (50_000, "50KB"), (500_000, "500KB")];

    let store = PromptStore::new(payload_sizes);

    // Pre-populate a cache with one entry per payload size for hit benchmarks.
    let root = temp_root();
    let cache = DiskCache::new(&root);

    // Compute hit keys (pre-populate the cache with these).
    let hit_keys: Vec<String> = store
        .hit_prompts
        .iter()
        .map(|p| {
            cache_key(&CacheKeyInput {
                prompt: p,
                model: MODEL,
                provider: PROVIDER,
                extras: None,
            })
            .expect("key")
        })
        .collect();

    {
        let _guard = rt.enter();
        for (key, (sz, _label)) in hit_keys.iter().zip(payload_sizes.iter()) {
            let body = make_payload(*sz);
            rt.block_on(cache.put(key, &body)).expect("populate");
        }
    }

    // Cache hit: lookup of a pre-populated entry.
    for (key, (_sz, label)) in hit_keys.iter().zip(payload_sizes.iter()) {
        group.bench_with_input(BenchmarkId::new("get_hit", label), key, |b, key| {
            b.iter(|| {
                let result = rt.block_on(cache.get(black_box(key))).expect("get");
                black_box(result);
            });
        });
    }

    // Cache miss: lookup of a key that was never stored.
    let miss_keys: Vec<String> = store
        .miss_prompts
        .iter()
        .map(|p| {
            cache_key(&CacheKeyInput {
                prompt: p,
                model: MODEL,
                provider: PROVIDER,
                extras: None,
            })
            .expect("key")
        })
        .collect();

    for (key, (_sz, label)) in miss_keys.iter().zip(payload_sizes.iter()) {
        group.bench_with_input(BenchmarkId::new("get_miss", label), key, |b, key| {
            b.iter(|| {
                let result = rt.block_on(cache.get(black_box(key))).expect("get");
                black_box(result);
            });
        });
    }

    group.finish();

    // Cleanup.
    let _ = std::fs::remove_dir_all(&root);
}

fn bench_cache_put(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("cache_put");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let payload_sizes: &[(usize, &str)] = &[(1_024, "1KB"), (50_000, "50KB"), (500_000, "500KB")];

    for (sz, label) in payload_sizes {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let payload = make_payload(*sz);

        // Use a distinct key per iteration via a counter suffix so each put
        // writes a new file (measuring real write cost, not in-place rewrite).
        let counter = std::sync::atomic::AtomicU64::new(0);

        group.bench_with_input(
            BenchmarkId::new("put", label),
            &(cache, payload, counter),
            |b, (cache, payload, counter)| {
                b.iter(|| {
                    let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let prompt = format!("put-prompt-{n}");
                    let input = CacheKeyInput {
                        prompt: &prompt,
                        model: MODEL,
                        provider: PROVIDER,
                        extras: None,
                    };
                    let key = cache_key(&input).expect("key");
                    rt.block_on(cache.put(black_box(&key), black_box(payload)))
                        .expect("put");
                });
            },
        );

        // Cleanup per-size root.
        let _ = std::fs::remove_dir_all(&root);
    }

    group.finish();
}

criterion_group!(benches, bench_cache_get, bench_cache_put);
criterion_main!(benches);
