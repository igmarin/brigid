//! Git-based incremental file detection for `decon-crawl`.
//!
//! Detects files that have changed since a given git ref (tag, commit, or
//! branch) using `git diff --name-only`. This avoids re-processing the entire
//! repository on subsequent runs — only files modified between the ref and
//! `HEAD` are returned.
//!
//! ## Merge-commit handling
//!
//! For branch workflows where `HEAD` may be a merge commit, the triple-dot
//! range `<ref>...HEAD` is used so that the diff is computed against the
//! merge-base of `<ref>` and `HEAD`. This surfaces files changed on either
//! side of the merge without listing every file touched by the full history
//! divergence (which the double-dot `<ref>..HEAD` would do).
//!
//! See `docs/adr/0013-git-diff-incremental.md` for the full rationale.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::local::CrawlError;

/// Errors produced while detecting changed files via `git diff`.
#[derive(Debug, Error)]
pub enum GitDiffError {
    /// `git` is not installed or not on `PATH`.
    #[error("git executable not found on PATH")]
    GitNotFound,
    /// `repo_root` is not a git repository.
    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),
    /// The given ref could not be resolved by `git rev-parse`.
    #[error("git ref not found: {ref_name}")]
    RefNotFound {
        /// The ref that could not be resolved.
        ref_name: String,
    },
    /// A `git` command exited with a non-zero status.
    #[error("git command failed: {stderr}")]
    CommandFailed {
        /// Captured stderr (trimmed) from the failed command.
        stderr: String,
    },
}

impl From<GitDiffError> for CrawlError {
    /// Convert a [`GitDiffError`] into a [`CrawlError`] so callers of
    /// `crawl_local` only need to handle a single error type.
    fn from(e: GitDiffError) -> Self {
        CrawlError::GitDiff(e.to_string())
    }
}

/// Detect files that have changed since `ref_name` in the repository at
/// `repo_root`.
///
/// Uses `git diff --name-only --ignore-submodules <ref_name>...HEAD` (the
/// triple-dot range selects the merge-base, so merge commits are handled
/// correctly for branch workflows).
///
/// The returned paths are:
///
/// - Relative to `repo_root` (as `git diff --name-only` reports them).
/// - Normalised to use `/` separators.
/// - Filtered to **existing** files (deleted files are excluded).
/// - Sorted and de-duplicated.
///
/// # Errors
///
/// - [`GitDiffError::GitNotFound`] if `git` is not installed.
/// - [`GitDiffError::NotARepository`] if `repo_root` is not a git repository.
/// - [`GitDiffError::RefNotFound`] if `ref_name` does not resolve.
/// - [`GitDiffError::CommandFailed`] if the diff command itself fails.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use decon_crawl::git_diff::changed_files_since;
///
/// let files = changed_files_since(Path::new("."), "v0.5.0")
///     .expect("changed files since tag");
/// for f in &files {
///     println!("{}", f.display());
/// }
/// ```
pub fn changed_files_since(repo_root: &Path, ref_name: &str) -> Result<Vec<PathBuf>, GitDiffError> {
    // 1. Validate that git is available.
    ensure_git_available()?;

    // 2. Validate that repo_root is a git repository.
    ensure_is_repository(repo_root)?;

    // 3. Validate that the ref exists.
    ensure_ref_exists(repo_root, ref_name)?;

    // 4. Run the diff (triple-dot for merge-base semantics).
    let output = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "diff",
            "--name-only",
            "--ignore-submodules",
            &format!("{ref_name}...HEAD"),
        ])
        .output()
        .map_err(|_| GitDiffError::GitNotFound)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(GitDiffError::CommandFailed { stderr });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths: Vec<PathBuf> = stdout
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|p| {
            // Only keep files that still exist on disk.
            repo_root.join(p).is_file()
        })
        .collect();

    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Return `Ok(())` if `git` is on `PATH`, otherwise [`GitDiffError::GitNotFound`].
fn ensure_git_available() -> Result<(), GitDiffError> {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(_) => Err(GitDiffError::CommandFailed {
            stderr: "git --version returned non-zero".to_owned(),
        }),
        Err(_) => Err(GitDiffError::GitNotFound),
    }
}

/// Return `Ok(())` if `repo_root` is inside a git repository.
///
/// Uses `git rev-parse --git-dir` which succeeds inside a worktree or a bare
/// repo checkout. A missing `.git` directory alone is not sufficient to reject
/// a path (worktrees do not contain one), so `git rev-parse` is authoritative.
fn ensure_is_repository(repo_root: &Path) -> Result<(), GitDiffError> {
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "rev-parse", "--git-dir"])
        .output()
        .map_err(|_| GitDiffError::GitNotFound)?;

    if output.status.success() {
        Ok(())
    } else {
        Err(GitDiffError::NotARepository(repo_root.to_path_buf()))
    }
}

/// Return `Ok(())` if `ref_name` resolves via `git rev-parse --verify`.
fn ensure_ref_exists(repo_root: &Path, ref_name: &str) -> Result<(), GitDiffError> {
    let spec = format!("{ref_name}^{{commit}}");
    let output = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "rev-parse",
            "--verify",
            "--quiet",
            &spec,
        ])
        .output()
        .map_err(|_| GitDiffError::GitNotFound)?;

    if output.status.success() {
        Ok(())
    } else {
        Err(GitDiffError::RefNotFound {
            ref_name: ref_name.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    /// Helper: run a `git` command in `dir`, panicking on failure.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-C", &dir.to_string_lossy()])
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("git {:?} failed: {e}", args));
        assert!(
            status.success(),
            "git {:?} failed in {}",
            args,
            dir.display()
        );
    }

    /// Create a temp git repo with one initial commit containing `file`.
    /// Returns the temp dir (kept alive for the test body).
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // Avoid leaking the host's git identity into the test repo.
        git(dir.path(), &["init", "--quiet"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn commit_all(dir: &Path, msg: &str) {
        git(dir, &["add", "--all"]);
        git(dir, &["commit", "--quiet", "-m", msg]);
    }

    /// Skip the test if `git` is not installed on this machine.
    fn require_git() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping test: git not installed");
        }
    }

    #[test]
    fn returns_changed_files_after_modify() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        // Initial commit with one file.
        fs::write(root.join("a.txt"), "initial\n").expect("write a.txt");
        commit_all(root, "initial");

        // Tag the initial state.
        git(root, &["tag", "v1"]);

        // Modify a.txt and add a new file.
        fs::write(root.join("a.txt"), "changed\n").expect("modify a.txt");
        fs::write(root.join("b.txt"), "new\n").expect("write b.txt");
        commit_all(root, "second");

        let changed = changed_files_since(root, "v1").expect("diff since v1");
        let names: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"a.txt".to_owned()),
            "a.txt changed: {names:?}"
        );
        assert!(
            names.contains(&"b.txt".to_owned()),
            "b.txt added: {names:?}"
        );
    }

    #[test]
    fn filters_out_deleted_files() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        fs::write(root.join("keep.txt"), "keep\n").expect("write keep.txt");
        fs::write(root.join("delete.txt"), "gone\n").expect("write delete.txt");
        commit_all(root, "initial");
        git(root, &["tag", "v1"]);

        // Delete delete.txt and modify keep.txt.
        fs::remove_file(root.join("delete.txt")).expect("remove delete.txt");
        fs::write(root.join("keep.txt"), "changed\n").expect("modify keep.txt");
        commit_all(root, "second");

        let changed = changed_files_since(root, "v1").expect("diff since v1");
        let names: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"keep.txt".to_owned()));
        // delete.txt no longer exists on disk -> filtered out.
        assert!(
            !names.iter().any(|n| n == "delete.txt"),
            "deleted file should be filtered: {names:?}"
        );
    }

    #[test]
    fn returns_empty_when_no_changes() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        fs::write(root.join("a.txt"), "data\n").expect("write a.txt");
        commit_all(root, "initial");
        git(root, &["tag", "v1"]);

        let changed = changed_files_since(root, "v1").expect("diff since v1");
        assert!(changed.is_empty(), "no changes since v1: {changed:?}");
    }

    #[test]
    fn paths_are_relative_to_root() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        fs::create_dir_all(root.join("src")).expect("mkdir src");
        fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
        commit_all(root, "initial");
        git(root, &["tag", "v1"]);

        fs::write(root.join("src/main.rs"), "fn main() { println!(); }\n").expect("modify");
        commit_all(root, "second");

        let changed = changed_files_since(root, "v1").expect("diff since v1");
        assert_eq!(changed, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn not_a_repository_errors() {
        require_git();
        let dir = tempfile::tempdir().expect("tempdir");
        let err = changed_files_since(dir.path(), "v1").expect_err("not a repo");
        assert!(matches!(err, GitDiffError::NotARepository(_)));
    }

    #[test]
    fn ref_not_found_errors() {
        require_git();
        let dir = init_repo();
        let root = dir.path();
        fs::write(root.join("a.txt"), "data\n").expect("write a.txt");
        commit_all(root, "initial");

        let err = changed_files_since(root, "nonexistent-tag-xyz").expect_err("ref not found");
        assert!(matches!(err, GitDiffError::RefNotFound { .. }));
    }

    #[test]
    fn works_with_commit_hash_ref() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        fs::write(root.join("a.txt"), "v1\n").expect("write a.txt");
        commit_all(root, "first");

        // Capture the first commit hash.
        let hash = String::from_utf8(
            Command::new("git")
                .args(["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_owned();

        fs::write(root.join("b.txt"), "new\n").expect("write b.txt");
        commit_all(root, "second");

        let changed = changed_files_since(root, &hash).expect("diff since hash");
        let names: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"b.txt".to_owned()),
            "b.txt added: {names:?}"
        );
    }

    #[test]
    fn handles_nested_directory_changes() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        fs::create_dir_all(root.join("a/b/c")).expect("mkdir nested");
        fs::write(root.join("a/b/c/deep.txt"), "deep\n").expect("write deep.txt");
        commit_all(root, "initial");
        git(root, &["tag", "v1"]);

        fs::write(root.join("a/b/c/deep.txt"), "changed\n").expect("modify deep.txt");
        commit_all(root, "second");

        let changed = changed_files_since(root, "v1").expect("diff since v1");
        assert_eq!(changed, vec![PathBuf::from("a/b/c/deep.txt")]);
    }

    #[test]
    fn deduplicates_paths() {
        require_git();
        let dir = init_repo();
        let root = dir.path();

        fs::write(root.join("a.txt"), "1\n").expect("write a.txt");
        commit_all(root, "initial");
        git(root, &["tag", "v1"]);

        // Two commits modifying the same file.
        fs::write(root.join("a.txt"), "2\n").expect("modify 1");
        commit_all(root, "second");
        fs::write(root.join("a.txt"), "3\n").expect("modify 2");
        commit_all(root, "third");

        let changed = changed_files_since(root, "v1").expect("diff since v1");
        assert_eq!(changed, vec![PathBuf::from("a.txt")]);
    }
}
