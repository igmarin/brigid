#![allow(missing_docs)]
#![cfg(feature = "live-llm")]

//! Live full-pipeline smoke test for `brigid generate` on a real monorepo.
//!
//! Only compiled when the `live-llm` feature is enabled AND only executed when
//! an API key is present in the environment. The test is `#[ignore]`d so it
//! does not run with regular `cargo test` — invoke it explicitly:
//!
//! ```sh
//! cargo test --workspace --features brigid-pipeline/live-llm \
//!   --test live_generate_smoke -- --ignored --nocapture
//! ```
//!
//! The test invokes the `brigid` CLI binary via [`assert_cmd`] to run the full
//! `generate` pipeline on the `umbrella` monorepo fixture, then runs `brigid
//! eval` and validates the checkpoint. It is budget-capped via
//! `BRIGID_MAX_LLM_CALLS` (default `20`) and uses a disk cache under
//! `target/brigid-llm-cache` so re-runs with unchanged prompts are free.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use assert_cmd::Command;
use brigid_core::StageId;
use brigid_pipeline::CheckpointStore;

/// Maximum LLM calls the full generate pipeline may make. Overridable via
/// `BRIGID_MAX_LLM_CALLS`; defaults to a conservative `20`.
fn max_llm_calls() -> u32 {
    env::var("BRIGID_MAX_LLM_CALLS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(20)
}

/// True when a non-empty API key is available in the environment.
fn has_api_key() -> bool {
    let key_ok = |var: &str| env::var(var).ok().filter(|s| !s.is_empty()).is_some();
    key_ok("BRIGID_LLM_API_KEY") || key_ok("DEEPSEEK_API_KEY")
}

/// Absolute path to the `umbrella` monorepo fixture.
fn umbrella_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/umbrella")
}

/// Absolute path to the disk cache directory under the workspace `target/`.
fn cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/brigid-llm-cache")
}

/// EXIT_LLM from the CLI — transient provider errors. The test tolerates
/// these by skipping instead of failing the suite.
const EXIT_LLM: i32 = 4;

/// Full-pipeline live smoke test: runs `brigid generate` on the umbrella
/// monorepo, then `brigid eval`, and validates the checkpoint.
#[test]
#[ignore]
fn live_full_generate_smoke() {
    if !has_api_key() {
        eprintln!("skipped: no API key");
        return;
    }

    let fixture = umbrella_fixture();
    let cache = cache_dir();
    let budget = max_llm_calls();
    let output_dir = PathBuf::from("/tmp/brigid-smoke-output");
    let checkpoint_dir = PathBuf::from("/tmp/brigid-smoke-checkpoint");

    let _ = fs::remove_dir_all(&output_dir);
    let _ = fs::remove_dir_all(&checkpoint_dir);

    eprintln!(
        "fixture: {} | budget: {budget} | cache: {}",
        fixture.display(),
        cache.display()
    );

    let generate_output = Command::cargo_bin("brigid")
        .expect("brigid binary found")
        .arg("generate")
        .arg("--dir")
        .arg(&fixture)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--checkpoint-dir")
        .arg(&checkpoint_dir)
        .arg("--max-abstractions")
        .arg("5")
        .env("BRIGID_MAX_LLM_CALLS", budget.to_string())
        .env("BRIGID_LLM_CACHE_DIR", &cache)
        .timeout(Duration::from_secs(300))
        .output()
        .expect("generate command runs");

    eprintln!(
        "generate stdout: {}",
        String::from_utf8_lossy(&generate_output.stdout)
    );
    eprintln!(
        "generate stderr: {}",
        String::from_utf8_lossy(&generate_output.stderr)
    );

    let code = generate_output.status.code().unwrap_or(-1);
    if code == EXIT_LLM {
        eprintln!("skipped: generate failed with transient LLM error (exit {code})");
        return;
    }
    assert!(
        generate_output.status.success(),
        "generate should exit 0, got exit {code}"
    );

    let index_path = output_dir.join("index.md");
    assert!(index_path.is_file(), "index.md should exist in output dir");
    let index_content = fs::read_to_string(&index_path).expect("read index.md");
    assert!(
        index_content.contains("```mermaid"),
        "index.md should contain mermaid blocks"
    );

    let chapter_count = fs::read_dir(&output_dir)
        .expect("read output dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("chapter_") && s.ends_with(".md")
        })
        .count();
    assert!(
        chapter_count >= 2,
        "expected at least 2 chapter files, found {chapter_count}"
    );

    let eval_output = Command::cargo_bin("brigid")
        .expect("brigid binary found")
        .arg("eval")
        .arg("--out")
        .arg(&output_dir)
        .arg("--threshold")
        .arg("60")
        .timeout(Duration::from_secs(30))
        .output()
        .expect("eval command runs");
    eprintln!(
        "eval stdout: {}",
        String::from_utf8_lossy(&eval_output.stdout)
    );
    eprintln!(
        "eval stderr: {}",
        String::from_utf8_lossy(&eval_output.stderr)
    );
    assert!(
        eval_output.status.success(),
        "eval should pass with threshold 60, got exit {}",
        eval_output.status.code().unwrap_or(-1)
    );

    let store = CheckpointStore::new(&checkpoint_dir);
    let (cp, _files) = store
        .load()
        .expect("checkpoint should load (valid manifest + integrity)");
    for stage in [
        StageId::Identify,
        StageId::Relationships,
        StageId::Order,
        StageId::Chapters,
        StageId::Combine,
    ] {
        assert!(
            cp.is_stage_complete(stage),
            "stage {:?} should be marked complete",
            stage
        );
    }

    let all_output = fs::read_dir(&output_dir)
        .expect("read output dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !all_output.contains("sk-"),
        "output should not contain unredacted API key prefix 'sk-'"
    );
    assert!(
        !all_output.contains("Bearer "),
        "output should not contain 'Bearer ' token"
    );

    eprintln!("live_full_generate_smoke: PASS ({chapter_count} chapters, budget={budget})");
}
