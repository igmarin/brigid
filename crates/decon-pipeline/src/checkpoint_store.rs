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
    ArchitectureOverview, Chapter, ChapterResult, CheckpointError, CheckpointV1, CombinedTutorial,
    DEFAULT_MANIFEST_REL_PATH, FileBundleRecord, ManifestPointer, SetupGuide, StageId,
    StageOutputEntry, StageOutputs, sha256_hex_prefixed,
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
    /// Stage output file integrity check failed (SHA-256 mismatch or missing).
    #[error("stage output integrity check failed: {0}")]
    StageOutputIntegrity(String),
    /// Stage output file is missing from the checkpoint directory.
    #[error("stage output not found: {0}")]
    StageOutputNotFound(PathBuf),
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

    fn write_stage_file(
        rel_path: &str,
        bytes: &[u8],
        checkpoint_dir: &Path,
    ) -> Result<StageOutputEntry, CheckpointStoreError> {
        let full = checkpoint_dir.join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|source| CheckpointStoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let tmp = full.with_extension("md.tmp");
        write_atomic(&tmp, &full, bytes)?;
        let digest = sha256_hex_prefixed(bytes);
        Ok(StageOutputEntry {
            path: rel_path.to_owned(),
            sha256: digest,
            size: bytes.len() as u64,
        })
    }

    fn read_stage_file(
        entry: &StageOutputEntry,
        checkpoint_dir: &Path,
    ) -> Result<Vec<u8>, CheckpointStoreError> {
        let full = checkpoint_dir.join(&entry.path);
        if !full.is_file() {
            return Err(CheckpointStoreError::StageOutputNotFound(full));
        }
        let bytes = fs::read(&full).map_err(|source| CheckpointStoreError::Io {
            path: full.clone(),
            source,
        })?;
        let actual = sha256_hex_prefixed(&bytes);
        if actual != entry.sha256 {
            return Err(CheckpointStoreError::StageOutputIntegrity(format!(
                "sha256 mismatch for {}: expected {}, got {}",
                entry.path, entry.sha256, actual
            )));
        }
        if bytes.len() as u64 != entry.size {
            return Err(CheckpointStoreError::StageOutputIntegrity(format!(
                "size mismatch for {}: expected {}, got {}",
                entry.path,
                entry.size,
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    fn ensure_stage_outputs(cp: &mut CheckpointV1) -> &mut StageOutputs {
        cp.stage_outputs.get_or_insert_with(StageOutputs::new)
    }

    fn persist_checkpoint(&self, cp: &CheckpointV1) -> Result<(), CheckpointStoreError> {
        let (_, files) = self.load()?;
        self.save(cp.clone(), &files)
    }

    /// Build the chapter filename: `chapters/NN_<slug>.md` where `NN` is the
    /// zero-padded 2-digit chapter number and `<slug>` is the kebab-case slug
    /// of the title (max 50 chars).
    fn chapter_rel_path(chapter: &Chapter) -> String {
        let slug = slugify(&chapter.title);
        format!("chapters/{:02}_{slug}.md", chapter.chapter_num)
    }

    /// Write all chapters as individual files under `chapters/` inside the
    /// checkpoint directory and record them in the stage-output manifest.
    ///
    /// Each chapter is written as `chapters/NN_<slug>.md`. The manifest
    /// entries are stored in `checkpoint.stage_outputs["chapters"]`.
    ///
    /// # Errors
    ///
    /// Returns I/O or serialization errors.
    pub fn write_chapters(
        &self,
        checkpoint_dir: &Path,
        chapters: &ChapterResult,
    ) -> Result<Vec<StageOutputEntry>, CheckpointStoreError> {
        let mut entries = Vec::with_capacity(chapters.chapters.len());
        for ch in &chapters.chapters {
            let rel = Self::chapter_rel_path(ch);
            let bytes = chapter_to_bytes(ch);
            let entry = Self::write_stage_file(&rel, &bytes, checkpoint_dir)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Write a single chapter file, overwriting any existing file for the
    /// same chapter number. Returns the manifest entry for the written file.
    ///
    /// # Errors
    ///
    /// Returns I/O errors.
    pub fn write_chapter_single(
        &self,
        checkpoint_dir: &Path,
        chapter: &Chapter,
    ) -> Result<StageOutputEntry, CheckpointStoreError> {
        let rel = Self::chapter_rel_path(chapter);
        let bytes = chapter_to_bytes(chapter);
        Self::write_stage_file(&rel, &bytes, checkpoint_dir)
    }

    /// Read all chapters from `chapters/*.md` inside the checkpoint directory.
    ///
    /// Verifies SHA-256 of each file against the manifest entries in
    /// `checkpoint.stage_outputs["chapters"]`. Files are sorted by chapter
    /// number before being returned.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointStoreError::StageOutputIntegrity`] if a file is
    /// corrupt, or I/O errors if files cannot be read.
    pub fn read_chapters(
        &self,
        checkpoint_dir: &Path,
        cp: &CheckpointV1,
    ) -> Result<ChapterResult, CheckpointStoreError> {
        let entries = cp
            .stage_outputs
            .as_ref()
            .and_then(|so| so.get(StageId::Chapters.as_str()))
            .unwrap_or(&[]);
        if entries.is_empty() {
            return Ok(ChapterResult::new(Vec::new()));
        }
        let mut chapters = Vec::with_capacity(entries.len());
        for entry in entries {
            let bytes = Self::read_stage_file(entry, checkpoint_dir)?;
            let ch = chapter_from_bytes(&bytes)?;
            chapters.push(ch);
        }
        chapters.sort_by_key(|c| c.chapter_num);
        Ok(ChapterResult::new(chapters))
    }

    /// Write the setup guide as `00_setup.md` inside the checkpoint directory.
    ///
    /// # Errors
    ///
    /// Returns I/O errors.
    pub fn write_setup_guide(
        &self,
        checkpoint_dir: &Path,
        guide: &SetupGuide,
    ) -> Result<StageOutputEntry, CheckpointStoreError> {
        let bytes = guide.markdown.as_bytes();
        Self::write_stage_file("00_setup.md", bytes, checkpoint_dir)
    }

    /// Read the setup guide from `00_setup.md` if present.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointStoreError::StageOutputIntegrity`] if the file is
    /// corrupt, or `Ok(None)` if no setup guide was recorded.
    pub fn read_setup_guide(
        &self,
        checkpoint_dir: &Path,
        cp: &CheckpointV1,
    ) -> Result<Option<SetupGuide>, CheckpointStoreError> {
        let entry = cp
            .stage_outputs
            .as_ref()
            .and_then(|so| so.get(StageId::Setup.as_str()))
            .and_then(|e| e.first());
        let Some(entry) = entry else {
            return Ok(None);
        };
        let bytes = Self::read_stage_file(entry, checkpoint_dir)?;
        let markdown = String::from_utf8(bytes).map_err(|e| {
            CheckpointStoreError::StageOutputIntegrity(format!("setup guide UTF-8: {e}"))
        })?;
        Ok(Some(SetupGuide::new(markdown, 0, Vec::new(), false)))
    }

    /// Write the architecture overview as `00_architecture_overview.md`.
    ///
    /// # Errors
    ///
    /// Returns I/O errors.
    pub fn write_architecture_overview(
        &self,
        checkpoint_dir: &Path,
        overview: &ArchitectureOverview,
    ) -> Result<StageOutputEntry, CheckpointStoreError> {
        let bytes = overview.markdown.as_bytes();
        Self::write_stage_file("00_architecture_overview.md", bytes, checkpoint_dir)
    }

    /// Read the architecture overview if present.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointStoreError::StageOutputIntegrity`] if the file is
    /// corrupt, or `Ok(None)` if none was recorded.
    pub fn read_architecture_overview(
        &self,
        checkpoint_dir: &Path,
        cp: &CheckpointV1,
    ) -> Result<Option<ArchitectureOverview>, CheckpointStoreError> {
        let entry = cp
            .stage_outputs
            .as_ref()
            .and_then(|so| so.get(StageId::Overview.as_str()))
            .and_then(|e| e.first());
        let Some(entry) = entry else {
            return Ok(None);
        };
        let bytes = Self::read_stage_file(entry, checkpoint_dir)?;
        let markdown = String::from_utf8(bytes).map_err(|e| {
            CheckpointStoreError::StageOutputIntegrity(format!("overview UTF-8: {e}"))
        })?;
        Ok(Some(ArchitectureOverview::new(markdown, Vec::new())))
    }

    /// Write the combined index as `index.md`.
    ///
    /// # Errors
    ///
    /// Returns I/O errors.
    pub fn write_combined_index(
        &self,
        checkpoint_dir: &Path,
        tutorial: &CombinedTutorial,
    ) -> Result<StageOutputEntry, CheckpointStoreError> {
        let bytes = tutorial.index_markdown.as_bytes();
        Self::write_stage_file("index.md", bytes, checkpoint_dir)
    }

    /// Read the combined index if present.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointStoreError::StageOutputIntegrity`] if the file is
    /// corrupt, or `Ok(None)` if none was recorded.
    pub fn read_combined_index(
        &self,
        checkpoint_dir: &Path,
        cp: &CheckpointV1,
    ) -> Result<Option<CombinedTutorial>, CheckpointStoreError> {
        let entry = cp
            .stage_outputs
            .as_ref()
            .and_then(|so| so.get(StageId::Combine.as_str()))
            .and_then(|e| e.first());
        let Some(entry) = entry else {
            return Ok(None);
        };
        let bytes = Self::read_stage_file(entry, checkpoint_dir)?;
        let markdown = String::from_utf8(bytes)
            .map_err(|e| CheckpointStoreError::StageOutputIntegrity(format!("index UTF-8: {e}")))?;
        Ok(Some(CombinedTutorial::new(
            markdown,
            0,
            false,
            false,
            String::new(),
        )))
    }

    /// Check whether a stage is complete **and** its output files are present
    /// and intact in the checkpoint directory.
    ///
    /// Unlike [`CheckpointV1::is_stage_complete`], this also verifies that the
    /// file-based outputs for M4 stages (chapters, setup, overview, combine)
    /// exist on disk and match their recorded SHA-256 digests.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointStoreError::StageOutputIntegrity`] if a file is
    /// missing or corrupt, or `Ok(false)` if the stage is not marked complete.
    pub fn is_stage_complete_with_files(
        &self,
        cp: &CheckpointV1,
        stage: StageId,
    ) -> Result<bool, CheckpointStoreError> {
        if !cp.is_stage_complete(stage) {
            return Ok(false);
        }
        let stage_name = stage.as_str();
        let entries = cp
            .stage_outputs
            .as_ref()
            .and_then(|so| so.get(stage_name))
            .unwrap_or(&[]);
        if entries.is_empty() {
            return Ok(false);
        }
        for entry in entries {
            let full = self.dir.join(&entry.path);
            if !full.is_file() {
                return Ok(false);
            }
            let bytes = fs::read(&full).map_err(|source| CheckpointStoreError::Io {
                path: full.clone(),
                source,
            })?;
            let actual = sha256_hex_prefixed(&bytes);
            if actual != entry.sha256 {
                return Err(CheckpointStoreError::StageOutputIntegrity(format!(
                    "sha256 mismatch for {}: expected {}, got {}",
                    entry.path, entry.sha256, actual
                )));
            }
        }
        Ok(true)
    }

    /// Record stage output entries in the checkpoint's stage-output manifest
    /// and persist the updated checkpoint to disk.
    ///
    /// # Errors
    ///
    /// Returns I/O or serialization errors.
    pub fn record_stage_outputs(
        &self,
        cp: &mut CheckpointV1,
        stage: StageId,
        entries: Vec<StageOutputEntry>,
    ) -> Result<(), CheckpointStoreError> {
        if entries.is_empty() {
            Self::ensure_stage_outputs(cp).remove(stage.as_str());
        } else {
            Self::ensure_stage_outputs(cp).set(stage.as_str(), entries);
        }
        self.persist_checkpoint(cp)
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

fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        return "chapter".to_owned();
    }
    let mut result = String::new();
    let mut count = 0;
    for ch in trimmed.chars() {
        if count >= 50 {
            break;
        }
        result.push(ch);
        count += ch.len_utf8();
    }
    result.trim_matches('-').to_owned()
}

fn chapter_to_bytes(ch: &Chapter) -> Vec<u8> {
    serde_json::to_vec(ch).unwrap_or_else(|_| Vec::new())
}

fn chapter_from_bytes(bytes: &[u8]) -> Result<Chapter, CheckpointStoreError> {
    serde_json::from_slice::<Chapter>(bytes).map_err(|e| {
        CheckpointStoreError::StageOutputIntegrity(format!("chapter parse error: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use decon_core::{RunConfig, StageId, Tier};
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

    fn seed_checkpoint(store: &CheckpointStore) -> CheckpointV1 {
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.clone(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}")]);
        store.save(cp.clone(), &files).unwrap();
        cp
    }

    fn sample_chapters() -> ChapterResult {
        ChapterResult::new(vec![
            Chapter::new(
                0,
                1,
                "Intro",
                "# Intro\n\nHello",
                Tier::S,
                "module",
                "footer 0",
            ),
            Chapter::new(
                1,
                2,
                "Core API",
                "# Core API\n\nWorld",
                Tier::M,
                "class",
                "footer 1",
            ),
            Chapter::new(
                2,
                3,
                "Advanced Topics",
                "# Advanced\n\nDone",
                Tier::L,
                "function",
                "footer 2",
            ),
        ])
    }

    #[test]
    fn write_read_chapters_round_trip() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let chapters = sample_chapters();
        let entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");
        let entries_clone = cp
            .stage_outputs
            .as_ref()
            .unwrap()
            .get(StageId::Chapters.as_str())
            .unwrap()
            .to_vec();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries_clone)
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store.read_chapters(&dir, &loaded_cp).unwrap();
        assert_eq!(loaded.chapters.len(), 3);
        assert_eq!(loaded.chapters[0].title, "Intro");
        assert_eq!(loaded.chapters[1].title, "Core API");
        assert_eq!(loaded.chapters[2].title, "Advanced Topics");
        assert_eq!(loaded.chapters[0].markdown, "# Intro\n\nHello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_read_setup_guide_round_trip() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let guide = SetupGuide::new("# Setup\n\nInstall Rust", 42, vec!["gap".into()], true);
        let entry = store.write_setup_guide(&dir, &guide).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Setup, vec![entry])
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store.read_setup_guide(&dir, &loaded_cp).unwrap().unwrap();
        assert_eq!(loaded.markdown, "# Setup\n\nInstall Rust");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_read_architecture_overview_round_trip() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let overview = ArchitectureOverview::new("# Architecture\n\nOverview", vec!["app1".into()]);
        let entry = store.write_architecture_overview(&dir, &overview).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Overview, vec![entry])
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store
            .read_architecture_overview(&dir, &loaded_cp)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.markdown, "# Architecture\n\nOverview");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_read_combined_index_round_trip() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let tutorial = CombinedTutorial::new("# Index\n\n## Chapters", 3, true, false, "en");
        let entry = store.write_combined_index(&dir, &tutorial).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Combine, vec![entry])
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store
            .read_combined_index(&dir, &loaded_cp)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.index_markdown, "# Index\n\n## Chapters");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_chapter_file_detected() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let chapters = sample_chapters();
        let entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let first_entry = loaded_cp
            .stage_outputs
            .as_ref()
            .unwrap()
            .get(StageId::Chapters.as_str())
            .unwrap()[0]
            .clone();
        fs::write(dir.join(&first_entry.path), b"corrupted").unwrap();
        let err = store.read_chapters(&dir, &loaded_cp).unwrap_err();
        assert!(
            matches!(err, CheckpointStoreError::StageOutputIntegrity(_)),
            "got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_detection_files_present() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let chapters = sample_chapters();
        let entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");
        let entries_clone = cp
            .stage_outputs
            .as_ref()
            .unwrap()
            .get(StageId::Chapters.as_str())
            .unwrap()
            .to_vec();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries_clone)
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        assert!(
            store
                .is_stage_complete_with_files(&loaded_cp, StageId::Chapters)
                .unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_detection_files_missing() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let chapters = sample_chapters();
        let entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");
        let entries_clone = cp
            .stage_outputs
            .as_ref()
            .unwrap()
            .get(StageId::Chapters.as_str())
            .unwrap()
            .to_vec();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries_clone)
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let chapter_files: Vec<_> = fs::read_dir(dir.join("chapters"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        for f in &chapter_files {
            fs::remove_file(f).unwrap();
        }
        let complete = store
            .is_stage_complete_with_files(&loaded_cp, StageId::Chapters)
            .unwrap();
        assert!(!complete);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_regeneration_overwrite_chapter() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let chapters = sample_chapters();
        let entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();

        let updated = Chapter::new(
            1,
            2,
            "Core API",
            "# Core API\n\nUPDATED CONTENT",
            Tier::M,
            "class",
            "footer 1 updated",
        );
        let new_entry = store.write_chapter_single(&dir, &updated).unwrap();
        let mut cp2 = store.load().unwrap().0;
        let existing = cp2
            .stage_outputs
            .as_mut()
            .unwrap()
            .entries
            .get_mut(StageId::Chapters.as_str())
            .unwrap();
        let pos = existing
            .iter()
            .position(|e| e.path == new_entry.path)
            .unwrap();
        existing[pos] = new_entry;
        let entries_clone = cp2
            .stage_outputs
            .as_ref()
            .unwrap()
            .get(StageId::Chapters.as_str())
            .unwrap()
            .to_vec();
        store
            .record_stage_outputs(&mut cp2, StageId::Chapters, entries_clone)
            .unwrap();

        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store.read_chapters(&dir, &loaded_cp).unwrap();
        assert_eq!(loaded.chapters.len(), 3);
        assert_eq!(loaded.chapters[1].markdown, "# Core API\n\nUPDATED CONTENT");
        assert_eq!(loaded.chapters[0].markdown, "# Intro\n\nHello");
        assert_eq!(loaded.chapters[2].markdown, "# Advanced\n\nDone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_chapters_writes_no_files() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let mut cp = seed_checkpoint(&store);
        let empty = ChapterResult::new(Vec::new());
        let entries = store.write_chapters(&dir, &empty).unwrap();
        assert!(entries.is_empty());
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, entries)
            .unwrap();
        assert!(!dir.join("chapters").exists());
        let (loaded_cp, _) = store.load().unwrap();
        let loaded = store.read_chapters(&dir, &loaded_cp).unwrap();
        assert!(loaded.chapters.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chapter_filename_convention() {
        let ch = Chapter::new(0, 1, "Query Processing!", "md", Tier::S, "module", "f");
        let rel = CheckpointStore::chapter_rel_path(&ch);
        assert_eq!(rel, "chapters/01_query-processing.md");

        let ch2 = Chapter::new(1, 12, "A Very Long Title", "md", Tier::L, "class", "f");
        let rel2 = CheckpointStore::chapter_rel_path(&ch2);
        assert!(rel2.starts_with("chapters/12_"));
        assert!(rel2.ends_with(".md"));
    }

    #[test]
    fn read_setup_guide_none_when_absent() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        seed_checkpoint(&store);
        let (loaded_cp, _) = store.load().unwrap();
        let result = store.read_setup_guide(&dir, &loaded_cp).unwrap();
        assert!(result.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_architecture_overview_none_when_absent() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        seed_checkpoint(&store);
        let (loaded_cp, _) = store.load().unwrap();
        let result = store.read_architecture_overview(&dir, &loaded_cp).unwrap();
        assert!(result.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_combined_index_none_when_absent() {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        seed_checkpoint(&store);
        let (loaded_cp, _) = store.load().unwrap();
        let result = store.read_combined_index(&dir, &loaded_cp).unwrap();
        assert!(result.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Query Processing!"), "query-processing");
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify("---"), "chapter");
        assert_eq!(slugify("a_b c"), "a-b-c");
    }

    #[test]
    fn slugify_max_50_chars() {
        let long = "a".repeat(100);
        let s = slugify(&long);
        assert!(s.len() <= 50);
    }
}
