//! Local filesystem and GitHub repository crawling for `decon`.
//!
//! Local inventory lives in [`local`]. GitHub fetch lands in a later milestone
//! — see `docs/move-to-rust.md` §5.

#![deny(missing_docs)]

pub mod git_diff;
pub mod local;

pub use git_diff::{GitDiffError, changed_files_since, deleted_files_since};
pub use local::{CrawlError, CrawlOptions, CrawlResult, crawl_local, crawl_local_with_options};

/// The version of this crate, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
