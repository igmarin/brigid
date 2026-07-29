//! Disk response cache for LLM calls (structure only — no network).
//!
//! Keys are stable SHA-256 digests of the canonical JSON object
//! `{ "prompt", "model", "provider", "extras" }`. Values are opaque UTF-8
//! response bodies stored as files under a cache root.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use thiserror::Error;

/// Errors from cache keying or filesystem operations.
#[derive(Debug, Error)]
pub enum CacheError {
    /// Failed to serialize the key material.
    #[error("cache key serialization failed: {0}")]
    Key(String),
    /// Filesystem I/O failure.
    #[error("cache I/O at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// Inputs that uniquely identify a cached LLM response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CacheKeyInput<'a> {
    /// Full prompt text (or rendered template).
    pub prompt: &'a str,
    /// Model identifier.
    pub model: &'a str,
    /// Provider identifier (e.g. `openai`, `anthropic`).
    pub provider: &'a str,
    /// Optional extra dimensions (temperature, tools hash, …) as a stable string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extras: Option<&'a str>,
}

/// Compute `sha256:<hex>` cache key for the given inputs.
///
/// # Errors
///
/// Returns [`CacheError::Key`] if JSON serialization fails.
pub fn cache_key(input: &CacheKeyInput<'_>) -> Result<String, CacheError> {
    let value = serde_json::json!({
        "extras": input.extras,
        "model": input.model,
        "prompt": input.prompt,
        "provider": input.provider,
    });
    let s = serde_json::to_string(&value).map_err(|e| CacheError::Key(e.to_string()))?;
    let digest = Sha256::digest(s.as_bytes());
    Ok(format!("sha256:{}", hex::encode(digest)))
}

/// Cache operation statistics, shared across clones of a [`DiskCache`].
#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    /// Number of cache hits (lookups that found an entry).
    pub hits: u64,
    /// Number of cache misses (lookups that found no entry).
    pub misses: u64,
    /// Number of entries evicted by LRU enforcement.
    pub evictions: u64,
    /// Total size of cache files in bytes (updated during eviction checks).
    pub current_size_bytes: u64,
    /// Internal write counter for periodic eviction checks.
    write_count: u64,
}

/// Number of writes between automatic eviction checks.
const EVICTION_CHECK_INTERVAL: u64 = 50;

/// Filesystem-backed response cache with optional size limits and LRU eviction.
#[derive(Clone, Debug)]
pub struct DiskCache {
    /// Root directory for cache entries.
    pub root: PathBuf,
    size_limit_bytes: u64,
    stats: Arc<Mutex<CacheStats>>,
}

impl DiskCache {
    /// Create a cache rooted at `root` with no size limit (created on first
    /// put if missing).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            size_limit_bytes: u64::MAX,
            stats: Arc::new(Mutex::new(CacheStats::default())),
        }
    }

    /// Create a cache with a size limit specified in megabytes.
    #[must_use]
    pub fn with_size_limit(root: impl Into<PathBuf>, limit_mb: usize) -> Self {
        Self::with_size_limit_bytes(root, (limit_mb as u64).saturating_mul(1024 * 1024))
    }

    /// Create a cache with a size limit specified in bytes.
    #[must_use]
    pub fn with_size_limit_bytes(root: impl Into<PathBuf>, limit_bytes: u64) -> Self {
        Self {
            root: root.into(),
            size_limit_bytes: limit_bytes,
            stats: Arc::new(Mutex::new(CacheStats::default())),
        }
    }

    /// Return the configured size limit in bytes.
    #[must_use]
    pub fn size_limit_bytes(&self) -> u64 {
        self.size_limit_bytes
    }

    /// Return a snapshot of the current cache statistics.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        self.stats
            .lock()
            .expect("cache stats mutex poisoned")
            .clone()
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        let name = key.strip_prefix("sha256:").unwrap_or(key);
        self.root.join(format!("{name}.json"))
    }

    /// Look up a cached response body by key.
    ///
    /// Updates the file's modification time on hit so LRU eviction can use
    /// mtime as the access-time proxy.
    ///
    /// # Errors
    ///
    /// I/O errors other than not-found. Missing entries return `Ok(None)`.
    pub async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let path = self.entry_path(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => {
                {
                    let mut st = self.stats.lock().expect("cache stats mutex poisoned");
                    st.hits += 1;
                }
                if let Ok(file) = tokio::fs::OpenOptions::new().write(true).open(&path).await {
                    let _ = file.into_std().await.set_modified(SystemTime::now());
                }
                Ok(Some(s))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                {
                    let mut st = self.stats.lock().expect("cache stats mutex poisoned");
                    st.misses += 1;
                }
                Ok(None)
            }
            Err(source) => {
                {
                    let mut st = self.stats.lock().expect("cache stats mutex poisoned");
                    st.misses += 1;
                }
                Err(CacheError::Io { path, source })
            }
        }
    }

    /// Store a response body under `key`.
    ///
    /// The cache directory is created lazily on the first write. Every 50
    /// writes, the size limit is enforced via
    /// [`DiskCache::enforce_size_limit`].
    ///
    /// # Errors
    ///
    /// Filesystem failures creating the root or writing the entry.
    pub async fn put(&self, key: &str, body: &str) -> Result<(), CacheError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|source| CacheError::Io {
                path: self.root.clone(),
                source,
            })?;
        let path = self.entry_path(key);
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, body)
            .await
            .map_err(|source| CacheError::Io {
                path: tmp.clone(),
                source,
            })?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|source| CacheError::Io {
                path: path.clone(),
                source,
            })?;
        let should_check = {
            let mut st = self.stats.lock().expect("cache stats mutex poisoned");
            st.write_count += 1;
            st.write_count % EVICTION_CHECK_INTERVAL == 0
        };
        if should_check {
            let _ = self.enforce_size_limit().await;
        }
        Ok(())
    }

    /// Enforce the size limit by evicting least-recently-accessed entries
    /// (oldest mtime first). Returns the number of bytes evicted.
    ///
    /// No-op when the size limit is `u64::MAX` (unlimited). Errors during
    /// individual file removal are ignored (best-effort eviction).
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Io`] only if the cache directory cannot be read.
    pub async fn enforce_size_limit(&self) -> Result<u64, CacheError> {
        if self.size_limit_bytes == u64::MAX {
            return Ok(0);
        }
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(source) => {
                return Err(CacheError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };

        let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
        while let Ok(Some(ent)) = entries.next_entry().await {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let md = match ent.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((path, mtime, md.len()));
        }

        let total_size: u64 = files.iter().map(|(_, _, sz)| *sz).sum();
        {
            let mut st = self.stats.lock().expect("cache stats mutex poisoned");
            st.current_size_bytes = total_size;
        }

        if total_size <= self.size_limit_bytes {
            return Ok(0);
        }

        files.sort_by_key(|a| a.1);

        let mut evicted_bytes: u64 = 0;
        let mut current = total_size;
        for (path, _mtime, size) in &files {
            if current <= self.size_limit_bytes {
                break;
            }
            if tokio::fs::remove_file(path).await.is_ok() {
                current -= size;
                evicted_bytes += size;
                {
                    let mut st = self.stats.lock().expect("cache stats mutex poisoned");
                    st.evictions += 1;
                    st.current_size_bytes = current;
                }
            }
        }
        Ok(evicted_bytes)
    }

    /// Convenience: key then get.
    ///
    /// # Errors
    ///
    /// Key or I/O errors.
    pub async fn get_for(&self, input: &CacheKeyInput<'_>) -> Result<Option<String>, CacheError> {
        let key = cache_key(input)?;
        self.get(&key).await
    }

    /// Convenience: key then put.
    ///
    /// # Errors
    ///
    /// Key or I/O errors.
    pub async fn put_for(&self, input: &CacheKeyInput<'_>, body: &str) -> Result<(), CacheError> {
        let key = cache_key(input)?;
        self.put(&key, body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("brigid-llm-cache-{n}"));
        // Create the directory eagerly so that `put()`'s `create_dir_all`
        // has a stable parent to work with. On Windows CI runners, the temp
        // dir can use 8.3 short names (e.g. `RUNNER~1`) which occasionally
        // cause `NotFound` errors if the directory doesn't pre-exist before
        // the async `tokio::fs::write` call. Creating it synchronously here
        // eliminates that race.
        std::fs::create_dir_all(&path).expect("create temp cache dir");
        path
    }

    #[test]
    fn keys_stable_and_sensitive() {
        let a = CacheKeyInput {
            prompt: "hello",
            model: "m1",
            provider: "p1",
            extras: None,
        };
        let b = CacheKeyInput {
            prompt: "hello",
            model: "m1",
            provider: "p1",
            extras: None,
        };
        assert_eq!(cache_key(&a).unwrap(), cache_key(&b).unwrap());
        assert!(cache_key(&a).unwrap().starts_with("sha256:"));

        let c = CacheKeyInput {
            prompt: "hello!",
            model: "m1",
            provider: "p1",
            extras: None,
        };
        assert_ne!(cache_key(&a).unwrap(), cache_key(&c).unwrap());

        let d = CacheKeyInput {
            prompt: "hello",
            model: "m2",
            provider: "p1",
            extras: None,
        };
        assert_ne!(cache_key(&a).unwrap(), cache_key(&d).unwrap());
    }

    #[tokio::test]
    async fn put_get_round_trip() {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "p",
            model: "m",
            provider: "prov",
            extras: Some("t=0"),
        };
        assert!(cache.get_for(&input).await.unwrap().is_none());
        cache.put_for(&input, r#"{"ok":true}"#).await.unwrap();
        assert_eq!(
            cache.get_for(&input).await.unwrap().as_deref(),
            Some(r#"{"ok":true}"#)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extras_change_key() {
        let a = CacheKeyInput {
            prompt: "x",
            model: "m",
            provider: "p",
            extras: None,
        };
        let b = CacheKeyInput {
            prompt: "x",
            model: "m",
            provider: "p",
            extras: Some("tools=v1"),
        };
        assert_ne!(cache_key(&a).unwrap(), cache_key(&b).unwrap());
    }

    #[tokio::test]
    async fn stats_track_hits_and_misses() {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "stats-test",
            model: "m",
            provider: "p",
            extras: None,
        };
        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);

        assert!(cache.get_for(&input).await.unwrap().is_none());
        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);

        cache.put_for(&input, r#"{"ok":true}"#).await.unwrap();
        let _ = cache.get_for(&input).await.unwrap();
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn eviction_removes_oldest_first() {
        let root = temp_root();
        let cache = DiskCache::with_size_limit_bytes(&root, 7);

        let input_a = CacheKeyInput {
            prompt: "a",
            model: "m",
            provider: "p",
            extras: None,
        };
        let input_b = CacheKeyInput {
            prompt: "b",
            model: "m",
            provider: "p",
            extras: None,
        };
        let input_c = CacheKeyInput {
            prompt: "c",
            model: "m",
            provider: "p",
            extras: None,
        };

        cache.put_for(&input_a, "aaa").await.unwrap();
        cache.put_for(&input_b, "bbb").await.unwrap();
        cache.put_for(&input_c, "ccc").await.unwrap();

        use std::fs::OpenOptions;
        use std::time::{Duration, UNIX_EPOCH};
        let path_a = cache.entry_path(&cache_key(&input_a).unwrap());
        let path_b = cache.entry_path(&cache_key(&input_b).unwrap());
        let path_c = cache.entry_path(&cache_key(&input_c).unwrap());
        // Open with write access — Windows requires it for set_modified.
        OpenOptions::new()
            .write(true)
            .open(&path_a)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(100))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path_b)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(200))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path_c)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(300))
            .unwrap();

        let evicted = cache.enforce_size_limit().await.unwrap();
        assert!(evicted > 0, "should have evicted bytes");
        assert!(!path_a.exists(), "oldest entry should be evicted");
        assert!(path_b.exists(), "middle entry should remain");
        assert!(path_c.exists(), "newest entry should remain");

        let stats = cache.stats();
        assert!(stats.evictions > 0, "eviction count should be positive");

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn eviction_respects_size_limit_config() {
        let root = temp_root();
        let limit_bytes: u64 = 20;
        let cache = DiskCache::with_size_limit_bytes(&root, limit_bytes);

        for i in 0..10u32 {
            let input = CacheKeyInput {
                prompt: &format!("evict-{i}"),
                model: "m",
                provider: "p",
                extras: None,
            };
            cache
                .put_for(&input, &format!("payload-{i:03}"))
                .await
                .unwrap();
        }

        let evicted = cache.enforce_size_limit().await.unwrap();
        assert!(evicted > 0, "should evict bytes when over limit");

        let remaining = fs::read_dir(&root)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .map(|e| e.path().extension().is_some())
                    .unwrap_or(false)
            })
            .count();
        let total_size: u64 = fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        assert!(
            total_size <= limit_bytes,
            "remaining size {total_size} should be within limit {limit_bytes}"
        );
        assert!(remaining > 0, "at least some entries should remain");

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn graceful_error_on_unwritable_cache_dir() {
        let blocker = std::env::temp_dir().join(format!(
            "brigid-llm-cache-blocker-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&blocker, b"blocker").unwrap();
        let root = blocker.join("subdir");

        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "fail",
            model: "m",
            provider: "p",
            extras: None,
        };
        let result = cache.put_for(&input, "data").await;
        assert!(
            result.is_err(),
            "put should return error, not panic, when dir cannot be created"
        );

        let _ = fs::remove_file(&blocker);
    }

    #[test]
    fn with_size_limit_stores_limit() {
        let root = temp_root();
        let cache = DiskCache::with_size_limit(&root, 50);
        assert_eq!(cache.size_limit_bytes(), 50 * 1024 * 1024);
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------
    // Issue #181: Permission denied on cache directory
    // ------------------------------------------------------------------

    /// When the cache directory is read-only, `put` must return a graceful
    /// error (not panic), and `get` must return `None` for missing entries
    /// (not crash).  This simulates a deployment where the cache partition
    /// is mounted read-only or permissions are misconfigured.
    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn permission_denied_cache_dir_graceful_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        fs::create_dir_all(&root).unwrap();

        // Write an entry while the dir is still writable.
        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "perm-test",
            model: "m",
            provider: "p",
            extras: None,
        };
        cache.put_for(&input, r#"{"ok":true}"#).await.unwrap();

        // Make the directory read-only.
        let original_perms = fs::metadata(&root).unwrap().permissions();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();

        // put on a *new* key must fail gracefully (cannot create new tmp file).
        let input_new = CacheKeyInput {
            prompt: "perm-test-new",
            model: "m",
            provider: "p",
            extras: None,
        };
        let put_result = cache.put_for(&input_new, r#"{"new":true}"#).await;
        assert!(
            put_result.is_err(),
            "put to read-only cache dir should return error, not panic"
        );
        assert!(
            matches!(put_result.unwrap_err(), CacheError::Io { .. }),
            "should be an I/O error"
        );

        // get on the existing entry should still work (read is allowed).
        let existing = cache.get_for(&input).await.unwrap();
        assert_eq!(existing.as_deref(), Some(r#"{"ok":true}"#));

        // get on a missing key returns None (not an error, not a panic).
        let missing = cache.get_for(&input_new).await.unwrap();
        assert!(missing.is_none(), "missing key should return None");

        // Restore permissions for cleanup.
        fs::set_permissions(&root, original_perms).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------
    // Issue #212: Mutex poisoning must panic with a descriptive message
    // ------------------------------------------------------------------

    /// When the stats mutex is poisoned (a thread panicked while holding
    /// the lock), subsequent access must panic with a clear message rather
    /// than the generic `unwrap()` panic.  We deliberately poison the
    /// mutex from a spawned thread, then verify that `stats()` panics.
    #[test]
    fn stats_mutex_poisoned_panics_with_descriptive_message() {
        use std::panic::AssertUnwindSafe;

        let root = temp_root();
        let cache = DiskCache::new(&root);
        // Clone shares the same Arc<Mutex<CacheStats>>.
        let cache_clone = cache.clone();

        // Poison the mutex by panicking while holding the lock.
        let handle = std::thread::spawn(move || {
            let _guard = cache_clone.stats.lock().unwrap();
            panic!("deliberate panic to poison the stats mutex");
        });
        // The thread panicked; the mutex is now poisoned.
        let _ = handle.join();

        // Accessing stats should panic with our expect message.
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = cache.stats();
        }));
        assert!(
            result.is_err(),
            "stats() should panic when the mutex is poisoned"
        );
        let msg = result
            .as_ref()
            .err()
            .and_then(|p| p.downcast_ref::<String>().cloned())
            .or_else(|| {
                result
                    .as_ref()
                    .err()
                    .and_then(|p| p.downcast_ref::<&'static str>().map(|s| s.to_string()))
            })
            .unwrap_or_default();
        assert!(
            msg.contains("poisoned"),
            "panic message should mention 'poisoned', got: {msg}"
        );
    }

    /// When the cache root cannot be created at all (parent is a file),
    /// both `put` and `get` must handle it gracefully — `put` returns an
    /// error, `get` returns `None` or a graceful I/O error (not a panic).
    /// The pipeline can continue without a working cache (degraded mode).
    #[tokio::test]
    #[serial_test::serial]
    async fn unwritable_cache_root_get_returns_none() {
        let blocker = std::env::temp_dir().join(format!(
            "brigid-llm-cache-blocker2-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&blocker, b"blocker").unwrap();
        let root = blocker.join("subdir");

        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "no-cache",
            model: "m",
            provider: "p",
            extras: None,
        };

        // put fails gracefully.
        let put_result = cache.put_for(&input, "data").await;
        assert!(put_result.is_err(), "put should fail when root is a file");

        // get returns None or a graceful I/O error — either way, no panic.
        // (When the parent path is a file, the OS may return NotADirectory
        // instead of NotFound; both are graceful.)
        let get_result = cache.get_for(&input).await;
        assert!(
            get_result.is_ok() || matches!(get_result, Err(CacheError::Io { .. })),
            "get should return None or a graceful I/O error, got: {get_result:?}"
        );

        let _ = fs::remove_file(&blocker);
    }

    // ------------------------------------------------------------------
    // Issue #230: LRU eviction loop, unlimited no-op, missing dir, rename error
    // ------------------------------------------------------------------

    /// An unlimited cache (`u64::MAX`) must treat `enforce_size_limit` as a
    /// no-op, returning `Ok(0)` without touching the filesystem.
    #[tokio::test]
    async fn unlimited_cache_enforce_size_limit_is_noop() {
        let root = temp_root();
        let cache = DiskCache::new(&root);

        // Write some entries so there is data on disk.
        let input = CacheKeyInput {
            prompt: "unlimited",
            model: "m",
            provider: "p",
            extras: None,
        };
        cache.put_for(&input, "data").await.unwrap();

        let evicted = cache.enforce_size_limit().await.expect("should be Ok");
        assert_eq!(evicted, 0, "unlimited cache should evict 0 bytes");

        // The entry must still be present.
        assert!(cache.get_for(&input).await.unwrap().is_some());

        let stats = cache.stats();
        assert_eq!(stats.evictions, 0, "no evictions for unlimited cache");

        let _ = fs::remove_dir_all(&root);
    }

    /// `enforce_size_limit` on a missing root directory must return `Ok(0)`
    /// (NotFound is treated as an empty cache, not an error).
    #[tokio::test]
    async fn enforce_size_limit_missing_root_returns_ok_zero() {
        let root = temp_root();
        // Note: we do NOT create the directory.
        let cache = DiskCache::with_size_limit_bytes(&root, 10);

        let evicted = cache.enforce_size_limit().await.expect("should be Ok");
        assert_eq!(evicted, 0, "missing root should return Ok(0)");

        let _ = fs::remove_dir_all(&root);
    }

    /// LRU eviction with a small size limit: writing 3 files that exceed the
    /// limit must evict the oldest-mtime file and increment `stats.evictions`.
    #[tokio::test]
    #[serial_test::serial]
    async fn lru_eviction_evicts_oldest_and_increments_stats() {
        let root = temp_root();
        // Limit small enough that 3 files of 4 bytes each (12 total) exceeds it.
        let cache = DiskCache::with_size_limit_bytes(&root, 5);

        let input_a = CacheKeyInput {
            prompt: "lru-a",
            model: "m",
            provider: "p",
            extras: None,
        };
        let input_b = CacheKeyInput {
            prompt: "lru-b",
            model: "m",
            provider: "p",
            extras: None,
        };
        let input_c = CacheKeyInput {
            prompt: "lru-c",
            model: "m",
            provider: "p",
            extras: None,
        };

        cache.put_for(&input_a, "aaaa").await.unwrap();
        cache.put_for(&input_b, "bbbb").await.unwrap();
        cache.put_for(&input_c, "cccc").await.unwrap();

        // Set distinct mtimes so the oldest is deterministic.
        use std::fs::OpenOptions;
        use std::time::{Duration, UNIX_EPOCH};
        let path_a = cache.entry_path(&cache_key(&input_a).unwrap());
        let path_b = cache.entry_path(&cache_key(&input_b).unwrap());
        let path_c = cache.entry_path(&cache_key(&input_c).unwrap());
        OpenOptions::new()
            .write(true)
            .open(&path_a)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(100))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path_b)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(200))
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path_c)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(300))
            .unwrap();

        let evicted = cache.enforce_size_limit().await.unwrap();
        assert!(evicted > 0, "should have evicted bytes");
        assert!(!path_a.exists(), "oldest entry (a) should be evicted");
        assert!(
            path_c.exists(),
            "newest entry (c) should remain after eviction"
        );

        let stats = cache.stats();
        assert!(
            stats.evictions >= 1,
            "stats.evictions should be >= 1, got {}",
            stats.evictions
        );
        assert!(
            stats.current_size_bytes <= 5,
            "current_size_bytes should be within limit after eviction"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// `enforce_size_limit` when files are already under the limit must
    /// return `Ok(0)` and update `current_size_bytes` without evicting.
    #[tokio::test]
    #[serial_test::serial]
    async fn enforce_size_limit_under_limit_no_eviction() {
        let root = temp_root();
        let cache = DiskCache::with_size_limit_bytes(&root, 1000);

        let input = CacheKeyInput {
            prompt: "under-limit",
            model: "m",
            provider: "p",
            extras: None,
        };
        cache.put_for(&input, "small").await.unwrap();

        let evicted = cache.enforce_size_limit().await.unwrap();
        assert_eq!(evicted, 0, "should not evict when under limit");

        let stats = cache.stats();
        assert_eq!(stats.evictions, 0, "no evictions when under limit");
        assert!(
            stats.current_size_bytes > 0,
            "current_size_bytes should reflect on-disk size"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// `put` into a read-only parent directory must return a graceful
    /// `CacheError::Io` (not panic). This simulates a read-only cache
    /// partition where the rename step fails.
    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn put_rename_into_readonly_parent_graceful_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        fs::create_dir_all(&root).unwrap();

        // Write an entry while writable.
        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "existing",
            model: "m",
            provider: "p",
            extras: None,
        };
        cache.put_for(&input, "data").await.unwrap();

        // Make the directory read-only.
        let original_perms = fs::metadata(&root).unwrap().permissions();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();

        // A new key must fail gracefully on the rename (or write) step.
        let input_new = CacheKeyInput {
            prompt: "new-key-readonly",
            model: "m",
            provider: "p",
            extras: None,
        };
        let result = cache.put_for(&input_new, "new-data").await;
        assert!(
            result.is_err(),
            "put into read-only dir should return error, not panic"
        );
        assert!(
            matches!(result.unwrap_err(), CacheError::Io { .. }),
            "should be an I/O error"
        );

        // Restore permissions for cleanup.
        fs::set_permissions(&root, original_perms).unwrap();
        let _ = fs::remove_dir_all(&root);
    }
}
