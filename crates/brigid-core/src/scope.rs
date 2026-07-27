//! Monorepo app/module scope filtering.
//!
//! When analyzing a subset of a monorepo (`--apps`), keep files that belong to
//! the selected modules **plus** shared root scaffolding (`_root` and
//! `config/`). See `docs/best-practices.md` §2.3.
//!
//! Filter statistics match the frozen parity baseline
//! (`tests/fixtures/baseline.json`).

use std::collections::HashSet;

use crate::module::{ModuleCount, ModuleKey, ROOT_MODULE, module_key, module_sort_key};

/// Statistics describing a (possibly scoped) file filter pass.
///
/// Invariant: `after <= before`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilterStats {
    /// `true` when a scope filter was applied (including an empty scope list).
    pub filtered: bool,
    /// File count before filtering.
    pub before: usize,
    /// File count after filtering (`<= before`).
    pub after: usize,
    /// Shared scaffolding files kept that were **not** already in scope.
    pub kept_shared: usize,
    /// Module keys for this run (deduplicated scope keys when filtered; full
    /// inventory keys when unscoped), ordered `apps/*` → `_root` → others.
    pub module_keys: Vec<ModuleKey>,
}

/// Result of applying a scope filter: kept relative paths and stats.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeFilterResult {
    /// Relative file paths retained after filtering (original order preserved
    /// among kept files).
    pub files: Vec<String>,
    /// Filter statistics for the run.
    pub stats: FilterStats,
}

/// Whether a module key is shared root scaffolding kept under app scope.
///
/// Shared modules are [`ROOT_MODULE`] (`_root`) and `config`.
///
/// # Examples
///
/// ```
/// use brigid_core::scope::is_shared_module;
/// use brigid_core::module::ModuleKey;
///
/// assert!(is_shared_module(&ModuleKey::new("_root")));
/// assert!(is_shared_module(&ModuleKey::new("config")));
/// assert!(!is_shared_module(&ModuleKey::new("apps/alpha")));
/// ```
#[must_use]
pub fn is_shared_module(key: &ModuleKey) -> bool {
    let s = key.as_str();
    s == ROOT_MODULE || s == "config"
}

/// Build unscoped filter stats (no app filter applied).
///
/// `module_keys` are taken from `modules` and re-sorted with the baseline
/// order (`apps/*` → `_root` → others), so callers need not pass a
/// pre-ordered inventory.
///
/// # Examples
///
/// ```
/// use brigid_core::module::discover_modules;
/// use brigid_core::scope::unscoped_filter_stats;
///
/// let files = ["README.md", "src/a.rs"];
/// let modules = discover_modules(files.iter().copied());
/// let stats = unscoped_filter_stats(files.len(), &modules);
/// assert!(!stats.filtered);
/// assert_eq!(stats.before, 2);
/// assert_eq!(stats.after, 2);
/// assert_eq!(stats.kept_shared, 0);
/// ```
#[must_use]
pub fn unscoped_filter_stats(file_count: usize, modules: &[ModuleCount]) -> FilterStats {
    let mut module_keys: Vec<ModuleKey> = modules.iter().map(|m| m.key.clone()).collect();
    sort_module_keys(&mut module_keys);
    FilterStats {
        filtered: false,
        before: file_count,
        after: file_count,
        kept_shared: 0,
        module_keys,
    }
}

/// Filter relative file paths to the given module scope plus shared scaffolding.
///
/// - Files whose [`module_key`] is in `scope` are kept.
/// - Files in shared modules (`_root`, `config`) are always kept.
/// - `kept_shared` counts shared files that were **not** already listed in
///   `scope`.
/// - `stats.module_keys` is the **deduplicated** scope list sorted with the
///   baseline order (`apps/*` → `_root` → others), not the discovered inventory.
///
/// An **empty** `scope` still sets `filtered: true` and keeps only shared
/// scaffolding (no app modules).
///
/// # Examples
///
/// ```
/// use brigid_core::module::ModuleKey;
/// use brigid_core::scope::filter_files_by_scope;
///
/// let files = [
///     "README.md",
///     "apps/alpha/lib/a.ex",
///     "apps/beta/lib/b.ex",
///     "config/config.exs",
/// ];
/// let scope = [ModuleKey::new("apps/alpha")];
/// let result = filter_files_by_scope(files.iter().copied(), &scope);
/// assert!(result.stats.filtered);
/// assert_eq!(result.stats.after, 3); // README, alpha, config
/// assert_eq!(result.stats.kept_shared, 2); // README + config
/// ```
#[must_use]
pub fn filter_files_by_scope<I, S>(files: I, scope: &[ModuleKey]) -> ScopeFilterResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let scope_set: HashSet<&str> = scope.iter().map(ModuleKey::as_str).collect();

    let mut kept_files: Vec<String> = Vec::new();
    let mut kept_shared = 0;
    let mut before = 0;

    for file in files {
        before += 1;
        let path = file.as_ref();
        let key = module_key(path);
        let key_str = key.as_str();
        let in_scope = scope_set.contains(key_str);
        let shared = is_shared_module(&key);
        if in_scope || shared {
            kept_files.push(path.to_owned());
            if shared && !in_scope {
                kept_shared += 1;
            }
        }
    }

    let module_keys = unique_sorted_module_keys(scope);

    let after = kept_files.len();
    ScopeFilterResult {
        files: kept_files,
        stats: FilterStats {
            filtered: true,
            before,
            after,
            kept_shared,
            module_keys,
        },
    }
}

/// Deduplicate `keys` (first occurrence wins) then sort by baseline order.
fn unique_sorted_module_keys(keys: &[ModuleKey]) -> Vec<ModuleKey> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(keys.len());
    let mut unique: Vec<ModuleKey> = Vec::with_capacity(keys.len());
    for key in keys {
        if seen.insert(key.as_str()) {
            unique.push(key.clone());
        }
    }
    sort_module_keys(&mut unique);
    unique
}

fn sort_module_keys(keys: &mut [ModuleKey]) {
    keys.sort_by(|a, b| module_sort_key(a.as_str()).cmp(&module_sort_key(b.as_str())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::discover_modules;
    use proptest::prelude::*;

    const UMBRELLA_FILES: &[&str] = &[
        ".env.example",
        "README.md",
        "apps/alpha/lib/alpha.ex",
        "apps/alpha/mix.exs",
        "apps/beta/lib/beta.ex",
        "apps/beta/mix.exs",
        "apps/gamma/lib/gamma.ex",
        "apps/gamma/mix.exs",
        "config/config.exs",
        "mix.exs",
    ];

    const PYTHON_LIB_FILES: &[&str] = &[
        ".env.example",
        "README.md",
        "pyproject.toml",
        "src/mylib/__init__.py",
        "src/mylib/cli.py",
        "src/mylib/core.py",
    ];

    const JS_LIB_FILES: &[&str] = &[
        ".env.example",
        "README.md",
        "package.json",
        "src/index.ts",
        "src/utils.ts",
        "tsconfig.json",
    ];

    #[test]
    fn is_shared_module_root_and_config_only() {
        assert!(is_shared_module(&ModuleKey::new("_root")));
        assert!(is_shared_module(&ModuleKey::new("config")));
        assert!(!is_shared_module(&ModuleKey::new("apps/alpha")));
        assert!(!is_shared_module(&ModuleKey::new("src")));
        assert!(!is_shared_module(&ModuleKey::new("apps")));
    }

    #[test]
    fn unscoped_python_lib_matches_baseline() {
        let modules = discover_modules(PYTHON_LIB_FILES.iter().copied());
        let stats = unscoped_filter_stats(PYTHON_LIB_FILES.len(), &modules);
        assert!(!stats.filtered);
        assert_eq!(stats.before, 6);
        assert_eq!(stats.after, 6);
        assert_eq!(stats.kept_shared, 0);
        assert_eq!(
            stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["_root", "src"]
        );
    }

    #[test]
    fn unscoped_umbrella_matches_baseline() {
        let modules = discover_modules(UMBRELLA_FILES.iter().copied());
        let stats = unscoped_filter_stats(UMBRELLA_FILES.len(), &modules);
        assert!(!stats.filtered);
        assert_eq!(stats.before, 10);
        assert_eq!(stats.after, 10);
        assert_eq!(stats.kept_shared, 0);
        assert_eq!(
            stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["apps/alpha", "apps/beta", "apps/gamma", "_root", "config"]
        );
    }

    #[test]
    fn unscoped_js_lib_matches_baseline() {
        let modules = discover_modules(JS_LIB_FILES.iter().copied());
        let stats = unscoped_filter_stats(JS_LIB_FILES.len(), &modules);
        assert!(!stats.filtered);
        assert_eq!(stats.before, 6);
        assert_eq!(stats.after, 6);
        assert_eq!(stats.kept_shared, 0);
        assert_eq!(
            stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["_root", "src"]
        );
    }

    #[test]
    fn scoped_umbrella_alpha_only_matches_baseline() {
        let scope = [ModuleKey::new("apps/alpha")];
        let result = filter_files_by_scope(UMBRELLA_FILES.iter().copied(), &scope);
        assert!(result.stats.filtered);
        assert_eq!(result.stats.before, 10);
        assert_eq!(result.stats.after, 6);
        assert_eq!(result.stats.kept_shared, 4);
        assert_eq!(
            result
                .stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["apps/alpha"]
        );
        // Shared: .env.example, README.md, mix.exs, config/config.exs
        // In scope: apps/alpha/lib/alpha.ex, apps/alpha/mix.exs
        assert_eq!(
            result.files,
            vec![
                ".env.example",
                "README.md",
                "apps/alpha/lib/alpha.ex",
                "apps/alpha/mix.exs",
                "config/config.exs",
                "mix.exs",
            ]
        );
    }

    #[test]
    fn scoped_umbrella_alpha_beta_matches_baseline() {
        let scope = [ModuleKey::new("apps/alpha"), ModuleKey::new("apps/beta")];
        let result = filter_files_by_scope(UMBRELLA_FILES.iter().copied(), &scope);
        assert!(result.stats.filtered);
        assert_eq!(result.stats.before, 10);
        assert_eq!(result.stats.after, 8);
        assert_eq!(result.stats.kept_shared, 4);
        assert_eq!(
            result
                .stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["apps/alpha", "apps/beta"]
        );
        assert!(!result.files.iter().any(|f| f.starts_with("apps/gamma")));
    }

    #[test]
    fn scoped_module_keys_sort_apps_before_root() {
        // Scope listed out of order; stats.module_keys must use baseline order.
        let scope = [
            ModuleKey::new("_root"),
            ModuleKey::new("apps/beta"),
            ModuleKey::new("apps/alpha"),
        ];
        let result = filter_files_by_scope(UMBRELLA_FILES.iter().copied(), &scope);
        assert_eq!(
            result
                .stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["apps/alpha", "apps/beta", "_root"]
        );
        // _root is in scope, so shared root files are not counted as kept_shared.
        // config is still shared-only → 1 kept_shared (config/config.exs).
        assert_eq!(result.stats.kept_shared, 1);
    }

    #[test]
    fn empty_scope_keeps_only_shared() {
        let result = filter_files_by_scope(UMBRELLA_FILES.iter().copied(), &[]);
        assert!(result.stats.filtered);
        assert_eq!(result.stats.after, 4); // 3 _root + 1 config
        assert_eq!(result.stats.kept_shared, 4);
        assert!(result.stats.module_keys.is_empty());
    }

    #[test]
    fn scoped_module_keys_deduplicate_duplicates() {
        let scope = [
            ModuleKey::new("apps/alpha"),
            ModuleKey::new("apps/alpha"),
            ModuleKey::new("apps/beta"),
            ModuleKey::new("apps/alpha"),
        ];
        let result = filter_files_by_scope(UMBRELLA_FILES.iter().copied(), &scope);
        assert_eq!(
            result
                .stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["apps/alpha", "apps/beta"]
        );
        // Filtering still uses a set, so duplicates do not change kept counts.
        assert_eq!(result.stats.after, 8);
        assert_eq!(result.stats.kept_shared, 4);
    }

    #[test]
    fn unscoped_filter_stats_sorts_unsorted_input() {
        // Intentionally reverse of baseline order; output must still sort.
        let modules = vec![
            ModuleCount {
                key: ModuleKey::new("config"),
                count: 1,
            },
            ModuleCount {
                key: ModuleKey::new("_root"),
                count: 3,
            },
            ModuleCount {
                key: ModuleKey::new("apps/gamma"),
                count: 2,
            },
            ModuleCount {
                key: ModuleKey::new("apps/alpha"),
                count: 2,
            },
        ];
        let stats = unscoped_filter_stats(10, &modules);
        assert_eq!(
            stats
                .module_keys
                .iter()
                .map(ModuleKey::as_str)
                .collect::<Vec<_>>(),
            vec!["apps/alpha", "apps/gamma", "_root", "config"]
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        })]

        #[test]
        fn prop_scoped_filter_subset_of_unscoped(
            files in proptest::collection::vec(
                "(apps/alpha|apps/beta|apps/gamma|src|config|lib)/[a-z]{1,8}\\.(rs|ex|ts)",
                1..40,
            ),
            scope_keys in proptest::collection::vec(
                "(apps/alpha|apps/beta|apps/gamma|src|lib)",
                0..4,
            ),
        ) {
            let unscoped: std::collections::HashSet<String> =
                files.iter().cloned().collect();

            let scope: Vec<ModuleKey> =
                scope_keys.iter().map(ModuleKey::new).collect();
            let result = filter_files_by_scope(files.iter().cloned(), &scope);

            for kept in &result.files {
                prop_assert!(
                    unscoped.contains(kept),
                    "scoped result contains {:?} not in the unscoped inventory",
                    kept,
                );
            }
            prop_assert!(result.stats.after <= result.stats.before);
        }
    }
}
