use brigid_core::{Abstraction, IdentifyResult, ProgressTracker, extract_yaml_block};
use crate::llm::{LlmClient, bounded_complete_with_budget};
use serde_json::json;

use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};

use super::types::IdentifyError;

/// Input to the single-shot identify stage.
#[derive(Clone, Debug)]
pub struct IdentifySingleShotInput {
    /// Relative file paths from the crawl inventory.
    pub files: Vec<String>,
    /// Project name.
    pub project_name: String,
    /// Language instruction (e.g. `"Use Chinese"` or `""`).
    pub language_instruction: String,
    /// Additional lang note (e.g. `"Use 简体中文"` or `""`), mapped to the
    /// `name_lang_hint` and `desc_lang_hint` template variables.
    pub lang_note: String,
    /// Max number of abstractions to return.
    pub max_abstraction_num: usize,
}

/// Run the single-shot identify stage: one LLM call produces the full
/// abstraction list. Used for small repos where map/reduce is unnecessary.
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt render failures, LLM call failures,
/// YAML extraction/parse failures, out-of-range file indices, an empty
/// abstraction list, or a budget overrun.
pub async fn identify_single_shot(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &IdentifySingleShotInput,
    progress: &mut ProgressTracker,
) -> Result<IdentifyResult, IdentifyError> {
    // a. Build the render context. The `identify_single_shot.md.j2` template
    //    expects: project_name, context, language_instruction,
    //    max_abstraction_num, name_lang_hint, desc_lang_hint, file_listing.
    //    Free-text variables are sanitized so untrusted input cannot execute
    //    as Jinja template code (see `prompts/README.md` security note).
    let file_listing = format_file_listing(&input.files);
    let context = json!({
        "project_name": sanitize_template_input(&input.project_name),
        // The single-shot input carries only the file inventory; the joined
        // file *contents* are injected by the caller into a future variant of
        // this input. For now `context` is empty so the template renders.
        "context": "",
        "language_instruction": sanitize_template_input(&input.language_instruction),
        "max_abstraction_num": input.max_abstraction_num,
        "name_lang_hint": sanitize_template_input(&input.lang_note),
        "desc_lang_hint": sanitize_template_input(&input.lang_note),
        "file_listing": file_listing,
    });

    // b. Render the prompt.
    let prompt = renderer.render(PromptId::IdentifySingleShot, &context)?;

    // c. Call the LLM with budget enforcement.
    progress.set_stage("identify");
    let mut results = bounded_complete_with_budget(client, vec![prompt], 1, progress).await?;
    let response = results
        .pop()
        .ok_or(IdentifyError::EmptyOutput)??;

    // f. Extract the YAML block from the (possibly prose-wrapped) response.
    let yaml_text = extract_yaml_block(&response)?;

    // g. Parse the extracted YAML into a list of abstractions.
    let abstractions: Vec<Abstraction> = serde_yaml_ng::from_str(&yaml_text)?;

    // h. Validate file_indices against the crawl inventory.
    let total = input.files.len();
    for abs in &abstractions {
        for &idx in &abs.file_indices {
            if idx >= total {
                return Err(IdentifyError::FileIndexOutOfRange { index: idx, total });
            }
        }
    }

    // i. Empty result is an error, not a silent success.
    if abstractions.is_empty() {
        return Err(IdentifyError::NoAbstractions);
    }

    // j. Done.
    Ok(IdentifyResult::new(abstractions))
}

/// Format the crawl inventory as the `file_listing` the template expects:
/// `idx # path` per line, matching the `idx # path/comment` format documented
/// in `prompts/identify_single_shot.md.j2`.
fn format_file_listing(files: &[String]) -> String {
    files
        .iter()
        .enumerate()
        .map(|(idx, path)| format!("{idx} # {path}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::{AbstractionKind, ProgressTracker, Tier};
    use crate::llm::{LlmClient, LlmError, MockClient};

    /// Two-file inventory used across happy-path tests.
    fn sample_files() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/config.rs".to_string(),
        ]
    }

    /// A canned LLM response wrapping a YAML list of two abstractions in a
    /// fenced block, plus some surrounding prose (as a real LLM would emit).
    fn canned_two_abstractions() -> String {
        let yaml = "\
- name: \"Core Module\"
  description: \"The main module\"
  file_indices: [0, 1, 2]
  tier: \"S\"
  kind: \"module\"
  apps: [\"app1\"]
  entry_files: [\"src/main.rs\"]
- name: \"Utils\"
  description: \"Utility functions\"
  file_indices: [3]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("Here are the abstractions:\n\n```yaml\n{yaml}```\n")
    }

    fn sample_input() -> IdentifySingleShotInput {
        IdentifySingleShotInput {
            files: sample_files(),
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
        }
    }

    #[tokio::test]
    async fn happy_path_returns_two_abstractions() {
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("happy path should succeed");
        assert_eq!(result.abstractions.len(), 2);
        assert_eq!(result.abstractions[0].name, "Core Module");
        assert_eq!(result.abstractions[0].tier, Tier::S);
        assert_eq!(result.abstractions[0].kind, AbstractionKind::new("module"));
        assert_eq!(result.abstractions[0].file_indices, vec![0, 1, 2]);
        assert_eq!(result.abstractions[1].name, "Utils");
        assert_eq!(result.abstractions[1].file_indices, vec![3]);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn empty_abstraction_list_returns_no_abstractions() {
        let yaml = "```yaml\n[]\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("empty list should error");
        assert!(matches!(err, IdentifyError::NoAbstractions), "got: {err:?}");
    }

    #[tokio::test]
    async fn file_index_out_of_range_returns_error() {
        // file_indices: [0, 99] — 99 is out of range for a 4-file inventory.
        let yaml = "\
```yaml
- name: \"Bad\"
  description: \"oob\"
  file_indices: [0, 99]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("oob index should error");
        match err {
            IdentifyError::FileIndexOutOfRange { index, total } => {
                assert_eq!(index, 99);
                assert_eq!(total, 4);
            }
            other => panic!("expected FileIndexOutOfRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_yaml_returns_parse_error() {
        let yaml = "```yaml\n- name: \"Broken\n  description: : :\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("malformed yaml should error");
        assert!(matches!(err, IdentifyError::Parse(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn missing_tier_and_kind_use_defaults() {
        // LLMs may omit optional fields; we should default tier to M and kind
        // to empty so plugins can enrich the kind later.
        let yaml = "\
```yaml
- name: \"Core Module\"
  description: \"The main module\"
  file_indices: [0, 1]
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("partial yaml should succeed with defaults");
        assert_eq!(result.abstractions.len(), 1);
        assert_eq!(result.abstractions[0].name, "Core Module");
        assert_eq!(result.abstractions[0].tier, Tier::M);
        assert_eq!(result.abstractions[0].kind.as_str(), "");
        assert!(result.abstractions[0].apps.is_empty());
        assert!(result.abstractions[0].entry_files.is_empty());
    }

    #[tokio::test]
    async fn malformed_yaml_preserves_typed_serde_yaml_error() {
        let yaml = "```yaml\n- name: \"Broken\n  description: : :\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("malformed yaml should error");
        // The Parse variant must hold a typed serde_yaml_ng::Error, not a String,
        // so callers can inspect the error kind and line number. This fails to
        // compile if Parse holds String instead of serde_yaml_ng::Error.
        match err {
            IdentifyError::Parse(inner) => {
                let _: serde_yaml_ng::Error = inner;
                assert!(!inner.to_string().is_empty());
            }
            other => panic!("expected IdentifyError::Parse(serde_yaml_ng::Error), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_yaml_block_returns_extract_error() {
        let client = MockClient::new("just prose, no structured output here".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("no block should error");
        assert!(matches!(err, IdentifyError::Extract(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new("ignored").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let err = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("llm failure should propagate");
        assert!(
            matches!(err, IdentifyError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn progress_tracker_records_the_call() {
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let mut progress = ProgressTracker::new(10);
        let result = identify_single_shot(&client, &renderer, &input, &mut progress)
            .await
            .expect("should succeed with progress");
        assert_eq!(result.abstractions.len(), 2);
        let snap = progress.snapshot();
        assert_eq!(snap.llm_calls_used, 1);
        assert_eq!(snap.llm_calls_remaining, 9);
    }

    #[tokio::test]
    async fn progress_tracker_budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_two_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        // max=0 means no calls allowed.
        let mut progress = ProgressTracker::new(0);
        let err = identify_single_shot(&client, &renderer, &input, &mut progress)
            .await
            .expect_err("budget exceeded should error");
        assert!(matches!(err, IdentifyError::Budget(_)), "got: {err:?}");
        // The LLM must NOT have been called when the budget is exhausted up
        // front.
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn rendered_prompt_contains_expected_variables() {
        // Use a capturing client that records the prompt it received.
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl llm_kernel::llm::LLMClient for CapturingClient {
            async fn complete(&self, request: llm_kernel::llm::LLMRequest) -> llm_kernel::error::Result<llm_kernel::llm::LLMResponse> {
                    let prompt = crate::llm::request_prompt(&request);
                    let result: Result<String, crate::llm::LlmError> = async {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_two_abstractions())
                    }.await;
                    match result {
                        Ok(s) => Ok(crate::llm::text_response(s)),
                        Err(e) => Err(e.into_kernel()),
                    }
            }
            fn model_name(&self) -> &str {
                "mock"
            }
            async fn stream_complete(
                    &self,
                    _request: llm_kernel::llm::LLMRequest,
                ) -> llm_kernel::error::Result<llm_kernel::llm::LLMStream> {
                    crate::llm::stream_unsupported()
            }
        }

        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let input = IdentifySingleShotInput {
            files: sample_files(),
            project_name: "AcmeCorp/{{ evil }}".to_string(),
            language_instruction: "Use Spanish".to_string(),
            lang_note: "Use 简体中文".to_string(),
            max_abstraction_num: 7,
        };
        let _ = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        // Project name present and sanitized (no raw `{{`).
        assert!(prompt.contains("AcmeCorp"), "prompt: {prompt}");
        assert!(
            !prompt.contains("{{ evil }}"),
            "prompt not sanitized: {prompt}"
        );
        // Language instruction present.
        assert!(prompt.contains("Use Spanish"), "prompt: {prompt}");
        // max_abstraction_num present.
        assert!(prompt.contains("top 5-7"), "prompt: {prompt}");
        // File listing present with indexed paths.
        assert!(prompt.contains("0 # src/main.rs"), "prompt: {prompt}");
        assert!(prompt.contains("3 # src/config.rs"), "prompt: {prompt}");
        // lang_note propagated to the name/desc hints.
        assert!(prompt.contains("简体中文"), "prompt: {prompt}");
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_two_abstractions()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&*client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("dyn client should work");
        assert_eq!(result.abstractions.len(), 2);
    }

    #[tokio::test]
    async fn single_abstraction_round_trips() {
        let yaml = "\
```yaml
- name: \"Solo\"
  description: \"only one\"
  file_indices: [0]
  tier: \"L\"
  kind: \"class\"
  apps: [\"web\", \"api\"]
  entry_files: [\"src/main.rs\", \"src/lib.rs\"]
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_input();
        let result = identify_single_shot(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("single abstraction should succeed");
        assert_eq!(result.abstractions.len(), 1);
        let a = &result.abstractions[0];
        assert_eq!(a.name, "Solo");
        assert_eq!(a.tier, Tier::L);
        assert_eq!(a.apps, vec!["web", "api"]);
        assert_eq!(a.entry_files, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn identify_error_display_is_sensible() {
        let e = IdentifyError::NoAbstractions;
        assert_eq!(e.to_string(), "no abstractions found in LLM output");
        let yaml_err = serde_yaml_ng::from_str::<Vec<Abstraction>>("bad: : :\n").unwrap_err();
        let e = IdentifyError::Parse(yaml_err);
        assert!(
            e.to_string()
                .contains("failed to parse abstractions from LLM output")
        );
        let e = IdentifyError::FileIndexOutOfRange { index: 5, total: 3 };
        assert!(e.to_string().contains("5"));
        assert!(e.to_string().contains("3"));
    }
}
