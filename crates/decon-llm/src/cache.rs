//! Disk response cache for LLM calls (structure only — no network).
//!
//! Keys are stable SHA-256 digests of the canonical JSON object
//! `{ "prompt", "model", "provider", "extras" }`. Values are opaque UTF-8
//! response bodies stored as files under a cache root.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
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
        self.stats.lock().unwrap().clone()
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
    pub fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let path = self.entry_path(key);
        match fs::read_to_string(&path) {
            Ok(s) => {
                {
                    let mut st = self.stats.lock().unwrap();
                    st.hits += 1;
                }
                if let Ok(file) = fs::File::open(&path) {
                    let _ = file.set_modified(SystemTime::now());
                }
                Ok(Some(s))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                {
                    let mut st = self.stats.lock().unwrap();
                    st.misses += 1;
                }
                Ok(None)
            }
            Err(source) => {
                {
                    let mut st = self.stats.lock().unwrap();
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
    pub fn put(&self, key: &str, body: &str) -> Result<(), CacheError> {
        fs::create_dir_all(&self.root).map_err(|source| CacheError::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.entry_path(key);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, body).map_err(|source| CacheError::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, &path).map_err(|source| CacheError::Io {
            path: path.clone(),
            source,
        })?;
        let should_check = {
            let mut st = self.stats.lock().unwrap();
            st.write_count += 1;
            st.write_count % EVICTION_CHECK_INTERVAL == 0
        };
        if should_check {
            let _ = self.enforce_size_limit();
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
    pub fn enforce_size_limit(&self) -> Result<u64, CacheError> {
        if self.size_limit_bytes == u64::MAX {
            return Ok(0);
        }
        let entries = match fs::read_dir(&self.root) {
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
        for ent in entries {
            let ent = match ent {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let md = match path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((path, mtime, md.len()));
        }

        let total_size: u64 = files.iter().map(|(_, _, sz)| *sz).sum();
        {
            let mut st = self.stats.lock().unwrap();
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
            if fs::remove_file(path).is_ok() {
                current -= size;
                evicted_bytes += size;
                {
                    let mut st = self.stats.lock().unwrap();
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
    pub fn get_for(&self, input: &CacheKeyInput<'_>) -> Result<Option<String>, CacheError> {
        let key = cache_key(input)?;
        self.get(&key)
    }

    /// Convenience: key then put.
    ///
    /// # Errors
    ///
    /// Key or I/O errors.
    pub fn put_for(&self, input: &CacheKeyInput<'_>, body: &str) -> Result<(), CacheError> {
        let key = cache_key(input)?;
        self.put(&key, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("decon-llm-cache-{n}"))
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

    #[test]
    fn put_get_round_trip() {
        let root = temp_root();
        let cache = DiskCache::new(&root);
        let input = CacheKeyInput {
            prompt: "p",
            model: "m",
            provider: "prov",
            extras: Some("t=0"),
        };
        assert!(cache.get_for(&input).unwrap().is_none());
        cache.put_for(&input, r#"{"ok":true}"#).unwrap();
        assert_eq!(
            cache.get_for(&input).unwrap().as_deref(),
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

    #[test]
    fn stats_track_hits_and_misses() {
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

        assert!(cache.get_for(&input).unwrap().is_none());
        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);

        cache.put_for(&input, r#"{"ok":true}"#).unwrap();
        let _ = cache.get_for(&input).unwrap();
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn eviction_removes_oldest_first() {
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

        cache.put_for(&input_a, "aaa").unwrap();
        cache.put_for(&input_b, "bbb").unwrap();
        cache.put_for(&input_c, "ccc").unwrap();

        use std::fs::File;
        use std::time::{Duration, UNIX_EPOCH};
        let path_a = cache.entry_path(&cache_key(&input_a).unwrap());
        let path_b = cache.entry_path(&cache_key(&input_b).unwrap());
        let path_c = cache.entry_path(&cache_key(&input_c).unwrap());
        File::open(&path_a)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(100))
            .unwrap();
        File::open(&path_b)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(200))
            .unwrap();
        File::open(&path_c)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(300))
            .unwrap();

        let evicted = cache.enforce_size_limit().unwrap();
        assert!(evicted > 0, "should have evicted bytes");
        assert!(!path_a.exists(), "oldest entry should be evicted");
        assert!(path_b.exists(), "middle entry should remain");
        assert!(path_c.exists(), "newest entry should remain");

        let stats = cache.stats();
        assert!(stats.evictions > 0, "eviction count should be positive");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn eviction_respects_size_limit_config() {
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
            cache.put_for(&input, &format!("payload-{i:03}")).unwrap();
        }

        let evicted = cache.enforce_size_limit().unwrap();
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

    #[test]
    fn graceful_error_on_unwritable_cache_dir() {
        let blocker = std::env::temp_dir().join(format!(
            "decon-llm-cache-blocker-{}",
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
        let result = cache.put_for(&input, "data");
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
}
