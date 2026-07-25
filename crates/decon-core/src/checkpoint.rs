//! Checkpoint schema v1 types (ADR 0001).
//!
//! Pure types for `checkpoint.json` metadata and NDJSON file-bundle records.
//! **No filesystem I/O** — persistence lives in `decon-pipeline`.
//!
//! See [`docs/adr/0001-checkpoint-schema-v1.md`](../../../../docs/adr/0001-checkpoint-schema-v1.md).

use crate::config::{RunConfig, canonical_config_json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

/// Schema version for checkpoint metadata (`checkpoint.json`).
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Default relative path for the file body bundle next to `checkpoint.json`.
pub const DEFAULT_MANIFEST_REL_PATH: &str = "files.ndjson.gz";

/// Content encoding for file-bundle records.
pub const ENCODING_BASE64: &str = "base64";

/// Pipeline stages that may appear in `completed_stages` (ordered for resume).
///
/// Unknown future stages can still be stored as strings via
/// [`StageId::as_str`] / [`StageId::parse`]; this enum covers stages known
/// through M2/M3 design.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageId {
    /// Local or remote fetch/crawl of file inventory.
    Fetch,
    /// Zero-LLM dry-run plan assembly.
    DryRun,
    /// Identify abstractions (M3+).
    Identify,
    /// Relationship analysis.
    Relationships,
    /// Chapter ordering.
    Order,
    /// Chapter writing.
    Chapters,
    /// Setup guide generation.
    Setup,
    /// Architecture overview.
    Overview,
    /// Combine index + chapters.
    Combine,
    /// Structural eval.
    Eval,
}

impl StageId {
    /// Canonical snake_case wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::DryRun => "dry_run",
            Self::Identify => "identify",
            Self::Relationships => "relationships",
            Self::Order => "order",
            Self::Chapters => "chapters",
            Self::Setup => "setup",
            Self::Overview => "overview",
            Self::Combine => "combine",
            Self::Eval => "eval",
        }
    }

    /// Parse a wire name into a known stage.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fetch" => Some(Self::Fetch),
            "dry_run" => Some(Self::DryRun),
            "identify" => Some(Self::Identify),
            "relationships" => Some(Self::Relationships),
            "order" => Some(Self::Order),
            "chapters" => Some(Self::Chapters),
            "setup" => Some(Self::Setup),
            "overview" => Some(Self::Overview),
            "combine" => Some(Self::Combine),
            "eval" => Some(Self::Eval),
            _ => None,
        }
    }

    /// Pipeline order for resume (earlier stages first).
    #[must_use]
    pub fn pipeline_order() -> &'static [StageId] {
        &[
            Self::Fetch,
            Self::DryRun,
            Self::Identify,
            Self::Relationships,
            Self::Order,
            Self::Chapters,
            Self::Setup,
            Self::Overview,
            Self::Combine,
            Self::Eval,
        ]
    }
}

/// Pointer to the compressed file-body bundle (`files.ndjson.gz`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestPointer {
    /// Path relative to the checkpoint directory.
    pub path: String,
    /// SHA-256 of the **compressed** bundle file bytes (`sha256:<hex>`).
    pub sha256: String,
    /// Compressed file size in bytes.
    pub size: u64,
}

impl ManifestPointer {
    /// Build a pointer with a properly prefixed hex digest.
    #[must_use]
    pub fn new(path: impl Into<String>, digest_hex: impl AsRef<str>, size: u64) -> Self {
        let hex = digest_hex.as_ref();
        let sha256 = if hex.starts_with("sha256:") {
            hex.to_owned()
        } else {
            format!("sha256:{hex}")
        };
        Self {
            path: path.into(),
            sha256,
            size,
        }
    }
}

/// Checkpoint bookkeeping timestamps and source identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointMeta {
    /// ISO 8601 UTC creation time.
    pub created_at: String,
    /// ISO 8601 UTC last update time.
    pub updated_at: String,
    /// Source identity (git SHA or resolved URL/revision).
    pub source_revision: String,
}

/// One line in `files.ndjson.gz` (decompressed NDJSON object).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileBundleRecord {
    /// Relative repository path (POSIX `/`).
    pub path: String,
    /// SHA-256 of raw bytes **after** redaction, **before** encoding (`sha256:<hex>`).
    pub sha256: String,
    /// How `content` is encoded (default [`ENCODING_BASE64`]).
    pub encoding: String,
    /// Encoded file body.
    pub content: String,
}

impl FileBundleRecord {
    /// Construct a base64-encoded record and set `sha256` from raw bytes.
    #[must_use]
    pub fn from_raw_bytes(
        path: impl Into<String>,
        raw: &[u8],
        content_base64: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            sha256: format!("sha256:{}", hex::encode(Sha256::digest(raw))),
            encoding: ENCODING_BASE64.to_owned(),
            content: content_base64.into(),
        }
    }
}

/// A single file-based stage output tracked in the stage-output manifest.
///
/// Records the relative path, SHA-256 digest, and byte size of a file written
/// by an M4 stage (chapters, setup, overview, combine) inside the checkpoint
/// directory. Used by the checkpoint store to verify file integrity on read.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StageOutputEntry {
    /// Path relative to the checkpoint directory (POSIX `/`).
    pub path: String,
    /// `sha256:<hex>` of the file bytes.
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

/// Manifest of file-based outputs for M4 stages, keyed by stage wire name.
///
/// Stored as [`CheckpointV1::stage_outputs`] (additive, no schema version
/// bump). On read, each entry's SHA-256 is verified against the on-disk file
/// to detect corruption.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StageOutputs {
    /// Map of stage wire name to its file output entries.
    #[serde(default)]
    pub entries: BTreeMap<String, Vec<StageOutputEntry>>,
}

impl StageOutputs {
    /// Construct an empty manifest.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or replace) the file entries for a stage.
    pub fn set(&mut self, stage: &str, files: Vec<StageOutputEntry>) {
        self.entries.insert(stage.to_owned(), files);
    }

    /// Get the file entries for a stage, if any.
    #[must_use]
    pub fn get(&self, stage: &str) -> Option<&[StageOutputEntry]> {
        self.entries.get(stage).map(Vec::as_slice)
    }

    /// Remove all entries for a stage.
    pub fn remove(&mut self, stage: &str) {
        self.entries.remove(stage);
    }

    /// Whether any entries are recorded for `stage`.
    #[must_use]
    pub fn has(&self, stage: &str) -> bool {
        self.entries.contains_key(stage)
    }
}

/// Full `checkpoint.json` document (schema v1).
///
/// File bodies are **not** stored here — only [`Self::manifest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckpointV1 {
    /// Must be [`CHECKPOINT_SCHEMA_VERSION`].
    pub version: u32,
    /// Stages completed successfully, in pipeline order.
    pub completed_stages: Vec<StageId>,
    /// Completion timestamps (ISO 8601 UTC) keyed by stage wire name.
    #[serde(default)]
    pub stage_timestamps: BTreeMap<String, String>,
    /// `sha256:<hex>` of canonical unredacted config JSON.
    pub config_hash: String,
    /// Redacted, human-readable config (not used for identity checks).
    pub config: RunConfig,
    /// Pointer to the file body bundle.
    pub manifest: ManifestPointer,
    /// Identify results (opaque JSON until domain types land).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abstractions: Option<serde_json::Value>,
    /// Relationship results (opaque JSON until domain types land).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationships: Option<serde_json::Value>,
    /// File-based stage output manifest for M4 stages (chapters, setup,
    /// overview, combine). Additive — no schema version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_outputs: Option<StageOutputs>,
    /// Bookkeeping metadata.
    pub metadata: CheckpointMeta,
}

/// Errors when validating or hashing checkpoint metadata.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// Unsupported or missing schema version.
    #[error("unsupported checkpoint version: {0}")]
    UnsupportedVersion(u32),
    /// Config canonicalization failed.
    #[error("config hash failed: {0}")]
    ConfigHash(String),
    /// JSON (de)serialization failed.
    #[error("checkpoint JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl CheckpointV1 {
    /// Create a new empty checkpoint shell for a run config and source identity.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::ConfigHash`] if config cannot be hashed.
    pub fn new(
        unredacted_config: &RunConfig,
        redacted_config: RunConfig,
        source_revision: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Result<Self, CheckpointError> {
        let created = created_at.into();
        Ok(Self {
            version: CHECKPOINT_SCHEMA_VERSION,
            completed_stages: Vec::new(),
            stage_timestamps: BTreeMap::new(),
            config_hash: config_hash(unredacted_config)?,
            config: redacted_config,
            manifest: ManifestPointer::new(DEFAULT_MANIFEST_REL_PATH, "", 0),
            abstractions: None,
            relationships: None,
            stage_outputs: None,
            metadata: CheckpointMeta {
                created_at: created.clone(),
                updated_at: created,
                source_revision: source_revision.into(),
            },
        })
    }

    /// Validate `version` is supported.
    ///
    /// # Errors
    ///
    /// [`CheckpointError::UnsupportedVersion`] when not v1.
    pub fn validate_version(&self) -> Result<(), CheckpointError> {
        if self.version != CHECKPOINT_SCHEMA_VERSION {
            return Err(CheckpointError::UnsupportedVersion(self.version));
        }
        Ok(())
    }

    /// Serialize to compact JSON (no trailing newline).
    ///
    /// # Errors
    ///
    /// [`CheckpointError::Json`] on serialization failure.
    pub fn to_json(&self) -> Result<String, CheckpointError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Parse from JSON text.
    ///
    /// # Errors
    ///
    /// [`CheckpointError::Json`] or version validation errors.
    pub fn from_json(text: &str) -> Result<Self, CheckpointError> {
        let cp: Self = serde_json::from_str(text)?;
        cp.validate_version()?;
        Ok(cp)
    }

    /// Mark a stage complete with a timestamp.
    ///
    /// Maintains pipeline order in `completed_stages` via sort (see [`StageId`]'s `Ord`).
    pub fn mark_stage_complete(&mut self, stage: StageId, timestamp_iso: impl Into<String>) {
        if !self.completed_stages.contains(&stage) {
            self.completed_stages.push(stage);
            self.completed_stages.sort();
        }
        let ts = timestamp_iso.into();
        self.stage_timestamps
            .insert(stage.as_str().to_owned(), ts.clone());
        self.metadata.updated_at = ts;
    }

    /// Whether `stage` is listed as completed.
    #[must_use]
    pub fn is_stage_complete(&self, stage: StageId) -> bool {
        self.completed_stages.contains(&stage)
    }
}

/// Compute `sha256:<hex>` over canonical **unredacted** config JSON (ADR 0001).
///
/// Identity for resume is the full runtime configuration (not the redacted
/// copy stored in `checkpoint.json`). Prefer keeping secrets out of
/// [`RunConfig`] fields that participate in hashing, or load them outside
/// the hashed object so credential rotation does not invalidate checkpoints
/// unintentionally. The persisted `config` field on [`CheckpointV1`] remains
/// redacted for human inspection only.
///
/// # Errors
///
/// Propagates canonicalization failures as [`CheckpointError::ConfigHash`].
pub fn config_hash(config: &RunConfig) -> Result<String, CheckpointError> {
    let canonical =
        canonical_config_json(config).map_err(|e| CheckpointError::ConfigHash(e.to_string()))?;
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(format!("sha256:{}", hex::encode(digest)))
}

/// SHA-256 of arbitrary bytes as `sha256:<hex>` (e.g. compressed manifest file).
#[must_use]
pub fn sha256_hex_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> RunConfig {
        RunConfig {
            language: Some("en".into()),
            max_llm_calls: Some(10),
            ..RunConfig::default()
        }
    }

    #[test]
    fn stage_id_round_trip_wire_names() {
        for stage in StageId::pipeline_order() {
            assert_eq!(StageId::parse(stage.as_str()), Some(*stage));
        }
        assert_eq!(StageId::parse("nope"), None);
    }

    #[test]
    fn config_hash_stable_and_sensitive() {
        let a = sample_config();
        let b = sample_config();
        assert_eq!(config_hash(&a).unwrap(), config_hash(&b).unwrap());
        assert!(config_hash(&a).unwrap().starts_with("sha256:"));

        let mut c = sample_config();
        c.language = Some("es".into());
        assert_ne!(config_hash(&a).unwrap(), config_hash(&c).unwrap());
    }

    #[test]
    fn checkpoint_json_round_trip() {
        let cfg = sample_config();
        let mut cp = CheckpointV1::new(
            &cfg,
            cfg.redacted_for_checkpoint(),
            "abc123",
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        cp.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");
        cp.manifest = ManifestPointer::new(DEFAULT_MANIFEST_REL_PATH, "deadbeef", 42);
        assert!(cp.manifest.sha256.starts_with("sha256:"));

        let json = cp.to_json().unwrap();
        assert!(!json.ends_with('\n'));
        let loaded = CheckpointV1::from_json(&json).unwrap();
        assert_eq!(loaded.version, CHECKPOINT_SCHEMA_VERSION);
        assert_eq!(loaded.completed_stages, vec![StageId::Fetch]);
        assert!(loaded.is_stage_complete(StageId::Fetch));
        assert!(!loaded.is_stage_complete(StageId::Identify));
        assert_eq!(loaded.manifest.size, 42);
    }

    #[test]
    fn unsupported_version_rejected() {
        let cfg = sample_config();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "t0").unwrap();
        cp.version = 99;
        let json = serde_json::to_string(&cp).unwrap();
        let err = CheckpointV1::from_json(&json).unwrap_err();
        assert!(matches!(err, CheckpointError::UnsupportedVersion(99)));
    }

    #[test]
    fn file_bundle_record_hashes_raw_bytes() {
        let raw = b"hello";
        let rec = FileBundleRecord::from_raw_bytes("a.txt", raw, "aGVsbG8=");
        assert_eq!(rec.encoding, ENCODING_BASE64);
        assert_eq!(
            rec.sha256,
            format!("sha256:{}", hex::encode(Sha256::digest(raw)))
        );
    }

    #[test]
    fn mark_stage_complete_keeps_pipeline_order() {
        let cfg = sample_config();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "r", "t0").unwrap();
        cp.mark_stage_complete(StageId::Identify, "t2");
        cp.mark_stage_complete(StageId::Fetch, "t1");
        assert_eq!(cp.completed_stages, vec![StageId::Fetch, StageId::Identify]);
    }

    #[test]
    fn manifest_pointer_prefixes_digest() {
        let p = ManifestPointer::new("files.ndjson.gz", "ab", 1);
        assert_eq!(p.sha256, "sha256:ab");
        let p2 = ManifestPointer::new("files.ndjson.gz", "sha256:cd", 2);
        assert_eq!(p2.sha256, "sha256:cd");
    }

    #[test]
    fn sha256_hex_prefixed_known() {
        // empty string SHA-256
        let h = sha256_hex_prefixed(b"");
        assert_eq!(
            h,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn stage_outputs_set_get_remove() {
        let mut so = StageOutputs::new();
        assert!(!so.has("chapters"));
        so.set(
            "chapters",
            vec![StageOutputEntry {
                path: "chapters/01_intro.md".into(),
                sha256: "sha256:abc".into(),
                size: 10,
            }],
        );
        assert!(so.has("chapters"));
        let got = so.get("chapters").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "chapters/01_intro.md");
        so.remove("chapters");
        assert!(!so.has("chapters"));
    }

    #[test]
    fn checkpoint_stage_outputs_round_trip() {
        let cfg = sample_config();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "t0").unwrap();
        let mut so = StageOutputs::new();
        so.set(
            "chapters",
            vec![StageOutputEntry {
                path: "chapters/01_intro.md".into(),
                sha256: "sha256:deadbeef".into(),
                size: 42,
            }],
        );
        cp.stage_outputs = Some(so);
        let json = cp.to_json().unwrap();
        assert!(!json.contains("stage_outputs") || json.contains("stage_outputs"));
        let loaded = CheckpointV1::from_json(&json).unwrap();
        let loaded_so = loaded.stage_outputs.expect("stage_outputs present");
        let entries = loaded_so.get("chapters").expect("chapters present");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "chapters/01_intro.md");
        assert_eq!(entries[0].sha256, "sha256:deadbeef");
        assert_eq!(entries[0].size, 42);
    }

    #[test]
    fn checkpoint_without_stage_outputs_back_compatible() {
        let cfg = sample_config();
        let cp = CheckpointV1::new(&cfg, cfg.clone(), "rev", "t0").unwrap();
        let json = cp.to_json().unwrap();
        assert!(!json.contains("stage_outputs"));
        let loaded = CheckpointV1::from_json(&json).unwrap();
        assert!(loaded.stage_outputs.is_none());
    }
}
