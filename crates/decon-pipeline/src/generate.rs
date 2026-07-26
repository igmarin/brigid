//! Full pipeline orchestration for `decon generate` (M4-CLI-1).
//!
//! Runs the complete tutorial generation pipeline:
//! identify -> relationships -> order -> chapters -> setup -> overview -> combine.
//!
//! Each stage checks the checkpoint for completion and skips if already done,
//! enabling resume from any point. The orchestration logic lives here so the
//! CLI binary stays thin.

use std::fs;
use std::path::{Path, PathBuf};

use decon_core::{
    ChapterOrder, ChapterResult, CheckpointV1, CombinedTutorial, IdentifyResult, Locale, ModuleKey,
    ProgressTracker, RelationshipsResult, RunConfig, StageId, config_hash,
};
use decon_crawl::crawl_local;

use crate::cancellation::CancelToken;
use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError, records_from_files};
use crate::dry_run::dry_run;
use crate::prompts::PromptRenderer;

use crate::chapters::{ChaptersConfig, DiagramLevel, chapters_and_checkpoint};
use crate::combine::combine_and_checkpoint;
use crate::identify_checkpoint::{identify_and_checkpoint, should_run_identify};
use crate::order::{OrderConfig, order_and_checkpoint};
use crate::overview::{OverviewInput, overview_and_checkpoint, should_generate_overview};
use crate::relationships::{RelationshipsConfig, relationships_and_checkpoint};
use crate::setup_guide::{
    WriteSetupGuideInput, should_generate_setup, write_setup_guide_and_checkpoint,
};

use decon_llm::LlmClient;

/// Errors returned by the generate pipeline.
#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    /// Config / path / I/O input error (bad language, missing dir, etc.).
    #[error("config error: {0}")]
    Config(String),
    /// Crawl failure.
    #[error("crawl failed: {0}")]
    Crawl(String),
    /// Identify stage failure.
    #[error("identify stage failed: {0}")]
    Identify(#[from] crate::identify::IdentifyError),
    /// Relationships stage failure.
    #[error("relationships stage failed: {0}")]
    Relationships(#[from] crate::relationships::RelationshipsError),
    /// Order stage failure.
    #[error("order stage failed: {0}")]
    Order(#[from] crate::order::OrderError),
    /// Chapters stage failure.
    #[error("chapters stage failed: {0}")]
    Chapters(#[from] crate::chapters::ChaptersError),
    /// Chapter review pass failure.
    #[error("chapter review failed: {0}")]
    Review(#[from] crate::review::ReviewError),
    /// Setup guide stage failure.
    #[error("setup guide stage failed: {0}")]
    Setup(#[from] crate::setup_guide::SetupGuideError),
    /// Overview stage failure.
    #[error("overview stage failed: {0}")]
    Overview(#[from] crate::overview::OverviewError),
    /// Combine stage failure.
    #[error("combine stage failed: {0}")]
    Combine(#[from] crate::combine::CombineError),
    /// Checkpoint persistence failure.
    #[error("checkpoint error: {0}")]
    Checkpoint(#[from] CheckpointStoreError),
    /// Budget exhausted mid-pipeline. The checkpoint has been saved with all
    /// completed stages; resume to continue.
    #[error("budget exhausted: {0}")]
    Budget(#[from] decon_core::BudgetExceeded),
}

/// Outcome of [`run_generate`].
#[derive(Debug)]
pub enum GenerateOutcome {
    /// All stages completed; the combined tutorial was written to the output
    /// directory.
    Completed(CombinedTutorial),
    /// The pipeline was cancelled (Ctrl+C / SIGTERM). A partial checkpoint was
    /// saved at `checkpoint_path`. Resume to continue.
    Cancelled {
        /// Path to the checkpoint directory.
        checkpoint_path: PathBuf,
    },
}

/// Configuration for the generate pipeline, mapping to the CLI flags.
#[derive(Clone, Debug)]
pub struct GenerateConfig {
    /// Repository root.
    pub dir: PathBuf,
    /// Optional app/module scope keys (e.g. `apps/alpha`).
    pub apps: Vec<String>,
    /// Output language code (e.g. `"en"`, `"es"`).
    pub language: String,
    /// Diagram richness level.
    pub diagram_level: DiagramLevel,
    /// Force setup guide generation regardless of score.
    pub force_setup: bool,
    /// Skip setup guide generation.
    pub no_setup: bool,
    /// Skip architecture overview generation.
    pub no_overview: bool,
    /// Checkpoint directory.
    pub checkpoint_dir: PathBuf,
    /// Output directory for the final tutorial.
    pub output_dir: PathBuf,
    /// Maximum abstractions to identify.
    pub max_abstractions: usize,
    /// Use single-shot identify instead of map+reduce.
    pub single_shot: bool,
    /// Run the full pipeline once per discovered app/module, writing separate
    /// output directories and a summary index.
    pub each_app: bool,
    /// Merged run config (CLI > file > env > defaults).
    pub run_config: RunConfig,
    /// Maximum concurrent chapter writes.
    pub chapter_concurrency: usize,
    /// Run a second LLM pass to polish each chapter (doubles chapter LLM cost).
    pub review_chapters: bool,
}

/// Run the full generate pipeline with cancellation support.
///
/// Stages run in order: identify -> relationships -> order -> chapters ->
/// setup -> overview -> combine. Each stage checks the checkpoint and skips
/// if already complete.
///
/// # Errors
///
/// Returns [`GenerateError`] for stage failures, budget exhaustion, or
/// checkpoint errors. On cancellation, returns `Ok(GenerateOutcome::Cancelled)`.
///
/// # Panics
///
/// Never panics.
#[allow(clippy::too_many_arguments)]
pub async fn run_generate(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    progress: &mut ProgressTracker,
    cancel: &CancelToken,
    config: &GenerateConfig,
    file_contents: &[(String, String)],
    files: Vec<String>,
    sizes: Vec<u64>,
    setup_score: i32,
    setup_gaps: &[String],
    setup_context: &str,
    modules: &[ModuleKey],
) -> Result<GenerateOutcome, GenerateError> {
    let project_name = config
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let locale = Locale::parse_or_default(&config.language);
    let language_instruction = if config.language.is_empty() {
        String::new()
    } else {
        format!("Use {}", config.language)
    };

    // --- Stage 1: Identify ---
    if should_run_identify(
        checkpoint,
        &config_hash(&config.run_config)
            .map_err(|e| GenerateError::Config(format!("config hash: {e}")))?,
    ) {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("identify");
        identify_and_checkpoint(
            client,
            renderer,
            store,
            checkpoint,
            files,
            sizes,
            &config.run_config,
            progress,
        )
        .await?;
        progress.complete_stage();
    }

    let identify = load_identify(checkpoint)?;

    // --- Stage 2: Relationships ---
    if crate::relationships::should_run_relationships(checkpoint) {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("relationships");
        let rel_config = RelationshipsConfig {
            project_name: project_name.clone(),
            language_instruction: language_instruction.clone(),
            ..Default::default()
        };
        relationships_and_checkpoint(
            client,
            renderer,
            store,
            checkpoint,
            &identify,
            file_contents,
            &rel_config,
            Some(progress),
        )
        .await?;
        progress.complete_stage();
    }

    let relationships = load_relationships(checkpoint)?;

    // --- Stage 3: Order ---
    if crate::order::should_run_order(checkpoint) {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("order");
        let order_config = OrderConfig {
            project_name: project_name.clone(),
            language_instruction: language_instruction.clone(),
            ..Default::default()
        };
        order_and_checkpoint(
            client,
            renderer,
            store,
            checkpoint,
            &identify,
            &relationships,
            &order_config,
            Some(progress),
        )
        .await?;
        progress.complete_stage();
    }

    let order = load_order(checkpoint)?;

    // --- Stage 4: Chapters ---
    if crate::chapters::should_run_chapters(checkpoint) {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("chapters");
        let chapters_config = ChaptersConfig {
            project_name: project_name.clone(),
            language_instruction: language_instruction.clone(),
            lang: locale.as_str().to_string(),
            diagram_level: config.diagram_level,
            max_concurrency: config.chapter_concurrency,
            ..Default::default()
        };
        chapters_and_checkpoint(
            client,
            renderer,
            store,
            checkpoint,
            &identify,
            &order,
            file_contents,
            &chapters_config,
            Some(progress),
        )
        .await?;
        progress.complete_stage();
    }

    let chapters = load_chapters(store, checkpoint)?;

    // --- Stage 4b: Optional chapter review pass ---
    if config.review_chapters && !chapters.chapters.is_empty() {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        let mut chapters = chapters;
        let allowed_paths: Vec<String> = file_contents.iter().map(|(p, _)| p.clone()).collect();
        let diagram_level = config.diagram_level;
        let lang_str = locale.as_str().to_string();
        let review_result = crate::review::review_chapters(
            &mut chapters,
            client,
            renderer,
            Some(progress),
            cancel,
            &lang_str,
            move |ch: &decon_core::Chapter| {
                crate::chapters::diagram_quota_for_tier(ch.tier, diagram_level)
            },
            &allowed_paths,
            config.chapter_concurrency,
        )
        .await;
        // Always write the (possibly partially) reviewed chapters to the
        // checkpoint so that progress is not lost on budget exhaustion or
        // cancellation.
        let entries = store.write_chapters(&store.dir, &chapters)?;
        store.record_stage_outputs(checkpoint, StageId::Chapters, entries)?;
        let summary = review_result?;
        eprintln!(
            "review: {} reviewed, {} kept original, {} warnings",
            summary.reviewed,
            summary.kept_original,
            summary.warnings.len()
        );
        for w in &summary.warnings {
            eprintln!("review warning: {w}");
        }
    }

    let chapters = load_chapters(store, checkpoint)?;

    // --- Stage 5: Setup ---
    let mut setup: Option<decon_core::SetupGuide> = None;
    let do_setup =
        should_generate_setup(setup_score, setup_gaps, config.force_setup, config.no_setup)
            && !checkpoint.is_stage_complete(StageId::Setup);

    if do_setup {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("setup");
        let input = WriteSetupGuideInput {
            project_name: &project_name,
            score: setup_score,
            gaps: setup_gaps,
            context: setup_context,
            lang: locale.as_str(),
            forced: config.force_setup,
        };
        let guide =
            write_setup_guide_and_checkpoint(client, renderer, store, checkpoint, &input).await?;
        setup = Some(guide);
        progress.complete_stage();
    } else if checkpoint.is_stage_complete(StageId::Setup) {
        setup = store
            .read_setup_guide(&store.dir, checkpoint)
            .ok()
            .flatten();
    }

    // --- Stage 6: Overview ---
    let mut overview: Option<decon_core::ArchitectureOverview> = None;
    let do_overview = !config.no_overview
        && should_generate_overview(modules)
        && !checkpoint.is_stage_complete(StageId::Overview);

    if do_overview {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("overview");
        let input = OverviewInput {
            project_name: project_name.clone(),
            summary: relationships.project_summary.clone(),
            inventory: modules.to_vec(),
            abstractions: identify.abstractions.clone(),
            relationships: relationships.relationships.clone(),
            lang_note: language_instruction.clone(),
            strict_app_validation: true,
        };
        let ov = overview_and_checkpoint(client, renderer, store, checkpoint, &input).await?;
        overview = Some(ov);
        progress.complete_stage();
    } else if checkpoint.is_stage_complete(StageId::Overview) {
        overview = store
            .read_architecture_overview(&store.dir, checkpoint)
            .ok()
            .flatten();
    }

    // --- Stage 7: Combine ---
    if crate::resume::should_run(StageId::Combine, checkpoint) {
        if cancel.is_cancelled() {
            return Ok(GenerateOutcome::Cancelled {
                checkpoint_path: config.checkpoint_dir.clone(),
            });
        }
        progress.set_stage("combine");
        let combined = combine_and_checkpoint(
            store,
            checkpoint,
            &identify,
            &relationships,
            &order,
            &chapters,
            setup.as_ref(),
            overview.as_ref(),
            modules,
            locale,
            &config.output_dir,
        )?;
        progress.complete_stage();
        Ok(GenerateOutcome::Completed(combined))
    } else {
        let combined = store
            .read_combined_index(&store.dir, checkpoint)
            .map_err(GenerateError::from)?
            .ok_or_else(|| {
                GenerateError::Config(
                    "combine stage marked complete but no index found in checkpoint".to_string(),
                )
            })?;
        write_output_if_needed(
            &config.output_dir,
            &combined,
            &chapters,
            setup.as_ref(),
            overview.as_ref(),
        )?;
        Ok(GenerateOutcome::Completed(combined))
    }
}

fn write_output_if_needed(
    output_dir: &Path,
    combined: &CombinedTutorial,
    chapters: &ChapterResult,
    setup: Option<&decon_core::SetupGuide>,
    overview: Option<&decon_core::ArchitectureOverview>,
) -> Result<(), GenerateError> {
    if output_dir.join("index.md").is_file() {
        return Ok(());
    }
    crate::combine::write_output_directory(output_dir, combined, chapters, setup, overview)?;
    Ok(())
}

/// Summary of one app's generate run within an `--each-app` batch.
#[derive(Clone, Debug)]
pub struct EachAppSummary {
    /// Module key (e.g. `apps/alpha`).
    pub app: String,
    /// Slugified directory name (e.g. `apps-alpha`).
    pub slug: String,
    /// Output directory for this app's tutorial.
    pub output_dir: PathBuf,
    /// Whether the pipeline completed successfully for this app.
    pub success: bool,
    /// Error message if the run failed.
    pub error: Option<String>,
}

/// Outcome of [`run_generate_each_app`].
#[derive(Debug)]
pub enum EachAppOutcome {
    /// All per-app runs finished (some may have failed; check each summary).
    Completed(Vec<EachAppSummary>),
    /// The batch was cancelled. `summaries` holds results for apps that
    /// ran before the cancellation; `cancelled_app` is the module key that
    /// was about to run.
    Partial {
        /// Results for apps that ran before cancellation.
        summaries: Vec<EachAppSummary>,
        /// Module key of the app that was cancelled.
        cancelled_app: String,
    },
}

/// Run the full generate pipeline once per discovered app/module.
///
/// 1. Runs a dry-run on the full repo to discover all modules.
/// 2. For each module, runs a scoped dry-run, sets up a scoped checkpoint
///    (`.decon-checkpoint-<slug>`), and calls [`run_generate`] with a scoped
///    `GenerateConfig` (output goes to `output/<slug>/`).
/// 3. If one app fails, continues with the remaining apps and records the
///    failure in the summary.
/// 4. If the cancel token fires, stops immediately and returns
///    [`EachAppOutcome::Partial`].
/// 5. Writes a summary `output/index.md` listing each app with a link to its
///    tutorial.
///
/// # Errors
///
/// Returns [`GenerateError`] only if the initial dry-run or crawl fails.
/// Per-app failures are captured in the returned summaries, not propagated.
pub async fn run_generate_each_app(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    cancel: &CancelToken,
    config: &GenerateConfig,
) -> Result<EachAppOutcome, GenerateError> {
    let full_plan = dry_run(&config.dir, None)
        .map_err(|e| GenerateError::Config(format!("each-app dry-run failed: {e}")))?;

    let modules: Vec<ModuleKey> = full_plan.modules.iter().map(|m| m.key.clone()).collect();

    if modules.is_empty() {
        write_each_app_index(&config.output_dir, &[])?;
        return Ok(EachAppOutcome::Completed(Vec::new()));
    }

    let crawl = crawl_local(&config.dir).map_err(|e| GenerateError::Crawl(e.to_string()))?;
    let size_map: std::collections::HashMap<&str, u64> = crawl
        .files
        .iter()
        .zip(crawl.sizes.iter())
        .map(|(f, s)| (f.as_str(), *s))
        .collect();

    let mut summaries: Vec<EachAppSummary> = Vec::new();

    for module in &modules {
        if cancel.is_cancelled() {
            return Ok(EachAppOutcome::Partial {
                summaries,
                cancelled_app: module.as_str().to_string(),
            });
        }

        let slug = slugify_module_key(module);
        let scoped_ckpt = PathBuf::from(format!("{}-{}", config.checkpoint_dir.display(), slug));
        let scoped_output = config.output_dir.join(&slug);

        let scoped_plan = match dry_run(&config.dir, Some(std::slice::from_ref(module))) {
            Ok(p) => p,
            Err(e) => {
                summaries.push(EachAppSummary {
                    app: module.as_str().to_string(),
                    slug: slug.clone(),
                    output_dir: scoped_output,
                    success: false,
                    error: Some(format!("scoped dry-run failed: {e}")),
                });
                continue;
            }
        };

        let scoped_sizes: Vec<u64> = scoped_plan
            .files
            .iter()
            .map(|f| size_map.get(f.as_str()).copied().unwrap_or(0))
            .collect();
        let scoped_file_contents: Vec<(String, String)> = scoped_plan
            .files
            .iter()
            .map(|f| (f.clone(), String::new()))
            .collect();
        let setup_context = scoped_plan
            .setup
            .config_files
            .iter()
            .map(|f| format!("# File: {f}\n"))
            .collect::<Vec<_>>()
            .join("\n");

        let store = CheckpointStore::new(&scoped_ckpt);
        let mut cp = match CheckpointV1::new_with_repo(
            &config.run_config,
            config.run_config.redacted_for_checkpoint(),
            config.dir.display().to_string(),
            "0Z",
            Some(config.dir.as_path()),
            config.run_config.since.clone(),
        ) {
            Ok(c) => c,
            Err(e) => {
                summaries.push(EachAppSummary {
                    app: module.as_str().to_string(),
                    slug: slug.clone(),
                    output_dir: scoped_output,
                    success: false,
                    error: Some(format!("checkpoint init: {e}")),
                });
                continue;
            }
        };
        cp.mark_stage_complete(StageId::Fetch, "0Z");
        cp.mark_stage_complete(StageId::DryRun, "0Z");

        let file_entries: Vec<(&str, &[u8])> = scoped_plan
            .files
            .iter()
            .map(|f| (f.as_str(), b"" as &[u8]))
            .collect();
        let records = records_from_files(&file_entries);
        if let Err(e) = store.save(cp.clone(), &records) {
            summaries.push(EachAppSummary {
                app: module.as_str().to_string(),
                slug: slug.clone(),
                output_dir: scoped_output,
                success: false,
                error: Some(format!("checkpoint save: {e}")),
            });
            continue;
        }

        let scoped_config = GenerateConfig {
            apps: vec![module.as_str().to_string()],
            checkpoint_dir: scoped_ckpt.clone(),
            output_dir: scoped_output.clone(),
            ..config.clone()
        };

        let mut progress = ProgressTracker::new(
            config
                .run_config
                .max_llm_calls
                .unwrap_or(decon_core::DEFAULT_MAX_LLM_CALLS),
        );

        let scoped_score = scoped_plan.setup.score;
        let scoped_gaps = scoped_plan.setup.gaps;
        let result = run_generate(
            client,
            renderer,
            &store,
            &mut cp,
            &mut progress,
            cancel,
            &scoped_config,
            &scoped_file_contents,
            scoped_plan.files,
            scoped_sizes,
            scoped_score,
            &scoped_gaps,
            &setup_context,
            &modules,
        )
        .await;

        match result {
            Ok(GenerateOutcome::Completed(_)) => {
                summaries.push(EachAppSummary {
                    app: module.as_str().to_string(),
                    slug: slug.clone(),
                    output_dir: scoped_output,
                    success: true,
                    error: None,
                });
            }
            Ok(GenerateOutcome::Cancelled { .. }) => {
                return Ok(EachAppOutcome::Partial {
                    summaries,
                    cancelled_app: module.as_str().to_string(),
                });
            }
            Err(e) => {
                summaries.push(EachAppSummary {
                    app: module.as_str().to_string(),
                    slug: slug.clone(),
                    output_dir: scoped_output,
                    success: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    write_each_app_index(&config.output_dir, &summaries)?;
    Ok(EachAppOutcome::Completed(summaries))
}

/// Convert a module key into a filesystem-safe slug (e.g. `apps/alpha` ->
/// `apps-alpha`).
fn slugify_module_key(key: &ModuleKey) -> String {
    key.as_str().replace('/', "-")
}

/// Write the summary `index.md` listing each per-app tutorial.
fn write_each_app_index(
    output_dir: &Path,
    summaries: &[EachAppSummary],
) -> Result<(), GenerateError> {
    fs::create_dir_all(output_dir)
        .map_err(|e| GenerateError::Config(format!("create output dir: {e}")))?;

    let mut content = String::from("# Tutorial Index\n\n");
    content.push_str("Per-app tutorials generated by `decon generate --each-app`.\n\n");

    if summaries.is_empty() {
        content.push_str("_No apps found._\n");
    } else {
        for s in summaries {
            let link = format!("{}/index.md", s.slug);
            let status = if s.success { "" } else { " (FAILED)" };
            content.push_str(&format!("- [{}]({}){status}\n", s.app, link));
        }
    }

    let index_path = output_dir.join("index.md");
    fs::write(&index_path, content)
        .map_err(|e| GenerateError::Config(format!("write index: {e}")))?;

    Ok(())
}

fn load_identify(checkpoint: &CheckpointV1) -> Result<IdentifyResult, GenerateError> {
    crate::identify_checkpoint::load_identify_result(checkpoint).ok_or_else(|| {
        GenerateError::Config(
            "identify stage marked complete but no abstractions found in checkpoint".to_string(),
        )
    })
}

fn load_relationships(checkpoint: &CheckpointV1) -> Result<RelationshipsResult, GenerateError> {
    crate::relationships::load_relationships_result(checkpoint).ok_or_else(|| {
        GenerateError::Config(
            "relationships stage marked complete but no result found in checkpoint".to_string(),
        )
    })
}

fn load_order(checkpoint: &CheckpointV1) -> Result<ChapterOrder, GenerateError> {
    crate::order::load_order_result(checkpoint).ok_or_else(|| {
        GenerateError::Config(
            "order stage marked complete but no order found in checkpoint".to_string(),
        )
    })
}

fn load_chapters(
    store: &CheckpointStore,
    checkpoint: &CheckpointV1,
) -> Result<ChapterResult, GenerateError> {
    store
        .read_chapters(&store.dir, checkpoint)
        .map_err(GenerateError::from)
}

fn prerequisite_error(stage: &str, prerequisite: &str) -> GenerateError {
    GenerateError::Config(format!(
        "{stage} stage requires '{prerequisite}' to be complete -- run 'decon {prerequisite}' or 'decon generate' first"
    ))
}

/// Run only the relationships stage for per-stage debugging.
///
/// Validates that the identify stage is complete in the checkpoint, loads the
/// identify result, and calls [`relationships_and_checkpoint`]. Returns the
/// [`RelationshipsResult`] and updates the checkpoint in place.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] if the identify stage is not complete.
/// Returns stage errors from [`relationships_and_checkpoint`].
pub async fn run_relationships_stage(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    file_contents: &[(String, String)],
    project_name: &str,
    language_instruction: &str,
) -> Result<RelationshipsResult, GenerateError> {
    if !checkpoint.is_stage_complete(StageId::Identify) {
        return Err(prerequisite_error("relationships", "identify"));
    }
    let identify = load_identify(checkpoint)?;
    let rel_config = RelationshipsConfig {
        project_name: project_name.to_string(),
        language_instruction: language_instruction.to_string(),
        ..Default::default()
    };
    let result = relationships_and_checkpoint(
        client,
        renderer,
        store,
        checkpoint,
        &identify,
        file_contents,
        &rel_config,
        None,
    )
    .await?;
    Ok(result)
}

/// Run only the order stage for per-stage debugging.
///
/// Validates that the relationships stage is complete, loads identify and
/// relationships results from the checkpoint, and calls
/// [`order_and_checkpoint`]. Returns the [`ChapterOrder`] and updates the
/// checkpoint in place.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] if the relationships stage is not
/// complete. Returns stage errors from [`order_and_checkpoint`].
pub async fn run_order_stage(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    project_name: &str,
    language_instruction: &str,
) -> Result<ChapterOrder, GenerateError> {
    if !checkpoint.is_stage_complete(StageId::Relationships) {
        return Err(prerequisite_error("order", "relationships"));
    }
    let identify = load_identify(checkpoint)?;
    let relationships = load_relationships(checkpoint)?;
    let order_config = OrderConfig {
        project_name: project_name.to_string(),
        language_instruction: language_instruction.to_string(),
        ..Default::default()
    };
    let result = order_and_checkpoint(
        client,
        renderer,
        store,
        checkpoint,
        &identify,
        &relationships,
        &order_config,
        None,
    )
    .await?;
    Ok(result)
}

/// Run only the chapters stage for per-stage debugging.
///
/// Validates that the order stage is complete, loads identify and order
/// results from the checkpoint, and calls [`chapters_and_checkpoint`]. Returns
/// the [`ChapterResult`] and updates the checkpoint in place.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] if the order stage is not complete.
/// Returns stage errors from [`chapters_and_checkpoint`].
#[allow(clippy::too_many_arguments)]
pub async fn run_chapters_stage(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    file_contents: &[(String, String)],
    project_name: &str,
    language_instruction: &str,
    lang: &str,
    diagram_level: DiagramLevel,
    chapter_concurrency: usize,
) -> Result<ChapterResult, GenerateError> {
    if !checkpoint.is_stage_complete(StageId::Order) {
        return Err(prerequisite_error("chapters", "order"));
    }
    let identify = load_identify(checkpoint)?;
    let order = load_order(checkpoint)?;
    let locale = Locale::parse_or_default(lang);
    let chapters_config = ChaptersConfig {
        project_name: project_name.to_string(),
        language_instruction: language_instruction.to_string(),
        lang: locale.as_str().to_string(),
        diagram_level,
        max_concurrency: chapter_concurrency,
        ..Default::default()
    };
    let result = chapters_and_checkpoint(
        client,
        renderer,
        store,
        checkpoint,
        &identify,
        &order,
        file_contents,
        &chapters_config,
        None,
    )
    .await?;
    Ok(result)
}

/// Run only the setup guide stage for per-stage debugging.
///
/// Validates that the identify stage is complete, runs a dry-run plan to
/// obtain the setup score and gaps, and calls
/// [`write_setup_guide_and_checkpoint`]. Returns the
/// [`decon_core::SetupGuide`] and updates the checkpoint in place.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] if the identify stage is not complete or
/// the dry-run fails. Returns stage errors from
/// [`write_setup_guide_and_checkpoint`].
pub async fn run_setup_stage(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    dir: &Path,
    forced: bool,
    lang: &str,
) -> Result<decon_core::SetupGuide, GenerateError> {
    if !checkpoint.is_stage_complete(StageId::Identify) {
        return Err(prerequisite_error("setup", "identify"));
    }
    let dry_run_plan = crate::dry_run::dry_run(dir, None)
        .map_err(|e| GenerateError::Config(format!("dry-run failed: {e}")))?;
    let project_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    let setup_context = dry_run_plan
        .setup
        .config_files
        .iter()
        .map(|f| format!("# File: {f}\n"))
        .collect::<Vec<_>>()
        .join("\n");
    let locale = Locale::parse_or_default(lang);
    let input = WriteSetupGuideInput {
        project_name: &project_name,
        score: dry_run_plan.setup.score,
        gaps: &dry_run_plan.setup.gaps,
        context: &setup_context,
        lang: locale.as_str(),
        forced,
    };
    let guide =
        write_setup_guide_and_checkpoint(client, renderer, store, checkpoint, &input).await?;
    Ok(guide)
}

/// Run only the architecture overview stage for per-stage debugging.
///
/// Validates that the relationships stage is complete, loads identify and
/// relationships results from the checkpoint, and calls
/// [`overview_and_checkpoint`]. Returns the
/// [`decon_core::ArchitectureOverview`] and updates the checkpoint in place.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] if the relationships stage is not
/// complete. Returns stage errors from [`overview_and_checkpoint`].
pub async fn run_overview_stage(
    client: &dyn LlmClient,
    renderer: &PromptRenderer,
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    project_name: &str,
    lang_note: &str,
    modules: &[ModuleKey],
) -> Result<decon_core::ArchitectureOverview, GenerateError> {
    if !checkpoint.is_stage_complete(StageId::Relationships) {
        return Err(prerequisite_error("overview", "relationships"));
    }
    let identify = load_identify(checkpoint)?;
    let relationships = load_relationships(checkpoint)?;
    let input = OverviewInput {
        project_name: project_name.to_string(),
        summary: relationships.project_summary.clone(),
        inventory: modules.to_vec(),
        abstractions: identify.abstractions.clone(),
        relationships: relationships.relationships.clone(),
        lang_note: lang_note.to_string(),
        strict_app_validation: true,
    };
    let overview = overview_and_checkpoint(client, renderer, store, checkpoint, &input).await?;
    Ok(overview)
}

/// Run only the combine stage for per-stage debugging.
///
/// Validates that the chapters stage is complete, loads all prior stage
/// results from the checkpoint (identify, relationships, order, chapters,
/// and optionally setup/overview), and calls [`combine_and_checkpoint`].
/// Writes the final `index.md` to `output_dir` and updates the checkpoint.
///
/// # Errors
///
/// Returns [`GenerateError::Config`] if the chapters stage is not complete.
/// Returns stage errors from [`combine_and_checkpoint`].
pub fn run_combine_stage(
    store: &CheckpointStore,
    checkpoint: &mut CheckpointV1,
    output_dir: &Path,
    language: &str,
    modules: &[ModuleKey],
) -> Result<CombinedTutorial, GenerateError> {
    if !checkpoint.is_stage_complete(StageId::Chapters) {
        return Err(prerequisite_error("combine", "chapters"));
    }
    let identify = load_identify(checkpoint)?;
    let relationships = load_relationships(checkpoint)?;
    let order = load_order(checkpoint)?;
    let chapters = load_chapters(store, checkpoint)?;
    let setup: Option<decon_core::SetupGuide> = store
        .read_setup_guide(&store.dir, checkpoint)
        .ok()
        .flatten();
    let overview: Option<decon_core::ArchitectureOverview> = store
        .read_architecture_overview(&store.dir, checkpoint)
        .ok()
        .flatten();
    let locale = Locale::parse_or_default(language);
    let combined = combine_and_checkpoint(
        store,
        checkpoint,
        &identify,
        &relationships,
        &order,
        &chapters,
        setup.as_ref(),
        overview.as_ref(),
        modules,
        locale,
        output_dir,
    )?;
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint_store::records_from_files;
    use decon_core::{Abstraction, RunConfig, Tier};
    use decon_llm::MockClient;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-gen-{label}-{n}-{seq}"));
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
                tier: Tier::S,
                kind: decon_core::AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/router.rs".into()],
            },
            Abstraction {
                name: "Store".into(),
                description: "Persistence layer".into(),
                file_indices: vec![1],
                tier: Tier::S,
                kind: decon_core::AbstractionKind::new("module"),
                apps: vec!["web".into()],
                entry_files: vec!["src/store.rs".into()],
            },
            Abstraction {
                name: "Worker".into(),
                description: "Background jobs".into(),
                file_indices: vec![2],
                tier: Tier::S,
                kind: decon_core::AbstractionKind::new("module"),
                apps: vec!["api".into()],
                entry_files: vec!["src/worker.rs".into()],
            },
        ]
    }

    fn file_contents_for() -> Vec<(String, String)> {
        vec![
            ("src/router.rs".to_string(), "fn route() {}".to_string()),
            ("src/store.rs".to_string(), "fn store() {}".to_string()),
            ("src/worker.rs".to_string(), "fn work() {}".to_string()),
        ]
    }

    fn files_and_sizes() -> (Vec<String>, Vec<u64>) {
        (
            vec![
                "src/router.rs".to_string(),
                "src/store.rs".to_string(),
                "src/worker.rs".to_string(),
            ],
            vec![15, 14, 14],
        )
    }

    fn canned_identify() -> String {
        let yaml = "- name: \"Router\"\n  description: \"Routes requests\"\n  file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: [\"web\"]\n  entry_files: [\"src/router.rs\"]\n- name: \"Store\"\n  description: \"Persistence layer\"\n  file_indices: [1]\n  tier: \"S\"\n  kind: \"module\"\n  apps: [\"web\"]\n  entry_files: [\"src/store.rs\"]\n- name: \"Worker\"\n  description: \"Background jobs\"\n  file_indices: [2]\n  tier: \"S\"\n  kind: \"module\"\n  apps: [\"api\"]\n  entry_files: [\"src/worker.rs\"]\n";
        format!("```yaml\n{yaml}```\n")
    }

    fn canned_relationships() -> String {
        let yaml = "summary: \"A web framework with routing and persistence.\"\nrelationships:\n  - from_abstraction: 0\n    to_abstraction: 1\n    label: \"routes to\"\n    kind: calls\n  - from_abstraction: 2\n    to_abstraction: 1\n    label: \"hands off\"\n    kind: publishes\n";
        format!("```yaml\n{yaml}```\n")
    }

    fn canned_order() -> String {
        "```yaml\n- 0\n- 1\n- 2\n```\n".to_string()
    }

    fn canned_chapter(name: &str, num: usize) -> String {
        format!(
            "# Chapter {num}: {name}\n\n\
## Motivation\n- Need {name}\n\n\
## Core idea\n{name} is key.\n\n\
## Mental model\nThink of a pipeline.\n\n\
## How to use it\nCall `{name}`.\n\n\
## Under the hood\nIt processes data.\n\n\
## Key files\n- src/main.rs\n\n\
## Connections\n- See [other](02_other.md)\n\n\
## Pitfalls\n- Handle errors\n\n\
## Summary\nWe learned about {name}.\n"
        )
    }

    fn canned_setup() -> String {
        "# Setup: my-project\n\n## Prerequisites\n\nRust 1.85.\n\n## Install\n\n```bash\ncargo build\n```\n".to_string()
    }

    fn canned_overview() -> String {
        "# Architecture Overview\n\nThis is a monorepo with apps/web and apps/api.\n".to_string()
    }

    fn full_pipeline_responses() -> Vec<String> {
        vec![
            canned_identify(),
            canned_relationships(),
            canned_order(),
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
            canned_setup(),
            canned_overview(),
        ]
    }

    fn gen_config(dir: PathBuf, output_dir: PathBuf) -> GenerateConfig {
        GenerateConfig {
            dir,
            apps: Vec::new(),
            language: "en".to_string(),
            diagram_level: DiagramLevel::Standard,
            force_setup: false,
            no_setup: false,
            no_overview: false,
            checkpoint_dir: PathBuf::new(),
            output_dir,
            max_abstractions: 10,
            single_shot: true,
            each_app: false,
            run_config: RunConfig::default(),
            chapter_concurrency: 4,
            review_chapters: false,
        }
    }

    fn two_modules() -> Vec<ModuleKey> {
        vec![ModuleKey::new("apps/web"), ModuleKey::new("apps/api")]
    }

    #[tokio::test]
    async fn full_pipeline_all_stages_run_and_output_created() {
        let ckpt_dir = temp_dir("full-ckpt");
        let output_dir = temp_dir("full-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::with_responses(full_pipeline_responses()).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string(), "gap2".to_string()],
            "README content",
            &two_modules(),
        )
        .await
        .expect("pipeline should complete");

        match outcome {
            GenerateOutcome::Completed(combined) => {
                assert!(output_dir.join("index.md").is_file());
                assert_eq!(combined.chapter_count, 3);
            }
            GenerateOutcome::Cancelled { .. } => panic!("expected completed"),
        }
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn resume_identify_complete_skips_identify_runs_rest() {
        let ckpt_dir = temp_dir("resume-id-ckpt");
        let output_dir = temp_dir("resume-id-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);

        let identify = IdentifyResult::new(three_abstractions());
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Identify, "2026-07-24T00:03:00Z");
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();

        let client = MockClient::with_responses(vec![
            canned_relationships(),
            canned_order(),
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
            canned_setup(),
            canned_overview(),
        ])
        .unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string()],
            "README",
            &two_modules(),
        )
        .await
        .expect("should complete");

        assert!(matches!(outcome, GenerateOutcome::Completed(_)));
        assert!(output_dir.join("index.md").is_file());
        assert_eq!(client.call_count(), 7);
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn resume_all_complete_skips_everything_writes_output() {
        let ckpt_dir = temp_dir("resume-all-ckpt");
        let output_dir = temp_dir("resume-all-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);

        let identify = IdentifyResult::new(three_abstractions());
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Identify, "t3");
        let rels = RelationshipsResult::new(
            "summary".to_string(),
            vec![decon_core::Relationship::new(
                0,
                1,
                "calls".to_string(),
                "calls".to_string(),
            )],
        );
        cp.relationships = Some(rels.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Relationships, "t4");
        let order = ChapterOrder::new(vec![0, 1, 2]);
        cp.order = Some(order.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Order, "t5");

        let chapters = ChapterResult::new(vec![
            decon_core::Chapter::new(0, 1, "Router", "# Router\n", Tier::S, "module", "f0"),
            decon_core::Chapter::new(1, 2, "Store", "# Store\n", Tier::S, "module", "f1"),
            decon_core::Chapter::new(2, 3, "Worker", "# Worker\n", Tier::S, "module", "f2"),
        ]);
        let entries = store.write_chapters(&store.dir, &chapters).unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t6");
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();

        let combined = CombinedTutorial::new("# Index\n", 3, false, false, "en");
        let entry = store.write_combined_index(&store.dir, &combined).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Combine, vec![entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Combine, "t7");
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();

        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            80,
            &[],
            "README",
            &[ModuleKey::new("web")],
        )
        .await
        .expect("should complete");

        assert!(matches!(outcome, GenerateOutcome::Completed(_)));
        assert_eq!(client.call_count(), 0);
        assert!(output_dir.join("index.md").is_file());
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn budget_exhausted_during_chapters_returns_partial() {
        let ckpt_dir = temp_dir("budget-ckpt");
        let output_dir = temp_dir("budget-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);

        let identify = IdentifyResult::new(three_abstractions());
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Identify, "t3");
        let rels = RelationshipsResult::new("summary".to_string(), vec![]);
        cp.relationships = Some(rels.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Relationships, "t4");
        let order = ChapterOrder::new(vec![0, 1, 2]);
        cp.order = Some(order.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Order, "t5");
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();

        let client = MockClient::with_responses(vec![
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
        ])
        .unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(2);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let result = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            80,
            &[],
            "README",
            &[ModuleKey::new("web")],
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Budget(_) | GenerateError::Chapters(_)),
            "expected budget or chapters error, got: {err:?}"
        );
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn config_error_bad_language_still_runs_with_default() {
        let ckpt_dir = temp_dir("bad-lang-ckpt");
        let output_dir = temp_dir("bad-lang-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::with_responses(full_pipeline_responses()).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.language = "xx".to_string();
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string()],
            "README",
            &two_modules(),
        )
        .await
        .expect("should complete with default locale");

        assert!(matches!(outcome, GenerateOutcome::Completed(_)));
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn force_setup_on_high_score_repo_generates_setup() {
        let ckpt_dir = temp_dir("force-setup-ckpt");
        let output_dir = temp_dir("force-setup-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::with_responses(full_pipeline_responses()).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.force_setup = true;
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            90,
            &[],
            "README",
            &two_modules(),
        )
        .await
        .expect("should complete");

        assert!(matches!(outcome, GenerateOutcome::Completed(_)));
        assert!(output_dir.join("00_setup.md").is_file());
        assert!(cp.is_stage_complete(StageId::Setup));
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn no_setup_flag_skips_setup_stage() {
        let ckpt_dir = temp_dir("no-setup-ckpt");
        let output_dir = temp_dir("no-setup-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::with_responses(vec![
            canned_identify(),
            canned_relationships(),
            canned_order(),
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
            canned_overview(),
        ])
        .unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.no_setup = true;
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string(), "gap2".to_string(), "gap3".to_string()],
            "README",
            &two_modules(),
        )
        .await
        .expect("should complete");

        assert!(matches!(outcome, GenerateOutcome::Completed(_)));
        assert!(!output_dir.join("00_setup.md").exists());
        assert!(!cp.is_stage_complete(StageId::Setup));
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn no_overview_on_multi_app_repo_skips_overview() {
        let ckpt_dir = temp_dir("no-overview-ckpt");
        let output_dir = temp_dir("no-overview-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::with_responses(vec![
            canned_identify(),
            canned_relationships(),
            canned_order(),
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
            canned_setup(),
        ])
        .unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.no_overview = true;
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string()],
            "README",
            &two_modules(),
        )
        .await
        .expect("should complete");

        assert!(matches!(outcome, GenerateOutcome::Completed(_)));
        assert!(!output_dir.join("00_architecture_overview.md").exists());
        assert!(!cp.is_stage_complete(StageId::Overview));
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[test]
    fn should_generate_setup_respects_thresholds() {
        assert!(!should_generate_setup(80, &[], false, false));
        assert!(should_generate_setup(40, &[], false, false));
        assert!(should_generate_setup(
            crate::setup_guide::SETUP_SCORE_THRESHOLD - 1,
            &[],
            false,
            false
        ));
        assert!(should_generate_setup(
            90,
            &["a".into(), "b".into(), "c".into()],
            false,
            false
        ));
        assert!(should_generate_setup(90, &[], true, false));
        assert!(!should_generate_setup(10, &[], false, true));
    }

    #[test]
    fn should_generate_overview_requires_multiple_modules() {
        assert!(!should_generate_overview(&[ModuleKey::new("web")]));
        assert!(should_generate_overview(&[
            ModuleKey::new("web"),
            ModuleKey::new("api")
        ]));
    }

    #[test]
    fn setup_gap_threshold_is_three() {
        assert_eq!(crate::setup_guide::SETUP_GAP_THRESHOLD, 3);
    }

    #[test]
    fn setup_score_threshold_is_fifty() {
        assert_eq!(crate::setup_guide::SETUP_SCORE_THRESHOLD, 50);
    }

    // --- each-app tests ---

    fn make_monorepo_dir(apps: &[&str]) -> PathBuf {
        let dir = temp_dir("each-app-repo");
        for app in apps {
            let app_path = dir.join("apps").join(app).join("lib");
            fs::create_dir_all(&app_path).unwrap();
            fs::write(
                app_path.join(format!("{app}.ex")),
                format!("defmodule {app} do\nend\n"),
            )
            .unwrap();
        }
        dir
    }

    fn each_app_config(
        dir: PathBuf,
        output_dir: PathBuf,
        checkpoint_dir: PathBuf,
    ) -> GenerateConfig {
        GenerateConfig {
            dir,
            apps: Vec::new(),
            language: "en".to_string(),
            diagram_level: DiagramLevel::Standard,
            force_setup: false,
            no_setup: true,
            no_overview: false,
            checkpoint_dir,
            output_dir,
            max_abstractions: 10,
            single_shot: true,
            each_app: true,
            run_config: RunConfig::default(),
            chapter_concurrency: 4,
            review_chapters: false,
        }
    }

    fn per_app_responses_with_overview() -> Vec<String> {
        vec![
            single_file_identify(),
            single_file_relationships(),
            single_file_order(),
            canned_chapter("Alpha", 1),
            each_app_overview(),
        ]
    }

    fn per_app_responses_no_overview() -> Vec<String> {
        vec![
            single_file_identify(),
            single_file_relationships(),
            single_file_order(),
            canned_chapter("Alpha", 1),
        ]
    }

    fn single_file_identify() -> String {
        let yaml = "- name: \"Alpha\"\n  description: \"Alpha module\"\n  file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: [\"apps/alpha\"]\n  entry_files: [\"apps/alpha/lib/alpha.ex\"]\n";
        format!("```yaml\n{yaml}```\n")
    }

    fn single_file_relationships() -> String {
        "```yaml\nsummary: \"Alpha module.\"\nrelationships: []\n```\n".to_string()
    }

    fn single_file_order() -> String {
        "```yaml\n- 0\n```\n".to_string()
    }

    fn each_app_overview() -> String {
        "# Architecture Overview\n\nThis is a monorepo with apps/alpha and apps/beta.\n".to_string()
    }

    fn repeated_responses(single: Vec<String>, times: usize) -> Vec<String> {
        let mut all = Vec::new();
        for _ in 0..times {
            all.extend(single.clone());
        }
        all
    }

    fn scoped_ckpt_path(base: &Path, slug: &str) -> PathBuf {
        PathBuf::from(format!("{}-{}", base.display(), slug))
    }

    #[tokio::test]
    async fn each_app_with_2_apps_runs_pipeline_twice_and_creates_index() {
        let repo = make_monorepo_dir(&["alpha", "beta"]);
        let output_dir = temp_dir("each-app-2-out");
        let ckpt_dir = temp_dir("each-app-2-ckpt");

        let responses = repeated_responses(per_app_responses_with_overview(), 2);
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let outcome = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should complete");

        match &outcome {
            EachAppOutcome::Completed(summaries) => {
                assert_eq!(summaries.len(), 2);
                assert!(summaries.iter().all(|s| s.success), "all should succeed");
            }
            EachAppOutcome::Partial { .. } => panic!("expected completed"),
        }

        assert!(
            output_dir.join("apps-alpha").join("index.md").is_file(),
            "alpha output should exist"
        );
        assert!(
            output_dir.join("apps-beta").join("index.md").is_file(),
            "beta output should exist"
        );

        let index = fs::read_to_string(output_dir.join("index.md")).unwrap();
        assert!(index.contains("apps-alpha"), "index should list alpha");
        assert!(index.contains("apps-beta"), "index should list beta");

        let alpha_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-alpha");
        let beta_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-beta");
        assert!(alpha_ckpt.join("checkpoint.json").is_file());
        assert!(beta_ckpt.join("checkpoint.json").is_file());
        assert_ne!(alpha_ckpt, beta_ckpt);

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
        let _ = fs::remove_dir_all(&alpha_ckpt);
        let _ = fs::remove_dir_all(&beta_ckpt);
    }

    #[tokio::test]
    async fn each_app_with_1_app_runs_once() {
        let repo = make_monorepo_dir(&["alpha"]);
        let output_dir = temp_dir("each-app-1-out");
        let ckpt_dir = temp_dir("each-app-1-ckpt");

        let responses = per_app_responses_no_overview();
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let outcome = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should complete");

        match &outcome {
            EachAppOutcome::Completed(summaries) => {
                assert_eq!(summaries.len(), 1);
                assert!(summaries[0].success);
            }
            EachAppOutcome::Partial { .. } => panic!("expected completed"),
        }

        assert!(
            output_dir.join("apps-alpha").join("index.md").is_file(),
            "alpha output should exist in scoped dir"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
        let alpha_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-alpha");
        let _ = fs::remove_dir_all(&alpha_ckpt);
    }

    #[tokio::test]
    async fn each_app_with_one_failing_continues_with_other() {
        let repo = make_monorepo_dir(&["alpha", "beta"]);
        let output_dir = temp_dir("each-app-fail-out");
        let ckpt_dir = temp_dir("each-app-fail-ckpt");

        let responses = repeated_responses(per_app_responses_with_overview(), 2);
        let client = MockClient::with_responses(responses).unwrap().fail_on(
            5,
            decon_llm::LlmError::network("mock failure for second app"),
        );
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let outcome = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should complete");

        match &outcome {
            EachAppOutcome::Completed(summaries) => {
                assert_eq!(summaries.len(), 2);
                let successes: Vec<_> = summaries.iter().filter(|s| s.success).collect();
                let failures: Vec<_> = summaries.iter().filter(|s| !s.success).collect();
                assert_eq!(successes.len(), 1, "one should succeed");
                assert_eq!(failures.len(), 1, "one should fail");
                assert!(
                    failures[0].error.is_some(),
                    "failure should have error message"
                );
            }
            EachAppOutcome::Partial { .. } => panic!("expected completed"),
        }

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
        let alpha_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-alpha");
        let beta_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-beta");
        let _ = fs::remove_dir_all(&alpha_ckpt);
        let _ = fs::remove_dir_all(&beta_ckpt);
    }

    #[tokio::test]
    async fn each_app_with_cancellation_returns_partial() {
        let repo = make_monorepo_dir(&["alpha", "beta"]);
        let output_dir = temp_dir("each-app-cancel-out");
        let ckpt_dir = temp_dir("each-app-cancel-ckpt");

        let responses = repeated_responses(per_app_responses_with_overview(), 2);
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let outcome = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should not error");

        match &outcome {
            EachAppOutcome::Partial { cancelled_app, .. } => {
                assert!(!cancelled_app.is_empty());
            }
            EachAppOutcome::Completed(_) => panic!("expected partial"),
        }

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[tokio::test]
    async fn each_app_summary_index_contains_links() {
        let repo = make_monorepo_dir(&["alpha", "beta"]);
        let output_dir = temp_dir("each-app-links-out");
        let ckpt_dir = temp_dir("each-app-links-ckpt");

        let responses = repeated_responses(per_app_responses_with_overview(), 2);
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let _ = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should complete");

        let index_path = output_dir.join("index.md");
        assert!(index_path.is_file(), "summary index.md should exist");
        let index = fs::read_to_string(&index_path).unwrap();
        assert!(
            index.contains("apps-alpha/index.md"),
            "index should link to alpha tutorial"
        );
        assert!(
            index.contains("apps-beta/index.md"),
            "index should link to beta tutorial"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
        let alpha_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-alpha");
        let beta_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-beta");
        let _ = fs::remove_dir_all(&alpha_ckpt);
        let _ = fs::remove_dir_all(&beta_ckpt);
    }

    #[tokio::test]
    async fn each_app_checkpoint_dirs_dont_collide() {
        let repo = make_monorepo_dir(&["alpha", "beta"]);
        let output_dir = temp_dir("each-app-collide-out");
        let ckpt_dir = temp_dir("each-app-collide-ckpt");

        let responses = repeated_responses(per_app_responses_with_overview(), 2);
        let client = MockClient::with_responses(responses).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let _ = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should complete");

        let alpha_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-alpha");
        let beta_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-beta");
        assert!(alpha_ckpt.is_dir(), "alpha checkpoint dir should exist");
        assert!(beta_ckpt.is_dir(), "beta checkpoint dir should exist");
        assert_ne!(alpha_ckpt, beta_ckpt, "checkpoint dirs must differ");
        assert!(
            alpha_ckpt.join("checkpoint.json").is_file(),
            "alpha checkpoint.json should exist"
        );
        assert!(
            beta_ckpt.join("checkpoint.json").is_file(),
            "beta checkpoint.json should exist"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
        let _ = fs::remove_dir_all(&alpha_ckpt);
        let _ = fs::remove_dir_all(&beta_ckpt);
    }

    fn seed_identify_complete(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = seed_store(store);
        let identify = IdentifyResult::new(three_abstractions());
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Identify, "t3");
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn seed_relationships_complete(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = seed_identify_complete(store);
        let rels = RelationshipsResult::new(
            "A web framework with routing and persistence.".to_string(),
            vec![decon_core::Relationship::new(
                0,
                1,
                "calls".to_string(),
                "calls".to_string(),
            )],
        );
        cp.relationships = Some(rels.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Relationships, "t4");
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn seed_order_complete(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = seed_relationships_complete(store);
        let order = ChapterOrder::new(vec![0, 1, 2]);
        cp.order = Some(order.to_checkpoint_value().unwrap());
        cp.mark_stage_complete(StageId::Order, "t5");
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn seed_chapters_complete(store: &CheckpointStore) -> CheckpointV1 {
        let mut cp = seed_order_complete(store);
        let chapters = ChapterResult::new(vec![
            decon_core::Chapter::new(0, 1, "Router", "# Router\n", Tier::S, "module", "f0"),
            decon_core::Chapter::new(1, 2, "Store", "# Store\n", Tier::S, "module", "f1"),
            decon_core::Chapter::new(2, 3, "Worker", "# Worker\n", Tier::S, "module", "f2"),
        ]);
        let entries = store.write_chapters(&store.dir, &chapters).unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t6");
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();
        let (_, files) = store.load().unwrap();
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    #[tokio::test]
    async fn run_relationships_stage_with_identify_complete_runs() {
        let ckpt_dir = temp_dir("rel-stage-ok");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_identify_complete(&store);
        let client = MockClient::with_responses(vec![canned_relationships()]).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let fc = file_contents_for();

        let result =
            run_relationships_stage(&client, &renderer, &store, &mut cp, &fc, "my-project", "")
                .await
                .expect("should run with identify complete");

        assert_eq!(
            result.project_summary,
            "A web framework with routing and persistence."
        );
        assert!(cp.is_stage_complete(StageId::Relationships));
        assert_eq!(client.call_count(), 1);
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_relationships_stage_without_identify_returns_error() {
        let ckpt_dir = temp_dir("rel-stage-no-id");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();
        let fc = file_contents_for();

        let result =
            run_relationships_stage(&client, &renderer, &store, &mut cp, &fc, "my-project", "")
                .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref m) if m.contains("identify")),
            "expected Config error about identify, got: {err:?}"
        );
        assert_eq!(client.call_count(), 0);
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_order_stage_with_relationships_complete_runs() {
        let ckpt_dir = temp_dir("order-stage-ok");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_relationships_complete(&store);
        let client = MockClient::with_responses(vec![canned_order()]).unwrap();
        let renderer = PromptRenderer::new().unwrap();

        let result = run_order_stage(&client, &renderer, &store, &mut cp, "my-project", "")
            .await
            .expect("should run with relationships complete");

        assert_eq!(result.ordered_indices, vec![0, 1, 2]);
        assert!(cp.is_stage_complete(StageId::Order));
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_order_stage_without_relationships_returns_error() {
        let ckpt_dir = temp_dir("order-stage-no-rel");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_identify_complete(&store);
        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();

        let result = run_order_stage(&client, &renderer, &store, &mut cp, "my-project", "").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref m) if m.contains("relationships")),
            "expected Config error about relationships, got: {err:?}"
        );
        assert_eq!(client.call_count(), 0);
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_chapters_stage_with_order_complete_runs() {
        let ckpt_dir = temp_dir("chapters-stage-ok");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_order_complete(&store);
        let client = MockClient::with_responses(vec![
            canned_chapter("Router", 1),
            canned_chapter("Store", 2),
            canned_chapter("Worker", 3),
        ])
        .unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let fc = file_contents_for();

        let result = run_chapters_stage(
            &client,
            &renderer,
            &store,
            &mut cp,
            &fc,
            "my-project",
            "",
            "en",
            DiagramLevel::Standard,
            4,
        )
        .await
        .expect("should run with order complete");

        assert_eq!(result.chapters.len(), 3);
        assert!(cp.is_stage_complete(StageId::Chapters));
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_chapters_stage_without_order_returns_error() {
        let ckpt_dir = temp_dir("chapters-stage-no-order");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_relationships_complete(&store);
        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();
        let fc = file_contents_for();

        let result = run_chapters_stage(
            &client,
            &renderer,
            &store,
            &mut cp,
            &fc,
            "my-project",
            "",
            "en",
            DiagramLevel::Standard,
            4,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref m) if m.contains("order")),
            "expected Config error about order, got: {err:?}"
        );
        assert_eq!(client.call_count(), 0);
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_setup_stage_with_identify_complete_runs() {
        let ckpt_dir = temp_dir("setup-stage-ok");
        let repo_dir = temp_dir("setup-stage-repo");
        fs::write(repo_dir.join("README.md"), b"# My Project\n").unwrap();
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_identify_complete(&store);
        let client = MockClient::with_responses(vec![canned_setup()]).unwrap();
        let renderer = PromptRenderer::new().unwrap();

        let result = run_setup_stage(&client, &renderer, &store, &mut cp, &repo_dir, true, "en")
            .await
            .expect("should run with identify complete");

        assert!(cp.is_stage_complete(StageId::Setup));
        assert!(result.forced);
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&repo_dir);
    }

    #[tokio::test]
    async fn run_setup_stage_without_identify_returns_error() {
        let ckpt_dir = temp_dir("setup-stage-no-id");
        let repo_dir = temp_dir("setup-stage-no-id-repo");
        fs::write(repo_dir.join("README.md"), b"# My Project\n").unwrap();
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();

        let result =
            run_setup_stage(&client, &renderer, &store, &mut cp, &repo_dir, true, "en").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref m) if m.contains("identify")),
            "expected Config error about identify, got: {err:?}"
        );
        assert_eq!(client.call_count(), 0);
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&repo_dir);
    }

    #[tokio::test]
    async fn run_overview_stage_with_relationships_complete_runs() {
        let ckpt_dir = temp_dir("overview-stage-ok");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_relationships_complete(&store);
        let client = MockClient::with_responses(vec![canned_overview()]).unwrap();
        let renderer = PromptRenderer::new().unwrap();

        let _result = run_overview_stage(
            &client,
            &renderer,
            &store,
            &mut cp,
            "my-project",
            "",
            &two_modules(),
        )
        .await
        .expect("should run with relationships complete");

        assert!(cp.is_stage_complete(StageId::Overview));
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[tokio::test]
    async fn run_overview_stage_without_relationships_returns_error() {
        let ckpt_dir = temp_dir("overview-stage-no-rel");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_identify_complete(&store);
        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();

        let result = run_overview_stage(
            &client,
            &renderer,
            &store,
            &mut cp,
            "my-project",
            "",
            &two_modules(),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref m) if m.contains("relationships")),
            "expected Config error about relationships, got: {err:?}"
        );
        assert_eq!(client.call_count(), 0);
        let _ = fs::remove_dir_all(&ckpt_dir);
    }

    #[test]
    fn run_combine_stage_with_all_stages_complete_runs() {
        let ckpt_dir = temp_dir("combine-stage-ok");
        let output_dir = temp_dir("combine-stage-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_chapters_complete(&store);

        let result = run_combine_stage(&store, &mut cp, &output_dir, "en", &two_modules())
            .expect("should run with all stages complete");

        assert!(output_dir.join("index.md").is_file());
        assert_eq!(result.chapter_count, 3);
        assert!(cp.is_stage_complete(StageId::Combine));
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    #[test]
    fn run_combine_stage_without_chapters_returns_error() {
        let ckpt_dir = temp_dir("combine-stage-no-chap");
        let output_dir = temp_dir("combine-stage-no-chap-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_order_complete(&store);

        let result = run_combine_stage(&store, &mut cp, &output_dir, "en", &two_modules());

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, GenerateError::Config(ref m) if m.contains("chapters")),
            "expected Config error about chapters, got: {err:?}"
        );
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    // ------------------------------------------------------------------
    // Issue #230: mid-pipeline cancellation, each-app edge cases, empty language
    // ------------------------------------------------------------------

    /// Pre-cancelling the token before calling `run_generate` must return
    /// `GenerateOutcome::Cancelled` at the first stage checkpoint (identify),
    /// without making any LLM calls.
    #[tokio::test]
    async fn cancel_before_identify_returns_cancelled() {
        let ckpt_dir = temp_dir("cancel-pre-ckpt");
        let output_dir = temp_dir("cancel-pre-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        cancel.cancel();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string()],
            "README",
            &two_modules(),
        )
        .await
        .expect("should not error on cancel");

        assert!(
            matches!(outcome, GenerateOutcome::Cancelled { .. }),
            "pre-cancelled token should return Cancelled, got {outcome:?}"
        );
        assert_eq!(
            client.call_count(),
            0,
            "no LLM calls should be made when cancelled before start"
        );
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    /// `run_generate` with an empty language string must handle it gracefully
    /// (the pipeline uses an empty `language_instruction` and default locale).
    #[tokio::test]
    async fn empty_language_instruction_handles_gracefully() {
        let ckpt_dir = temp_dir("empty-lang-ckpt");
        let output_dir = temp_dir("empty-lang-out");
        let store = CheckpointStore::new(&ckpt_dir);
        let mut cp = seed_store(&store);
        let client = MockClient::with_responses(full_pipeline_responses()).unwrap();
        let renderer = PromptRenderer::new().unwrap();
        let mut progress = ProgressTracker::new(200);
        let cancel = CancelToken::new();
        let (files, sizes) = files_and_sizes();
        let fc = file_contents_for();
        let mut cfg = gen_config(PathBuf::from("/repo"), output_dir.clone());
        cfg.language = String::new();
        cfg.checkpoint_dir.clone_from(&ckpt_dir);

        let outcome = run_generate(
            &client,
            &renderer,
            &store,
            &mut cp,
            &mut progress,
            &cancel,
            &cfg,
            &fc,
            files,
            sizes,
            20,
            &["gap1".to_string()],
            "README",
            &two_modules(),
        )
        .await
        .expect("empty language should complete with default locale");

        assert!(
            matches!(outcome, GenerateOutcome::Completed(_)),
            "empty language should complete, got {outcome:?}"
        );
        let _ = fs::remove_dir_all(&ckpt_dir);
        let _ = fs::remove_dir_all(&output_dir);
    }

    /// `run_generate_each_app` with zero modules (empty repo) must write an
    /// empty summary index and return `EachAppOutcome::Completed([])`.
    #[tokio::test]
    async fn each_app_with_zero_modules_writes_empty_index() {
        // Create a truly empty repo directory (no files at all).
        let repo = temp_dir("each-app-empty-repo");
        fs::create_dir_all(&repo).unwrap();

        let output_dir = temp_dir("each-app-empty-out");
        let ckpt_dir = temp_dir("each-app-empty-ckpt");

        let client = MockClient::new("should-not-be-called");
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let outcome = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app with zero modules should complete");

        match &outcome {
            EachAppOutcome::Completed(summaries) => {
                assert!(
                    summaries.is_empty(),
                    "zero modules should produce empty summaries"
                );
            }
            EachAppOutcome::Partial { .. } => panic!("expected completed with empty summaries"),
        }

        // The summary index.md should exist and indicate no apps.
        let index_path = output_dir.join("index.md");
        assert!(index_path.is_file(), "summary index.md should exist");
        let index = fs::read_to_string(&index_path).unwrap();
        assert!(
            index.contains("No apps"),
            "index should indicate no apps found, got: {index}"
        );

        assert_eq!(client.call_count(), 0, "no LLM calls for zero modules");

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
    }

    /// `run_generate_each_app` with one module whose LLM call fails must
    /// record the failure in the summary and continue with other modules.
    /// This complements `each_app_with_one_failing_continues_with_other` by
    /// verifying the error message content and index file content.
    #[tokio::test]
    async fn each_app_failure_error_message_and_index_content() {
        let repo = make_monorepo_dir(&["alpha", "beta"]);
        let output_dir = temp_dir("each-app-fail-msg-out");
        let ckpt_dir = temp_dir("each-app-fail-msg-ckpt");

        // Fail on call 5 (first call of second app, beta).
        let responses = repeated_responses(per_app_responses_with_overview(), 2);
        let client = MockClient::with_responses(responses)
            .unwrap()
            .fail_on(5, decon_llm::LlmError::network("beta identify failure"));
        let renderer = PromptRenderer::new().unwrap();
        let cancel = CancelToken::new();

        let cfg = each_app_config(repo.clone(), output_dir.clone(), ckpt_dir.clone());

        let outcome = run_generate_each_app(&client, &renderer, &cancel, &cfg)
            .await
            .expect("each-app should complete even with one failure");

        match &outcome {
            EachAppOutcome::Completed(summaries) => {
                assert_eq!(summaries.len(), 2, "both apps should be in summaries");
                let failures: Vec<_> = summaries.iter().filter(|s| !s.success).collect();
                assert_eq!(
                    failures.len(),
                    1,
                    "one app should fail, got {} failures",
                    failures.len()
                );
                let err = failures[0].error.as_ref().expect("error message");
                assert!(!err.is_empty(), "error message should not be empty");
            }
            EachAppOutcome::Partial { .. } => panic!("expected completed"),
        }

        // The index.md should contain both apps, one marked FAILED.
        let index = fs::read_to_string(output_dir.join("index.md")).unwrap();
        assert!(
            index.contains("FAILED"),
            "index should mark the failed app, got: {index}"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&output_dir);
        let alpha_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-alpha");
        let beta_ckpt = scoped_ckpt_path(&ckpt_dir, "apps-beta");
        let _ = fs::remove_dir_all(&alpha_ckpt);
        let _ = fs::remove_dir_all(&beta_ckpt);
    }
}
