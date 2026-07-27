//! Plugin trait and registry for custom abstraction "kind" detectors.
//!
//! This module defines the extension point that lets users plug
//! domain-specific classification logic into the **identify** stage without
//! modifying the core pipeline. See ADR 0014 for the full architecture
//! rationale.
//!
//! # Design
//!
//! - [`KindDetector`] is an **object-safe** trait (no generics, no `Self` in
//!   return position) so it can be used as a `dyn KindDetector` trait object.
//! - [`PluginRegistry`] holds `Vec<Box<dyn KindDetector>>` and dispatches
//!   [`PluginRegistry::detect_kind`] by trying each plugin in registration
//!   order, returning the first match.
//! - [`DefaultKindDetector`] wraps the built-in file-extension heuristic so
//!   the registry always has a sensible fallback.
//!
//! Dynamic loading from shared libraries (`.so`/`.dylib`/`.dll`) is
//! **out of scope** — this module defines the trait and in-process registry
//! only. WASM/process isolation is future work (see ADR 0014 §Future
//! extension points).

use std::path::Path;

use crate::AbstractionKind;

/// Extension point for custom abstraction "kind" classification.
///
/// Implementors inspect a file's path and (optionally) its content to decide
/// whether it matches a domain-specific kind. The trait is **object-safe**:
/// no generics, no `Self` in return position, so it can be stored as
/// `Box<dyn KindDetector>` inside [`PluginRegistry`].
///
/// # Example
///
/// ```
/// use brigid_core::plugin::KindDetector;
/// use brigid_core::AbstractionKind;
///
/// struct RustModuleDetector;
///
/// impl KindDetector for RustModuleDetector {
///     fn detect_kind(&self, file_path: &str, _content: &str) -> Option<AbstractionKind> {
///         if file_path.ends_with(".rs") {
///             Some(AbstractionKind::new("rust module"))
///         } else {
///             None
///         }
///     }
///     fn name(&self) -> &str {
///         "rust-module-detector"
///     }
/// }
/// ```
pub trait KindDetector: Send + Sync {
    /// Attempt to classify `file_path` (with `content` available for
    /// content-based heuristics) into an [`AbstractionKind`].
    ///
    /// Return `None` when this detector does not match, so the registry can
    /// fall through to the next plugin (or the default).
    fn detect_kind(&self, file_path: &str, content: &str) -> Option<AbstractionKind>;

    /// Human-readable, stable name for this detector (used in diagnostics
    /// and logs). Should be unique within a registry.
    fn name(&self) -> &str;
}

/// Registry of custom [`KindDetector`] plugins.
///
/// Plugins are tried in **registration order**; the first non-`None` result
/// wins. When no registered plugin matches, callers can fall back to the
/// built-in [`DefaultKindDetector`] (see [`PluginRegistry::with_default`]).
#[derive(Default)]
pub struct PluginRegistry {
    /// Ordered list of plugin trait objects.
    detectors: Vec<Box<dyn KindDetector>>,
}

impl PluginRegistry {
    /// Create an empty registry with no plugins.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry pre-populated with the built-in
    /// [`DefaultKindDetector`] as the **last** (fallback) detector.
    #[must_use]
    pub fn with_default() -> Self {
        let mut registry = Self::new();
        registry.register_default();
        registry
    }

    /// Register a custom [`KindDetector`].
    ///
    /// The detector is appended to the end of the list, so it is tried
    /// **after** all previously registered detectors.
    pub fn register(&mut self, detector: Box<dyn KindDetector>) {
        self.detectors.push(detector);
    }

    /// Register the built-in [`DefaultKindDetector`] as the fallback.
    ///
    /// This is typically called last so custom plugins take priority over
    /// the default heuristic.
    pub fn register_default(&mut self) {
        self.register(Box::new(DefaultKindDetector));
    }

    /// Returns `true` when the registry has no registered detectors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.detectors.is_empty()
    }

    /// Number of registered detectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.detectors.len()
    }

    /// Try every registered detector in order and return the first match.
    ///
    /// Returns `None` when no detector matches (including when the registry
    /// is empty). Callers that want a guaranteed result should use
    /// [`PluginRegistry::with_default`] or handle `None` themselves.
    #[must_use]
    pub fn detect_kind(&self, file_path: &str, content: &str) -> Option<AbstractionKind> {
        for detector in &self.detectors {
            if let Some(kind) = detector.detect_kind(file_path, content) {
                return Some(kind);
            }
        }
        None
    }

    /// Return the names of all registered detectors, in registration order.
    /// Useful for diagnostics and tests.
    #[must_use]
    pub fn detector_names(&self) -> Vec<&str> {
        self.detectors.iter().map(|d| d.name()).collect()
    }
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("detector_count", &self.detectors.len())
            .field("detector_names", &self.detector_names())
            .finish()
    }
}

/// Built-in kind detector that classifies files by extension using the
/// same heuristic the identify stage has always relied on.
///
/// This wraps the file-extension-based classification so the registry has a
/// sensible fallback when no custom plugin matches. It is intentionally
/// conservative: it only returns `Some` for extensions it recognises, and
/// `None` otherwise (letting a caller decide what to do with unknown files).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultKindDetector;

impl DefaultKindDetector {
    /// Create a new default detector.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Heuristic kind classification based on file extension.
    ///
    /// This is the built-in mapping that mirrors the kinds the Python
    /// reference and the LLM prompts use (`"module"`, `"config"`,
    /// `"documentation"`, `"test"`, `"script"`, `"source"`).
    #[must_use]
    pub fn detect_by_extension(file_path: &str) -> Option<AbstractionKind> {
        let path = Path::new(file_path);
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let kind = match ext.as_str() {
            // Rust / Go / Python / TS / JS / Java / C/C++ / Ruby source.
            "rs" | "go" | "py" | "ts" | "tsx" | "js" | "jsx" | "java" | "c" | "cpp" | "cc"
            | "h" | "hpp" | "rb" => "source",
            // Markup / documentation.
            "md" | "rst" | "txt" | "adoc" => "documentation",
            // Configuration.
            "toml" | "yaml" | "yml" | "json" | "ini" | "cfg" | "conf" | "env" => "config",
            // Shell / build scripts.
            "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" => "script",
            // Lockfiles / manifests treated as config.
            "lock" => "config",
            _ => return None,
        };
        Some(AbstractionKind::new(kind))
    }
}

impl KindDetector for DefaultKindDetector {
    fn detect_kind(&self, file_path: &str, content: &str) -> Option<AbstractionKind> {
        if let Some(kind) = Self::detect_by_extension(file_path) {
            return Some(kind);
        }
        // Content-based fallback: detect a module file by `mod ` / `pub mod`
        // declarations (Rust) or `package ` (Go) when the extension is
        // unknown but the content looks like a module declaration.
        if content.contains("pub mod ") || content.contains("mod ") {
            return Some(AbstractionKind::new("module"));
        }
        None
    }

    fn name(&self) -> &str {
        "default-kind-detector"
    }
}

/// Enrich the `kind` of each abstraction in `abstractions` using the
/// registry, falling back to the existing kind when no detector matches.
///
/// For every abstraction whose [`AbstractionKind`] is empty, the registry is
/// consulted with the abstraction's first entry file (or, if there are no
/// entry files, the first file in `files` at the abstraction's first
/// `file_index`). When the registry returns `Some`, the kind is updated;
/// otherwise the original (empty) kind is preserved.
///
/// This is the integration point called by the identify stage after the LLM
/// produces its abstraction list. Abstractions that already have a non-empty
/// kind (the normal case — the LLM sets it) are left untouched, so plugins
/// only fill gaps rather than overriding LLM output.
pub fn enrich_abstraction_kinds(
    abstractions: &mut [crate::Abstraction],
    files: &[String],
    contents: &[String],
    registry: &PluginRegistry,
) {
    for abs in abstractions.iter_mut() {
        // Only fill in empty kinds — never override the LLM's classification.
        if !abs.kind.as_str().is_empty() {
            continue;
        }
        // Pick the best file path + content to classify with.
        let (path, content) = pick_classification_target(abs, files, contents);
        if let Some(kind) = registry.detect_kind(path, content) {
            abs.kind = kind;
        }
    }
}

/// Choose the file path and content to feed the registry for one abstraction.
///
/// Preference order: first `entry_files` entry, then the file at the first
/// `file_indices` entry. Content is looked up by matching the path against
/// the parallel `files`/`contents` arrays.
fn pick_classification_target<'a>(
    abs: &'a crate::Abstraction,
    files: &'a [String],
    contents: &'a [String],
) -> (&'a str, &'a str) {
    // Try entry_files first.
    if let Some(entry) = abs.entry_files.first() {
        let content = lookup_content(entry, files, contents);
        return (entry.as_str(), content);
    }
    // Fall back to the first file_index.
    if let Some(&idx) = abs.file_indices.first() {
        if let Some(path) = files.get(idx) {
            let content = contents.get(idx).map(String::as_str).unwrap_or("");
            return (path.as_str(), content);
        }
    }
    ("", "")
}

/// Look up the content for `path` in the parallel `files`/`contents` arrays.
fn lookup_content<'a>(path: &str, files: &'a [String], contents: &'a [String]) -> &'a str {
    for (i, f) in files.iter().enumerate() {
        if f == path {
            return contents.get(i).map(String::as_str).unwrap_or("");
        }
    }
    ""
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Abstraction, Tier};

    // --- A custom KindDetector impl used across tests ---

    /// A test detector that classifies `.rs` files as "rust module".
    struct RustModuleDetector;

    impl KindDetector for RustModuleDetector {
        fn detect_kind(&self, file_path: &str, _content: &str) -> Option<AbstractionKind> {
            if file_path.ends_with(".rs") {
                Some(AbstractionKind::new("rust module"))
            } else {
                None
            }
        }
        fn name(&self) -> &str {
            "rust-module-detector"
        }
    }

    /// A test detector that classifies `.py` files as "python package".
    struct PythonPackageDetector;

    impl KindDetector for PythonPackageDetector {
        fn detect_kind(&self, file_path: &str, _content: &str) -> Option<AbstractionKind> {
            if file_path.ends_with(".py") {
                Some(AbstractionKind::new("python package"))
            } else {
                None
            }
        }
        fn name(&self) -> &str {
            "python-package-detector"
        }
    }

    /// A detector that never matches (always returns None).
    struct NeverDetector;

    impl KindDetector for NeverDetector {
        fn detect_kind(&self, _file_path: &str, _content: &str) -> Option<AbstractionKind> {
            None
        }
        fn name(&self) -> &str {
            "never-detector"
        }
    }

    // -----------------------------------------------------------------------
    // KindDetector trait
    // -----------------------------------------------------------------------

    #[test]
    fn custom_detector_classifies_rust_files() {
        let detector = RustModuleDetector;
        assert_eq!(
            detector.detect_kind("src/lib.rs", "pub mod foo;"),
            Some(AbstractionKind::new("rust module"))
        );
        assert_eq!(detector.name(), "rust-module-detector");
    }

    #[test]
    fn custom_detector_returns_none_for_non_matching() {
        let detector = RustModuleDetector;
        assert_eq!(detector.detect_kind("src/main.py", "print('hi')"), None);
    }

    // -----------------------------------------------------------------------
    // PluginRegistry dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn registry_dispatches_to_first_matching_plugin() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));
        registry.register(Box::new(PythonPackageDetector));
        // .rs matches the first plugin.
        assert_eq!(
            registry.detect_kind("src/lib.rs", ""),
            Some(AbstractionKind::new("rust module"))
        );
        // .py matches the second plugin.
        assert_eq!(
            registry.detect_kind("src/app.py", ""),
            Some(AbstractionKind::new("python package"))
        );
    }

    #[test]
    fn registry_returns_first_match_in_registration_order() {
        // Two detectors that both match .rs — the first registered wins.
        struct FirstDetector;
        impl KindDetector for FirstDetector {
            fn detect_kind(&self, fp: &str, _: &str) -> Option<AbstractionKind> {
                if fp.ends_with(".rs") {
                    Some(AbstractionKind::new("first"))
                } else {
                    None
                }
            }
            fn name(&self) -> &str {
                "first"
            }
        }
        struct SecondDetector;
        impl KindDetector for SecondDetector {
            fn detect_kind(&self, fp: &str, _: &str) -> Option<AbstractionKind> {
                if fp.ends_with(".rs") {
                    Some(AbstractionKind::new("second"))
                } else {
                    None
                }
            }
            fn name(&self) -> &str {
                "second"
            }
        }
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(FirstDetector));
        registry.register(Box::new(SecondDetector));
        assert_eq!(
            registry.detect_kind("src/lib.rs", ""),
            Some(AbstractionKind::new("first"))
        );
    }

    #[test]
    fn registry_falls_through_when_plugin_returns_none() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(NeverDetector));
        registry.register(Box::new(RustModuleDetector));
        // NeverDetector returns None, so RustModuleDetector is tried.
        assert_eq!(
            registry.detect_kind("src/lib.rs", ""),
            Some(AbstractionKind::new("rust module"))
        );
    }

    // -----------------------------------------------------------------------
    // No-plugin fallback
    // -----------------------------------------------------------------------

    #[test]
    fn empty_registry_returns_none() {
        let registry = PluginRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.detect_kind("src/lib.rs", "pub mod foo;"), None);
    }

    #[test]
    fn registry_with_default_falls_back_to_default_detector() {
        let registry = PluginRegistry::with_default();
        // No custom plugin — default detector classifies by extension.
        assert_eq!(
            registry.detect_kind("src/lib.rs", ""),
            Some(AbstractionKind::new("source"))
        );
        assert_eq!(
            registry.detect_kind("README.md", ""),
            Some(AbstractionKind::new("documentation"))
        );
    }

    #[test]
    fn custom_plugin_takes_priority_over_default() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));
        registry.register_default();
        // Custom plugin matches .rs first.
        assert_eq!(
            registry.detect_kind("src/lib.rs", ""),
            Some(AbstractionKind::new("rust module"))
        );
        // Default handles .md since custom plugin returns None.
        assert_eq!(
            registry.detect_kind("README.md", ""),
            Some(AbstractionKind::new("documentation"))
        );
    }

    #[test]
    fn registry_len_and_names() {
        let mut registry = PluginRegistry::new();
        assert_eq!(registry.len(), 0);
        registry.register(Box::new(RustModuleDetector));
        registry.register(Box::new(PythonPackageDetector));
        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.detector_names(),
            vec!["rust-module-detector", "python-package-detector"]
        );
    }

    #[test]
    fn registry_debug_shows_count_and_names() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));
        let s = format!("{registry:?}");
        assert!(s.contains("PluginRegistry"));
        assert!(s.contains("rust-module-detector"));
    }

    // -----------------------------------------------------------------------
    // DefaultKindDetector
    // -----------------------------------------------------------------------

    #[test]
    fn default_detector_classifies_source_extensions() {
        assert_eq!(
            DefaultKindDetector::detect_by_extension("lib.rs"),
            Some(AbstractionKind::new("source"))
        );
        assert_eq!(
            DefaultKindDetector::detect_by_extension("app.py"),
            Some(AbstractionKind::new("source"))
        );
        assert_eq!(
            DefaultKindDetector::detect_by_extension("main.go"),
            Some(AbstractionKind::new("source"))
        );
        assert_eq!(
            DefaultKindDetector::detect_by_extension("index.ts"),
            Some(AbstractionKind::new("source"))
        );
    }

    #[test]
    fn default_detector_classifies_documentation_extensions() {
        assert_eq!(
            DefaultKindDetector::detect_by_extension("README.md"),
            Some(AbstractionKind::new("documentation"))
        );
        assert_eq!(
            DefaultKindDetector::detect_by_extension("guide.rst"),
            Some(AbstractionKind::new("documentation"))
        );
    }

    #[test]
    fn default_detector_classifies_config_extensions() {
        assert_eq!(
            DefaultKindDetector::detect_by_extension("brigid.toml"),
            Some(AbstractionKind::new("config"))
        );
        assert_eq!(
            DefaultKindDetector::detect_by_extension("config.yaml"),
            Some(AbstractionKind::new("config"))
        );
        assert_eq!(
            DefaultKindDetector::detect_by_extension("Cargo.lock"),
            Some(AbstractionKind::new("config"))
        );
    }

    #[test]
    fn default_detector_classifies_script_extensions() {
        assert_eq!(
            DefaultKindDetector::detect_by_extension("build.sh"),
            Some(AbstractionKind::new("script"))
        );
    }

    #[test]
    fn default_detector_returns_none_for_unknown_extension() {
        assert_eq!(DefaultKindDetector::detect_by_extension("data.bin"), None);
        assert_eq!(DefaultKindDetector::detect_by_extension("noext"), None);
    }

    #[test]
    fn default_detector_content_fallback_detects_module() {
        let detector = DefaultKindDetector::new();
        // Unknown extension but content has `pub mod`.
        assert_eq!(
            detector.detect_kind("mystery.xyz", "pub mod foo;\n"),
            Some(AbstractionKind::new("module"))
        );
        // Unknown extension and no module declaration -> None.
        assert_eq!(detector.detect_kind("mystery.xyz", "hello world"), None);
    }

    #[test]
    fn default_detector_name_is_stable() {
        let detector = DefaultKindDetector::new();
        assert_eq!(detector.name(), "default-kind-detector");
    }

    // -----------------------------------------------------------------------
    // enrich_abstraction_kinds
    // -----------------------------------------------------------------------

    #[test]
    fn enrich_fills_empty_kinds_via_registry() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));

        let mut abs = Abstraction::new("A", "desc", Tier::S, "");
        abs.file_indices = vec![0]; // index 0 -> src/lib.rs
        let mut abstractions = vec![abs];
        let files = vec!["src/lib.rs".to_string()];
        let contents = vec!["pub mod foo;".to_string()];

        enrich_abstraction_kinds(&mut abstractions, &files, &contents, &registry);

        assert_eq!(abstractions[0].kind, AbstractionKind::new("rust module"));
    }

    #[test]
    fn enrich_does_not_override_non_empty_kinds() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));

        let mut abstractions = vec![
            // Already has a kind — must NOT be overridden.
            Abstraction::new("A", "desc", Tier::S, "class"),
        ];
        let files = vec!["src/lib.rs".to_string()];
        let contents = vec!["pub mod foo;".to_string()];

        enrich_abstraction_kinds(&mut abstractions, &files, &contents, &registry);

        assert_eq!(abstractions[0].kind, AbstractionKind::new("class"));
    }

    #[test]
    fn enrich_preserves_kind_when_registry_returns_none() {
        let registry = PluginRegistry::new(); // empty registry

        let mut abs = Abstraction::new("A", "desc", Tier::S, "");
        abs.file_indices = vec![0];
        let mut abstractions = vec![abs];
        let files = vec!["src/lib.rs".to_string()];
        let contents = vec!["pub mod foo;".to_string()];

        enrich_abstraction_kinds(&mut abstractions, &files, &contents, &registry);

        assert_eq!(abstractions[0].kind.as_str(), "");
    }

    #[test]
    fn enrich_uses_entry_files_when_available() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));

        let mut abs = Abstraction::new("A", "desc", Tier::S, "");
        abs.entry_files = vec!["src/lib.rs".to_string()];
        // file_indices point to a .py file, but entry_files should win.
        abs.file_indices = vec![0];
        let mut abstractions = vec![abs];
        let files = vec!["src/app.py".to_string()];
        let contents = vec!["print('hi')".to_string()];

        enrich_abstraction_kinds(&mut abstractions, &files, &contents, &registry);

        assert_eq!(abstractions[0].kind, AbstractionKind::new("rust module"));
    }

    #[test]
    fn enrich_falls_back_to_file_indices_when_no_entry_files() {
        let mut registry = PluginRegistry::new();
        registry.register(Box::new(RustModuleDetector));

        let mut abs = Abstraction::new("A", "desc", Tier::S, "");
        abs.file_indices = vec![1]; // index 1 -> src/lib.rs
        let mut abstractions = vec![abs];
        let files = vec!["src/app.py".to_string(), "src/lib.rs".to_string()];
        let contents = vec!["print('hi')".to_string(), "pub mod foo;".to_string()];

        enrich_abstraction_kinds(&mut abstractions, &files, &contents, &registry);

        assert_eq!(abstractions[0].kind, AbstractionKind::new("rust module"));
    }

    // -----------------------------------------------------------------------
    // Object safety smoke test
    // -----------------------------------------------------------------------

    #[test]
    fn kind_detector_can_be_used_as_dyn_trait_object() {
        let detector: Box<dyn KindDetector> = Box::new(RustModuleDetector);
        assert_eq!(
            detector.detect_kind("src/lib.rs", ""),
            Some(AbstractionKind::new("rust module"))
        );
        assert_eq!(detector.name(), "rust-module-detector");
    }
}
