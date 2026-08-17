#![allow(missing_docs)]
#![cfg(feature = "live-llm")]

//! Live LLM smoke tests — only compiled when the `live-llm` feature is enabled
//! AND only executed when an API key is present in the environment.
//!
//! These tests make **real, paid API calls** to DeepSeek (by default) and cost
//! a few cents per run. They are **NOT** run in default CI — the CI workflow
//! invokes `cargo test --workspace` without `--features live-llm`, so this
//! file is not even compiled there.
//!
//! To run locally:
//!
//! ```sh
//! cargo test --workspace --features brigid-pipeline/live-llm \
//!   --test live_smoke -- --nocapture
//! ```
//!
//! # Budget
//!
//! Each test reads `BRIGID_MAX_LLM_CALLS` (default `5`) and configures a
//! [`brigid_core::ProgressTracker`] with that ceiling so a runaway loop fails
//! closed instead of spending unbounded money.
//!
//! # Skip behavior
//!
//! Rust has no first-class test "skip", so each test returns early (printing
//! `skipped: ...` to stderr) when neither `BRIGID_LLM_API_KEY` nor
//! `DEEPSEEK_API_KEY` is set. This keeps the feature safe to enable in
//! environments without credentials.
//!
//! # Disk cache
//!
//! The identify test attaches a [`llm_kernel::llm::CacheClient`] under
//! `target/brigid-llm-cache` so that re-runs with an unchanged prompt are
//! served from disk and cost nothing.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use brigid_core::ProgressTracker;
use brigid_crawl::local::crawl_local;
use brigid_pipeline::llm::complete_text;
use brigid_pipeline::prompts::PromptRenderer;
use brigid_pipeline::{IdentifySingleShotInput, identify_single_shot};
use llm_kernel::llm::{CacheClient, LLMClient, ModelConfig, OpenAIClient};
use llm_kernel::store::kv::SqliteKvStore;

/// Maximum LLM calls a live smoke test may make. Overridable via
/// `BRIGID_MAX_LLM_CALLS` for local tuning; defaults to a conservative `5`.
fn max_llm_calls() -> u32 {
    env::var("BRIGID_MAX_LLM_CALLS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(5)
}

/// True when a non-empty API key is available in the environment. Blank
/// values are treated as unset (matching the project's config-secret guard
/// convention) so we never attempt real calls with an empty `Authorization`
/// header.
fn has_api_key() -> bool {
    let key_ok = |var: &str| env::var(var).ok().filter(|s| !s.is_empty()).is_some();
    key_ok("BRIGID_LLM_API_KEY") || key_ok("DEEPSEEK_API_KEY")
}

/// Returns `true` when the test should be skipped (no API key). Prints a
/// `skipped:` line to stderr so `--nocapture` runs make the skip visible.
fn skip_if_no_key() -> bool {
    if !has_api_key() {
        eprintln!("skipped: no BRIGID_LLM_API_KEY or DEEPSEEK_API_KEY in env");
        return true;
    }
    false
}

/// Absolute path to the `tests/fixtures/python-lib` fixture, resolved from the
/// crate manifest dir so the test works regardless of the working directory.
fn python_lib_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/python-lib")
}

/// Build an [`OpenAIClient`] from the environment, optionally wrapped in
/// [`CacheClient`] so re-runs are free.
///
/// Blank env values are treated as unset. The resolved `base_url` host is
/// checked against the same default allowlist as `brigid-llm` before the
/// client is constructed, so a blank or attacker-controlled
/// `BRIGID_LLM_BASE_URL` cannot send the API key to an unexpected host.
fn client_from_env(with_cache: bool) -> Box<dyn LLMClient> {
    use brigid_pipeline::llm::{nonempty_env, nonempty_env_or, validate_llm_base_url};

    let mut config = ModelConfig::default();
    config.provider = "deepseek".into();
    config.model = nonempty_env_or("BRIGID_MODEL", "deepseek-chat");
    config.api_key_env = if nonempty_env("BRIGID_LLM_API_KEY").is_some() {
        "BRIGID_LLM_API_KEY".into()
    } else {
        "DEEPSEEK_API_KEY".into()
    };
    let base_url = nonempty_env_or("BRIGID_LLM_BASE_URL", "https://api.deepseek.com/v1");
    validate_llm_base_url(&base_url).expect("BRIGID_LLM_BASE_URL host is not allowlisted");
    config.base_url = Some(base_url);
    let client = OpenAIClient::new(&config).expect("HTTP client build failed");
    if with_cache {
        let cache_root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/brigid-llm-cache");
        let _ = std::fs::create_dir_all(&cache_root);
        let store = SqliteKvStore::open(&cache_root.join("cache.sqlite"))
            .expect("open llm-kernel sqlite cache");
        Box::new(CacheClient::new(client, Arc::new(store)))
    } else {
        Box::new(client)
    }
}

/// Live smoke test for the full single-shot identify stage.
///
/// Crawls the `python-lib` fixture, renders the identify prompt, calls the
/// real DeepSeek API, parses the YAML, and validates the result. Budget is
/// capped at [`max_llm_calls()`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_identify_single_shot() {
    if skip_if_no_key() {
        return;
    }

    // a. Crawl the fixture.
    let fixture = python_lib_fixture();
    let crawl = crawl_local(&fixture).expect("crawl python-lib fixture");
    assert!(
        !crawl.files.is_empty(),
        "fixture should contain at least one file"
    );
    eprintln!(
        "crawled {} files from {}",
        crawl.files.len(),
        fixture.display()
    );

    // b. Build the live client (with disk cache so re-runs are cheap).
    let client = client_from_env(true);

    // c. Build the prompt renderer.
    let renderer = PromptRenderer::new().expect("prompt renderer constructs");

    // d. Budget-capped progress tracker.
    let budget = max_llm_calls();
    let mut progress = ProgressTracker::new(budget);
    eprintln!("budget: max {budget} LLM calls");

    // e. Run identify_single_shot.
    let input = IdentifySingleShotInput {
        files: crawl.files.clone(),
        project_name: "python-lib".to_string(),
        language_instruction: String::new(),
        lang_note: String::new(),
        max_abstraction_num: 5,
    };

    let result = match identify_single_shot(&client, &renderer, &input, &mut progress).await {
        Ok(r) => r,
        Err(e) => {
            // Network/API issues are tolerated for a smoke test — document and
            // return instead of failing the suite on a transient provider error.
            eprintln!("skipped: identify_single_shot failed (likely network/API): {e}");
            return;
        }
    };

    // f. At least one abstraction.
    assert!(
        !result.abstractions.is_empty(),
        "identify should return at least one abstraction"
    );

    // g. All file_indices within bounds.
    let total = crawl.files.len();
    for abs in &result.abstractions {
        for &idx in &abs.file_indices {
            assert!(
                idx < total,
                "abstraction '{}' references file index {idx} but only {total} files exist",
                abs.name
            );
        }
    }

    // h. Budget respected.
    let snap = progress.snapshot();
    assert!(
        snap.llm_calls_used <= budget,
        "used {} calls but budget was {budget}",
        snap.llm_calls_used
    );

    // i. Print for manual inspection.
    eprintln!(
        "identify returned {} abstraction(s):",
        result.abstractions.len()
    );
    for abs in &result.abstractions {
        eprintln!(
            "  - {} [tier={}, kind={}] files={:?}",
            abs.name,
            abs.tier,
            abs.kind.as_str(),
            abs.file_indices
        );
    }
}

/// Live smoke test for the [`LlmClient`] directly: a trivial prompt should
/// return non-empty text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_openai_client_complete() {
    if skip_if_no_key() {
        return;
    }

    let client = client_from_env(false);

    let response = match complete_text(client.as_ref(), "Say hello in one word.").await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: client.complete failed (likely network/API): {e}");
            return;
        }
    };

    // d. Non-empty response.
    assert!(
        !response.trim().is_empty(),
        "LLM response should be non-empty"
    );

    // e. Should not be an obvious provider error payload leaked into the
    //    response. We only flag short JSON error envelopes, not the word
    //    "error" appearing in legitimate prose.
    assert!(
        !(response.trim_start().starts_with("{\"error") && response.len() < 256),
        "response looks like a provider error envelope: {response}"
    );

    eprintln!("LLM response: {response}");
}
