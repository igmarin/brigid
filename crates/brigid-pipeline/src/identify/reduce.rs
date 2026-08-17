use crate::llm::{LlmClient, bounded_complete_with_budget};
use brigid_core::{Abstraction, IdentifyResult, ProgressTracker, extract_yaml_block};
use serde_json::json;

use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};

use super::types::{CandidateAbstraction, IdentifyError};

/// Input for the identify reduce stage.
#[derive(Clone, Debug)]
pub struct IdentifyReduceInput {
    /// Candidate abstractions from all map batches.
    pub candidates: Vec<CandidateAbstraction>,
    /// The original file inventory (for index validation).
    pub files: Vec<String>,
    /// Project name.
    pub project_name: String,
    /// Language instruction.
    pub language_instruction: String,
    /// Lang note.
    pub lang_note: String,
    /// Maximum number of abstractions in the final list.
    pub max_abstraction_num: usize,
    /// Module summary (optional text describing the module structure).
    pub module_summary: String,
    /// Optional multimodal concept context from a graph provider (ADR 0016).
    /// Empty string when no provider is configured (NoneProvider).
    pub multimodal_context: String,
}

/// Run the reduce stage of identify: merge and rank per-batch candidates
/// from the map stage into the final top-N [`Vec<Abstraction>`] via one LLM
/// call.
///
/// This produces the authoritative [`IdentifyResult`] that should be written
/// to the checkpoint.
///
/// # Algorithm
///
/// 1. **Serialize** the candidates into a YAML blob for the prompt.
/// 2. **Render** the `identify_reduce` prompt with the candidates blob,
///    module summary, and sanitized free-text variables.
/// 3. **Reserve** one LLM call against the budget (fail closed before
///    spending a network call).
/// 4. **Call** the LLM.
/// 5. **Extract** the YAML block from the (possibly prose-wrapped) response.
/// 6. **Parse** the extracted YAML into a [`Vec<Abstraction>`].
/// 7. **Cap** the result at `max_abstraction_num` (truncate, keeping the
///    top-ranked items as emitted by the LLM).
/// 8. **Validate** `file_indices` against the crawl inventory.
/// 9. **Fail closed** on an empty result.
///
/// # Errors
///
/// Returns [`IdentifyError`] for prompt render failures, LLM call failures,
/// YAML extraction/parse failures, out-of-range file indices, an empty
/// abstraction list, or a budget overrun.
pub async fn identify_reduce(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    input: &IdentifyReduceInput,
    progress: &mut ProgressTracker,
) -> Result<IdentifyResult, IdentifyError> {
    // a. Build the render context for identify_reduce.md.j2. The template
    //    expects: project_name, module_summary, language_instruction,
    //    max_abstraction_num, name_lang_hint, desc_lang_hint, candidates_blob.
    //    Free-text variables are sanitized so untrusted input cannot execute
    //    as Jinja template code (see `prompts/README.md` security note).
    //    The candidates_blob is also sanitized: candidate names/descriptions
    //    are derived from crawled file content (untrusted), and a value like
    //    `{{ 7 * 7 }}` would be evaluated as a Jinja expression when the
    //    outer template is rendered. `sanitize_template_input` breaks the
    //    `{{`/`}}` delimiters by inserting a space, which is safe for YAML
    //    (YAML does not use `{{`/`}}` as syntax) and invisible to an LLM.
    let candidates_blob = sanitize_template_input(&candidates_to_yaml(&input.candidates)?);
    let context = json!({
        "project_name": sanitize_template_input(&input.project_name),
        "module_summary": sanitize_template_input(&input.module_summary),
        "language_instruction": sanitize_template_input(&input.language_instruction),
        "max_abstraction_num": input.max_abstraction_num,
        "name_lang_hint": sanitize_template_input(&input.lang_note),
        "desc_lang_hint": sanitize_template_input(&input.lang_note),
        "candidates_blob": candidates_blob,
        // Multimodal concept context from a graph provider (ADR 0016). Empty
        // string when no provider is configured — the template conditional
        // skips it.
        "multimodal_context": sanitize_template_input(&input.multimodal_context),
    });

    // b. Render the prompt.
    let prompt = renderer.render(PromptId::IdentifyReduce, &context)?;

    // c. Call the LLM with budget enforcement. `bounded_complete_with_budget`
    //    reserves the call and sets the stage for observability.
    progress.set_stage("identify_reduce");
    let mut results = bounded_complete_with_budget(client, vec![prompt], 1, progress).await?;
    let response = results.pop().ok_or(IdentifyError::EmptyOutput)??;

    // d. Extract the YAML block from the (possibly prose-wrapped) response.
    let yaml_text = extract_yaml_block(&response)?;

    // f. Parse the extracted YAML into a list of abstractions.
    let mut abstractions: Vec<Abstraction> = serde_yaml_ng::from_str(&yaml_text)?;

    // g. Enforce max_abstraction_num cap: if the LLM returned more than the
    //    requested maximum, truncate to the top-N (the LLM is instructed to
    //    rank, so the leading items are the highest-priority). We truncate
    //    rather than error so a slightly-over-long response is still usable.
    if abstractions.len() > input.max_abstraction_num {
        abstractions.truncate(input.max_abstraction_num);
    }

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

/// Serialize candidates to a YAML string for inclusion in the reduce prompt.
///
/// Produces a clean YAML list of [`CandidateAbstraction`]s (including
/// `batch_idx` so the LLM can trace each candidate back to its originating
/// batch). An empty candidate list serializes to `[]`.
///
/// # Errors
///
/// Returns [`IdentifyError::Parse`] if `serde_yaml_ng` fails to serialize the
/// candidates. In practice [`CandidateAbstraction`] is a plain serializable
/// struct so this never fails, but the `Result` return avoids silently
/// swallowing a serialization error (which would send an empty blob to the
/// LLM and produce misleading results).
fn candidates_to_yaml(candidates: &[CandidateAbstraction]) -> Result<String, IdentifyError> {
    if candidates.is_empty() {
        return Ok("[]".to_string());
    }
    Ok(serde_yaml_ng::to_string(candidates)?)
}

// ===========================================================================
// Reduce stage tests
// ===========================================================================

#[cfg(test)]
mod reduce_tests {
    use super::*;
    use crate::llm::{LlmClient, LlmError, MockClient};
    use brigid_core::{AbstractionKind, ProgressTracker, Tier};

    /// Five-file inventory used across reduce-stage tests.
    fn sample_files() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "src/utils.rs".to_string(),
            "src/config.rs".to_string(),
            "src/api.rs".to_string(),
        ]
    }

    /// Build a single [`CandidateAbstraction`].
    fn cand(
        name: &str,
        desc: &str,
        indices: Vec<usize>,
        tier: Tier,
        kind: &str,
        batch_idx: usize,
    ) -> CandidateAbstraction {
        CandidateAbstraction {
            name: name.to_string(),
            description: desc.to_string(),
            file_indices: indices,
            tier,
            kind: AbstractionKind::new(kind),
            apps: Vec::new(),
            entry_files: Vec::new(),
            batch_idx,
        }
    }

    /// Three candidates from two batches (2 from batch 0, 1 from batch 1).
    fn sample_candidates() -> Vec<CandidateAbstraction> {
        vec![
            cand("Module A", "desc a", vec![0, 1], Tier::S, "module", 0),
            cand("Module B", "desc b", vec![2, 3], Tier::M, "utility", 0),
            cand("Module C", "desc c", vec![4], Tier::L, "service", 1),
        ]
    }

    /// A canned LLM response wrapping a YAML list of two final abstractions in
    /// a fenced block, plus some surrounding prose (as a real LLM would emit).
    fn canned_two_final_abstractions() -> String {
        let yaml = "\
- name: \"Core System\"
  description: \"The main system\"
  file_indices: [0, 1, 2, 3]
  tier: \"S\"
  kind: \"module\"
  apps: [\"app1\"]
  entry_files: [\"src/main.rs\"]
- name: \"Utils\"
  description: \"Utilities\"
  file_indices: [4]
  tier: \"M\"
  kind: \"utility\"
  apps: []
  entry_files: []
";
        format!("Here are the final abstractions:\n\n```yaml\n{yaml}```\n")
    }

    fn sample_reduce_input(candidates: Vec<CandidateAbstraction>) -> IdentifyReduceInput {
        IdentifyReduceInput {
            candidates,
            files: sample_files(),
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: 5,
            module_summary: "core, utils, api".to_string(),
            multimodal_context: String::new(),
        }
    }

    #[tokio::test]
    async fn happy_path_returns_two_final_abstractions() {
        let client = MockClient::new(canned_two_final_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let result = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("happy path should succeed");
        assert_eq!(result.abstractions.len(), 2);
        assert_eq!(result.abstractions[0].name, "Core System");
        assert_eq!(result.abstractions[0].tier, Tier::S);
        assert_eq!(result.abstractions[0].kind, AbstractionKind::new("module"));
        assert_eq!(result.abstractions[0].file_indices, vec![0, 1, 2, 3]);
        assert_eq!(result.abstractions[1].name, "Utils");
        assert_eq!(result.abstractions[1].file_indices, vec![4]);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn max_abstraction_cap_truncates_to_limit() {
        // LLM returns 5 abstractions but max_abstraction_num=3 -> truncated to 3.
        let yaml = "\
```yaml
- name: \"A\"
  description: \"a\"
  file_indices: [0]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"B\"
  description: \"b\"
  file_indices: [1]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"C\"
  description: \"c\"
  file_indices: [2]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"D\"
  description: \"d\"
  file_indices: [3]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
- name: \"E\"
  description: \"e\"
  file_indices: [4]
  tier: \"S\"
  kind: \"module\"
  apps: []
  entry_files: []
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let mut input = sample_reduce_input(sample_candidates());
        input.max_abstraction_num = 3;
        let result = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("cap should succeed with truncation");
        assert_eq!(result.abstractions.len(), 3);
        assert_eq!(result.abstractions[0].name, "A");
        assert_eq!(result.abstractions[2].name, "C");
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn empty_result_returns_no_abstractions() {
        let yaml = "```yaml\n[]\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("empty list should error");
        assert!(matches!(err, IdentifyError::NoAbstractions), "got: {err:?}");
    }

    #[tokio::test]
    async fn file_index_out_of_range_returns_error() {
        // file_indices: [0, 99] — 99 is out of range for a 5-file inventory.
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
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("oob index should error");
        match err {
            IdentifyError::FileIndexOutOfRange { index, total } => {
                assert_eq!(index, 99);
                assert_eq!(total, 5);
            }
            other => panic!("expected FileIndexOutOfRange, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_yaml_returns_parse_error() {
        let yaml = "```yaml\n- name: \"Broken\n  description: : :\n```\n";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("malformed yaml should error");
        assert!(matches!(err, IdentifyError::Parse(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn missing_tier_and_kind_use_defaults() {
        let yaml = "\
```yaml
- name: \"Core System\"
  description: \"The main system\"
  file_indices: [0, 1, 2, 3]
```
";
        let client = MockClient::new(yaml.to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let result = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("partial yaml should succeed with defaults");
        assert_eq!(result.abstractions.len(), 1);
        assert_eq!(result.abstractions[0].name, "Core System");
        assert_eq!(result.abstractions[0].tier, Tier::M);
        assert_eq!(result.abstractions[0].kind.as_str(), "");
        assert!(result.abstractions[0].apps.is_empty());
        assert!(result.abstractions[0].entry_files.is_empty());
    }

    #[tokio::test]
    async fn no_yaml_block_returns_extract_error() {
        let client = MockClient::new("just prose, no structured output here".to_string());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect_err("no block should error");
        assert!(matches!(err, IdentifyError::Extract(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let client = MockClient::new("ignored").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let err = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
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
        let client = MockClient::new(canned_two_final_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let mut progress = ProgressTracker::new(10);
        let result = identify_reduce(&client, &renderer, &input, &mut progress)
            .await
            .expect("should succeed with progress");
        assert_eq!(result.abstractions.len(), 2);
        let snap = progress.snapshot();
        assert_eq!(snap.llm_calls_used, 1);
        assert_eq!(snap.llm_calls_remaining, 9);
    }

    #[tokio::test]
    async fn progress_tracker_budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_two_final_abstractions());
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        // max=0 means no calls allowed.
        let mut progress = ProgressTracker::new(0);
        let err = identify_reduce(&client, &renderer, &input, &mut progress)
            .await
            .expect_err("budget exceeded should error");
        assert!(matches!(err, IdentifyError::Budget(_)), "got: {err:?}");
        // The LLM must NOT have been called when the budget is exhausted up
        // front.
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn works_as_dyn_llm_client() {
        let client: Box<dyn LlmClient> = Box::new(MockClient::new(canned_two_final_abstractions()));
        let renderer = PromptRenderer::new().unwrap();
        let input = sample_reduce_input(sample_candidates());
        let result = identify_reduce(&*client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("dyn client should work");
        assert_eq!(result.abstractions.len(), 2);
    }

    #[tokio::test]
    async fn rendered_prompt_contains_expected_variables() {
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl llm_kernel::llm::LLMClient for CapturingClient {
            async fn complete(
                &self,
                request: llm_kernel::llm::LLMRequest,
            ) -> llm_kernel::error::Result<llm_kernel::llm::LLMResponse> {
                let prompt = crate::llm::request_prompt(&request);
                let result: Result<String, crate::llm::LlmError> = async {
                    *self.captured.lock().unwrap() = prompt.to_string();
                    Ok(canned_two_final_abstractions())
                }
                .await;
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
        let input = IdentifyReduceInput {
            candidates: sample_candidates(),
            files: sample_files(),
            project_name: "AcmeCorp/{{ evil }}".to_string(),
            language_instruction: "Use Spanish".to_string(),
            lang_note: "Use 简体中文".to_string(),
            max_abstraction_num: 7,
            module_summary: "core, utils".to_string(),
            multimodal_context: String::new(),
        };
        let _ = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
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
        // module_summary present.
        assert!(prompt.contains("core, utils"), "prompt: {prompt}");
        // lang_note propagated to the name/desc hints.
        assert!(prompt.contains("简体中文"), "prompt: {prompt}");
        // candidates_blob present (candidate names appear in the YAML blob).
        assert!(prompt.contains("Module A"), "prompt: {prompt}");
        assert!(prompt.contains("Module C"), "prompt: {prompt}");
    }

    #[test]
    fn candidates_to_yaml_produces_valid_yaml() {
        let candidates = sample_candidates();
        let yaml = candidates_to_yaml(&candidates).expect("should serialize");
        // Should parse back into the same number of candidates.
        let back: Vec<CandidateAbstraction> =
            serde_yaml_ng::from_str(&yaml).expect("should parse back");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].name, "Module A");
        assert_eq!(back[2].batch_idx, 1);
    }

    #[test]
    fn candidates_to_yaml_empty_produces_empty_list() {
        let yaml = candidates_to_yaml(&[]).expect("should serialize empty");
        assert_eq!(yaml.trim(), "[]");
    }

    #[test]
    fn candidates_to_yaml_round_trips_special_yaml_chars() {
        // Candidate names with special YAML characters should round-trip
        // through serde_yaml_ng without corruption.
        let candidates = vec![
            cand(
                "name: with colon",
                "desc # hash",
                vec![0],
                Tier::S,
                "module",
                0,
            ),
            cand("[bracketed]", "desc {braces}", vec![1], Tier::M, "class", 1),
        ];
        let yaml = candidates_to_yaml(&candidates).expect("should serialize");
        let back: Vec<CandidateAbstraction> =
            serde_yaml_ng::from_str(&yaml).expect("should parse back");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "name: with colon");
        assert_eq!(back[0].description, "desc # hash");
        assert_eq!(back[1].name, "[bracketed]");
        assert_eq!(back[1].description, "desc {braces}");
    }

    #[tokio::test]
    async fn candidates_blob_template_injection_is_sanitized() {
        use std::sync::{Arc, Mutex};

        struct CapturingClient {
            captured: Arc<Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl llm_kernel::llm::LLMClient for CapturingClient {
            async fn complete(
                &self,
                request: llm_kernel::llm::LLMRequest,
            ) -> llm_kernel::error::Result<llm_kernel::llm::LLMResponse> {
                let prompt = crate::llm::request_prompt(&request);
                let result: Result<String, crate::llm::LlmError> = async {
                    *self.captured.lock().unwrap() = prompt.to_string();
                    Ok(canned_two_final_abstractions())
                }
                .await;
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
        // A candidate with a Jinja injection payload in its name/description.
        let candidates = vec![cand(
            "{{ 7 * 7 }}",
            "desc {{ evil }}",
            vec![0],
            Tier::S,
            "module",
            0,
        )];
        let input = sample_reduce_input(candidates);
        let _ = identify_reduce(&client, &renderer, &input, &mut ProgressTracker::new(10))
            .await
            .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        // The raw `{{` / `}}` Jinja delimiters must NOT appear in the
        // candidates_blob portion of the rendered prompt.
        assert!(
            !prompt.contains("{{ 7 * 7 }}"),
            "candidate name not sanitized in prompt: {prompt}"
        );
        assert!(
            !prompt.contains("{{ evil }}"),
            "candidate description not sanitized in prompt: {prompt}"
        );
    }
}
