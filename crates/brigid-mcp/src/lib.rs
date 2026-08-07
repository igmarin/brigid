//! MCP server exposing `brigid`'s checkpoint knowledge graph for AI assistants.
//!
//! Implements [ADR 0015](../../docs/adr/0015-mcp-server.md): a read-only
//! [Model Context Protocol](https://modelcontextprotocol.io/) server that
//! loads a `brigid generate` checkpoint directory into memory and serves its
//! structured data (abstractions, relationships, chapter ordering, chapters,
//! setup guide, architecture overview, combined index, file inventory) to AI
//! assistants such as Claude Desktop, Cursor, and Windsurf.
//!
//! # Design
//!
//! - Depends on [`brigid_core`] (pure domain models) and [`brigid_pipeline`]
//!   (checkpoint loading via [`CheckpointStore`][brigid_pipeline::CheckpointStore]).
//!   It does **not** depend on `brigid-cli` or `brigid-llm` — the server is
//!   read-only and never runs pipeline stages or makes LLM calls.
//! - [`CheckpointLoader`] reads `checkpoint.json` plus all completed stage
//!   outputs into a [`CheckpointData`] struct held in memory for the lifetime
//!   of the server process.
//! - Missing stages are represented as `None` fields, so a partially
//!   completed checkpoint loads gracefully and the server can expose whatever
//!   data is available.
//!
//! See the ADR for the full capability matrix (resources, tools, prompts) and
//! transport strategy (stdio first, SSE later).

#![deny(missing_docs)]

pub mod checkpoint_loader;
pub mod prompts;
pub mod resources;
pub mod tools;

pub use checkpoint_loader::{CheckpointData, CheckpointLoader, CheckpointLoaderError, FileEntry};

/// The version of this crate, as declared in `Cargo.toml`.
///
/// Exposed for diagnostics (e.g. `brigid serve --version`) without callers
/// needing to parse `Cargo.toml` themselves.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
