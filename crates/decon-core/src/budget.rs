//! Context budget estimates for dry-run and map-batch packing.
//!
//! Pure helpers that answer: given file sizes and module keys, how many
//! characters/tokens and map batches will an identify pass roughly need?
//! See `docs/best-practices.md` §3.
//!
//! Defaults are frozen for stable dry-run JSON; they are heuristics, not a
//! real tokenizer.

use std::collections::BTreeMap;

use crate::module::{ModuleKey, module_sort_key};

/// Default per-file character cap after head+tail truncation.
pub const DEFAULT_MAX_FILE_CHARS: usize = 12_000;

/// Default soft character budget for one map batch (sum of budgeted bodies).
pub const DEFAULT_BATCH_CHAR_BUDGET: usize = 80_000;

/// Default rough token divisor: `token_estimate ≈ chars.div_ceil(CHARS_PER_TOKEN)`.
///
/// This is **not** a model tokenizer; it is a stable dry-run heuristic.
pub const DEFAULT_CHARS_PER_TOKEN: usize = 4;

/// Default max files kept with full (capped) body per module; the rest become
/// path-only stubs.
pub const DEFAULT_MAX_FULL_FILES_PER_MODULE: usize = 40;

/// Marker inserted between the kept head and tail when truncating a file.
pub const TRUNCATION_MARKER: &str = "\n...[truncated]...\n";

/// Prefix used for path-only stub lines.
pub const PATH_STUB_PREFIX: &str = "// path-only: ";

/// Tunable budget parameters. Prefer [`BudgetConfig::default`] for dry-run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetConfig {
    /// Max characters kept from a single file body after truncation.
    pub max_file_chars: usize,
    /// Soft max characters packed into one map batch.
    pub batch_char_budget: usize,
    /// Divisor for rough token estimate (`chars.div_ceil(chars_per_token)`).
    pub chars_per_token: usize,
    /// Full (capped) bodies kept per module before path stubs.
    pub max_full_files_per_module: usize,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_file_chars: DEFAULT_MAX_FILE_CHARS,
            batch_char_budget: DEFAULT_BATCH_CHAR_BUDGET,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            max_full_files_per_module: DEFAULT_MAX_FULL_FILES_PER_MODULE,
        }
    }
}

/// One file's size contribution for budgeting (no file body required).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSize {
    /// Relative path (POSIX `/` separators preferred).
    pub path: String,
    /// Character length of the file body (`content.chars().count()` or byte
    /// length — callers should be consistent; dry-run typically uses bytes).
    pub chars: usize,
    /// Module this file belongs to.
    pub module: ModuleKey,
}

/// Result of truncating a single file body to a character budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TruncateResult {
    /// Possibly truncated text.
    pub text: String,
    /// Whether truncation was applied.
    pub truncated: bool,
    /// Original character length of the input.
    pub original_chars: usize,
}

/// Aggregate dry-run budget estimate for a set of files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetEstimate {
    /// Number of input files.
    pub file_count: usize,
    /// Distinct modules present.
    pub module_count: usize,
    /// Sum of raw character lengths before caps/stubs.
    pub raw_chars: usize,
    /// Sum of budgeted sizes after per-file caps and path stubs.
    pub budgeted_chars: usize,
    /// Rough token estimate: `budgeted_chars.div_ceil(chars_per_token)`.
    pub token_estimate: usize,
    /// Number of map batches after packing modules by size.
    pub batch_count: usize,
    /// Files whose raw size exceeded `max_file_chars`.
    pub truncated_file_count: usize,
    /// Files represented as path-only stubs (module overflow).
    pub stubbed_file_count: usize,
    /// `true` if at least one module alone exceeds `batch_char_budget`.
    pub oversized_batch: bool,
}

/// Truncate `content` to at most `max_chars` using head + tail and
/// [`TRUNCATION_MARKER`].
///
/// When `content.chars().count() <= max_chars`, returns the content unchanged.
/// Otherwise keeps roughly half the budget for the head and half for the tail
/// (marker length is reserved from the budget).
///
/// # Examples
///
/// ```
/// use decon_core::budget::{truncate_content, TRUNCATION_MARKER};
///
/// let short = truncate_content("hello", 100);
/// assert!(!short.truncated);
/// assert_eq!(short.text, "hello");
///
/// let long = "a".repeat(100);
/// let out = truncate_content(&long, 20);
/// assert!(out.truncated);
/// assert!(out.text.contains(TRUNCATION_MARKER));
/// assert!(out.text.chars().count() <= 20 + TRUNCATION_MARKER.chars().count());
/// ```
#[must_use]
pub fn truncate_content(content: &str, max_chars: usize) -> TruncateResult {
    let original_chars = content.chars().count();
    if max_chars == 0 {
        return TruncateResult {
            text: String::new(),
            truncated: original_chars > 0,
            original_chars,
        };
    }
    if original_chars <= max_chars {
        return TruncateResult {
            text: content.to_owned(),
            truncated: false,
            original_chars,
        };
    }

    let marker_len = TRUNCATION_MARKER.chars().count();
    let body_budget = max_chars.saturating_sub(marker_len);
    // If max_chars is smaller than the marker, keep a tiny head only.
    if body_budget == 0 {
        let head: String = content.chars().take(max_chars).collect();
        return TruncateResult {
            text: head,
            truncated: true,
            original_chars,
        };
    }

    let head_len = body_budget / 2;
    let tail_len = body_budget - head_len;
    let head: String = content.chars().take(head_len).collect();
    let tail: String = content
        .chars()
        .skip(original_chars.saturating_sub(tail_len))
        .collect();
    TruncateResult {
        text: format!("{head}{TRUNCATION_MARKER}{tail}"),
        truncated: true,
        original_chars,
    }
}

/// Build a path-only stub line for a file kept as structure without body.
///
/// # Examples
///
/// ```
/// use decon_core::budget::{path_stub, PATH_STUB_PREFIX};
///
/// let s = path_stub("src/lib.rs");
/// assert!(s.starts_with(PATH_STUB_PREFIX));
/// assert!(s.contains("src/lib.rs"));
/// ```
#[must_use]
pub fn path_stub(path: &str) -> String {
    format!("{PATH_STUB_PREFIX}{path}")
}

/// Character cost of a path-only stub for budgeting (no allocation).
#[must_use]
pub fn path_stub_chars(path: &str) -> usize {
    PATH_STUB_PREFIX.chars().count() + path.chars().count()
}

/// Effective character contribution of one file after the per-file cap.
#[must_use]
pub fn capped_file_chars(raw_chars: usize, max_file_chars: usize) -> usize {
    raw_chars.min(max_file_chars)
}

/// Estimate context budget for a file inventory.
///
/// Algorithm (deterministic):
/// 1. Group files by module; within each module sort by path.
/// 2. Keep up to `max_full_files_per_module` files as full (capped) bodies;
///    remaining files in the module become path stubs.
/// 3. Pack modules in the same order as [`crate::discover_modules`]
///    (`apps/*` → `_root` → others) into batches under `batch_char_budget`.
///    A module that alone exceeds the budget still occupies its own batch and
///    sets [`BudgetEstimate::oversized_batch`].
///
/// # Examples
///
/// ```
/// use decon_core::budget::{estimate_budget, BudgetConfig, FileSize};
/// use decon_core::module::ModuleKey;
///
/// let files = [
///     FileSize {
///         path: "README.md".into(),
///         chars: 100,
///         module: ModuleKey::new("_root"),
///     },
///     FileSize {
///         path: "src/a.rs".into(),
///         chars: 50,
///         module: ModuleKey::new("src"),
///     },
/// ];
/// let est = estimate_budget(&files, &BudgetConfig::default());
/// assert_eq!(est.file_count, 2);
/// assert_eq!(est.raw_chars, 150);
/// assert!(est.batch_count >= 1);
/// ```
#[must_use]
pub fn estimate_budget(files: &[FileSize], config: &BudgetConfig) -> BudgetEstimate {
    if files.is_empty() {
        return BudgetEstimate {
            file_count: 0,
            module_count: 0,
            raw_chars: 0,
            budgeted_chars: 0,
            token_estimate: 0,
            batch_count: 0,
            truncated_file_count: 0,
            stubbed_file_count: 0,
            oversized_batch: false,
        };
    }

    let mut by_module: BTreeMap<String, Vec<&FileSize>> = BTreeMap::new();
    let mut raw_chars = 0;
    let mut truncated_file_count = 0;

    for f in files {
        raw_chars += f.chars;
        if f.chars > config.max_file_chars {
            truncated_file_count += 1;
        }
        by_module
            .entry(f.module.as_str().to_owned())
            .or_default()
            .push(f);
    }

    let mut module_keys: Vec<String> = by_module.keys().cloned().collect();
    module_keys.sort_by(|a, b| module_sort_key(a).cmp(&module_sort_key(b)));

    let mut module_costs: Vec<(String, usize, usize)> = Vec::new(); // key, cost, stubs
    let mut stubbed_file_count = 0;
    let mut budgeted_chars = 0;

    for key in &module_keys {
        let mut module_files = by_module.get(key).cloned().unwrap_or_default();
        module_files.sort_by(|a, b| a.path.cmp(&b.path));

        let mut cost = 0;
        let mut stubs = 0;
        for (i, f) in module_files.iter().enumerate() {
            if i < config.max_full_files_per_module {
                cost += capped_file_chars(f.chars, config.max_file_chars);
            } else {
                cost += path_stub_chars(&f.path);
                stubs += 1;
            }
        }
        stubbed_file_count += stubs;
        budgeted_chars += cost;
        module_costs.push((key.clone(), cost, stubs));
    }

    // Pack modules into batches (saturating arithmetic — overflow is not a
    // realistic file inventory size, but stay explicit).
    let mut batch_count = 0usize;
    let mut current = 0usize;
    let mut oversized_batch = false;

    for (_key, cost, _) in &module_costs {
        if *cost > config.batch_char_budget {
            oversized_batch = true;
        }
        if batch_count == 0 {
            batch_count = 1;
            current = *cost;
            continue;
        }
        if current.saturating_add(*cost) > config.batch_char_budget {
            batch_count = batch_count.saturating_add(1);
            current = *cost;
        } else {
            current = current.saturating_add(*cost);
        }
    }

    let token_estimate = if budgeted_chars == 0 || config.chars_per_token == 0 {
        0
    } else {
        budgeted_chars.div_ceil(config.chars_per_token)
    };

    BudgetEstimate {
        file_count: files.len(),
        module_count: module_keys.len(),
        raw_chars,
        budgeted_chars,
        token_estimate,
        batch_count,
        truncated_file_count,
        stubbed_file_count,
        oversized_batch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::{ModuleKey, module_key};
    use proptest::prelude::*;

    fn file(path: &str, chars: usize, module: &str) -> FileSize {
        FileSize {
            path: path.to_owned(),
            chars,
            module: ModuleKey::new(module),
        }
    }

    #[test]
    fn truncate_leaves_short_content_unchanged() {
        let r = truncate_content("hello world", 100);
        assert!(!r.truncated);
        assert_eq!(r.text, "hello world");
        assert_eq!(r.original_chars, 11);
    }

    #[test]
    fn truncate_long_content_inserts_marker() {
        let content = "H".repeat(50) + &"T".repeat(50);
        let r = truncate_content(&content, 30);
        assert!(r.truncated);
        assert!(r.text.contains(TRUNCATION_MARKER));
        assert!(r.text.starts_with('H'));
        assert!(r.text.ends_with('T'));
        assert_eq!(r.original_chars, 100);
    }

    #[test]
    fn truncate_counts_unicode_scalar_values_not_bytes() {
        // Each emoji is one char but multiple bytes.
        let content = "😀".repeat(40);
        assert_eq!(content.chars().count(), 40);
        assert!(content.len() > 40);
        let max_chars = 24; // larger than TRUNCATION_MARKER so head+marker+tail is used
        let r = truncate_content(&content, max_chars);
        assert!(r.truncated);
        assert_eq!(r.original_chars, 40);
        assert!(r.text.contains(TRUNCATION_MARKER));
        // Result is head + marker + tail with body budget max_chars - marker.
        assert_eq!(r.text.chars().count(), max_chars);
    }

    #[test]
    fn path_stub_format() {
        assert_eq!(path_stub("src/lib.rs"), "// path-only: src/lib.rs");
        assert_eq!(path_stub_chars("a"), PATH_STUB_PREFIX.len() + 1);
    }

    #[test]
    fn estimate_empty_input() {
        let est = estimate_budget(&[], &BudgetConfig::default());
        assert_eq!(est.file_count, 0);
        assert_eq!(est.batch_count, 0);
        assert_eq!(est.token_estimate, 0);
        assert!(!est.oversized_batch);
    }

    #[test]
    fn estimate_single_huge_file_truncates_and_may_oversized() {
        let cfg = BudgetConfig {
            max_file_chars: 100,
            batch_char_budget: 50,
            chars_per_token: 4,
            max_full_files_per_module: 10,
        };
        let files = [file("big.rs", 10_000, "src")];
        let est = estimate_budget(&files, &cfg);
        assert_eq!(est.file_count, 1);
        assert_eq!(est.truncated_file_count, 1);
        assert_eq!(est.budgeted_chars, 100); // capped
        assert_eq!(est.raw_chars, 10_000);
        assert_eq!(est.token_estimate, 25); // 100/4
        assert_eq!(est.batch_count, 1);
        assert!(est.oversized_batch); // 100 > 50
        assert_eq!(est.stubbed_file_count, 0);
    }

    #[test]
    fn estimate_batch_budget_smaller_than_single_truncated_file() {
        let cfg = BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 500,
            chars_per_token: 4,
            max_full_files_per_module: 40,
        };
        let files = [file("src/huge.rs", 5_000, "src")];
        let est = estimate_budget(&files, &cfg);
        assert_eq!(est.file_count, 1);
        assert_eq!(est.truncated_file_count, 1);
        assert_eq!(est.budgeted_chars, 1_000);
        assert_eq!(est.raw_chars, 5_000);
        assert_eq!(est.batch_count, 1);
        assert!(est.oversized_batch);
        assert_eq!(est.stubbed_file_count, 0);
    }

    #[test]
    fn estimate_multi_module_packing_splits_batches() {
        let cfg = BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 150,
            chars_per_token: 4,
            max_full_files_per_module: 10,
        };
        // Each module costs 100; two fit in 150? 100+100=200 > 150 → 2 batches.
        let files = [
            file("apps/alpha/a.ex", 100, "apps/alpha"),
            file("apps/beta/b.ex", 100, "apps/beta"),
            file("README.md", 100, "_root"),
        ];
        let est = estimate_budget(&files, &cfg);
        assert_eq!(est.module_count, 3);
        assert_eq!(est.file_count, 3);
        assert_eq!(est.budgeted_chars, 300);
        // Order: apps/alpha, apps/beta, _root — each 100.
        // Batch1: alpha(100)+beta fails → only alpha; Batch2: beta(100)+root fails → beta; Batch3: root.
        // Actually: start batch1 with alpha 100. beta 100 → 200 > 150 → new batch with beta 100.
        // root 100 → 200 > 150 → new batch with root. → 3 batches.
        assert_eq!(est.batch_count, 3);
        assert!(!est.oversized_batch);
    }

    #[test]
    fn estimate_stubs_overflow_files_in_module() {
        let cfg = BudgetConfig {
            max_file_chars: 1_000,
            batch_char_budget: 100_000,
            chars_per_token: 4,
            max_full_files_per_module: 2,
        };
        let files = [
            file("src/a.rs", 10, "src"),
            file("src/b.rs", 10, "src"),
            file("src/c.rs", 10, "src"),
            file("src/d.rs", 10, "src"),
        ];
        let est = estimate_budget(&files, &cfg);
        assert_eq!(est.stubbed_file_count, 2);
        // Two full at 10 + two path stubs (sorted: a,b full; c,d stub).
        assert_eq!(
            est.budgeted_chars,
            10 + 10 + path_stub_chars("src/c.rs") + path_stub_chars("src/d.rs")
        );
        assert_eq!(est.batch_count, 1);
    }

    #[test]
    fn estimate_token_uses_div_ceil() {
        let cfg = BudgetConfig {
            max_file_chars: 100,
            batch_char_budget: 10_000,
            chars_per_token: 4,
            max_full_files_per_module: 10,
        };
        let files = [file("a.rs", 5, "_root")];
        let est = estimate_budget(&files, &cfg);
        assert_eq!(est.budgeted_chars, 5);
        assert_eq!(est.token_estimate, 2); // ceil(5/4)
    }

    #[test]
    fn default_config_matches_public_constants() {
        let d = BudgetConfig::default();
        assert_eq!(d.max_file_chars, DEFAULT_MAX_FILE_CHARS);
        assert_eq!(d.batch_char_budget, DEFAULT_BATCH_CHAR_BUDGET);
        assert_eq!(d.chars_per_token, DEFAULT_CHARS_PER_TOKEN);
        assert_eq!(
            d.max_full_files_per_module,
            DEFAULT_MAX_FULL_FILES_PER_MODULE
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        })]

        #[test]
        fn prop_truncate_content_output_le_max_chars(
            content in proptest::collection::vec("[\\x20-\\x7e]{1,512}", 0..8),
            max_chars in 0usize..512,
        ) {
            let input = content.join("\n");
            let result = truncate_content(&input, max_chars);
            prop_assert!(
                result.text.chars().count() <= max_chars,
                "text len {} exceeds max_chars {} for input len {}",
                result.text.chars().count(),
                max_chars,
                input.chars().count(),
            );
        }

        #[test]
        fn prop_truncate_content_original_chars_matches_input(
            content in proptest::collection::vec("[\\x20-\\x7e]{1,256}", 0..8),
            max_chars in 0usize..512,
        ) {
            let input = content.join("\n");
            let result = truncate_content(&input, max_chars);
            prop_assert_eq!(result.original_chars, input.chars().count());
        }

        #[test]
        fn prop_estimate_budget_file_count_matches_input(
            files in proptest::collection::vec(
                ("(src|apps/alpha|apps/beta|config|lib)/[a-z]{1,8}\\.(rs|ex|ts)", 1usize..5000),
                0..32,
            ),
        ) {
            let file_sizes: Vec<FileSize> = files
                .iter()
                .map(|(path, chars)| file(path, *chars, module_key(path).as_str()))
                .collect();
            let est = estimate_budget(&file_sizes, &BudgetConfig::default());
            prop_assert_eq!(est.file_count, file_sizes.len());
            prop_assert_eq!(est.raw_chars, file_sizes.iter().map(|f| f.chars).sum::<usize>());
        }
    }
}
