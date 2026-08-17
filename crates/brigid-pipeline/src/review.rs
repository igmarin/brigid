//! Optional chapter review pass (M4-CHP-2).
//!
//! When `--review-chapters` is enabled, each generated chapter is sent back to
//! the LLM for a light quality-polishing pass. The reviewed markdown is
//! validated to ensure structural fidelity before replacing the original.
//!
//! # Flow
//!
//! 1. [`review_chapter`] renders `review_chapter.md.j2`, calls the LLM, and
//!    sanitizes mermaid blocks in the response.
//! 2. [`validate_reviewed_chapter`] checks that the reviewed chapter preserves
//!    the original structure (heading count, diagram count, no invented file
//!    paths).
//! 3. [`review_chapters`] runs the review over all chapters with bounded
//!    concurrency, replacing valid reviews and keeping originals (with a
//!    warning) when validation fails or the budget is exhausted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::llm::{LlmClient, LlmError, complete_text};
use brigid_core::{
    BudgetExceeded, Chapter, ChapterResult, ProgressTracker, sanitize_markdown_mermaid_blocks,
};
use futures::future::join_all;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::chapters::count_mermaid_blocks;
use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};

/// Errors returned by the review pass.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The prompt template failed to render.
    #[error("review prompt rendering failed: {0}")]
    Prompt(#[from] crate::prompts::PromptError),
    /// The LLM call failed.
    #[error("review LLM call failed: {0}")]
    Llm(#[from] LlmError),
    /// The LLM returned empty output.
    #[error("review LLM returned empty output")]
    EmptyOutput,
    /// The configured LLM call budget was exceeded.
    #[error("budget exceeded: {0}")]
    Budget(#[from] BudgetExceeded),
}

/// Outcome of reviewing a single chapter.
#[derive(Clone, Debug)]
pub enum ReviewOutcome {
    /// The reviewed markdown replaced the original.
    Reviewed(String),
    /// The original markdown was kept, with a reason why the review was
    /// rejected.
    KeptOriginal {
        /// The original markdown (unchanged).
        original: String,
        /// Human-readable warning explaining why the review was rejected.
        warning: String,
    },
}

/// Summary of a `review_chapters` run.
#[derive(Clone, Debug, Default)]
pub struct ReviewSummary {
    /// Number of chapters whose reviewed markdown replaced the original.
    pub reviewed: usize,
    /// Number of chapters that kept their original markdown.
    pub kept_original: usize,
    /// Warnings collected for chapters that kept their originals.
    pub warnings: Vec<String>,
}

/// Review a single chapter via the LLM.
///
/// Renders `review_chapter.md.j2` with `language`, `need`, `have`, and
/// `chapter_md`, calls the LLM, sanitizes mermaid blocks, and returns the
/// reviewed markdown.
///
/// # Errors
///
/// Returns [`ReviewError`] for prompt render failures, LLM call failures, or
/// empty LLM output.
pub async fn review_chapter(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    chapter_md: &str,
    language: &str,
    need: usize,
    have: usize,
) -> Result<String, ReviewError> {
    let ctx = json!({
        "language": sanitize_template_input(language),
        "need": need,
        "have": have,
        "chapter_md": sanitize_template_input(chapter_md),
    });
    let prompt = renderer.render(PromptId::ReviewChapter, &ctx)?;

    let response = complete_text(client, &prompt).await?;
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err(ReviewError::EmptyOutput);
    }
    let sanitized = sanitize_markdown_mermaid_blocks(trimmed);
    Ok(sanitized)
}

/// Validate that a reviewed chapter preserves the structure of the original.
///
/// Checks:
/// - Same number of `## ` headings (section structure preserved).
/// - Diagram count not reduced (`>=` original mermaid fence count).
/// - No new file paths invented (paths in reviewed must appear in the original
///   or the allowed inventory).
///
/// Returns `Ok(())` if all checks pass, or `Err(warning)` with a
/// human-readable reason.
pub fn validate_reviewed_chapter(
    original: &str,
    reviewed: &str,
    allowed_paths: &[String],
) -> Result<(), String> {
    let orig_headings = count_h2_headings(original);
    let rev_headings = count_h2_headings(reviewed);
    if rev_headings != orig_headings {
        return Err(format!(
            "heading count mismatch: original has {orig_headings} ## headings, reviewed has {rev_headings}"
        ));
    }

    let orig_diagrams = count_mermaid_blocks(original);
    let rev_diagrams = count_mermaid_blocks(reviewed);
    if rev_diagrams < orig_diagrams {
        return Err(format!(
            "diagram count reduced: original has {orig_diagrams} mermaid blocks, reviewed has {rev_diagrams}"
        ));
    }

    let orig_paths = extract_file_paths(original);
    let allowed: std::collections::BTreeSet<&str> = allowed_paths
        .iter()
        .map(String::as_str)
        .chain(orig_paths.iter().map(String::as_str))
        .collect();
    let rev_paths = extract_file_paths(reviewed);
    for path in &rev_paths {
        if !allowed.contains(path.as_str()) {
            return Err(format!(
                "reviewed chapter invents new file path not in original or inventory: {path}"
            ));
        }
    }

    Ok(())
}

/// Review all chapters in a [`ChapterResult`] with bounded concurrency.
///
/// For each chapter: calls [`review_chapter`], validates the result, and
/// replaces the chapter markdown if valid. If validation fails, the LLM
/// errors, or the budget is exhausted, the original is kept with a warning.
/// The `cancel` token is checked before each chapter review; if cancelled,
/// remaining chapters keep their originals.
///
/// # Errors
///
/// Returns [`ReviewError`] only if the budget cannot be reserved up front.
/// Per-chapter LLM errors are captured as warnings, not propagated.
#[allow(clippy::too_many_arguments)]
pub async fn review_chapters(
    result: &mut ChapterResult,
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    progress: &mut ProgressTracker,
    cancel: &crate::cancellation::CancelToken,
    language: &str,
    diagram_need: impl Fn(&Chapter) -> usize + Send + Sync + 'static,
    allowed_paths: &[String],
    max_concurrency: usize,
) -> Result<ReviewSummary, ReviewError> {
    let count = result.chapters.len();
    if count == 0 {
        return Ok(ReviewSummary::default());
    }

    if count > 0 {
        progress.reserve_llm_calls(count as u32)?;
    }
    progress.set_stage("review");

    let max_concurrency = max_concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));
    let original_markdowns: Vec<String> =
        result.chapters.iter().map(|c| c.markdown.clone()).collect();
    // Chapters are moved out of `result` and sent through an mpsc channel.
    // Each review task receives its chapter by index and sends the (possibly
    // updated) chapter back through the channel. This avoids holding a
    // `Mutex<Vec<Chapter>>` lock across `.await` points.
    let chapter_count = result.chapters.len();
    let chapters: Vec<Chapter> = std::mem::take(&mut result.chapters);
    let (chapter_tx, mut chapter_rx) = mpsc::channel::<(usize, Chapter)>(chapter_count);
    let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let budget_exhausted: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let diagram_need = Arc::new(diagram_need);

    let futures = (0..chapter_count).map(|idx| {
        let sem = Arc::clone(&semaphore);
        let chapter = chapters[idx].clone();
        let warnings = Arc::clone(&warnings);
        let budget_exhausted = Arc::clone(&budget_exhausted);
        let diagram_need = Arc::clone(&diagram_need);
        let language = language.to_string();
        let allowed = allowed_paths.to_vec();
        let cancel = cancel.clone();
        let chapter_tx = chapter_tx.clone();
        async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    let msg = format!("chapter {}: review semaphore closed unexpectedly", idx + 1);
                    warnings.lock().await.push(msg);
                    let _ = chapter_tx.send((idx, chapter)).await;
                    return;
                }
            };

            if cancel.is_cancelled() || budget_exhausted.load(Ordering::Relaxed) {
                let _ = chapter_tx.send((idx, chapter)).await;
                return;
            }

            let need = diagram_need(&chapter);
            let have = count_mermaid_blocks(&chapter.markdown);

            let reviewed =
                match review_chapter(client, renderer, &chapter.markdown, &language, need, have)
                    .await
                {
                    Ok(md) => md,
                    Err(ReviewError::Llm(e)) => {
                        let msg = format!(
                            "chapter {}: review LLM error, keeping original: {e}",
                            chapter.chapter_num
                        );
                        warnings.lock().await.push(msg);
                        let _ = chapter_tx.send((idx, chapter)).await;
                        return;
                    }
                    Err(ReviewError::Budget(e)) => {
                        budget_exhausted.store(true, Ordering::Relaxed);
                        let msg = format!(
                            "chapter {}: budget exhausted, keeping original: {e}",
                            chapter.chapter_num
                        );
                        warnings.lock().await.push(msg);
                        let _ = chapter_tx.send((idx, chapter)).await;
                        return;
                    }
                    Err(e) => {
                        let msg = format!(
                            "chapter {}: review error, keeping original: {e}",
                            chapter.chapter_num
                        );
                        warnings.lock().await.push(msg);
                        let _ = chapter_tx.send((idx, chapter)).await;
                        return;
                    }
                };

            match validate_reviewed_chapter(&chapter.markdown, &reviewed, &allowed) {
                Ok(()) => {
                    let mut reviewed_chapter = chapter;
                    reviewed_chapter.markdown = reviewed;
                    let _ = chapter_tx.send((idx, reviewed_chapter)).await;
                }
                Err(reason) => {
                    let msg = format!(
                        "chapter {}: review rejected, keeping original: {reason}",
                        chapter.chapter_num
                    );
                    warnings.lock().await.push(msg);
                    let _ = chapter_tx.send((idx, chapter)).await;
                }
            }
        }
    });

    join_all(futures).await;

    // Drop the last sender so the receiver terminates after all chapters are
    // collected.
    drop(chapter_tx);

    // Collect all chapters from the channel, preserving original order.
    let mut collected: Vec<Chapter> = Vec::with_capacity(chapter_count);
    collected.resize(chapter_count, chapters[0].clone());
    while let Some((idx, ch)) = chapter_rx.recv().await {
        if idx < chapter_count {
            collected[idx] = ch;
        }
    }
    result.chapters = collected;

    let mut summary = ReviewSummary::default();
    for (i, ch) in result.chapters.iter().enumerate() {
        if ch.markdown != original_markdowns[i] {
            summary.reviewed += 1;
        } else {
            summary.kept_original += 1;
        }
    }

    summary.warnings = match Arc::try_unwrap(warnings) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => arc.lock().await.clone(),
    };

    progress.complete_stage();

    Ok(summary)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Count the number of `## ` (level-2) headings in a markdown string,
/// skipping lines inside fenced code blocks.
fn count_h2_headings(markdown: &str) -> usize {
    let mut in_fence = false;
    let mut count = 0;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && trimmed.starts_with("## ") {
            count += 1;
        }
    }
    count
}

/// Extract file-path-like tokens from a markdown string.
///
/// Recognizes paths that contain a `/` and a `.` (e.g. `src/main.rs`,
/// `apps/web/lib.rs`), as well as backtick-quoted paths. Markdown link targets
/// ending in `.md` are excluded (they are chapter cross-references, not source
/// files).
fn extract_file_paths(markdown: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for token in markdown.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| c == '`' || c == ',' || c == '.');
        if cleaned.is_empty() {
            continue;
        }
        if cleaned.ends_with(".md") {
            continue;
        }
        if cleaned.contains('/') && cleaned.contains('.') && seen.insert(cleaned.to_string()) {
            paths.push(cleaned.to_string());
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::CancelToken;
    use crate::llm::MockClient;
    use brigid_core::Tier;

    fn no_cancel() -> CancelToken {
        CancelToken::new()
    }

    fn sample_chapter(num: usize, name: &str) -> Chapter {
        let markdown = format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n\
- Need {name}\n\n\
## Core idea\n\
{name} is key.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n\n\
## How to use it\n\
Call `{name}`.\n\n\
## Under the hood\n\
It processes data.\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        );
        Chapter::new(
            0,
            num,
            format!("Chapter {num}: {name}"),
            markdown,
            Tier::M,
            "module",
            "tier=M | kind=module",
        )
    }

    fn sample_chapter_two_diagrams(num: usize, name: &str) -> Chapter {
        let markdown = format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n\
- Need {name}\n\n\
## Core idea\n\
{name} is key.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n\n\
## How to use it\n\
Call `{name}`.\n\n\
## Under the hood\n\
```mermaid\nsequenceDiagram\n  participant A0 as C\n  participant A1 as S\n  A0->>A1: R\n```\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        );
        Chapter::new(
            0,
            num,
            format!("Chapter {num}: {name}"),
            markdown,
            Tier::L,
            "module",
            "tier=L | kind=module",
        )
    }

    fn improved_chapter(num: usize, name: &str) -> String {
        format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n\
- Need {name} for routing\n\n\
## Core idea\n\
{name} is a key concept with broad impact.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[Process] --> A2[End]\n```\n\n\
## How to use it\n\
Call `{name}` in your code.\n\n\
## Under the hood\n\
It processes data efficiently.\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Handle errors gracefully\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn chapter_missing_diagram(num: usize, name: &str) -> String {
        format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n\
- Need {name}\n\n\
## Core idea\n\
{name} is key.\n\n\
## Mental model\n\
Think of a pipeline.\n\n\
## How to use it\n\
Call `{name}`.\n\n\
## Under the hood\n\
It processes data.\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn chapter_invented_path(num: usize, name: &str) -> String {
        format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n\
- Need {name}\n\n\
## Core idea\n\
{name} is key.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n\n\
## How to use it\n\
Call `{name}`.\n\n\
## Under the hood\n\
It processes data.\n\n\
## Key files\n\
- src/main.rs\n- src/fake_module.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn chapter_with_bad_mermaid(num: usize, name: &str) -> String {
        format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n\
- Need {name}\n\n\
## Core idea\n\
{name} is key.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Hello \"world\"; #x] --> A1[End]\n```\n\n\
## How to use it\n\
Call `{name}`.\n\n\
## Under the hood\n\
It processes data.\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn allowed_paths() -> Vec<String> {
        vec![
            "src/main.rs".to_string(),
            "src/router.rs".to_string(),
            "src/store.rs".to_string(),
        ]
    }

    fn diagram_need_m(_ch: &Chapter) -> usize {
        1
    }

    // --- review_chapter: happy path ---

    #[tokio::test]
    async fn review_chapter_happy_path() {
        let client = MockClient::new(improved_chapter(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let original = sample_chapter(1, "Router");
        let reviewed = review_chapter(&client, &renderer, &original.markdown, "English", 1, 1)
            .await
            .expect("review should succeed");
        assert!(reviewed.contains("# Chapter 1: Router"));
        assert!(reviewed.contains("## Motivation"));
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn review_chapter_empty_output_returns_error() {
        let client = MockClient::new("   ");
        let renderer = PromptRenderer::new().unwrap();
        let original = sample_chapter(1, "Router");
        let err = review_chapter(&client, &renderer, &original.markdown, "English", 1, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewError::EmptyOutput));
    }

    #[tokio::test]
    async fn review_chapter_llm_error_returns_error() {
        let client = MockClient::new("ok").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let original = sample_chapter(1, "Router");
        let err = review_chapter(&client, &renderer, &original.markdown, "English", 1, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewError::Llm(_)));
    }

    // --- validate_reviewed_chapter ---

    #[test]
    fn validate_accepts_improved_chapter() {
        let original = &sample_chapter(1, "Router").markdown;
        let reviewed = improved_chapter(1, "Router");
        assert!(validate_reviewed_chapter(original, &reviewed, &allowed_paths()).is_ok());
    }

    #[test]
    fn validate_rejects_missing_diagram() {
        let original = &sample_chapter(1, "Router").markdown;
        let reviewed = chapter_missing_diagram(1, "Router");
        let result = validate_reviewed_chapter(original, &reviewed, &allowed_paths());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("diagram count reduced"), "{err}");
    }

    #[test]
    fn validate_rejects_invented_file_path() {
        let original = &sample_chapter(1, "Router").markdown;
        let reviewed = chapter_invented_path(1, "Router");
        let result = validate_reviewed_chapter(original, &reviewed, &allowed_paths());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("invents new file path"), "{err}");
        assert!(err.contains("src/fake_module.rs"), "{err}");
    }

    #[test]
    fn validate_rejects_heading_count_mismatch() {
        let original = &sample_chapter(1, "Router").markdown;
        let reviewed = "# Chapter 1: Router\n\n\
## Motivation\n\
- Need\n\n\
## Core idea\n\
Key.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n"
            .to_string();
        let result = validate_reviewed_chapter(original, &reviewed, &allowed_paths());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("heading count mismatch"), "{err}");
    }

    #[test]
    fn validate_allows_path_from_inventory() {
        let original = &sample_chapter(1, "Router").markdown;
        let reviewed = "# Chapter 1: Router\n\n\
## Motivation\n\
- Need\n\n\
## Core idea\n\
Key.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n\n\
## How to use it\n\
Call it.\n\n\
## Under the hood\n\
Data.\n\n\
## Key files\n\
- src/router.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Errors\n\n\
## Summary\n\
Done.\n"
            .to_string();
        assert!(validate_reviewed_chapter(original, &reviewed, &allowed_paths()).is_ok());
    }

    #[test]
    fn validate_allows_extra_diagrams() {
        let original = &sample_chapter(1, "Router").markdown;
        let reviewed = &sample_chapter_two_diagrams(1, "Router").markdown;
        assert!(validate_reviewed_chapter(original, reviewed, &allowed_paths()).is_ok());
    }

    // --- review_chapters: happy path ---

    #[tokio::test]
    async fn review_chapters_happy_path_all_reviewed() {
        let client = MockClient::new(improved_chapter(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(vec![sample_chapter(1, "Router")]);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 1);
        assert_eq!(summary.kept_original, 0);
        assert!(result.chapters[0].markdown.contains("broad impact"));
    }

    #[tokio::test]
    async fn review_chapters_review_removes_diagram_keeps_original() {
        let client = MockClient::new(chapter_missing_diagram(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let original_md = sample_chapter(1, "Router").markdown.clone();
        let mut result = ChapterResult::new(vec![sample_chapter(1, "Router")]);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 1);
        assert_eq!(summary.warnings.len(), 1);
        assert!(summary.warnings[0].contains("diagram count reduced"));
        assert_eq!(result.chapters[0].markdown, original_md);
    }

    #[tokio::test]
    async fn review_chapters_invents_path_keeps_original() {
        let client = MockClient::new(chapter_invented_path(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let original_md = sample_chapter(1, "Router").markdown.clone();
        let mut result = ChapterResult::new(vec![sample_chapter(1, "Router")]);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 1);
        assert!(summary.warnings[0].contains("invents new file path"));
        assert_eq!(result.chapters[0].markdown, original_md);
    }

    #[tokio::test]
    async fn review_chapters_llm_error_keeps_original() {
        let client = MockClient::new("ok").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let original_md = sample_chapter(1, "Router").markdown.clone();
        let mut result = ChapterResult::new(vec![sample_chapter(1, "Router")]);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 1);
        assert!(summary.warnings[0].contains("LLM error"));
        assert_eq!(result.chapters[0].markdown, original_md);
    }

    #[tokio::test]
    async fn review_chapters_budget_exhaustion_keeps_remaining_originals() {
        let client = MockClient::new(improved_chapter(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(vec![
            sample_chapter(1, "Router"),
            sample_chapter(2, "Store"),
        ]);
        let mut progress = ProgressTracker::new(1);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut progress,
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            1,
        )
        .await
        .unwrap_err();
        assert!(matches!(summary, ReviewError::Budget(_)));
    }

    #[tokio::test]
    async fn review_chapters_mixed_some_reviewed_some_kept() {
        let responses = vec![
            improved_chapter(1, "Router"),
            chapter_missing_diagram(2, "Store"),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(vec![
            sample_chapter(1, "Router"),
            sample_chapter(2, "Store"),
        ]);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 1);
        assert_eq!(summary.kept_original, 1);
        assert_eq!(summary.warnings.len(), 1);
        assert!(result.chapters[0].markdown.contains("broad impact"));
        assert!(!result.chapters[1].markdown.contains("broad impact"));
    }

    #[tokio::test]
    async fn review_chapters_sanitizes_bad_mermaid_in_reviewed() {
        let client = MockClient::new(chapter_with_bad_mermaid(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(vec![sample_chapter(1, "Router")]);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 1, "chapter should be reviewed");
        assert!(
            !result.chapters[0].markdown.contains('"'),
            "reviewed markdown should not contain raw double-quotes after mermaid sanitization"
        );
    }

    #[tokio::test]
    async fn review_chapters_empty_result_returns_empty_summary() {
        let client = MockClient::new("ok");
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(Vec::new());
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 0);
    }

    #[tokio::test]
    async fn review_chapters_cancelled_keeps_all_originals() {
        let client = MockClient::new(improved_chapter(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(vec![sample_chapter(1, "Router")]);
        let cancel = CancelToken::new();
        cancel.cancel();
        let original_md = result.chapters[0].markdown.clone();
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &cancel,
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 1);
        assert_eq!(result.chapters[0].markdown, original_md);
        assert_eq!(client.call_count(), 0);
    }

    // --- high-concurrency / lock-contention tests ---

    #[tokio::test]
    async fn review_chapters_high_concurrency_completes_without_deadlock() {
        // 8 chapters reviewed concurrently with max_concurrency=8.
        // If locks are held across .await points this can deadlock or hang.
        let responses: Vec<String> = (1..=8)
            .map(|i| improved_chapter(i, &format!("Mod{i}")))
            .collect();
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let chapters: Vec<Chapter> = (1..=8)
            .map(|i| sample_chapter(i, &format!("Mod{i}")))
            .collect();
        let mut result = ChapterResult::new(chapters);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            8,
        )
        .await
        .expect("should succeed under high concurrency");
        assert_eq!(summary.reviewed, 8);
        assert_eq!(summary.kept_original, 0);
        assert_eq!(summary.warnings.len(), 0);
        assert_eq!(client.call_count(), 8);
        for ch in &result.chapters {
            assert!(ch.markdown.contains("broad impact"));
        }
    }

    #[tokio::test]
    async fn review_chapters_high_concurrency_collects_all_warnings() {
        // 8 chapters all fail validation (missing diagram) concurrently.
        // Every failure must produce a warning — none should be lost.
        let responses: Vec<String> = (1..=8)
            .map(|i| chapter_missing_diagram(i, &format!("Mod{i}")))
            .collect();
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let chapters: Vec<Chapter> = (1..=8)
            .map(|i| sample_chapter(i, &format!("Mod{i}")))
            .collect();
        let original_mds: Vec<String> = chapters.iter().map(|c| c.markdown.clone()).collect();
        let mut result = ChapterResult::new(chapters);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            8,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 8);
        assert_eq!(
            summary.warnings.len(),
            8,
            "all 8 chapters should produce a warning"
        );
        for w in &summary.warnings {
            assert!(w.contains("diagram count reduced"), "{w}");
        }
        for (i, ch) in result.chapters.iter().enumerate() {
            assert_eq!(ch.markdown, original_mds[i], "originals must be preserved");
        }
    }

    #[tokio::test]
    async fn review_chapters_budget_exhausted_flag_visible_across_tasks() {
        // When the budget is exhausted upfront, review_chapters returns a
        // Budget error immediately and no chapters are modified.
        let client = MockClient::new(improved_chapter(1, "Router"));
        let renderer = PromptRenderer::new().unwrap();
        let mut result = ChapterResult::new(vec![
            sample_chapter(1, "Router"),
            sample_chapter(2, "Store"),
            sample_chapter(3, "Cache"),
        ]);
        let original_mds: Vec<String> =
            result.chapters.iter().map(|c| c.markdown.clone()).collect();
        let mut progress = ProgressTracker::new(2);
        let err = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut progress,
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ReviewError::Budget(_)));
        // No LLM calls should have been made.
        assert_eq!(client.call_count(), 0);
        // All chapters must retain their originals.
        for (i, ch) in result.chapters.iter().enumerate() {
            assert_eq!(ch.markdown, original_mds[i]);
        }
    }

    #[tokio::test]
    async fn review_chapters_concurrent_llm_errors_all_warned() {
        // 4 chapters, each hitting an LLM timeout concurrently.
        // All 4 errors must be captured as warnings.
        let client = MockClient::new("ok").fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let chapters: Vec<Chapter> = (1..=4)
            .map(|i| sample_chapter(i, &format!("Mod{i}")))
            .collect();
        let original_mds: Vec<String> = chapters.iter().map(|c| c.markdown.clone()).collect();
        let mut result = ChapterResult::new(chapters);
        let summary = review_chapters(
            &mut result,
            &client,
            &renderer,
            &mut ProgressTracker::new(10),
            &no_cancel(),
            "English",
            diagram_need_m,
            &allowed_paths(),
            4,
        )
        .await
        .expect("should succeed");
        assert_eq!(summary.reviewed, 0);
        assert_eq!(summary.kept_original, 4);
        // fail_on(0) only fails the first call; the other 3 succeed but
        // return "ok" which fails validation (heading mismatch), so all 4
        // produce warnings.
        assert_eq!(summary.warnings.len(), 4);
        for (i, ch) in result.chapters.iter().enumerate() {
            assert_eq!(ch.markdown, original_mds[i]);
        }
    }

    // --- helper unit tests ---

    #[test]
    fn count_h2_headings_works() {
        let md = "## A\n## B\n### C\n## D\n";
        assert_eq!(count_h2_headings(md), 3);
    }

    #[test]
    fn count_h2_headings_skips_code_blocks() {
        let md = "## A\n```rust\n## not_a_heading\n```\n## B\n";
        assert_eq!(count_h2_headings(md), 2);
    }

    #[test]
    fn extract_file_paths_finds_source_paths() {
        let md = "See src/main.rs and apps/web/lib.rs for details.\nLink: [x](02_other.md)\n";
        let paths = extract_file_paths(md);
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(paths.contains(&"apps/web/lib.rs".to_string()));
        assert!(!paths.contains(&"02_other.md".to_string()));
    }

    #[test]
    fn extract_file_paths_dedupes() {
        let md = "src/main.rs src/main.rs";
        let paths = extract_file_paths(md);
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn extract_file_paths_ignores_non_paths() {
        let md = "hello world foo.bar";
        let paths = extract_file_paths(md);
        assert!(paths.is_empty());
    }
}
