//! Checkpoint directory store: `checkpoint.json` + `files.ndjson.gz`.
//!
//! Implements ADR 0001 persistence: atomic publish of the file bundle then
//! metadata, and load-time validation of the manifest pointer checksum.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use decon_core::{
    CheckpointError, CheckpointV1, DEFAULT_MANIFEST_REL_PATH, FileBundleRecord, ManifestPointer,
    sha256_hex_prefixed,
};
use flate2::Compression;
use flate2::write::GzEncoder;
use thiserror::Error;

/// Errors while saving or loading a checkpoint directory.
#[derive(Debug, Error)]
pub enum CheckpointStoreError {
    /// Schema / JSON errors from core types.
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    /// Filesystem I/O failure.
    #[error("checkpoint I/O at {path}: {source}")]
    Io {
        /// Path related to the failure.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// Manifest pointer does not match on-disk file.
    #[error("manifest integrity check failed: {0}")]
    ManifestIntegrity(String),
    /// Checkpoint directory or required file is missing.
    #[error("checkpoint not found: {0}")]
    NotFound(PathBuf),
}

impl From<serde_json::Error> for CheckpointStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Checkpoint(e.into())
    }
}

/// Save and load ADR 0001 checkpoint directories.
#[derive(Clone, Debug)]
pub struct CheckpointStore {
    /// Root directory containing `checkpoint.json` and the manifest bundle.
    pub dir: PathBuf,
}

impl CheckpointStore {
    /// Create a store rooted at `dir` (directory need not exist yet for save).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.dir.join("checkpoint.json")
    }

    fn manifest_path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// Ensure manifest relative path is a single non-empty filename (no dirs / `..`).
    fn validate_manifest_rel(rel: &str) -> Result<(), CheckpointStoreError> {
        if rel.is_empty()
            || rel.contains('/')
            || rel.contains('\\')
            || rel == "."
            || rel == ".."
            || rel.contains("..")
        {
            return Err(CheckpointStoreError::ManifestIntegrity(format!(
                "unsafe manifest path: {rel:?} (must be a single relative filename)"
            )));
        }
        Ok(())
    }

    /// Write file records and metadata atomically (tmp → fsync → rename).
    ///
    /// # Errors
    ///
    /// Returns I/O or serialization errors.
    pub fn save(
        &self,
        mut meta: CheckpointV1,
        files: &[FileBundleRecord],
    ) -> Result<(), CheckpointStoreError> {
        fs::create_dir_all(&self.dir).map_err(|source| CheckpointStoreError::Io {
            path: self.dir.clone(),
            source,
        })?;

        let manifest_rel = if meta.manifest.path.is_empty() {
            DEFAULT_MANIFEST_REL_PATH.to_owned()
        } else {
            meta.manifest.path.clone()
        };
        Self::validate_manifest_rel(&manifest_rel)?;
        let manifest_final = self.manifest_path(&manifest_rel);
        let manifest_tmp = self.dir.join(format!("{manifest_rel}.tmp"));

        // Build compressed multi-member gzip NDJSON.
        let compressed = encode_file_bundle(files)?;
        write_atomic(&manifest_tmp, &manifest_final, &compressed)?;

        let digest = sha256_hex_prefixed(&compressed);
        meta.manifest = ManifestPointer::new(manifest_rel, digest, compressed.len() as u64);

        let json = meta.to_json()?;
        let cp_final = self.checkpoint_path();
        let cp_tmp = self.dir.join("checkpoint.json.tmp");
        write_atomic(&cp_tmp, &cp_final, json.as_bytes())?;
        Ok(())
    }

    /// Load metadata and all file records; validates manifest checksum/size.
    ///
    /// # Errors
    ///
    /// Missing files, integrity mismatch, or parse errors.
    pub fn load(&self) -> Result<(CheckpointV1, Vec<FileBundleRecord>), CheckpointStoreError> {
        let cp_path = self.checkpoint_path();
        if !cp_path.is_file() {
            return Err(CheckpointStoreError::NotFound(cp_path));
        }
        let json = fs::read_to_string(&cp_path).map_err(|source| CheckpointStoreError::Io {
            path: cp_path.clone(),
            source,
        })?;
        let meta = CheckpointV1::from_json(&json)?;

        Self::validate_manifest_rel(&meta.manifest.path)?;
        let manifest_path = self.manifest_path(&meta.manifest.path);
        if !manifest_path.is_file() {
            return Err(CheckpointStoreError::NotFound(manifest_path));
        }
        let compressed = fs::read(&manifest_path).map_err(|source| CheckpointStoreError::Io {
            path: manifest_path.clone(),
            source,
        })?;

        let expected = &meta.manifest.sha256;
        let actual = sha256_hex_prefixed(&compressed);
        if actual != *expected {
            return Err(CheckpointStoreError::ManifestIntegrity(format!(
                "sha256 mismatch: expected {expected}, got {actual}"
            )));
        }
        if compressed.len() as u64 != meta.manifest.size {
            return Err(CheckpointStoreError::ManifestIntegrity(format!(
                "size mismatch: expected {}, got {}",
                meta.manifest.size,
                compressed.len()
            )));
        }

        let files = decode_file_bundle(&compressed)?;
        Ok((meta, files))
    }
}

/// Build [`FileBundleRecord`] values from `(path, raw_bytes)` pairs.
///
/// Each body is base64-encoded and hashed with SHA-256 over the raw bytes
/// (ADR 0001). Paths should be relative POSIX inventory paths.
#[must_use]
pub fn records_from_files(entries: &[(&str, &[u8])]) -> Vec<FileBundleRecord> {
    entries
        .iter()
        .map(|(path, raw)| FileBundleRecord::from_raw_bytes(*path, raw, B64.encode(raw)))
        .collect()
}

fn encode_file_bundle(files: &[FileBundleRecord]) -> Result<Vec<u8>, CheckpointStoreError> {
    let mut out = Vec::new();
    for rec in files {
        let line = serde_json::to_string(rec)?;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(line.as_bytes())
            .and_then(|_| encoder.write_all(b"\n"))
            .map_err(|source| CheckpointStoreError::Io {
                path: PathBuf::from("files.ndjson.gz"),
                source,
            })?;
        let member = encoder
            .finish()
            .map_err(|source| CheckpointStoreError::Io {
                path: PathBuf::from("files.ndjson.gz"),
                source,
            })?;
        out.extend_from_slice(&member);
    }
    Ok(out)
}

fn decode_file_bundle(compressed: &[u8]) -> Result<Vec<FileBundleRecord>, CheckpointStoreError> {
    use flate2::bufread::MultiGzDecoder;
    let mut decoder = MultiGzDecoder::new(compressed);
    let mut plain = String::new();
    decoder
        .read_to_string(&mut plain)
        .map_err(|source| CheckpointStoreError::Io {
            path: PathBuf::from("files.ndjson.gz"),
            source,
        })?;
    let mut records = Vec::new();
    for line in plain.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: FileBundleRecord = serde_json::from_str(line)?;
        records.push(rec);
    }
    Ok(records)
}

fn write_atomic(tmp: &Path, final_path: &Path, bytes: &[u8]) -> Result<(), CheckpointStoreError> {
    {
        let mut f = File::create(tmp).map_err(|source| CheckpointStoreError::Io {
            path: tmp.to_path_buf(),
            source,
        })?;
        f.write_all(bytes)
            .map_err(|source| CheckpointStoreError::Io {
                path: tmp.to_path_buf(),
                source,
            })?;
        f.sync_all().map_err(|source| CheckpointStoreError::Io {
            path: tmp.to_path_buf(),
            source,
        })?;
    }
    fs::rename(tmp, final_path).map_err(|source| CheckpointStoreError::Io {
        path: final_path.to_path_buf(),
        source,
    })?;
    // Best-effort dir fsync (may fail on some FS; ignore ErrorKind::Unsupported).
    if let Some(parent) = final_path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use decon_core::{RunConfig, StageId};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Monotonic counter to guarantee unique temp dirs across parallel tests.
    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("decon-checkpoint-store-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_load_round_trip() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut meta = CheckpointV1::new(
            &cfg,
            cfg.redacted_for_checkpoint(),
            "rev1",
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        meta.mark_stage_complete(StageId::Fetch, "2026-07-24T00:01:00Z");

        let files = vec![
            FileBundleRecord::from_raw_bytes("a.txt", b"hello", B64.encode(b"hello")),
            FileBundleRecord::from_raw_bytes("b.rs", b"fn main(){}", B64.encode(b"fn main(){}")),
        ];
        store.save(meta.clone(), &files).unwrap();

        let (loaded, loaded_files) = store.load().unwrap();
        assert_eq!(loaded.version, meta.version);
        assert!(loaded.is_stage_complete(StageId::Fetch));
        assert_eq!(loaded_files.len(), 2);
        assert_eq!(loaded_files[0].path, "a.txt");
        assert_eq!(loaded_files[0].sha256, files[0].sha256);
        assert!(loaded.manifest.size > 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_errors() {
        let dir = temp_dir();
        let err = CheckpointStore::new(&dir).load().unwrap_err();
        assert!(matches!(err, CheckpointStoreError::NotFound(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_manifest_detected() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let meta = CheckpointV1::new(&cfg, cfg.clone(), "r", "t0").unwrap();
        let files = vec![FileBundleRecord::from_raw_bytes(
            "x",
            b"y",
            B64.encode(b"y"),
        )];
        store.save(meta, &files).unwrap();

        // Tamper with gzip file without updating checkpoint.json
        let path = dir.join(DEFAULT_MANIFEST_REL_PATH);
        fs::write(&path, b"not-a-valid-gzip").unwrap();
        let err = store.load().unwrap_err();
        assert!(
            matches!(err, CheckpointStoreError::ManifestIntegrity(_))
                || matches!(err, CheckpointStoreError::Io { .. }),
            "got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
