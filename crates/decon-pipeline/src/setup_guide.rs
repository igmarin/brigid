//! WriteSetupGuide pipeline stage: generate a setup/onboarding chapter when
//! repo docs are weak (or when forced via flag).
//!
//! This module wires the [`SetupGuide`] domain type to the LLM pipeline:
//!
//! - [`should_generate_setup`] is a pure predicate that decides whether the
//!   stage runs based on the setup assessment score, gaps, and CLI flags.
//! - [`write_setup_guide`] renders `write_setup_guide.md.j2`, redacts secrets
//!   from the context, calls the LLM, sanitizes mermaid blocks in the output,
//!   and returns a [`SetupGuide`].
//! - [`write_setup_guide_and_checkpoint`] wraps [`write_setup_guide`] with
//!   checkpoint persistence and resume semantics.

use std::time::{SystemTime, UNIX_EPOCH};

use decon_core::{
    CheckpointV1, SetupGuide, StageId, redact_content, sanitize_markdown_mermaid_blocks,
};
use decon_llm::LlmClient;
use serde_json::json;

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::prompts::{PromptError, PromptId, PromptRenderer, sanitize_template_input};

/// Score below which a setup guide is always generated (when not forced and
/// not suppressed).
pub const SETUP_SCORE_THRESHOLD: i32 = 50;

/// Minimum number of detected gaps that triggers generation regardless of
/// score (when not forced and not suppressed).
pub const SETUP_GAP_THRESHOLD: usize = 3;

/// Errors returned by the WriteSetupGuide stage.
#[derive(Debug, thiserror::Error)]
pub enum SetupGuideError {
    /// The prompt template failed to render (missing/invalid variable).
    #[error("prompt rendering failed: {0}")]
    Prompt(#[from] PromptError),
    /// The LLM call failed (network, timeout, rate limit, provider error).
    #[error("LLM call failed: {0}")]
    Llm(#[from] decon_llm::LlmError),
    /// The LLM returned empty output.
    #[error("LLM returned empty setup guide output")]
    EmptyOutput,
    /// A checkpoint save/load failed during the setup stage.
    #[error("checkpoint error during setup: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
}

/// Decide whether the setup guide stage should run.
///
/// Logic:
/// - `no_setup_flag` -> `false` (stage explicitly suppressed)
/// - `force_flag` -> `true` (generate regardless of score/gaps)
/// - `score < SETUP_SCORE_THRESHOLD` -> `true`
/// - `gaps.len() >= SETUP_GAP_THRESHOLD` -> `true`
/// - otherwise -> `false`
#[must_use]
pub fn should_generate_setup(
    setup_score: i32,
    gaps: &[String],
    force_flag: bool,
    no_setup_flag: bool,
) -> bool {
    if no_setup_flag {
        return false;
    }
    if force_flag {
        return true;
    }
    setup_score < SETUP_SCORE_THRESHOLD || gaps.len() >= SETUP_GAP_THRESHOLD
}

/// Input to the [`write_setup_guide`] function.
#[derive(Clone, Debug)]
pub struct WriteSetupGuideInput<'a> {
    /// Project name.
    pub project_name: &'a str,
    /// Setup assessment score (0-100).
    pub score: i32,
    /// Detected setup doc gaps.
    pub gaps: &'a [String],
    /// Context text (README + config file fragments) to feed the LLM.
    pub context: &'a str,
    /// Target language for the output (e.g. `"English"`, `"Spanish"`).
    pub lang: &'a str,
    /// Whether generation was forced via flag (recorded on the result).
    pub forced: bool,
}

/// Generate a setup guide chapter via the LLM.
///
/// Renders `write_setup_guide.md.j2`, redacts secrets from the context before
/// rendering, calls the LLM, sanitizes mermaid blocks in the output, and
/// returns a [`SetupGuide`].
///
/// # Errors
///
/// Returns [`SetupGuideError`] for prompt render failures, LLM call failures,
/// empty LLM output, or checkpoint persistence failures.
pub async fn write_setup_guide(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &WriteSetupGuideInput<'_>,
) -> Result<SetupGuide, SetupGuideError> {
    let redacted_context = redact_content(input.context);
    let gaps_text = input
        .gaps
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let context = json!({
        "project_name": sanitize_template_input(input.project_name),
        "score": input.score,
        "gaps": gaps_text,
        "context": redacted_context,
        "lang": sanitize_template_input(input.lang),
    });

    let prompt = renderer.render(PromptId::WriteSetupGuide, &context)?;
    let response = client.complete(&prompt).await?;

    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err(SetupGuideError::EmptyOutput);
    }

    let sanitized = sanitize_markdown_mermaid_blocks(trimmed);

    Ok(SetupGuide::new(
        sanitized,
        input.score,
        input.gaps.to_vec(),
        input.forced,
    ))
}

/// Run the WriteSetupGuide stage with checkpoint persistence and resume.
///
/// If [`StageId::Setup`] is already complete (and its output file is present
/// and intact), the stage is skipped and the existing guide is returned.
/// Otherwise [`write_setup_guide`] is called, the result is written to
/// `00_setup.md` via [`CheckpointStore::write_setup_guide`], the stage output
/// is recorded, and [`StageId::Setup`] is marked complete.
///
/// # Errors
///
/// Returns [`SetupGuideError`] for generation or checkpoint failures.
pub async fn write_setup_guide_and_checkpoint(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    input: &WriteSetupGuideInput<'_>,
) -> Result<SetupGuide, SetupGuideError> {
    if store.is_stage_complete_with_files(checkpoint, StageId::Setup)? {
        if let Some(existing) = store.read_setup_guide(&store.dir, checkpoint)? {
            return Ok(existing);
        }
    }

    let guide = write_setup_guide(client, renderer, input).await?;

    let entry = store.write_setup_guide(&store.dir, &guide)?;
    checkpoint.mark_stage_complete(StageId::Setup, now_iso8601_utc());
    store.record_stage_outputs(checkpoint, StageId::Setup, vec![entry])?;

    Ok(guide)
}

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
    use decon_core::{RunConfig, StageId};
    use decon_llm::{LlmClient, LlmError, MockClient};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-setup-guide-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fresh_checkpoint() -> CheckpointV1 {
        let cfg = RunConfig::default();
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
        let files = records_from_files(&[("README.md", b"# Project"), ("Makefile", b"all:")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn canned_markdown() -> String {
        "# Setup: my-project\n\n## Prerequisites\n\nRust 1.85.\n\n## Install dependencies\n\n```bash\ncargo build\n```\n\n## Run locally\n\n```bash\ncargo run\n```\n".to_string()
    }

    fn canned_markdown_with_mermaid() -> String {
        "# Setup: my-project\n\n```mermaid\nflowchart LR\n  A0[Prereq \"bad\"; #x] --> A1[Install]\n```\n".to_string()
    }

    fn sample_gaps() -> Vec<String> {
        vec!["No install commands".to_string(), "No env docs".to_string()]
    }

    fn sample_input<'a>(context: &'a str, gaps: &'a [String]) -> WriteSetupGuideInput<'a> {
        WriteSetupGuideInput {
            project_name: "my-project",
            score: 30,
            gaps,
            context,
            lang: "English",
            forced: false,
        }
    }

    #[test]
    fn should_generate_low_score_returns_true() {
        assert!(should_generate_setup(30, &[], false, false));
    }

    #[test]
    fn should_generate_high_score_no_force_returns_false() {
        assert!(!should_generate_setup(80, &[], false, false));
    }

    #[test]
    fn should_generate_high_score_with_force_returns_true() {
        assert!(should_generate_setup(80, &[], true, false));
    }

    #[test]
    fn should_generate_no_setup_flag_returns_false() {
        assert!(!should_generate_setup(
            10,
            &["a".into(), "b".into(), "c".into()],
            true,
            true
        ));
        assert!(!should_generate_setup(10, &[], false, true));
        assert!(!should_generate_setup(10, &[], true, true));
    }

    #[test]
    fn should_generate_gaps_threshold_returns_true() {
        let gaps = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(should_generate_setup(60, &gaps, false, false));
    }

    #[test]
    fn should_generate_medium_score_two_gaps_returns_false() {
        let gaps = vec!["a".to_string(), "b".to_string()];
        assert!(!should_generate_setup(60, &gaps, false, false));
    }

    #[test]
    fn should_generate_force_overrides_no_setup_false() {
        assert!(should_generate_setup(80, &[], true, false));
    }

    #[tokio::test]
    async fn happy_path_low_score_generates_guide() {
        let gaps = sample_gaps();
        let client = MockClient::new(canned_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README content here", &gaps);
        let guide = write_setup_guide(&client, &renderer, &input)
            .await
            .expect("happy path should succeed");
        assert!(guide.markdown.contains("# Setup: my-project"));
        assert_eq!(guide.score, 30);
        assert_eq!(guide.gaps.len(), 2);
        assert!(!guide.forced);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn mermaid_sanitized_in_output() {
        let gaps = sample_gaps();
        let client = MockClient::new(canned_markdown_with_mermaid());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README", &gaps);
        let guide = write_setup_guide(&client, &renderer, &input)
            .await
            .expect("should succeed");
        assert!(!guide.markdown.contains('"'));
        assert!(guide.markdown.contains("flowchart LR"));
    }

    #[tokio::test]
    async fn secrets_redacted_before_rendering() {
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_markdown())
            }
        }

        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let context = "DB_KEY=super-secret-value\nname=ok\n";
        let input = WriteSetupGuideInput {
            project_name: "my-project",
            score: 30,
            gaps: &[],
            context,
            lang: "English",
            forced: false,
        };
        let _ = write_setup_guide(&client, &renderer, &input)
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        assert!(!prompt.contains("super-secret-value"));
        assert!(prompt.contains("DB_KEY=****"));
        assert!(prompt.contains("name=ok"));
    }

    #[tokio::test]
    async fn empty_llm_output_returns_typed_error() {
        let gaps = sample_gaps();
        let client = MockClient::new("   \n  \n");
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README", &gaps);
        let err = write_setup_guide(&client, &renderer, &input)
            .await
            .expect_err("empty output should error");
        assert!(matches!(err, SetupGuideError::EmptyOutput), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let gaps = sample_gaps();
        let client = MockClient::new("ignored").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README", &gaps);
        let err = write_setup_guide(&client, &renderer, &input)
            .await
            .expect_err("llm failure should propagate");
        assert!(
            matches!(err, SetupGuideError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn checkpoint_persists_and_marks_complete() {
        let gaps = sample_gaps();
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new(canned_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README content", &gaps);
        let guide = write_setup_guide_and_checkpoint(&client, &renderer, &store, &mut cp, &input)
            .await
            .expect("should succeed");
        assert!(guide.markdown.contains("# Setup: my-project"));
        assert!(cp.is_stage_complete(StageId::Setup));
        assert!(dir.join("00_setup.md").is_file());
        let (loaded, _) = store.load().unwrap();
        assert!(loaded.is_stage_complete(StageId::Setup));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_skips_when_stage_complete() {
        let gaps = sample_gaps();
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let client = MockClient::new(canned_markdown());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README content", &gaps);
        let first = write_setup_guide_and_checkpoint(&client, &renderer, &store, &mut cp, &input)
            .await
            .expect("first run should succeed");
        assert_eq!(client.call_count(), 1);

        let second_client = MockClient::new("SHOULD NOT BE CALLED");
        let second =
            write_setup_guide_and_checkpoint(&second_client, &renderer, &store, &mut cp, &input)
                .await
                .expect("resume should succeed");
        assert_eq!(second_client.call_count(), 0);
        assert_eq!(second.markdown, first.markdown);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let gaps = sample_gaps();
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_markdown()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input("README", &gaps);
        let guide = write_setup_guide(&*client, &renderer, &input)
            .await
            .expect("dyn client should work");
        assert!(guide.markdown.contains("# Setup: my-project"));
    }

    #[test]
    fn setup_guide_error_display_is_sensible() {
        let e = SetupGuideError::EmptyOutput;
        assert_eq!(e.to_string(), "LLM returned empty setup guide output");
    }
}
