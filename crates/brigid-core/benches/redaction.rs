#![allow(missing_docs)]

//! Criterion benchmark: content redaction.
//!
//! Measures [`brigid_core::redact_content`] with small (1 KB), medium (50 KB),
//! and large (500 KB) inputs. The redactor scans each line for secret-shaped
//! `KEY=value` pairs and `Bearer` tokens, replacing them with a placeholder.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use brigid_core::redact_content;
use std::time::Duration;

/// Build a synthetic input of approximately `bytes` length.
///
/// The input mixes normal source lines with secret-bearing lines so the
/// redactor's per-line scan and replacement logic is exercised realistically.
fn make_input(bytes: usize) -> String {
    let normal = "fn do_something() -> i32 { 42 }\n";
    let secret = "API_KEY=super-secret-value-1234567890\n";
    let bearer = "Authorization: Bearer abcdef1234567890abcdef1234567890\n";

    let mut out = String::with_capacity(bytes);
    let mut i = 0usize;
    while out.len() < bytes {
        out.push_str(match i % 3 {
            0 => normal,
            1 => secret,
            _ => bearer,
        });
        i += 1;
    }
    // Truncate to the target size (keep it a valid string ending on a newline).
    if out.len() > bytes {
        out.truncate(bytes);
    }
    out
}

fn bench_redaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("redact_content");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for (size, label) in [(1_024usize, "1KB"), (50_000, "50KB"), (500_000, "500KB")] {
        let input = make_input(size);
        group.bench_with_input(
            BenchmarkId::new("redact_content", label),
            &input,
            |b, input| {
                b.iter(|| {
                    let result = redact_content(black_box(input));
                    black_box(result);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_redaction);
criterion_main!(benches);
