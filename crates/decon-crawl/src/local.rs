//! Local filesystem crawl: inventory of relative paths under a root.
//!
//! For Milestone 1, the walk matches the frozen fixture baseline
//! (`tests/fixtures/baseline.json`):
//!
//! - Skip **hidden directories** (name starts with `.`)
//! - Include **hidden files** (e.g. `.env.example`)
//! - Emit relative POSIX paths (`/` separators), sorted lexicographically
//! - Paths must be valid UTF-8 (non-UTF-8 names fail the crawl)
//!
//! `.gitignore` support (via the `ignore` crate) is deferred; fixtures do not
//! rely on it. GitHub fetch is out of scope for this module.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Inventory of files discovered under a repository root.
///
/// `files` and `sizes` are parallel arrays: `sizes[i]` is the byte length of
/// `files[i]`, obtained via `fs::metadata` (which **follows symlinks**, matching
/// the classification logic that uses `Path::is_file`). This lets downstream
/// budget estimation skip a second full re-stat of every path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrawlResult {
    /// Relative file paths using `/` separators, sorted ascending.
    pub files: Vec<String>,
    /// Byte length of each file, parallel to [`Self::files`] (same length and
    /// order). `sizes[i]` is the size of `files[i]`, following symlinks.
    pub sizes: Vec<u64>,
}

impl CrawlResult {
    /// Number of files in the inventory.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Iterate over `(path, size)` pairs, zipping [`Self::files`] and
    /// [`Self::sizes`].
    ///
    /// Because the two vectors are always the same length, every file is
    /// paired with its size. The returned iterator is already `#[must_use]`
    /// via `impl Iterator`, so no separate attribute is needed.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.files
            .iter()
            .map(String::as_str)
            .zip(self.sizes.iter().copied())
    }
}

/// Errors produced while crawling a local tree.
#[derive(Debug, Error)]
pub enum CrawlError {
    /// The path exists but is not a directory.
    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),
    /// The path does not exist or cannot be accessed as a directory.
    #[error("failed to access directory {path}: {source}")]
    Io {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A path component is not valid UTF-8 (cannot form a POSIX inventory string).
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
}

/// Inventory files under `root` (iterative walk; no recursion stack risk).
///
/// Returns sorted relative paths with their byte sizes (following symlinks
/// via `fs::metadata`). Hidden directories are skipped; hidden files are
/// included.
///
/// ## Symlinks
///
/// Entries are classified with `Path::is_file` / `is_dir`, which **follow**
/// symlinks (same as a plain `read_dir` walk). Inventory paths are always
/// relative to `root` as discovered under the tree (the symlink's path), never
/// the absolute target path:
///
/// - A **file** symlink `leak.txt` -> outside file is listed as `leak.txt`.
/// - A **directory** symlink `out_link` -> outside dir is descended into; files
///   appear as `out_link/...` (still under `root` via the link path).
///
/// Paths that cannot be expressed relative to `root` are omitted.
/// Symlink cycles are detected via a visited-set of canonical paths and
/// skipped with a warning to stderr (the crawl continues for other files).
///
/// # Errors
///
/// - [`CrawlError::NotADirectory`] if `root` exists but is not a directory
/// - [`CrawlError::Io`] if `root` cannot be opened as a directory
/// - [`CrawlError::NonUtf8Path`] if any inventoried path is not valid UTF-8
///
/// Nested unreadable directories are skipped (best-effort walk) rather than
/// failing the whole crawl, so partial trees still produce an inventory.
///
/// # Examples
///
/// ```no_run
/// use decon_crawl::local::crawl_local;
///
/// let result = crawl_local(".").expect("cwd is a directory");
/// assert!(result.file_count() > 0);
/// ```
pub fn crawl_local(root: impl AsRef<Path>) -> Result<CrawlResult, CrawlError> {
    let root = root.as_ref();
    let meta = fs::metadata(root).map_err(|source| CrawlError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(CrawlError::NotADirectory(root.to_path_buf()));
    }

    let mut entries = crawl_tree(root)?;
    // Sort by path, keeping sizes parallel.
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let (files, sizes) = entries.into_iter().unzip();
    Ok(CrawlResult { files, sizes })
}

/// Whether a directory/file **name** is hidden (leading `.`), without lossy UTF-8.
fn is_hidden_name(name: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().first() == Some(&b'.')
    }
    #[cfg(not(unix))]
    {
        // Windows: OsStr is WTF-8; lossy is acceptable for the leading-dot check.
        name.to_string_lossy().starts_with('.')
    }
}

/// Convert `path` to a relative POSIX string under `root`, or `Ok(None)` if
/// `path` is not under `root`.
fn relative_posix(root: &Path, path: &Path) -> Result<Option<String>, CrawlError> {
    let Ok(rel) = path.strip_prefix(root) else {
        return Ok(None);
    };
    let Some(s) = rel.to_str() else {
        return Err(CrawlError::NonUtf8Path(path.to_path_buf()));
    };
    Ok(Some(s.replace('\\', "/")))
}

const MAX_SYMLINK_DEPTH: usize = 40;

fn crawl_tree(root: &Path) -> Result<Vec<(String, u64)>, CrawlError> {
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut symlink_visited: HashSet<PathBuf> = HashSet::new();
    let root_canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    symlink_visited.insert(root_canonical);

    while let Some((dir, depth)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();

            // Skip hidden directories; allow hidden files.
            if path.is_dir() && is_hidden_name(&name) {
                continue;
            }

            let is_symlink = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());

            if is_symlink {
                match fs::canonicalize(&path) {
                    Ok(canon) => {
                        if path.is_dir() {
                            if depth >= MAX_SYMLINK_DEPTH || symlink_visited.contains(&canon) {
                                eprintln!("skipping symlink cycle: {}", path.display());
                                continue;
                            }
                            symlink_visited.insert(canon);
                            stack.push((path, depth + 1));
                        } else if path.is_file() {
                            if let Some(rel) = relative_posix(root, &path)? {
                                if let Ok(meta) = fs::metadata(&path) {
                                    files.push((rel, meta.len()));
                                }
                            }
                        }
                    }
                    Err(_) => {
                        eprintln!("skipping symlink cycle: {}", path.display());
                    }
                }
            } else if path.is_file() {
                if let Some(rel) = relative_posix(root, &path)? {
                    if let Ok(meta) = fs::metadata(&path) {
                        files.push((rel, meta.len()));
                    }
                }
            } else if path.is_dir() {
                stack.push((path, depth));
            }
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
    }

    fn baseline_files(fixture: &str) -> Vec<String> {
        let baseline_path = fixtures_dir().join("baseline.json");
        let raw = fs::read_to_string(&baseline_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", baseline_path.display()));
        let v: serde_json::Value = serde_json::from_str(&raw).expect("baseline.json is valid JSON");
        v[fixture]["crawl"]["files"]
            .as_array()
            .unwrap_or_else(|| panic!("baseline missing crawl.files for {fixture}"))
            .iter()
            .map(|x| x.as_str().expect("file path is a string").to_owned())
            .collect()
    }

    #[test]
    fn crawl_python_lib_matches_baseline() {
        let root = fixtures_dir().join("python-lib");
        let result = crawl_local(&root).expect("crawl python-lib");
        let expected = baseline_files("python-lib");
        assert_eq!(result.file_count(), expected.len());
        assert_eq!(result.files, expected);
    }

    #[test]
    fn crawl_umbrella_matches_baseline() {
        let root = fixtures_dir().join("umbrella");
        let result = crawl_local(&root).expect("crawl umbrella");
        let expected = baseline_files("umbrella");
        assert_eq!(result.file_count(), expected.len());
        assert_eq!(result.files, expected);
    }

    #[test]
    fn crawl_js_lib_matches_baseline() {
        let root = fixtures_dir().join("js-lib");
        let result = crawl_local(&root).expect("crawl js-lib");
        let expected = baseline_files("js-lib");
        assert_eq!(result.file_count(), expected.len());
        assert_eq!(result.files, expected);
    }

    #[test]
    fn empty_root_returns_empty_inventory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = crawl_local(dir.path()).expect("empty crawl");
        assert_eq!(result, CrawlResult::default());
        assert_eq!(result.file_count(), 0);
    }

    #[test]
    fn hidden_directory_skipped_hidden_file_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        File::create(root.join(".env.example"))
            .and_then(|mut f| f.write_all(b"KEY=1\n"))
            .expect("hidden file");
        File::create(root.join("visible.txt"))
            .and_then(|mut f| f.write_all(b"ok\n"))
            .expect("visible file");

        let hidden_dir = root.join(".hidden");
        fs::create_dir(&hidden_dir).expect("hidden dir");
        File::create(hidden_dir.join("secret.txt"))
            .and_then(|mut f| f.write_all(b"nope\n"))
            .expect("file inside hidden dir");

        let nested = root.join("src");
        fs::create_dir(&nested).expect("src");
        File::create(nested.join("main.rs"))
            .and_then(|mut f| f.write_all(b"fn main() {}\n"))
            .expect("nested file");

        let result = crawl_local(root).expect("crawl temp tree");
        assert_eq!(
            result.files,
            vec![
                ".env.example".to_owned(),
                "src/main.rs".to_owned(),
                "visible.txt".to_owned(),
            ]
        );
        assert!(!result.files.iter().any(|f| f.contains("secret")));
    }

    #[test]
    fn not_a_directory_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("only-file");
        File::create(&file).expect("touch");
        let err = crawl_local(&file).expect_err("file is not a directory");
        assert!(matches!(err, CrawlError::NotADirectory(_)));
    }

    #[test]
    fn missing_path_errors() {
        let err = crawl_local("/nonexistent/decon-crawl-path-xyz").expect_err("missing");
        assert!(matches!(err, CrawlError::Io { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn relative_posix_rejects_non_utf8_path() {
        // macOS APFS rejects non-UTF-8 names on create; exercise the conversion
        // path in memory instead of relying on the filesystem.
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("/tmp/decon-root");
        let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/decon-root/bad\xffname.txt"));
        let err = relative_posix(root, &bad).expect_err("non-utf8");
        assert!(matches!(err, CrawlError::NonUtf8Path(_)));
    }

    #[test]
    #[cfg(unix)]
    fn file_symlink_listed_even_when_target_is_outside_root() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_file = outside.path().join("external.txt");
        File::create(&outside_file)
            .and_then(|mut f| f.write_all(b"external\n"))
            .expect("outside file");

        let root = tempfile::tempdir().expect("root tempdir");
        File::create(root.path().join("inside.txt"))
            .and_then(|mut f| f.write_all(b"inside\n"))
            .expect("inside file");

        // Symlink path lives under root; target is outside.
        symlink(&outside_file, root.path().join("leak.txt")).expect("create symlink");

        let result = crawl_local(root.path()).expect("crawl");
        // Listed by symlink path under root (not the absolute outside path).
        assert_eq!(
            result.files,
            vec!["inside.txt".to_owned(), "leak.txt".to_owned()]
        );
    }

    #[test]
    #[cfg(unix)]
    fn dir_symlink_outside_root_listed_under_link_path() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().expect("outside tempdir");
        File::create(outside.path().join("secret.txt"))
            .and_then(|mut f| f.write_all(b"secret\n"))
            .expect("outside file");

        let root = tempfile::tempdir().expect("root tempdir");
        File::create(root.path().join("inside.txt"))
            .and_then(|mut f| f.write_all(b"inside\n"))
            .expect("inside file");
        symlink(outside.path(), root.path().join("out_link")).expect("dir symlink");

        let result = crawl_local(root.path()).expect("crawl");
        // Content is inventoriable via the in-tree link path (not absolute outside).
        assert_eq!(
            result.files,
            vec!["inside.txt".to_owned(), "out_link/secret.txt".to_owned(),]
        );
        assert!(!result.files.iter().any(|f| f.starts_with('/')));
    }

    #[test]
    #[cfg(unix)]
    fn symlink_to_file_inside_root_is_included() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let target = root.path().join("target.txt");
        File::create(&target)
            .and_then(|mut f| f.write_all(b"data\n"))
            .expect("target");
        symlink(&target, root.path().join("alias.txt")).expect("symlink");

        let result = crawl_local(root.path()).expect("crawl");
        // Both the real file and the symlink path appear as files under root.
        assert!(result.files.contains(&"target.txt".to_owned()));
        assert!(result.files.contains(&"alias.txt".to_owned()));
    }

    #[test]
    fn sizes_match_known_file_lengths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        File::create(root.join("a.txt"))
            .and_then(|mut f| f.write_all(b"hello")) // 5 bytes
            .expect("a.txt");
        File::create(root.join("b.txt"))
            .and_then(|mut f| f.write_all(b"world!")) // 6 bytes
            .expect("b.txt");

        let result = crawl_local(root).expect("crawl");
        assert_eq!(result.files.len(), result.sizes.len());
        // files are sorted: a.txt, b.txt
        assert_eq!(result.files, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
        assert_eq!(result.sizes, vec![5, 6]);
    }

    #[test]
    fn empty_file_is_inventoried_with_size_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        File::create(root.join("empty.txt")).expect("touch empty");
        File::create(root.join("nonempty.txt"))
            .and_then(|mut f| f.write_all(b"x"))
            .expect("nonempty");

        let result = crawl_local(root).expect("crawl");
        assert_eq!(result.files.len(), result.sizes.len());
        let empty_idx = result
            .files
            .iter()
            .position(|f| f == "empty.txt")
            .expect("empty.txt inventoried");
        assert_eq!(result.sizes[empty_idx], 0);
    }

    #[test]
    fn fixture_sizes_populated_and_parallel_to_files() {
        let root = fixtures_dir().join("python-lib");
        let result = crawl_local(&root).expect("crawl python-lib");
        assert_eq!(result.sizes.len(), result.files.len());
        // Every non-empty fixture file should report a positive size.
        for (path, size) in result.iter() {
            assert!(
                size > 0,
                "fixture file {path} should be non-empty (size {size})"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn symlink_size_follows_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let target = root.path().join("target.txt");
        File::create(&target)
            .and_then(|mut f| f.write_all(b"target-data")) // 11 bytes
            .expect("target");
        symlink(&target, root.path().join("alias.txt")).expect("symlink");

        let result = crawl_local(root.path()).expect("crawl");
        assert_eq!(result.files.len(), result.sizes.len());
        let alias_idx = result
            .files
            .iter()
            .position(|f| f == "alias.txt")
            .expect("alias.txt inventoried");
        // Size follows the symlink to the target's 11 bytes, NOT the symlink
        // lstat size (which would be the length of the target path string).
        assert_eq!(result.sizes[alias_idx], 11);
    }

    #[test]
    fn iter_yields_path_size_pairs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        File::create(root.join("a.txt"))
            .and_then(|mut f| f.write_all(b"abc"))
            .expect("a.txt");

        let result = crawl_local(root).expect("crawl");
        let pairs: Vec<(&str, u64)> = result.iter().collect();
        assert_eq!(pairs, vec![("a.txt", 3)]);
    }

    #[test]
    #[cfg(unix)]
    fn self_referencing_symlink_is_skipped() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let link = root.path().join("a");
        symlink(&link, &link).expect("self symlink");

        File::create(root.path().join("real.txt"))
            .and_then(|mut f| f.write_all(b"ok\n"))
            .expect("real file");

        let result = crawl_local(root.path()).expect("crawl");
        assert!(
            !result.files.iter().any(|f| f == "a"),
            "self-referencing symlink must be skipped"
        );
        assert!(result.files.contains(&"real.txt".to_owned()));
    }

    #[test]
    #[cfg(unix)]
    fn mutual_symlink_loop_is_skipped() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let a = root.path().join("a");
        let b = root.path().join("b");
        symlink(&b, &a).expect("a -> b");
        symlink(&a, &b).expect("b -> a");

        File::create(root.path().join("real.txt"))
            .and_then(|mut f| f.write_all(b"ok\n"))
            .expect("real file");

        let result = crawl_local(root.path()).expect("crawl");
        assert!(
            !result.files.iter().any(|f| f == "a" || f == "b"),
            "mutual symlink loop must be skipped"
        );
        assert!(result.files.contains(&"real.txt".to_owned()));
    }

    #[test]
    #[cfg(unix)]
    fn deep_symlink_chain_is_skipped_at_depth_limit() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let target = root.path().join("real.txt");
        File::create(&target)
            .and_then(|mut f| f.write_all(b"ok\n"))
            .expect("real file");

        let mut prev = target;
        for i in 0..50 {
            let link = root.path().join(format!("link{i}"));
            symlink(&prev, &link).expect("chain link");
            prev = link;
        }

        let result = crawl_local(root.path()).expect("crawl");
        assert!(
            !result.files.iter().any(|f| f == "link49"),
            "deepest symlink in chain must be skipped at depth limit"
        );
        assert!(result.files.contains(&"real.txt".to_owned()));
    }

    #[test]
    #[cfg(unix)]
    fn dir_symlink_to_root_is_skipped() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        File::create(root.path().join("real.txt"))
            .and_then(|mut f| f.write_all(b"ok\n"))
            .expect("real file");
        symlink(root.path(), root.path().join("link")).expect("dir symlink to root");

        let result = crawl_local(root.path()).expect("crawl");
        assert!(
            !result.files.iter().any(|f| f.starts_with("link")),
            "directory symlink cycle to root must be skipped"
        );
        assert!(result.files.contains(&"real.txt".to_owned()));
    }

    #[test]
    #[cfg(unix)]
    fn mutual_dir_symlink_loop_is_skipped() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let dir_a = root.path().join("a");
        let dir_b = root.path().join("b");
        fs::create_dir(&dir_a).expect("dir a");
        fs::create_dir(&dir_b).expect("dir b");
        symlink(&dir_b, dir_a.join("loop_b")).expect("a/loop_b -> b");
        symlink(&dir_a, dir_b.join("loop_a")).expect("b/loop_a -> a");

        File::create(dir_a.join("a.txt"))
            .and_then(|mut f| f.write_all(b"a\n"))
            .expect("a.txt");
        File::create(dir_b.join("b.txt"))
            .and_then(|mut f| f.write_all(b"b\n"))
            .expect("b.txt");

        let result = crawl_local(root.path()).expect("crawl");
        assert!(
            result.files.contains(&"a/a.txt".to_owned()),
            "a/a.txt must be found via real directory"
        );
        assert!(
            result.files.contains(&"b/b.txt".to_owned()),
            "b/b.txt must be found via real directory"
        );
        let loop_files: Vec<_> = result.files.iter().filter(|f| f.contains("loop")).collect();
        assert!(
            loop_files.len() <= 2,
            "mutual symlink loop must not recurse infinitely (found {} files under loop paths: {:?})",
            loop_files.len(),
            loop_files
        );
    }

    #[test]
    #[cfg(unix)]
    fn normal_symlink_to_file_still_followed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let target = root.path().join("target.txt");
        File::create(&target)
            .and_then(|mut f| f.write_all(b"data\n"))
            .expect("target");
        symlink(&target, root.path().join("alias.txt")).expect("symlink");

        let result = crawl_local(root.path()).expect("crawl");
        assert!(result.files.contains(&"target.txt".to_owned()));
        assert!(result.files.contains(&"alias.txt".to_owned()));
    }

    // ------------------------------------------------------------------
    // Issue #181: Deep nesting and long filename edge cases
    // ------------------------------------------------------------------

    /// Crawl a tree with 120 levels of nested directories and a file at the
    /// bottom.  The iterative walker (stack-based, no recursion) must handle
    /// this without stack overflow or errors.
    #[test]
    fn deep_nesting_120_levels_crawled_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // Use single-character dir names to stay well under PATH_MAX (1024
        // on macOS).  120 levels × 2 chars (name + '/') = 240 char relative
        // path, plus the temp-dir prefix keeps the absolute path under 1024.
        let depth = 120;
        let mut current = root.to_path_buf();
        for _ in 0..depth {
            current = current.join("d");
            fs::create_dir(&current).expect("create nested dir");
        }
        // Place a file at the bottom.
        let deep_file = current.join("leaf.txt");
        File::create(&deep_file)
            .and_then(|mut f| f.write_all(b"deep content\n"))
            .expect("deep file");

        // Also place a file at a shallow level for sanity.
        File::create(root.join("shallow.txt"))
            .and_then(|mut f| f.write_all(b"shallow\n"))
            .expect("shallow file");

        let result = crawl_local(root).expect("crawl deep tree");
        assert!(
            result.file_count() >= 2,
            "should find at least 2 files, got {}",
            result.file_count()
        );

        // Build the expected relative path for the deep file.
        let mut expected_rel = String::new();
        for _ in 0..depth {
            expected_rel.push_str("d/");
        }
        expected_rel.push_str("leaf.txt");

        assert!(
            result.files.contains(&expected_rel),
            "deep file should be in inventory: {expected_rel}"
        );
        assert!(
            result.files.contains(&"shallow.txt".to_owned()),
            "shallow file should be in inventory"
        );

        // Verify sizes are parallel.
        assert_eq!(result.files.len(), result.sizes.len());
    }

    /// A file with a 255-character name (the POSIX max per component) must
    /// be crawled successfully.  Attempting to create a file with a 256+
    /// character name must fail with a clear OS error (ENAMETOOLONG),
    /// demonstrating the filesystem enforces the limit.
    #[test]
    fn long_filename_255_chars_crawled_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // 255-char filename (max on most POSIX filesystems).
        let long_name = "a".repeat(255);
        let long_path = root.join(&long_name);
        match File::create(&long_path) {
            Ok(mut f) => {
                f.write_all(b"long name file\n").expect("write");
                let result = crawl_local(root).expect("crawl");
                assert!(
                    result.files.contains(&long_name),
                    "255-char filename should be crawled"
                );
            }
            Err(e) => {
                // Some filesystems may not support 255 chars; skip gracefully.
                eprintln!("skipping 255-char test: filesystem limit: {e}");
            }
        }
    }

    /// A filename exceeding 255 characters cannot be created on most
    /// POSIX filesystems.  The OS returns ENAMETOOLONG — a clear, actionable
    /// error.  This test verifies the error is propagated (not silently
    /// ignored) and that the crawl of the surrounding directory still works.
    #[test]
    fn filename_over_255_chars_os_rejects_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // Create a normal file alongside.
        File::create(root.join("normal.txt"))
            .and_then(|mut f| f.write_all(b"ok\n"))
            .expect("normal file");

        // Attempt to create a file with a 300-char name.
        let too_long = "x".repeat(300);
        let too_long_path = root.join(&too_long);
        let create_result = File::create(&too_long_path);

        if let Err(e) = &create_result {
            // OS rejected the long name — verify it's a clear I/O error
            // (not a silent success or panic).  The exact error kind varies
            // by platform (InvalidFilename on macOS, InvalidInput on Linux,
            // InvalidFilename with OS error 123 on Windows).
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("too long")
                    || msg.contains("filename")
                    || e.raw_os_error() == Some(36), // ENAMETOOLONG (Unix)
                "expected a filename-length error, got: {e} (kind={:?})",
                e.kind()
            );
        }
        // If the OS *did* allow it (unusual FS), that's fine too — the crawl
        // should still work.

        // Crawl must succeed regardless.
        let result = crawl_local(root).expect("crawl");
        assert!(
            result.files.contains(&"normal.txt".to_owned()),
            "normal file must be crawled even if long-name creation failed"
        );
    }

    /// Total relative path exceeding 255 chars (via many nested components)
    /// must be crawled without issues.  This is distinct from individual
    /// component length — the OS allows long total paths (PATH_MAX=1024).
    #[test]
    fn total_path_over_255_chars_crawled_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        // 30 dirs × 10 chars each = 300+ char relative path.
        let mut current = root.to_path_buf();
        let mut expected_rel = String::new();
        for i in 0..30 {
            let comp = format!("d{i:02}_5678"); // 10 chars each
            current = current.join(&comp);
            fs::create_dir(&current).expect("create dir");
            expected_rel.push_str(&comp);
            expected_rel.push('/');
        }
        expected_rel.push_str("file.txt");
        File::create(current.join("file.txt"))
            .and_then(|mut f| f.write_all(b"deep\n"))
            .expect("deep file");

        assert!(
            expected_rel.len() > 255,
            "test setup: relative path should exceed 255 chars, got {}",
            expected_rel.len()
        );

        let result = crawl_local(root).expect("crawl");
        assert!(
            result.files.contains(&expected_rel),
            "file with >255 char relative path should be crawled"
        );
    }
}
