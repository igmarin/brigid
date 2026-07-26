//! Single-shot **identify** stage: one LLM call produces the full abstraction
//! list for small repos where map/reduce is unnecessary.
//!
//! This is the Rust port of the Python reference's `_single_shot_identify`
//! node. The function takes a [`decon_llm::LlmClient`] (so it works with
//! [`decon_llm::MockClient`] in tests and a real provider client in
//! production), renders the `identify_single_shot` prompt, calls the LLM,
//! extracts the YAML block, parses it into [`Abstraction`]s, and validates the
//! `file_indices` against the crawl inventory.
//!
//! Caching is intentionally NOT handled here — the caller (or a later ticket)
//! wraps the LLM call with [`decon_llm::DiskCache`]. Likewise, heuristic
//! enrichment of `tier`/`kind`/`apps`/`entry_files` beyond what the LLM
//! returns is a separate concern.

#[allow(unused_imports)]
use decon_core::Abstraction;

mod incremental;
mod map;
mod reduce;
mod single_shot;
mod types;

pub use incremental::incremental_identify;
pub use map::{IdentifyMapInput, identify_map};
pub use reduce::{IdentifyReduceInput, identify_reduce};
pub use single_shot::{IdentifySingleShotInput, identify_single_shot};
pub use types::{CandidateAbstraction, CandidateBatch, IdentifyError};

/// Re-export of [`PromptError`] for ergonomic matching at call sites that
/// only depend on `decon-pipeline`.
pub use crate::prompts::PromptError;
/// Re-export of [`decon_core::ExtractError`] for ergonomic matching at call
/// sites that only depend on `decon-pipeline`.
pub use decon_core::ExtractError;
/// Re-export of [`decon_llm::LlmError`] for ergonomic matching at call sites
/// that only depend on `decon-pipeline`.
pub use decon_llm::LlmError;

pub(crate) use map::{batch_files_by_size, run_single_map_batch};
#[allow(unused_imports)]
pub(crate) use map::{parse_candidates, render_map_prompt};
