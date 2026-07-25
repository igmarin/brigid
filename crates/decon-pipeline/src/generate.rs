//! Full pipeline orchestration for `decon generate` (M4-CLI-1).
//!
//! Runs the complete tutorial generation pipeline:
//! identify -> relationships -> order -> chapters -> setup -> overview -> combine.
//!
//! Each stage checks the checkpoint for completion and skips if already done,
//! enabling resume from any point. The orchestration logic lives here so the
//! CLI binary stays thin.

use std::path::{Path, PathBuf};

use decon_core::{
    ChapterOrder, ChapterResult, CheckpointV1, CombinedTutorial, IdentifyResult, Locale, ModuleKey,
    ProgressTracker, RelationshipsResult, RunConfig, StageId, config_hash,
};

use crate::cancellation::CancelToken;
use crate::checkpoint_store::{CheckpointStore, CheckpointStoreError};
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
    /// Merged run config (CLI > file > env > defaults).
    pub run_config: RunConfig,
    /// Maximum concurrent chapter writes.
    pub chapter_concurrency: usize,
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
            files.clone(),
            sizes.clone(),
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
            run_config: RunConfig::default(),
            chapter_concurrency: 4,
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
}
