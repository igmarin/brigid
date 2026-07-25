//! CombineTutorial pipeline stage (M4-CMB-1): assemble the final `index.md`
//! with deterministic diagrams, i18n chrome strings, chapter list, and
//! sanitized Mermaid. Writes the final `output/` directory.
//!
//! This stage is **deterministic** — no LLM calls. It consumes the outputs of
//! the identify, relationships, order, chapters, setup, and overview stages and
//! assembles the final tutorial directory.
//!
//! # Flow
//!
//! 1. [`build_index_markdown`] (pure) assembles `index.md` from abstractions,
//!    relationships, chapter order, chapter content, setup/overview options,
//!    module inventory, and i18n chrome strings.
//! 2. [`combine_tutorial`] calls [`build_index_markdown`], then
//!    [`write_output_directory`] writes all files to the output directory.
//! 3. [`combine_and_checkpoint`] wraps the above with checkpoint save/load and
//!    resume semantics, writing `index.md` to the checkpoint directory and
//!    marking [`StageId::Combine`] complete.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use decon_core::{
    Abstraction, ArchitectureOverview, ChapterOrder, ChapterResult, CheckpointV1, ChromeStrings,
    CombinedTutorial, DiagramEdge, IdentifyResult, Locale, ModuleKey, Relationship,
    RelationshipsResult, SetupGuide, StageId, sanitize_markdown_mermaid_blocks, sanitize_mermaid,
    system_map_flowchart, validate_mermaid,
};
use thiserror::Error;

use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
use crate::resume;

/// Errors returned by the combine stage.
#[derive(Debug, Error)]
pub enum CombineError {
    /// Filesystem I/O failure while writing the output directory.
    #[error("output I/O at {path}: {source}")]
    Io {
        /// Path related to the failure.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A Mermaid block failed validation after sanitization.
    #[error("mermaid validation failed: {0}")]
    Mermaid(String),
    /// A checkpoint save/load failed during the combine stage.
    #[error("checkpoint error during combine: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
}

/// Build the chapter filename: `NN_<slug>.md` where `NN` is the zero-padded
/// 2-digit chapter number and `<slug>` is the kebab-case slug of the title
/// (max 50 chars).
///
/// Matches the naming convention used by
/// [`CheckpointStore::write_chapters`](crate::CheckpointStore::write_chapters).
///
/// # Examples
///
/// ```
/// use decon_pipeline::combine::slugify_chapter_filename;
///
/// assert_eq!(slugify_chapter_filename(1, "Authentication"), "01_authentication.md");
/// assert_eq!(
///     slugify_chapter_filename(2, "Order Processing Pipeline"),
///     "02_order-processing-pipeline.md"
/// );
/// ```
#[must_use]
pub fn slugify_chapter_filename(chapter_num: usize, title: &str) -> String {
    let slug = slugify(title);
    format!("{chapter_num:02}_{slug}.md")
}

/// Pure assembly of `index.md` content from pipeline outputs.
///
/// Builds all six sections from `docs/best-practices.md` §7.5:
///
/// 1. **How to use this tutorial** — i18n chrome heading + brief instructions
/// 2. **Module/app inventory** — list of modules (when monorepo, i.e. >1 module)
/// 3. **System map** — deterministic Mermaid flowchart (apps as nodes)
/// 4. **Core concepts map** — deterministic Mermaid flowchart (abstractions +
///    relationships)
/// 5. **Learning path** — deterministic Mermaid flowchart (ordered chapters)
/// 6. **Chapter list** — markdown links to chapter files (including
///    setup/overview when present)
///
/// All Mermaid blocks are sanitized via [`sanitize_mermaid`] and validated via
/// [`validate_mermaid`]. All headings use the provided [`ChromeStrings`].
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_index_markdown(
    abstractions: &[Abstraction],
    relationships: &[Relationship],
    order: &ChapterOrder,
    chapters: &ChapterResult,
    setup: Option<&SetupGuide>,
    overview: Option<&ArchitectureOverview>,
    modules: &[ModuleKey],
    chrome: &ChromeStrings,
) -> String {
    let mut md = String::new();

    md.push_str(&format!("# {}\n\n", chrome.how_to_use_heading));
    md.push_str("This tutorial walks through the core concepts of the codebase in a pedagogically ordered way. ");
    md.push_str(
        "Follow the chapters in order, or jump to the concept that interests you most.\n\n",
    );

    if modules.len() > 1 {
        md.push_str(&format!("## {}\n\n", chrome.system_map_heading));
        let edges = derive_cross_app_edges(abstractions, relationships);
        let diagram = system_map_flowchart(modules, &edges);
        md.push_str(&mermaid_fence(&diagram));
        md.push('\n');

        md.push_str(
            &modules
                .iter()
                .map(|m| format!("- {}", m.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        md.push_str("\n\n");
    }

    md.push_str(&format!("## {}\n\n", chrome.concept_map_heading));
    let concept_diagram = concept_map_flowchart(abstractions, relationships);
    md.push_str(&mermaid_fence(&concept_diagram));
    md.push('\n');

    md.push_str(&format!("## {}\n\n", chrome.learning_path_heading));
    let steps: Vec<&str> = ordered_chapter_titles(order, chapters, abstractions);
    let path_diagram = decon_core::learning_path_flowchart(&steps);
    md.push_str(&mermaid_fence(&path_diagram));
    md.push('\n');

    md.push_str(&format!("## {}\n\n", chrome.chapters_heading));
    if let Some(_setup) = setup {
        md.push_str(&format!("- [{}](00_setup.md)\n", chrome.setup_link));
    }
    if let Some(_overview) = overview {
        md.push_str(&format!(
            "- [{}](00_architecture_overview.md)\n",
            chrome.overview_link
        ));
    }
    for ch in &chapters.chapters {
        let filename = slugify_chapter_filename(ch.chapter_num, &ch.title);
        md.push_str(&format!("- [{}]({})\n", ch.title, filename));
    }

    md.push_str(&format!("\n---\n_{}_  \n", chrome.attribution));
    md
}

/// Write the final output directory: `index.md`, optional setup/overview, and
/// chapter files.
///
/// Creates the output directory if it doesn't exist.
///
/// # Errors
///
/// Returns [`CombineError::Io`] for filesystem failures.
pub fn write_output_directory(
    output_dir: &Path,
    combined: &CombinedTutorial,
    chapters: &ChapterResult,
    setup: Option<&SetupGuide>,
    overview: Option<&ArchitectureOverview>,
) -> Result<(), CombineError> {
    std::fs::create_dir_all(output_dir).map_err(|source| CombineError::Io {
        path: output_dir.to_path_buf(),
        source,
    })?;

    write_file(output_dir, "index.md", combined.index_markdown.as_bytes())?;

    if let Some(guide) = setup {
        write_file(output_dir, "00_setup.md", guide.markdown.as_bytes())?;
    }
    if let Some(ov) = overview {
        write_file(
            output_dir,
            "00_architecture_overview.md",
            ov.markdown.as_bytes(),
        )?;
    }

    for ch in &chapters.chapters {
        let filename = slugify_chapter_filename(ch.chapter_num, &ch.title);
        write_file(output_dir, &filename, ch.markdown.as_bytes())?;
    }

    Ok(())
}

/// Run the combine stage: build `index.md`, write the output directory, and
/// return the [`CombinedTutorial`] metadata.
///
/// This stage is **deterministic** — no LLM calls.
///
/// # Errors
///
/// Returns [`CombineError`] for filesystem I/O or Mermaid validation failures.
#[allow(clippy::too_many_arguments)]
pub fn combine_tutorial(
    identify: &IdentifyResult,
    relationships: &RelationshipsResult,
    order: &ChapterOrder,
    chapters: &ChapterResult,
    setup: Option<&SetupGuide>,
    overview: Option<&ArchitectureOverview>,
    modules: &[ModuleKey],
    locale: Locale,
    output_dir: &Path,
) -> Result<CombinedTutorial, CombineError> {
    let chrome = ChromeStrings::for_locale(locale);
    let index_markdown = build_index_markdown(
        &identify.abstractions,
        &relationships.relationships,
        order,
        chapters,
        setup,
        overview,
        modules,
        &chrome,
    );

    let sanitized = sanitize_markdown_mermaid_blocks(&index_markdown);

    let combined = CombinedTutorial::new(
        sanitized,
        chapters.chapters.len(),
        setup.is_some(),
        overview.is_some(),
        locale.as_str(),
    );

    write_output_directory(output_dir, &combined, chapters, setup, overview)?;

    Ok(combined)
}

/// Run the combine stage with checkpoint integration.
///
/// 1. Check [`resume::should_run`] for [`StageId::Combine`] — if `false`, load
///    and return the existing combined index from the checkpoint.
/// 2. Call [`combine_tutorial`].
/// 3. Write `index.md` to the checkpoint directory via
///    [`CheckpointStore::write_combined_index`].
/// 4. Record the stage output and mark [`StageId::Combine`] complete.
///
/// # Errors
///
/// Returns [`CombineError`] for filesystem I/O, Mermaid validation, or
/// checkpoint persistence failures.
#[allow(clippy::too_many_arguments)]
pub fn combine_and_checkpoint(
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    identify: &IdentifyResult,
    relationships: &RelationshipsResult,
    order: &ChapterOrder,
    chapters: &ChapterResult,
    setup: Option<&SetupGuide>,
    overview: Option<&ArchitectureOverview>,
    modules: &[ModuleKey],
    locale: Locale,
    output_dir: &Path,
) -> Result<CombinedTutorial, CombineError> {
    if !resume::should_run(StageId::Combine, checkpoint) {
        if let Some(existing) = store
            .read_combined_index(&store.dir, checkpoint)
            .map_err(CombineError::from)?
        {
            return Ok(existing);
        }
    }

    let combined = combine_tutorial(
        identify,
        relationships,
        order,
        chapters,
        setup,
        overview,
        modules,
        locale,
        output_dir,
    )?;

    let entry = store.write_combined_index(&store.dir, &combined)?;
    store.record_stage_outputs(checkpoint, StageId::Combine, vec![entry])?;

    let (mut loaded, files) = store.load()?;
    loaded.mark_stage_complete(StageId::Combine, now_iso8601_utc());
    store.save(loaded.clone(), &files)?;

    *checkpoint = loaded;

    Ok(combined)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

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

/// Wrap a Mermaid diagram body in a markdown fence.
fn mermaid_fence(body: &str) -> String {
    let mut out = String::from("```mermaid\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

/// Derive cross-app edges from relationships for the system map.
///
/// For each relationship, if the source and target abstractions belong to
/// different apps, emit a [`DiagramEdge`] between those app module keys.
fn derive_cross_app_edges(
    abstractions: &[Abstraction],
    relationships: &[Relationship],
) -> Vec<DiagramEdge> {
    let mut edges = Vec::new();
    for rel in relationships {
        let Some(from_abs) = abstractions.get(rel.from) else {
            continue;
        };
        let Some(to_abs) = abstractions.get(rel.to) else {
            continue;
        };
        let Some(from_app) = from_abs.apps.first() else {
            continue;
        };
        let Some(to_app) = to_abs.apps.first() else {
            continue;
        };
        if from_app != to_app {
            edges.push(DiagramEdge {
                from: from_app.clone(),
                to: to_app.clone(),
                label: Some(rel.kind.clone()),
            });
        }
    }
    edges
}

/// Build a deterministic concept-map flowchart from abstractions + relationships.
///
/// Nodes are abstractions (stable ids `C0`, `C1`, …). Edges are relationships
/// with sanitized labels. Output is sanitized and validated.
fn concept_map_flowchart(abstractions: &[Abstraction], relationships: &[Relationship]) -> String {
    use decon_core::mermaid::{sanitize_label, stable_node_id};

    let mut body = String::from("flowchart TD\n");
    for (i, abs) in abstractions.iter().enumerate() {
        let id = stable_node_id("C", i);
        let label = sanitize_label(&abs.name);
        let label = if label.is_empty() {
            format!("Concept {i}")
        } else {
            label
        };
        body.push_str(&format!("  {id}[{label}]\n"));
    }
    for rel in relationships {
        let from_id = stable_node_id("C", rel.from);
        let to_id = stable_node_id("C", rel.to);
        let label = sanitize_label(&rel.label);
        if label.is_empty() {
            body.push_str(&format!("  {from_id} --> {to_id}\n"));
        } else {
            body.push_str(&format!("  {from_id} -->|{label}| {to_id}\n"));
        }
    }
    if abstractions.is_empty() && relationships.is_empty() {
        body.push_str("  C0[No concepts]\n");
    }
    let sanitized = sanitize_mermaid(&body);
    if validate_mermaid(&sanitized).valid {
        sanitized
    } else {
        sanitize_mermaid("flowchart TD\n  C0[Concept map unavailable]\n")
    }
}

/// Return ordered chapter titles for the learning-path diagram.
fn ordered_chapter_titles<'a>(
    order: &ChapterOrder,
    chapters: &'a ChapterResult,
    abstractions: &'a [Abstraction],
) -> Vec<&'a str> {
    let mut titles = Vec::with_capacity(order.ordered_indices.len());
    for &idx in &order.ordered_indices {
        let ch = chapters
            .chapters
            .iter()
            .find(|c| c.abstraction_index == idx);
        if let Some(ch) = ch {
            titles.push(ch.title.as_str());
        } else if let Some(abs) = abstractions.get(idx) {
            titles.push(abs.name.as_str());
        }
    }
    titles
}

/// Write a file inside `dir`, creating parent directories as needed.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), CombineError> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CombineError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, bytes).map_err(|source| CombineError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(())
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
    use decon_core::{Chapter, RunConfig, Tier};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-combine-{n}-{seq}"));
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
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
        cp.mark_stage_complete(StageId::Relationships, "2026-07-24T00:04:00Z");
        cp.mark_stage_complete(StageId::Order, "2026-07-24T00:05:00Z");
        cp.mark_stage_complete(StageId::Chapters, "2026-07-24T00:06:00Z");
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.rs", b"fn b() {}")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn three_abstractions() -> Vec<Abstraction> {
        vec![
            Abstraction::new("Auth Service", "Handles authentication", Tier::M, "module"),
            Abstraction::new("Query Engine", "Processes queries", Tier::L, "class"),
            Abstraction::new(
                "Response Builder",
                "Builds HTTP responses",
                Tier::S,
                "function",
            ),
        ]
    }

    fn three_relationships() -> Vec<Relationship> {
        vec![
            Relationship::new(0, 1, "calls", "calls"),
            Relationship::new(1, 2, "feeds", "publishes"),
        ]
    }

    fn three_chapters() -> ChapterResult {
        ChapterResult::new(vec![
            Chapter::new(
                0,
                1,
                "Authentication",
                "# Authentication\n\n...",
                Tier::M,
                "module",
                "footer 0",
            ),
            Chapter::new(
                1,
                2,
                "Query Engine",
                "# Query Engine\n\n...",
                Tier::L,
                "class",
                "footer 1",
            ),
            Chapter::new(
                2,
                3,
                "Response Builder",
                "# Response Builder\n\n...",
                Tier::S,
                "function",
                "footer 2",
            ),
        ])
    }

    fn three_module_inventory() -> Vec<ModuleKey> {
        vec![
            ModuleKey::new("apps/api"),
            ModuleKey::new("apps/web"),
            ModuleKey::new("apps/worker"),
        ]
    }

    fn sample_setup() -> SetupGuide {
        SetupGuide::new(
            "# Setup Guide\n\nInstall Rust...",
            42,
            vec!["gap".into()],
            true,
        )
    }

    fn sample_overview() -> ArchitectureOverview {
        ArchitectureOverview::new(
            "# Architecture Overview\n\n...",
            three_module_inventory()
                .iter()
                .map(|m| m.as_str().to_owned())
                .collect(),
        )
    }

    // --- slugify_chapter_filename ---

    #[test]
    fn slugify_simple_title() {
        assert_eq!(
            slugify_chapter_filename(1, "Authentication"),
            "01_authentication.md"
        );
    }

    #[test]
    fn slugify_multi_word_title() {
        assert_eq!(
            slugify_chapter_filename(2, "Order Processing Pipeline"),
            "02_order-processing-pipeline.md"
        );
    }

    #[test]
    fn slugify_long_title_truncated_to_50_chars() {
        let long_title =
            "This Is A Very Long Chapter Title That Exceeds The Maximum Allowed Slug Length Limit";
        let filename = slugify_chapter_filename(3, long_title);
        assert!(filename.starts_with("03_"));
        assert!(filename.ends_with(".md"));
        let slug_part = &filename[3..filename.len() - 3];
        assert!(
            slug_part.len() <= 50,
            "slug '{slug_part}' is {} chars, max 50",
            slug_part.len()
        );
    }

    #[test]
    fn slugify_double_digit_chapter() {
        assert_eq!(
            slugify_chapter_filename(10, "Advanced Topics"),
            "10_advanced-topics.md"
        );
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(
            slugify_chapter_filename(1, "Auth & Sessions!"),
            "01_auth-sessions.md"
        );
    }

    // --- build_index_markdown happy path ---

    #[test]
    fn build_index_markdown_happy_path_all_six_sections() {
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let setup = sample_setup();
        let overview = sample_overview();
        let modules = three_module_inventory();
        let chrome = ChromeStrings::for_locale(Locale::En);

        let md = build_index_markdown(
            &abs,
            &rels,
            &order,
            &chapters,
            Some(&setup),
            Some(&overview),
            &modules,
            &chrome,
        );

        assert!(md.contains(&format!("# {}", chrome.how_to_use_heading)));
        assert!(md.contains(&format!("## {}", chrome.system_map_heading)));
        assert!(md.contains(&format!("## {}", chrome.concept_map_heading)));
        assert!(md.contains(&format!("## {}", chrome.learning_path_heading)));
        assert!(md.contains(&format!("## {}", chrome.chapters_heading)));
        assert!(md.contains(&format!("[{}](00_setup.md)", chrome.setup_link)));
        assert!(
            md.contains(&format!(
                "[{}](00_architecture_overview.md)",
                chrome.overview_link
            )),
            "overview link missing in: {md}"
        );
        assert!(md.contains("01_authentication.md"));
        assert!(md.contains("02_query-engine.md"));
        assert!(md.contains("03_response-builder.md"));
    }

    // --- single-app repo: no system map, no overview ---

    #[test]
    fn single_app_repo_no_system_map() {
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let modules = vec![ModuleKey::new("src")];
        let chrome = ChromeStrings::for_locale(Locale::En);

        let md = build_index_markdown(
            &abs, &rels, &order, &chapters, None, None, &modules, &chrome,
        );

        assert!(!md.contains(&format!("## {}", chrome.system_map_heading)));
        assert!(!md.contains("00_architecture_overview.md"));
        assert!(!md.contains("00_setup.md"));
    }

    // --- Spanish locale ---

    #[test]
    fn spanish_locale_chrome_strings() {
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let setup = sample_setup();
        let overview = sample_overview();
        let modules = three_module_inventory();
        let chrome = ChromeStrings::for_locale(Locale::Es);

        let md = build_index_markdown(
            &abs,
            &rels,
            &order,
            &chapters,
            Some(&setup),
            Some(&overview),
            &modules,
            &chrome,
        );

        assert!(md.contains("Capitulos"));
        assert!(md.contains("Guía de Configuración"));
        assert!(md.contains("Vista General de Arquitectura"));
        assert!(md.contains("Ruta de Aprendizaje"));
        assert!(md.contains("Mapa del Sistema"));
        assert!(md.contains("Conceptos Centrales"));
    }

    // --- mermaid sanitization ---

    #[test]
    fn all_mermaid_blocks_pass_validation() {
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let modules = three_module_inventory();
        let chrome = ChromeStrings::for_locale(Locale::En);

        let md = build_index_markdown(
            &abs, &rels, &order, &chapters, None, None, &modules, &chrome,
        );

        let mut in_mermaid = false;
        let mut block = String::new();
        let mut block_count = 0;
        for line in md.lines() {
            if line.trim() == "```mermaid" {
                in_mermaid = true;
                block.clear();
                continue;
            }
            if in_mermaid && line.trim() == "```" {
                in_mermaid = false;
                block_count += 1;
                let result = validate_mermaid(&block);
                assert!(
                    result.valid,
                    "mermaid block #{block_count} invalid: {:?}\n---\n{block}\n---",
                    result.issues
                );
                continue;
            }
            if in_mermaid {
                block.push_str(line);
                block.push('\n');
            }
        }
        assert!(
            block_count >= 3,
            "expected at least 3 mermaid blocks, got {block_count}"
        );
    }

    // --- internal links resolve ---

    #[test]
    fn chapter_links_point_to_actual_filenames() {
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let modules = three_module_inventory();
        let chrome = ChromeStrings::for_locale(Locale::En);

        let md = build_index_markdown(
            &abs, &rels, &order, &chapters, None, None, &modules, &chrome,
        );

        for ch in &chapters.chapters {
            let expected = slugify_chapter_filename(ch.chapter_num, &ch.title);
            assert!(md.contains(&expected), "index missing link to {expected}");
        }
    }

    // --- empty chapters ---

    #[test]
    fn empty_chapters_graceful_index() {
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = ChapterResult::new(Vec::new());
        let modules = three_module_inventory();
        let chrome = ChromeStrings::for_locale(Locale::En);

        let md = build_index_markdown(
            &abs, &rels, &order, &chapters, None, None, &modules, &chrome,
        );

        assert!(md.contains(&format!("## {}", chrome.chapters_heading)));
        assert!(!md.contains("01_"));
        assert!(!md.contains("02_"));
    }

    // --- write_output_directory ---

    #[test]
    fn write_output_directory_creates_all_files() {
        let dir = temp_dir();
        let output = dir.join("output");
        let abs = three_abstractions();
        let rels = three_relationships();
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let setup = sample_setup();
        let overview = sample_overview();
        let modules = three_module_inventory();
        let chrome = ChromeStrings::for_locale(Locale::En);

        let md = build_index_markdown(
            &abs,
            &rels,
            &order,
            &chapters,
            Some(&setup),
            Some(&overview),
            &modules,
            &chrome,
        );
        let combined = CombinedTutorial::new(&md, 3, true, true, "en");

        write_output_directory(&output, &combined, &chapters, Some(&setup), Some(&overview))
            .expect("write should succeed");

        assert!(output.join("index.md").is_file());
        assert!(output.join("00_setup.md").is_file());
        assert!(output.join("00_architecture_overview.md").is_file());
        assert!(output.join("01_authentication.md").is_file());
        assert!(output.join("02_query-engine.md").is_file());
        assert!(output.join("03_response-builder.md").is_file());

        let index_content = fs::read_to_string(output.join("index.md")).unwrap();
        assert!(index_content.contains(chrome.how_to_use_heading));

        let setup_content = fs::read_to_string(output.join("00_setup.md")).unwrap();
        assert!(setup_content.contains("# Setup Guide"));

        let ch1 = fs::read_to_string(output.join("01_authentication.md")).unwrap();
        assert!(ch1.contains("# Authentication"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_output_directory_creates_dir_if_missing() {
        let dir = temp_dir();
        let output = dir.join("nested").join("output");
        let chapters = three_chapters();
        let combined = CombinedTutorial::new("# Index", 3, false, false, "en");

        write_output_directory(&output, &combined, &chapters, None, None)
            .expect("should create nested dirs");

        assert!(output.join("index.md").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    // --- combine_tutorial integration ---

    #[test]
    fn combine_tutorial_writes_output_and_returns_metadata() {
        let dir = temp_dir();
        let output = dir.join("output");
        let identify = IdentifyResult::new(three_abstractions());
        let relationships = RelationshipsResult::new("A multi-app system.", three_relationships());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let setup = sample_setup();
        let overview = sample_overview();
        let modules = three_module_inventory();

        let combined = combine_tutorial(
            &identify,
            &relationships,
            &order,
            &chapters,
            Some(&setup),
            Some(&overview),
            &modules,
            Locale::En,
            &output,
        )
        .expect("combine should succeed");

        assert_eq!(combined.chapter_count, 3);
        assert!(combined.has_setup_guide);
        assert!(combined.has_architecture_overview);
        assert_eq!(combined.locale, "en");
        assert!(output.join("index.md").is_file());
        assert!(output.join("01_authentication.md").is_file());

        let _ = fs::remove_dir_all(&dir);
    }

    // --- resume: skip if complete ---

    #[test]
    fn combine_and_checkpoint_skips_when_already_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let existing_md = "# Existing Index\n\n## Chapters\n";
        let existing = CombinedTutorial::new(existing_md, 3, true, true, "en");
        let entry = store.write_combined_index(&dir, &existing).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Combine, vec![entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Combine, "2026-07-24T00:07:00Z");
        {
            let (_, files) = store.load().unwrap();
            store.save(cp.clone(), &files).unwrap();
        }

        let identify = IdentifyResult::new(three_abstractions());
        let relationships = RelationshipsResult::new("summary", three_relationships());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let modules = three_module_inventory();
        let output = dir.join("output");

        let result = combine_and_checkpoint(
            &store,
            &mut cp,
            &identify,
            &relationships,
            &order,
            &chapters,
            None,
            None,
            &modules,
            Locale::En,
            &output,
        )
        .expect("should succeed");

        assert_eq!(result.index_markdown, existing_md);
        assert!(
            !output.join("index.md").exists(),
            "should not write output when skipping"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // --- checkpoint: writes index.md and marks complete ---

    #[test]
    fn combine_and_checkpoint_writes_index_and_marks_complete() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_store(&store);

        let identify = IdentifyResult::new(three_abstractions());
        let relationships = RelationshipsResult::new("A multi-app system.", three_relationships());
        let order = ChapterOrder::new(vec![0, 1, 2]);
        let chapters = three_chapters();
        let setup = sample_setup();
        let overview = sample_overview();
        let modules = three_module_inventory();
        let output = dir.join("output");

        let result = combine_and_checkpoint(
            &store,
            &mut cp,
            &identify,
            &relationships,
            &order,
            &chapters,
            Some(&setup),
            Some(&overview),
            &modules,
            Locale::En,
            &output,
        )
        .expect("should succeed");

        assert!(cp.is_stage_complete(StageId::Combine));
        assert!(
            dir.join("index.md").is_file(),
            "index.md should be in checkpoint dir"
        );
        let ckpt_index = fs::read_to_string(dir.join("index.md")).unwrap();
        assert_eq!(ckpt_index, result.index_markdown);

        let _ = fs::remove_dir_all(&dir);
    }

    // --- now_iso8601_utc format ---

    #[test]
    fn now_iso8601_utc_is_valid_format() {
        let ts = now_iso8601_utc();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
    }

    // --- CombineError display ---

    #[test]
    fn combine_error_mermaid_display() {
        let err = CombineError::Mermaid("bad block".to_owned());
        assert!(err.to_string().contains("mermaid validation failed"));
        assert!(err.to_string().contains("bad block"));
    }

    #[test]
    fn combine_error_io_display() {
        let err = CombineError::Io {
            path: PathBuf::from("/tmp/out"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        assert!(err.to_string().contains("/tmp/out"));
        assert!(err.to_string().contains("missing"));
    }
}
