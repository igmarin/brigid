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
    custom_host_warning, evaluate_tutorial, parse_toml_config, parse_yaml_config, redact_content,
    resolve_config,
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
/// LLM call budget exceeded (fail-closed on max-LLM-call ceiling).
const EXIT_BUDGET: u8 = 3;
/// LLM provider error (network, timeout, rate-limit, provider, parse).
const EXIT_LLM: u8 = 4;
/// Partial success with checkpoint: the stage was cancelled (Ctrl+C / SIGTERM)
/// mid-flight, but a partial checkpoint was written. Resume to continue.
///
/// This is **not** an error — it means "we made progress and saved it". The
/// checkpoint's `completed_stages` does **not** include the cancelled stage,
/// so a subsequent `decon resume` will re-run it with the partial work
/// available.
const EXIT_PARTIAL_CHECKPOINT: u8 = 5;

/// Compute the default `max_llm_calls` budget.
///
/// The base budget covers identify + relationships + order + chapters.
/// When `review_chapters` is active, each chapter gets an additional LLM
/// call, so we add `max_abstractions` to the budget.
fn default_max_llm_calls(max_abstractions: usize, review_chapters: bool) -> u32 {
    let base = 10 + max_abstractions as u32;
    if review_chapters {
        base + max_abstractions as u32
    } else {
        base
    }
}

/// Default cache size limit in megabytes.
const DEFAULT_CACHE_SIZE_LIMIT_MB: usize = 100;

/// Check whether the `DECON_NO_CACHE` env var disables the cache.
fn cache_is_disabled(vars: &BTreeMap<String, String>) -> bool {
    vars.get("DECON_NO_CACHE")
        .map(|s| {
            let trimmed = s.trim();
            trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Resolve the cache root directory from env, config, or the platform default.
///
/// Precedence: `DECON_LLM_CACHE_DIR` env var > `cache_dir` from config >
/// platform default (`<cache_dir>/decon/llm-cache`).
fn resolve_cache_root(
    vars: &BTreeMap<String, String>,
    cfg_cache_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(v) = vars.get("DECON_LLM_CACHE_DIR") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(dir) = cfg_cache_dir {
        return Some(dir.to_path_buf());
    }
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".cache"));
    Some(base.join("decon").join("llm-cache"))
}

/// Build a [`decon_llm::DiskCache`] from the environment and run config, or
/// `None` when the cache is disabled via `DECON_NO_CACHE=1`.
fn build_llm_cache(cfg: &RunConfig) -> Option<decon_llm::DiskCache> {
    let env_map: BTreeMap<String, String> = env::vars().collect();
    if cache_is_disabled(&env_map) {
        return None;
    }
    let root = resolve_cache_root(&env_map, cfg.cache_dir.as_deref())?;
    let limit_mb = cfg
        .cache_size_limit_mb
        .unwrap_or(DEFAULT_CACHE_SIZE_LIMIT_MB);
    Some(decon_llm::DiskCache::with_size_limit(root, limit_mb))
}

/// Print cache statistics to stderr.
fn print_cache_stats(cache: Option<&decon_llm::DiskCache>) {
    if let Some(cache) = cache {
        let stats = cache.stats();
        eprintln!(
            "cache: hits={} misses={} evictions={} size={}B",
            stats.hits, stats.misses, stats.evictions, stats.current_size_bytes
        );
    }
}

/// Build a live [`decon_llm::LlmClient`] from the environment when an API key
/// is present, optionally with a disk cache.
///
/// Returns `None` when no non-empty `DECON_LLM_API_KEY` / `DEEPSEEK_API_KEY`
/// is set or the client cannot be constructed, so callers can fall back to a
/// mock client for offline/test runs.
fn build_real_llm_client(
    cache: Option<decon_llm::DiskCache>,
    custom_hosts: &[String],
) -> Option<Box<dyn decon_llm::LlmClient>> {
    if env::var("DECON_FORCE_MOCK")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
    {
        return None;
    }
    let key_ok = |var: &str| env::var(var).ok().filter(|s| !s.is_empty()).is_some();
    if !key_ok("DECON_LLM_API_KEY") && !key_ok("DEEPSEEK_API_KEY") {
        return None;
    }
    if let Some(msg) = custom_host_warning(custom_hosts) {
        eprintln!("{msg}");
    }
    let config = decon_llm::OpenAiClientConfig::from_env().ok()?;
    let config = custom_hosts
        .iter()
        .fold(config, |acc, h| acc.with_allowed_host(h));
    let client = decon_llm::OpenAiCompatibleClient::new(config).ok()?;
    let client = if let Some(cache) = cache {
        client.with_cache(cache)
    } else {
        client
    };
    Some(Box::new(client))
}

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
    /// Run the full generate pipeline: identify -> relationships -> order ->
    /// chapters -> setup -> overview -> combine.
    ///
    /// Supports stage skip/resume from checkpoint, all quality flags, and
    /// proper exit codes. On completion: exit 0. On cancellation: exit 5.
    /// On budget exhaustion: exit 3. On LLM error: exit 4. On config error:
    /// exit 2.
    Generate {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Optional app/module scope keys (repeatable), e.g. `apps/alpha`.
        #[arg(long = "apps", value_name = "MODULE")]
        apps: Vec<String>,
        /// Output language (default: `en`).
        #[arg(long = "language", value_name = "LANG", default_value = "en")]
        language: String,
        /// Diagram richness level: minimal, standard, or rich (default: standard).
        #[arg(
            long = "diagram-level",
            value_name = "LEVEL",
            default_value = "standard"
        )]
        diagram_level: String,
        /// Force setup guide generation regardless of score.
        #[arg(long = "force-setup", default_value_t = false)]
        force_setup: bool,
        /// Skip setup guide generation.
        #[arg(long = "no-setup", default_value_t = false)]
        no_setup: bool,
        /// Skip architecture overview generation.
        #[arg(long = "no-overview", default_value_t = false)]
        no_overview: bool,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output directory (default: `output`).
        #[arg(long = "output-dir", value_name = "PATH")]
        output_dir: Option<PathBuf>,
        /// Maximum abstractions to identify (default: 10).
        #[arg(long = "max-abstractions", default_value_t = 10)]
        max_abstractions: usize,
        /// Use single-shot identify instead of map+reduce.
        #[arg(long = "single-shot", default_value_t = false)]
        single_shot: bool,
        /// Run the full pipeline once per discovered app/module, writing
        /// separate output directories and a summary index.
        #[arg(long = "each-app", default_value_t = false)]
        each_app: bool,
        /// Run a second LLM pass to polish each chapter (doubles chapter LLM cost).
        #[arg(long = "review-chapters", default_value_t = false)]
        review_chapters: bool,
    },
    /// Run only the relationships stage (reads identify from checkpoint).
    Relationships {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
    },
    /// Run only the order stage (reads identify + relationships from checkpoint).
    Order {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
    },
    /// Run only the chapters stage (reads identify + relationships + order from checkpoint).
    Chapters {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output directory (default: `output`).
        #[arg(long = "output-dir", value_name = "PATH")]
        output_dir: Option<PathBuf>,
        /// Output language (default: `en`).
        #[arg(long = "language", value_name = "LANG", default_value = "en")]
        language: String,
        /// Diagram richness level: minimal, standard, or rich (default: standard).
        #[arg(
            long = "diagram-level",
            value_name = "LEVEL",
            default_value = "standard"
        )]
        diagram_level: String,
    },
    /// Run only the setup guide stage (reads identify + dry-run from checkpoint).
    Setup {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Force generation even if the setup score is high.
        #[arg(long = "force", default_value_t = false)]
        force: bool,
    },
    /// Run only the architecture overview stage (reads identify + relationships from checkpoint).
    Overview {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
    },
    /// Run only the combine stage (reads all prior outputs from checkpoint).
    Combine {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.decon-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output directory (default: `output`).
        #[arg(long = "output-dir", value_name = "PATH")]
        output_dir: Option<PathBuf>,
        /// Output language (default: `en`).
        #[arg(long = "language", value_name = "LANG", default_value = "en")]
        language: String,
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
        Commands::Generate {
            dir,
            apps,
            language,
            diagram_level,
            force_setup,
            no_setup,
            no_overview,
            checkpoint_dir,
            output_dir,
            max_abstractions,
            single_shot,
            each_app,
            review_chapters,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            let output_dir = output_dir
                .or_else(|| cfg.output.clone())
                .unwrap_or_else(|| PathBuf::from("output"));
            let apps = if apps.is_empty() {
                cfg.apps.clone().unwrap_or_default()
            } else {
                apps
            };
            cmd_generate(
                &dir,
                &apps,
                &language,
                &diagram_level,
                force_setup,
                no_setup,
                no_overview,
                &checkpoint_dir,
                &output_dir,
                max_abstractions,
                single_shot,
                each_app,
                review_chapters,
                &cfg,
            )
        }
        Commands::Relationships {
            dir,
            checkpoint_dir,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            cmd_relationships(&dir, &checkpoint_dir, &cfg)
        }
        Commands::Order {
            dir,
            checkpoint_dir,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            cmd_order(&dir, &checkpoint_dir, &cfg)
        }
        Commands::Chapters {
            dir,
            checkpoint_dir,
            output_dir,
            language,
            diagram_level,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            let output_dir = output_dir
                .or_else(|| cfg.output.clone())
                .unwrap_or_else(|| PathBuf::from("output"));
            cmd_chapters(
                &dir,
                &checkpoint_dir,
                &output_dir,
                &language,
                &diagram_level,
            )
        }
        Commands::Setup {
            dir,
            checkpoint_dir,
            force,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            cmd_setup(&dir, &checkpoint_dir, force, &cfg)
        }
        Commands::Overview {
            dir,
            checkpoint_dir,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            cmd_overview(&dir, &checkpoint_dir, &cfg)
        }
        Commands::Combine {
            dir,
            checkpoint_dir,
            output_dir,
            language,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".decon-checkpoint"));
            let output_dir = output_dir
                .or_else(|| cfg.output.clone())
                .unwrap_or_else(|| PathBuf::from("output"));
            cmd_combine(&dir, &checkpoint_dir, &output_dir, &language, &cfg)
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
    // A path with no file-name component (e.g. a trailing-slash directory
    // path) cannot be a config file — surface a clear error instead of
    // falling through to a confusing `read_to_string` failure.
    if path.file_name().is_none() {
        return Err(format!(
            "config path {} has no file name component",
            path.display()
        ));
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // Detect format from the file extension. An absent or unrecognized
    // extension falls back to "try both parsers" so extensionless config
    // files (and oddball names) keep working.
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("toml") => parse_toml_config(&text).map_err(|e| e.to_string()),
        Some("yaml") | Some("yml") => parse_yaml_config(&text).map_err(|e| e.to_string()),
        // Absent or unknown extension: try TOML first, then YAML. This is the
        // explicit fallback for extensionless files like `decon` or unknown
        // extensions like `.json`.
        _ => parse_toml_config(&text)
            .or_else(|_| parse_yaml_config(&text))
            .map_err(|e| e.to_string()),
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
#
# Additional LLM provider hosts allowed to receive the Authorization header.
# Defaults: api.openai.com, api.deepseek.com, localhost, 127.0.0.1.
# Also extendable via the DECON_ALLOWED_HOSTS env var (comma-separated).
# [[allowed_hosts]]
# host = "my-proxy.internal"
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
    // Validate the checkpoint path is an existing directory before
    // constructing a `CheckpointStore`, so the user gets a specific message
    // instead of a generic "checkpoint not found" IO error from `load`.
    // This covers both "path does not exist" and "path is a file, not a dir".
    if !checkpoint.is_dir() {
        eprintln!(
            "checkpoint directory '{}' does not exist or is not a directory",
            checkpoint.display()
        );
        return ExitCode::from(EXIT_CONFIG);
    }
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
/// - Budget exceeded → exit 3.
/// - LLM error → exit 4.
/// - Prompt / config error → exit 2.
/// - Other errors → exit 1 (generic).
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
    let project_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let (strategy, reduce_input) = if single_shot {
        (
            decon_pipeline::IdentifyStrategy::SingleShot(decon_pipeline::IdentifySingleShotInput {
                files: crawl_result.files,
                project_name,
                language_instruction: String::new(),
                lang_note: String::new(),
                max_abstraction_num: max_abstractions,
            }),
            None,
        )
    } else {
        let map_files = crawl_result.files;
        let reduce_files = map_files.clone();
        (
            decon_pipeline::IdentifyStrategy::MapReduce(decon_pipeline::IdentifyMapInput {
                files: map_files,
                sizes: crawl_result.sizes,
                project_name: project_name.clone(),
                language_instruction: String::new(),
                lang_note: String::new(),
                max_abstraction_num: max_abstractions,
                max_concurrency: 4,
                budget_config: decon_core::BudgetConfig::default(),
            }),
            Some(decon_pipeline::IdentifyReduceInput {
                candidates: Vec::new(),
                files: reduce_files,
                project_name,
                language_instruction: String::new(),
                lang_note: String::new(),
                max_abstraction_num: max_abstractions,
                module_summary: String::new(),
            }),
        )
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
    // We warn the user so they know the output is a placeholder, not a real
    // LLM analysis.
    let api_key = env::var("DECON_LLM_API_KEY").ok();
    if api_key.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
        eprintln!(
            "warning: identify: no DECON_LLM_API_KEY set — using a mock client. \
             The output will be a placeholder, not a real LLM analysis. \
             Set DECON_LLM_API_KEY to use a real provider (M4)."
        );
    }
    let placeholder_yaml = "```yaml\n- name: \"Placeholder\"\n  description: \"Auto-generated placeholder abstraction\"\n  file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: []\n  entry_files: []\n```\n";
    // Debug/test-only affordance: when DECON_LLM_MOCK_FAIL is set, the mock
    // client fails on the first call with the requested LlmError variant.
    // Guarded by cfg(debug_assertions) so it cannot affect release builds.
    #[cfg(debug_assertions)]
    let client: Box<dyn decon_llm::LlmClient> = if let Some(kind) = env::var("DECON_LLM_MOCK_FAIL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let err = match kind.as_str() {
            "timeout" => decon_llm::LlmError::Timeout,
            "ratelimit" => decon_llm::LlmError::RateLimit { retry_after: None },
            "provider" => decon_llm::LlmError::Provider {
                status: 502,
                body: "mock provider error".to_string(),
            },
            "parse" => decon_llm::LlmError::parse("mock parse failure"),
            _ => decon_llm::LlmError::network("mock network failure"),
        };
        Box::new(decon_llm::MockClient::new("").fail_on(0, err))
    } else {
        Box::new(decon_llm::MockClient::new(placeholder_yaml))
    };
    #[cfg(not(debug_assertions))]
    let client: Box<dyn decon_llm::LlmClient> =
        Box::new(decon_llm::MockClient::new(placeholder_yaml));

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
                let code = match &e {
                    decon_pipeline::IdentifyError::Budget(_) => EXIT_BUDGET,
                    decon_pipeline::IdentifyError::Llm(_)
                    | decon_pipeline::IdentifyError::LlmBatch { .. } => EXIT_LLM,
                    decon_pipeline::IdentifyError::Prompt(_) => EXIT_CONFIG,
                    _ => EXIT_FAIL,
                };
                eprintln!("error: identify failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

/// Run the full generate pipeline with cancellation support.
///
/// Sets up a Ctrl+C / SIGTERM handler, runs all pipeline stages
/// (identify -> relationships -> order -> chapters -> setup -> overview ->
/// combine), and maps the outcome to an exit code:
///
/// - Completed -> exit 0.
/// - Cancelled -> exit 5 (partial checkpoint saved).
/// - Budget exceeded -> exit 3.
/// - LLM error -> exit 4.
/// - Config / prompt error -> exit 2.
/// - Other errors -> exit 1 (generic).
///
/// The LLM client is a `MockClient` with canned responses when no API key is
/// present, mirroring the `identify` subcommand pattern. A real provider client
/// will be wired in M4-SMK-2.
#[allow(clippy::too_many_arguments)]
fn cmd_generate(
    dir: &Path,
    apps: &[String],
    language: &str,
    diagram_level: &str,
    force_setup: bool,
    no_setup: bool,
    no_overview: bool,
    checkpoint_dir: &Path,
    output_dir: &Path,
    max_abstractions: usize,
    single_shot: bool,
    each_app: bool,
    review_chapters: bool,
    cfg: &RunConfig,
) -> ExitCode {
    let diagram_level_parsed = match decon_pipeline::DiagramLevel::parse(diagram_level) {
        Some(dl) => dl,
        None => {
            eprintln!(
                "error: generate: invalid diagram level '{diagram_level}' \
                 (expected: minimal, standard, or rich)"
            );
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    if each_app {
        return cmd_generate_each_app(
            dir,
            language,
            diagram_level_parsed,
            force_setup,
            no_setup,
            no_overview,
            checkpoint_dir,
            output_dir,
            max_abstractions,
            single_shot,
            review_chapters,
            cfg,
        );
    }

    let crawl_result = match crawl_local(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: generate: crawl failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    if crawl_result.files.is_empty() {
        eprintln!("error: generate: no files found in {}", dir.display());
        return ExitCode::from(EXIT_CONFIG);
    }

    let dry_run_plan = match decon_pipeline::dry_run(dir, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: generate: dry-run failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let file_entries: Vec<(&str, &[u8])> = crawl_result
        .files
        .iter()
        .map(|f| (f.as_str(), b"" as &[u8]))
        .collect();
    let records = decon_pipeline::records_from_files(&file_entries);

    let file_contents: Vec<(String, String)> = crawl_result
        .files
        .iter()
        .map(|f| (f.clone(), String::new()))
        .collect();

    let modules: Vec<decon_core::ModuleKey> = dry_run_plan
        .modules
        .iter()
        .map(|m| decon_core::ModuleKey::new(m.key.as_str()))
        .collect();

    let setup_context = dry_run_plan
        .setup
        .config_files
        .iter()
        .map(|f| format!("# File: {f}\n"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut run_config = cfg.clone();
    if run_config.language.is_none() {
        run_config.language = Some(language.to_string());
    }
    run_config.max_llm_calls = run_config
        .max_llm_calls
        .or_else(|| Some(default_max_llm_calls(max_abstractions, review_chapters)));

    let cache = build_llm_cache(&run_config);
    let client: Box<dyn decon_llm::LlmClient> = match build_real_llm_client(
        cache.clone(),
        run_config.allowed_hosts.as_deref().unwrap_or(&[]),
    ) {
        Some(c) => {
            eprintln!("generate: using live LLM provider");
            c
        }
        None => {
            eprintln!(
                "warning: generate: no DECON_LLM_API_KEY set -- using a mock client. \
                 The output will be a placeholder, not a real LLM analysis. \
                 Set DECON_LLM_API_KEY to use a real provider (M4)."
            );
            let placeholder_yaml = "```yaml\n- name: \"Placeholder\"\n  description: \"Auto-generated placeholder abstraction\"\n  file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: []\n  entry_files: []\n```\n";
            let placeholder_rel =
                "```yaml\nsummary: \"Placeholder project summary.\"\nrelationships: []\n```\n";
            let placeholder_order = "```yaml\n- 0\n```\n";
            let placeholder_chapter = "# Chapter 1: Placeholder\n\n## Motivation\n- Need placeholder\n\n## Core idea\nPlaceholder is key.\n\n## Summary\nWe learned about placeholder.\n";
            let placeholder_setup = "# Setup: project\n\n## Prerequisites\n\nInstall dependencies.\n\n## Run\n\n```bash\nmake run\n```\n";
            let placeholder_overview =
                "# Architecture Overview\n\nThis project has multiple modules.\n";

            let mut responses: Vec<String> = Vec::new();
            if single_shot {
                responses.push(placeholder_yaml.to_string());
            } else {
                responses.push(placeholder_yaml.to_string());
                responses.push(placeholder_yaml.to_string());
            }
            responses.push(placeholder_rel.to_string());
            responses.push(placeholder_order.to_string());
            for _ in 0..max_abstractions {
                responses.push(placeholder_chapter.to_string());
            }
            if review_chapters {
                for _ in 0..max_abstractions {
                    responses.push(placeholder_chapter.to_string());
                }
            }
            if !no_setup {
                let do_setup = force_setup
                    || dry_run_plan.setup.score < 50
                    || dry_run_plan.setup.gaps.len() >= 3;
                if do_setup {
                    responses.push(placeholder_setup.to_string());
                }
            }
            if !no_overview && modules.len() > 1 {
                responses.push(placeholder_overview.to_string());
            }

            #[cfg(debug_assertions)]
            let mock: Box<dyn decon_llm::LlmClient> = if let Some(kind) =
                env::var("DECON_LLM_MOCK_FAIL")
                    .ok()
                    .filter(|s| !s.is_empty())
            {
                let err = match kind.as_str() {
                    "timeout" => decon_llm::LlmError::Timeout,
                    "ratelimit" => decon_llm::LlmError::RateLimit { retry_after: None },
                    "provider" => decon_llm::LlmError::Provider {
                        status: 502,
                        body: "mock provider error".to_string(),
                    },
                    "parse" => decon_llm::LlmError::parse("mock parse failure"),
                    _ => decon_llm::LlmError::network("mock network failure"),
                };
                Box::new(decon_llm::MockClient::new("").fail_on(0, err))
            } else {
                Box::new(
                    decon_llm::MockClient::with_responses(responses)
                        .unwrap_or_else(|_| decon_llm::MockClient::new(placeholder_yaml)),
                )
            };
            #[cfg(not(debug_assertions))]
            let mock: Box<dyn decon_llm::LlmClient> = Box::new(
                decon_llm::MockClient::with_responses(responses)
                    .unwrap_or_else(|_| decon_llm::MockClient::new(placeholder_yaml)),
            );
            mock
        }
    };

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: generate: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let store = CheckpointStore::new(checkpoint_dir);

    let (mut checkpoint, existing_files) = match store.load() {
        Ok((meta, files)) => (meta, files),
        Err(_) => {
            let mut meta = decon_core::CheckpointV1::new(
                &run_config,
                run_config.redacted_for_checkpoint(),
                dir.display().to_string(),
                "0Z",
            )
            .map_err(|e| {
                eprintln!("error: generate: checkpoint init: {e}");
                ExitCode::from(EXIT_CONFIG)
            })
            .unwrap();
            meta.mark_stage_complete(decon_core::StageId::Fetch, "0Z");
            meta.mark_stage_complete(decon_core::StageId::DryRun, "0Z");
            (meta, records)
        }
    };
    if !checkpoint.is_stage_complete(decon_core::StageId::Fetch) {
        checkpoint.mark_stage_complete(decon_core::StageId::Fetch, "0Z");
    }
    if !checkpoint.is_stage_complete(decon_core::StageId::DryRun) {
        checkpoint.mark_stage_complete(decon_core::StageId::DryRun, "0Z");
    }
    let _ = store.save(checkpoint.clone(), &existing_files);

    let mut progress = decon_core::ProgressTracker::new(
        run_config
            .max_llm_calls
            .unwrap_or(decon_core::DEFAULT_MAX_LLM_CALLS),
    );

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: generate: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    let exit_code = rt.block_on(async {
        let cancel = match decon_pipeline::setup_ctrl_c_handler() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: generate: signal handler: {e}");
                return ExitCode::from(EXIT_FAIL);
            }
        };

        let gen_config = decon_pipeline::GenerateConfig {
            dir: dir.to_path_buf(),
            apps: apps.to_vec(),
            language: language.to_string(),
            diagram_level: diagram_level_parsed,
            force_setup,
            no_setup,
            no_overview,
            checkpoint_dir: checkpoint_dir.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
            max_abstractions,
            single_shot,
            each_app: false,
            run_config: run_config.clone(),
            chapter_concurrency: decon_pipeline::DEFAULT_CHAPTERS_CONCURRENCY,
            review_chapters,
        };

        let outcome = decon_pipeline::run_generate(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &mut progress,
            &cancel,
            &gen_config,
            &file_contents,
            crawl_result.files,
            crawl_result.sizes,
            dry_run_plan.setup.score,
            &dry_run_plan.setup.gaps,
            &setup_context,
            &modules,
        )
        .await;

        match outcome {
            Ok(decon_pipeline::GenerateOutcome::Completed(combined)) => {
                eprintln!(
                    "generate: completed with {} chapters (locale={})",
                    combined.chapter_count, combined.locale
                );
                eprintln!("output: {}", output_dir.display());
                eprintln!("checkpoint: {}", checkpoint_dir.display());
                ExitCode::from(EXIT_OK)
            }
            Ok(decon_pipeline::GenerateOutcome::Cancelled { checkpoint_path }) => {
                eprintln!("generate: cancelled");
                eprintln!(
                    "partial checkpoint: {} -- resume to continue",
                    checkpoint_path.display()
                );
                ExitCode::from(EXIT_PARTIAL_CHECKPOINT)
            }
            Err(e) => {
                let code = match &e {
                    decon_pipeline::GenerateError::Budget(_)
                    | decon_pipeline::GenerateError::Identify(
                        decon_pipeline::identify::IdentifyError::Budget(_),
                    )
                    | decon_pipeline::GenerateError::Relationships(
                        decon_pipeline::relationships::RelationshipsError::Budget(_),
                    )
                    | decon_pipeline::GenerateError::Order(
                        decon_pipeline::order::OrderError::Budget(_),
                    )
                    | decon_pipeline::GenerateError::Chapters(
                        decon_pipeline::chapters::ChaptersError::Budget(_),
                    )
                    | decon_pipeline::GenerateError::Review(
                        decon_pipeline::review::ReviewError::Budget(_),
                    ) => EXIT_BUDGET,
                    decon_pipeline::GenerateError::Identify(
                        decon_pipeline::identify::IdentifyError::Llm(_)
                        | decon_pipeline::identify::IdentifyError::LlmBatch { .. },
                    )
                    | decon_pipeline::GenerateError::Relationships(
                        decon_pipeline::relationships::RelationshipsError::Llm(_),
                    )
                    | decon_pipeline::GenerateError::Order(
                        decon_pipeline::order::OrderError::Llm(_),
                    )
                    | decon_pipeline::GenerateError::Chapters(
                        decon_pipeline::chapters::ChaptersError::Llm(_),
                    )
                    | decon_pipeline::GenerateError::Review(
                        decon_pipeline::review::ReviewError::Llm(_),
                    )
                    | decon_pipeline::GenerateError::Setup(
                        decon_pipeline::setup_guide::SetupGuideError::Llm(_),
                    )
                    | decon_pipeline::GenerateError::Overview(
                        decon_pipeline::overview::OverviewError::Llm(_),
                    ) => EXIT_LLM,
                    decon_pipeline::GenerateError::Config(_) => EXIT_CONFIG,
                    _ => EXIT_FAIL,
                };
                eprintln!("error: generate failed: {e}");
                ExitCode::from(code)
            }
        }
    });
    print_cache_stats(cache.as_ref());
    exit_code
}

/// Run the full generate pipeline once per discovered app/module.
///
/// Delegates to `decon_pipeline::run_generate_each_app`, which discovers
/// modules via dry-run, runs the pipeline once per module with scoped
/// output/checkpoint dirs, and writes a summary `index.md`.
///
/// Exit codes:
/// - All apps succeeded -> exit 0.
/// - Some apps failed (none cancelled) -> exit 1.
/// - Cancelled -> exit 5.
/// - Config / prompt error -> exit 2.
/// - Other errors -> exit 1.
#[allow(clippy::too_many_arguments)]
fn cmd_generate_each_app(
    dir: &Path,
    language: &str,
    diagram_level: decon_pipeline::DiagramLevel,
    force_setup: bool,
    no_setup: bool,
    no_overview: bool,
    checkpoint_dir: &Path,
    output_dir: &Path,
    max_abstractions: usize,
    single_shot: bool,
    review_chapters: bool,
    cfg: &RunConfig,
) -> ExitCode {
    let mut run_config = cfg.clone();
    if run_config.language.is_none() {
        run_config.language = Some(language.to_string());
    }
    run_config.max_llm_calls = run_config
        .max_llm_calls
        .or_else(|| Some(default_max_llm_calls(max_abstractions, review_chapters)));

    let cache = build_llm_cache(&run_config);
    let client: Box<dyn decon_llm::LlmClient> = match build_real_llm_client(
        cache.clone(),
        run_config.allowed_hosts.as_deref().unwrap_or(&[]),
    ) {
        Some(c) => {
            eprintln!("generate: using live LLM provider");
            c
        }
        None => {
            eprintln!(
                "warning: generate: no DECON_LLM_API_KEY set -- using a mock client. \
                 The output will be a placeholder, not a real LLM analysis. \
                 Set DECON_LLM_API_KEY to use a real provider (M4)."
            );
            let placeholder_yaml = "```yaml\n- name: \"Placeholder\"\n  description: \"Auto-generated placeholder abstraction\"\n  file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: []\n  entry_files: []\n```\n";
            let placeholder_rel =
                "```yaml\nsummary: \"Placeholder project summary.\"\nrelationships: []\n```\n";
            let placeholder_order = "```yaml\n- 0\n```\n";
            let placeholder_chapter = "# Chapter 1: Placeholder\n\n## Motivation\n- Need placeholder\n\n## Core idea\nPlaceholder is key.\n\n## Summary\nWe learned about placeholder.\n";
            let placeholder_setup = "# Setup: project\n\n## Prerequisites\n\nInstall dependencies.\n\n## Run\n\n```bash\nmake run\n```\n";
            let placeholder_overview =
                "# Architecture Overview\n\nThis project has multiple modules.\n";

            let mut single_app_responses: Vec<String> = Vec::new();
            if single_shot {
                single_app_responses.push(placeholder_yaml.to_string());
            } else {
                single_app_responses.push(placeholder_yaml.to_string());
                single_app_responses.push(placeholder_yaml.to_string());
            }
            single_app_responses.push(placeholder_rel.to_string());
            single_app_responses.push(placeholder_order.to_string());
            single_app_responses.push(placeholder_chapter.to_string());
            if review_chapters {
                single_app_responses.push(placeholder_chapter.to_string());
            }
            if !no_setup {
                single_app_responses.push(placeholder_setup.to_string());
            }
            if !no_overview {
                single_app_responses.push(placeholder_overview.to_string());
            }

            let mut responses: Vec<String> = Vec::new();
            for _ in 0..20 {
                responses.extend(single_app_responses.clone());
            }

            #[cfg(debug_assertions)]
            let mock: Box<dyn decon_llm::LlmClient> = if let Some(kind) =
                env::var("DECON_LLM_MOCK_FAIL")
                    .ok()
                    .filter(|s| !s.is_empty())
            {
                let err = match kind.as_str() {
                    "timeout" => decon_llm::LlmError::Timeout,
                    "ratelimit" => decon_llm::LlmError::RateLimit { retry_after: None },
                    "provider" => decon_llm::LlmError::Provider {
                        status: 502,
                        body: "mock provider error".to_string(),
                    },
                    "parse" => decon_llm::LlmError::parse("mock parse failure"),
                    _ => decon_llm::LlmError::network("mock network failure"),
                };
                Box::new(decon_llm::MockClient::new("").fail_on(0, err))
            } else {
                Box::new(
                    decon_llm::MockClient::with_responses(responses)
                        .unwrap_or_else(|_| decon_llm::MockClient::new(placeholder_yaml)),
                )
            };
            #[cfg(not(debug_assertions))]
            let mock: Box<dyn decon_llm::LlmClient> = Box::new(
                decon_llm::MockClient::with_responses(responses)
                    .unwrap_or_else(|_| decon_llm::MockClient::new(placeholder_yaml)),
            );
            mock
        }
    };

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: generate: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let gen_config = decon_pipeline::GenerateConfig {
        dir: dir.to_path_buf(),
        apps: Vec::new(),
        language: language.to_string(),
        diagram_level,
        force_setup,
        no_setup,
        no_overview,
        checkpoint_dir: checkpoint_dir.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        max_abstractions,
        single_shot,
        each_app: true,
        run_config: run_config.clone(),
        chapter_concurrency: decon_pipeline::DEFAULT_CHAPTERS_CONCURRENCY,
        review_chapters,
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: generate: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    let exit_code = rt.block_on(async {
        let cancel = match decon_pipeline::setup_ctrl_c_handler() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: generate: signal handler: {e}");
                return ExitCode::from(EXIT_FAIL);
            }
        };

        let outcome =
            decon_pipeline::run_generate_each_app(client.as_ref(), &renderer, &cancel, &gen_config)
                .await;

        match outcome {
            Ok(decon_pipeline::EachAppOutcome::Completed(summaries)) => {
                let failures: Vec<_> = summaries.iter().filter(|s| !s.success).collect();
                let success_count = summaries.len() - failures.len();
                eprintln!(
                    "generate: each-app completed: {success_count}/{} apps succeeded",
                    summaries.len()
                );
                for s in &summaries {
                    if !s.success {
                        eprintln!(
                            "  FAILED: {} -- {}",
                            s.app,
                            s.error.as_deref().unwrap_or("unknown error")
                        );
                    }
                }
                eprintln!("output: {}", output_dir.display());
                if failures.is_empty() {
                    ExitCode::from(EXIT_OK)
                } else {
                    ExitCode::from(EXIT_FAIL)
                }
            }
            Ok(decon_pipeline::EachAppOutcome::Partial {
                summaries,
                cancelled_app,
            }) => {
                eprintln!("generate: each-app cancelled at '{cancelled_app}'");
                eprintln!(
                    "  {}/{} apps completed before cancellation",
                    summaries.len(),
                    summaries.len() + 1
                );
                eprintln!("output: {}", output_dir.display());
                ExitCode::from(EXIT_PARTIAL_CHECKPOINT)
            }
            Err(e) => {
                let code = match &e {
                    decon_pipeline::GenerateError::Budget(_) => EXIT_BUDGET,
                    decon_pipeline::GenerateError::Config(_) => EXIT_CONFIG,
                    _ => EXIT_FAIL,
                };
                eprintln!("error: generate --each-app failed: {e}");
                ExitCode::from(code)
            }
        }
    });
    print_cache_stats(cache.as_ref());
    exit_code
}

fn load_stage_checkpoint(
    checkpoint_dir: &Path,
) -> Result<(decon_core::CheckpointV1, Vec<decon_core::FileBundleRecord>), ExitCode> {
    if !checkpoint_dir.is_dir() {
        eprintln!(
            "error: checkpoint directory '{}' does not exist or is not a directory",
            checkpoint_dir.display()
        );
        return Err(ExitCode::from(EXIT_CONFIG));
    }
    let store = CheckpointStore::new(checkpoint_dir);
    match store.load() {
        Ok(v) => Ok(v),
        Err(e) => {
            eprintln!("error: checkpoint load failed: {e}");
            Err(ExitCode::from(EXIT_CONFIG))
        }
    }
}

fn stage_project_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

fn stage_language_instruction(language: &str) -> String {
    if language.is_empty() {
        String::new()
    } else {
        format!("Use {language}")
    }
}

fn make_stage_client(responses: Vec<String>, placeholder: &str) -> Box<dyn decon_llm::LlmClient> {
    let api_key = env::var("DECON_LLM_API_KEY").ok();
    if api_key.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
        eprintln!(
            "warning: no DECON_LLM_API_KEY set -- using a mock client. \
             The output will be a placeholder, not a real LLM analysis. \
             Set DECON_LLM_API_KEY to use a real provider (M4)."
        );
    }
    #[cfg(debug_assertions)]
    {
        if let Some(kind) = env::var("DECON_LLM_MOCK_FAIL")
            .ok()
            .filter(|s| !s.is_empty())
        {
            let err = match kind.as_str() {
                "timeout" => decon_llm::LlmError::Timeout,
                "ratelimit" => decon_llm::LlmError::RateLimit { retry_after: None },
                "provider" => decon_llm::LlmError::Provider {
                    status: 502,
                    body: "mock provider error".to_string(),
                },
                "parse" => decon_llm::LlmError::parse("mock parse failure"),
                _ => decon_llm::LlmError::network("mock network failure"),
            };
            return Box::new(decon_llm::MockClient::new("").fail_on(0, err));
        }
    }
    Box::new(
        decon_llm::MockClient::with_responses(responses)
            .unwrap_or_else(|_| decon_llm::MockClient::new(placeholder)),
    )
}

fn stage_exit_code(err: &decon_pipeline::GenerateError) -> u8 {
    match err {
        decon_pipeline::GenerateError::Budget(_)
        | decon_pipeline::GenerateError::Identify(
            decon_pipeline::identify::IdentifyError::Budget(_),
        )
        | decon_pipeline::GenerateError::Relationships(
            decon_pipeline::relationships::RelationshipsError::Budget(_),
        )
        | decon_pipeline::GenerateError::Order(decon_pipeline::order::OrderError::Budget(_))
        | decon_pipeline::GenerateError::Chapters(
            decon_pipeline::chapters::ChaptersError::Budget(_),
        ) => EXIT_BUDGET,
        decon_pipeline::GenerateError::Identify(
            decon_pipeline::identify::IdentifyError::Llm(_)
            | decon_pipeline::identify::IdentifyError::LlmBatch { .. },
        )
        | decon_pipeline::GenerateError::Relationships(
            decon_pipeline::relationships::RelationshipsError::Llm(_),
        )
        | decon_pipeline::GenerateError::Order(decon_pipeline::order::OrderError::Llm(_))
        | decon_pipeline::GenerateError::Chapters(decon_pipeline::chapters::ChaptersError::Llm(
            _,
        ))
        | decon_pipeline::GenerateError::Setup(
            decon_pipeline::setup_guide::SetupGuideError::Llm(_),
        )
        | decon_pipeline::GenerateError::Overview(decon_pipeline::overview::OverviewError::Llm(
            _,
        )) => EXIT_LLM,
        decon_pipeline::GenerateError::Config(_) => EXIT_CONFIG,
        _ => EXIT_FAIL,
    }
}

fn cmd_relationships(dir: &Path, checkpoint_dir: &Path, cfg: &RunConfig) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let crawl_result = match crawl_local(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: relationships: crawl failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let file_contents: Vec<(String, String)> = crawl_result
        .files
        .iter()
        .map(|f| (f.clone(), String::new()))
        .collect();

    let project_name = stage_project_name(dir);
    let language_instruction = stage_language_instruction(cfg.language.as_deref().unwrap_or("en"));

    let placeholder_rel =
        "```yaml\nsummary: \"Placeholder project summary.\"\nrelationships: []\n```\n";
    let client = make_stage_client(vec![placeholder_rel.to_string()], placeholder_rel);

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: relationships: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: relationships: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    rt.block_on(async {
        match decon_pipeline::run_relationships_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &file_contents,
            &project_name,
            &language_instruction,
        )
        .await
        {
            Ok(result) => {
                eprintln!(
                    "relationships: completed (summary_len={}, rels={})",
                    result.project_summary.len(),
                    result.relationships.len()
                );
                eprintln!("checkpoint: {}", checkpoint_dir.display());
                let _ = store.save(checkpoint, &files);
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                let code = stage_exit_code(&e);
                eprintln!("error: relationships failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_order(dir: &Path, checkpoint_dir: &Path, cfg: &RunConfig) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let project_name = stage_project_name(dir);
    let language_instruction = stage_language_instruction(cfg.language.as_deref().unwrap_or("en"));

    let placeholder_order = "```yaml\n- 0\n```\n";
    let client = make_stage_client(vec![placeholder_order.to_string()], placeholder_order);

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: order: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: order: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    rt.block_on(async {
        match decon_pipeline::run_order_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &project_name,
            &language_instruction,
        )
        .await
        {
            Ok(result) => {
                eprintln!(
                    "order: completed (chapters={})",
                    result.ordered_indices.len()
                );
                eprintln!("checkpoint: {}", checkpoint_dir.display());
                let _ = store.save(checkpoint, &files);
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                let code = stage_exit_code(&e);
                eprintln!("error: order failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn cmd_chapters(
    dir: &Path,
    checkpoint_dir: &Path,
    output_dir: &Path,
    language: &str,
    diagram_level: &str,
) -> ExitCode {
    let diagram_level_parsed = match decon_pipeline::DiagramLevel::parse(diagram_level) {
        Some(dl) => dl,
        None => {
            eprintln!(
                "error: chapters: invalid diagram level '{diagram_level}' \
                 (expected: minimal, standard, or rich)"
            );
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let crawl_result = match crawl_local(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: chapters: crawl failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let file_contents: Vec<(String, String)> = crawl_result
        .files
        .iter()
        .map(|f| (f.clone(), String::new()))
        .collect();

    let project_name = stage_project_name(dir);
    let language_instruction = stage_language_instruction(language);

    let placeholder_chapter = "# Chapter 1: Placeholder\n\n## Motivation\n- Need placeholder\n\n## Core idea\nPlaceholder is key.\n\n## Summary\nWe learned about placeholder.\n";
    let identify = match decon_pipeline::identify_checkpoint::load_identify_result(&checkpoint) {
        Some(i) => i,
        None => {
            eprintln!("error: chapters: identify result not found in checkpoint");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let max_chapters = identify.abstractions.len().max(1);
    let responses: Vec<String> = (0..max_chapters)
        .map(|_| placeholder_chapter.to_string())
        .collect();
    let client = make_stage_client(responses, placeholder_chapter);

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: chapters: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: chapters: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    rt.block_on(async {
        match decon_pipeline::run_chapters_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &file_contents,
            &project_name,
            &language_instruction,
            language,
            diagram_level_parsed,
            decon_pipeline::DEFAULT_CHAPTERS_CONCURRENCY,
        )
        .await
        {
            Ok(result) => {
                eprintln!("chapters: completed (chapters={})", result.chapters.len());
                eprintln!("checkpoint: {}", checkpoint_dir.display());
                eprintln!("output: {}", output_dir.display());
                let _ = store.save(checkpoint, &files);
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                let code = stage_exit_code(&e);
                eprintln!("error: chapters failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_setup(dir: &Path, checkpoint_dir: &Path, force: bool, cfg: &RunConfig) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let placeholder_setup = "# Setup: project\n\n## Prerequisites\n\nInstall dependencies.\n\n## Run\n\n```bash\nmake run\n```\n";
    let client = make_stage_client(vec![placeholder_setup.to_string()], placeholder_setup);

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: setup: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let lang = cfg.language.as_deref().unwrap_or("en");

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: setup: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    rt.block_on(async {
        match decon_pipeline::run_setup_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            dir,
            force,
            lang,
        )
        .await
        {
            Ok(_guide) => {
                eprintln!("setup: completed (forced={force})");
                eprintln!("checkpoint: {}", checkpoint_dir.display());
                let _ = store.save(checkpoint, &files);
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                let code = stage_exit_code(&e);
                eprintln!("error: setup failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_overview(dir: &Path, checkpoint_dir: &Path, cfg: &RunConfig) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let dry_run_plan = match decon_pipeline::dry_run(dir, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: overview: dry-run failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let modules: Vec<decon_core::ModuleKey> = dry_run_plan
        .modules
        .iter()
        .map(|m| decon_core::ModuleKey::new(m.key.as_str()))
        .collect();

    let project_name = stage_project_name(dir);
    let language_instruction = stage_language_instruction(cfg.language.as_deref().unwrap_or("en"));

    let placeholder_overview = "# Architecture Overview\n\nThis project has multiple modules.\n";
    let client = make_stage_client(vec![placeholder_overview.to_string()], placeholder_overview);

    let renderer = match decon_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: overview: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: overview: runtime: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    rt.block_on(async {
        match decon_pipeline::run_overview_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &project_name,
            &language_instruction,
            &modules,
        )
        .await
        {
            Ok(_overview) => {
                eprintln!("overview: completed");
                eprintln!("checkpoint: {}", checkpoint_dir.display());
                let _ = store.save(checkpoint, &files);
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                let code = stage_exit_code(&e);
                eprintln!("error: overview failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_combine(
    dir: &Path,
    checkpoint_dir: &Path,
    output_dir: &Path,
    language: &str,
    cfg: &RunConfig,
) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let dry_run_plan = match decon_pipeline::dry_run(dir, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: combine: dry-run failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let modules: Vec<decon_core::ModuleKey> = dry_run_plan
        .modules
        .iter()
        .map(|m| decon_core::ModuleKey::new(m.key.as_str()))
        .collect();

    let lang = if language.is_empty() {
        cfg.language.as_deref().unwrap_or("en")
    } else {
        language
    };

    match decon_pipeline::run_combine_stage(&store, &mut checkpoint, output_dir, lang, &modules) {
        Ok(combined) => {
            eprintln!(
                "combine: completed with {} chapters (locale={})",
                combined.chapter_count, combined.locale
            );
            eprintln!("output: {}", output_dir.display());
            eprintln!("checkpoint: {}", checkpoint_dir.display());
            let _ = store.save(checkpoint, &files);
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            let code = stage_exit_code(&e);
            eprintln!("error: combine failed: {e}");
            ExitCode::from(code)
        }
    }
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
            let content = redact_content(&content);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir helper for unit tests in main.rs.
    fn temp_dir(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("decon-cli-unit-{label}-{n}"))
    }

    #[test]
    fn load_file_config_toml_extension_parses_as_toml() {
        let dir = temp_dir("cfg-toml-ext");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("myconf.toml");
        std::fs::write(&path, b"language = \"es\"\n").unwrap();
        let cfg = load_file_config(Some(&path)).expect("toml should parse");
        assert_eq!(cfg.language.as_deref(), Some("es"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_yaml_extension_parses_as_yaml() {
        let dir = temp_dir("cfg-yaml-ext");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("myconf.yaml");
        std::fs::write(&path, b"language: fr\n").unwrap();
        let cfg = load_file_config(Some(&path)).expect("yaml should parse");
        assert_eq!(cfg.language.as_deref(), Some("fr"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_yml_extension_parses_as_yaml() {
        let dir = temp_dir("cfg-yml-ext");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("myconf.yml");
        std::fs::write(&path, b"language: de\n").unwrap();
        let cfg = load_file_config(Some(&path)).expect("yml should parse");
        assert_eq!(cfg.language.as_deref(), Some("de"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_no_extension_tries_both() {
        let dir = temp_dir("cfg-no-ext");
        std::fs::create_dir_all(&dir).unwrap();
        // Valid TOML content with no extension — "try both" should succeed via TOML.
        let path = dir.join("myconf");
        std::fs::write(&path, b"language = \"it\"\n").unwrap();
        let cfg = load_file_config(Some(&path)).expect("no-ext should try both");
        assert_eq!(cfg.language.as_deref(), Some("it"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_unknown_extension_tries_both() {
        let dir = temp_dir("cfg-json-ext");
        std::fs::create_dir_all(&dir).unwrap();
        // A .json extension is unknown — fall back to "try both". TOML parser
        // accepts this simple key=value-free JSON-ish content? No: JSON is not
        // valid TOML nor YAML. Use valid YAML content under a .json name so the
        // "try both" fallback resolves via the YAML parser.
        let path = dir.join("myconf.json");
        std::fs::write(&path, b"language: pt\n").unwrap();
        let cfg = load_file_config(Some(&path)).expect("unknown ext should try both");
        assert_eq!(cfg.language.as_deref(), Some("pt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_config_directory_path_returns_clear_error() {
        // A path whose final component has no file name (e.g. the filesystem
        // root `/`, or `..`) yields `file_name() == None`. The loader must
        // surface a clear error instead of silently falling through.
        let path = PathBuf::from("/");
        let err = load_file_config(Some(&path)).expect_err("dir path should error");
        // The error must mention the missing file name, not silently fall through.
        assert!(
            err.contains("file name") || err.contains("file_name"),
            "expected clear error about missing file name, got: {err}"
        );
    }

    #[test]
    fn load_tutorial_markdown_redacts_secrets() {
        let dir = temp_dir("eval-redact");
        std::fs::create_dir_all(&dir).unwrap();
        let content = "# Tutorial\n\nDB_KEY=dummyvalue\n";
        std::fs::write(dir.join("index.md"), content).unwrap();

        let files = load_tutorial_markdown(&dir).expect("load tutorial");
        assert_eq!(files.len(), 1);
        assert!(
            files[0].content.contains("DB_KEY=****"),
            "secret should be redacted: {}",
            files[0].content
        );
        assert!(
            !files[0].content.contains("dummyvalue"),
            "raw secret must not survive redaction"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_cache_root_uses_env_override() {
        let mut vars = BTreeMap::new();
        vars.insert("DECON_LLM_CACHE_DIR".into(), "/custom/cache".into());
        let root = resolve_cache_root(&vars, None);
        assert_eq!(root, Some(PathBuf::from("/custom/cache")));
    }

    #[test]
    fn resolve_cache_root_uses_config_cache_dir_when_no_env() {
        let vars = BTreeMap::new();
        let cfg_dir = PathBuf::from("/from/config");
        let root = resolve_cache_root(&vars, Some(&cfg_dir));
        assert_eq!(root, Some(PathBuf::from("/from/config")));
    }

    #[test]
    fn resolve_cache_root_env_overrides_config() {
        let mut vars = BTreeMap::new();
        vars.insert("DECON_LLM_CACHE_DIR".into(), "/env/wins".into());
        let cfg_dir = PathBuf::from("/from/config");
        let root = resolve_cache_root(&vars, Some(&cfg_dir));
        assert_eq!(root, Some(PathBuf::from("/env/wins")));
    }

    #[test]
    fn resolve_cache_root_default_when_nothing_set() {
        let vars = BTreeMap::new();
        let root = resolve_cache_root(&vars, None);
        assert!(root.is_some(), "cache should be enabled by default");
        let root = root.unwrap();
        assert!(
            root.ends_with("decon/llm-cache") || root.ends_with("decon\\llm-cache"),
            "default cache root should end with decon/llm-cache, got: {}",
            root.display()
        );
    }

    #[test]
    fn cache_disabled_when_no_cache_env_set() {
        let mut vars = BTreeMap::new();
        vars.insert("DECON_NO_CACHE".into(), "1".into());
        assert!(cache_is_disabled(&vars));
    }

    #[test]
    fn cache_not_disabled_when_no_cache_unset() {
        let vars = BTreeMap::new();
        assert!(!cache_is_disabled(&vars));
    }

    #[test]
    fn cache_not_disabled_when_no_cache_other_value() {
        let mut vars = BTreeMap::new();
        vars.insert("DECON_NO_CACHE".into(), "0".into());
        assert!(!cache_is_disabled(&vars));
    }

    #[test]
    fn cache_disabled_when_no_cache_true() {
        let mut vars = BTreeMap::new();
        vars.insert("DECON_NO_CACHE".into(), "true".into());
        assert!(cache_is_disabled(&vars));
    }

    #[test]
    fn cache_disabled_blank_env_not_disabled() {
        let mut vars = BTreeMap::new();
        vars.insert("DECON_NO_CACHE".into(), "".into());
        assert!(!cache_is_disabled(&vars));
    }
}
