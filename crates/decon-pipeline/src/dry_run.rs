//! Dry-run plan: crawl + scope + setup assessment + budget (no LLM).
//!
//! Assembles the Milestone 1 plan used by `decon dry-run`. Parity with
//! `tests/fixtures/baseline.json` is enforced by integration tests.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use decon_core::{
    BudgetConfig, BudgetEstimate, FileSize, FilterStats, ModuleCount, ModuleKey, SetupAssessment,
    assess_setup, discover_modules, estimate_budget, filter_files_by_scope, module_key,
    redact_content, unscoped_filter_stats,
};
use decon_crawl::{CrawlError, CrawlOptions, crawl_local_with_options};
use thiserror::Error;

/// Full dry-run plan for a repository root (zero LLM calls).
#[must_use]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DryRunPlan {
    /// Root path supplied to [`dry_run`] / [`dry_run_with_budget`] (may be relative).
    pub root: PathBuf,
    /// Relative file inventory after optional scope filter (POSIX `/`).
    pub files: Vec<String>,
    /// Module inventory from the **unscoped** crawl (baseline `modules` map).
    ///
    /// Intentionally not re-scoped: baseline and setup assessment always see
    /// the full-repo module layout; only [`Self::files`] / [`Self::budget`]
    /// reflect `--apps` filtering.
    pub modules: Vec<ModuleCount>,
    /// Filter statistics for this run (unscoped or scoped).
    pub filter_stats: FilterStats,
    /// Setup-doc assessment (README + full unscoped file list, matching baseline).
    pub setup: SetupAssessment,
    /// Context budget estimate for the **scoped** working set.
    pub budget: BudgetEstimate,
}

/// Errors while building a dry-run plan (crawl failures or file I/O).
///
/// The CLI maps these to non-zero exit codes; library callers should treat
/// them as terminal for the plan assembly step.
#[derive(Debug, Error)]
pub enum DryRunError {
    /// Local crawl failed.
    #[error(transparent)]
    Crawl(#[from] CrawlError),
    /// Failed to read a file under the root (e.g. README for setup scoring).
    #[error("failed to read {path}: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// File byte length does not fit in `usize` on this platform (e.g. multi-GiB
    /// file on a 32-bit target). Prefer failing loudly over silently clamping.
    #[error("file size overflow for {path}: {size} bytes exceeds usize::MAX")]
    FileSizeOverflow {
        /// Path that was too large to represent as `usize` chars.
        path: PathBuf,
        /// Raw size from metadata (`u64`).
        size: u64,
    },
}

/// Build a dry-run plan for `root`, optionally scoping to `apps` / modules.
///
/// Steps:
/// 1. [`crawl_local_with_options`] -- sorted relative inventory with per-file byte sizes
/// 2. [`discover_modules`] on the full inventory
/// 3. Optional [`filter_files_by_scope`] (or unscoped stats)
/// 4. [`assess_setup`] from `README.md` + **full** inventory (parity with baseline)
/// 5. [`estimate_budget`] on the working (scoped) file set using crawl sizes
///
/// # Errors
///
/// Propagates crawl failures and I/O when reading the README for setup scoring,
/// or when a crawl-reported file size does not fit in `usize` on this platform.
/// A **missing** README is treated as empty content (score gaps), not an error.
///
/// # Examples
///
/// ```no_run
/// use decon_pipeline::dry_run::dry_run;
///
/// let plan = dry_run(".", None).expect("dry-run");
/// // Empty repos are valid: zero files and zero batches.
/// assert_eq!(plan.budget.file_count, plan.files.len());
/// if plan.files.is_empty() {
///     assert_eq!(plan.budget.batch_count, 0);
/// } else {
///     assert!(plan.budget.batch_count >= 1);
/// }
/// ```
pub fn dry_run(
    root: impl AsRef<Path>,
    scope: Option<&[ModuleKey]>,
) -> Result<DryRunPlan, DryRunError> {
    dry_run_with_budget(root, scope, &BudgetConfig::default())
}

/// Same as [`dry_run`] with an explicit budget configuration.
///
/// # Errors
///
/// Same as [`dry_run`].
pub fn dry_run_with_budget(
    root: impl AsRef<Path>,
    scope: Option<&[ModuleKey]>,
    budget_config: &BudgetConfig,
) -> Result<DryRunPlan, DryRunError> {
    dry_run_with_options(root, scope, budget_config, CrawlOptions::default())
}

/// Same as [`dry_run_with_budget`] with explicit [`CrawlOptions`] for
/// incremental git-diff crawl.
///
/// When [`CrawlOptions::since`] is `Some(ref)`, the file inventory is filtered
/// to only files that changed since that git ref (via
/// [`decon_crawl::crawl_local_with_options`]). Setup assessment and module
/// discovery still use the filtered inventory (consistent with `--apps`
/// scoping behavior).
///
/// # Errors
///
/// Same as [`dry_run_with_budget`], plus [`DryRunError::Crawl`] wrapping
/// [`CrawlError::GitDiff`] when `since` is set but the directory is not a git
/// repo or the ref cannot be resolved.
///
/// # Examples
///
/// ```no_run
/// use decon_crawl::CrawlOptions;
/// use decon_pipeline::dry_run::dry_run_with_options;
///
/// let opts = CrawlOptions::since("v0.5.0");
/// let plan = dry_run_with_options(".", None, &Default::default(), opts)
///     .expect("incremental dry-run");
/// ```
pub fn dry_run_with_options(
    root: impl AsRef<Path>,
    scope: Option<&[ModuleKey]>,
    budget_config: &BudgetConfig,
    crawl_options: CrawlOptions,
) -> Result<DryRunPlan, DryRunError> {
    let root = root.as_ref();
    let crawl = crawl_local_with_options(root, crawl_options)?;
    let all_files = crawl.files;
    let all_sizes = crawl.sizes;
    let modules = discover_modules(all_files.iter().map(String::as_str));

    // Setup always uses the full inventory (baseline parity); evaluate before
    // we possibly move `all_files` into the unscoped working set.
    let readme_path = root.join("README.md");
    // Tolerate a missing README only; other I/O errors (permissions, EISDIR, ...)
    // must surface as DryRunError::Io so setup is not silently wrong.
    let readme = match fs::read_to_string(&readme_path) {
        Ok(content) => redact_content(&content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(DryRunError::Io {
                path: readme_path,
                source,
            });
        }
    };
    let setup = assess_setup(&readme, all_files.iter().map(String::as_str));

    let (files, sizes, filter_stats) = match scope {
        None => {
            // Common path: move inventory -- no clone.
            let stats = unscoped_filter_stats(all_files.len(), &modules);
            (all_files, all_sizes, stats)
        }
        Some(keys) => {
            let filtered = filter_files_by_scope(all_files.iter().map(String::as_str), keys);
            // Keep sizes parallel to the filtered file list. Build a lookup
            // from path -> size so we can map each kept file to its byte length
            // without re-statting the filesystem.
            let size_map: HashMap<&str, u64> = all_files
                .iter()
                .zip(all_sizes.iter())
                .map(|(f, s)| (f.as_str(), *s))
                .collect();
            let filtered_sizes: Vec<u64> = filtered
                .files
                .iter()
                .map(|f| size_map.get(f.as_str()).copied().unwrap_or(0))
                .collect();
            (filtered.files, filtered_sizes, filtered.stats)
        }
    };

    let budget = estimate_budget_for_files(&files, &sizes, budget_config)?;

    Ok(DryRunPlan {
        root: root.to_path_buf(),
        files,
        modules,
        filter_stats,
        setup,
        budget,
    })
}

fn estimate_budget_for_files(
    files: &[String],
    sizes: &[u64],
    config: &BudgetConfig,
) -> Result<BudgetEstimate, DryRunError> {
    // Sizes were collected during `crawl_local_with_options` (following symlinks via
    // `fs::metadata`), so dry-run no longer re-stats every path. We only
    // need to convert `u64` -> `usize` for the budget model.
    let mut file_sizes: Vec<FileSize> = Vec::with_capacity(files.len());
    for (rel, &size) in files.iter().zip(sizes.iter()) {
        let chars = match usize::try_from(size) {
            Ok(n) => n,
            Err(_) => {
                return Err(DryRunError::FileSizeOverflow {
                    path: PathBuf::from(rel),
                    size,
                });
            }
        };
        file_sizes.push(FileSize {
            path: rel.clone(),
            chars,
            module: module_key(rel),
        });
    }
    Ok(estimate_budget(&file_sizes, config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Monotonic counter to guarantee unique temp dirs across parallel tests.
    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-pipeline-dry-run-{nanos}-{seq}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn missing_readme_is_tolerated_as_empty() {
        let dir = unique_temp_dir();
        // Empty tree: crawl succeeds; no README.md -> empty string, not an error.
        let plan = dry_run(&dir, None).expect("missing README must not fail dry-run");
        assert!(!plan.setup.signals.has_readme);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_readme_returns_io_error() {
        let dir = unique_temp_dir();
        // A directory named README.md makes read_to_string fail with a non-NotFound
        // error (IsADirectory / InvalidInput depending on OS) -- not silently empty.
        fs::create_dir(dir.join("README.md")).expect("create README.md as directory");
        let err = dry_run(&dir, None).expect_err("unreadable README must be DryRunError::Io");
        assert!(
            matches!(err, DryRunError::Io { .. }),
            "expected Io error, got: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_on_empty_repo_yields_zero_files_batches_and_chars() {
        let dir = unique_temp_dir();
        let plan = dry_run(&dir, None).expect("empty repo dry-run");
        assert_eq!(plan.files.len(), 0);
        assert_eq!(plan.budget.file_count, 0);
        assert_eq!(plan.budget.batch_count, 0);
        assert_eq!(plan.budget.raw_chars, 0);
        assert_eq!(plan.budget.budgeted_chars, 0);
        assert_eq!(plan.budget.token_estimate, 0);
        assert!(!plan.budget.oversized_batch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn readme_secrets_are_redacted_before_assess_setup() {
        let dir = unique_temp_dir();
        let readme = "# Project\n\n## Prerequisites\nRequires Rust.\n## Install\ncargo install.\n## Run\ncargo run.\n\nDB_KEY=dummyvalue\n";
        fs::write(dir.join("README.md"), readme).expect("write README");

        let plan = dry_run(&dir, None).expect("dry-run with secret in README");

        // If redaction was applied, readme_length reflects the redacted content
        // (the secret value replaced with ****), not the raw file length.
        let redacted = decon_core::redact_content(readme);
        assert_ne!(
            redacted.len(),
            readme.len(),
            "test precondition: redaction must change length"
        );
        assert_eq!(
            plan.setup.signals.readme_length,
            redacted.len(),
            "README content should be redacted before reaching assess_setup"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_with_budget_respects_custom_config() {
        let dir = unique_temp_dir();
        // 100-byte file exceeds a tiny max_file_chars so truncated_file_count ticks.
        fs::write(dir.join("big.txt"), "x".repeat(100)).expect("write big.txt");
        let cfg = BudgetConfig {
            max_file_chars: 10,
            batch_char_budget: 50,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        };
        let plan = dry_run_with_budget(&dir, None, &cfg).expect("custom budget dry-run");
        assert_eq!(plan.budget.file_count, 1);
        assert_eq!(plan.budget.raw_chars, 100);
        assert!(
            plan.budget.truncated_file_count >= 1,
            "expected truncation under max_file_chars=10, got {:?}",
            plan.budget
        );
        assert!(
            plan.budget.budgeted_chars < plan.budget.raw_chars,
            "budgeted should be capped below raw"
        );
        // Default path would use max_file_chars=12_000 and not truncate this file.
        let default_plan = dry_run(&dir, None).expect("default budget");
        assert_eq!(default_plan.budget.truncated_file_count, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    // --- Issue #225: dry_run_with_options with CrawlOptions::since ---

    /// Helper: create a temp git repo with an initial commit (tag `v1`) and a
    /// second commit adding `new.txt`.
    fn git_repo_with_two_commits() -> PathBuf {
        use std::process::Command;
        let dir = unique_temp_dir();
        fs::write(dir.join("old.txt"), "old content\n").expect("write old.txt");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C"])
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test")
                .status()
                .expect("git command");
            assert!(
                status.success(),
                "git {:?} failed in {}",
                args,
                dir.display()
            );
        };
        git(&["init"]);
        git(&["add", "."]);
        git(&["commit", "-m", "initial"]);
        git(&["tag", "v1"]);
        // Second commit: add a new file.
        fs::write(dir.join("new.txt"), "new content\n").expect("write new.txt");
        git(&["add", "."]);
        git(&["commit", "-m", "add new.txt"]);
        dir
    }

    /// `dry_run_with_options` with `CrawlOptions::since("v1")` filters the
    /// inventory to only files changed since `v1` (i.e. `new.txt`).
    #[test]
    fn dry_run_with_options_since_filters_files() {
        let dir = git_repo_with_two_commits();
        let opts = decon_crawl::CrawlOptions::since("v1");
        let plan = dry_run_with_options(&dir, None, &BudgetConfig::default(), opts)
            .expect("incremental dry-run");
        // Only new.txt should be in the inventory (changed since v1).
        assert_eq!(
            plan.files.len(),
            1,
            "expected 1 changed file, got {:?}",
            plan.files
        );
        assert!(
            plan.files.iter().any(|f| f == "new.txt"),
            "expected new.txt in files: {:?}",
            plan.files
        );
        assert!(
            !plan.files.iter().any(|f| f == "old.txt"),
            "old.txt should be filtered out: {:?}",
            plan.files
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `dry_run_with_options` with default (full) options behaves like `dry_run`.
    #[test]
    fn dry_run_with_options_full_matches_dry_run() {
        let dir = git_repo_with_two_commits();
        let full = dry_run(&dir, None).expect("full dry-run");
        let opts = decon_crawl::CrawlOptions::full();
        let plan = dry_run_with_options(&dir, None, &BudgetConfig::default(), opts)
            .expect("full options dry-run");
        assert_eq!(plan.files, full.files, "full options should match dry_run");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `dry_run_with_options` with `since` set on a non-git directory returns
    /// `DryRunError::Crawl` (wrapping `CrawlError::GitDiff`).
    #[test]
    fn dry_run_with_options_since_non_git_repo_errors() {
        let dir = unique_temp_dir();
        fs::write(dir.join("a.txt"), "a\n").expect("write a.txt");
        let opts = decon_crawl::CrawlOptions::since("v1");
        let err = dry_run_with_options(&dir, None, &BudgetConfig::default(), opts)
            .expect_err("non-git should error");
        assert!(
            matches!(err, DryRunError::Crawl(ref e) if e.to_string().contains("git diff")),
            "expected Crawl(GitDiff) error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
