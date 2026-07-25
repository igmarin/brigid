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
    unscoped_filter_stats,
};
use decon_crawl::{CrawlError, crawl_local};
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
/// 1. [`crawl_local`] -- sorted relative inventory with per-file byte sizes
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
    let root = root.as_ref();
    let crawl = crawl_local(root)?;
    let all_files = crawl.files;
    let all_sizes = crawl.sizes;
    let modules = discover_modules(all_files.iter().map(String::as_str));

    // Setup always uses the full inventory (baseline parity); evaluate before
    // we possibly move `all_files` into the unscoped working set.
    let readme_path = root.join("README.md");
    // Tolerate a missing README only; other I/O errors (permissions, EISDIR, ...)
    // must surface as DryRunError::Io so setup is not silently wrong.
    let readme = match fs::read_to_string(&readme_path) {
        Ok(content) => content,
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
    // Sizes were collected during `crawl_local` (following symlinks via
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
}
