//! WriteArchitectureOverview pipeline stage: generate a chapter-0 architecture
//! overview for multi-app monorepos.
//!
//! This stage runs only when the module inventory contains more than one
//! app/module ([`should_generate_overview`]). It renders the
//! `write_architecture_overview.md.j2` prompt, calls the LLM, sanitizes Mermaid
//! blocks in the output, validates that named apps match the crawled inventory,
//! and returns an [`ArchitectureOverview`].
//!
//! Checkpoint integration is provided by [`overview_and_checkpoint`], which
//! writes `00_architecture_overview.md` to the checkpoint directory and marks
//! [`StageId::Overview`] complete. Resume skips the stage if already complete.

use std::time::{SystemTime, UNIX_EPOCH};

use decon_core::{
    Abstraction, ArchitectureOverview, CheckpointV1, ModuleKey, Relationship, StageId,
    redact_content, sanitize_markdown_mermaid_blocks,
};
use decon_llm::LlmClient;
use serde_json::json;
use thiserror::Error;

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::prompts::{PromptError, PromptId, PromptRenderer, sanitize_template_input};
use crate::resume;

/// Errors returned by the overview stage.
#[derive(Debug, Error)]
pub enum OverviewError {
    /// The prompt template failed to render (missing/invalid variable).
    #[error("prompt rendering failed: {0}")]
    Prompt(#[from] PromptError),
    /// The LLM call failed (network, timeout, rate limit, provider error).
    #[error("LLM call failed: {0}")]
    Llm(#[from] decon_llm::LlmError),
    /// The LLM returned empty output.
    #[error("LLM returned empty output")]
    EmptyOutput,
    /// The LLM invented app names not present in the crawled inventory.
    #[error("app validation failed: LLM invented apps not in inventory: {invented:?}")]
    AppValidation {
        /// App names found in the output that are not in the inventory.
        invented: Vec<String>,
    },
    /// A checkpoint save/load failed during the overview stage.
    #[error("checkpoint error during overview: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
}

/// Input to the overview stage.
#[derive(Clone, Debug)]
pub struct OverviewInput {
    /// Project name (free-text, sanitized before rendering).
    pub project_name: String,
    /// Project summary from the relationships stage
    /// ([`decon_core::RelationshipsResult::project_summary`]).
    pub summary: String,
    /// Module inventory (module keys from the crawl).
    pub inventory: Vec<ModuleKey>,
    /// Core abstractions from the identify stage.
    pub abstractions: Vec<Abstraction>,
    /// Relationships from the relationships stage.
    pub relationships: Vec<Relationship>,
    /// Language note (e.g. `"Use Spanish"` or `""`), mapped to the `lang_note`
    /// template variable.
    pub lang_note: String,
    /// When `true`, invented app names cause [`OverviewError::AppValidation`].
    /// When `false`, invented apps are accepted silently (lenient mode).
    pub strict_app_validation: bool,
}

/// Decide whether to generate an architecture overview.
///
/// Returns `true` when the module inventory contains more than one
/// app/module — the overview is only useful for multi-app monorepos.
#[must_use]
pub fn should_generate_overview(modules: &[ModuleKey]) -> bool {
    modules.len() > 1
}

/// Run the overview stage: render the prompt, call the LLM, sanitize Mermaid,
/// validate app names, and return an [`ArchitectureOverview`].
///
/// # Errors
///
/// Returns [`OverviewError`] for prompt render failures, LLM call failures,
/// empty output, or app-validation failures (when `strict_app_validation` is
/// `true`).
pub async fn write_architecture_overview(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &OverviewInput,
) -> Result<ArchitectureOverview, OverviewError> {
    let summary = redact_content(&input.summary);
    let inventory_text = redact_content(&format_inventory(&input.inventory));
    let abstractions_text = redact_content(&format_abstractions(&input.abstractions));
    let relationships_text = redact_content(&format_relationships(&input.relationships));

    let context = json!({
        "lang_note": sanitize_template_input(&input.lang_note),
        "project_name": sanitize_template_input(&input.project_name),
        "summary": summary,
        "inventory": inventory_text,
        "abstractions": abstractions_text,
        "relationships": relationships_text,
    });

    let prompt = renderer.render(PromptId::WriteArchitectureOverview, &context)?;
    let response = client.complete(&prompt).await?;

    if response.trim().is_empty() {
        return Err(OverviewError::EmptyOutput);
    }

    let sanitized = sanitize_markdown_mermaid_blocks(&response);

    if input.strict_app_validation {
        let invented = validate_app_names(&sanitized, &input.inventory);
        if !invented.is_empty() {
            return Err(OverviewError::AppValidation { invented });
        }
    }

    let app_inventory: Vec<String> = input
        .inventory
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    Ok(ArchitectureOverview::new(sanitized, app_inventory))
}

/// Run the overview stage with checkpoint integration.
///
/// 1. Check [`resume::should_run`] for [`StageId::Overview`] — if `false`,
///    load and return the existing overview from the checkpoint.
/// 2. Call [`write_architecture_overview`].
/// 3. Write `00_architecture_overview.md` to the checkpoint directory via
///    [`CheckpointStore::write_architecture_overview`].
/// 4. Record the stage output and mark [`StageId::Overview`] complete.
///
/// # Errors
///
/// Returns [`OverviewError`] for LLM/prompt/validation failures or checkpoint
/// persistence failures.
pub async fn overview_and_checkpoint(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    input: &OverviewInput,
) -> Result<ArchitectureOverview, OverviewError> {
    if !resume::should_run(StageId::Overview, checkpoint) {
        if let Some(existing) = store
            .read_architecture_overview(&store.dir, checkpoint)
            .map_err(OverviewError::from)?
        {
            return Ok(existing);
        }
    }

    let overview = write_architecture_overview(client, renderer, input).await?;

    let entry = store.write_architecture_overview(&store.dir, &overview)?;
    store.record_stage_outputs(checkpoint, StageId::Overview, vec![entry])?;

    let (mut loaded, files) = store.load()?;
    loaded.mark_stage_complete(StageId::Overview, now_iso8601_utc());
    store.save(loaded.clone(), &files)?;

    *checkpoint = loaded;

    Ok(overview)
}

/// Extract `apps/<name>` tokens from markdown text.
///
/// Scans for the literal prefix `apps/` and collects the following path
/// component (alphanumeric, `-`, `_`). Returns deduplicated names in
/// first-occurrence order.
///
/// Module keys produced by [`decon_core::module_key`] do not contain dots,
/// so `.` is excluded from the allowed character set to avoid false positives
/// from trailing punctuation (e.g. `apps/worker.` at end of sentence).
fn extract_app_names(markdown: &str) -> Vec<String> {
    let prefix = "apps/";
    let mut seen: Vec<String> = Vec::new();
    let mut rest = markdown;
    while let Some(idx) = rest.find(prefix) {
        let after = &rest[idx + prefix.len()..];
        let name_end = after
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if name_end > 0 {
            let name = &after[..name_end];
            let full = format!("apps/{name}");
            if !seen.contains(&full) {
                seen.push(full);
            }
        }
        rest = &after[name_end.max(1)..];
    }
    seen
}

/// Validate that app names mentioned in the output exist in the inventory.
/// Returns a list of invented app names (empty if all are valid).
fn validate_app_names(markdown: &str, inventory: &[ModuleKey]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = inventory.iter().map(|k| k.as_str()).collect();
    extract_app_names(markdown)
        .into_iter()
        .filter(|name| !known.contains(name.as_str()))
        .collect()
}

/// Format the module inventory as a human-readable string for the prompt.
fn format_inventory(inventory: &[ModuleKey]) -> String {
    inventory
        .iter()
        .map(|k| format!("- {}", k.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format abstractions as a human-readable string for the prompt.
fn format_abstractions(abstractions: &[Abstraction]) -> String {
    abstractions
        .iter()
        .map(|a| format!("- {} ({}): {}", a.name, a.kind.as_str(), a.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format relationships as a human-readable string for the prompt.
fn format_relationships(relationships: &[Relationship]) -> String {
    relationships
        .iter()
        .map(|r| format!("- #{} {} #{}: {}", r.from, r.kind, r.to, r.label))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Generate an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) from
/// [`SystemTime::now`].
fn now_iso8601_utc() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert days since the Unix epoch (1970-01-01) to a proleptic Gregorian
/// `(year, month, day)` tuple (Howard Hinnant `civil_from_days` algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_store::records_from_files;
    use decon_core::{StageId, Tier};
    use decon_llm::{LlmClient, LlmError, MockClient};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-overview-ckpt-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fresh_checkpoint() -> CheckpointV1 {
        let cfg = decon_core::RunConfig::default();
        CheckpointV1::new(
            &cfg,
            cfg.redacted_for_checkpoint(),
            "rev-abc",
            "2026-07-24T00:00:00Z",
        )
        .unwrap()
    }

    fn seed_store(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
        cp.mark_stage_complete(StageId::DryRun, "2026-07-24T00:02:00Z");
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
        cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:04:00Z");
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn three_app_inventory() -> Vec<ModuleKey> {
        vec![
            ModuleKey::new("apps/api"),
            ModuleKey::new("apps/web"),
            ModuleKey::new("apps/worker"),
        ]
    }

    fn sample_abstractions() -> Vec<Abstraction> {
        vec![
            Abstraction::new("API Core", "REST handlers", Tier::M, "module"),
            Abstraction::new("Web UI", "Frontend app", Tier::M, "module"),
        ]
    }

    fn sample_relationships() -> Vec<Relationship> {
        vec![Relationship::new(0, 1, "calls", "calls")]
    }

    fn sample_input(inventory: Vec<ModuleKey>) -> OverviewInput {
        OverviewInput {
            project_name: "my-monorepo".to_string(),
            summary: "A multi-app monorepo with API, web, and worker.".to_string(),
            inventory,
            abstractions: sample_abstractions(),
            relationships: sample_relationships(),
            lang_note: String::new(),
            strict_app_validation: true,
        }
    }

    fn canned_overview_markdown() -> String {
        "\
# Architecture overview

## What kind of system this is
A multi-app monorepo.

## How the monorepo is carved up
- apps/api: REST API
- apps/web: Frontend
- apps/worker: Background jobs

## How apps collaborate
apps/web calls apps/api which dispatches to apps/worker

## Suggested reading order for newcomers
1. apps/api
2. apps/web
3. apps/worker

## Mental model diagram
```mermaid
flowchart LR
  A0[apps/api] --> A1[apps/web]
  A0 --> A2[apps/worker]
```
"
        .to_string()
    }

    fn canned_overview_with_fake_app() -> String {
        "\
# Architecture overview

## How the monorepo is carved up
- apps/api: REST API
- apps/fake: Invented service

```mermaid
flowchart LR
  A0[apps/api] --> A1[apps/fake]
```
"
        .to_string()
    }

    // --- should_generate_overview ---

    #[test]
    fn should_generate_overview_multi_app_returns_true() {
        let modules = three_app_inventory();
        assert!(should_generate_overview(&modules));
    }

    #[test]
    fn should_generate_overview_single_app_returns_false() {
        let modules = vec![ModuleKey::new("apps/api")];
        assert!(!should_generate_overview(&modules));
    }

    #[test]
    fn should_generate_overview_empty_returns_false() {
        let modules: Vec<ModuleKey> = vec![];
        assert!(!should_generate_overview(&modules));
    }

    // --- happy path ---

    #[tokio::test]
    async fn happy_path_multi_app_generates_overview() {
        let client = MockClient::new(canned_overview_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let result = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect("happy path should succeed");
        assert!(!result.markdown.is_empty());
        assert!(result.markdown.contains("# Architecture overview"));
        assert!(result.markdown.contains("apps/api"));
        assert_eq!(result.app_inventory.len(), 3);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn happy_path_works_as_dyn_llm_client() {
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_overview_markdown()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let result = write_architecture_overview(&*client, &renderer, &input)
            .await
            .expect("dyn client should work");
        assert!(!result.markdown.is_empty());
    }

    // --- app name validation ---

    #[tokio::test]
    async fn app_validation_known_app_ok() {
        let client = MockClient::new(canned_overview_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let result = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect("known apps should pass validation");
        assert!(result.markdown.contains("apps/api"));
    }

    #[tokio::test]
    async fn app_validation_invented_app_returns_error_strict() {
        let client = MockClient::new(canned_overview_with_fake_app());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let err = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect_err("invented app should error in strict mode");
        match err {
            OverviewError::AppValidation { invented } => {
                assert!(invented.contains(&"apps/fake".to_string()));
            }
            other => panic!("expected AppValidation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn app_validation_invented_app_lenient_mode_succeeds() {
        let client = MockClient::new(canned_overview_with_fake_app());
        let renderer = PromptRenderer::new().unwrap();
        let mut input = sample_input(three_app_inventory());
        input.strict_app_validation = false;
        let result = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect("lenient mode should not error on invented apps");
        assert!(result.markdown.contains("apps/fake"));
    }

    // --- mermaid sanitization ---

    #[tokio::test]
    async fn mermaid_sanitization_applied_to_output() {
        let raw = "\
# Architecture overview

```mermaid
flowchart LR
  A0[apps/api] --> A1[\"bad; #x\"]
```
";
        let client = MockClient::new(raw.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let result = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect("should succeed");
        let mermaid_block = result
            .markdown
            .split("```mermaid")
            .nth(1)
            .expect("mermaid block should exist");
        assert!(
            !mermaid_block.contains('"'),
            "double-quote not sanitized in mermaid block: {mermaid_block}"
        );
        assert!(
            !mermaid_block.contains('#'),
            "hash not sanitized in mermaid block: {mermaid_block}"
        );
    }

    // --- secrets redaction ---

    #[tokio::test]
    async fn secrets_redacted_before_rendering() {
        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_overview_markdown())
            }
        }

        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let mut input = sample_input(three_app_inventory());
        input.summary = "API_KEY=super-secret-value\nA multi-app monorepo.".to_string();
        let _ = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        assert!(
            !prompt.contains("super-secret-value"),
            "secret leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("****"),
            "redaction placeholder missing: {prompt}"
        );
    }

    // --- malformed / empty LLM output ---

    #[tokio::test]
    async fn empty_llm_output_returns_error() {
        let client = MockClient::new("");
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let err = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect_err("empty output should error");
        assert!(matches!(err, OverviewError::EmptyOutput), "got: {err:?}");
    }

    #[tokio::test]
    async fn whitespace_only_llm_output_returns_error() {
        let client = MockClient::new("   \n\n  \n");
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let err = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect_err("whitespace-only output should error");
        assert!(matches!(err, OverviewError::EmptyOutput), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new("ignored").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());
        let err = write_architecture_overview(&client, &renderer, &input)
            .await
            .expect_err("llm failure should propagate");
        assert!(
            matches!(err, OverviewError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
        assert_eq!(client.call_count(), 1);
    }

    // --- checkpoint integration ---

    #[tokio::test]
    async fn overview_and_checkpoint_writes_file_and_marks_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new(canned_overview_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());

        let result = overview_and_checkpoint(&client, &renderer, &store, &mut cp, &input)
            .await
            .expect("checkpoint run should succeed");

        assert!(!result.markdown.is_empty());
        assert!(cp.is_stage_complete(StageId::Overview));
        assert!(cp.stage_timestamps.contains_key("overview"));

        let overview_file = dir.join("00_architecture_overview.md");
        assert!(overview_file.is_file(), "overview file should exist");
        let on_disk = fs::read_to_string(&overview_file).unwrap();
        assert!(on_disk.contains("# Architecture overview"));

        let (loaded, _) = store.load().unwrap();
        assert!(loaded.is_stage_complete(StageId::Overview));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn overview_and_checkpoint_resume_skips_when_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let client = MockClient::new(canned_overview_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input(three_app_inventory());

        let first = overview_and_checkpoint(&client, &renderer, &store, &mut cp, &input)
            .await
            .expect("first run should succeed");
        assert_eq!(client.call_count(), 1);

        let second = overview_and_checkpoint(&client, &renderer, &store, &mut cp, &input)
            .await
            .expect("resume should succeed");
        assert_eq!(second.markdown, first.markdown);
        assert_eq!(client.call_count(), 1, "LLM should not be called on resume");

        let _ = fs::remove_dir_all(&dir);
    }

    // --- pure helper tests ---

    #[test]
    fn extract_app_names_finds_apps_prefix() {
        let md = "Mentions apps/api and apps/web but not src.";
        let names = extract_app_names(md);
        assert!(names.contains(&"apps/api".to_string()));
        assert!(names.contains(&"apps/web".to_string()));
    }

    #[test]
    fn extract_app_names_no_apps_prefix_returns_empty() {
        let md = "Just a regular single-app project.";
        let names = extract_app_names(md);
        assert!(names.is_empty());
    }

    #[test]
    fn validate_app_names_all_known_returns_empty() {
        let md = "apps/api and apps/web are mentioned.";
        let inventory = vec![ModuleKey::new("apps/api"), ModuleKey::new("apps/web")];
        let invented = validate_app_names(md, &inventory);
        assert!(invented.is_empty());
    }

    #[test]
    fn validate_app_names_invented_returns_them() {
        let md = "apps/api and apps/fake are mentioned.";
        let inventory = vec![ModuleKey::new("apps/api")];
        let invented = validate_app_names(md, &inventory);
        assert!(invented.contains(&"apps/fake".to_string()));
        assert!(!invented.contains(&"apps/api".to_string()));
    }

    #[test]
    fn overview_error_display_is_sensible() {
        let e = OverviewError::EmptyOutput;
        assert!(e.to_string().contains("empty"));
        let e = OverviewError::AppValidation {
            invented: vec!["apps/fake".into()],
        };
        assert!(e.to_string().contains("apps/fake"));
    }
}
