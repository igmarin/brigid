//! Run configuration and layered loading.
//!
//! Precedence (highest wins): **CLI overrides** → **project file**
//! (`brigid.toml` / `.brigid.yaml`) → **environment** (`BRIGID_*`) → **defaults**.
//!
//! Blank environment values are ignored so exporting empty vars never
//! accidentally clears defaults (move-to-rust config rules).
//!
//! File and env loaders are pure with respect to the process: callers pass
//! strings / key-value maps. Filesystem discovery stays in CLI/pipeline.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Errors while parsing or serializing configuration layers.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// TOML body is invalid.
    #[error("invalid brigid.toml: {0}")]
    Toml(String),
    /// YAML body is invalid.
    #[error("invalid .brigid.yaml: {0}")]
    Yaml(String),
    /// Canonical JSON serialization failed.
    #[error("config JSON serialization failed: {0}")]
    Json(String),
    /// Environment variable value is present but not parseable.
    #[error("invalid value for {key}: {value:?}")]
    InvalidEnvValue {
        /// Environment variable name.
        key: String,
        /// Raw value that failed to parse.
        value: String,
    },
    /// A secret-bearing field was found in a config file (secrets must come from env only).
    #[error(
        "secret field {field:?} is not allowed in config files; use the {env_var} environment variable instead"
    )]
    SecretFieldRejected {
        /// The rejected field name.
        field: String,
        /// Suggested environment variable for this secret.
        env_var: String,
    },
    /// An allowed-host entry is not a valid hostname (wildcards, paths, empty, etc.).
    #[error("invalid allowed host {host:?}: {reason}")]
    InvalidAllowedHost {
        /// The rejected host string.
        host: String,
        /// Why it was rejected.
        reason: String,
    },
}

/// Default max LLM calls before the budget tracker fails closed.
pub const DEFAULT_MAX_LLM_CALLS: u32 = 200;

/// Default rough chars-per-token heuristic (matches budget defaults).
pub const DEFAULT_CONFIG_CHARS_PER_TOKEN: usize = 4;

/// Full run configuration used by dry-run, generate, and checkpoint hashing.
///
/// Optional fields use `None` to mean “unset at this layer” during merge;
/// after [`resolve_config`] / [`RunConfig::default`] every operational field
/// is populated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    /// Repository root or source path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// Output directory for generated tutorials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    /// Optional monorepo app/module scope keys (`apps/alpha`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// Tutorial language / chrome locale (e.g. `en`, `es`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Hard ceiling on LLM calls for a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_llm_calls: Option<u32>,
    /// LLM provider id (structure only in M2; no live clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id for the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Disk cache directory for LLM responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Checkpoint directory (when set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_dir: Option<PathBuf>,
    /// Soft per-batch character budget override for dry-run packing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_char_budget: Option<usize>,
    /// Chars-per-token heuristic for token estimates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars_per_token: Option<usize>,
    /// Disk cache size limit in megabytes (default 100 when resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_size_limit_mb: Option<usize>,
    /// Maximum concurrent chapter writes (default 4 when resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// Maximum number of abstractions to identify (default 10 when resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_abstractions: Option<usize>,
    /// Diagram richness level: `minimal`, `standard`, or `rich`
    /// (default `standard` when resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagram_level: Option<String>,
    /// Additional LLM provider hosts allowed to receive the `Authorization`
    /// header, beyond the built-in allowlist. Sourced from
    /// `BRIGID_ALLOWED_HOSTS` (comma-separated) and `[[allowed_hosts]]` config
    /// file sections. Layers accumulate (env + file) and are deduplicated.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_allowed_hosts"
    )]
    pub allowed_hosts: Option<Vec<String>>,
    /// Git ref (tag, commit, or branch) for incremental git-diff crawl.
    ///
    /// When set, the file inventory is filtered to only files that changed
    /// since this ref (via `brigid_crawl::git_diff::changed_files_since`).
    /// Sourced from `--since` CLI flag, `since = "…"` in `brigid.toml`, or
    /// the `BRIGID_SINCE` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Directories to load custom [`crate::plugin::KindDetector`] plugins
    /// from (issue #228 / ADR 0014).
    ///
    /// Sourced from `[plugins] dirs = […]` in `brigid.toml` /
    /// `.brigid.yaml`, or the `BRIGID_PLUGIN_DIRS` environment variable
    /// (colon-separated). Dynamic loading from shared libraries is
    /// **out of scope** for this field — it is reserved for a future
    /// milestone. Today it is parsed and stored so config round-trips
    /// are stable, but the identify stage uses an in-process
    /// [`crate::plugin::PluginRegistry`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_dirs: Option<Vec<PathBuf>>,
    /// Tutorial writing style: `book` (long-form, multi-section chapters) or
    /// `blog-post` (shorter, conversational chapters). Default `blog-post`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tutorial_style: Option<TutorialStyle>,
    /// Optional graph provider configuration for structural ground truth
    /// (ADR 0016). When absent, [`crate::NoneProvider`] is used — `brigid`
    /// works LLM-only with zero configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_provider: Option<GraphProviderConfig>,
}

/// Configuration for an external graph provider (ADR 0016).
///
/// Parsed from the `[graph_provider]` section in `brigid.toml` /
/// `.brigid.yaml`, or the `BRIGID_GRAPH_PROVIDER` environment variable
/// (which sets only the type). When absent, [`crate::NoneProvider`] is
/// used — `brigid` works exactly as today (LLM-only).
///
/// # Examples
///
/// ## codegraph (SQLite index)
///
/// ```toml
/// [graph_provider]
/// type = "codegraph"
/// index_path = ".codegraph/graph.db"
/// ```
///
/// ## Graphify (graph.json output file)
///
/// ```toml
/// [graph_provider]
/// type = "graphify"
/// graph_path = "graphify-out/graph.json"
/// ```
///
/// ## Composed (codegraph + Graphify)
///
/// ```toml
/// [graph_provider]
/// type = "composed"
/// providers = ["codegraph:.codegraph/graph.db", "graphify:graphify-out/graph.json"]
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphProviderConfig {
    /// Provider type: `"codegraph"`, `"graphify"`, `"composed"`, or `"none"`.
    ///
    /// Mapped from the `type` key in the config file. The
    /// `BRIGID_GRAPH_PROVIDER` env var sets only this field.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub provider_type: Option<String>,
    /// Path to the codegraph SQLite index file (`.codegraph/graph.db`).
    ///
    /// Used when `provider_type` is `"codegraph"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_path: Option<PathBuf>,
    /// Path to the Graphify `graph.json` output file.
    ///
    /// Used when `provider_type` is `"graphify"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_path: Option<PathBuf>,
    /// List of provider specs for composed mode (`"type:path"` strings).
    ///
    /// Used when `provider_type` is `"composed"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
}

/// Tutorial writing style.
///
/// - `Book`: long-form chapters with 10 sections, 2+ mermaid diagrams, formal tone.
/// - `BlogPost`: shorter chapters with 5 sections, 0-1 diagrams, conversational tone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TutorialStyle {
    /// Long-form, book-like chapters with full structure.
    Book,
    /// Shorter, blog-post-style chapters.
    #[default]
    BlogPost,
}

impl TutorialStyle {
    /// Returns the string identifier used in CLI flags and config files.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Book => "book",
            Self::BlogPost => "blog-post",
        }
    }

    /// Parses a style string (case-insensitive). Returns `None` if unrecognized.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "book" => Some(Self::Book),
            "blog-post" | "blogpost" | "blog" => Some(Self::BlogPost),
            _ => None,
        }
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            root: Some(PathBuf::from(".")),
            output: Some(PathBuf::from("output")),
            apps: Some(Vec::new()),
            language: Some("en".into()),
            max_llm_calls: Some(DEFAULT_MAX_LLM_CALLS),
            provider: None,
            model: None,
            cache_dir: None,
            checkpoint_dir: None,
            batch_char_budget: None,
            chars_per_token: Some(DEFAULT_CONFIG_CHARS_PER_TOKEN),
            cache_size_limit_mb: None,
            concurrency: None,
            max_abstractions: None,
            diagram_level: None,
            allowed_hosts: None,
            since: None,
            plugin_dirs: None,
            tutorial_style: Some(TutorialStyle::BlogPost),
            graph_provider: None,
        }
    }
}

impl RunConfig {
    /// Empty layer (all fields unset) for building overlays.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            root: None,
            output: None,
            apps: None,
            language: None,
            max_llm_calls: None,
            provider: None,
            model: None,
            cache_dir: None,
            checkpoint_dir: None,
            batch_char_budget: None,
            chars_per_token: None,
            cache_size_limit_mb: None,
            concurrency: None,
            max_abstractions: None,
            diagram_level: None,
            allowed_hosts: None,
            since: None,
            plugin_dirs: None,
            tutorial_style: None,
            graph_provider: None,
        }
    }

    /// Merge `overlay` on top of `self`: only `Some` fields in `overlay` win.
    #[must_use]
    pub fn merge_layer(&self, overlay: &Self) -> Self {
        Self {
            root: overlay.root.clone().or_else(|| self.root.clone()),
            output: overlay.output.clone().or_else(|| self.output.clone()),
            apps: overlay.apps.clone().or_else(|| self.apps.clone()),
            language: overlay.language.clone().or_else(|| self.language.clone()),
            max_llm_calls: overlay.max_llm_calls.or(self.max_llm_calls),
            provider: overlay.provider.clone().or_else(|| self.provider.clone()),
            model: overlay.model.clone().or_else(|| self.model.clone()),
            cache_dir: overlay.cache_dir.clone().or_else(|| self.cache_dir.clone()),
            checkpoint_dir: overlay
                .checkpoint_dir
                .clone()
                .or_else(|| self.checkpoint_dir.clone()),
            batch_char_budget: overlay.batch_char_budget.or(self.batch_char_budget),
            chars_per_token: overlay.chars_per_token.or(self.chars_per_token),
            cache_size_limit_mb: overlay.cache_size_limit_mb.or(self.cache_size_limit_mb),
            concurrency: overlay.concurrency.or(self.concurrency),
            max_abstractions: overlay.max_abstractions.or(self.max_abstractions),
            diagram_level: overlay
                .diagram_level
                .clone()
                .or_else(|| self.diagram_level.clone()),
            allowed_hosts: merge_host_layers(&self.allowed_hosts, &overlay.allowed_hosts),
            since: overlay.since.clone().or_else(|| self.since.clone()),
            plugin_dirs: overlay
                .plugin_dirs
                .clone()
                .or_else(|| self.plugin_dirs.clone()),
            tutorial_style: overlay.tutorial_style.or(self.tutorial_style),
            graph_provider: overlay
                .graph_provider
                .clone()
                .or_else(|| self.graph_provider.clone()),
        }
    }

    /// Copy suitable for checkpoint display: drop secret-bearing fields if any
    /// are added later. Today this is a clone with provider/model kept (not secrets).
    #[must_use]
    pub fn redacted_for_checkpoint(&self) -> Self {
        self.clone()
    }
}

/// Resolve full config by merging layers in order:
/// defaults, then `env_layer`, then `file_layer`, then `cli_layer`.
///
/// **CLI** overrides file; **file** overrides env; **env** overrides defaults.
#[must_use]
pub fn resolve_config(
    env_layer: &RunConfig,
    file_layer: &RunConfig,
    cli_layer: &RunConfig,
) -> RunConfig {
    RunConfig::default()
        .merge_layer(env_layer)
        .merge_layer(file_layer)
        .merge_layer(cli_layer)
}

/// Parse a TOML document into a config layer (`brigid.toml` body).
///
/// The raw parsed value is scanned for secret-bearing field names *before*
/// deserializing into [`RunConfig`], so unknown secret-like keys are also
/// rejected (defense-in-depth — see issue #73 and move-to-rust §4.3/§8.1).
///
/// # Errors
///
/// Returns [`ConfigError::Toml`] when TOML is invalid or types do not match,
/// or [`ConfigError::SecretFieldRejected`] when a secret-bearing key is found.
pub fn parse_toml_config(text: &str) -> Result<RunConfig, ConfigError> {
    let value: toml::Value = toml::from_str(text).map_err(|e| ConfigError::Toml(e.to_string()))?;
    let mut json_value =
        serde_json::to_value(&value).map_err(|e| ConfigError::Toml(e.to_string()))?;
    check_for_secret_fields(&json_value)?;
    lift_plugins_dirs(&mut json_value);
    let cfg: RunConfig =
        serde_json::from_value(json_value).map_err(|e| ConfigError::Toml(e.to_string()))?;
    validate_config_hosts(&cfg)?;
    Ok(cfg)
}

/// Parse a YAML document into a config layer (`.brigid.yaml` body).
///
/// The raw parsed value is scanned for secret-bearing field names *before*
/// deserializing into [`RunConfig`] (defense-in-depth — see issue #73).
///
/// # Errors
///
/// Returns [`ConfigError::Yaml`] when YAML is invalid or types do not match,
/// or [`ConfigError::SecretFieldRejected`] when a secret-bearing key is found.
pub fn parse_yaml_config(text: &str) -> Result<RunConfig, ConfigError> {
    let mut value: serde_json::Value =
        serde_yaml_ng::from_str(text).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    check_for_secret_fields(&value)?;
    lift_plugins_dirs(&mut value);
    let cfg: RunConfig =
        serde_json::from_value(value).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    validate_config_hosts(&cfg)?;
    Ok(cfg)
}

/// Load a config layer from environment-style key/value pairs.
///
/// Recognized keys (case-sensitive):
/// - `BRIGID_ROOT`, `BRIGID_OUTPUT`, `BRIGID_APPS` (comma-separated),
/// - `BRIGID_LANGUAGE`, `BRIGID_MAX_LLM_CALLS`, `BRIGID_PROVIDER`, `BRIGID_MODEL`,
/// - `BRIGID_CACHE_DIR`, `BRIGID_CHECKPOINT_DIR`,
/// - `BRIGID_BATCH_CHAR_BUDGET`, `BRIGID_CHARS_PER_TOKEN`,
/// - `BRIGID_CACHE_SIZE_LIMIT_MB`, `BRIGID_CONCURRENCY`,
/// - `BRIGID_MAX_ABSTRACTIONS`, `BRIGID_DIAGRAM_LEVEL`,
/// - `BRIGID_SINCE` (git ref for incremental crawl),
/// - `BRIGID_GRAPH_PROVIDER` (graph provider type: codegraph, graphify, composed),
///
/// **Blank values are ignored** (treated as unset). Non-blank values that fail
/// numeric parse return [`ConfigError::InvalidEnvValue`].
///
/// # Errors
///
/// Returns [`ConfigError::InvalidEnvValue`] when a numeric env var is set but
/// not a valid integer.
pub fn config_from_env_map(vars: &BTreeMap<String, String>) -> Result<RunConfig, ConfigError> {
    let mut cfg = RunConfig::empty();
    if let Some(v) = nonblank(vars.get("BRIGID_ROOT")) {
        cfg.root = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("BRIGID_OUTPUT")) {
        cfg.output = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("BRIGID_APPS")) {
        let apps: Vec<String> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        cfg.apps = Some(apps);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_LANGUAGE")) {
        cfg.language = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("BRIGID_MAX_LLM_CALLS")) {
        cfg.max_llm_calls = Some(parse_env_u32("BRIGID_MAX_LLM_CALLS", v)?);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_PROVIDER")) {
        cfg.provider = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("BRIGID_MODEL")) {
        cfg.model = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("BRIGID_CACHE_DIR")) {
        cfg.cache_dir = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("BRIGID_CHECKPOINT_DIR")) {
        cfg.checkpoint_dir = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("BRIGID_BATCH_CHAR_BUDGET")) {
        cfg.batch_char_budget = Some(parse_env_usize("BRIGID_BATCH_CHAR_BUDGET", v)?);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_CHARS_PER_TOKEN")) {
        cfg.chars_per_token = Some(parse_env_usize("BRIGID_CHARS_PER_TOKEN", v)?);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_CACHE_SIZE_LIMIT_MB")) {
        cfg.cache_size_limit_mb = Some(parse_env_usize("BRIGID_CACHE_SIZE_LIMIT_MB", v)?);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_CONCURRENCY")) {
        cfg.concurrency = Some(parse_env_usize("BRIGID_CONCURRENCY", v)?);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_MAX_ABSTRACTIONS")) {
        cfg.max_abstractions = Some(parse_env_usize("BRIGID_MAX_ABSTRACTIONS", v)?);
    }
    if let Some(v) = nonblank(vars.get("BRIGID_DIAGRAM_LEVEL")) {
        cfg.diagram_level = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("BRIGID_ALLOWED_HOSTS")) {
        let hosts: Vec<String> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        if hosts.is_empty() {
            cfg.allowed_hosts = None;
        } else {
            for h in &hosts {
                validate_hostname(h)?;
            }
            cfg.allowed_hosts = Some(hosts);
        }
    }
    if let Some(v) = nonblank(vars.get("BRIGID_SINCE")) {
        cfg.since = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("BRIGID_PLUGIN_DIRS")) {
        let dirs: Vec<PathBuf> = v
            .split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if dirs.is_empty() {
            cfg.plugin_dirs = None;
        } else {
            cfg.plugin_dirs = Some(dirs);
        }
    }
    if let Some(v) = nonblank(vars.get("BRIGID_GRAPH_PROVIDER")) {
        cfg.graph_provider = Some(GraphProviderConfig {
            provider_type: Some(v.to_owned()),
            index_path: None,
            graph_path: None,
            providers: None,
        });
    }
    Ok(cfg)
}

fn parse_env_u32(key: &str, value: &str) -> Result<u32, ConfigError> {
    value
        .parse::<u32>()
        .map_err(|_| ConfigError::InvalidEnvValue {
            key: key.to_owned(),
            value: value.to_owned(),
        })
}

fn parse_env_usize(key: &str, value: &str) -> Result<usize, ConfigError> {
    value
        .parse::<usize>()
        .map_err(|_| ConfigError::InvalidEnvValue {
            key: key.to_owned(),
            value: value.to_owned(),
        })
}

/// Validate a single hostname for the LLM host allowlist.
///
/// A valid host is non-empty, contains no wildcards (`*`), no path separators
/// (`/`), no whitespace, no port (`:`), and only ASCII letters, digits, dots,
/// and hyphens. This prevents injection of path or glob fragments into the
/// allowlist that decides where the `Authorization` header may be sent.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidAllowedHost`] when the host is not acceptable.
pub fn validate_hostname(host: &str) -> Result<(), ConfigError> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidAllowedHost {
            host: host.to_owned(),
            reason: "host is empty".to_owned(),
        });
    }
    if trimmed.contains('*') {
        return Err(ConfigError::InvalidAllowedHost {
            host: host.to_owned(),
            reason: "wildcards are not allowed".to_owned(),
        });
    }
    if trimmed.contains('/') {
        return Err(ConfigError::InvalidAllowedHost {
            host: host.to_owned(),
            reason: "path separators are not allowed".to_owned(),
        });
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(ConfigError::InvalidAllowedHost {
            host: host.to_owned(),
            reason: "whitespace is not allowed".to_owned(),
        });
    }
    if trimmed.contains(':') {
        return Err(ConfigError::InvalidAllowedHost {
            host: host.to_owned(),
            reason: "port separators are not allowed; specify the host only".to_owned(),
        });
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(ConfigError::InvalidAllowedHost {
            host: host.to_owned(),
            reason: "only ASCII letters, digits, dots, and hyphens are allowed".to_owned(),
        });
    }
    Ok(())
}

/// Validate every host in a parsed config layer.
fn validate_config_hosts(cfg: &RunConfig) -> Result<(), ConfigError> {
    if let Some(hosts) = &cfg.allowed_hosts {
        for h in hosts {
            validate_hostname(h)?;
        }
    }
    Ok(())
}

/// A single issue found by [`validate_config_for_check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
    /// Human-readable severity: `"error"` or `"warning"`.
    pub severity: &'static str,
    /// A clear, actionable description of the issue.
    pub message: String,
}

/// Validate a parsed [`RunConfig`] for `brigid init --check`.
///
/// Returns a list of [`ConfigIssue`]s. An empty list means the config is
/// valid. Issues include:
///
/// - **error**: `max_llm_calls` is zero (would block every run).
/// - **error**: `concurrency` is zero (would deadlock chapter writes).
/// - **error**: `max_abstractions` is zero (no chapters would be generated).
/// - **error**: `diagram_level` is not one of `minimal`, `standard`, `rich`.
/// - **error**: `cache_size_limit_mb` is zero.
/// - **warning**: `language` is empty.
/// - **warning**: `provider` is set but `model` is not (or vice-versa).
#[must_use]
pub fn validate_config_for_check(cfg: &RunConfig) -> Vec<ConfigIssue> {
    let mut issues = Vec::new();

    if let Some(0) = cfg.max_llm_calls {
        issues.push(ConfigIssue {
            severity: "error",
            message: "max_llm_calls is 0 — every run would immediately exhaust the budget."
                .to_owned(),
        });
    }
    if let Some(0) = cfg.concurrency {
        issues.push(ConfigIssue {
            severity: "error",
            message: "concurrency is 0 — chapter writes would deadlock. Use a positive integer."
                .to_owned(),
        });
    }
    if let Some(0) = cfg.max_abstractions {
        issues.push(ConfigIssue {
            severity: "error",
            message: "max_abstractions is 0 — no chapters would be generated.".to_owned(),
        });
    }
    if let Some(0) = cfg.cache_size_limit_mb {
        issues.push(ConfigIssue {
            severity: "error",
            message: "cache_size_limit_mb is 0 — the cache would be unusable.".to_owned(),
        });
    }
    if let Some(level) = &cfg.diagram_level {
        let lower = level.to_ascii_lowercase();
        if lower != "minimal" && lower != "standard" && lower != "rich" {
            issues.push(ConfigIssue {
                severity: "error",
                message: format!(
                    "diagram_level is {level:?} — expected one of: minimal, standard, rich"
                ),
            });
        }
    }
    if let Some(lang) = &cfg.language {
        if lang.trim().is_empty() {
            issues.push(ConfigIssue {
                severity: "warning",
                message: "language is empty — the default 'en' will be used.".to_owned(),
            });
        }
    }
    if cfg.provider.is_some() && cfg.model.is_none() {
        issues.push(ConfigIssue {
            severity: "warning",
            message: "provider is set but model is not — consider specifying a model id."
                .to_owned(),
        });
    }
    if cfg.model.is_some() && cfg.provider.is_none() {
        issues.push(ConfigIssue {
            severity: "warning",
            message: "model is set but provider is not — consider specifying a provider id."
                .to_owned(),
        });
    }

    issues
}

/// Combine two optional host layers (self + overlay), deduplicating
/// case-insensitively while preserving first-seen order. Used by
/// [`RunConfig::merge_layer`] so env and file hosts accumulate rather than
/// one layer shadowing the other.
fn merge_host_layers(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> Option<Vec<String>> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(dedup_lowercased(x)),
        (Some(x), Some(y)) => {
            let mut out = dedup_lowercased(x);
            for h in y {
                let lower = h.to_ascii_lowercase();
                if !out.iter().any(|e| e == &lower) {
                    out.push(lower);
                }
            }
            Some(out)
        }
    }
}

/// Deduplicate a host list case-insensitively, preserving first-seen order
/// and lowercasing every entry.
fn dedup_lowercased(hosts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(hosts.len());
    for h in hosts {
        let lower = h.to_ascii_lowercase();
        if !out.iter().any(|e| e == &lower) {
            out.push(lower);
        }
    }
    out
}

/// Merge the built-in default hosts with env-layer and file-layer hosts,
/// deduplicating case-insensitively and preserving first-seen order
/// (defaults first, then env, then file).
#[must_use]
pub fn merge_allowed_hosts(default: &[&str], env: &[String], file: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(default.len() + env.len() + file.len());
    for h in default {
        let lower = h.to_ascii_lowercase();
        if !out.iter().any(|e| e == &lower) {
            out.push(lower);
        }
    }
    for h in env.iter().chain(file.iter()) {
        let lower = h.to_ascii_lowercase();
        if !out.iter().any(|e| e == &lower) {
            out.push(lower);
        }
    }
    out
}

/// Build a security-awareness warning message for stderr when custom
/// (non-default) hosts are added to the LLM allowlist. Returns `None` when
/// no custom hosts are present.
#[must_use]
pub fn custom_host_warning(hosts: &[String]) -> Option<String> {
    if hosts.is_empty() {
        return None;
    }
    let list = hosts.join(", ");
    Some(format!(
        "warning: custom LLM host allowlist entries added: [{list}]. \
         The Authorization header may be sent to these hosts; \
         only allow providers you trust."
    ))
}

/// Custom deserializer for `RunConfig.allowed_hosts` that accepts either an
/// array of strings (`["a.com", "b.com"]`) or an array of tables
/// (`[[allowed_hosts]] host = "a.com"` → `[{host: "a.com"}]`).
fn deserialize_allowed_hosts<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<Vec<serde_json::Value>> = Option::deserialize(deserializer)?;
    let Some(arr) = opt else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let host = match v {
            serde_json::Value::String(s) => s,
            serde_json::Value::Object(map) => {
                let Some(h) = map.get("host").and_then(|x| x.as_str()) else {
                    return Err(serde::de::Error::custom(
                        "allowed_hosts entry must have a string 'host' field",
                    ));
                };
                h.to_string()
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "allowed_hosts entry must be a string or object with 'host', got {other}"
                )));
            }
        };
        out.push(host);
    }
    Ok(Some(out))
}

/// Canonical JSON for hashing (sorted keys, no insignificant whitespace).
///
/// Used by checkpoint `config_hash` (ADR 0001). Serializes the full config
/// including optional fields that are set.
///
/// # Errors
///
/// Returns an error if serialization fails (should not happen for `RunConfig`).
pub fn canonical_config_json(config: &RunConfig) -> Result<String, ConfigError> {
    let value = serde_json::to_value(config).map_err(|e| ConfigError::Json(e.to_string()))?;
    let normalized = sort_json_value(value);
    serde_json::to_string(&normalized).map_err(|e| ConfigError::Json(e.to_string()))
}

fn sort_json_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                if let Some(val) = map.get(&k) {
                    out.insert(k, sort_json_value(val.clone()));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_json_value).collect())
        }
        other => other,
    }
}

fn nonblank(v: Option<&String>) -> Option<&str> {
    v.map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Lift a nested `[plugins] dirs = […]` table into the flat `plugin_dirs`
/// field that [`RunConfig`] expects.
///
/// TOML/YAML config files use the nested form:
///
/// ```toml
/// [plugins]
/// dirs = ["./plugins", "./custom"]
/// ```
///
/// which parses into `{"plugins": {"dirs": [...]}}`. The [`RunConfig`] struct
/// uses a flat `plugin_dirs` field, so this helper moves
/// `plugins.dirs` → `plugin_dirs` (and removes the now-empty `plugins`
/// table) before serde deserialization. If `plugin_dirs` is already present
/// at the top level, it is left untouched (top-level takes precedence).
fn lift_plugins_dirs(value: &mut serde_json::Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    // Only lift when the flat field is not already set.
    if map.contains_key("plugin_dirs") {
        return;
    }
    let Some(plugins) = map.remove("plugins") else {
        return;
    };
    if let Some(dirs) = plugins.get("dirs") {
        map.insert("plugin_dirs".to_string(), dirs.clone());
    }
}

/// Exact-match secret field names (compared case-insensitively against keys).
const SECRET_EXACT_MATCHES: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "credential",
    "credentials",
    "private_key",
    "authorization",
];

/// Suffix patterns that mark a key as secret-bearing (case-insensitive).
const SECRET_SUFFIXES: &[&str] = &["_key", "_token", "_secret", "_password", "_credential"];

/// Substrings that mark a key as secret-bearing (case-insensitive).
const SECRET_CONTAINS: &[&str] = &["secret", "password", "credential"];

/// Returns true when `key` (case-insensitively) looks like a secret field.
fn is_secret_field(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if SECRET_EXACT_MATCHES.iter().any(|&m| lower == m) {
        return true;
    }
    if SECRET_SUFFIXES.iter().any(|&s| lower.ends_with(s)) {
        return true;
    }
    if SECRET_CONTAINS.iter().any(|&s| lower.contains(s)) {
        return true;
    }
    false
}

/// Suggest the environment variable a rejected secret field should move to.
///
/// Well-known fields map to specific `BRIGID_LLM_*` vars; everything else falls
/// back to a generic `BRIGID_*` suggestion derived from the field name.
fn env_var_for_field(field: &str) -> String {
    let lower = field.to_ascii_lowercase();
    match lower.as_str() {
        "api_key" | "apikey" => "BRIGID_LLM_API_KEY".to_owned(),
        "token" => "BRIGID_LLM_TOKEN".to_owned(),
        "secret" => "BRIGID_LLM_SECRET".to_owned(),
        "password" => "BRIGID_LLM_PASSWORD".to_owned(),
        "credential" | "credentials" => "BRIGID_LLM_CREDENTIAL".to_owned(),
        "private_key" => "BRIGID_LLM_PRIVATE_KEY".to_owned(),
        "authorization" => "BRIGID_LLM_AUTHORIZATION".to_owned(),
        _ => {
            // Strip known secret suffixes/prefixes and build BRIGID_<STEM>.
            let stem = lower
                .trim_end_matches("_key")
                .trim_end_matches("_token")
                .trim_end_matches("_secret")
                .trim_end_matches("_password")
                .trim_end_matches("_credential");
            format!("BRIGID_{}", stem.to_ascii_uppercase())
        }
    }
}

/// Check a parsed config value for secret-bearing field names.
///
/// Recursively walks objects (and arrays of objects) so nested tables like
/// `[llm] api_key = …` are also caught. Returns the first rejected field, if
/// any. This runs *before* deserializing into [`RunConfig`] so unknown
/// secret-like keys — not just known struct fields — are rejected.
///
/// # Errors
///
/// Returns [`ConfigError::SecretFieldRejected`] for the first secret-like key.
fn check_for_secret_fields(value: &serde_json::Value) -> Result<(), ConfigError> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                if is_secret_field(key) {
                    return Err(ConfigError::SecretFieldRejected {
                        field: key.clone(),
                        env_var: env_var_for_field(key),
                    });
                }
                check_for_secret_fields(val)?;
            }
            Ok(())
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                check_for_secret_fields(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_populated() {
        let d = RunConfig::default();
        assert_eq!(d.root.as_deref(), Some(std::path::Path::new(".")));
        assert_eq!(d.max_llm_calls, Some(DEFAULT_MAX_LLM_CALLS));
        assert_eq!(d.language.as_deref(), Some("en"));
    }

    #[test]
    fn precedence_cli_beats_file_beats_env_beats_defaults() {
        let env = RunConfig {
            language: Some("fr".into()),
            max_llm_calls: Some(10),
            ..RunConfig::empty()
        };

        let file = RunConfig {
            language: Some("es".into()),
            provider: Some("openai".into()),
            ..RunConfig::empty()
        };

        let cli = RunConfig {
            language: Some("de".into()),
            ..RunConfig::empty()
        };

        let resolved = resolve_config(&env, &file, &cli);
        assert_eq!(resolved.language.as_deref(), Some("de")); // CLI
        assert_eq!(resolved.provider.as_deref(), Some("openai")); // file
        assert_eq!(resolved.max_llm_calls, Some(10)); // env
        assert_eq!(resolved.root.as_deref(), Some(std::path::Path::new("."))); // default
    }

    #[test]
    fn blank_env_does_not_override_defaults() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_LANGUAGE".into(), "   ".into());
        vars.insert("BRIGID_MAX_LLM_CALLS".into(), "".into());
        vars.insert("BRIGID_ROOT".into(), "/tmp/repo".into());
        let env = config_from_env_map(&vars).expect("env map");
        let resolved = resolve_config(&env, &RunConfig::empty(), &RunConfig::empty());
        assert_eq!(resolved.language.as_deref(), Some("en")); // default, blank ignored
        assert_eq!(resolved.max_llm_calls, Some(DEFAULT_MAX_LLM_CALLS));
        assert_eq!(
            resolved.root.as_deref(),
            Some(std::path::Path::new("/tmp/repo"))
        );
    }

    #[test]
    fn parse_toml_and_yaml_layers() {
        let toml_cfg = parse_toml_config(
            r#"
language = "es"
max_llm_calls = 42
apps = ["apps/alpha", "apps/beta"]
"#,
        )
        .expect("toml");
        assert_eq!(toml_cfg.language.as_deref(), Some("es"));
        assert_eq!(toml_cfg.max_llm_calls, Some(42));
        assert_eq!(
            toml_cfg.apps.as_deref(),
            Some(["apps/alpha".to_owned(), "apps/beta".to_owned()].as_slice())
        );

        let yaml_cfg = parse_yaml_config(
            r#"
language: fr
provider: anthropic
"#,
        )
        .expect("yaml");
        assert_eq!(yaml_cfg.language.as_deref(), Some("fr"));
        assert_eq!(yaml_cfg.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn env_apps_comma_separated() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_APPS".into(), "apps/a, apps/b ,".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(
            env.apps.as_deref(),
            Some(["apps/a".to_owned(), "apps/b".to_owned()].as_slice())
        );
    }

    #[test]
    fn canonical_json_is_stable_and_key_ordered() {
        let a = RunConfig {
            provider: Some("x".into()),
            ..RunConfig::default()
        };
        let b = RunConfig {
            provider: Some("x".into()),
            ..RunConfig::default()
        };
        let ja = canonical_config_json(&a).unwrap();
        let jb = canonical_config_json(&b).unwrap();
        assert_eq!(ja, jb);
        // keys sorted: apps before language, etc.
        assert!(ja.find("apps").unwrap() < ja.find("language").unwrap());

        let b2 = RunConfig {
            provider: Some("y".into()),
            ..RunConfig::default()
        };
        assert_ne!(
            canonical_config_json(&a).unwrap(),
            canonical_config_json(&b2).unwrap()
        );
    }

    #[test]
    fn invalid_toml_errors() {
        let err = parse_toml_config("language = [").unwrap_err();
        assert!(
            matches!(err, ConfigError::Toml(_)),
            "expected Toml error, got {err}"
        );
    }

    #[test]
    fn invalid_env_numeric_errors() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_MAX_LLM_CALLS".into(), "abc".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::InvalidEnvValue { ref key, .. } if key == "BRIGID_MAX_LLM_CALLS"
            ),
            "got {err}"
        );
    }

    #[test]
    fn invalid_yaml_errors() {
        let err = parse_yaml_config("language: [").unwrap_err();
        assert!(
            matches!(err, ConfigError::Yaml(_)),
            "expected Yaml error, got {err}"
        );
    }

    // --- Secret-field guard (issue #73) ---

    fn assert_secret_rejected(err: ConfigError, expected_field: &str) {
        match err {
            ConfigError::SecretFieldRejected { field, env_var } => {
                assert_eq!(field, expected_field, "field name mismatch");
                assert!(
                    !env_var.is_empty(),
                    "env var suggestion should be non-empty"
                );
                assert!(
                    env_var.starts_with("BRIGID_"),
                    "env var should start with BRIGID_, got {env_var}"
                );
            }
            other => panic!("expected SecretFieldRejected, got {other:?}"),
        }
    }

    #[test]
    fn toml_api_key_rejected() {
        let err = parse_toml_config(r#"api_key = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "api_key");
    }

    #[test]
    fn toml_token_rejected() {
        let err = parse_toml_config(r#"token = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "token");
    }

    #[test]
    fn toml_suffix_key_rejected() {
        let err = parse_toml_config(r#"llm_api_key = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "llm_api_key");
    }

    #[test]
    fn toml_suffix_token_rejected() {
        let err = parse_toml_config(r#"github_token = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "github_token");
    }

    #[test]
    fn toml_contains_secret_rejected() {
        let err = parse_toml_config(r#"my_secret_field = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "my_secret_field");
    }

    #[test]
    fn toml_password_rejected() {
        let err = parse_toml_config(r#"password = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "password");
    }

    #[test]
    fn toml_benign_config_accepted() {
        let cfg = parse_toml_config(
            r#"
language = "es"
max_llm_calls = 42
apps = ["apps/alpha", "apps/beta"]
"#,
        )
        .expect("benign toml should be accepted");
        assert_eq!(cfg.language.as_deref(), Some("es"));
        assert_eq!(cfg.max_llm_calls, Some(42));
    }

    #[test]
    fn yaml_api_key_rejected() {
        let err = parse_yaml_config("api_key: xxx\n").unwrap_err();
        assert_secret_rejected(err, "api_key");
    }

    #[test]
    fn yaml_token_rejected() {
        let err = parse_yaml_config("token: xxx\n").unwrap_err();
        assert_secret_rejected(err, "token");
    }

    #[test]
    fn yaml_benign_config_accepted() {
        let cfg = parse_yaml_config("language: fr\nprovider: anthropic\n")
            .expect("benign yaml should be accepted");
        assert_eq!(cfg.language.as_deref(), Some("fr"));
        assert_eq!(cfg.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn yaml_nested_secret_rejected() {
        let err = parse_yaml_config("llm:\n  api_key: xxx\n").unwrap_err();
        assert_secret_rejected(err, "api_key");
    }

    #[test]
    fn toml_case_insensitive_api_key_rejected() {
        let err = parse_toml_config(r#"API_KEY = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "API_KEY");
    }

    #[test]
    fn toml_case_insensitive_mixed_case_rejected() {
        let err = parse_toml_config(r#"Api_Key = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "Api_Key");
    }

    #[test]
    fn error_message_includes_field_and_env_var() {
        let err = parse_toml_config(r#"api_key = "xxx""#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key"), "msg should mention field: {msg}");
        assert!(
            msg.contains("BRIGID_LLM_API_KEY"),
            "msg should suggest env var: {msg}"
        );
    }

    #[test]
    fn toml_nested_secret_rejected() {
        let err = parse_toml_config(
            r#"
[llm]
api_key = "xxx"
"#,
        )
        .unwrap_err();
        assert_secret_rejected(err, "api_key");
    }

    #[test]
    fn toml_credential_rejected() {
        let err = parse_toml_config(r#"credentials = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "credentials");
    }

    #[test]
    fn toml_private_key_rejected() {
        let err = parse_toml_config(r#"private_key = "xxx""#).unwrap_err();
        assert_secret_rejected(err, "private_key");
    }

    #[test]
    fn toml_authorization_rejected() {
        let err = parse_toml_config(r#"authorization = "Bearer xxx""#).unwrap_err();
        assert_secret_rejected(err, "authorization");
    }

    #[test]
    fn cache_size_limit_mb_from_toml() {
        let cfg = parse_toml_config("cache_size_limit_mb = 50\n").expect("toml");
        assert_eq!(cfg.cache_size_limit_mb, Some(50));
    }

    #[test]
    fn cache_size_limit_mb_from_yaml() {
        let cfg = parse_yaml_config("cache_size_limit_mb: 200\n").expect("yaml");
        assert_eq!(cfg.cache_size_limit_mb, Some(200));
    }

    #[test]
    fn cache_size_limit_mb_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_CACHE_SIZE_LIMIT_MB".into(), "42".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.cache_size_limit_mb, Some(42));
    }

    #[test]
    fn cache_size_limit_mb_default_is_none() {
        let d = RunConfig::default();
        assert_eq!(d.cache_size_limit_mb, None);
    }

    #[test]
    fn cache_size_limit_mb_merge_layer() {
        let env = RunConfig {
            cache_size_limit_mb: Some(42),
            ..RunConfig::empty()
        };
        let file = RunConfig {
            cache_size_limit_mb: Some(100),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &RunConfig::empty());
        assert_eq!(resolved.cache_size_limit_mb, Some(100));
    }

    #[test]
    fn cache_size_limit_mb_blank_env_ignored() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_CACHE_SIZE_LIMIT_MB".into(), "".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.cache_size_limit_mb, None);
    }

    // --- Configurable host allowlist (issue #175) ---

    #[test]
    fn toml_allowed_hosts_array_of_tables() {
        let cfg = parse_toml_config(
            r#"
language = "en"

[[allowed_hosts]]
host = "my-proxy.internal"

[[allowed_hosts]]
host = "llm-gateway.corp.example"
"#,
        )
        .expect("toml");
        assert_eq!(
            cfg.allowed_hosts.as_deref(),
            Some(
                [
                    "my-proxy.internal".to_owned(),
                    "llm-gateway.corp.example".to_owned()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn yaml_allowed_hosts_array_of_tables() {
        let cfg = parse_yaml_config(
            "language: en\nallowed_hosts:\n  - host: my-proxy.internal\n  - host: llm-gateway.corp.example\n",
        )
        .expect("yaml");
        assert_eq!(
            cfg.allowed_hosts.as_deref(),
            Some(
                [
                    "my-proxy.internal".to_owned(),
                    "llm-gateway.corp.example".to_owned()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn env_allowed_hosts_comma_separated_and_trimmed() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "BRIGID_ALLOWED_HOSTS".into(),
            " my-proxy.internal , Another.Host.COM ".into(),
        );
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(
            env.allowed_hosts.as_deref(),
            Some(
                [
                    "my-proxy.internal".to_owned(),
                    "another.host.com".to_owned()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn env_allowed_hosts_blank_ignored() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_ALLOWED_HOSTS".into(), "  ,  ".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.allowed_hosts, None);
    }

    #[test]
    fn env_allowed_hosts_unset_is_none() {
        let vars = BTreeMap::new();
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.allowed_hosts, None);
    }

    #[test]
    fn invalid_host_wildcard_rejected() {
        let err = validate_hostname("*.example.com").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAllowedHost { .. }));
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_ALLOWED_HOSTS".into(), "*.example.com".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAllowedHost { .. }));
    }

    #[test]
    fn invalid_host_path_rejected() {
        let err = validate_hostname("evil.com/path").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAllowedHost { .. }));
    }

    #[test]
    fn invalid_host_empty_rejected() {
        let err = validate_hostname("").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAllowedHost { .. }));
    }

    #[test]
    fn invalid_host_whitespace_rejected() {
        let err = validate_hostname("evil host.com").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAllowedHost { .. }));
    }

    #[test]
    fn validate_hostname_accepts_known_hosts() {
        validate_hostname("api.openai.com").expect("openai");
        validate_hostname("api.deepseek.com").expect("deepseek");
        validate_hostname("localhost").expect("localhost");
        validate_hostname("127.0.0.1").expect("loopback");
        validate_hostname("my-proxy.internal").expect("custom");
    }

    #[test]
    fn merge_allowed_hosts_dedups_preserving_order() {
        let defaults = [
            "api.openai.com",
            "api.deepseek.com",
            "localhost",
            "127.0.0.1",
        ];
        let env = vec!["my-proxy.internal".to_owned(), "api.openai.com".to_owned()];
        let file = vec![
            "my-proxy.internal".to_owned(),
            "llm-gateway.corp".to_owned(),
        ];
        let merged = merge_allowed_hosts(&defaults, &env, &file);
        assert_eq!(
            merged,
            vec![
                "api.openai.com",
                "api.deepseek.com",
                "localhost",
                "127.0.0.1",
                "my-proxy.internal",
                "llm-gateway.corp",
            ]
        );
    }

    #[test]
    fn custom_host_warning_none_when_empty() {
        assert!(custom_host_warning(&[]).is_none());
    }

    #[test]
    fn custom_host_warning_message_when_custom() {
        let hosts = vec!["my-proxy.internal".to_owned()];
        let msg = custom_host_warning(&hosts).expect("warning");
        assert!(msg.to_lowercase().contains("warning"), "msg: {msg}");
        assert!(msg.contains("my-proxy.internal"), "msg: {msg}");
        assert!(msg.contains("host"), "msg: {msg}");
    }

    #[test]
    fn allowed_hosts_layer_accumulates_env_and_file() {
        let env = RunConfig {
            allowed_hosts: Some(vec!["env-host.local".to_owned()]),
            ..RunConfig::empty()
        };
        let file = RunConfig {
            allowed_hosts: Some(vec!["file-host.local".to_owned()]),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &RunConfig::empty());
        assert_eq!(
            resolved.allowed_hosts.as_deref(),
            Some(["env-host.local".to_owned(), "file-host.local".to_owned()].as_slice())
        );
    }

    // --- Issue #185: concurrency, max_abstractions, diagram_level config ---

    #[test]
    fn concurrency_from_toml() {
        let cfg = parse_toml_config("concurrency = 8\n").expect("toml");
        assert_eq!(cfg.concurrency, Some(8));
    }

    #[test]
    fn concurrency_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_CONCURRENCY".into(), "8".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.concurrency, Some(8));
    }

    #[test]
    fn concurrency_blank_env_ignored() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_CONCURRENCY".into(), "".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.concurrency, None);
    }

    #[test]
    fn concurrency_merge_layer() {
        let env = RunConfig {
            concurrency: Some(2),
            ..RunConfig::empty()
        };
        let file = RunConfig {
            concurrency: Some(4),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &RunConfig::empty());
        assert_eq!(resolved.concurrency, Some(4));
    }

    #[test]
    fn max_abstractions_from_toml() {
        let cfg = parse_toml_config("max_abstractions = 15\n").expect("toml");
        assert_eq!(cfg.max_abstractions, Some(15));
    }

    #[test]
    fn max_abstractions_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_MAX_ABSTRACTIONS".into(), "20".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.max_abstractions, Some(20));
    }

    #[test]
    fn diagram_level_from_toml() {
        let cfg = parse_toml_config("diagram_level = \"rich\"\n").expect("toml");
        assert_eq!(cfg.diagram_level.as_deref(), Some("rich"));
    }

    #[test]
    fn diagram_level_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_DIAGRAM_LEVEL".into(), "minimal".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.diagram_level.as_deref(), Some("minimal"));
    }

    #[test]
    fn diagram_level_merge_layer() {
        let env = RunConfig {
            diagram_level: Some("minimal".into()),
            ..RunConfig::empty()
        };
        let file = RunConfig {
            diagram_level: Some("rich".into()),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &RunConfig::empty());
        assert_eq!(resolved.diagram_level.as_deref(), Some("rich"));
    }

    // --- Issue #185: validate_config_for_check ---

    #[test]
    fn check_valid_config_no_issues() {
        let cfg = RunConfig::default();
        let issues = validate_config_for_check(&cfg);
        assert!(issues.is_empty(), "default config should have no issues");
    }

    #[test]
    fn check_max_llm_calls_zero_is_error() {
        let cfg = RunConfig {
            max_llm_calls: Some(0),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "error");
        assert!(issues[0].message.contains("max_llm_calls"));
    }

    #[test]
    fn check_concurrency_zero_is_error() {
        let cfg = RunConfig {
            concurrency: Some(0),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "error");
        assert!(issues[0].message.contains("concurrency"));
    }

    #[test]
    fn check_max_abstractions_zero_is_error() {
        let cfg = RunConfig {
            max_abstractions: Some(0),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "error");
        assert!(issues[0].message.contains("max_abstractions"));
    }

    #[test]
    fn check_invalid_diagram_level_is_error() {
        let cfg = RunConfig {
            diagram_level: Some("ultra".into()),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "error");
        assert!(issues[0].message.contains("diagram_level"));
    }

    #[test]
    fn check_valid_diagram_levels_pass() {
        for level in &["minimal", "standard", "rich", "MINIMAL", "Standard"] {
            let cfg = RunConfig {
                diagram_level: Some((*level).to_owned()),
                ..RunConfig::empty()
            };
            let issues = validate_config_for_check(&cfg);
            assert!(
                issues.is_empty(),
                "diagram_level {level:?} should be valid, got issues: {issues:?}"
            );
        }
    }

    #[test]
    fn check_cache_size_zero_is_error() {
        let cfg = RunConfig {
            cache_size_limit_mb: Some(0),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "error");
        assert!(issues[0].message.contains("cache_size_limit_mb"));
    }

    #[test]
    fn check_empty_language_is_warning() {
        let cfg = RunConfig {
            language: Some("".into()),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "warning");
        assert!(issues[0].message.contains("language"));
    }

    #[test]
    fn check_provider_without_model_is_warning() {
        let cfg = RunConfig {
            provider: Some("openai".into()),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "warning");
        assert!(issues[0].message.contains("model"));
    }

    #[test]
    fn check_model_without_provider_is_warning() {
        let cfg = RunConfig {
            model: Some("gpt-4".into()),
            ..RunConfig::empty()
        };
        let issues = validate_config_for_check(&cfg);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "warning");
        assert!(issues[0].message.contains("provider"));
    }

    // ------------------------------------------------------------------
    // Issue #230: env var coverage and allowed_hosts deserialization edge cases
    // ------------------------------------------------------------------

    /// Every `BRIGID_*` env var must be recognized by `config_from_env_map`.
    /// This test sets all of them at once and verifies each field is populated.
    #[test]
    fn env_map_covers_all_brigid_vars() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_OUTPUT".into(), "/tmp/out".into());
        vars.insert("BRIGID_LANGUAGE".into(), "es".into());
        vars.insert("BRIGID_PROVIDER".into(), "openai".into());
        vars.insert("BRIGID_MODEL".into(), "gpt-4".into());
        vars.insert("BRIGID_CACHE_DIR".into(), "/tmp/cache".into());
        vars.insert("BRIGID_CHECKPOINT_DIR".into(), "/tmp/ckpt".into());
        vars.insert("BRIGID_BATCH_CHAR_BUDGET".into(), "50000".into());
        vars.insert("BRIGID_CHARS_PER_TOKEN".into(), "3".into());

        let env = config_from_env_map(&vars).expect("env map should parse");

        assert_eq!(
            env.output.as_deref(),
            Some(std::path::Path::new("/tmp/out"))
        );
        assert_eq!(env.language.as_deref(), Some("es"));
        assert_eq!(env.provider.as_deref(), Some("openai"));
        assert_eq!(env.model.as_deref(), Some("gpt-4"));
        assert_eq!(
            env.cache_dir.as_deref(),
            Some(std::path::Path::new("/tmp/cache"))
        );
        assert_eq!(
            env.checkpoint_dir.as_deref(),
            Some(std::path::Path::new("/tmp/ckpt"))
        );
        assert_eq!(env.batch_char_budget, Some(50_000));
        assert_eq!(env.chars_per_token, Some(3));
    }

    /// Each numeric `BRIGID_*` var must return `InvalidEnvValue` when set to
    /// a non-integer string.
    #[test]
    fn env_map_numeric_vars_invalid_values_error() {
        let numeric_vars = ["BRIGID_BATCH_CHAR_BUDGET", "BRIGID_CHARS_PER_TOKEN"];
        for key in &numeric_vars {
            let mut vars = BTreeMap::new();
            vars.insert((*key).to_string(), "not-a-number".into());
            let err = config_from_env_map(&vars).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidEnvValue { key: ref k, .. } if k == *key),
                "{key} should give InvalidEnvValue, got {err:?}"
            );
        }
    }

    /// `allowed_hosts` deserialization: the `[[allowed_hosts]] host = "x"`
    /// table form must produce a `Vec<String>` with the host values.
    #[test]
    fn allowed_hosts_table_form_single_entry() {
        let cfg = parse_toml_config(
            r#"
[[allowed_hosts]]
host = "x.example.com"
"#,
        )
        .expect("toml");
        assert_eq!(
            cfg.allowed_hosts.as_deref(),
            Some(["x.example.com".to_owned()].as_slice())
        );
    }

    /// `allowed_hosts` entry missing the `host` field must produce a
    /// deserialization error.
    #[test]
    fn allowed_hosts_missing_host_field_errors() {
        let err = parse_toml_config(
            r#"
[[allowed_hosts]]
port = 8080
"#,
        )
        .unwrap_err();
        // The error should be a Toml parse error (from serde custom).
        assert!(
            matches!(err, ConfigError::Toml(ref msg)
                if msg.contains("host") || msg.contains("allowed_hosts")),
            "missing host field should give Toml error mentioning host, got {err:?}"
        );
    }

    /// `allowed_hosts` entry that is a bare number (not a string or object)
    /// must produce a deserialization error.
    #[test]
    fn allowed_hosts_numeric_entry_errors() {
        let err = parse_toml_config(
            r#"
allowed_hosts = [42]
"#,
        )
        .unwrap_err();
        // The error should be a Toml parse error.
        assert!(
            matches!(err, ConfigError::Toml(_)),
            "numeric allowed_hosts entry should give Toml error, got {err:?}"
        );
    }

    /// `allowed_hosts` array-of-strings form should also work (the simple
    /// form alongside the table form).
    #[test]
    fn allowed_hosts_array_of_strings_form() {
        let cfg = parse_toml_config(
            r#"
allowed_hosts = ["a.com", "b.com"]
"#,
        )
        .expect("toml");
        assert_eq!(
            cfg.allowed_hosts.as_deref(),
            Some(["a.com".to_owned(), "b.com".to_owned()].as_slice())
        );
    }

    /// `BRIGID_MAX_LLM_CALLS` with an invalid value must error.
    #[test]
    fn env_max_llm_calls_invalid_errors() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_MAX_LLM_CALLS".into(), "xyz".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnvValue { ref key, .. }
                if key == "BRIGID_MAX_LLM_CALLS"),
            "got {err:?}"
        );
    }

    /// `BRIGID_CACHE_SIZE_LIMIT_MB` with an invalid value must error.
    #[test]
    fn env_cache_size_limit_mb_invalid_errors() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_CACHE_SIZE_LIMIT_MB".into(), "abc".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnvValue { ref key, .. }
                if key == "BRIGID_CACHE_SIZE_LIMIT_MB"),
            "got {err:?}"
        );
    }

    /// `BRIGID_CONCURRENCY` with an invalid value must error.
    #[test]
    fn env_concurrency_invalid_errors() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_CONCURRENCY".into(), "NaN".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnvValue { ref key, .. }
                if key == "BRIGID_CONCURRENCY"),
            "got {err:?}"
        );
    }

    /// `BRIGID_MAX_ABSTRACTIONS` with an invalid value must error.
    #[test]
    fn env_max_abstractions_invalid_errors() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_MAX_ABSTRACTIONS".into(), "forty".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnvValue { ref key, .. }
                if key == "BRIGID_MAX_ABSTRACTIONS"),
            "got {err:?}"
        );
    }

    // --- Issue #225: `since` field for git-diff incremental crawl ---

    /// `RunConfig::default()` has `since = None` (unset).
    #[test]
    fn since_default_is_none() {
        let d = RunConfig::default();
        assert_eq!(d.since, None);
    }

    /// `RunConfig::empty()` has `since = None`.
    #[test]
    fn since_empty_is_none() {
        let e = RunConfig::empty();
        assert_eq!(e.since, None);
    }

    /// `brigid.toml` supports `since = "v0.5.0"`.
    #[test]
    fn since_from_toml() {
        let cfg = parse_toml_config(r#"since = "v0.5.0""#).expect("toml");
        assert_eq!(cfg.since.as_deref(), Some("v0.5.0"));
    }

    /// `.brigid.yaml` supports `since: v0.5.0`.
    #[test]
    fn since_from_yaml() {
        let cfg = parse_yaml_config("since: v0.5.0\n").expect("yaml");
        assert_eq!(cfg.since.as_deref(), Some("v0.5.0"));
    }

    /// `BRIGID_SINCE` env var maps to `config.since`.
    #[test]
    fn since_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_SINCE".into(), "HEAD~1".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.since.as_deref(), Some("HEAD~1"));
    }

    /// Blank `BRIGID_SINCE` is ignored (does not override defaults).
    #[test]
    fn since_blank_env_ignored() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_SINCE".into(), "   ".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.since, None);
    }

    /// `merge_layer`: CLI `since` overrides file `since`.
    #[test]
    fn since_cli_overrides_file() {
        let env = RunConfig::empty();
        let file = RunConfig {
            since: Some("v0.4.0".into()),
            ..RunConfig::empty()
        };
        let cli = RunConfig {
            since: Some("v0.5.0".into()),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &cli);
        assert_eq!(resolved.since.as_deref(), Some("v0.5.0"));
    }

    /// `merge_layer`: file `since` overrides env `since`.
    #[test]
    fn since_file_overrides_env() {
        let env = RunConfig {
            since: Some("HEAD~3".into()),
            ..RunConfig::empty()
        };
        let file = RunConfig {
            since: Some("v0.5.0".into()),
            ..RunConfig::empty()
        };
        let cli = RunConfig::empty();
        let resolved = resolve_config(&env, &file, &cli);
        assert_eq!(resolved.since.as_deref(), Some("v0.5.0"));
    }

    /// `merge_layer`: env `since` overrides default (None).
    #[test]
    fn since_env_overrides_default() {
        let env = RunConfig {
            since: Some("HEAD~1".into()),
            ..RunConfig::empty()
        };
        let file = RunConfig::empty();
        let cli = RunConfig::empty();
        let resolved = resolve_config(&env, &file, &cli);
        assert_eq!(resolved.since.as_deref(), Some("HEAD~1"));
    }

    /// Full layering: CLI > file > env > defaults for `since`.
    #[test]
    fn since_full_precedence_cli_file_env() {
        let env = RunConfig {
            since: Some("env-ref".into()),
            ..RunConfig::empty()
        };
        let file = RunConfig {
            since: Some("file-ref".into()),
            ..RunConfig::empty()
        };
        let cli = RunConfig {
            since: Some("cli-ref".into()),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &cli);
        assert_eq!(resolved.since.as_deref(), Some("cli-ref"));
    }

    /// `since` is included in canonical JSON (checkpoint hashing).
    #[test]
    fn since_included_in_canonical_json() {
        let a = RunConfig {
            since: Some("v0.5.0".into()),
            ..RunConfig::default()
        };
        let b = RunConfig {
            since: Some("v0.6.0".into()),
            ..RunConfig::default()
        };
        assert_ne!(
            canonical_config_json(&a).unwrap(),
            canonical_config_json(&b).unwrap()
        );
    }

    // -----------------------------------------------------------------
    // plugin_dirs (issue #228 / ADR 0014)
    // -----------------------------------------------------------------

    /// `RunConfig::empty()` has `plugin_dirs = None`.
    #[test]
    fn plugin_dirs_empty_is_none() {
        let e = RunConfig::empty();
        assert_eq!(e.plugin_dirs, None);
    }

    /// `RunConfig::default()` has `plugin_dirs = None`.
    #[test]
    fn plugin_dirs_default_is_none() {
        let d = RunConfig::default();
        assert_eq!(d.plugin_dirs, None);
    }

    /// `brigid.toml` supports `[plugins] dirs = […]`.
    #[test]
    fn plugin_dirs_from_toml() {
        let cfg = parse_toml_config(
            r#"
[plugins]
dirs = ["./plugins", "./custom"]
"#,
        )
        .expect("toml");
        assert_eq!(
            cfg.plugin_dirs.as_deref(),
            Some(
                [
                    std::path::PathBuf::from("./plugins"),
                    std::path::PathBuf::from("./custom")
                ]
                .as_slice()
            )
        );
    }

    /// `.brigid.yaml` supports `plugins: { dirs: [...] }`.
    #[test]
    fn plugin_dirs_from_yaml() {
        let cfg = parse_yaml_config("plugins:\n  dirs:\n    - ./plugins\n    - ./custom\n")
            .expect("yaml");
        assert_eq!(
            cfg.plugin_dirs.as_deref(),
            Some(
                [
                    std::path::PathBuf::from("./plugins"),
                    std::path::PathBuf::from("./custom")
                ]
                .as_slice()
            )
        );
    }

    /// `BRIGID_PLUGIN_DIRS` env var (colon-separated) maps to `plugin_dirs`.
    #[test]
    fn plugin_dirs_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_PLUGIN_DIRS".into(), "./a:./b".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(
            env.plugin_dirs.as_deref(),
            Some(
                [
                    std::path::PathBuf::from("./a"),
                    std::path::PathBuf::from("./b")
                ]
                .as_slice()
            )
        );
    }

    /// Blank `BRIGID_PLUGIN_DIRS` is ignored.
    #[test]
    fn plugin_dirs_blank_env_ignored() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_PLUGIN_DIRS".into(), "   ".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.plugin_dirs, None);
    }

    /// `merge_layer`: CLI `plugin_dirs` overrides file `plugin_dirs`.
    #[test]
    fn plugin_dirs_cli_overrides_file() {
        let env = RunConfig::empty();
        let file = RunConfig {
            plugin_dirs: Some(vec![std::path::PathBuf::from("./file-plugins")]),
            ..RunConfig::empty()
        };
        let cli = RunConfig {
            plugin_dirs: Some(vec![std::path::PathBuf::from("./cli-plugins")]),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &cli);
        assert_eq!(
            resolved.plugin_dirs.as_deref(),
            Some([std::path::PathBuf::from("./cli-plugins")].as_slice())
        );
    }

    /// `plugin_dirs` is included in canonical JSON (checkpoint hashing).
    #[test]
    fn plugin_dirs_included_in_canonical_json() {
        let a = RunConfig {
            plugin_dirs: Some(vec![std::path::PathBuf::from("./a")]),
            ..RunConfig::default()
        };
        let b = RunConfig {
            plugin_dirs: Some(vec![std::path::PathBuf::from("./b")]),
            ..RunConfig::default()
        };
        assert_ne!(
            canonical_config_json(&a).unwrap(),
            canonical_config_json(&b).unwrap()
        );
    }

    /// `[plugins]` with no `dirs` key does not error and leaves
    /// `plugin_dirs` as `None`.
    #[test]
    fn plugins_table_without_dirs_is_ok() {
        let cfg = parse_toml_config("[plugins]\n").expect("toml");
        assert_eq!(cfg.plugin_dirs, None);
    }

    // -----------------------------------------------------------------------
    // graph_provider (ADR 0016)
    // -----------------------------------------------------------------------

    /// `RunConfig::empty()` has `graph_provider = None`.
    #[test]
    fn graph_provider_empty_is_none() {
        let e = RunConfig::empty();
        assert_eq!(e.graph_provider, None);
    }

    /// `RunConfig::default()` has `graph_provider = None`.
    #[test]
    fn graph_provider_default_is_none() {
        let d = RunConfig::default();
        assert_eq!(d.graph_provider, None);
    }

    /// `brigid.toml` supports `[graph_provider] type = "codegraph"`.
    #[test]
    fn graph_provider_codegraph_from_toml() {
        let cfg = parse_toml_config(
            r#"
[graph_provider]
type = "codegraph"
index_path = ".codegraph/graph.db"
"#,
        )
        .expect("toml");
        let gp = cfg.graph_provider.expect("graph_provider should be set");
        assert_eq!(gp.provider_type.as_deref(), Some("codegraph"));
        assert_eq!(
            gp.index_path.as_deref(),
            Some(std::path::Path::new(".codegraph/graph.db"))
        );
        assert_eq!(gp.graph_path, None);
        assert_eq!(gp.providers, None);
    }

    /// `brigid.toml` supports `[graph_provider] type = "graphify"`.
    #[test]
    fn graph_provider_graphify_from_toml() {
        let cfg = parse_toml_config(
            r#"
[graph_provider]
type = "graphify"
graph_path = "graphify-out/graph.json"
"#,
        )
        .expect("toml");
        let gp = cfg.graph_provider.expect("graph_provider should be set");
        assert_eq!(gp.provider_type.as_deref(), Some("graphify"));
        assert_eq!(gp.index_path, None);
        assert_eq!(
            gp.graph_path.as_deref(),
            Some(std::path::Path::new("graphify-out/graph.json"))
        );
    }

    /// `brigid.toml` supports `[graph_provider] type = "composed"`.
    #[test]
    fn graph_provider_composed_from_toml() {
        let cfg = parse_toml_config(
            r#"
[graph_provider]
type = "composed"
providers = ["codegraph:.codegraph/graph.db", "graphify:graphify-out/graph.json"]
"#,
        )
        .expect("toml");
        let gp = cfg.graph_provider.expect("graph_provider should be set");
        assert_eq!(gp.provider_type.as_deref(), Some("composed"));
        assert_eq!(
            gp.providers.as_deref(),
            Some(
                [
                    "codegraph:.codegraph/graph.db".to_string(),
                    "graphify:graphify-out/graph.json".to_string()
                ]
                .as_slice()
            )
        );
    }

    /// `.brigid.yaml` supports `graph_provider: { type: ... }`.
    #[test]
    fn graph_provider_from_yaml() {
        let cfg = parse_yaml_config(
            "graph_provider:\n  type: codegraph\n  index_path: .codegraph/graph.db\n",
        )
        .expect("yaml");
        let gp = cfg.graph_provider.expect("graph_provider should be set");
        assert_eq!(gp.provider_type.as_deref(), Some("codegraph"));
        assert_eq!(
            gp.index_path.as_deref(),
            Some(std::path::Path::new(".codegraph/graph.db"))
        );
    }

    /// `BRIGID_GRAPH_PROVIDER` env var sets the provider type.
    #[test]
    fn graph_provider_from_env() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_GRAPH_PROVIDER".into(), "codegraph".into());
        let env = config_from_env_map(&vars).expect("env map");
        let gp = env.graph_provider.expect("graph_provider should be set");
        assert_eq!(gp.provider_type.as_deref(), Some("codegraph"));
        assert_eq!(gp.index_path, None);
    }

    /// Blank `BRIGID_GRAPH_PROVIDER` is ignored.
    #[test]
    fn graph_provider_blank_env_ignored() {
        let mut vars = BTreeMap::new();
        vars.insert("BRIGID_GRAPH_PROVIDER".into(), "   ".into());
        let env = config_from_env_map(&vars).expect("env map");
        assert_eq!(env.graph_provider, None);
    }

    /// `merge_layer`: CLI `graph_provider` overrides file `graph_provider`.
    #[test]
    fn graph_provider_cli_overrides_file() {
        let env = RunConfig::empty();
        let file = RunConfig {
            graph_provider: Some(GraphProviderConfig {
                provider_type: Some("graphify".into()),
                index_path: None,
                graph_path: Some(std::path::PathBuf::from("file.json")),
                providers: None,
            }),
            ..RunConfig::empty()
        };
        let cli = RunConfig {
            graph_provider: Some(GraphProviderConfig {
                provider_type: Some("codegraph".into()),
                index_path: Some(std::path::PathBuf::from("cli.db")),
                graph_path: None,
                providers: None,
            }),
            ..RunConfig::empty()
        };
        let resolved = resolve_config(&env, &file, &cli);
        let gp = resolved
            .graph_provider
            .expect("graph_provider should be set");
        assert_eq!(gp.provider_type.as_deref(), Some("codegraph"));
        assert_eq!(
            gp.index_path.as_deref(),
            Some(std::path::Path::new("cli.db"))
        );
    }

    /// `graph_provider` is included in canonical JSON (checkpoint hashing).
    #[test]
    fn graph_provider_included_in_canonical_json() {
        let a = RunConfig {
            graph_provider: Some(GraphProviderConfig {
                provider_type: Some("codegraph".into()),
                index_path: Some(std::path::PathBuf::from("a.db")),
                graph_path: None,
                providers: None,
            }),
            ..RunConfig::default()
        };
        let b = RunConfig {
            graph_provider: Some(GraphProviderConfig {
                provider_type: Some("graphify".into()),
                index_path: None,
                graph_path: Some(std::path::PathBuf::from("b.json")),
                providers: None,
            }),
            ..RunConfig::default()
        };
        assert_ne!(
            canonical_config_json(&a).unwrap(),
            canonical_config_json(&b).unwrap()
        );
    }

    /// Config without `[graph_provider]` section has `graph_provider = None`.
    #[test]
    fn graph_provider_absent_in_toml_is_none() {
        let cfg = parse_toml_config("language = \"en\"\n").expect("toml");
        assert_eq!(cfg.graph_provider, None);
    }
}
