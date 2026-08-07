//! Checkpoint loading for the MCP server.
//!
//! [`CheckpointLoader`] reads a `brigid generate` checkpoint directory into a
//! [`CheckpointData`] struct held in memory for the lifetime of the MCP server.
//! It reuses [`brigid_pipeline::CheckpointStore`] for the on-disk format
//! (ADR 0001 + ADR 0006): `checkpoint.json` metadata, the compressed
//! `files.ndjson.gz` file bundle, and the file-based stage outputs
//! (chapters, setup, overview, combine).
//!
//! Missing stages are represented as `None` fields, so a partially completed
//! checkpoint loads gracefully — the server can expose whatever data is
//! available without erroring on stages that have not yet run.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use brigid_core::{
    ArchitectureOverview, ChapterOrder, ChapterResult, CheckpointV1, CombinedTutorial,
    FileBundleRecord, IdentifyResult, RelationshipsResult, SetupGuide, StageId,
};
use brigid_pipeline::{CheckpointStore, CheckpointStoreError};
use thiserror::Error;

/// A single file from the crawl inventory, decoded for serving.
///
/// Carries the relative POSIX path and the raw (decoded) byte size. The
/// base64-encoded body from [`FileBundleRecord`] is decoded once at load time
/// so the MCP server never re-decodes on every request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    /// Relative repository path (POSIX `/` separators).
    pub path: String,
    /// Raw (decoded) byte size of the file content.
    pub size: u64,
}

impl FileEntry {
    /// Construct a file entry from a path and a raw byte size.
    #[must_use]
    pub fn new(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
        }
    }

    /// Build a [`FileEntry`] from a checkpoint [`FileBundleRecord`], decoding
    /// the base64 content to determine the raw byte size.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointLoaderError::Decode`] if the recorded content is
    /// not valid base64.
    pub fn from_record(rec: &FileBundleRecord) -> Result<Self, CheckpointLoaderError> {
        let raw = B64
            .decode(&rec.content)
            .map_err(|e| CheckpointLoaderError::Decode {
                path: rec.path.clone(),
                source: e,
            })?;
        Ok(Self::new(rec.path.clone(), raw.len() as u64))
    }
}

/// Loaded checkpoint data ready to serve via MCP.
///
/// Every stage output field is [`Option`]: `None` when the stage has not yet
/// completed (or its output is absent from the checkpoint directory). The
/// [`CheckpointV1`] metadata and file inventory are always present after a
/// successful load.
#[derive(Clone, Debug)]
pub struct CheckpointData {
    /// The checkpoint metadata (config, completed stages, git commit).
    pub checkpoint: CheckpointV1,
    /// Identify result (abstractions), if the identify stage completed.
    pub abstractions: Option<IdentifyResult>,
    /// Relationships result, if the relationships stage completed.
    pub relationships: Option<RelationshipsResult>,
    /// Chapter order, if the order stage completed.
    pub chapter_order: Option<ChapterOrder>,
    /// Chapter results, if the chapters stage completed.
    pub chapters: Option<ChapterResult>,
    /// Setup guide, if the setup stage completed.
    pub setup_guide: Option<SetupGuide>,
    /// Architecture overview, if the overview stage completed.
    pub overview: Option<ArchitectureOverview>,
    /// Combined tutorial, if the combine stage completed.
    pub combined: Option<CombinedTutorial>,
    /// File inventory from the crawl.
    pub files: Vec<FileEntry>,
}

/// Errors while loading a checkpoint directory for the MCP server.
#[derive(Debug, Error)]
pub enum CheckpointLoaderError {
    /// Underlying checkpoint store error (missing files, integrity mismatch,
    /// parse failure).
    #[error(transparent)]
    Store(#[from] CheckpointStoreError),
    /// A stage output stored as opaque JSON in `checkpoint.json` could not be
    /// deserialized into its typed domain struct.
    #[error("checkpoint deserialization failed for stage {stage}: {source}")]
    Deserialize {
        /// Stage wire name (e.g. `"identify"`, `"order"`).
        stage: String,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// A file-bundle record's base64 content could not be decoded.
    #[error("failed to decode file content for {path}: {source}")]
    Decode {
        /// Relative path of the file whose content failed to decode.
        path: String,
        /// Underlying base64 decode error.
        #[source]
        source: base64::DecodeError,
    },
}

/// Loads a brigid checkpoint directory into memory for the MCP server.
///
/// Wraps [`brigid_pipeline::CheckpointStore`], adding the higher-level
/// deserialization of opaque JSON stage outputs (`abstractions`,
/// `relationships`, `order`) into typed domain structs and the decoding of
/// file-bundle records into [`FileEntry`] values.
#[derive(Clone, Debug)]
pub struct CheckpointLoader {
    /// Root directory containing `checkpoint.json` and the manifest bundle.
    pub dir: PathBuf,
}

impl CheckpointLoader {
    /// Create a loader rooted at `dir`.
    ///
    /// The directory must exist and contain a valid `checkpoint.json` when
    /// [`CheckpointLoader::load`] is called.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Load the checkpoint directory into a [`CheckpointData`] struct.
    ///
    /// Reads `checkpoint.json` and the compressed file bundle via
    /// [`CheckpointStore::load`], then deserializes the opaque JSON stage
    /// outputs and reads the file-based stage outputs (chapters, setup,
    /// overview, combine). Stages that have not completed are left as `None`.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointLoaderError::Store`] if the checkpoint directory is
    /// missing, corrupt, or fails an integrity check. Returns
    /// [`CheckpointLoaderError::Deserialize`] if a stage output's JSON cannot
    /// be parsed into its typed struct. Returns
    /// [`CheckpointLoaderError::Decode`] if a file-bundle record's base64
    /// content is invalid.
    pub fn load(&self) -> Result<CheckpointData, CheckpointLoaderError> {
        let store = CheckpointStore::new(&self.dir);
        let (cp, file_records) = store.load()?;

        let abstractions = cp
            .abstractions
            .as_ref()
            .map(|v| IdentifyResult::from_checkpoint_value(v.clone()))
            .transpose()
            .map_err(|source| CheckpointLoaderError::Deserialize {
                stage: StageId::Identify.as_str().to_owned(),
                source,
            })?;

        let relationships = cp
            .relationships
            .as_ref()
            .map(|v| RelationshipsResult::from_checkpoint_value(v.clone()))
            .transpose()
            .map_err(|source| CheckpointLoaderError::Deserialize {
                stage: StageId::Relationships.as_str().to_owned(),
                source,
            })?;

        let chapter_order = cp
            .order
            .as_ref()
            .map(|v| ChapterOrder::from_checkpoint_value(v.clone()))
            .transpose()
            .map_err(|source| CheckpointLoaderError::Deserialize {
                stage: StageId::Order.as_str().to_owned(),
                source,
            })?;

        // File-based stage outputs: only attempt to read stages that are
        // marked complete so incomplete checkpoints do not surface empty
        // results as if the stage ran.
        let chapters = if cp.is_stage_complete(StageId::Chapters) {
            store
                .read_chapters(&self.dir, &cp)
                .map(Some)
                .unwrap_or(None)
        } else {
            None
        };

        let setup_guide = if cp.is_stage_complete(StageId::Setup) {
            store.read_setup_guide(&self.dir, &cp).ok().flatten()
        } else {
            None
        };

        let overview = if cp.is_stage_complete(StageId::Overview) {
            store
                .read_architecture_overview(&self.dir, &cp)
                .ok()
                .flatten()
        } else {
            None
        };

        let combined = if cp.is_stage_complete(StageId::Combine) {
            store.read_combined_index(&self.dir, &cp).ok().flatten()
        } else {
            None
        };

        let files = file_records
            .iter()
            .map(FileEntry::from_record)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CheckpointData {
            checkpoint: cp,
            abstractions,
            relationships,
            chapter_order,
            chapters,
            setup_guide,
            overview,
            combined,
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::{Abstraction, Chapter, RunConfig, StageId, Tier};
    use brigid_pipeline::records_from_files;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Monotonic counter to guarantee unique temp dirs across parallel tests.
    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Create a unique temp directory for a test checkpoint.
    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-mcp-test-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Seed a minimal valid checkpoint directory (fetch stage complete) with
    /// two files. Returns the directory path.
    fn seed_minimal_checkpoint() -> PathBuf {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}"), ("b.txt", b"hello world")]);
        store.save(cp, &files).unwrap();
        dir
    }

    /// Seed a fully populated checkpoint with all stages complete.
    fn seed_full_checkpoint() -> PathBuf {
        let dir = temp_dir();
        let store = CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}")]);
        store.save(cp.clone(), &files).unwrap();

        // Identify stage.
        let identify = IdentifyResult::new(vec![
            Abstraction::new("Core", "The core system", Tier::M, "module"),
            Abstraction::new("Routing", "Routes requests", Tier::S, "class"),
        ]);
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());

        // Relationships stage.
        let relationships = RelationshipsResult::new(
            "A small web framework.",
            vec![brigid_core::Relationship::new(0, 1, "routes to", "calls")],
        );
        cp.relationships = Some(relationships.to_checkpoint_value().unwrap());

        // Order stage.
        let order = ChapterOrder::new(vec![1, 0]);
        cp.order = Some(order.to_checkpoint_value().unwrap());

        // Chapters stage (file-based).
        let chapters = ChapterResult::new(vec![Chapter::new(
            0,
            1,
            "Intro",
            "# Intro\n\nHello",
            Tier::S,
            "module",
            "footer 0",
        )]);
        let chapter_entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, chapter_entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");

        // Setup stage (file-based).
        let guide = SetupGuide::new("# Setup\n\nInstall Rust", 42, vec!["gap".into()], true);
        let setup_entry = store.write_setup_guide(&dir, &guide).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Setup, vec![setup_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Setup, "t3");

        // Overview stage (file-based).
        let overview = ArchitectureOverview::new("# Architecture\n", vec!["app1".into()]);
        let overview_entry = store.write_architecture_overview(&dir, &overview).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Overview, vec![overview_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Overview, "t4");

        // Combine stage (file-based).
        let tutorial = CombinedTutorial::new("# Index\n", 1, true, true, "en");
        let combine_entry = store.write_combined_index(&dir, &tutorial).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Combine, vec![combine_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Combine, "t5");

        // Persist the final metadata (with all stage outputs + completion).
        store.save(cp, &files).unwrap();

        dir
    }

    #[test]
    fn load_valid_full_checkpoint() {
        let dir = seed_full_checkpoint();
        let loader = CheckpointLoader::new(&dir);
        let data = loader.load().expect("full checkpoint should load");

        assert_eq!(data.checkpoint.version, 1);
        assert!(data.checkpoint.is_stage_complete(StageId::Fetch));
        assert!(data.checkpoint.is_stage_complete(StageId::Chapters));

        let abs = data.abstractions.expect("abstractions present");
        assert_eq!(abs.abstractions.len(), 2);
        assert_eq!(abs.abstractions[0].name, "Core");

        let rel = data.relationships.expect("relationships present");
        assert_eq!(rel.relationships.len(), 1);
        assert_eq!(rel.project_summary, "A small web framework.");

        let order = data.chapter_order.expect("chapter order present");
        assert_eq!(order.ordered_indices, vec![1, 0]);

        let chapters = data.chapters.expect("chapters present");
        assert_eq!(chapters.chapters.len(), 1);
        assert_eq!(chapters.chapters[0].title, "Intro");
        assert_eq!(chapters.chapters[0].markdown, "# Intro\n\nHello");

        let guide = data.setup_guide.expect("setup guide present");
        assert_eq!(guide.markdown, "# Setup\n\nInstall Rust");

        let overview = data.overview.expect("overview present");
        assert_eq!(overview.markdown, "# Architecture\n");

        let combined = data.combined.expect("combined present");
        assert_eq!(combined.index_markdown, "# Index\n");

        assert_eq!(data.files.len(), 1);
        assert_eq!(data.files[0].path, "a.rs");
        assert_eq!(data.files[0].size, 9);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_checkpoint_directory_errors() {
        let dir = temp_dir();
        // Empty directory — no checkpoint.json.
        let loader = CheckpointLoader::new(&dir);
        let err = loader.load().unwrap_err();
        assert!(
            matches!(err, CheckpointLoaderError::Store(_)),
            "missing checkpoint should give Store error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_checkpoint_errors() {
        let dir = seed_minimal_checkpoint();
        // Truncate checkpoint.json to invalid JSON.
        let cp_path = dir.join("checkpoint.json");
        fs::write(&cp_path, b"{ not valid json").unwrap();
        let loader = CheckpointLoader::new(&dir);
        let err = loader.load().unwrap_err();
        assert!(
            matches!(err, CheckpointLoaderError::Store(_)),
            "corrupt checkpoint.json should give Store error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_incomplete_checkpoint_has_none_for_missing_stages() {
        let dir = seed_minimal_checkpoint();
        let loader = CheckpointLoader::new(&dir);
        let data = loader.load().expect("minimal checkpoint should load");

        // Only fetch is complete; all stage outputs should be None.
        assert!(data.abstractions.is_none());
        assert!(data.relationships.is_none());
        assert!(data.chapter_order.is_none());
        assert!(data.chapters.is_none());
        assert!(data.setup_guide.is_none());
        assert!(data.overview.is_none());
        assert!(data.combined.is_none());

        // File inventory is still present.
        assert_eq!(data.files.len(), 2);
        assert_eq!(data.files[0].path, "a.rs");
        assert_eq!(data.files[1].path, "b.txt");
        assert_eq!(data.files[1].size, 11);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_entry_from_record_decodes_size() {
        let rec = FileBundleRecord::from_raw_bytes("x.txt", b"hello", B64.encode(b"hello"));
        let entry = FileEntry::from_record(&rec).unwrap();
        assert_eq!(entry.path, "x.txt");
        assert_eq!(entry.size, 5);
    }
}
