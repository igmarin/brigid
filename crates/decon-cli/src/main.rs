//! `decon` — deconstruct a codebase into an AI-generated tutorial.
//!
//! This binary only parses arguments and wires up library crates; business
//! logic lives in `decon-core`, `decon-crawl`, and `decon-pipeline`.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use decon_core::{
    DEFAULT_EVAL_PASS_THRESHOLD, ModuleKey, RunConfig, TutorialFile, config_from_env_map,
    evaluate_tutorial, parse_toml_config, parse_yaml_config, resolve_config,
};
use decon_crawl::crawl_local;
use decon_pipeline::{
    CheckpointStore, DryRunError, check_identity, dry_run, next_stage, pending_stages,
};

/// Success.
const EXIT_OK: u8 = 0;
/// Generic failure (including structural eval fail).
const EXIT_FAIL: u8 = 1;
/// Config / path / I/O input errors (best-practices exit table).
const EXIT_CONFIG: u8 = 2;
/// Partial success with checkpoint: the stage was cancelled (Ctrl+C / SIGTERM)
/// mid-flight, but a partial checkpoint was written. Resume to continue.
///
/// This is **not** an error — it means "we made progress and saved it". The
/// checkpoint's `completed_stages` does **not** include the cancelled stage,
/// so a subsequent `decon resume` will re-run it with the partial work
/// available.
const EXIT_PARTIAL_CHECKPOINT: u8 = 5;

/// Deconstruct a codebase into an AI-generated tutorial.
#[derive(Parser, Debug)]
#[command(name = "decon", version, about, long_about = None)]
struct Cli {
    /// Optional path to `decon.toml` or `.decon.yaml` (else discover in cwd).
    #[arg(long = "config", global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Write a starter `decon.toml` in the current or given directory.
    Init {
        /// Directory for the config file (default: `.`).
        #[arg(long = "dir", value_name = "PATH", default_value = ".")]
        dir: PathBuf,
    },
    /// List relative file inventory under a directory (no LLM).
    Crawl {
        /// Repository root to crawl.
        #[arg(long = "dir", value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Build a dry-run plan: crawl + scope + setup + budget (no LLM).
    DryRun {
        /// Repository root.
        #[arg(long = "dir", value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Optional app/module scope keys (repeatable), e.g. `apps/alpha`.
        #[arg(long = "apps", value_name = "MODULE")]
        apps: Vec<String>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Structural eval of a generated tutorial directory (no LLM).
    Eval {
        /// Tutorial output directory (contains index.md and chapters).
        #[arg(long = "out", value_name = "PATH")]
        out: Option<PathBuf>,
        /// Pass threshold 0–100 (default 70).
        #[arg(long, default_value_t = DEFAULT_EVAL_PASS_THRESHOLD)]
        threshold: i32,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Load a checkpoint directory and report resume status (no LLM).
    Resume {
        /// Checkpoint directory (`checkpoint.json` + manifest).
        #[arg(long = "checkpoint", value_name = "PATH")]
        checkpoint: PathBuf,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run the identify stage with graceful Ctrl+C / SIGTERM shutdown.
    ///
    /// On completion: exit 0. On cancellation with a partial checkpoint:
    /// exit 5. On error: exit 1 or 2.
    Identify {
        /// Repository root.
        #[arg(long = "dir", value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Checkpoint directory to write (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Use single-shot mode (one LLM call) instead of map+reduce.
        #[arg(long = "single-shot", default_value_t = false)]
        single_shot: bool,
        /// Maximum abstractions to return.
        #[arg(long = "max-abstractions", default_value_t = 10)]
        max_abstractions: usize,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cfg = match load_merged_config(cli.config.as_deref(), &RunConfig::empty()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: config: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    match cli.command {
        Commands::Init { dir } => cmd_init(&dir),
        Commands::Crawl { dir, format } => {
            let dir = dir
                .or_else(|| cfg.root.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            cmd_crawl(&dir, format)
        }
        Commands::DryRun { dir, apps, format } => {
            let dir = dir
                .or_else(|| cfg.root.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let apps = if apps.is_empty() {
                cfg.apps.clone().unwrap_or_default()
            } else {
                apps
            };
            cmd_dry_run(&dir, &apps, format)
        }
        Commands::Eval {
            out,
            threshold,
            format,
        } => {
            let out = out
                .or_else(|| cfg.output.clone())
                .unwrap_or_else(|| PathBuf::from("output"));
            cmd_eval(&out, threshold, format)
        }
        Commands::Resume { checkpoint, format } => cmd_resume(&checkpoint, &cfg, format),
        Commands::Identify {
            dir,
            checkpoint_dir,
            single_shot,
            max_abstractions,
        } => {
            let dir = dir
                .or_else(|| cfg.root.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            cmd_identify(&dir, &checkpoint_dir, single_shot, max_abstractions, &cfg)
        }
    }
}

/// Load env + optional config file; `cli_overlay` supplies highest-priority fields.
fn load_merged_config(
    config_path: Option<&Path>,
    cli_overlay: &RunConfig,
) -> Result<RunConfig, String> {
    let env_map: BTreeMap<String, String> = env::vars().collect();
    let env_layer = config_from_env_map(&env_map).map_err(|e| e.to_string())?;
    let file_layer = load_file_config(config_path)?;
    Ok(resolve_config(&env_layer, &file_layer, cli_overlay))
}

fn load_file_config(explicit: Option<&Path>) -> Result<RunConfig, String> {
    let path = if let Some(p) = explicit {
        Some(p.to_path_buf())
    } else {
        discover_config_file()
    };
    let Some(path) = path else {
        return Ok(RunConfig::empty());
    };
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".toml") || name == "decon.toml" {
        parse_toml_config(&text).map_err(|e| e.to_string())
    } else if name.ends_with(".yaml") || name.ends_with(".yml") || name.starts_with(".decon") {
        parse_yaml_config(&text).map_err(|e| e.to_string())
    } else {
        // try toml then yaml
        parse_toml_config(&text)
            .or_else(|_| parse_yaml_config(&text))
            .map_err(|e| e.to_string())
    }
}

fn discover_config_file() -> Option<PathBuf> {
    for name in ["decon.toml", ".decon.yaml", ".decon.yml"] {
        let p = PathBuf::from(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn cmd_init(dir: &Path) -> ExitCode {
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("error: create {}: {e}", dir.display());
        return ExitCode::from(EXIT_CONFIG);
    }
    let path = dir.join("decon.toml");
    if path.exists() {
        eprintln!("error: {} already exists", path.display());
        return ExitCode::from(EXIT_CONFIG);
    }
    let sample = r#"# decon configuration (CLI > this file > DECON_* env > defaults)
# root = "."
# output = "output"
# language = "en"
# max_llm_calls = 200
# apps = []
# API keys are read from DECON_LLM_API_KEY env var only — never put them here.
"#;
    match fs::write(&path, sample) {
        Ok(()) => {
            println!("wrote {}", path.display());
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("error: write {}: {e}", path.display());
            ExitCode::from(EXIT_CONFIG)
        }
    }
}

fn cmd_crawl(dir: &Path, format: OutputFormat) -> ExitCode {
    match crawl_local(dir) {
        Ok(result) => {
            match format {
                OutputFormat::Text => {
                    println!("files: {}", result.files.len());
                    for f in &result.files {
                        println!("{f}");
                    }
                }
                OutputFormat::Json => {
                    let v = serde_json::json!({
                        "file_count": result.files.len(),
                        "files": result.files,
                    });
                    println!("{v}");
                }
            }
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("error: crawl failed: {e}");
            ExitCode::from(EXIT_CONFIG)
        }
    }
}

fn cmd_dry_run(dir: &Path, apps: &[String], format: OutputFormat) -> ExitCode {
    let scope: Option<Vec<ModuleKey>> = if apps.is_empty() {
        None
    } else {
        Some(apps.iter().map(ModuleKey::new).collect())
    };
    let scope_ref = scope.as_deref();
    match dry_run(dir, scope_ref) {
        Ok(plan) => {
            match format {
                OutputFormat::Text => {
                    println!("root: {}", plan.root.display());
                    println!("files: {}", plan.files.len());
                    println!("modules: {}", plan.modules.len());
                    println!(
                        "filter: filtered={} before={} after={}",
                        plan.filter_stats.filtered,
                        plan.filter_stats.before,
                        plan.filter_stats.after
                    );
                    println!(
                        "setup: score={} needs_setup_guide={}",
                        plan.setup.score, plan.setup.needs_setup_guide
                    );
                    println!(
                        "budget: files={} raw_chars={} batches={} tokens≈{}",
                        plan.budget.file_count,
                        plan.budget.raw_chars,
                        plan.budget.batch_count,
                        plan.budget.token_estimate
                    );
                }
                OutputFormat::Json => {
                    let modules: serde_json::Map<String, serde_json::Value> = plan
                        .modules
                        .iter()
                        .map(|m| (m.key.as_str().to_owned(), serde_json::json!(m.count)))
                        .collect();
                    let v = serde_json::json!({
                        "root": plan.root.to_string_lossy(),
                        "files": plan.files,
                        "modules": modules,
                        "filter_stats": {
                            "filtered": plan.filter_stats.filtered,
                            "before": plan.filter_stats.before,
                            "after": plan.filter_stats.after,
                            "kept_shared": plan.filter_stats.kept_shared,
                            "module_keys": plan.filter_stats.module_keys.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                        },
                        "setup": {
                            "needs_setup_guide": plan.setup.needs_setup_guide,
                            "score": plan.setup.score,
                            "gaps": plan.setup.gaps,
                            "config_files": plan.setup.config_files,
                        },
                        "budget": {
                            "file_count": plan.budget.file_count,
                            "module_count": plan.budget.module_count,
                            "raw_chars": plan.budget.raw_chars,
                            "budgeted_chars": plan.budget.budgeted_chars,
                            "token_estimate": plan.budget.token_estimate,
                            "batch_count": plan.budget.batch_count,
                        },
                    });
                    println!("{v}");
                }
            }
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("error: dry-run failed: {e}");
            let code = match e {
                DryRunError::Crawl(_)
                | DryRunError::Io { .. }
                | DryRunError::FileSizeOverflow { .. } => EXIT_CONFIG,
            };
            ExitCode::from(code)
        }
    }
}

fn cmd_eval(out: &Path, threshold: i32, format: OutputFormat) -> ExitCode {
    let files = match load_tutorial_markdown(out) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: eval failed to load tutorial: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let report = evaluate_tutorial(&files, threshold);
    match format {
        OutputFormat::Text => {
            println!(
                "score={} passed={} threshold={}",
                report.score, report.passed, report.threshold
            );
            for r in &report.reasons {
                println!("- {r}");
            }
        }
        OutputFormat::Json => {
            let v = serde_json::json!({
                "score": report.score,
                "passed": report.passed,
                "threshold": report.threshold,
                "reasons": report.reasons,
                "checks": {
                    "has_index": report.checks.has_index,
                    "index_has_mermaid": report.checks.index_has_mermaid,
                    "has_setup_or_overview": report.checks.has_setup_or_overview,
                    "mermaid_block_count": report.checks.mermaid_block_count,
                    "mermaid_valid_count": report.checks.mermaid_valid_count,
                    "has_path_citations": report.checks.has_path_citations,
                    "has_evidence_footer": report.checks.has_evidence_footer,
                    "links_resolved": report.checks.links_resolved,
                    "links_total": report.checks.links_total,
                },
            });
            println!("{v}");
        }
    }
    if report.passed {
        ExitCode::from(EXIT_OK)
    } else {
        ExitCode::from(EXIT_FAIL)
    }
}

fn cmd_resume(checkpoint: &Path, current_cfg: &RunConfig, format: OutputFormat) -> ExitCode {
    let store = CheckpointStore::new(checkpoint);
    let (meta, files) = match store.load() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: resume failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let source = current_cfg
        .root
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| meta.metadata.source_revision.clone());

    let identity_ok = check_identity(&meta, current_cfg, &source).is_ok();
    let next = next_stage(&meta);
    let pending: Vec<String> = pending_stages(&meta)
        .into_iter()
        .map(|s| s.as_str().to_owned())
        .collect();
    let completed: Vec<String> = meta
        .completed_stages
        .iter()
        .map(|s| s.as_str().to_owned())
        .collect();

    match format {
        OutputFormat::Text => {
            println!("checkpoint: {}", checkpoint.display());
            println!("version: {}", meta.version);
            println!("source_revision: {}", meta.metadata.source_revision);
            println!("identity_ok: {identity_ok}");
            println!("files_in_bundle: {}", files.len());
            println!("completed: {}", completed.join(","));
            println!(
                "next_stage: {}",
                next.map(|s| s.as_str()).unwrap_or("(done)")
            );
            println!("pending: {}", pending.join(","));
        }
        OutputFormat::Json => {
            let v = serde_json::json!({
                "checkpoint": checkpoint.to_string_lossy(),
                "version": meta.version,
                "source_revision": meta.metadata.source_revision,
                "identity_ok": identity_ok,
                "files_in_bundle": files.len(),
                "completed_stages": completed,
                "next_stage": next.map(|s| s.as_str()),
                "pending_stages": pending,
                "config_hash": meta.config_hash,
            });
            println!("{v}");
        }
    }
    ExitCode::from(EXIT_OK)
}

/// Run the identify stage with cancellation support.
///
/// Sets up a Ctrl+C / SIGTERM handler, runs the identify stage (single-shot
/// or map+reduce), and maps the outcome to an exit code:
///
/// - Completed → exit 0.
/// - Cancelled → exit 5 (partial checkpoint saved).
/// - Error → exit 1 (generic) or 2 (config / I/O).
///
/// This is a thin CLI wrapper around
/// `decon_pipeline::identify_with_cancellation`. The LLM client is a
/// `decon_llm::MockClient` with a canned response when no API key is
/// present — this lets the subcommand be exercised in tests without network
/// access. A real provider client will be wired in M4.
fn cmd_identify(
    dir: &Path,
    checkpoint_dir: &Path,
    single_shot: bool,
    max_abstractions: usize,
    cfg: &RunConfig,
) -> ExitCode {
    // Crawl the repo to get the file inventory.
    let crawl_result = match crawl_local(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: identify: crawl failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    if crawl_result.files.is_empty() {
        eprintln!("error: identify: no files found in {}", dir.display());
        return ExitCode::from(EXIT_CONFIG);
    }

    // Build file-bundle records for the checkpoint. We use empty content for
    // now (the full file-body injection is a later ticket); the checkpoint
    // store requires at least one record.
    let file_entries: Vec<(&str, &[u8])> = crawl_result
        .files
        .iter()
        .map(|f| (f.as_str(), b"" as &[u8]))
        .collect();
    // records_from_files with empty bytes still produces valid records.
    let records = decon_pipeline::records_from_files(&file_entries);

    // Build the identify run config.
    let files = crawl_result.files.clone();
    let sizes = crawl_result.sizes.clone();

    let strategy = if single_shot {
        decon_pipeline::IdentifyStrategy::SingleShot(decon_pipeline::IdentifySingleShotInput {
            files: files.clone(),
            project_name: dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions,
        })
    } else {
        decon_pipeline::IdentifyStrategy::MapReduce(decon_pipeline::IdentifyMapInput {
            files: files.clone(),
            sizes: sizes.clone(),
            project_name: dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions,
            max_concurrency: 4,
            budget_config: decon_core::BudgetConfig::default(),
        })
    };

    let reduce_input = if !single_shot {
        Some(decon_pipeline::IdentifyReduceInput {
            candidates: Vec::new(),
            files: files.clone(),
            project_name: dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_string(),
            language_instruction: String::new(),
            lang_note: String::new(),
            max_abstraction_num: max_abstractions,
            module_summary: String::new(),
        })
    } else {
        None
    };

    let run_cfg = decon_pipeline::IdentifyRunConfig {
        strategy,
        reduce_input,
        unredacted_config: cfg.clone(),
        source_revision: dir.display().to_string(),
        files: records,
    };

    // Set up the LLM client. Without a real API key, use a mock that returns
    // a minimal valid YAML abstraction list. This lets the subcommand be
    // exercised end-to-end in tests. M4 will wire the real provider client.
    let client: Box<dyn decon_llm::LlmClient> = Box::new(decon_llm::MockClient::new(
        "```yaml\n- name: \"Placeholder\"\n  description: \"Auto-generated placeholder abstraction\"\n  file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: []\n  entry_files: []\n```\n",
    ));

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: identify: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let store = CheckpointStore::new(checkpoint_dir);
    let mut progress = decon_core::ProgressTracker::new(
        cfg.max_llm_calls
            .unwrap_or(decon_core::DEFAULT_MAX_LLM_CALLS),
    );

    // Run inside a tokio runtime with the Ctrl+C handler.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: identify: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    rt.block_on(async {
        let cancel = match decon_pipeline::setup_ctrl_c_handler() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: identify: signal handler: {e}");
                return ExitCode::from(EXIT_FAIL);
            }
        };

        let outcome = decon_pipeline::identify_with_cancellation(
            client.as_ref(),
            &renderer,
            &run_cfg,
            &mut progress,
            &cancel,
            &store,
        )
        .await;

        match outcome {
            Ok(decon_pipeline::IdentifyRunOutcome::Completed(result)) => {
                println!(
                    "identify: completed with {} abstractions",
                    result.abstractions.len()
                );
                println!("checkpoint: {}", checkpoint_dir.display());
                ExitCode::from(EXIT_OK)
            }
            Ok(decon_pipeline::IdentifyRunOutcome::Cancelled {
                batches_completed,
                candidates_collected,
            }) => {
                eprintln!(
                    "identify: cancelled (batches_completed={batches_completed}, \
                     candidates_collected={candidates_collected})"
                );
                eprintln!(
                    "partial checkpoint: {} -- resume to continue",
                    checkpoint_dir.display()
                );
                ExitCode::from(EXIT_PARTIAL_CHECKPOINT)
            }
            Err(e) => {
                eprintln!("error: identify failed: {e}");
                ExitCode::from(EXIT_FAIL)
            }
        }
    })
}

fn load_tutorial_markdown(root: &Path) -> Result<Vec<TutorialFile>, String> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let mut out = Vec::new();
    walk_md(root, root, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk_md(dir: &Path, root: &Path, out: &mut Vec<TutorialFile>) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for ent in rd {
        let ent = ent.map_err(|e| format!("dir entry: {e}"))?;
        let path = ent.path();
        if path.is_dir() {
            walk_md(&path, root, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let content =
                fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            let rel = path
                .strip_prefix(root)
                .map_err(|_| format!("strip prefix for {}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            out.push(TutorialFile { path: rel, content });
        }
    }
    Ok(())
}
