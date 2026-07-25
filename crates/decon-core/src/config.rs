//! Run configuration and layered loading.
//!
//! Precedence (highest wins): **CLI overrides** → **project file**
//! (`decon.toml` / `.decon.yaml`) → **environment** (`DECON_*`) → **defaults**.
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
    #[error("invalid decon.toml: {0}")]
    Toml(String),
    /// YAML body is invalid.
    #[error("invalid .decon.yaml: {0}")]
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

/// Parse a TOML document into a config layer (`decon.toml` body).
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
    let json_value = serde_json::to_value(&value).map_err(|e| ConfigError::Toml(e.to_string()))?;
    check_for_secret_fields(&json_value)?;
    serde_json::from_value(json_value).map_err(|e| ConfigError::Toml(e.to_string()))
}

/// Parse a YAML document into a config layer (`.decon.yaml` body).
///
/// The raw parsed value is scanned for secret-bearing field names *before*
/// deserializing into [`RunConfig`] (defense-in-depth — see issue #73).
///
/// # Errors
///
/// Returns [`ConfigError::Yaml`] when YAML is invalid or types do not match,
/// or [`ConfigError::SecretFieldRejected`] when a secret-bearing key is found.
pub fn parse_yaml_config(text: &str) -> Result<RunConfig, ConfigError> {
    let value: serde_json::Value =
        serde_yaml_ng::from_str(text).map_err(|e| ConfigError::Yaml(e.to_string()))?;
    check_for_secret_fields(&value)?;
    serde_json::from_value(value).map_err(|e| ConfigError::Yaml(e.to_string()))
}

/// Load a config layer from environment-style key/value pairs.
///
/// Recognized keys (case-sensitive):
/// - `DECON_ROOT`, `DECON_OUTPUT`, `DECON_APPS` (comma-separated),
/// - `DECON_LANGUAGE`, `DECON_MAX_LLM_CALLS`, `DECON_PROVIDER`, `DECON_MODEL`,
/// - `DECON_CACHE_DIR`, `DECON_CHECKPOINT_DIR`,
/// - `DECON_BATCH_CHAR_BUDGET`, `DECON_CHARS_PER_TOKEN`
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
    if let Some(v) = nonblank(vars.get("DECON_ROOT")) {
        cfg.root = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("DECON_OUTPUT")) {
        cfg.output = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("DECON_APPS")) {
        let apps: Vec<String> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        cfg.apps = Some(apps);
    }
    if let Some(v) = nonblank(vars.get("DECON_LANGUAGE")) {
        cfg.language = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("DECON_MAX_LLM_CALLS")) {
        cfg.max_llm_calls = Some(parse_env_u32("DECON_MAX_LLM_CALLS", v)?);
    }
    if let Some(v) = nonblank(vars.get("DECON_PROVIDER")) {
        cfg.provider = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("DECON_MODEL")) {
        cfg.model = Some(v.to_owned());
    }
    if let Some(v) = nonblank(vars.get("DECON_CACHE_DIR")) {
        cfg.cache_dir = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("DECON_CHECKPOINT_DIR")) {
        cfg.checkpoint_dir = Some(PathBuf::from(v));
    }
    if let Some(v) = nonblank(vars.get("DECON_BATCH_CHAR_BUDGET")) {
        cfg.batch_char_budget = Some(parse_env_usize("DECON_BATCH_CHAR_BUDGET", v)?);
    }
    if let Some(v) = nonblank(vars.get("DECON_CHARS_PER_TOKEN")) {
        cfg.chars_per_token = Some(parse_env_usize("DECON_CHARS_PER_TOKEN", v)?);
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
/// Well-known fields map to specific `DECON_LLM_*` vars; everything else falls
/// back to a generic `DECON_*` suggestion derived from the field name.
fn env_var_for_field(field: &str) -> String {
    let lower = field.to_ascii_lowercase();
    match lower.as_str() {
        "api_key" | "apikey" => "DECON_LLM_API_KEY".to_owned(),
        "token" => "DECON_LLM_TOKEN".to_owned(),
        "secret" => "DECON_LLM_SECRET".to_owned(),
        "password" => "DECON_LLM_PASSWORD".to_owned(),
        "credential" | "credentials" => "DECON_LLM_CREDENTIAL".to_owned(),
        "private_key" => "DECON_LLM_PRIVATE_KEY".to_owned(),
        "authorization" => "DECON_LLM_AUTHORIZATION".to_owned(),
        _ => {
            // Strip known secret suffixes/prefixes and build DECON_<STEM>.
            let stem = lower
                .trim_end_matches("_key")
                .trim_end_matches("_token")
                .trim_end_matches("_secret")
                .trim_end_matches("_password")
                .trim_end_matches("_credential");
            format!("DECON_{}", stem.to_ascii_uppercase())
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
        vars.insert("DECON_LANGUAGE".into(), "   ".into());
        vars.insert("DECON_MAX_LLM_CALLS".into(), "".into());
        vars.insert("DECON_ROOT".into(), "/tmp/repo".into());
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
        vars.insert("DECON_APPS".into(), "apps/a, apps/b ,".into());
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
        vars.insert("DECON_MAX_LLM_CALLS".into(), "abc".into());
        let err = config_from_env_map(&vars).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::InvalidEnvValue { ref key, .. } if key == "DECON_MAX_LLM_CALLS"
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
                    env_var.starts_with("DECON_"),
                    "env var should start with DECON_, got {env_var}"
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
            msg.contains("DECON_LLM_API_KEY"),
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
}
