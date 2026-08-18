//! `brigid` — deconstruct a codebase into an AI-generated tutorial.
//!
//! This binary only parses arguments and wires up library crates; business
//! logic lives in `brigid-core`, `brigid-crawl`, and `brigid-pipeline`.

#![allow(deprecated)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use brigid_core::{
    ChapterSummary, ChaptersOutput, CombineOutput, DEFAULT_EVAL_PASS_THRESHOLD, ModuleKey,
    OverviewOutput, ProgressTracker, RunConfig, SCHEMA_VERSION, SetupOutput, StageOutput,
    StageStats, StageStatus, TutorialFile, config_from_env_map, current_git_head,
    custom_host_warning, evaluate_tutorial, parse_toml_config, parse_yaml_config, redact_content,
    resolve_config, validate_config_for_check,
};
use brigid_crawl::{CrawlOptions, crawl_local, crawl_local_with_options};
use brigid_pipeline::{
    CheckpointStore, DryRunError, LlmClient, LlmError, MockClient, check_identity,
    dry_run_with_options, is_checkpoint_stale, next_stage, pending_stages,
};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use llm_kernel::store::kv::SqliteKvStore;
use std::sync::Arc;

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
/// so a subsequent `brigid resume` will re-run it with the partial work
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

/// Check whether the `BRIGID_NO_CACHE` env var disables the cache.
fn cache_is_disabled(vars: &BTreeMap<String, String>) -> bool {
    vars.get("BRIGID_NO_CACHE")
        .map(|s| {
            let trimmed = s.trim();
            trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Resolve the cache root directory from env, config, or the platform default.
///
/// Precedence: `BRIGID_LLM_CACHE_DIR` env var > `cache_dir` from config >
/// platform default (`<cache_dir>/brigid/llm-cache`).
fn resolve_cache_root(
    vars: &BTreeMap<String, String>,
    cfg_cache_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(v) = vars.get("BRIGID_LLM_CACHE_DIR") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(dir) = cfg_cache_dir {
        return Some(dir.to_path_buf());
    }
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".cache"));
    Some(base.join("brigid").join("llm-cache"))
}

/// Build a [`SqliteKvStore`] for LLM response caching from the environment and
/// run config, or `None` when the cache is disabled via `BRIGID_NO_CACHE=1`.
fn build_llm_cache(cfg: &RunConfig) -> Option<Arc<SqliteKvStore>> {
    let env_map: BTreeMap<String, String> = env::vars().collect();
    if cache_is_disabled(&env_map) {
        return None;
    }
    let root = resolve_cache_root(&env_map, cfg.cache_dir.as_deref())?;
    let _ = std::fs::create_dir_all(&root);
    let store = SqliteKvStore::open(&root.join("cache.sqlite"))
        .map_err(|e| {
            eprintln!(
                "warning: failed to open LLM cache at {}: {e}",
                root.display()
            )
        })
        .ok()?;
    Some(Arc::new(store))
}

/// Print cache status to stderr, including hit/miss stats when available.
fn print_cache_stats(
    cache: Option<&Arc<SqliteKvStore>>,
    stats: &brigid_pipeline::CacheStatsHandle,
) {
    if cache.is_some() {
        let s = stats.snapshot();
        if s.total() > 0 {
            eprintln!(
                "cache: enabled (sqlite kv-store, {} hits, {} misses, {:.0}% hit rate)",
                s.hits,
                s.misses,
                s.hit_rate_percent(),
            );
        } else {
            eprintln!("cache: enabled (sqlite kv-store)");
        }
    }
}

/// Whether a `BRIGID_FORCE_MOCK` value should enable forced mock output.
///
/// Recognized falsy values (case-insensitive, after trimming): `0`, `false`,
/// `no`, `off`, and empty/whitespace. Any other non-blank value enables
/// forced mock output. `None` (env var unset) is disabled.
fn is_force_mock_enabled(value: Option<&str>) -> bool {
    value
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| !matches!(v.as_str(), "" | "0" | "false" | "no" | "off"))
}

/// Whether the user explicitly requested deterministic placeholder output.
fn force_mock_client() -> bool {
    is_force_mock_enabled(env::var("BRIGID_FORCE_MOCK").ok().as_deref())
}

/// Build a live [`LlmClient`] from the environment and optional `RunConfig`
/// provider/model overrides, optionally with a SQLite response cache.
///
/// Delegates to [`brigid_pipeline::build_live_client`] which lives in the
/// library crate so the provider resolution, key-chain, and host-allowlist
/// logic is unit-tested independently of the CLI.
///
/// When a cache is configured, [`brigid_pipeline::build_live_client`] wraps
/// the `KvStore` in [`brigid_pipeline::CountingKvStore`] so cache hit/miss
/// statistics can be reported in verbose output. The
/// [`brigid_pipeline::CacheStatsHandle`] is returned alongside the client
/// for later reporting.
///
/// Callers must only select a mock client when [`force_mock_client`] returns
/// `true`. Missing credentials or invalid client configuration are surfaced as
/// errors so a successful command never emits placeholder output by accident.
fn build_real_llm_client(
    cache: Option<Arc<SqliteKvStore>>,
    custom_hosts: &[String],
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(Box<dyn LlmClient>, brigid_pipeline::CacheStatsHandle), String> {
    if let Some(msg) = custom_host_warning(custom_hosts) {
        eprintln!("{msg}");
    }
    brigid_pipeline::build_live_client(cache, provider, model, custom_hosts)
}

// ---------------------------------------------------------------------------
// Forced-mock placeholder responses
//
// These constants and the `mock_client` helper centralise the deterministic
// placeholder output used when `BRIGID_FORCE_MOCK` is set.  Keeping them in one
// place avoids the three-way duplication that previously existed across
// `cmd_identify`, `cmd_generate`, and `cmd_generate_each_app`.
// ---------------------------------------------------------------------------

/// Placeholder identify YAML — a single trivial abstraction.
const PLACEHOLDER_IDENTIFY_YAML: &str = "```yaml\n- name: \"Placeholder\"\n  description: \"Auto-generated placeholder abstraction\"\n  \
     file_indices: [0]\n  tier: \"S\"\n  kind: \"module\"\n  apps: []\n  entry_files: []\n```\n";

/// Placeholder relationships YAML — empty relationship list.
const PLACEHOLDER_RELATIONSHIPS_YAML: &str =
    "```yaml\nsummary: \"Placeholder project summary.\"\nrelationships: []\n```\n";

/// Placeholder chapter order YAML — a single chapter (abstraction 0).
const PLACEHOLDER_ORDER_YAML: &str = "```yaml\n- 0\n```\n";

/// Placeholder chapter Markdown body.
const PLACEHOLDER_CHAPTER: &str = "# Chapter 1: Placeholder\n\n## Motivation\n- Need \
    placeholder\n\n## Core idea\nPlaceholder is key.\n\n## Summary\nWe learned about \
    placeholder.\n";

/// Placeholder setup guide Markdown body.
const PLACEHOLDER_SETUP: &str = "# Setup: project\n\n## Prerequisites\n\nInstall dependencies.\n\n## Run\n\n```bash\nmake \
     run\n```\n";

/// Placeholder architecture overview Markdown body.
const PLACEHOLDER_OVERVIEW: &str = "# Architecture Overview\n\nThis project has multiple \
    modules.\n";

fn mock_fail_error(kind: &str) -> LlmError {
    match kind {
        "timeout" => LlmError::Timeout,
        "ratelimit" => LlmError::RateLimit { retry_after: None },
        "provider" => LlmError::Provider {
            status: 502,
            body: "mock provider error".to_string(),
        },
        "parse" => LlmError::parse("mock parse failure"),
        _ => LlmError::network("mock network failure"),
    }
}

/// Build a mock [`LlmClient`] from a pre-assembled response sequence.
///
/// In `debug_assertions` builds, the `BRIGID_LLM_MOCK_FAIL` environment variable
/// can inject a typed error on the first call instead of returning placeholder
/// responses.  This is a developer-only fault-injection hook and is compiled out
/// of release builds.
fn mock_client(responses: Vec<String>) -> Box<dyn LlmClient> {
    mock_client_with_diag(responses, &mut std::io::stderr())
}

/// Like [`mock_client`] but writes the fallback warning to `diag` instead of
/// stderr, so tests can assert the diagnostic without capturing the process
/// stderr.
fn mock_client_with_diag(
    responses: Vec<String>,
    diag: &mut dyn std::io::Write,
) -> Box<dyn LlmClient> {
    let fallback = PLACEHOLDER_IDENTIFY_YAML;
    #[cfg(debug_assertions)]
    {
        if let Some(kind) = env::var("BRIGID_LLM_MOCK_FAIL")
            .ok()
            .filter(|value| !value.is_empty())
        {
            let error = mock_fail_error(&kind);
            return Box::new(MockClient::new("").fail_on(0, error));
        }
    }
    Box::new(
        MockClient::with_responses(responses).unwrap_or_else(|error| {
            let _ = writeln!(
                diag,
                "warning: mock client: falling back to default placeholder response: {error}"
            );
            MockClient::new(fallback)
        }),
    )
}

/// Deconstruct a codebase into an AI-generated tutorial.
#[derive(Parser, Debug)]
#[command(name = "brigid", version, about, long_about = None)]
struct Cli {
    /// Optional path to `brigid.toml` or `.brigid.yaml` (else discover in cwd).
    #[arg(long = "config", global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

/// Subcommand for `brigid cache`.
#[derive(Subcommand, Debug)]
enum CacheAction {
    /// Delete all cached LLM responses.
    Prune,
    /// Print cache entry count and on-disk size.
    Stats,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Write a starter `brigid.toml` in the current or given directory.
    Init {
        /// Directory for the config file (default: `.`).
        #[arg(long = "dir", value_name = "PATH", default_value = ".")]
        dir: PathBuf,
        /// Write defaults without prompting (for CI/scripts).
        #[arg(long = "non-interactive", default_value_t = false)]
        non_interactive: bool,
        /// Validate an existing `brigid.toml` and report issues.
        #[arg(long = "check", default_value_t = false)]
        check: bool,
    },
    /// List relative file inventory under a directory (no LLM).
    Crawl {
        /// Repository root to crawl.
        #[arg(long = "dir", value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Git ref (tag, commit, or branch) to diff against. When set, only
        /// files changed since this ref are listed (requires a git repo).
        #[arg(long = "since", value_name = "REF")]
        since: Option<String>,
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
        /// Git ref (tag, commit, or branch) to diff against. When set, only
        /// files changed since this ref are included (requires a git repo).
        #[arg(long = "since", value_name = "REF")]
        since: Option<String>,
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
        /// Checkpoint directory to write (default: `.brigid-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Use single-shot mode (one LLM call) instead of map+reduce.
        #[arg(long = "single-shot", default_value_t = false)]
        single_shot: bool,
        /// Maximum abstractions to return.
        #[arg(long = "max-abstractions", default_value_t = 10)]
        max_abstractions: usize,
        /// Git ref (tag, commit, or branch) to diff against. When set, only
        /// files changed since this ref are crawled (requires a git repo).
        #[arg(long = "since", value_name = "REF")]
        since: Option<String>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
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
        /// Checkpoint directory (default: `.brigid-checkpoint`).
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
        /// Fail the overview stage if the LLM mentions app paths not in the
        /// inventory. By default, invented apps are reported as a warning but
        /// the overview is still generated.
        #[arg(long = "strict-app-validation", default_value_t = false)]
        strict_app_validation: bool,
        /// Tutorial writing style: `blog-post` (short, conversational — default)
        /// or `book` (long-form, multi-section chapters with more diagrams).
        #[arg(
            long = "tutorial-style",
            value_name = "STYLE",
            default_value = "blog-post"
        )]
        tutorial_style: String,

        /// Maximum concurrent chapter writes (overrides config default of 4).
        #[arg(long = "concurrency", value_name = "N")]
        concurrency: Option<usize>,
        /// Hard ceiling on LLM calls for this run (overrides budget from
        /// config/env).
        #[arg(long = "max-llm-calls", value_name = "N")]
        max_llm_calls: Option<u32>,
        /// Git ref (tag, commit, or branch) to diff against. When set, only
        /// files changed since this ref are crawled (requires a git repo).
        #[arg(long = "since", value_name = "REF")]
        since: Option<String>,
        /// Detailed progress output: stage timing, LLM call count, cache
        /// stats, checkpoint path. Sent to stderr so stdout piping is
        /// unaffected. Mutually exclusive with `--quiet`.
        #[arg(long = "verbose", default_value_t = false)]
        verbose: bool,
        /// Minimal output: errors only, no progress messages. Mutually
        /// exclusive with `--verbose`.
        #[arg(long = "quiet", default_value_t = false)]
        quiet: bool,
        /// Output format: human-readable text (default) or machine-readable
        /// JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run only the relationships stage (reads identify from checkpoint).
    Relationships {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.brigid-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run only the order stage (reads identify + relationships from checkpoint).
    Order {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.brigid-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run only the chapters stage (reads identify + relationships + order from checkpoint).
    Chapters {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.brigid-checkpoint`).
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
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run only the setup guide stage (reads identify + dry-run from checkpoint).
    Setup {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.brigid-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Force generation even if the setup score is high.
        #[arg(long = "force", default_value_t = false)]
        force: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run only the architecture overview stage (reads identify + relationships from checkpoint).
    Overview {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.brigid-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run only the combine stage (reads all prior outputs from checkpoint).
    Combine {
        /// Repository root (required).
        #[arg(long = "dir", value_name = "PATH", required = true)]
        dir: PathBuf,
        /// Checkpoint directory (default: `.brigid-checkpoint`).
        #[arg(long = "checkpoint-dir", value_name = "PATH")]
        checkpoint_dir: Option<PathBuf>,
        /// Output directory (default: `output`).
        #[arg(long = "output-dir", value_name = "PATH")]
        output_dir: Option<PathBuf>,
        /// Output language (default: `en`).
        #[arg(long = "language", value_name = "LANG", default_value = "en")]
        language: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Manage the LLM response cache (prune or show stats).
    ///
    /// The cache is a SQLite database at `<cache-dir>/cache.sqlite`. Use
    /// `brigid cache prune` to delete all cached responses, or `brigid cache
    /// stats` to show the entry count and on-disk size.
    Cache {
        /// Action: `prune` deletes all cached entries; `stats` prints the
        /// entry count and on-disk size.
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Generate a troff-formatted man page for `brigid` to stdout.
    ///
    /// The man page documents every subcommand and flag. Use `--output PATH`
    /// to write directly to a file instead of stdout.
    Manpage {
        /// Write the man page to PATH instead of stdout.
        #[arg(long = "output", value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Print shell completion script for bash, zsh, fish, or PowerShell.
    ///
    /// The script is written to stdout by default. Use `--output PATH` to
    /// write directly to a file. This subcommand does not require `--dir` or
    /// any other run-time argument.
    Completions {
        /// Target shell.
        #[arg(long = "shell", value_enum)]
        shell: ShellKind,
        /// Optional file path. When set, the completion script is written
        /// there instead of stdout.
        #[arg(long = "output", value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

/// Supported shells for `brigid completions`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ShellKind {
    /// Bourne-again shell.
    Bash,
    /// Z shell.
    Zsh,
    /// Friendly interactive shell.
    Fish,
    /// PowerShell.
    #[value(name = "powershell")]
    PowerShell,
}

impl ShellKind {
    /// Convert to the matching `clap_complete::Shell` variant.
    fn to_completion_shell(self) -> clap_complete::Shell {
        match self {
            ShellKind::Bash => clap_complete::Shell::Bash,
            ShellKind::Zsh => clap_complete::Shell::Zsh,
            ShellKind::Fish => clap_complete::Shell::Fish,
            ShellKind::PowerShell => clap_complete::Shell::PowerShell,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

/// Output verbosity level for `brigid generate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verbosity {
    /// Minimal output: errors only, no progress messages.
    Quiet,
    /// Normal output: progress messages and warnings.
    Normal,
    /// Detailed output: stage timing, LLM call count, cache stats, checkpoint path.
    Verbose,
}

impl Verbosity {
    /// Returns `true` if progress messages should be printed.
    fn show_progress(self) -> bool {
        !matches!(self, Verbosity::Quiet)
    }

    /// Returns `true` if verbose detail should be printed.
    fn is_verbose(self) -> bool {
        matches!(self, Verbosity::Verbose)
    }
}

/// Print a progress message to stderr unless in quiet mode.
fn print_progress(verbosity: Verbosity, msg: &str) {
    if verbosity.show_progress() {
        eprintln!("{msg}");
    }
}

/// Print a verbose-only message to stderr.
fn verbose_msg(verbosity: Verbosity, msg: &str) {
    if verbosity.is_verbose() {
        eprintln!("verbose: {msg}");
    }
}

/// Serialize a value to JSON, pretty-printing when stdout is a TTY and
/// emitting compact JSON when piped.
///
/// # Errors
///
/// Propagates `serde_json` serialization errors.
fn serialize_json_for_stdout<T: ?Sized + serde::Serialize>(
    value: &T,
) -> Result<String, serde_json::Error> {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
}

/// Print a JSON-serialized value to stdout, pretty-printing when stdout is a
/// TTY and emitting compact JSON when piped.
fn print_json<T: ?Sized + serde::Serialize>(value: &T) {
    match serialize_json_for_stdout(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: json serialization failed: {e}"),
    }
}

/// Format a [`std::time::Duration`] as a human-readable string (e.g. `"12ms"`,
/// `"1.5s"`).
fn fmt_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Print verbose summary after a generate run: stage timings, LLM call count,
/// cache stats, and checkpoint path.
fn print_verbose_summary(
    progress: &brigid_core::ProgressTracker,
    cache: Option<&Arc<SqliteKvStore>>,
    cache_stats: &brigid_pipeline::CacheStatsHandle,
    checkpoint_dir: &Path,
) {
    let snap = progress.snapshot();
    verbose_msg(
        Verbosity::Verbose,
        &format!("llm-calls: {}/{}", snap.llm_calls_used, snap.max_llm_calls),
    );
    for timing in progress.stage_timings() {
        verbose_msg(
            Verbosity::Verbose,
            &format!("stage {}: {}", timing.stage, fmt_duration(timing.elapsed)),
        );
    }
    if cache.is_some() {
        let stats = cache_stats.snapshot();
        if stats.total() > 0 {
            verbose_msg(
                Verbosity::Verbose,
                &format!(
                    "cache: enabled (sqlite kv-store, {} hits, {} misses, {:.0}% hit rate)",
                    stats.hits,
                    stats.misses,
                    stats.hit_rate_percent(),
                ),
            );
        } else {
            verbose_msg(Verbosity::Verbose, "cache: enabled (sqlite kv-store)");
        }
    }
    verbose_msg(
        Verbosity::Verbose,
        &format!("checkpoint: {}", checkpoint_dir.display()),
    );
}

/// Map a [`brigid_pipeline::GenerateError`] to an actionable suggestion string.
fn error_suggestion(err: &brigid_pipeline::GenerateError) -> Option<String> {
    match err {
        brigid_pipeline::GenerateError::Budget(_)
        | brigid_pipeline::GenerateError::Identify(
            brigid_pipeline::identify::IdentifyError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Relationships(
            brigid_pipeline::relationships::RelationshipsError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Order(brigid_pipeline::order::OrderError::Budget(_))
        | brigid_pipeline::GenerateError::Chapters(
            brigid_pipeline::chapters::ChaptersError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Review(
            brigid_pipeline::review::ReviewError::Budget(_),
        ) => Some(
            "Increase the budget with --max-llm-calls N or BRIGID_MAX_LLM_CALLS, \
             then run 'brigid resume --checkpoint-dir <path>' to continue from the last completed stage."
                .to_string(),
        ),
        brigid_pipeline::GenerateError::Identify(
            brigid_pipeline::identify::IdentifyError::Llm(_)
            | brigid_pipeline::identify::IdentifyError::LlmBatch { .. },
        )
        | brigid_pipeline::GenerateError::Relationships(
            brigid_pipeline::relationships::RelationshipsError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Order(brigid_pipeline::order::OrderError::Llm(_))
        | brigid_pipeline::GenerateError::Chapters(
            brigid_pipeline::chapters::ChaptersError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Review(
            brigid_pipeline::review::ReviewError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Setup(
            brigid_pipeline::setup_guide::SetupGuideError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Overview(
            brigid_pipeline::overview::OverviewError::Llm(_),
        ) => Some(
            "Check your network connection and BRIGID_LLM_API_KEY. \
             Run with --checkpoint-dir to resume from the last completed stage."
                .to_string(),
        ),
        brigid_pipeline::GenerateError::Config(_) => Some(
            "Verify your brigid.toml / .brigid.yaml and CLI flags. \
             Run 'brigid dry-run --dir <path>' to validate the project setup."
                .to_string(),
        ),
        brigid_pipeline::GenerateError::Crawl(_) => Some(
            "Ensure the --dir path exists and is readable. \
             Run 'brigid crawl --dir <path>' to inspect the file inventory."
                .to_string(),
        ),
        brigid_pipeline::GenerateError::Checkpoint(_) => Some(
            "Check disk space and permissions on the checkpoint directory. \
             Run with --checkpoint-dir to specify a writable location."
                .to_string(),
        ),
        _ => Some(
            "Run with --checkpoint-dir to resume from the last completed stage."
                .to_string(),
        ),
    }
}

/// Print an error with an actionable suggestion (to stderr, always shown).
fn print_error(prefix: &str, err: &brigid_pipeline::GenerateError) {
    eprintln!("error: {prefix}: {err}");
    if let Some(hint) = error_suggestion(err) {
        eprintln!("hint: {hint}");
    }
}

/// Troff content for the EXAMPLES section of the man page.
const MAN_EXAMPLES: &str = "\
.brigid crawl --dir ./my-project
.brigid dry-run --dir ./my-project --format json
.brigid generate --dir ./my-project --output-dir ./tutorial --language en
.brigid generate --dir ./monorepo --each-app --review-chapters
.brigid resume --checkpoint .brigid-checkpoint --format json
.brigid eval --out ./tutorial --threshold 80
";

/// Troff content for the ENVIRONMENT section of the man page.
const MAN_ENVIRONMENT: &str = "\
BRIGID_LLM_API_KEY
  API key for the LLM provider (checked first; falls back to DEEPSEEK_API_KEY).
DEEPSEEK_API_KEY
  Fallback API key for the LLM provider.
BRIGID_LLM_BASE_URL
  OpenAI-compatible endpoint URL (default: https://api.deepseek.com/v1).
BRIGID_LLM_MODEL
  Model identifier sent in requests (default: deepseek-chat).
BRIGID_LLM_MAX_TOKENS
  Output token cap sent as max_tokens (default: 8192). Raise if responses
  are truncated; lower to cut cost.
BRIGID_LLM_ALLOWED_HOSTS
  Comma-separated extra hosts for the Authorization-header allowlist.
BRIGID_LLM_CACHE_DIR
  Disk cache root directory for LLM responses.
BRIGID_NO_CACHE
  Set to 1 or true to disable the disk cache. Use 'brigid cache prune' to
  delete all cached responses, or 'brigid cache stats' to inspect the cache.
BRIGID_FORCE_MOCK
  Set to force the mock LLM client (offline). Falsy values (0, false, no,
  off, blank; case-insensitive) do NOT enable mock mode.
BRIGID_SINCE
  Git ref (tag, commit, or branch) for incremental git-diff crawl.
";

/// Troff content for the FILES section of the man page.
const MAN_FILES: &str = "\
brigid.toml
  Project configuration file (TOML format).
.brigid.yaml / .brigid.yml
  Alternative project configuration file (YAML format).
.brigid-checkpoint/
  Default checkpoint directory (checkpoint.json + files.ndjson.gz).
output/
  Default output directory for generated tutorials.
";

/// Troff content for the EXIT STATUS section of the man page.
const MAN_EXIT_STATUS: &str = "\
0  Success.
1  Generic failure (including structural eval fail).
2  Config / path / I/O input error.
3  LLM call budget exceeded (fail-closed on max-LLM-call ceiling).
4  LLM provider error (network, timeout, rate-limit, provider, parse).
5  Partial success with checkpoint: the stage was cancelled (Ctrl+C / SIGTERM)
   mid-flight, but a partial checkpoint was written. Resume to continue.
";

/// Troff content for the SEE ALSO section of the man page.
const MAN_SEE_ALSO: &str = "\
brigid(5), cargo(1)
";

/// Append a custom troff `.SH` section to `buf`.
fn append_man_section(buf: &mut Vec<u8>, title: &str, body: &str) {
    use std::io::Write;
    let _ = writeln!(buf, ".SH \"{title}\"");
    // Each non-empty line becomes a troff paragraph/line. We use .br to
    // preserve line breaks within the section body.
    for line in body.lines() {
        if line.is_empty() {
            let _ = writeln!(buf, ".br");
        } else {
            let _ = writeln!(buf, "{line}");
        }
    }
}

/// Generate the complete man page (troff) for the `brigid` CLI.
///
/// Uses `clap_mangen` to render the standard sections (NAME, SYNOPSIS,
/// DESCRIPTION, OPTIONS, COMMANDS) from the `clap::Command` struct, then
/// appends custom sections (EXAMPLES, ENVIRONMENT, FILES, EXIT STATUS,
/// SEE ALSO) that `clap_mangen` does not generate automatically.
fn generate_man_page() -> Vec<u8> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    let mut buf: Vec<u8> = Vec::new();
    // Render the standard sections from the clap Command.
    man.render(&mut buf)
        .expect("clap_mangen render should not fail for a valid Command");
    // Append custom sections that clap_mangen does not generate.
    append_man_section(&mut buf, "EXAMPLES", MAN_EXAMPLES);
    append_man_section(&mut buf, "ENVIRONMENT", MAN_ENVIRONMENT);
    append_man_section(&mut buf, "FILES", MAN_FILES);
    append_man_section(&mut buf, "EXIT STATUS", MAN_EXIT_STATUS);
    append_man_section(&mut buf, "SEE ALSO", MAN_SEE_ALSO);
    buf
}

/// Run the `brigid manpage` subcommand.
///
/// Generates a troff-formatted man page and writes it to stdout (default)
/// or to the file specified by `--output PATH`.
fn cmd_manpage(output: Option<PathBuf>) -> ExitCode {
    let buf = generate_man_page();
    match output {
        Some(path) => match fs::write(&path, &buf) {
            Ok(()) => {
                println!("wrote {}", path.display());
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                eprintln!("error: write {}: {e}", path.display());
                ExitCode::from(EXIT_CONFIG)
            }
        },
        None => {
            use std::io::Write;
            match std::io::stdout().write_all(&buf) {
                Ok(()) => ExitCode::from(EXIT_OK),
                Err(e) => {
                    eprintln!("error: write stdout: {e}");
                    ExitCode::from(EXIT_FAIL)
                }
            }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The `manpage` subcommand is a pure documentation generator: it does
    // not need a config file, LLM client, or repository. Handle it before
    // loading config so a broken `brigid.toml` in the cwd doesn't block it.
    if let Commands::Manpage { output } = cli.command {
        return cmd_manpage(output);
    }

    // The `completions` subcommand is a pure help/utility command: it must
    // work even when no `brigid.toml` is present or the cwd contains a broken
    // config file, so skip loading the merged config for it entirely.
    let cfg = if !matches!(cli.command, Commands::Completions { .. }) {
        // Build a CLI overlay with the `--since` flag so it participates in
        // config layering (CLI > file > env > defaults).
        let cli_overlay = RunConfig {
            since: cli_since(&cli.command),
            ..RunConfig::empty()
        };
        match load_merged_config(cli.config.as_deref(), &cli_overlay) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: config: {e}");
                return ExitCode::from(EXIT_CONFIG);
            }
        }
    } else {
        RunConfig::empty()
    };

    match cli.command {
        Commands::Init {
            dir,
            non_interactive,
            check,
        } => cmd_init(&dir, non_interactive, check),
        Commands::Crawl {
            dir,
            since: _,
            format,
        } => {
            let dir = dir
                .or_else(|| cfg.root.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            cmd_crawl(&dir, cfg.since.as_deref(), format)
        }
        Commands::DryRun {
            dir,
            apps,
            since: _,
            format,
        } => {
            let dir = dir
                .or_else(|| cfg.root.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let apps = if apps.is_empty() {
                cfg.apps.clone().unwrap_or_default()
            } else {
                apps
            };
            cmd_dry_run(&dir, &apps, cfg.since.as_deref(), format)
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
            since: _,
            format,
        } => {
            let dir = dir
                .or_else(|| cfg.root.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            cmd_identify(
                &dir,
                &checkpoint_dir,
                single_shot,
                max_abstractions,
                &cfg,
                format,
            )
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
            strict_app_validation,
            tutorial_style,

            concurrency,
            max_llm_calls,
            since: _,
            verbose,
            quiet,
            format,
        } => {
            if verbose && quiet {
                eprintln!(
                    "error: --verbose and --quiet are mutually exclusive. \
                     Specify at most one of these flags."
                );
                return ExitCode::from(EXIT_CONFIG);
            }
            let verbosity = if quiet {
                Verbosity::Quiet
            } else if verbose {
                Verbosity::Verbose
            } else {
                Verbosity::Normal
            };
            // Validate positive-integer flags (clap parses them, but 0 is
            // semantically invalid for concurrency and max-llm-calls).
            if let Some(n) = concurrency
                && n == 0
            {
                eprintln!("error: --concurrency must be a positive integer (got 0)");
                return ExitCode::from(EXIT_CONFIG);
            }
            if let Some(n) = max_llm_calls
                && n == 0
            {
                eprintln!("error: --max-llm-calls must be a positive integer (got 0)");
                return ExitCode::from(EXIT_CONFIG);
            }
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
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
                strict_app_validation,
                &tutorial_style,
                concurrency,
                max_llm_calls,
                verbosity,
                format,
                &cfg,
            )
        }
        Commands::Relationships {
            dir,
            checkpoint_dir,
            format,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            cmd_relationships(&dir, &checkpoint_dir, &cfg, format)
        }
        Commands::Order {
            dir,
            checkpoint_dir,
            format,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            cmd_order(&dir, &checkpoint_dir, &cfg, format)
        }
        Commands::Chapters {
            dir,
            checkpoint_dir,
            output_dir,
            language,
            diagram_level,
            format,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            let output_dir = output_dir
                .or_else(|| cfg.output.clone())
                .unwrap_or_else(|| PathBuf::from("output"));
            cmd_chapters(
                &dir,
                &checkpoint_dir,
                &output_dir,
                &language,
                &diagram_level,
                format,
                cfg.max_llm_calls,
            )
        }
        Commands::Setup {
            dir,
            checkpoint_dir,
            force,
            format,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            cmd_setup(&dir, &checkpoint_dir, force, &cfg, format)
        }
        Commands::Overview {
            dir,
            checkpoint_dir,
            format,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            cmd_overview(&dir, &checkpoint_dir, &cfg, format)
        }
        Commands::Combine {
            dir,
            checkpoint_dir,
            output_dir,
            language,
            format,
        } => {
            let checkpoint_dir =
                checkpoint_dir.unwrap_or_else(|| PathBuf::from(".brigid-checkpoint"));
            let output_dir = output_dir
                .or_else(|| cfg.output.clone())
                .unwrap_or_else(|| PathBuf::from("output"));
            cmd_combine(&dir, &checkpoint_dir, &output_dir, &language, &cfg, format)
        }
        Commands::Cache { action } => cmd_cache(action, &cfg),
        // `Manpage` is handled before config loading (see top of `main`),
        // so this arm is unreachable.
        Commands::Manpage { .. } => unreachable!("manpage handled before config load"),
        Commands::Completions { shell, output } => cmd_completions(shell, output),
    }
}

/// Extract the `--since` CLI flag value from a subcommand for config layering.
///
/// Returns `Some(ref)` when `--since` was passed on the command line, or
/// `None` when it was absent (so file/env layers can supply it).
fn cli_since(command: &Commands) -> Option<String> {
    match command {
        Commands::Crawl { since, .. }
        | Commands::DryRun { since, .. }
        | Commands::Identify { since, .. }
        | Commands::Generate { since, .. } => since.clone(),
        _ => None,
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
        // explicit fallback for extensionless files like `brigid` or unknown
        // extensions like `.json`.
        _ => parse_toml_config(&text)
            .or_else(|_| parse_yaml_config(&text))
            .map_err(|e| e.to_string()),
    }
}

fn discover_config_file() -> Option<PathBuf> {
    for name in ["brigid.toml", ".brigid.yaml", ".brigid.yml"] {
        let p = PathBuf::from(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Default values shown in the wizard and used when input is skipped.
const DEFAULT_LANGUAGE: &str = "en";
const DEFAULT_DIAGRAM_LEVEL: &str = "standard";
const DEFAULT_MAX_ABSTRACTIONS: usize = 10;
const DEFAULT_CONCURRENCY: usize = 4;
const DEFAULT_CACHE_SIZE_MB: usize = 100;

/// Generate the comprehensive `brigid.toml` template with all options commented
/// out (except the header). When `answers` is provided, the selected options
/// are uncommented.
fn generate_config_template(answers: &WizardAnswers) -> String {
    let mut out = String::new();

    out.push_str("# brigid configuration file\n");
    out_str(&mut out, "#");
    out_str(
        &mut out,
        "# Precedence: CLI flags > this file > BRIGID_* env vars > built-in defaults.",
    );
    out_str(&mut out, "#");
    out_str(
        &mut out,
        "# API keys are read from BRIGID_LLM_API_KEY env var only — never put them here.",
    );
    out_str(&mut out, "#");

    // --- Core paths ---
    out_str(&mut out, "# Repository root (default: \".\")");
    maybe_write_str(&mut out, "root", &answers.root, ".");
    out_str(&mut out, "");
    out_str(
        &mut out,
        "# Output directory for generated tutorials (default: \"output\")",
    );
    maybe_write_str(&mut out, "output", &answers.output, "output");
    out_str(&mut out, "");

    // --- Language & diagram ---
    out_str(
        &mut out,
        "# Tutorial language / locale code, e.g. \"en\", \"es\", \"fr\" (default: \"en\")",
    );
    maybe_write_str(&mut out, "language", &answers.language, DEFAULT_LANGUAGE);
    out_str(&mut out, "");
    out_str(
        &mut out,
        "# Diagram richness: \"minimal\", \"standard\", or \"rich\" (default: \"standard\")",
    );
    maybe_write_str(
        &mut out,
        "diagram_level",
        &answers.diagram_level,
        DEFAULT_DIAGRAM_LEVEL,
    );
    out_str(&mut out, "");

    // --- Abstractions & concurrency ---
    out_str(
        &mut out,
        "# Maximum number of abstractions (chapters) to identify (default: 10)",
    );
    maybe_write_usize(
        &mut out,
        "max_abstractions",
        answers.max_abstractions,
        DEFAULT_MAX_ABSTRACTIONS,
    );
    out_str(&mut out, "");
    out_str(&mut out, "# Maximum concurrent chapter writes (default: 4)");
    maybe_write_usize(
        &mut out,
        "concurrency",
        answers.concurrency,
        DEFAULT_CONCURRENCY,
    );
    out_str(&mut out, "");

    // --- Budget ---
    out_str(
        &mut out,
        "# Hard ceiling on LLM calls per run (default: 200)",
    );
    maybe_write_opt_u32(&mut out, "max_llm_calls", answers.max_llm_calls);
    out_str(&mut out, "");

    // --- Provider / model ---
    out_str(
        &mut out,
        "# LLM provider id (optional, e.g. \"openai\", \"deepseek\")",
    );
    maybe_write_opt_str(&mut out, "provider", answers.provider.as_deref());
    out_str(&mut out, "");
    out_str(
        &mut out,
        "# Model id for the provider (optional, e.g. \"deepseek-chat\")",
    );
    maybe_write_opt_str(&mut out, "model", answers.model.as_deref());
    out_str(&mut out, "");

    // --- Cache ---
    out_str(
        &mut out,
        "# Disk cache directory for LLM responses (default: platform cache dir)",
    );
    maybe_write_opt_str(&mut out, "cache_dir", answers.cache_dir.as_deref());
    out_str(&mut out, "");
    out_str(
        &mut out,
        "# Disk cache size limit in megabytes (default: 100)",
    );
    maybe_write_opt_usize(&mut out, "cache_size_limit_mb", answers.cache_size_limit_mb);
    out_str(&mut out, "");

    // --- Advanced ---
    out_str(
        &mut out,
        "# Checkpoint directory (default: \".brigid-checkpoint\")",
    );
    maybe_write_opt_str(
        &mut out,
        "checkpoint_dir",
        answers.checkpoint_dir.as_deref(),
    );
    out_str(&mut out, "");
    out_str(
        &mut out,
        "# Soft per-batch character budget for dry-run packing (optional)",
    );
    maybe_write_opt_usize(&mut out, "batch_char_budget", answers.batch_char_budget);
    out_str(&mut out, "");
    out_str(
        &mut out,
        "# Chars-per-token heuristic for token estimates (default: 4)",
    );
    maybe_write_opt_usize(&mut out, "chars_per_token", answers.chars_per_token);
    out_str(&mut out, "");

    // --- Apps ---
    out_str(
        &mut out,
        "# Monorepo app/module scope keys, e.g. [\"apps/alpha\"] (default: [])",
    );
    maybe_write_array(&mut out, "apps", &answers.apps);
    out_str(&mut out, "");

    // --- Allowed hosts ---
    out_str(
        &mut out,
        "# Additional LLM provider hosts allowed to receive the Authorization header.",
    );
    out_str(
        &mut out,
        "# Defaults: api.openai.com, api.deepseek.com, localhost, 127.0.0.1.",
    );
    out_str(
        &mut out,
        "# Also extendable via the BRIGID_ALLOWED_HOSTS env var (comma-separated).",
    );
    if !answers.allowed_hosts.is_empty() {
        for host in &answers.allowed_hosts {
            out_str(&mut out, &format!("[[allowed_hosts]]\n# host = \"{host}\""));
        }
    } else {
        out_str(&mut out, "# [[allowed_hosts]]");
        out_str(&mut out, "# host = \"my-proxy.internal\"");
    }
    out_str(&mut out, "");

    // --- Incremental crawl ---
    out_str(
        &mut out,
        "# Git ref (tag, commit, or branch) for incremental git-diff crawl (optional).",
    );
    out_str(
        &mut out,
        "# When set, only files changed since this ref are crawled. Also settable",
    );
    out_str(&mut out, "# via BRIGID_SINCE env var or --since CLI flag.");
    maybe_write_opt_str(&mut out, "since", answers.since.as_deref());

    out
}

fn out_str(buf: &mut String, s: &str) {
    buf.push_str(s);
    buf.push('\n');
}

fn maybe_write_str(buf: &mut String, key: &str, value: &str, default: &str) {
    if value == default {
        out_str(buf, &format!("# {key} = \"{value}\""));
    } else {
        out_str(buf, &format!("{key} = \"{value}\""));
    }
}

fn maybe_write_usize(buf: &mut String, key: &str, value: usize, default: usize) {
    if value == default {
        out_str(buf, &format!("# {key} = {value}"));
    } else {
        out_str(buf, &format!("{key} = {value}"));
    }
}

fn maybe_write_opt_u32(buf: &mut String, key: &str, value: Option<u32>) {
    match value {
        None => out_str(buf, &format!("# {key} = 200")),
        Some(v) => out_str(buf, &format!("{key} = {v}")),
    }
}

fn maybe_write_opt_str(buf: &mut String, key: &str, value: Option<&str>) {
    match value {
        None => out_str(buf, &format!("# {key} = \"...\"")),
        Some(v) => out_str(buf, &format!("{key} = \"{v}\"")),
    }
}

fn maybe_write_opt_usize(buf: &mut String, key: &str, value: Option<usize>) {
    match value {
        None => out_str(buf, &format!("# {key} = ...")),
        Some(v) => out_str(buf, &format!("{key} = {v}")),
    }
}

fn maybe_write_array(buf: &mut String, key: &str, value: &[String]) {
    if value.is_empty() {
        out_str(buf, &format!("# {key} = []"));
    } else {
        let items: Vec<String> = value.iter().map(|s| format!("\"{s}\"")).collect();
        out_str(buf, &format!("{key} = [{}]", items.join(", ")));
    }
}

/// Answers collected from the interactive wizard.
#[derive(Clone, Debug, Default)]
struct WizardAnswers {
    root: String,
    output: String,
    language: String,
    diagram_level: String,
    max_abstractions: usize,
    concurrency: usize,
    max_llm_calls: Option<u32>,
    provider: Option<String>,
    model: Option<String>,
    cache_dir: Option<String>,
    cache_size_limit_mb: Option<usize>,
    checkpoint_dir: Option<String>,
    batch_char_budget: Option<usize>,
    chars_per_token: Option<usize>,
    apps: Vec<String>,
    allowed_hosts: Vec<String>,
    /// Git ref for incremental crawl (optional).
    since: Option<String>,
}

impl WizardAnswers {
    /// Default answers (all fields at their built-in defaults).
    fn defaults() -> Self {
        Self {
            root: ".".to_owned(),
            output: "output".to_owned(),
            language: DEFAULT_LANGUAGE.to_owned(),
            diagram_level: DEFAULT_DIAGRAM_LEVEL.to_owned(),
            max_abstractions: DEFAULT_MAX_ABSTRACTIONS,
            concurrency: DEFAULT_CONCURRENCY,
            max_llm_calls: None,
            provider: None,
            model: None,
            cache_dir: None,
            cache_size_limit_mb: None,
            checkpoint_dir: None,
            batch_char_budget: None,
            chars_per_token: None,
            apps: Vec::new(),
            allowed_hosts: Vec::new(),
            since: None,
        }
    }
}

/// Read a single line from stdin, trimming whitespace. Returns `None` on EOF
/// or read error (caller falls back to the default).
fn read_line_trimmed() -> Option<String> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => None, // EOF
        Ok(_) => {
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Err(_) => None,
    }
}

/// Prompt for a string value with a default. Returns the default when the
/// user presses Enter or EOF is reached.
fn prompt_string(label: &str, default: &str) -> String {
    eprint!("{label} [{default}]: ");
    match read_line_trimmed() {
        Some(s) => s,
        None => default.to_owned(),
    }
}

/// Prompt for a usize value with a default.
fn prompt_usize(label: &str, default: usize) -> usize {
    eprint!("{label} [{default}]: ");
    match read_line_trimmed() {
        Some(s) => s.parse::<usize>().unwrap_or(default),
        None => default,
    }
}

/// Prompt for a choice from a list of valid values (case-insensitive).
fn prompt_choice(label: &str, choices: &[&str], default: &str) -> String {
    let choices_str = choices.join(", ");
    eprint!("{label} ({choices_str}) [{default}]: ");
    match read_line_trimmed() {
        Some(s) => {
            let lower = s.to_ascii_lowercase();
            for c in choices {
                if *c == lower {
                    return lower;
                }
            }
            default.to_owned()
        }
        None => default.to_owned(),
    }
}

/// Run the interactive wizard, collecting answers from stdin.
///
/// When stdin is not a terminal (e.g. piped in CI), the wizard still runs but
/// each prompt gets EOF immediately and falls back to defaults — effectively
/// behaving like `--non-interactive`.
fn run_wizard() -> WizardAnswers {
    let mut a = WizardAnswers::defaults();

    eprintln!("brigid init — interactive configuration wizard");
    eprintln!("(Press Enter to accept the default for each prompt)");
    eprintln!();

    a.language = prompt_string("Output language", DEFAULT_LANGUAGE);
    a.diagram_level = prompt_choice(
        "Diagram level",
        &["minimal", "standard", "rich"],
        DEFAULT_DIAGRAM_LEVEL,
    );
    a.max_abstractions = prompt_usize("Max abstractions", DEFAULT_MAX_ABSTRACTIONS);
    a.concurrency = prompt_usize("Concurrency (chapter writes)", DEFAULT_CONCURRENCY);

    // Cache settings
    eprintln!();
    eprintln!("Cache settings:");
    let cache_dir = prompt_string("Cache directory (blank = platform default)", "");
    a.cache_dir = if cache_dir.is_empty() {
        None
    } else {
        Some(cache_dir)
    };
    eprint!("Cache size limit (MB) [{DEFAULT_CACHE_SIZE_MB}]: ");
    a.cache_size_limit_mb =
        read_line_trimmed().map(|s| s.parse::<usize>().unwrap_or(DEFAULT_CACHE_SIZE_MB));

    a
}

/// Run `brigid init --check`: validate an existing `brigid.toml` and report
/// issues. Exits with code 2 on any error-level issue.
fn cmd_init_check(dir: &Path) -> ExitCode {
    let path = dir.join("brigid.toml");
    if !path.is_file() {
        eprintln!("error: {} does not exist", path.display());
        return ExitCode::from(EXIT_CONFIG);
    }
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: read {}: {e}", path.display());
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    // Try parsing as TOML. If that fails, try YAML (the file might have a
    // .toml extension but YAML content — unlikely but consistent with the
    // "try both" fallback in load_file_config).
    let cfg = match parse_toml_config(&text) {
        Ok(c) => c,
        Err(toml_err) => match parse_yaml_config(&text) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("error: invalid brigid.toml: {toml_err}");
                return ExitCode::from(EXIT_CONFIG);
            }
        },
    };

    let issues = validate_config_for_check(&cfg);
    if issues.is_empty() {
        println!("{}: OK — no issues found", path.display());
        return ExitCode::from(EXIT_OK);
    }

    let has_errors = issues.iter().any(|i| i.severity == "error");
    for issue in &issues {
        println!("[{}] {}", issue.severity, issue.message);
    }
    if has_errors {
        eprintln!(
            "error: {} has {} error(s) and {} warning(s)",
            path.display(),
            issues.iter().filter(|i| i.severity == "error").count(),
            issues.iter().filter(|i| i.severity == "warning").count(),
        );
        ExitCode::from(EXIT_CONFIG)
    } else {
        eprintln!(
            "warning: {} has {} warning(s)",
            path.display(),
            issues.iter().filter(|i| i.severity == "warning").count(),
        );
        ExitCode::from(EXIT_OK)
    }
}

fn cmd_init(dir: &Path, non_interactive: bool, check: bool) -> ExitCode {
    if check {
        return cmd_init_check(dir);
    }

    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("error: create {}: {e}", dir.display());
        return ExitCode::from(EXIT_CONFIG);
    }
    let path = dir.join("brigid.toml");
    if path.exists() {
        eprintln!("error: {} already exists", path.display());
        return ExitCode::from(EXIT_CONFIG);
    }

    // Determine answers: interactive wizard, or defaults.
    // When --non-interactive is set, skip the wizard entirely.
    // Otherwise, run the wizard which reads from stdin. If stdin is piped
    // with data, the wizard reads the answers. If stdin is EOF (e.g. /dev/null
    // or closed in CI), each prompt falls back to its default.
    let answers = if non_interactive {
        WizardAnswers::defaults()
    } else {
        run_wizard()
    };

    let template = generate_config_template(&answers);
    match fs::write(&path, template) {
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

/// Serialize a [`StageOutput`] envelope to stdout.
///
/// Uses pretty-printed JSON when stdout is a TTY (interactive), and compact
/// JSON when piped (machine-readable).
fn print_stage_json<T: serde::Serialize>(out: &StageOutput<T>) {
    let value = serde_json::to_value(out).unwrap_or_else(|e| {
        eprintln!("error: failed to serialize JSON output: {e}");
        serde_json::Value::Null
    });
    let json = if std::io::stdout().is_terminal() {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
    .unwrap_or_else(|e| {
        eprintln!("error: failed to serialize JSON output: {e}");
        String::from("{}")
    });
    let _ = writeln!(std::io::stdout(), "{json}");
}

fn cmd_crawl(dir: &Path, since: Option<&str>, format: OutputFormat) -> ExitCode {
    let options = CrawlOptions {
        since: since.map(str::to_owned),
    };
    match crawl_local_with_options(dir, options) {
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

fn cmd_dry_run(dir: &Path, apps: &[String], since: Option<&str>, format: OutputFormat) -> ExitCode {
    let scope: Option<Vec<ModuleKey>> = if apps.is_empty() {
        None
    } else {
        Some(apps.iter().map(ModuleKey::new).collect())
    };
    let scope_ref = scope.as_deref();
    let crawl_options = CrawlOptions {
        since: since.map(str::to_owned),
    };
    match dry_run_with_options(
        dir,
        scope_ref,
        &brigid_core::BudgetConfig::default(),
        crawl_options,
    ) {
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

    // Capture the current repo HEAD for git revision tracking (issue #226).
    // Prefer the configured repo root; fall back to the current working
    // directory so `brigid resume` still reports staleness when run from inside
    // the repo without an explicit `--root`.
    let repo_root = current_cfg
        .root
        .as_deref()
        .unwrap_or_else(|| Path::new("."));
    let current_head = current_git_head(repo_root);
    let stale = current_head
        .as_deref()
        .map(|head| is_checkpoint_stale(&meta, head))
        .unwrap_or(false);

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
            println!(
                "git_commit: {}",
                meta.git_commit.as_deref().unwrap_or("(none)")
            );
            println!(
                "since_ref: {}",
                meta.since_ref.as_deref().unwrap_or("(none)")
            );
            println!(
                "current_head: {}",
                current_head.as_deref().unwrap_or("(none)")
            );
            println!("stale: {stale}");
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
                "git_commit": meta.git_commit,
                "since_ref": meta.since_ref,
                "current_head": current_head,
                "stale": stale,
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
/// `brigid_pipeline::identify_with_cancellation`. The LLM client is a
/// `brigid_pipeline::MockClient` with a canned response when no API key is
/// present — this lets the subcommand be exercised in tests without network
/// access. A real provider client will be wired in M4.
fn cmd_identify(
    dir: &Path,
    checkpoint_dir: &Path,
    single_shot: bool,
    max_abstractions: usize,
    cfg: &RunConfig,
    format: OutputFormat,
) -> ExitCode {
    // Crawl the repo to get the file inventory (incremental when --since is set).
    let crawl_options = CrawlOptions {
        since: cfg.since.clone(),
    };
    let crawl_result = match crawl_local_with_options(dir, crawl_options) {
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
    let records = brigid_pipeline::records_from_files(&file_entries);

    // Build the identify run config.
    let project_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let (strategy, reduce_input) = if single_shot {
        (
            brigid_pipeline::IdentifyStrategy::SingleShot(
                brigid_pipeline::IdentifySingleShotInput {
                    files: crawl_result.files,
                    project_name,
                    language_instruction: String::new(),
                    lang_note: String::new(),
                    max_abstraction_num: max_abstractions,
                },
            ),
            None,
        )
    } else {
        let map_files = crawl_result.files;
        let reduce_files = map_files.clone();
        (
            brigid_pipeline::IdentifyStrategy::MapReduce(brigid_pipeline::IdentifyMapInput {
                files: map_files,
                sizes: crawl_result.sizes,
                project_name: project_name.clone(),
                language_instruction: String::new(),
                lang_note: String::new(),
                max_abstraction_num: max_abstractions,
                max_concurrency: 4,
                budget_config: brigid_core::BudgetConfig::default(),
                community_context: String::new(),
            }),
            Some(brigid_pipeline::IdentifyReduceInput {
                candidates: Vec::new(),
                files: reduce_files,
                project_name,
                language_instruction: String::new(),
                lang_note: String::new(),
                max_abstraction_num: max_abstractions,
                module_summary: String::new(),
                multimodal_context: String::new(),
            }),
        )
    };

    let mut identify_config = cfg.clone();
    // Write --dir into config.root so config_hash differs between
    // different source directories (prevents checkpoint collision).
    identify_config.root = Some(dir.to_path_buf());
    let run_cfg = brigid_pipeline::IdentifyRunConfig {
        strategy,
        reduce_input,
        unredacted_config: identify_config,
        source_revision: dir.display().to_string(),
        files: records,
    };

    let (client, _identify_stats): (Box<dyn LlmClient>, _) = if force_mock_client() {
        eprintln!(
            "warning: identify: BRIGID_FORCE_MOCK is set — using a mock client. \
             The output will be a placeholder, not a real LLM analysis."
        );
        (
            mock_client(vec![PLACEHOLDER_IDENTIFY_YAML.to_string()]),
            brigid_pipeline::CacheStatsHandle::empty(),
        )
    } else {
        match build_real_llm_client(
            build_llm_cache(cfg),
            cfg.allowed_hosts.as_deref().unwrap_or(&[]),
            cfg.provider.as_deref(),
            cfg.model.as_deref(),
        ) {
            Ok((client, stats)) => {
                eprintln!("identify: using live LLM provider");
                (client, stats)
            }
            Err(error) => {
                eprintln!("error: identify: failed to configure LLM client: {error}");
                return ExitCode::from(EXIT_LLM);
            }
        }
    };

    let renderer = match brigid_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: identify: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let store = CheckpointStore::new(checkpoint_dir);
    let mut progress = brigid_core::ProgressTracker::new(
        cfg.max_llm_calls
            .unwrap_or(brigid_core::DEFAULT_MAX_LLM_CALLS),
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
        let cancel = match brigid_pipeline::setup_ctrl_c_handler() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: identify: signal handler: {e}");
                return ExitCode::from(EXIT_FAIL);
            }
        };
        // Debug/test-only affordance: immediately cancel the token so the
        // pipeline returns `Cancelled` without needing a real signal. This
        // lets us test exit code 5 in process-boundary tests.
        #[cfg(debug_assertions)]
        if env::var("BRIGID_MOCK_CANCEL")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            cancel.cancel();
        }

        // Build the plugin registry for custom kind detectors (issue #228 /
        // ADR 0014). Today the registry always includes the built-in
        // [`DefaultKindDetector`] as a fallback; `plugin_dirs` from the
        // resolved config is reserved for future dynamic loading.
        let registry = brigid_core::PluginRegistry::with_default();

        let outcome = brigid_pipeline::identify_with_cancellation(
            client.as_ref(),
            &renderer,
            &run_cfg,
            &mut progress,
            &cancel,
            &store,
            Some(&registry),
        )
        .await;

        match outcome {
            Ok(brigid_pipeline::IdentifyRunOutcome::Completed(result)) => {
                match format {
                    OutputFormat::Text => {
                        println!(
                            "identify: completed with {} abstractions",
                            result.abstractions.len()
                        );
                        println!("checkpoint: {}", checkpoint_dir.display());
                    }
                    OutputFormat::Json => {
                        let data = brigid_core::IdentifyOutput {
                            abstractions: result.abstractions.clone(),
                            relationships: Vec::new(),
                        };
                        let stats = brigid_core::StageStats {
                            items_processed: Some(result.abstractions.len() as u32),
                            llm_calls: Some(progress.snapshot().llm_calls_used),
                            elapsed_ms: None,
                        };
                        let out = brigid_core::StageOutput {
                            schema_version: brigid_core::SCHEMA_VERSION,
                            stage: "identify".to_string(),
                            status: brigid_core::StageStatus::Ok,
                            data,
                            stats: Some(stats),
                        };
                        print_json(&out);
                    }
                }
                ExitCode::from(EXIT_OK)
            }
            Ok(brigid_pipeline::IdentifyRunOutcome::Cancelled {
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
                    brigid_pipeline::IdentifyError::Budget(_) => EXIT_BUDGET,
                    brigid_pipeline::IdentifyError::Llm(_)
                    | brigid_pipeline::IdentifyError::LlmBatch { .. } => EXIT_LLM,
                    brigid_pipeline::IdentifyError::Prompt(_) => EXIT_CONFIG,
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
    strict_app_validation: bool,
    tutorial_style_str: &str,

    concurrency: Option<usize>,
    max_llm_calls: Option<u32>,
    verbosity: Verbosity,
    format: OutputFormat,
    cfg: &RunConfig,
) -> ExitCode {
    let diagram_level_parsed = match brigid_pipeline::DiagramLevel::parse(diagram_level) {
        Some(dl) => dl,
        None => {
            eprintln!(
                "error: generate: invalid diagram level '{diagram_level}' \
                 (expected: minimal, standard, or rich)"
            );
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let tutorial_style = match brigid_core::config::TutorialStyle::parse(tutorial_style_str) {
        Some(ts) => ts,
        None => {
            eprintln!(
                "error: generate: invalid tutorial style '{tutorial_style_str}' \
                 (expected: book or blog-post)"
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
            strict_app_validation,
            tutorial_style,
            concurrency,
            max_llm_calls,
            verbosity,
            format,
            cfg,
        );
    }

    let crawl_options = CrawlOptions {
        since: cfg.since.clone(),
    };
    let crawl_result = match crawl_local_with_options(dir, crawl_options) {
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

    let dry_run_plan = match brigid_pipeline::dry_run_with_options(
        dir,
        None,
        &brigid_core::BudgetConfig::default(),
        CrawlOptions {
            since: cfg.since.clone(),
        },
    ) {
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
    let records = brigid_pipeline::records_from_files(&file_entries);

    let file_contents: Vec<(String, String)> = crawl_result
        .files
        .iter()
        .map(|f| (f.clone(), String::new()))
        .collect();

    let modules: Vec<brigid_core::ModuleKey> = dry_run_plan
        .modules
        .iter()
        .map(|m| brigid_core::ModuleKey::new(m.key.as_str()))
        .collect();

    let setup_context = dry_run_plan
        .setup
        .config_files
        .iter()
        .map(|f| format!("# File: {f}\n"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut run_config = cfg.clone();
    // Write --dir into run_config.root so config_hash differs between
    // different source directories (prevents checkpoint collision when
    // the same --checkpoint-dir is reused for a different project).
    run_config.root = Some(dir.to_path_buf());
    if run_config.language.is_none() {
        run_config.language = Some(language.to_string());
    }
    // CLI --max-llm-calls overrides config/env budget.
    if let Some(n) = max_llm_calls {
        run_config.max_llm_calls = Some(n);
    } else {
        run_config.max_llm_calls = run_config
            .max_llm_calls
            .or_else(|| Some(default_max_llm_calls(max_abstractions, review_chapters)));
    }
    // CLI --concurrency overrides config, which overrides the default (4).
    let chapter_concurrency = concurrency
        .or(cfg.concurrency)
        .unwrap_or(brigid_pipeline::DEFAULT_CHAPTERS_CONCURRENCY);

    verbose_msg(
        verbosity,
        &format!(
            "concurrency={} max-llm-calls={}",
            chapter_concurrency,
            run_config
                .max_llm_calls
                .unwrap_or(brigid_core::DEFAULT_MAX_LLM_CALLS)
        ),
    );

    let cache = build_llm_cache(&run_config);
    let (client, cache_stats): (Box<dyn LlmClient>, brigid_pipeline::CacheStatsHandle) =
        if force_mock_client() {
            print_progress(
                verbosity,
                "warning: generate: BRIGID_FORCE_MOCK is set -- using a mock client. \
                 The output will be a placeholder, not a real LLM analysis.",
            );

            let mut responses: Vec<String> = Vec::new();
            if single_shot {
                responses.push(PLACEHOLDER_IDENTIFY_YAML.to_string());
            } else {
                responses.push(PLACEHOLDER_IDENTIFY_YAML.to_string());
                responses.push(PLACEHOLDER_IDENTIFY_YAML.to_string());
            }
            responses.push(PLACEHOLDER_RELATIONSHIPS_YAML.to_string());
            responses.push(PLACEHOLDER_ORDER_YAML.to_string());
            for _ in 0..max_abstractions {
                responses.push(PLACEHOLDER_CHAPTER.to_string());
            }
            if review_chapters {
                for _ in 0..max_abstractions {
                    responses.push(PLACEHOLDER_CHAPTER.to_string());
                }
            }
            if !no_setup {
                let do_setup = force_setup
                    || dry_run_plan.setup.score < 50
                    || dry_run_plan.setup.gaps.len() >= 3;
                if do_setup {
                    responses.push(PLACEHOLDER_SETUP.to_string());
                }
            }
            if !no_overview && modules.len() > 1 {
                responses.push(PLACEHOLDER_OVERVIEW.to_string());
            }

            (
                mock_client(responses),
                brigid_pipeline::CacheStatsHandle::empty(),
            )
        } else {
            match build_real_llm_client(
                cache.clone(),
                run_config.allowed_hosts.as_deref().unwrap_or(&[]),
                run_config.provider.as_deref(),
                run_config.model.as_deref(),
            ) {
                Ok((client, stats)) => {
                    print_progress(verbosity, "generate: using live LLM provider");
                    (client, stats)
                }
                Err(error) => {
                    eprintln!("error: generate: failed to configure LLM client: {error}");
                    return ExitCode::from(EXIT_LLM);
                }
            }
        };

    let renderer = match brigid_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: generate: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let store = CheckpointStore::new(checkpoint_dir);

    let (mut checkpoint, files) = match store.load() {
        Ok((meta, files)) => {
            // Checkpoint collision detection (issue #266): if the checkpoint
            // was created for a different --dir, discard it and start fresh.
            // Normalize both paths via canonicalize to avoid false positives
            // from symlinks, "./" segments, etc. Fall back to string
            // comparison if canonicalize fails (e.g., path doesn't exist yet).
            let current_dir = dir.to_string_lossy();
            let current_canonical = std::fs::canonicalize(dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| current_dir.to_string());
            if let Some(ref cp_dir) = meta.source_dir {
                let cp_canonical = std::fs::canonicalize(cp_dir)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| cp_dir.clone());
                if cp_canonical != current_canonical {
                    eprintln!(
                        "warning: generate: checkpoint at {} was created for \
                         '{cp_dir}' but --dir is '{}' — discarding old \
                         checkpoint and starting fresh.",
                        checkpoint_dir.display(),
                        current_dir,
                    );
                    let mut fresh = match brigid_core::CheckpointV1::new(
                        &run_config,
                        run_config.redacted_for_checkpoint(),
                        dir.display().to_string(),
                        "0Z",
                    ) {
                        Ok(cp) => cp,
                        Err(e) => {
                            eprintln!("error: generate: checkpoint init: {e}");
                            return ExitCode::from(EXIT_CONFIG);
                        }
                    };
                    fresh.mark_stage_complete(brigid_core::StageId::Fetch, "0Z");
                    fresh.mark_stage_complete(brigid_core::StageId::DryRun, "0Z");
                    (fresh, records)
                } else {
                    (meta, files)
                }
            } else {
                (meta, files)
            }
        }
        Err(_) => {
            let mut meta = brigid_core::CheckpointV1::new(
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
            meta.mark_stage_complete(brigid_core::StageId::Fetch, "0Z");
            meta.mark_stage_complete(brigid_core::StageId::DryRun, "0Z");
            (meta, records)
        }
    };
    if !checkpoint.is_stage_complete(brigid_core::StageId::Fetch) {
        checkpoint.mark_stage_complete(brigid_core::StageId::Fetch, "0Z");
    }
    if !checkpoint.is_stage_complete(brigid_core::StageId::DryRun) {
        checkpoint.mark_stage_complete(brigid_core::StageId::DryRun, "0Z");
    }
    if let Err(e) = store.save(checkpoint.clone(), &files) {
        eprintln!("error: generate: checkpoint save failed: {e}");
        return ExitCode::from(EXIT_CONFIG);
    }

    let mut progress =
        ProgressTracker::from_config_and_checkpoint(run_config.max_llm_calls, &checkpoint);

    let generate_start = std::time::Instant::now();

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
        let cancel = match brigid_pipeline::setup_ctrl_c_handler() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: generate: signal handler: {e}");
                return ExitCode::from(EXIT_FAIL);
            }
        };
        // Debug/test-only affordance: immediately cancel the token so the
        // pipeline returns `Cancelled` without needing a real signal. This
        // lets us test exit code 5 in process-boundary tests.
        #[cfg(debug_assertions)]
        if env::var("BRIGID_MOCK_CANCEL")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            cancel.cancel();
        }

        let gen_config = brigid_pipeline::GenerateConfig {
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
            chapter_concurrency,
            review_chapters,
            strict_app_validation,
            tutorial_style,
        };

        let outcome = brigid_pipeline::run_generate(
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
            Ok(brigid_pipeline::GenerateOutcome::Completed(combined)) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("error: generate: checkpoint save failed: {e}");
                    return ExitCode::from(EXIT_CONFIG);
                }

                print_progress(
                    verbosity,
                    &format!(
                        "generate: completed with {} chapters (locale={})",
                        combined.chapter_count, combined.locale
                    ),
                );
                print_progress(verbosity, &format!("output: {}", output_dir.display()));
                print_progress(
                    verbosity,
                    &format!("checkpoint: {}", checkpoint_dir.display()),
                );
                if verbosity.is_verbose() {
                    print_verbose_summary(&progress, cache.as_ref(), &cache_stats, checkpoint_dir);
                }

                if format == OutputFormat::Json {
                    let stages: Vec<brigid_core::StageSummary> = progress
                        .stage_timings()
                        .iter()
                        .map(|t| brigid_core::StageSummary {
                            name: t.stage.clone(),
                            status: "ok".to_string(),
                            duration_ms: t.elapsed.as_millis() as u64,
                            llm_calls: t.llm_calls,
                        })
                        .collect();
                    let total_llm_calls = checkpoint.metadata.llm_calls_used;
                    let elapsed_ms = generate_start.elapsed().as_millis() as u64;
                    let data = brigid_core::GenerateOutput {
                        stages,
                        output_dir: output_dir.display().to_string(),
                        checkpoint_path: checkpoint_dir
                            .join("checkpoint.json")
                            .display()
                            .to_string(),
                        total_llm_calls,
                        elapsed_ms,
                    };
                    let envelope = brigid_core::StageOutput::new(
                        "generate",
                        brigid_core::StageStatus::Ok,
                        data,
                        None,
                    );
                    let json = serde_json::to_string(&envelope).unwrap_or_else(|e| {
                        eprintln!("error: generate: JSON serialization failed: {e}");
                        "{}".to_string()
                    });
                    println!("{json}");
                }

                ExitCode::from(EXIT_OK)
            }
            Ok(brigid_pipeline::GenerateOutcome::Cancelled { checkpoint_path }) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: generate: checkpoint save failed: {e}");
                }
                print_progress(verbosity, "generate: cancelled");
                print_progress(
                    verbosity,
                    &format!(
                        "partial checkpoint: {} -- resume to continue",
                        checkpoint_path.display()
                    ),
                );
                ExitCode::from(EXIT_PARTIAL_CHECKPOINT)
            }
            Err(e) => {
                if let Err(save_err) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: generate: checkpoint save failed: {save_err}");
                }
                let code = stage_exit_code(&e);
                print_error("generate failed", &e);
                ExitCode::from(code)
            }
        }
    });
    if verbosity.is_verbose() {
        print_cache_stats(cache.as_ref(), &cache_stats);
    }
    exit_code
}

/// Run the full generate pipeline once per discovered app/module.
///
/// Delegates to `brigid_pipeline::run_generate_each_app`, which discovers
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
    diagram_level: brigid_pipeline::DiagramLevel,
    force_setup: bool,
    no_setup: bool,
    no_overview: bool,
    checkpoint_dir: &Path,
    output_dir: &Path,
    max_abstractions: usize,
    single_shot: bool,
    review_chapters: bool,
    strict_app_validation: bool,
    tutorial_style: brigid_core::config::TutorialStyle,

    concurrency: Option<usize>,
    max_llm_calls: Option<u32>,
    verbosity: Verbosity,
    format: OutputFormat,
    cfg: &RunConfig,
) -> ExitCode {
    // `--format json` for `--each-app` is not yet supported; text output only.
    let _ = format;
    let mut run_config = cfg.clone();
    // Write --dir into run_config.root so config_hash differs between
    // different source directories (prevents checkpoint collision when
    // the same --checkpoint-dir is reused for a different project).
    run_config.root = Some(dir.to_path_buf());
    if run_config.language.is_none() {
        run_config.language = Some(language.to_string());
    }
    // CLI --max-llm-calls overrides config/env budget.
    if let Some(n) = max_llm_calls {
        run_config.max_llm_calls = Some(n);
    } else {
        run_config.max_llm_calls = run_config
            .max_llm_calls
            .or_else(|| Some(default_max_llm_calls(max_abstractions, review_chapters)));
    }
    // CLI --concurrency overrides config, which overrides the default (4).
    let chapter_concurrency = concurrency
        .or(cfg.concurrency)
        .unwrap_or(brigid_pipeline::DEFAULT_CHAPTERS_CONCURRENCY);

    verbose_msg(
        verbosity,
        &format!(
            "concurrency={} max-llm-calls={}",
            chapter_concurrency,
            run_config
                .max_llm_calls
                .unwrap_or(brigid_core::DEFAULT_MAX_LLM_CALLS)
        ),
    );

    let cache = build_llm_cache(&run_config);
    let (client, cache_stats): (Box<dyn LlmClient>, brigid_pipeline::CacheStatsHandle) =
        if force_mock_client() {
            print_progress(
                verbosity,
                "warning: generate: BRIGID_FORCE_MOCK is set -- using a mock client. \
             The output will be a placeholder, not a real LLM analysis.",
            );

            let mut single_app_responses: Vec<String> = Vec::new();
            if single_shot {
                single_app_responses.push(PLACEHOLDER_IDENTIFY_YAML.to_string());
            } else {
                single_app_responses.push(PLACEHOLDER_IDENTIFY_YAML.to_string());
                single_app_responses.push(PLACEHOLDER_IDENTIFY_YAML.to_string());
            }
            single_app_responses.push(PLACEHOLDER_RELATIONSHIPS_YAML.to_string());
            single_app_responses.push(PLACEHOLDER_ORDER_YAML.to_string());
            single_app_responses.push(PLACEHOLDER_CHAPTER.to_string());
            if review_chapters {
                single_app_responses.push(PLACEHOLDER_CHAPTER.to_string());
            }
            if !no_setup {
                single_app_responses.push(PLACEHOLDER_SETUP.to_string());
            }
            if !no_overview {
                single_app_responses.push(PLACEHOLDER_OVERVIEW.to_string());
            }

            // Repeat the per-app sequence enough times for typical monorepos.
            // The cap of 20 apps covers the vast majority of real-world repos;
            // repos with more apps will exhaust the mock sequence and surface a
            // pipeline error, which is acceptable for a developer-only mock mode.
            let mut responses: Vec<String> = Vec::new();
            for _ in 0..20 {
                responses.extend(single_app_responses.clone());
            }

            (
                mock_client(responses),
                brigid_pipeline::CacheStatsHandle::empty(),
            )
        } else {
            match build_real_llm_client(
                cache.clone(),
                run_config.allowed_hosts.as_deref().unwrap_or(&[]),
                run_config.provider.as_deref(),
                run_config.model.as_deref(),
            ) {
                Ok((client, stats)) => {
                    print_progress(verbosity, "generate: using live LLM provider");
                    (client, stats)
                }
                Err(error) => {
                    eprintln!("error: generate: failed to configure LLM client: {error}");
                    return ExitCode::from(EXIT_LLM);
                }
            }
        };

    let renderer = match brigid_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: generate: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let gen_config = brigid_pipeline::GenerateConfig {
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
        chapter_concurrency,
        review_chapters,
        strict_app_validation,
        tutorial_style,
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
        let cancel = match brigid_pipeline::setup_ctrl_c_handler() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: generate: signal handler: {e}");
                return ExitCode::from(EXIT_FAIL);
            }
        };
        // Debug/test-only affordance: immediately cancel the token so the
        // pipeline returns `Cancelled` without needing a real signal. This
        // lets us test exit code 5 in process-boundary tests.
        #[cfg(debug_assertions)]
        if env::var("BRIGID_MOCK_CANCEL")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            cancel.cancel();
        }

        let outcome = brigid_pipeline::run_generate_each_app(
            client.as_ref(),
            &renderer,
            &cancel,
            &gen_config,
        )
        .await;

        match outcome {
            Ok(brigid_pipeline::EachAppOutcome::Completed(summaries)) => {
                let failures: Vec<_> = summaries.iter().filter(|s| !s.success).collect();
                let success_count = summaries.len() - failures.len();
                print_progress(
                    verbosity,
                    &format!(
                        "generate: each-app completed: {success_count}/{} apps succeeded",
                        summaries.len()
                    ),
                );
                for s in &summaries {
                    if !s.success {
                        print_progress(
                            verbosity,
                            &format!(
                                "  FAILED: {} -- {}",
                                s.app,
                                s.error.as_deref().unwrap_or("unknown error")
                            ),
                        );
                    }
                }
                print_progress(verbosity, &format!("output: {}", output_dir.display()));
                if failures.is_empty() {
                    ExitCode::from(EXIT_OK)
                } else {
                    ExitCode::from(EXIT_FAIL)
                }
            }
            Ok(brigid_pipeline::EachAppOutcome::Partial {
                summaries,
                cancelled_app,
            }) => {
                print_progress(
                    verbosity,
                    &format!("generate: each-app cancelled at '{cancelled_app}'"),
                );
                print_progress(
                    verbosity,
                    &format!(
                        "  {}/{} apps completed before cancellation",
                        summaries.len(),
                        summaries.len() + 1
                    ),
                );
                print_progress(verbosity, &format!("output: {}", output_dir.display()));
                ExitCode::from(EXIT_PARTIAL_CHECKPOINT)
            }
            Err(e) => {
                let code = match &e {
                    brigid_pipeline::GenerateError::Budget(_) => EXIT_BUDGET,
                    brigid_pipeline::GenerateError::Config(_) => EXIT_CONFIG,
                    _ => EXIT_FAIL,
                };
                print_error("generate --each-app failed", &e);
                ExitCode::from(code)
            }
        }
    });
    if verbosity.is_verbose() {
        print_cache_stats(cache.as_ref(), &cache_stats);
    }
    exit_code
}

fn load_stage_checkpoint(
    checkpoint_dir: &Path,
) -> Result<
    (
        brigid_core::CheckpointV1,
        Vec<brigid_core::FileBundleRecord>,
    ),
    ExitCode,
> {
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

fn make_stage_client(responses: Vec<String>, placeholder: &str) -> Box<dyn LlmClient> {
    let api_key = env::var("BRIGID_LLM_API_KEY").ok();
    if api_key.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
        eprintln!(
            "warning: no BRIGID_LLM_API_KEY set -- using a mock client. \
             The output will be a placeholder, not a real LLM analysis. \
             Set BRIGID_LLM_API_KEY to use a real provider (M4)."
        );
    }
    #[cfg(debug_assertions)]
    {
        if let Some(kind) = env::var("BRIGID_LLM_MOCK_FAIL")
            .ok()
            .filter(|s| !s.is_empty())
        {
            let err = mock_fail_error(&kind);
            return Box::new(MockClient::new("").fail_on(0, err));
        }
    }
    Box::new(MockClient::with_responses(responses).unwrap_or_else(|_| MockClient::new(placeholder)))
}

fn stage_exit_code(err: &brigid_pipeline::GenerateError) -> u8 {
    match err {
        brigid_pipeline::GenerateError::Budget(_)
        | brigid_pipeline::GenerateError::Identify(
            brigid_pipeline::identify::IdentifyError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Relationships(
            brigid_pipeline::relationships::RelationshipsError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Order(brigid_pipeline::order::OrderError::Budget(_))
        | brigid_pipeline::GenerateError::Chapters(
            brigid_pipeline::chapters::ChaptersError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Review(brigid_pipeline::review::ReviewError::Budget(_))
        | brigid_pipeline::GenerateError::Setup(
            brigid_pipeline::setup_guide::SetupGuideError::Budget(_),
        )
        | brigid_pipeline::GenerateError::Overview(
            brigid_pipeline::overview::OverviewError::Budget(_),
        ) => EXIT_BUDGET,
        brigid_pipeline::GenerateError::Identify(
            brigid_pipeline::identify::IdentifyError::Llm(_)
            | brigid_pipeline::identify::IdentifyError::LlmBatch { .. },
        )
        | brigid_pipeline::GenerateError::Relationships(
            brigid_pipeline::relationships::RelationshipsError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Order(brigid_pipeline::order::OrderError::Llm(_))
        | brigid_pipeline::GenerateError::Chapters(
            brigid_pipeline::chapters::ChaptersError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Review(brigid_pipeline::review::ReviewError::Llm(_))
        | brigid_pipeline::GenerateError::Setup(
            brigid_pipeline::setup_guide::SetupGuideError::Llm(_),
        )
        | brigid_pipeline::GenerateError::Overview(
            brigid_pipeline::overview::OverviewError::Llm(_),
        ) => EXIT_LLM,
        brigid_pipeline::GenerateError::Config(_) => EXIT_CONFIG,
        _ => EXIT_FAIL,
    }
}

fn cmd_relationships(
    dir: &Path,
    checkpoint_dir: &Path,
    cfg: &RunConfig,
    format: OutputFormat,
) -> ExitCode {
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
    let mut progress = ProgressTracker::from_config_and_checkpoint(cfg.max_llm_calls, &checkpoint);

    let placeholder_rel =
        "```yaml\nsummary: \"Placeholder project summary.\"\nrelationships: []\n```\n";
    let client = make_stage_client(vec![placeholder_rel.to_string()], placeholder_rel);

    let renderer = match brigid_pipeline::PromptRenderer::new() {
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
        match brigid_pipeline::run_relationships_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &file_contents,
            &project_name,
            &language_instruction,
            &mut progress,
        )
        .await
        {
            Ok(result) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("error: relationships: checkpoint save failed: {e}");
                    return ExitCode::from(EXIT_CONFIG);
                }
                match format {
                    OutputFormat::Text => {
                        eprintln!(
                            "relationships: completed (summary_len={}, rels={})",
                            result.project_summary.len(),
                            result.relationships.len()
                        );
                        eprintln!("checkpoint: {}", checkpoint_dir.display());
                    }
                    OutputFormat::Json => {
                        let data = brigid_core::RelationshipsOutput {
                            relationships: result.relationships.clone(),
                            evidence: Vec::new(),
                        };
                        let stats = brigid_core::StageStats {
                            items_processed: Some(result.relationships.len() as u32),
                            llm_calls: None,
                            elapsed_ms: None,
                        };
                        let out = brigid_core::StageOutput {
                            schema_version: brigid_core::SCHEMA_VERSION,
                            stage: "relationships".to_string(),
                            status: brigid_core::StageStatus::Ok,
                            data,
                            stats: Some(stats),
                        };
                        print_json(&out);
                    }
                }
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                if let Err(save_err) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: relationships: checkpoint save failed: {save_err}");
                }
                let code = stage_exit_code(&e);
                eprintln!("error: relationships failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_order(dir: &Path, checkpoint_dir: &Path, cfg: &RunConfig, format: OutputFormat) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let project_name = stage_project_name(dir);
    let language_instruction = stage_language_instruction(cfg.language.as_deref().unwrap_or("en"));
    let mut progress = ProgressTracker::from_config_and_checkpoint(cfg.max_llm_calls, &checkpoint);

    let placeholder_order = "```yaml\n- 0\n```\n";
    let client = make_stage_client(vec![placeholder_order.to_string()], placeholder_order);

    let renderer = match brigid_pipeline::PromptRenderer::new() {
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
        match brigid_pipeline::run_order_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &project_name,
            &language_instruction,
            &mut progress,
        )
        .await
        {
            Ok(result) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("error: order: checkpoint save failed: {e}");
                    return ExitCode::from(EXIT_CONFIG);
                }
                match format {
                    OutputFormat::Text => {
                        eprintln!(
                            "order: completed (chapters={})",
                            result.ordered_indices.len()
                        );
                        eprintln!("checkpoint: {}", checkpoint_dir.display());
                    }
                    OutputFormat::Json => {
                        // Derive chapter titles from the abstraction names
                        // in the checkpoint, indexed by ordered_indices.
                        let identify = brigid_pipeline::load_identify_result(&checkpoint);
                        let titles: Vec<String> = result
                            .ordered_indices
                            .iter()
                            .filter_map(|&idx| {
                                identify
                                    .as_ref()
                                    .and_then(|r| r.abstractions.get(idx))
                                    .map(|a| a.name.clone())
                            })
                            .collect();
                        let data = brigid_core::OrderOutput {
                            ordered_indices: result.ordered_indices.clone(),
                            titles,
                        };
                        let stats = brigid_core::StageStats {
                            items_processed: Some(result.ordered_indices.len() as u32),
                            llm_calls: None,
                            elapsed_ms: None,
                        };
                        let out = brigid_core::StageOutput {
                            schema_version: brigid_core::SCHEMA_VERSION,
                            stage: "order".to_string(),
                            status: brigid_core::StageStatus::Ok,
                            data,
                            stats: Some(stats),
                        };
                        print_json(&out);
                    }
                }
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                if let Err(save_err) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: order: checkpoint save failed: {save_err}");
                }
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
    format: OutputFormat,
    max_llm_calls: Option<u32>,
) -> ExitCode {
    let diagram_level_parsed = match brigid_pipeline::DiagramLevel::parse(diagram_level) {
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
    let mut progress = ProgressTracker::from_config_and_checkpoint(max_llm_calls, &checkpoint);

    let placeholder_chapter = "# Chapter 1: Placeholder\n\n## Motivation\n- Need placeholder\n\n## Core idea\nPlaceholder is key.\n\n## Summary\nWe learned about placeholder.\n";
    let identify = match brigid_pipeline::identify_checkpoint::load_identify_result(&checkpoint) {
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

    let renderer = match brigid_pipeline::PromptRenderer::new() {
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
        match brigid_pipeline::run_chapters_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &file_contents,
            &project_name,
            &language_instruction,
            language,
            diagram_level_parsed,
            brigid_pipeline::DEFAULT_CHAPTERS_CONCURRENCY,
            &mut progress,
        )
        .await
        {
            Ok(result) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("error: chapters: checkpoint save failed: {e}");
                    return ExitCode::from(EXIT_CONFIG);
                }
                match format {
                    OutputFormat::Json => {
                        let summaries: Vec<ChapterSummary> = result
                            .chapters
                            .iter()
                            .map(|ch| ChapterSummary {
                                chapter_num: ch.chapter_num as u32,
                                title: ch.title.clone(),
                                markdown_length: ch.markdown.len(),
                                file_indices: identify
                                    .abstractions
                                    .get(ch.abstraction_index)
                                    .map(|a| a.file_indices.clone())
                                    .unwrap_or_default(),
                            })
                            .collect();
                        let out = StageOutput {
                            schema_version: SCHEMA_VERSION,
                            stage: "chapters".into(),
                            status: StageStatus::Ok,
                            data: ChaptersOutput {
                                chapters: summaries,
                            },
                            stats: Some(StageStats {
                                items_processed: Some(result.chapters.len() as u32),
                                llm_calls: None,
                                elapsed_ms: None,
                            }),
                        };
                        print_stage_json(&out);
                    }
                    OutputFormat::Text => {
                        eprintln!("chapters: completed (chapters={})", result.chapters.len());
                        eprintln!("checkpoint: {}", checkpoint_dir.display());
                        eprintln!("output: {}", output_dir.display());
                    }
                }
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                if let Err(save_err) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: chapters: checkpoint save failed: {save_err}");
                }
                let code = stage_exit_code(&e);
                eprintln!("error: chapters failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_setup(
    dir: &Path,
    checkpoint_dir: &Path,
    force: bool,
    cfg: &RunConfig,
    format: OutputFormat,
) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let placeholder_setup = "# Setup: project\n\n## Prerequisites\n\nInstall dependencies.\n\n## Run\n\n```bash\nmake run\n```\n";
    let client = make_stage_client(vec![placeholder_setup.to_string()], placeholder_setup);

    let renderer = match brigid_pipeline::PromptRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: setup: prompt renderer: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let lang = cfg.language.as_deref().unwrap_or("en");
    let mut progress = ProgressTracker::from_config_and_checkpoint(cfg.max_llm_calls, &checkpoint);

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
        match brigid_pipeline::run_setup_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            dir,
            force,
            lang,
            &mut progress,
        )
        .await
        {
            Ok(guide) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("error: setup: checkpoint save failed: {e}");
                    return ExitCode::from(EXIT_CONFIG);
                }
                match format {
                    OutputFormat::Json => {
                        let out = StageOutput {
                            schema_version: SCHEMA_VERSION,
                            stage: "setup".into(),
                            status: StageStatus::Ok,
                            data: SetupOutput {
                                markdown: guide.markdown.clone(),
                                score: guide.score.max(0) as u32,
                                generated: !guide.markdown.is_empty(),
                            },
                            stats: None,
                        };
                        print_stage_json(&out);
                    }
                    OutputFormat::Text => {
                        eprintln!("setup: completed (forced={force})");
                        eprintln!("checkpoint: {}", checkpoint_dir.display());
                    }
                }
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                if let Err(save_err) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: setup: checkpoint save failed: {save_err}");
                }
                let code = stage_exit_code(&e);
                eprintln!("error: setup failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

fn cmd_overview(
    dir: &Path,
    checkpoint_dir: &Path,
    cfg: &RunConfig,
    format: OutputFormat,
) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let dry_run_plan = match brigid_pipeline::dry_run(dir, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: overview: dry-run failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let modules: Vec<brigid_core::ModuleKey> = dry_run_plan
        .modules
        .iter()
        .map(|m| brigid_core::ModuleKey::new(m.key.as_str()))
        .collect();

    let project_name = stage_project_name(dir);
    let language_instruction = stage_language_instruction(cfg.language.as_deref().unwrap_or("en"));
    let mut progress = ProgressTracker::from_config_and_checkpoint(cfg.max_llm_calls, &checkpoint);

    let placeholder_overview = "# Architecture Overview\n\nThis project has multiple modules.\n";
    let client = make_stage_client(vec![placeholder_overview.to_string()], placeholder_overview);

    let renderer = match brigid_pipeline::PromptRenderer::new() {
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
        match brigid_pipeline::run_overview_stage(
            client.as_ref(),
            &renderer,
            &store,
            &mut checkpoint,
            &project_name,
            &language_instruction,
            &modules,
            &mut progress,
        )
        .await
        {
            Ok(overview) => {
                if let Err(e) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("error: overview: checkpoint save failed: {e}");
                    return ExitCode::from(EXIT_CONFIG);
                }
                match format {
                    OutputFormat::Json => {
                        let out = StageOutput {
                            schema_version: SCHEMA_VERSION,
                            stage: "overview".into(),
                            status: StageStatus::Ok,
                            data: OverviewOutput {
                                markdown: overview.markdown.clone(),
                                apps: overview.app_inventory.clone(),
                                generated: !overview.markdown.is_empty(),
                            },
                            stats: None,
                        };
                        print_stage_json(&out);
                    }
                    OutputFormat::Text => {
                        eprintln!("overview: completed");
                        eprintln!("checkpoint: {}", checkpoint_dir.display());
                    }
                }
                ExitCode::from(EXIT_OK)
            }
            Err(e) => {
                if let Err(save_err) = store.persist_llm_calls(&mut checkpoint, &files, &progress) {
                    eprintln!("warning: overview: checkpoint save failed: {save_err}");
                }
                let code = stage_exit_code(&e);
                eprintln!("error: overview failed: {e}");
                ExitCode::from(code)
            }
        }
    })
}

/// Handle `brigid cache <action>` — prune or stats.
fn cmd_cache(action: CacheAction, cfg: &RunConfig) -> ExitCode {
    let env_map: BTreeMap<String, String> = env::vars().collect();
    let Some(root) = resolve_cache_root(&env_map, cfg.cache_dir.as_deref()) else {
        eprintln!("cache: no cache directory configured");
        return ExitCode::from(EXIT_CONFIG);
    };
    let db_path = root.join("cache.sqlite");

    match action {
        CacheAction::Prune => cmd_cache_prune(&db_path),
        CacheAction::Stats => cmd_cache_stats(&db_path),
    }
}

/// Delete the cache database file and its WAL/SHM sidecars.
///
/// Delegates to [`brigid_pipeline::CacheAdmin::prune`] which clears all
/// entries inside a `BEGIN IMMEDIATE` transaction, checkpoints the WAL
/// after commit, then removes the files. See `CacheAdmin::prune` docs
/// for the full concurrency-safety rationale.
fn cmd_cache_prune(db_path: &Path) -> ExitCode {
    match brigid_pipeline::CacheAdmin::prune(db_path) {
        Ok(0) => {
            eprintln!("cache: no cache file found at {}", db_path.display());
            ExitCode::from(EXIT_OK)
        }
        Ok(removed) => {
            eprintln!("cache: pruned ({removed} file(s) removed)");
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("cache: {e}");
            ExitCode::from(EXIT_FAIL)
        }
    }
}

/// Print cache entry count and on-disk size.
///
/// Delegates to [`brigid_pipeline::CacheAdmin`] for entry count (read-only
/// SQLite open) and on-disk size (including WAL/SHM sidecars).
fn cmd_cache_stats(db_path: &Path) -> ExitCode {
    if !db_path.exists() {
        eprintln!("cache: no cache file found at {}", db_path.display());
        return ExitCode::from(EXIT_OK);
    }

    let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

    let entry_count = match brigid_pipeline::CacheAdmin::entry_count(db_path) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("cache: cannot read entry count: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    let total_size = match brigid_pipeline::CacheAdmin::on_disk_size(db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cache: cannot read on-disk size: {e}");
            return ExitCode::from(EXIT_FAIL);
        }
    };

    eprintln!("cache: {}", db_path.display());
    eprintln!("  entries: {entry_count}");
    eprintln!("  size:    {}", fmt_file_size(db_size));
    if total_size != db_size {
        eprintln!("  total:   {} (incl. WAL/SHM)", fmt_file_size(total_size));
    }
    ExitCode::from(EXIT_OK)
}

/// Format a byte count as a human-readable string (e.g. "1.2 MB").
fn fmt_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn cmd_combine(
    dir: &Path,
    checkpoint_dir: &Path,
    output_dir: &Path,
    language: &str,
    cfg: &RunConfig,
    format: OutputFormat,
) -> ExitCode {
    let (mut checkpoint, files) = match load_stage_checkpoint(checkpoint_dir) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let store = CheckpointStore::new(checkpoint_dir);

    let dry_run_plan = match brigid_pipeline::dry_run(dir, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: combine: dry-run failed: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let modules: Vec<brigid_core::ModuleKey> = dry_run_plan
        .modules
        .iter()
        .map(|m| brigid_core::ModuleKey::new(m.key.as_str()))
        .collect();

    let lang = if language.is_empty() {
        cfg.language.as_deref().unwrap_or("en")
    } else {
        language
    };

    match brigid_pipeline::run_combine_stage(&store, &mut checkpoint, output_dir, lang, &modules) {
        Ok(combined) => {
            match format {
                OutputFormat::Json => {
                    let out = StageOutput {
                        schema_version: SCHEMA_VERSION,
                        stage: "combine".into(),
                        status: StageStatus::Ok,
                        data: CombineOutput {
                            index: combined.index_markdown.clone(),
                            chapter_count: combined.chapter_count as u32,
                            setup_present: combined.has_setup_guide,
                            overview_present: combined.has_architecture_overview,
                        },
                        stats: Some(StageStats {
                            items_processed: Some(combined.chapter_count as u32),
                            llm_calls: None,
                            elapsed_ms: None,
                        }),
                    };
                    print_stage_json(&out);
                }
                OutputFormat::Text => {
                    eprintln!(
                        "combine: completed with {} chapters (locale={})",
                        combined.chapter_count, combined.locale
                    );
                    eprintln!("output: {}", output_dir.display());
                    eprintln!("checkpoint: {}", checkpoint_dir.display());
                }
            }
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

/// Generate and emit a shell completion script for the `brigid` CLI.
///
/// When `output` is `Some(path)` the script is written to that file; otherwise
/// it is written to stdout. The completions are generated from the live
/// `clap::Command` built via [`Cli::command`] so every subcommand and flag is
/// covered automatically.
fn cmd_completions(shell: ShellKind, output: Option<PathBuf>) -> ExitCode {
    let mut cmd = Cli::command();
    let completion_shell = shell.to_completion_shell();
    match output {
        Some(path) => {
            let file = match fs::File::create(&path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error: completions: cannot create {}: {e}", path.display());
                    return ExitCode::from(EXIT_CONFIG);
                }
            };
            let mut writer = std::io::BufWriter::new(file);
            clap_complete::generate(completion_shell, &mut cmd, "brigid", &mut writer);
            eprintln!(
                "completions: wrote {} script to {}",
                shell_name(shell),
                path.display()
            );
            ExitCode::from(EXIT_OK)
        }
        None => {
            let mut stdout = std::io::stdout();
            clap_complete::generate(completion_shell, &mut cmd, "brigid", &mut stdout);
            ExitCode::from(EXIT_OK)
        }
    }
}

/// Human-readable shell name for status messages.
fn shell_name(shell: ShellKind) -> &'static str {
    match shell {
        ShellKind::Bash => "bash",
        ShellKind::Zsh => "zsh",
        ShellKind::Fish => "fish",
        ShellKind::PowerShell => "powershell",
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
    use brigid_pipeline::complete_text;

    /// Unique temp dir helper for unit tests in main.rs.
    fn temp_dir(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("brigid-cli-unit-{label}-{n}"))
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
        vars.insert("BRIGID_LLM_CACHE_DIR".into(), "/custom/cache".into());
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
        vars.insert("BRIGID_LLM_CACHE_DIR".into(), "/env/wins".into());
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
            root.ends_with("brigid/llm-cache") || root.ends_with("brigid\\llm-cache"),
            "default cache root should end with brigid/llm-cache, got: {}",
            root.display()
        );
    }

    #[test]
    fn cache_disabled_when_no_cache_env_set() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_NO_CACHE".into(), "1".into());
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
        vars.insert("BRIGID_NO_CACHE".into(), "0".into());
        assert!(!cache_is_disabled(&vars));
    }

    #[test]
    fn cache_disabled_when_no_cache_true() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_NO_CACHE".into(), "true".into());
        assert!(cache_is_disabled(&vars));
    }

    #[test]
    fn cache_disabled_blank_env_not_disabled() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_NO_CACHE".into(), "".into());
        assert!(!cache_is_disabled(&vars));
    }

    // --- Issue #183: Verbosity, fmt_duration, error_suggestion unit tests ---

    #[test]
    fn verbosity_quiet_suppresses_progress() {
        assert!(!Verbosity::Quiet.show_progress());
        assert!(!Verbosity::Quiet.is_verbose());
    }

    #[test]
    fn verbosity_normal_shows_progress_not_verbose() {
        assert!(Verbosity::Normal.show_progress());
        assert!(!Verbosity::Normal.is_verbose());
    }

    #[test]
    fn verbosity_verbose_shows_all() {
        assert!(Verbosity::Verbose.show_progress());
        assert!(Verbosity::Verbose.is_verbose());
    }

    #[test]
    fn fmt_duration_milliseconds() {
        let d = std::time::Duration::from_millis(42);
        assert_eq!(fmt_duration(d), "42ms");
    }

    #[test]
    fn fmt_duration_seconds() {
        let d = std::time::Duration::from_millis(1500);
        assert_eq!(fmt_duration(d), "1.5s");
    }

    #[test]
    fn error_suggestion_budget_mentions_max_llm_calls() {
        let err = brigid_pipeline::GenerateError::Budget(brigid_core::BudgetExceeded {
            used: 10,
            max: 10,
        });
        let hint = error_suggestion(&err).expect("budget error should have a hint");
        assert!(
            hint.contains("--max-llm-calls"),
            "budget hint should mention --max-llm-calls: {hint}"
        );
    }

    #[test]
    fn error_suggestion_config_mentions_dry_run() {
        let err = brigid_pipeline::GenerateError::Config("bad config".to_string());
        let hint = error_suggestion(&err).expect("config error should have a hint");
        assert!(
            hint.contains("dry-run"),
            "config hint should suggest dry-run: {hint}"
        );
    }

    // --- Issue #185: init wizard, template, --check unit tests ---

    #[test]
    fn wizard_defaults_have_expected_values() {
        let a = WizardAnswers::defaults();
        assert_eq!(a.language, "en");
        assert_eq!(a.diagram_level, "standard");
        assert_eq!(a.max_abstractions, 10);
        assert_eq!(a.concurrency, 4);
        assert_eq!(a.max_llm_calls, None);
        assert_eq!(a.cache_dir, None);
        assert_eq!(a.cache_size_limit_mb, None);
        assert!(a.apps.is_empty());
        assert!(a.allowed_hosts.is_empty());
    }

    #[test]
    fn template_with_defaults_is_all_comments() {
        let a = WizardAnswers::defaults();
        let template = generate_config_template(&a);
        // Every non-empty, non-comment line should be a comment.
        for line in template.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                assert!(
                    trimmed.starts_with('#'),
                    "expected all lines to be comments, got: {trimmed}"
                );
            }
        }
    }

    #[test]
    fn template_with_defaults_is_valid_toml() {
        let a = WizardAnswers::defaults();
        let template = generate_config_template(&a);
        // The template should parse as valid TOML (all comments = empty doc).
        let cfg = parse_toml_config(&template).expect("template should be valid TOML");
        // All fields should be None/empty since everything is commented out.
        assert_eq!(cfg.language, None);
        assert_eq!(cfg.concurrency, None);
        assert_eq!(cfg.max_abstractions, None);
    }

    #[test]
    fn template_with_custom_answers_uncomments_selected() {
        let mut a = WizardAnswers::defaults();
        a.language = "es".to_owned();
        a.diagram_level = "rich".to_owned();
        a.max_abstractions = 15;
        a.concurrency = 8;
        let template = generate_config_template(&a);
        // The custom values should be uncommented.
        assert!(
            template.contains("language = \"es\""),
            "template should uncomment language: {template}"
        );
        assert!(
            template.contains("diagram_level = \"rich\""),
            "template should uncomment diagram_level: {template}"
        );
        assert!(
            template.contains("max_abstractions = 15"),
            "template should uncomment max_abstractions: {template}"
        );
        assert!(
            template.contains("concurrency = 8"),
            "template should uncomment concurrency: {template}"
        );
        // Default values should still be commented (start with #).
        assert!(
            !template
                .lines()
                .any(|l| l.trim_start().starts_with("root =")),
            "template should keep root commented (it's the default)"
        );
    }

    #[test]
    fn template_with_custom_answers_is_valid_toml() {
        let mut a = WizardAnswers::defaults();
        a.language = "es".to_owned();
        a.diagram_level = "rich".to_owned();
        a.max_abstractions = 15;
        a.concurrency = 8;
        a.max_llm_calls = Some(300);
        let template = generate_config_template(&a);
        let cfg = parse_toml_config(&template).expect("custom template should be valid TOML");
        assert_eq!(cfg.language.as_deref(), Some("es"));
        assert_eq!(cfg.diagram_level.as_deref(), Some("rich"));
        assert_eq!(cfg.max_abstractions, Some(15));
        assert_eq!(cfg.concurrency, Some(8));
        assert_eq!(cfg.max_llm_calls, Some(300));
    }

    #[test]
    fn template_includes_all_m5_options() {
        let a = WizardAnswers::defaults();
        let template = generate_config_template(&a);
        // All M5 options should be mentioned in the template.
        for option in &[
            "language",
            "diagram_level",
            "max_abstractions",
            "concurrency",
            "max_llm_calls",
            "cache_dir",
            "cache_size_limit_mb",
            "allowed_hosts",
        ] {
            assert!(
                template.contains(option),
                "template should mention {option}"
            );
        }
    }

    #[test]
    fn template_includes_secret_warning() {
        let a = WizardAnswers::defaults();
        let template = generate_config_template(&a);
        assert!(
            template.contains("API keys") || template.contains("BRIGID_LLM_API_KEY"),
            "template should warn about API keys"
        );
    }

    #[test]
    fn cmd_init_non_interactive_writes_valid_config() {
        let dir = temp_dir("init-non-interactive");
        std::fs::create_dir_all(&dir).unwrap();
        let code = cmd_init(&dir, true, false);
        assert_eq!(code, ExitCode::from(EXIT_OK));
        let path = dir.join("brigid.toml");
        assert!(path.is_file());
        let content = std::fs::read_to_string(&path).unwrap();
        // Should parse as valid TOML.
        let cfg = parse_toml_config(&content).expect("non-interactive config should be valid TOML");
        // All fields should be None since everything is commented out.
        assert_eq!(cfg.language, None);
        // Should contain key option comments.
        assert!(content.contains("language"));
        assert!(content.contains("concurrency"));
        assert!(content.contains("diagram_level"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_init_check_valid_config_exits_zero() {
        let dir = temp_dir("init-check-valid");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brigid.toml"), b"language = \"en\"\n").unwrap();
        let code = cmd_init(&dir, false, true);
        assert_eq!(code, ExitCode::from(EXIT_OK));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_init_check_invalid_config_exits_two() {
        let dir = temp_dir("init-check-invalid");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brigid.toml"), b"concurrency = 0\n").unwrap();
        let code = cmd_init(&dir, false, true);
        assert_eq!(code, ExitCode::from(EXIT_CONFIG));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_init_check_missing_file_exits_two() {
        let dir = temp_dir("init-check-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let code = cmd_init(&dir, false, true);
        assert_eq!(code, ExitCode::from(EXIT_CONFIG));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_init_check_secret_field_exits_two() {
        let dir = temp_dir("init-check-secret");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brigid.toml"), b"api_key = \"xxx\"\n").unwrap();
        let code = cmd_init(&dir, false, true);
        assert_eq!(code, ExitCode::from(EXIT_CONFIG));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_init_refuses_overwrite() {
        let dir = temp_dir("init-overwrite-unit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brigid.toml"), b"# pre-existing").unwrap();
        let code = cmd_init(&dir, true, false);
        assert_eq!(code, ExitCode::from(EXIT_CONFIG));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Issue #188: man page generation tests ---

    #[test]
    fn man_page_starts_with_th_is_valid_troff() {
        let buf = generate_man_page();
        let text = String::from_utf8(buf).expect("man page should be valid UTF-8");
        assert!(
            text.contains(".TH "),
            "man page should contain a .TH title macro (valid troff), got: {text}"
        );
    }

    #[test]
    fn man_page_contains_all_subcommand_names() {
        let buf = generate_man_page();
        let text = String::from_utf8(buf).expect("man page should be valid UTF-8");
        // clap_mangen renders subcommand names as brigid-<name>(1).
        for name in &[
            "init",
            "crawl",
            "dry-run",
            "eval",
            "resume",
            "identify",
            "generate",
            "relationships",
            "order",
            "chapters",
            "setup",
            "overview",
            "combine",
            "manpage",
        ] {
            assert!(
                text.contains(name),
                "man page should mention subcommand '{name}'"
            );
        }
    }

    #[test]
    fn man_page_contains_key_sections() {
        let buf = generate_man_page();
        let text = String::from_utf8(buf).expect("man page should be valid UTF-8");
        // Standard sections generated by clap_mangen.
        for section in &["SYNOPSIS", "DESCRIPTION", "OPTIONS"] {
            assert!(
                text.contains(&format!(".SH {section}")),
                "man page should have a {section} section"
            );
        }
        // SUBCOMMANDS is the section clap_mangen uses for subcommands (the
        // issue's "COMMANDS" requirement).
        assert!(
            text.contains(".SH SUBCOMMANDS"),
            "man page should have a SUBCOMMANDS section (covers COMMANDS)"
        );
        // Custom sections appended after clap_mangen output.
        for section in &[
            "EXAMPLES",
            "ENVIRONMENT",
            "FILES",
            "EXIT STATUS",
            "SEE ALSO",
        ] {
            assert!(
                text.contains(&format!(".SH \"{section}\"")),
                "man page should have a {section} section"
            );
        }
    }

    #[test]
    fn man_page_contains_environment_variables() {
        let buf = generate_man_page();
        let text = String::from_utf8(buf).expect("man page should be valid UTF-8");
        for var in &[
            "BRIGID_LLM_API_KEY",
            "DEEPSEEK_API_KEY",
            "BRIGID_LLM_BASE_URL",
            "BRIGID_FORCE_MOCK",
        ] {
            assert!(
                text.contains(var),
                "man page ENVIRONMENT section should mention {var}"
            );
        }
    }

    #[test]
    fn man_page_contains_exit_codes() {
        let buf = generate_man_page();
        let text = String::from_utf8(buf).expect("man page should be valid UTF-8");
        // The EXIT STATUS section should mention exit codes 0 through 5.
        for code in 0..=5u8 {
            assert!(
                text.contains(&format!("{code}  ")),
                "man page EXIT STATUS should mention exit code {code}"
            );
        }
    }

    #[test]
    fn cmd_manpage_stdout_exits_zero() {
        let code = cmd_manpage(None);
        assert_eq!(code, ExitCode::from(EXIT_OK));
    }

    #[test]
    fn cmd_manpage_output_flag_writes_file() {
        let dir = temp_dir("manpage-output");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("brigid.1");
        let code = cmd_manpage(Some(path.clone()));
        assert_eq!(code, ExitCode::from(EXIT_OK));
        assert!(path.is_file(), "man page file should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(".TH "),
            "written man page should be valid troff"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_kind_to_completion_shell_maps_all_variants() {
        assert!(matches!(
            ShellKind::Bash.to_completion_shell(),
            clap_complete::Shell::Bash
        ));
        assert!(matches!(
            ShellKind::Zsh.to_completion_shell(),
            clap_complete::Shell::Zsh
        ));
        assert!(matches!(
            ShellKind::Fish.to_completion_shell(),
            clap_complete::Shell::Fish
        ));
        assert!(matches!(
            ShellKind::PowerShell.to_completion_shell(),
            clap_complete::Shell::PowerShell
        ));
    }

    #[test]
    fn shell_name_returns_lowercase_identifiers() {
        assert_eq!(shell_name(ShellKind::Bash), "bash");
        assert_eq!(shell_name(ShellKind::Zsh), "zsh");
        assert_eq!(shell_name(ShellKind::Fish), "fish");
        assert_eq!(shell_name(ShellKind::PowerShell), "powershell");
    }

    #[test]
    fn cmd_completions_bash_to_stdout_succeeds() {
        let code = cmd_completions(ShellKind::Bash, None);
        assert_eq!(code, ExitCode::from(EXIT_OK));
    }

    #[test]
    fn cmd_completions_writes_file_when_output_given() {
        let dir = temp_dir("completions-unit-output");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("brigid.bash");
        let code = cmd_completions(ShellKind::Bash, Some(path.clone()));
        assert_eq!(code, ExitCode::from(EXIT_OK));
        assert!(path.is_file());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.is_empty());
        assert!(content.contains("_brigid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_completions_output_to_unwritable_path_exits_config() {
        // A path whose parent directory does not exist cannot be created.
        let path = PathBuf::from("/nonexistent-dir-xyz/brigid.bash");
        let code = cmd_completions(ShellKind::Bash, Some(path));
        assert_eq!(code, ExitCode::from(EXIT_CONFIG));
    }

    #[test]
    fn is_force_mock_enabled_treats_falsy_values_as_disabled() {
        // Unset env var → disabled.
        assert!(!is_force_mock_enabled(None));
        // Falsy values → disabled (case-insensitive, trimmed).
        for falsy in &["", "  ", "0", "false", "FALSE", "No", "  off  "] {
            assert!(
                !is_force_mock_enabled(Some(falsy)),
                "BRIGID_FORCE_MOCK={falsy:?} should be treated as disabled"
            );
        }
        // Truthy values → enabled.
        for truthy in &["1", "true", "yes", "on", "anything"] {
            assert!(
                is_force_mock_enabled(Some(truthy)),
                "BRIGID_FORCE_MOCK={truthy:?} should be treated as enabled"
            );
        }
    }

    #[test]
    fn mock_fail_error_returns_expected_variant_for_each_keyword() {
        assert!(matches!(mock_fail_error("timeout"), LlmError::Timeout));
        assert!(matches!(
            mock_fail_error("ratelimit"),
            LlmError::RateLimit { retry_after: None }
        ));
        match mock_fail_error("provider") {
            LlmError::Provider { status, body } => {
                assert_eq!(status, 502);
                assert_eq!(body, "mock provider error");
            }
            other => panic!("expected Provider, got {other:?}"),
        }
        assert!(matches!(mock_fail_error("parse"), LlmError::Parse { .. }));
        assert!(matches!(
            mock_fail_error("network"),
            LlmError::Network { .. }
        ));
        // Unknown keyword falls through to network error.
        assert!(matches!(
            mock_fail_error("unknown"),
            LlmError::Network { .. }
        ));
    }

    #[tokio::test]
    async fn mock_client_with_empty_responses_falls_back_to_placeholder() {
        // `MockClient::with_responses` rejects an empty sequence; the helper
        // must fall back to a single placeholder response (with a stderr
        // warning) instead of panicking. The fallback is defensive: all
        // current call sites assemble non-empty sequences, so it cannot be
        // exercised through the CLI binary itself. The diagnostic writer is
        // injected so the warning text can be asserted without capturing the
        // process stderr.
        let mut diag = Vec::new();
        let client = mock_client_with_diag(Vec::new(), &mut diag);
        let response = complete_text(client.as_ref(), "anything")
            .await
            .expect("fallback client should respond");
        assert_eq!(response, PLACEHOLDER_IDENTIFY_YAML);
        let diag = String::from_utf8(diag).expect("diag should be valid utf-8");
        assert!(
            diag.contains("warning: mock client: falling back to default placeholder response"),
            "expected fallback warning in diag, got: {diag:?}"
        );
        assert!(
            diag.contains("MockClient::with_responses requires at least one response"),
            "expected underlying construction error in diag, got: {diag:?}"
        );
    }

    #[tokio::test]
    async fn mock_client_with_responses_serves_sequence_without_fallback() {
        let client = mock_client(vec!["first".to_string(), "second".to_string()]);
        assert_eq!(complete_text(client.as_ref(), "a").await.unwrap(), "first");
        assert_eq!(complete_text(client.as_ref(), "b").await.unwrap(), "second");
    }

    // --- cache prune / stats ---

    #[test]
    fn cache_prune_deletes_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.sqlite");

        // Create a valid SQLite kv store and add an entry.
        let store = SqliteKvStore::open(&db_path).unwrap();
        use llm_kernel::store::KvStore;
        store.put("key1", b"value1").unwrap();
        drop(store);
        assert!(db_path.exists());

        // Prune should delete the file.
        let code = cmd_cache_prune(&db_path);
        assert_eq!(code, ExitCode::from(EXIT_OK));
        assert!(!db_path.exists());
    }

    #[test]
    fn cache_prune_no_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.sqlite");
        assert!(!db_path.exists());
        let code = cmd_cache_prune(&db_path);
        assert_eq!(code, ExitCode::from(EXIT_OK));
    }

    #[test]
    fn cache_stats_reports_entry_count() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.sqlite");

        // Create a store with 3 entries.
        let store = SqliteKvStore::open(&db_path).unwrap();
        use llm_kernel::store::KvStore;
        store.put("k1", b"v1").unwrap();
        store.put("k2", b"v2").unwrap();
        store.put("k3", b"v3").unwrap();
        drop(store);

        // Stats should report 3 entries.
        let count = brigid_pipeline::CacheAdmin::entry_count(&db_path).unwrap();
        assert_eq!(count, 3);

        let code = cmd_cache_stats(&db_path);
        assert_eq!(code, ExitCode::from(EXIT_OK));
    }

    #[test]
    fn cache_stats_no_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.sqlite");
        let code = cmd_cache_stats(&db_path);
        assert_eq!(code, ExitCode::from(EXIT_OK));
    }

    #[test]
    fn cache_stats_does_not_create_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.sqlite");
        assert!(!db_path.exists());

        // CacheAdmin::entry_count opens read-only, so it must not create the file.
        let _ = brigid_pipeline::CacheAdmin::entry_count(&db_path);
        assert!(
            !db_path.exists(),
            "read-only open created the database file"
        );
    }

    #[test]
    fn fmt_file_size_human_readable() {
        assert_eq!(fmt_file_size(512), "512 B");
        assert_eq!(fmt_file_size(1024), "1.0 KB");
        assert_eq!(fmt_file_size(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_file_size(1024 * 1024 * 1024), "1.0 GB");
    }
}
