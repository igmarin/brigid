#![allow(missing_docs)]

//! Criterion benchmark: YAML block extraction from LLM output.
//!
//! Measures [`brigid_core::extract_yaml_block`] with various response sizes.
//! The extractor locates a ```yaml fenced block (or bare fence / bare YAML
//! heuristic), strips wrapping prose, and dedents the content.

use brigid_core::extract_yaml_block;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::time::Duration;

/// Build a synthetic LLM response containing a ```yaml fenced block whose
/// total size is approximately `bytes`.
fn make_response(bytes: usize) -> String {
    // Leading prose before the fence.
    let header = "Here is the structured analysis of the repository:\n\n```yaml\n";
    // Trailing prose after the fence.
    let footer = "\n```\n\nLet me know if you need more details.\n";

    // Each YAML line is ~40 chars; generate enough to fill the target.
    let yaml_line = "  - name: component_%name%\n    kind: module\n    tier: L\n";
    let mut yaml_body = String::new();
    let mut idx = 0usize;
    let target_body = bytes.saturating_sub(header.len() + footer.len());
    while yaml_body.len() < target_body {
        let line = yaml_line.replace("%name%", &format!("{idx:04}"));
        yaml_body.push_str(&line);
        idx += 1;
    }

    format!("{header}{yaml_body}{footer}")
}

fn bench_yaml_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("extract_yaml_block");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for (size, label) in [(2_000usize, "2KB"), (20_000, "20KB"), (200_000, "200KB")] {
        let response = make_response(size);
        group.bench_with_input(
            BenchmarkId::new("extract_yaml_block", label),
            &response,
            |b, response| {
                b.iter(|| {
                    let result = extract_yaml_block(black_box(response)).expect("extract");
                    black_box(result);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_yaml_extraction);
criterion_main!(benches);
