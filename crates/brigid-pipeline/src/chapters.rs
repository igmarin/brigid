//! WriteChapters pipeline stage (M4-CHP-1).
//!
//! Generates one markdown chapter per abstraction using the fixed 10-section
//! outline, diagram quota enforcement, grounding rules, and evidence footers.
//! Chapters are written with bounded concurrency (tokio semaphore).
//!
//! # Flow
//!
//! 1. [`select_chapter_file_context`] picks entry files first, then
//!    `file_indices`, truncating by budget and using path-only stubs for
//!    overflow (pure, testable).
//! 2. [`write_single_chapter`] renders `chapter_outline.md.j2` then
//!    `write_chapter.md.j2`, redacts secrets from file context, calls the LLM,
//!    sanitizes mermaid blocks, and attaches an evidence footer.
//! 3. [`write_chapters`] runs all chapters concurrently with a bounded
//!    semaphore, passing summaries of previously completed chapters for
//!    continuity (best-practices §6.1).
//! 4. [`chapters_and_checkpoint`] wraps the above with checkpoint persistence
//!    and partial-resume semantics.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use brigid_core::{
    Abstraction, BudgetExceeded, Chapter, ChapterOrder, ChapterResult, CheckpointV1,
    IdentifyResult, ProgressTracker, StageId, Tier, path_stub, path_stub_chars, redact_content,
    sanitize_markdown_mermaid_blocks, truncate_content,
};
use brigid_llm::{LlmClient, LlmError};
use futures::future::join_all;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::prompts::{PromptId, PromptRenderer, sanitize_template_input};
use crate::resume;

/// Re-export of the core [`brigid_core::CheckpointError`] for ergonomic matching.
pub use brigid_core::CheckpointError as CoreCheckpointError;

/// Default context character budget for chapter file selection.
pub const DEFAULT_CHAPTERS_BUDGET: usize = 80_000;

/// Default per-file character cap for chapter file context.
pub const DEFAULT_CHAPTER_MAX_FILE_CHARS: usize = 12_000;

/// Default maximum concurrent chapter writes.
pub const DEFAULT_CHAPTERS_CONCURRENCY: usize = 4;

/// Diagram richness level controlling minimum mermaid block counts.
///
/// See `docs/best-practices.md` §7.2 for the policy table.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagramLevel {
    /// Minimal diagrams — only required for tier L.
    Minimal,
    /// Standard diagrams — required for tiers M and L.
    #[default]
    Standard,
    /// Rich diagrams — required for all tiers.
    Rich,
}

impl DiagramLevel {
    /// Canonical wire string (`"minimal"`, `"standard"`, `"rich"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Standard => "standard",
            Self::Rich => "rich",
        }
    }

    /// Parse a wire string into a known diagram level.
    ///
    /// Returns `None` for unrecognized input.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "minimal" => Some(Self::Minimal),
            "standard" => Some(Self::Standard),
            "rich" => Some(Self::Rich),
            _ => None,
        }
    }
}

impl std::fmt::Display for DiagramLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors returned by the chapters stage.
#[derive(Debug, thiserror::Error)]
pub enum ChaptersError {
    /// The prompt template failed to render (missing/invalid variable).
    #[error("prompt rendering failed: {0}")]
    Prompt(#[from] crate::prompts::PromptError),
    /// The LLM call failed (network, timeout, rate limit, provider error).
    #[error("LLM call failed: {0}")]
    Llm(#[from] brigid_llm::LlmError),
    /// The LLM returned empty output.
    #[error("LLM returned empty chapter output")]
    EmptyOutput,
    /// A checkpoint save/load failed during the chapters stage.
    #[error("checkpoint error during chapters: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
    /// The configured LLM call budget was exceeded.
    #[error("budget exceeded: {0}")]
    Budget(#[from] BudgetExceeded),
}

/// Configuration for the chapters stage.
#[derive(Clone, Debug)]
pub struct ChaptersConfig {
    /// Project name.
    pub project_name: String,
    /// Language instruction for the chapter prompt (e.g. `"Use Spanish"`).
    pub language_instruction: String,
    /// Short language label for the outline template (e.g. `"English"`).
    pub lang: String,
    /// Diagram richness level (drives minimum mermaid block counts).
    pub diagram_level: DiagramLevel,
    /// Maximum number of concurrent chapter writes.
    pub max_concurrency: usize,
    /// Context character budget for file selection.
    pub budget: usize,
    /// Per-file character cap before truncation.
    pub max_file_chars: usize,
}

impl Default for ChaptersConfig {
    fn default() -> Self {
        Self {
            project_name: String::new(),
            language_instruction: String::new(),
            lang: "English".to_string(),
            diagram_level: DiagramLevel::Standard,
            max_concurrency: DEFAULT_CHAPTERS_CONCURRENCY,
            budget: DEFAULT_CHAPTERS_BUDGET,
            max_file_chars: DEFAULT_CHAPTER_MAX_FILE_CHARS,
        }
    }
}

/// Minimum number of mermaid diagrams required for a tier and diagram level.
///
/// Table from `docs/best-practices.md` §7.2:
///
/// | Tier | Minimal | Standard | Rich |
/// |------|---------|----------|------|
/// | S    | 0       | 0        | 1    |
/// | M    | 0       | 1        | 2    |
/// | L    | 1       | 2        | 3    |
#[must_use]
pub fn diagram_quota_for_tier(tier: Tier, level: DiagramLevel) -> usize {
    match (tier, level) {
        (Tier::S, DiagramLevel::Minimal) => 0,
        (Tier::S, DiagramLevel::Standard) => 0,
        (Tier::S, DiagramLevel::Rich) => 1,
        (Tier::M, DiagramLevel::Minimal) => 0,
        (Tier::M, DiagramLevel::Standard) => 1,
        (Tier::M, DiagramLevel::Rich) => 2,
        (Tier::L, DiagramLevel::Minimal) => 1,
        (Tier::L, DiagramLevel::Standard) => 2,
        (Tier::L, DiagramLevel::Rich) => 3,
    }
}

/// Count the number of fenced ```mermaid blocks in a markdown string.
#[must_use]
pub fn count_mermaid_blocks(markdown: &str) -> usize {
    markdown.matches("```mermaid").count()
}

/// Extract a short summary (headings + up to 2 bullet points) from chapter
/// markdown for passing to subsequent chapters.
///
/// Implements best-practices §6.1: "Pass summaries of previous chapters, not
/// full prior chapter text."
#[must_use]
pub fn extract_chapter_summary(markdown: &str) -> String {
    let mut summary = String::new();
    let mut bullet_count = 0usize;

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            if !summary.is_empty() {
                summary.push('\n');
            }
            summary.push_str(line.trim());
        } else if (trimmed.starts_with("- ") || trimmed.starts_with("* ")) && bullet_count < 2 {
            if !summary.is_empty() {
                summary.push('\n');
            }
            summary.push_str(trimmed);
            bullet_count += 1;
        }
    }

    summary
}

/// Select file contents for a chapter prompt, respecting the context budget.
///
/// # Algorithm
///
/// 1. Collect candidate paths: `entry_files` first, then paths referenced by
///    `file_indices` (de-duplicated, in order).
/// 2. For each candidate, add its full (truncated) content while the total
///    stays under `budget`.
/// 3. When the budget is exhausted, add path-only stubs for remaining
///    candidates (best-practices §3.2).
///
/// `file_contents` is a slice of `(path, content)` pairs from the crawl
/// inventory. `file_indices` indexes into this slice.
#[must_use]
pub fn select_chapter_file_context(
    abstraction: &Abstraction,
    file_contents: &[(String, String)],
    budget: usize,
    max_file_chars: usize,
) -> String {
    let mut candidates: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for ef in &abstraction.entry_files {
        if seen.insert(ef.clone()) {
            candidates.push(ef.clone());
        }
    }
    for &idx in &abstraction.file_indices {
        if let Some((path, _)) = file_contents.get(idx) {
            if seen.insert(path.clone()) {
                candidates.push(path.clone());
            }
        }
    }

    let mut context = String::new();
    let mut total: usize = 0;

    // Build a HashMap once for O(1) per-file content lookup instead of O(n)
    // linear scan on every candidate path (issue #216).
    let file_map: HashMap<&str, &str> = file_contents
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();

    for path in &candidates {
        let content = file_map.get(path.as_str()).copied().unwrap_or("");

        let truncated = truncate_content(content, max_file_chars);
        let content_size = truncated.text.chars().count();

        if total.saturating_add(content_size) <= budget {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str("# File: ");
            context.push_str(path);
            context.push('\n');
            context.push_str(&truncated.text);
            total = total.saturating_add(content_size);
        } else {
            let stub_chars = path_stub_chars(path);
            if total.saturating_add(stub_chars) <= budget {
                if !context.is_empty() {
                    context.push_str("\n\n");
                }
                context.push_str(&path_stub(path));
                total = total.saturating_add(stub_chars);
            }
        }
    }

    context
}

/// Generate a single chapter via the LLM.
///
/// Renders `chapter_outline.md.j2` to produce the outline fragment, then
/// renders `write_chapter.md.j2` with all required variables. Secrets in the
/// file context are redacted before rendering. The LLM response is treated as
/// raw markdown (no YAML extraction), mermaid blocks are sanitized, and an
/// evidence footer is attached.
///
/// # Errors
///
/// Returns [`ChaptersError`] for prompt render failures, LLM call failures,
/// or empty LLM output.
#[allow(clippy::too_many_arguments)]
pub async fn write_single_chapter(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    abstraction: &Abstraction,
    abstraction_index: usize,
    chapter_num: usize,
    prev_link: &str,
    next_link: &str,
    full_chapter_listing: &str,
    previous_chapters_summary: &str,
    file_context: &str,
    project_name: &str,
    language_instruction: &str,
    lang: &str,
    diagram_level: DiagramLevel,
) -> Result<Chapter, ChaptersError> {
    let tier = abstraction.tier;
    let need = diagram_quota_for_tier(tier, diagram_level);

    let outline_ctx = json!({
        "lang": sanitize_template_input(lang),
        "tier": tier.as_str(),
        "diagram_level": diagram_level.as_str(),
        "need": need,
    });
    let chapter_outline = renderer.render(PromptId::ChapterOutline, &outline_ctx)?;

    let apps_line = format_apps_line(&abstraction.apps);
    let entry_list = format_entry_list(&abstraction.entry_files);

    let redacted_context = redact_content(file_context);

    let render_ctx = json!({
        "project_name": sanitize_template_input(project_name),
        "abstraction_name": sanitize_template_input(&abstraction.name),
        "chapter_num": chapter_num,
        "abstraction_description": sanitize_template_input(&abstraction.description),
        "tier": tier.as_str(),
        "kind": abstraction.kind.as_str(),
        "apps_line": sanitize_template_input(&apps_line),
        "entry_list": entry_list,
        "full_chapter_listing": sanitize_template_input(full_chapter_listing),
        "prev_link": sanitize_template_input(prev_link),
        "next_link": sanitize_template_input(next_link),
        "previous_chapters_summary": sanitize_template_input(previous_chapters_summary),
        "file_context_str": sanitize_template_input(&redacted_context),
        "chapter_outline": chapter_outline,
        "need": need,
        "language_instruction": sanitize_template_input(language_instruction),
    });

    let prompt = renderer.render(PromptId::WriteChapter, &render_ctx)?;

    let response = client.complete(&prompt).await?;

    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err(ChaptersError::EmptyOutput);
    }

    let sanitized = sanitize_markdown_mermaid_blocks(trimmed);
    let title = extract_chapter_title(&sanitized).unwrap_or_else(|| abstraction.name.clone());
    let evidence_footer = build_evidence_footer(abstraction);

    let mut chapter = Chapter::new(
        abstraction_index,
        chapter_num,
        title,
        sanitized,
        tier,
        abstraction.kind.clone(),
        evidence_footer,
    );
    // `abstraction` is borrowed (`&Abstraction`) so we cannot move `apps`
    // or `entry_files` out of it with `std::mem::take`. The clone is
    // unavoidable here unless the signature is changed to take ownership,
    // which would force the caller to clone the entire `Abstraction` anyway
    // (it lives in a shared `Vec<Abstraction>`). See issue #217.
    chapter.apps = abstraction.apps.clone();
    chapter.entry_files = abstraction.entry_files.clone();

    Ok(chapter)
}

/// Generate all chapters for the given identify result and chapter order,
/// running concurrently with bounded concurrency.
///
/// Each chapter receives a summary of previously completed chapters for
/// continuity (best-practices §6.1). Budget is reserved up front via
/// [`ProgressTracker::reserve_llm_calls`].
///
/// # Errors
///
/// Returns [`ChaptersError`] for prompt/LLM failures, budget overruns, or
/// empty LLM output.
#[allow(clippy::too_many_arguments)]
pub async fn write_chapters(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    identify: &IdentifyResult,
    order: &ChapterOrder,
    file_contents: &[(String, String)],
    config: &ChaptersConfig,
    progress: Option<&mut ProgressTracker>,
) -> Result<ChapterResult, ChaptersError> {
    generate_chapters_internal(
        client,
        renderer,
        identify,
        order,
        file_contents,
        config,
        None,
        progress,
    )
    .await
}

/// Run the full chapters stage with checkpoint persistence and resume.
///
/// # Flow
///
/// 1. If [`StageId::Chapters`] is complete and its files are intact, load and
///    return the existing chapters.
/// 2. Load any partially-written chapters from the checkpoint.
/// 3. Generate only the missing chapters (partial regeneration).
/// 4. Merge, write all chapter files, record stage outputs, and mark the
///    stage complete.
///
/// # Errors
///
/// Returns [`ChaptersError`] for generation or checkpoint failures.
#[allow(clippy::too_many_arguments)]
pub async fn chapters_and_checkpoint(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    identify: &IdentifyResult,
    order: &ChapterOrder,
    file_contents: &[(String, String)],
    config: &ChaptersConfig,
    progress: Option<&mut ProgressTracker>,
) -> Result<ChapterResult, ChaptersError> {
    if store.is_stage_complete_with_files(checkpoint, StageId::Chapters)? {
        return store
            .read_chapters(&store.dir, checkpoint)
            .map_err(ChaptersError::from);
    }

    let existing = store.read_chapters(&store.dir, checkpoint)?;
    let existing_by_num: HashMap<usize, Chapter> = existing
        .chapters
        .into_iter()
        .map(|c| (c.chapter_num, c))
        .collect();

    let result = generate_chapters_internal(
        client,
        renderer,
        identify,
        order,
        file_contents,
        config,
        Some(&existing_by_num),
        progress,
    )
    .await?;

    let entries = store.write_chapters(&store.dir, &result)?;
    checkpoint.mark_stage_complete(StageId::Chapters, now_iso8601_utc());
    store.record_stage_outputs(checkpoint, StageId::Chapters, entries)?;

    Ok(result)
}

/// Check if the chapters stage should run based on the checkpoint state.
#[must_use]
pub fn should_run_chapters(checkpoint: &CheckpointV1) -> bool {
    resume::should_run(StageId::Chapters, checkpoint)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Metadata for a single chapter position in the ordered list.
struct ChapterMeta {
    abs_idx: usize,
    chapter_num: usize,
    prev_link: String,
    next_link: String,
}

/// Internal generation engine shared by [`write_chapters`] and
/// [`chapters_and_checkpoint`].
///
/// When `existing` is `Some`, chapters already present are skipped (only
/// missing ones are generated). Budget is reserved only for the chapters that
/// need generation.
#[allow(clippy::too_many_arguments)]
async fn generate_chapters_internal(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    identify: &IdentifyResult,
    order: &ChapterOrder,
    file_contents: &[(String, String)],
    config: &ChaptersConfig,
    existing: Option<&HashMap<usize, Chapter>>,
    progress: Option<&mut ProgressTracker>,
) -> Result<ChapterResult, ChaptersError> {
    let abstractions = &identify.abstractions;
    let ordered_indices = &order.ordered_indices;

    if ordered_indices.is_empty() {
        return Ok(ChapterResult::new(Vec::new()));
    }

    let full_chapter_listing = build_full_chapter_listing(abstractions, ordered_indices);

    let metas: Vec<ChapterMeta> = ordered_indices
        .iter()
        .enumerate()
        .map(|(pos, &abs_idx)| {
            let chapter_num = pos + 1;
            let prev_link = if pos > 0 {
                let prev_idx = ordered_indices[pos - 1];
                let prev_name = &abstractions[prev_idx].name;
                chapter_link(pos, prev_name)
            } else {
                "None".to_string()
            };
            let next_link = if pos < ordered_indices.len() - 1 {
                let next_idx = ordered_indices[pos + 1];
                let next_name = &abstractions[next_idx].name;
                chapter_link(pos + 2, next_name)
            } else {
                "None".to_string()
            };
            ChapterMeta {
                abs_idx,
                chapter_num,
                prev_link,
                next_link,
            }
        })
        .collect();

    let positions_to_generate: Vec<usize> = match existing {
        Some(map) => (0..metas.len())
            .filter(|&pos| !map.contains_key(&(pos + 1)))
            .collect(),
        None => (0..metas.len()).collect(),
    };

    let gen_count = positions_to_generate.len();
    if let Some(tracker) = progress {
        if gen_count > 0 {
            tracker
                .reserve_llm_calls(gen_count as u32)
                .map_err(ChaptersError::from)?;
        }
        tracker.set_stage("chapters");
    }

    if gen_count == 0 {
        let mut chapters: Vec<Chapter> = existing
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        chapters.sort_by_key(|c| c.chapter_num);
        return Ok(ChapterResult::new(chapters));
    }

    let max_concurrency = config.max_concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));

    // Lightweight summary store: (chapter_num, summary_string) pairs.
    //
    // The lock is held only briefly for synchronous clone/insert and is
    // never held across `.await` points. This eliminates the contention
    // caused by the former `Arc<RwLock<Vec<Chapter>>>` which held a read
    // guard across string-allocating `extract_chapter_summary` calls and
    // required cloning the entire `Vec<Chapter>` on `try_unwrap` fallback.
    let summaries: Arc<Mutex<Vec<(usize, String)>>> = Arc::new(Mutex::new(
        existing
            .map(|m| {
                m.values()
                    .map(|c| (c.chapter_num, extract_chapter_summary(&c.markdown)))
                    .collect()
            })
            .unwrap_or_default(),
    ));

    // Channel-based collection: each chapter task sends its result through
    // an mpsc channel. This replaces the `Arc<RwLock<Vec<Chapter>>>` and
    // avoids cloning the entire Vec on `try_unwrap` fallback.
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<Chapter, ChaptersError>>();

    let futures = positions_to_generate.iter().map(|&pos| {
        let sem = Arc::clone(&semaphore);
        let summaries = Arc::clone(&summaries);
        let tx = tx.clone();
        let meta = &metas[pos];
        let abstraction = &abstractions[meta.abs_idx];
        let listing = &full_chapter_listing;
        let diagram_level = config.diagram_level;
        let budget = config.budget;
        let max_file_chars = config.max_file_chars;
        async move {
            let result = async {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|_| LlmError::network("chapter semaphore closed unexpectedly"))?;

                // Clone summary data under a brief lock, then release
                // before any async work (no guard held across `.await`).
                let summary = {
                    let guard = summaries.lock().await;
                    join_chapter_summaries(
                        guard
                            .iter()
                            .filter(|(num, _)| *num < meta.chapter_num)
                            .map(|(_, s)| s.as_str()),
                    )
                };

                let file_context =
                    select_chapter_file_context(abstraction, file_contents, budget, max_file_chars);

                let chapter = write_single_chapter(
                    client,
                    renderer,
                    abstraction,
                    meta.abs_idx,
                    meta.chapter_num,
                    &meta.prev_link,
                    &meta.next_link,
                    listing,
                    &summary,
                    &file_context,
                    &config.project_name,
                    &config.language_instruction,
                    &config.lang,
                    diagram_level,
                )
                .await?;

                // Update the summary store for subsequent chapters.
                {
                    let mut guard = summaries.lock().await;
                    guard.push((meta.chapter_num, extract_chapter_summary(&chapter.markdown)));
                }

                Ok::<Chapter, ChaptersError>(chapter)
            }
            .await;

            // Send result through the channel (no shared lock for collection).
            let _ = tx.send(result);
        }
    });

    // Collect futures into a Vec so each closure's `tx.clone()` executes,
    // releasing the borrow on the original `tx`.
    let futures: Vec<_> = futures.collect();

    // Drop the last sender so `rx.recv()` completes after all tasks finish.
    drop(tx);

    join_all(futures).await;

    // Collect all generated chapters from the channel.
    let mut generated: Vec<Chapter> = Vec::with_capacity(gen_count);
    while let Some(result) = rx.recv().await {
        generated.push(result?);
    }

    // Merge generated chapters with pre-existing ones (from checkpoint resume).
    let mut all_chapters: Vec<Chapter> = existing
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();
    all_chapters.extend(generated);
    all_chapters.sort_by_key(|c| c.chapter_num);

    for chapter in &all_chapters {
        let count = count_mermaid_blocks(&chapter.markdown);
        let required = diagram_quota_for_tier(chapter.tier, config.diagram_level);
        if count < required {
            eprintln!(
                "Warning: chapter {} ({}) has {} mermaid blocks, minimum is {}",
                chapter.chapter_num, chapter.title, count, required
            );
        }
    }

    Ok(ChapterResult::new(all_chapters))
}

/// Build the full chapter listing string for the prompt.
///
/// Format: `1. [Name](01_slug.md)` per line.
fn build_full_chapter_listing(abstractions: &[Abstraction], ordered_indices: &[usize]) -> String {
    ordered_indices
        .iter()
        .enumerate()
        .map(|(pos, &idx)| {
            let chapter_num = pos + 1;
            let name = &abstractions[idx].name;
            format!("{chapter_num}. {}", chapter_link(chapter_num, name))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a markdown link to a chapter file: `[Name](NN_slug.md)`.
fn chapter_link(chapter_num: usize, name: &str) -> String {
    let slug = slugify(name);
    format!("[{name}]({chapter_num:02}_{slug}.md)")
}

/// Extract the chapter title from the first `# ` heading in the markdown.
fn extract_chapter_title(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Build the evidence footer string for a chapter.
fn build_evidence_footer(abstraction: &Abstraction) -> String {
    let apps = if abstraction.apps.is_empty() {
        "N/A".to_string()
    } else {
        abstraction.apps.join(", ")
    };
    let entry_files = if abstraction.entry_files.is_empty() {
        "N/A".to_string()
    } else {
        abstraction.entry_files.join(", ")
    };
    format!(
        "---\nEvidence: tier={} | kind={} | apps={} | entry_files={}",
        abstraction.tier.as_str(),
        abstraction.kind.as_str(),
        apps,
        entry_files
    )
}

/// Format the apps line for a chapter prompt, joining apps with `", "`.
///
/// Returns `"N/A"` when the list is empty. Uses a single pre-allocated
/// `String` instead of `Vec::join` to avoid an intermediate allocation.
#[must_use]
fn format_apps_line(apps: &[String]) -> String {
    if apps.is_empty() {
        return "N/A".to_string();
    }
    // Estimate: sum of app lengths + 2 bytes (", ") per separator.
    let estimated = apps.iter().map(|a| a.len() + 2).sum::<usize>();
    let mut out = String::with_capacity(estimated);
    for (i, app) in apps.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(app);
    }
    out
}

/// Format the entry-files list for a chapter prompt as bullet points.
///
/// Returns `"(none)"` when the list is empty. Each entry is written as
/// `"- {file}\n"` into a pre-allocated `String`, then the trailing newline
/// is removed. This replaces the `map(format!).collect().join()` pattern
/// which required two intermediate allocations (a `Vec<String>` and the
/// joined `String`).
#[must_use]
fn format_entry_list(entry_files: &[String]) -> String {
    if entry_files.is_empty() {
        return "(none)".to_string();
    }
    let mut out = String::with_capacity(entry_files.len() * 40);
    for f in entry_files {
        writeln!(out, "- {f}").ok();
    }
    // Remove the single trailing newline added by the last `writeln!`.
    out.pop();
    out
}

/// Join chapter summaries with `"\n\n"` separators into a single `String`.
///
/// Replaces `collect::<Vec<_>>().join("\n\n")` which allocated an
/// intermediate `Vec<&str>` before the final `String`.
#[must_use]
fn join_chapter_summaries<'a>(summaries: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    let mut first = true;
    for s in summaries {
        if !first {
            out.push_str("\n\n");
        }
        out.push_str(s);
        first = false;
    }
    out
}

/// Slugify a title for use in chapter filenames (matches checkpoint_store).
fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        return "chapter".to_owned();
    }
    let mut result = String::new();
    let mut count = 0usize;
    for ch in trimmed.chars() {
        if count >= 50 {
            break;
        }
        result.push(ch);
        count += ch.len_utf8();
    }
    result.trim_matches('-').to_owned()
}

/// Generate an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
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

/// Convert days since the Unix epoch to a proleptic Gregorian `(year, month,
/// day)` tuple (Howard Hinnant `civil_from_days`).
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
    use brigid_core::{AbstractionKind, RunConfig};
    use brigid_llm::{LlmClient, MockClient};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-chp-ckpt-{n}-{seq}"));
        std::fs::create_dir_all(&dir).unwrap();
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
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
        cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:04:00Z");
        cp.mark_stage_complete(StageId::Order, "2026-07-24T00:05:00Z");
        let files = records_from_files(&[
            ("src/router.rs", b"fn route() {}"),
            ("src/store.rs", b"fn store() {}"),
            ("src/worker.rs", b"fn work() {}"),
        ]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn three_abstractions() -> Vec<Abstraction> {
        vec![
            Abstraction {
                name: "Router".into(),
                description: "Routes requests".into(),
                file_indices: vec![0],
                tier: Tier::M,
                kind: AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/router.rs".into()],
            },
            Abstraction {
                name: "Store".into(),
                description: "Persistence layer".into(),
                file_indices: vec![1],
                tier: Tier::S,
                kind: AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/store.rs".into()],
            },
            Abstraction {
                name: "Worker".into(),
                description: "Background jobs".into(),
                file_indices: vec![2],
                tier: Tier::L,
                kind: AbstractionKind::new("module"),
                apps: vec!["api".into()],
                entry_files: vec!["src/worker.rs".into()],
            },
        ]
    }

    fn file_contents_for() -> Vec<(String, String)> {
        vec![
            (
                "src/router.rs".to_string(),
                "fn route() { /* routing logic */ }".to_string(),
            ),
            (
                "src/store.rs".to_string(),
                "fn store() { /* persistence */ }".to_string(),
            ),
            (
                "src/worker.rs".to_string(),
                "fn work() { /* background */ }".to_string(),
            ),
        ]
    }

    fn sample_config() -> ChaptersConfig {
        ChaptersConfig {
            project_name: "my-project".to_string(),
            language_instruction: String::new(),
            lang: "English".to_string(),
            diagram_level: DiagramLevel::Standard,
            max_concurrency: 4,
            budget: 80_000,
            max_file_chars: 12_000,
        }
    }

    fn canned_chapter(name: &str, chapter_num: usize) -> String {
        format!(
            "# Chapter {chapter_num}: {name}\n\n\
## Motivation\n\
- Need to handle {name}\n\
- Real-world use case\n\n\
## Core idea\n\
{name} is a key concept.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n\n\
## How to use it\n\
Call `{name}` in your code.\n\n\
## Under the hood\n\
```mermaid\nsequenceDiagram\n  participant A0 as Client\n  participant A1 as Server\n  A0->>A1: Request\n  A1-->>A0: Response\n```\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Don't forget to handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn canned_chapter_no_diagrams(name: &str, chapter_num: usize) -> String {
        format!(
            "# Chapter {chapter_num}: {name}\n\n\
## Motivation\n\
- Need to handle {name}\n\n\
## Core idea\n\
{name} is a key concept.\n\n\
## Mental model\n\
Think of it as a pipeline.\n\n\
## How to use it\n\
Call `{name}` in your code.\n\n\
## Under the hood\n\
It processes data.\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Don't forget to handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn canned_chapter_one_diagram(name: &str, chapter_num: usize) -> String {
        format!(
            "# Chapter {chapter_num}: {name}\n\n\
## Motivation\n\
- Need to handle {name}\n\n\
## Core idea\n\
{name} is a key concept.\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Start] --> A1[End]\n```\n\n\
## How to use it\n\
Call `{name}` in your code.\n\n\
## Under the hood\n\
It processes data.\n\n\
## Key files\n\
- src/main.rs\n\n\
## Connections\n\
- See [other](02_other.md)\n\n\
## Pitfalls\n\
- Don't forget to handle errors\n\n\
## Summary\n\
We learned about {name}.\n"
        )
    }

    fn canned_chapter_with_bad_mermaid(name: &str, chapter_num: usize) -> String {
        format!(
            "# Chapter {chapter_num}: {name}\n\n\
## Mental model\n\
```mermaid\nflowchart LR\n  A0[Hello \"world\"; #x] --> A1[End]\n```\n\n\
## Summary\n\
Done.\n"
        )
    }

    // --- diagram_quota_for_tier ---

    #[test]
    fn quota_all_nine_combinations() {
        assert_eq!(diagram_quota_for_tier(Tier::S, DiagramLevel::Minimal), 0);
        assert_eq!(diagram_quota_for_tier(Tier::S, DiagramLevel::Standard), 0);
        assert_eq!(diagram_quota_for_tier(Tier::S, DiagramLevel::Rich), 1);
        assert_eq!(diagram_quota_for_tier(Tier::M, DiagramLevel::Minimal), 0);
        assert_eq!(diagram_quota_for_tier(Tier::M, DiagramLevel::Standard), 1);
        assert_eq!(diagram_quota_for_tier(Tier::M, DiagramLevel::Rich), 2);
        assert_eq!(diagram_quota_for_tier(Tier::L, DiagramLevel::Minimal), 1);
        assert_eq!(diagram_quota_for_tier(Tier::L, DiagramLevel::Standard), 2);
        assert_eq!(diagram_quota_for_tier(Tier::L, DiagramLevel::Rich), 3);
    }

    // --- count_mermaid_blocks ---

    #[test]
    fn count_zero_blocks() {
        assert_eq!(count_mermaid_blocks("no blocks here"), 0);
    }

    #[test]
    fn count_one_block() {
        let md = "text\n```mermaid\nflowchart LR\n  A --> B\n```\nmore";
        assert_eq!(count_mermaid_blocks(md), 1);
    }

    #[test]
    fn count_three_blocks() {
        let md =
            "```mermaid\na\n```\n```\nnot mermaid\n```\n```mermaid\nb\n```\n```mermaid\nc\n```";
        assert_eq!(count_mermaid_blocks(md), 3);
    }

    // --- extract_chapter_summary ---

    #[test]
    fn summary_extracts_headings_and_bullets() {
        let md = "# Chapter 1: Router\n\n## Motivation\n\
- Need routing\n\
- Real use case\n\n\
## Core idea\n\
Long paragraph about routing that should NOT appear in the summary \
because it is not a heading or a bullet point.\n\n\
## Summary\n\
We learned routing.\n";
        let summary = extract_chapter_summary(md);
        assert!(summary.contains("# Chapter 1: Router"), "{summary}");
        assert!(summary.contains("## Motivation"), "{summary}");
        assert!(summary.contains("## Core idea"), "{summary}");
        assert!(summary.contains("## Summary"), "{summary}");
        assert!(summary.contains("- Need routing"), "{summary}");
        assert!(summary.contains("- Real use case"), "{summary}");
        assert!(
            !summary.contains("Long paragraph about routing"),
            "summary should not contain full text: {summary}"
        );
    }

    #[test]
    fn summary_caps_at_two_bullets() {
        let md = "# Title\n\n- bullet 1\n- bullet 2\n- bullet 3\n- bullet 4\n";
        let summary = extract_chapter_summary(md);
        assert!(summary.contains("bullet 1"), "{summary}");
        assert!(summary.contains("bullet 2"), "{summary}");
        assert!(!summary.contains("bullet 3"), "{summary}");
        assert!(!summary.contains("bullet 4"), "{summary}");
    }

    #[test]
    fn summary_empty_markdown_returns_empty() {
        assert_eq!(extract_chapter_summary(""), "");
    }

    // --- select_chapter_file_context ---

    #[test]
    fn file_context_prefers_entry_files() {
        let mut a = Abstraction::new("Core", "desc", Tier::M, "module");
        a.entry_files = vec!["src/entry.rs".into()];
        a.file_indices = vec![0];
        let files = vec![
            ("src/other.rs".to_string(), "other content".to_string()),
            ("src/entry.rs".to_string(), "entry content".to_string()),
        ];
        let ctx = select_chapter_file_context(&a, &files, 100_000, 12_000);
        let entry_pos = ctx.find("src/entry.rs");
        let other_pos = ctx.find("src/other.rs");
        assert!(
            entry_pos < other_pos,
            "entry file should come first: entry_pos={entry_pos:?}, other_pos={other_pos:?}\n{ctx}"
        );
    }

    #[test]
    fn file_context_truncates_by_budget() {
        let mut a = Abstraction::new("Core", "desc", Tier::M, "module");
        a.entry_files = vec!["src/big.rs".into()];
        let big_content = "x".repeat(10_000);
        let files = vec![("src/big.rs".to_string(), big_content)];
        let ctx = select_chapter_file_context(&a, &files, 500, 12_000);
        assert!(
            ctx.chars().count() <= 600,
            "context should be truncated: {} chars",
            ctx.chars().count()
        );
    }

    #[test]
    fn file_context_uses_path_stubs_when_budget_exhausted() {
        let mut a = Abstraction::new("Core", "desc", Tier::M, "module");
        a.entry_files = vec!["src/first.rs".into()];
        a.file_indices = vec![1];
        let files = vec![
            ("src/first.rs".to_string(), "x".repeat(100)),
            ("src/second.rs".to_string(), "y".repeat(100)),
        ];
        let ctx = select_chapter_file_context(&a, &files, 150, 12_000);
        assert!(ctx.contains("# File: src/first.rs"), "{ctx}");
        assert!(ctx.contains("path-only: src/second.rs"), "{ctx}");
    }

    #[test]
    fn file_context_empty_abstraction_returns_empty() {
        let a = Abstraction::new("Core", "desc", Tier::S, "module");
        let ctx = select_chapter_file_context(&a, &[], 100_000, 12_000);
        assert!(ctx.is_empty());
    }

    #[test]
    fn file_context_falls_back_to_file_indices() {
        let mut a = Abstraction::new("Core", "desc", Tier::S, "module");
        a.file_indices = vec![0];
        let files = vec![("src/core.rs".to_string(), "content".to_string())];
        let ctx = select_chapter_file_context(&a, &files, 100_000, 12_000);
        assert!(ctx.contains("src/core.rs"), "{ctx}");
        assert!(ctx.contains("content"), "{ctx}");
    }

    #[test]
    fn file_context_large_list_returns_correct_content() {
        // Build a list of 500 files; the target is in the middle.
        let target_path = "src/middle.rs";
        let target_content = "TARGET_CONTENT_MIDDLE";
        let mut files: Vec<(String, String)> = Vec::with_capacity(500);
        for i in 0..250 {
            files.push((format!("src/file_{i}.rs"), format!("content_{i}")));
        }
        files.push((target_path.to_string(), target_content.to_string()));
        for i in 251..501 {
            files.push((format!("src/file_{i}.rs"), format!("content_{i}")));
        }
        let mut a = Abstraction::new("Core", "desc", Tier::M, "module");
        a.entry_files = vec![target_path.to_string()];
        let ctx = select_chapter_file_context(&a, &files, 1_000_000, 12_000);
        assert!(
            ctx.contains(target_content),
            "expected target content in context:\n{ctx}"
        );
    }

    #[test]
    fn file_context_large_list_end_of_list() {
        // Worst-case for O(n) scan: the target file is the LAST entry.
        let target_path = "src/last.rs";
        let target_content = "TARGET_CONTENT_LAST";
        let mut files: Vec<(String, String)> = Vec::with_capacity(500);
        for i in 0..499 {
            files.push((format!("src/file_{i}.rs"), format!("content_{i}")));
        }
        files.push((target_path.to_string(), target_content.to_string()));
        let mut a = Abstraction::new("Core", "desc", Tier::M, "module");
        a.entry_files = vec![target_path.to_string()];
        let ctx = select_chapter_file_context(&a, &files, 1_000_000, 12_000);
        assert!(
            ctx.contains(target_content),
            "expected last-file content in context:\n{ctx}"
        );
    }

    // --- write_single_chapter ---

    #[tokio::test]
    async fn single_chapter_happy_path() {
        let client = MockClient::new(canned_chapter("Router", 1));
        let renderer = PromptRenderer::new().unwrap();
        let abs = &three_abstractions()[0];
        let chapter = write_single_chapter(
            &client,
            &renderer,
            abs,
            0,
            1,
            "None",
            "[Store](02_store.md)",
            "1. [Router](01_router.md)\n2. [Store](02_store.md)",
            "",
            "fn route() {}",
            "my-project",
            "",
            "English",
            DiagramLevel::Standard,
        )
        .await
        .expect("happy path should succeed");
        assert_eq!(chapter.chapter_num, 1);
        assert_eq!(chapter.abstraction_index, 0);
        assert_eq!(chapter.title, "Chapter 1: Router");
        assert_eq!(chapter.tier, Tier::M);
        assert!(chapter.markdown.contains("# Chapter 1: Router"));
        assert!(
            chapter.evidence_footer.contains("tier=M"),
            "footer: {}",
            chapter.evidence_footer
        );
        assert!(
            chapter.evidence_footer.contains("kind=module"),
            "footer: {}",
            chapter.evidence_footer
        );
        assert!(
            chapter.evidence_footer.contains("apps=web"),
            "footer: {}",
            chapter.evidence_footer
        );
        assert!(
            chapter
                .evidence_footer
                .contains("entry_files=src/router.rs"),
            "footer: {}",
            chapter.evidence_footer
        );
        assert_eq!(chapter.apps, vec!["web".to_string()]);
        assert_eq!(chapter.entry_files, vec!["src/router.rs".to_string()]);
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn single_chapter_empty_output_returns_error() {
        let client = MockClient::new("   ");
        let renderer = PromptRenderer::new().unwrap();
        let abs = &three_abstractions()[0];
        let err = write_single_chapter(
            &client,
            &renderer,
            abs,
            0,
            1,
            "None",
            "None",
            "",
            "",
            "",
            "p",
            "",
            "English",
            DiagramLevel::Standard,
        )
        .await
        .expect_err("empty output should error");
        assert!(matches!(err, ChaptersError::EmptyOutput), "got: {err:?}");
    }

    #[tokio::test]
    async fn single_chapter_llm_failure_propagates() {
        let client = MockClient::new(canned_chapter("Router", 1)).fail_on(0, LlmError::Timeout);
        let renderer = PromptRenderer::new().unwrap();
        let abs = &three_abstractions()[0];
        let err = write_single_chapter(
            &client,
            &renderer,
            abs,
            0,
            1,
            "None",
            "None",
            "",
            "",
            "",
            "p",
            "",
            "English",
            DiagramLevel::Standard,
        )
        .await
        .expect_err("llm failure should propagate");
        assert!(
            matches!(err, ChaptersError::Llm(LlmError::Timeout)),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn single_chapter_sanitizes_mermaid() {
        let client = MockClient::new(canned_chapter_with_bad_mermaid("Test", 1));
        let renderer = PromptRenderer::new().unwrap();
        let abs = &three_abstractions()[0];
        let chapter = write_single_chapter(
            &client,
            &renderer,
            abs,
            0,
            1,
            "None",
            "None",
            "",
            "",
            "",
            "p",
            "",
            "English",
            DiagramLevel::Standard,
        )
        .await
        .expect("should succeed");
        let mermaid_start = chapter.markdown.find("```mermaid").unwrap();
        let after_open = &chapter.markdown[mermaid_start + 10..];
        let mermaid_end_rel = after_open.find("```").unwrap();
        let mermaid_block =
            &chapter.markdown[mermaid_start..mermaid_start + 10 + mermaid_end_rel + 3];
        assert!(
            !mermaid_block.contains('"'),
            "double quotes should be sanitized in mermaid: {mermaid_block}"
        );
        assert!(
            !mermaid_block.contains('#'),
            "raw # should be sanitized in mermaid: {mermaid_block}"
        );
        assert!(
            !mermaid_block.contains(';'),
            "raw ; should be sanitized in mermaid: {mermaid_block}"
        );
    }

    #[tokio::test]
    async fn single_chapter_redacts_secrets() {
        struct CapturingClient {
            captured: Arc<std::sync::Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                *self.captured.lock().unwrap() = prompt.to_string();
                Ok(canned_chapter("Router", 1))
            }
        }
        let captured: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let abs = &three_abstractions()[0];
        let _ = write_single_chapter(
            &client,
            &renderer,
            abs,
            0,
            1,
            "None",
            "None",
            "",
            "",
            "DB_KEY=super-secret\nfn route() {}",
            "p",
            "",
            "English",
            DiagramLevel::Standard,
        )
        .await
        .expect("should succeed");
        let prompt = captured.lock().unwrap().clone();
        assert!(
            !prompt.contains("super-secret"),
            "secret leaked into prompt: {prompt}"
        );
        assert!(
            prompt.contains("DB_KEY=****"),
            "secret not redacted in prompt: {prompt}"
        );
    }

    // --- write_chapters ---

    #[tokio::test]
    async fn happy_path_three_chapters() {
        let responses = vec![
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let files = file_contents_for();
        let config = sample_config();
        let result = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect("happy path should succeed");
        assert_eq!(result.chapters.len(), 3);
        assert_eq!(result.chapters[0].chapter_num, 1);
        assert_eq!(result.chapters[0].title, "Chapter 1: Router");
        assert_eq!(result.chapters[1].chapter_num, 2);
        assert_eq!(result.chapters[1].title, "Chapter 2: Store");
        assert_eq!(result.chapters[2].chapter_num, 3);
        assert_eq!(result.chapters[2].title, "Chapter 3: Worker");
        for ch in &result.chapters {
            assert!(
                ch.evidence_footer.contains("tier="),
                "missing evidence footer: {}",
                ch.evidence_footer
            );
        }
        assert_eq!(client.call_count(), 3);
    }

    #[tokio::test]
    async fn chapters_in_order_respect_chapter_order() {
        let responses = vec![
            canned_chapter("Store", 1),
            canned_chapter("Router", 2),
            canned_chapter("Worker", 3),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![1, 0, 2]);
        let files = file_contents_for();
        let config = sample_config();
        let result = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect("should succeed");
        assert_eq!(result.chapters[0].abstraction_index, 1);
        assert_eq!(result.chapters[0].chapter_num, 1);
        assert_eq!(result.chapters[1].abstraction_index, 0);
        assert_eq!(result.chapters[1].chapter_num, 2);
        assert_eq!(result.chapters[2].abstraction_index, 2);
        assert_eq!(result.chapters[2].chapter_num, 3);
    }

    #[tokio::test]
    async fn budget_exceeded_returns_budget_error() {
        let client = MockClient::new(canned_chapter("Router", 1));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let files = file_contents_for();
        let config = sample_config();
        let mut progress = ProgressTracker::new(0);
        let err = write_chapters(
            &client,
            &renderer,
            &identify,
            &order,
            &files,
            &config,
            Some(&mut progress),
        )
        .await
        .expect_err("budget exceeded should error");
        assert!(matches!(err, ChaptersError::Budget(_)), "got: {err:?}");
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn progress_tracker_records_calls() {
        let responses = vec![
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let files = file_contents_for();
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let _ = write_chapters(
            &client,
            &renderer,
            &identify,
            &order,
            &files,
            &config,
            Some(&mut progress),
        )
        .await
        .expect("should succeed");
        let snap = progress.snapshot();
        assert_eq!(snap.llm_calls_used, 3);
    }

    #[tokio::test]
    async fn empty_order_returns_empty_result() {
        let client = MockClient::new("anything");
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![]);
        let order = ChapterOrder::new(vec![]);
        let config = sample_config();
        let result = write_chapters(&client, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("empty order should succeed");
        assert!(result.chapters.is_empty());
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn malformed_llm_output_returns_empty_error() {
        let client = MockClient::new("   ");
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![0]);
        let files = file_contents_for();
        let config = sample_config();
        let err = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect_err("malformed output should error");
        assert!(matches!(err, ChaptersError::EmptyOutput), "got: {err:?}");
    }

    // --- diagram quota validation (warnings) ---

    #[tokio::test]
    async fn tier_s_zero_diagrams_ok_standard() {
        let client = MockClient::new(canned_chapter_no_diagrams("Store", 1));
        let renderer = PromptRenderer::new().unwrap();
        let mut abs = three_abstractions();
        abs[1].tier = Tier::S;
        let identify = IdentifyResult::new(vec![abs[1].clone()]);
        let order = ChapterOrder::new(vec![0]);
        let files = file_contents_for();
        let config = sample_config();
        let result = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect("should succeed");
        assert_eq!(result.chapters.len(), 1);
        assert_eq!(count_mermaid_blocks(&result.chapters[0].markdown), 0);
        assert_eq!(diagram_quota_for_tier(Tier::S, DiagramLevel::Standard), 0);
    }

    #[tokio::test]
    async fn tier_m_zero_diagrams_below_quota_standard() {
        let client = MockClient::new(canned_chapter_no_diagrams("Router", 1));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![three_abstractions()[0].clone()]);
        let order = ChapterOrder::new(vec![0]);
        let files = file_contents_for();
        let config = sample_config();
        let result = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect("should succeed (warning, not error)");
        assert_eq!(count_mermaid_blocks(&result.chapters[0].markdown), 0);
        assert!(
            diagram_quota_for_tier(Tier::M, DiagramLevel::Standard) > 0,
            "M standard should require > 0"
        );
    }

    #[tokio::test]
    async fn tier_l_one_diagram_ok_minimal() {
        let client = MockClient::new(canned_chapter_one_diagram("Worker", 1));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![three_abstractions()[2].clone()]);
        let order = ChapterOrder::new(vec![0]);
        let files = file_contents_for();
        let mut config = sample_config();
        config.diagram_level = DiagramLevel::Minimal;
        let result = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect("should succeed");
        assert_eq!(count_mermaid_blocks(&result.chapters[0].markdown), 1);
        assert_eq!(diagram_quota_for_tier(Tier::L, DiagramLevel::Minimal), 1);
    }

    #[tokio::test]
    async fn tier_l_zero_diagrams_below_quota_minimal() {
        let client = MockClient::new(canned_chapter_no_diagrams("Worker", 1));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![three_abstractions()[2].clone()]);
        let order = ChapterOrder::new(vec![0]);
        let files = file_contents_for();
        let mut config = sample_config();
        config.diagram_level = DiagramLevel::Minimal;
        let result = write_chapters(&client, &renderer, &identify, &order, &files, &config, None)
            .await
            .expect("should succeed (warning, not error)");
        assert_eq!(count_mermaid_blocks(&result.chapters[0].markdown), 0);
        assert!(diagram_quota_for_tier(Tier::L, DiagramLevel::Minimal) > 0);
    }

    // --- previous chapter summary ---

    #[tokio::test]
    async fn previous_chapter_summary_passed_not_full_text() {
        struct CapturingClient {
            captured: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                self.captured.lock().unwrap().push(prompt.to_string());
                Ok(canned_chapter("Next", 2))
            }
        }
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![
            Abstraction::new("First", "desc1", Tier::S, "module"),
            Abstraction::new("Second", "desc2", Tier::S, "module"),
        ]);
        let order = ChapterOrder::new(vec![0, 1]);
        let mut config = sample_config();
        config.max_concurrency = 1;
        let result = write_chapters(&client, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("should succeed");
        assert_eq!(result.chapters.len(), 2);
        let prompts = captured.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        let second_prompt = &prompts[1];
        assert!(
            second_prompt.contains("# Chapter 1: First") || second_prompt.contains("## Motivation"),
            "second prompt should contain summary of first chapter: {second_prompt}"
        );
        assert!(
            !second_prompt.contains("Long paragraph"),
            "second prompt should not contain full first chapter text"
        );
    }

    // --- bounded concurrency ---

    struct ConcurrencyTracker {
        current: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl ConcurrencyTracker {
        fn new(delay: Duration) -> Self {
            Self {
                current: Arc::new(AtomicUsize::new(0)),
                max_seen: Arc::new(AtomicUsize::new(0)),
                calls: Arc::new(AtomicUsize::new(0)),
                delay,
            }
        }

        fn max_concurrent(&self) -> usize {
            self.max_seen.load(Ordering::SeqCst)
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for ConcurrencyTracker {
        async fn complete(&self, _prompt: &str) -> Result<String, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(canned_chapter("Test", 1))
        }
    }

    #[tokio::test]
    async fn bounded_concurrency_limits_parallelism() {
        let tracker = ConcurrencyTracker::new(Duration::from_millis(50));
        let renderer = PromptRenderer::new().unwrap();
        let abstractions: Vec<Abstraction> = (0..5)
            .map(|i| Abstraction::new(format!("Abs{i}"), "desc", Tier::S, "module"))
            .collect();
        let identify = IdentifyResult::new(abstractions);
        let order = ChapterOrder::new(vec![0, 1, 2, 3, 4]);
        let mut config = sample_config();
        config.max_concurrency = 2;
        let result = write_chapters(&tracker, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("should succeed");
        assert_eq!(result.chapters.len(), 5);
        assert_eq!(
            tracker.max_concurrent(),
            2,
            "max concurrent calls should not exceed the limit"
        );
    }

    #[tokio::test]
    async fn twenty_chapters_concurrent_no_deadlock() {
        // Regression test for M5-PERF-4: 20-chapter concurrent generation must
        // complete without deadlock or excessive contention when using RwLock
        // for the shared completed-chapters vector.
        let tracker = ConcurrencyTracker::new(Duration::from_millis(10));
        let renderer = PromptRenderer::new().unwrap();
        let abstractions: Vec<Abstraction> = (0..20)
            .map(|i| Abstraction::new(format!("Abs{i}"), "desc", Tier::S, "module"))
            .collect();
        let identify = IdentifyResult::new(abstractions);
        let order = ChapterOrder::new((0..20).collect());
        let mut config = sample_config();
        config.max_concurrency = 8;
        let result = write_chapters(&tracker, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("20-chapter concurrent generation should succeed without deadlock");
        assert_eq!(result.chapters.len(), 20);
        // Chapters should be sorted by chapter_num.
        for (i, ch) in result.chapters.iter().enumerate() {
            assert_eq!(ch.chapter_num, i + 1, "chapters should be in order");
        }
        assert_eq!(tracker.call_count(), 20);
    }

    // --- channel-based collection (M6-HARD-3) ---

    #[tokio::test]
    async fn eight_chapters_high_concurrency_all_collected() {
        // Verify that 8 chapters generated with concurrency=8 are all
        // collected via the mpsc channel without deadlock or loss.
        let tracker = ConcurrencyTracker::new(Duration::from_millis(5));
        let renderer = PromptRenderer::new().unwrap();
        let abstractions: Vec<Abstraction> = (0..8)
            .map(|i| Abstraction::new(format!("Abs{i}"), "desc", Tier::S, "module"))
            .collect();
        let identify = IdentifyResult::new(abstractions);
        let order = ChapterOrder::new((0..8).collect());
        let mut config = sample_config();
        config.max_concurrency = 8;
        let result = write_chapters(&tracker, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("8-chapter high-concurrency generation should succeed");
        assert_eq!(
            result.chapters.len(),
            8,
            "all 8 chapters should be collected"
        );
        for (i, ch) in result.chapters.iter().enumerate() {
            assert_eq!(
                ch.chapter_num,
                i + 1,
                "chapters should be sorted by chapter_num"
            );
        }
        assert_eq!(tracker.call_count(), 8);
    }

    #[tokio::test]
    async fn sequential_summaries_include_all_previous_chapters() {
        // With concurrency=1, chapter 3's prompt should contain summaries
        // of BOTH chapter 1 and chapter 2 (not just the immediately
        // preceding one). This verifies the summary store accumulates
        // correctly across chapters.
        struct CapturingClient {
            captured: Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl LlmClient for CapturingClient {
            async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                let idx = {
                    let mut caps = self.captured.lock().unwrap();
                    caps.push(prompt.to_string());
                    caps.len()
                };
                Ok(canned_chapter(&format!("Abs{}", idx - 1), idx))
            }
        }
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = CapturingClient {
            captured: captured.clone(),
        };
        let renderer = PromptRenderer::new().unwrap();
        let abstractions: Vec<Abstraction> = (0..3)
            .map(|i| Abstraction::new(format!("Abs{i}"), "desc", Tier::S, "module"))
            .collect();
        let identify = IdentifyResult::new(abstractions);
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let mut config = sample_config();
        config.max_concurrency = 1;
        let result = write_chapters(&client, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("should succeed");
        assert_eq!(result.chapters.len(), 3);
        let prompts = captured.lock().unwrap();
        assert_eq!(prompts.len(), 3, "should have 3 prompts");
        let third_prompt = &prompts[2];
        // Third chapter should have summaries of both chapter 1 and chapter 2.
        assert!(
            third_prompt.contains("# Chapter 1: Abs0"),
            "third prompt should contain summary of chapter 1: {third_prompt}"
        );
        assert!(
            third_prompt.contains("# Chapter 2: Abs1"),
            "third prompt should contain summary of chapter 2: {third_prompt}"
        );
    }

    #[tokio::test]
    async fn all_chapters_collected_and_sorted_after_generation() {
        // Verify that all chapters are collected and sorted after generation,
        // even with moderate concurrency. This confirms the channel-based
        // collection does not drop results.
        let responses: Vec<String> = (0..6)
            .map(|i| canned_chapter(&format!("Abs{i}"), i + 1))
            .collect();
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let abstractions: Vec<Abstraction> = (0..6)
            .map(|i| Abstraction::new(format!("Abs{i}"), "desc", Tier::S, "module"))
            .collect();
        let identify = IdentifyResult::new(abstractions);
        let order = ChapterOrder::new((0..6).collect());
        let mut config = sample_config();
        config.max_concurrency = 3;
        let result = write_chapters(&client, &renderer, &identify, &order, &[], &config, None)
            .await
            .expect("should succeed");
        assert_eq!(
            result.chapters.len(),
            6,
            "all 6 chapters should be collected"
        );
        for (i, ch) in result.chapters.iter().enumerate() {
            assert_eq!(
                ch.chapter_num,
                i + 1,
                "chapters should be sorted by chapter_num"
            );
        }
        assert_eq!(client.call_count(), 6);
    }

    // --- checkpoint integration ---

    #[tokio::test]
    async fn checkpoint_writes_files_and_marks_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);
        let responses = vec![
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
        ];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let files = file_contents_for();
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = chapters_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &order,
            &files,
            &config,
            Some(&mut progress),
        )
        .await
        .expect("should succeed");
        assert_eq!(result.chapters.len(), 3);
        assert!(cp.is_stage_complete(StageId::Chapters));
        assert!(dir.join("chapters").is_dir());
        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store.read_chapters(&dir, &loaded_cp).unwrap();
        assert_eq!(loaded.chapters.len(), 3);
        assert_eq!(loaded.chapters[0].title, "Chapter 1: Router");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_skips_completed_chapters() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let existing_chapters = ChapterResult::new(vec![
            Chapter {
                abstraction_index: 0,
                chapter_num: 1,
                title: "Chapter 1: Router".into(),
                markdown: canned_chapter("Router", 1),
                tier: Tier::M,
                kind: AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/router.rs".into()],
                evidence_footer: "tier=M | kind=module".into(),
            },
            Chapter {
                abstraction_index: 1,
                chapter_num: 2,
                title: "Chapter 2: Store".into(),
                markdown: canned_chapter("Store", 2),
                tier: Tier::S,
                kind: AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/store.rs".into()],
                evidence_footer: "tier=S | kind=module".into(),
            },
        ]);
        let entries = store.write_chapters(&dir, &existing_chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();

        let responses = vec![canned_chapter("Worker", 3)];
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(three_abstractions());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let files = file_contents_for();
        let config = sample_config();
        let mut progress = ProgressTracker::new(10);
        let result = chapters_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &order,
            &files,
            &config,
            Some(&mut progress),
        )
        .await
        .expect("resume should succeed");
        assert_eq!(result.chapters.len(), 3);
        assert_eq!(client.call_count(), 1, "only chapter 3 should be generated");
        assert_eq!(result.chapters[0].title, "Chapter 1: Router");
        assert_eq!(result.chapters[1].title, "Chapter 2: Store");
        assert_eq!(result.chapters[2].title, "Chapter 3: Worker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resume_all_complete_skips_generation() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let existing = ChapterResult::new(vec![
            Chapter::new(0, 1, "Chapter 1: A", "md1", Tier::S, "module", "f1"),
            Chapter::new(1, 2, "Chapter 2: B", "md2", Tier::M, "module", "f2"),
        ]);
        let entries = store.write_chapters(&dir, &existing).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "2026-07-24T00:06:00Z");
        let entries_clone = cp
            .stage_outputs
            .as_ref()
            .unwrap()
            .get(StageId::Chapters.as_str())
            .unwrap()
            .to_vec();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries_clone)
            .unwrap();

        let client = MockClient::new(canned_chapter("ShouldNotBeCalled", 99));
        let renderer = PromptRenderer::new().unwrap();
        let identify = IdentifyResult::new(vec![
            Abstraction::new("A", "d", Tier::S, "module"),
            Abstraction::new("B", "d", Tier::M, "module"),
        ]);
        let order = ChapterOrder::new(vec![0, 1]);
        let config = sample_config();
        let result = chapters_and_checkpoint(
            &client,
            &renderer,
            &store,
            &mut cp,
            &identify,
            &order,
            &[],
            &config,
            None,
        )
        .await
        .expect("should succeed");
        assert_eq!(client.call_count(), 0);
        assert_eq!(result.chapters.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- helper tests ---

    #[test]
    fn should_run_chapters_fresh_returns_true() {
        let cp = fresh_checkpoint();
        assert!(should_run_chapters(&cp));
    }

    #[test]
    fn should_run_chapters_complete_returns_false() {
        let mut cp = fresh_checkpoint();
        cp.mark_stage_complete(StageId::Chapters, "2026-07-24T00:06:00Z");
        assert!(!should_run_chapters(&cp));
    }

    #[test]
    fn diagram_level_round_trip() {
        for level in [
            DiagramLevel::Minimal,
            DiagramLevel::Standard,
            DiagramLevel::Rich,
        ] {
            assert_eq!(DiagramLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(DiagramLevel::parse("nope"), None);
    }

    #[test]
    fn extract_title_from_heading() {
        assert_eq!(
            extract_chapter_title("# Chapter 1: Router\n\nbody"),
            Some("Chapter 1: Router".to_string())
        );
    }

    #[test]
    fn extract_title_no_heading_returns_none() {
        assert_eq!(extract_chapter_title("just text"), None);
    }

    #[test]
    fn build_evidence_footer_format() {
        let a = three_abstractions()[0].clone();
        let footer = build_evidence_footer(&a);
        assert!(footer.contains("tier=M"), "{footer}");
        assert!(footer.contains("kind=module"), "{footer}");
        assert!(footer.contains("apps=web"), "{footer}");
        assert!(footer.contains("entry_files=src/router.rs"), "{footer}");
    }

    #[test]
    fn build_evidence_footer_empty_apps_and_files() {
        let a = Abstraction::new("X", "d", Tier::S, "function");
        let footer = build_evidence_footer(&a);
        assert!(footer.contains("apps=N/A"), "{footer}");
        assert!(footer.contains("entry_files=N/A"), "{footer}");
    }

    #[test]
    fn build_full_chapter_listing_format() {
        let abs = three_abstractions();
        let listing = build_full_chapter_listing(&abs, &[0, 1, 2]);
        assert!(listing.contains("1. [Router](01_router.md)"), "{listing}");
        assert!(listing.contains("2. [Store](02_store.md)"), "{listing}");
        assert!(listing.contains("3. [Worker](03_worker.md)"), "{listing}");
    }

    #[test]
    fn slugify_matches_checkpoint_store() {
        assert_eq!(slugify("Router"), "router");
        assert_eq!(slugify("Query Processing!"), "query-processing");
        assert_eq!(slugify("---"), "chapter");
    }

    #[test]
    fn now_iso8601_utc_is_valid_format() {
        let ts = now_iso8601_utc();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
    }

    // --- format_entry_list ---

    #[test]
    fn format_entry_list_three_files() {
        let files = vec![
            "src/router.rs".to_string(),
            "src/store.rs".to_string(),
            "src/worker.rs".to_string(),
        ];
        let result = format_entry_list(&files);
        assert_eq!(
            result, "- src/router.rs\n- src/store.rs\n- src/worker.rs",
            "entry list must match collect().join(\"\\n\") output"
        );
        assert!(!result.ends_with('\n'), "no trailing newline");
    }

    #[test]
    fn format_entry_list_single_file() {
        let files = vec!["main.rs".to_string()];
        let result = format_entry_list(&files);
        assert_eq!(result, "- main.rs");
    }

    #[test]
    fn format_entry_list_empty_returns_none_marker() {
        let files: Vec<String> = vec![];
        let result = format_entry_list(&files);
        assert_eq!(result, "(none)");
    }

    // --- format_apps_line ---

    #[test]
    fn format_apps_line_multiple_apps() {
        let apps = vec!["web".to_string(), "api".to_string(), "worker".to_string()];
        let result = format_apps_line(&apps);
        assert_eq!(
            result, "web, api, worker",
            "apps line must match join(\", \") output"
        );
    }

    #[test]
    fn format_apps_line_single_app() {
        let apps = vec!["web".to_string()];
        let result = format_apps_line(&apps);
        assert_eq!(result, "web");
    }

    #[test]
    fn format_apps_line_empty_returns_na() {
        let apps: Vec<String> = vec![];
        let result = format_apps_line(&apps);
        assert_eq!(result, "N/A");
    }

    // --- join_chapter_summaries ---

    #[test]
    fn join_chapter_summaries_multiple() {
        let summaries = ["Summary A", "Summary B", "Summary C"];
        let result = join_chapter_summaries(summaries.iter().copied());
        assert_eq!(
            result, "Summary A\n\nSummary B\n\nSummary C",
            "must match collect().join(\"\\n\\n\") output"
        );
    }

    #[test]
    fn join_chapter_summaries_single() {
        let summaries = ["Only one"];
        let result = join_chapter_summaries(summaries.iter().copied());
        assert_eq!(result, "Only one");
    }

    #[test]
    fn join_chapter_summaries_empty() {
        let summaries: [&str; 0] = [];
        let result = join_chapter_summaries(summaries.iter().copied());
        assert_eq!(result, "");
    }
}
