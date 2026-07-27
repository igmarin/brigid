//! Module key derivation and inventory discovery.
//!
//! A **module key** groups repository files into coarse units used for
//! monorepo scoping, dry-run inventory, and later map-reduce batching.
//!
//! Rules match the frozen parity baseline (`tests/fixtures/baseline.json`)
//! and the independent regenerator heuristics:
//!
//! - Root-level files (no `/`) map to [`ModuleKey::ROOT`] (`_root`).
//! - Paths under `apps/<name>/…` (at least three components) map to
//!   `apps/<name>` (two-level umbrella key).
//! - Files directly under `apps/` (e.g. `apps/README.md`) map to `apps`.
//! - All other paths map to their first path component (e.g. `src/…` → `src`).
//!
//! A bare filename of `apps` (single component) is treated as a root-level
//! file and maps to `_root`, consistent with the “no `/` → `_root`” rule.
//!
//! Inventory ordering is: all `apps/*` keys (alphabetically), then `_root`,
//! then remaining keys alphabetically.

use std::collections::BTreeMap;
use std::fmt;

/// Sentinel module key for repository-root files (no directory component).
pub const ROOT_MODULE: &str = "_root";

/// A coarse module identifier derived from a relative file path.
///
/// Keys are path-like strings such as `_root`, `src`, or `apps/alpha`.
/// Equality is by string value.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ModuleKey(String);

impl ModuleKey {
    /// Module key for root-level files (`_root`).
    pub const ROOT: &'static str = ROOT_MODULE;

    /// Create a module key from an owned string (already-normalized).
    ///
    /// Prefer [`module_key`] when deriving from a file path.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Borrow the key as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ModuleKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<ModuleKey> for String {
    fn from(key: ModuleKey) -> Self {
        key.0
    }
}

/// One entry in a module inventory: key plus file count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleCount {
    /// Module key.
    pub key: ModuleKey,
    /// Number of files assigned to this module.
    pub count: usize,
}

/// Derive the module key for a relative file path using `/` separators.
///
/// # Examples
///
/// ```
/// use brigid_core::module::{module_key, ModuleKey};
///
/// assert_eq!(module_key("README.md").as_str(), ModuleKey::ROOT);
/// assert_eq!(module_key("apps/alpha/lib/alpha.ex").as_str(), "apps/alpha");
/// assert_eq!(module_key("src/index.ts").as_str(), "src");
/// ```
#[must_use]
pub fn module_key(path: &str) -> ModuleKey {
    let mut parts = path.split('/');
    let Some(first) = parts.next() else {
        return ModuleKey::new(ROOT_MODULE);
    };
    let second = parts.next();
    // True when the path has a third (or further) component.
    let has_nested = parts.next().is_some();

    match (first, second, has_nested) {
        // Single component (including a bare file named `apps`) → `_root`.
        (_, None, _) => ModuleKey::new(ROOT_MODULE),
        // Umbrella app: `apps/<name>/…` requires at least three components so
        // `apps/README.md` is not misclassified as app "README.md".
        ("apps", Some(name), true) => ModuleKey::new(format!("apps/{name}")),
        // File or dir directly under `apps/` (e.g. `apps/README.md`).
        ("apps", Some(_), false) => ModuleKey::new("apps"),
        // Any other top-level directory.
        (dir, Some(_), _) => ModuleKey::new(dir),
    }
}

/// Priority group for inventory ordering: `0 = apps/*`, `1 = _root`, `2 = other`.
pub(crate) fn module_sort_key(key: &str) -> (u8, &str) {
    if key.starts_with("apps/") {
        (0, key)
    } else if key == ROOT_MODULE {
        (1, "")
    } else {
        (2, key)
    }
}

/// Group relative file paths into modules and count files per module.
///
/// Returns modules ordered for parity with the frozen baseline:
/// `apps/*` (alpha) → `_root` → remaining keys (alpha).
///
/// # Examples
///
/// ```
/// use brigid_core::module::discover_modules;
///
/// let files = ["README.md", "src/a.rs", "src/b.rs"];
/// let inv = discover_modules(files.iter().copied());
/// assert_eq!(inv[0].key.as_str(), "_root");
/// assert_eq!(inv[0].count, 1);
/// assert_eq!(inv[1].key.as_str(), "src");
/// assert_eq!(inv[1].count, 2);
/// ```
#[must_use]
pub fn discover_modules<I, S>(files: I) -> Vec<ModuleCount>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        let key = module_key(file.as_ref());
        *counts.entry(key.as_str().to_owned()).or_insert(0) += 1;
    }
    let mut modules: Vec<ModuleCount> = counts
        .into_iter()
        .map(|(key, count)| ModuleCount {
            key: ModuleKey::new(key),
            count,
        })
        .collect();
    modules.sort_by(|a, b| module_sort_key(a.key.as_str()).cmp(&module_sort_key(b.key.as_str())));
    modules
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- module_key -------------------------------------------------------

    #[test]
    fn module_key_root_level_file_is_root() {
        assert_eq!(module_key("README.md").as_str(), ModuleKey::ROOT);
        assert_eq!(module_key(".env.example").as_str(), "_root");
        assert_eq!(module_key("pyproject.toml").as_str(), "_root");
    }

    #[test]
    fn module_key_apps_is_two_level() {
        assert_eq!(module_key("apps/alpha/lib/alpha.ex").as_str(), "apps/alpha");
        assert_eq!(module_key("apps/beta/mix.exs").as_str(), "apps/beta");
        assert_eq!(module_key("apps/gamma/lib/gamma.ex").as_str(), "apps/gamma");
    }

    #[test]
    fn module_key_file_directly_under_apps_is_apps_not_app_name() {
        // Regression: do not treat `apps/README.md` as umbrella app "README.md".
        assert_eq!(module_key("apps/README.md").as_str(), "apps");
        assert_eq!(module_key("apps/mix.exs").as_str(), "apps");
    }

    #[test]
    fn module_key_bare_apps_filename_is_root() {
        // Single path component named `apps` is a root-level file, not an app.
        assert_eq!(module_key("apps").as_str(), ModuleKey::ROOT);
    }

    #[test]
    fn module_key_top_level_directory() {
        assert_eq!(module_key("src/index.ts").as_str(), "src");
        assert_eq!(module_key("src/mylib/core.py").as_str(), "src");
        assert_eq!(module_key("config/config.exs").as_str(), "config");
    }

    // --- discover_modules: fixture parity ---------------------------------

    #[test]
    fn discover_modules_python_lib_matches_baseline() {
        let files = [
            ".env.example",
            "README.md",
            "pyproject.toml",
            "src/mylib/__init__.py",
            "src/mylib/cli.py",
            "src/mylib/core.py",
        ];
        let inv = discover_modules(files.iter().copied());
        assert_eq!(
            inv.iter()
                .map(|m| (m.key.as_str(), m.count))
                .collect::<Vec<_>>(),
            vec![("_root", 3), ("src", 3)]
        );
    }

    #[test]
    fn discover_modules_umbrella_matches_baseline() {
        let files = [
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
        let inv = discover_modules(files.iter().copied());
        assert_eq!(
            inv.iter()
                .map(|m| (m.key.as_str(), m.count))
                .collect::<Vec<_>>(),
            vec![
                ("apps/alpha", 2),
                ("apps/beta", 2),
                ("apps/gamma", 2),
                ("_root", 3),
                ("config", 1),
            ]
        );
    }

    #[test]
    fn discover_modules_js_lib_matches_baseline() {
        let files = [
            ".env.example",
            "README.md",
            "package.json",
            "src/index.ts",
            "src/utils.ts",
            "tsconfig.json",
        ];
        let inv = discover_modules(files.iter().copied());
        assert_eq!(
            inv.iter()
                .map(|m| (m.key.as_str(), m.count))
                .collect::<Vec<_>>(),
            vec![("_root", 4), ("src", 2)]
        );
    }

    #[test]
    fn discover_modules_empty_input() {
        let inv = discover_modules(std::iter::empty::<&str>());
        assert!(inv.is_empty());
    }

    #[test]
    fn module_key_display_and_as_ref() {
        let key = module_key("src/main.rs");
        assert_eq!(key.to_string(), "src");
        assert_eq!(AsRef::<str>::as_ref(&key), "src");
        let owned: String = key.clone().into();
        assert_eq!(owned, "src");
    }
}
