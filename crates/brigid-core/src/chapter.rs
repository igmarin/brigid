//! Chapter ordering + writing domain types (M4 foundation).
//!
//! Pure types for the **order** and **chapters** pipeline stages.
//! **No filesystem I/O** — orchestration lives in `brigid-pipeline`.
//!
//! See `docs/move-to-rust.md` §4.1 for the full domain model.

use crate::abstraction::{AbstractionKind, Tier};
use serde::{Deserialize, Serialize};

/// Pedagogical ordering of abstraction indices (M4 "order" stage).
///
/// `ordered_indices` is a permutation of `0..abstraction_count` placing each
/// abstraction in the order it should be taught.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChapterOrder {
    /// Abstraction indices in pedagogical order.
    pub ordered_indices: Vec<usize>,
}

/// Error returned by [`ChapterOrder::validate`].
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
pub enum ChapterOrderError {
    /// An abstraction index is absent from the ordering.
    #[error("missing abstraction index {index}")]
    MissingAbstraction {
        /// The abstraction index that was missing.
        index: usize,
    },
    /// An abstraction index appears more than once.
    #[error("duplicate abstraction index {index}")]
    DuplicateIndex {
        /// The abstraction index that appeared more than once.
        index: usize,
    },
    /// An index is >= the abstraction count.
    #[error("index {index} out of bounds for abstraction count {count}")]
    OutOfBounds {
        /// The out-of-bounds index.
        index: usize,
        /// The total abstraction count.
        count: usize,
    },
}

impl ChapterOrder {
    /// Construct a chapter order from a vector of indices.
    #[must_use]
    pub fn new(ordered_indices: Vec<usize>) -> Self {
        Self { ordered_indices }
    }

    /// Serialize to a [`serde_json::Value`] for checkpoint storage.
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`ChapterOrder`] is always serializable, but the `Result` return
    /// keeps the API panic-free and symmetric with
    /// [`ChapterOrder::from_checkpoint_value`].
    pub fn to_checkpoint_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// Deserialize from a [`serde_json::Value`] stored in a checkpoint.
    ///
    /// # Errors
    ///
    /// Propagates serde_json deserialization errors.
    pub fn from_checkpoint_value(v: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(v)
    }

    /// Validate that every abstraction appears exactly once, with no
    /// duplicates and no out-of-bounds indices.
    ///
    /// # Errors
    ///
    /// Returns [`ChapterOrderError::OutOfBounds`] if any index >= `abstraction_count`,
    /// [`ChapterOrderError::DuplicateIndex`] if any index appears more than once,
    /// or [`ChapterOrderError::MissingAbstraction`] if any index in
    /// `0..abstraction_count` is absent.
    pub fn validate(&self, abstraction_count: usize) -> Result<(), ChapterOrderError> {
        let mut seen = vec![false; abstraction_count];
        for &idx in &self.ordered_indices {
            if idx >= abstraction_count {
                return Err(ChapterOrderError::OutOfBounds {
                    index: idx,
                    count: abstraction_count,
                });
            }
            if seen[idx] {
                return Err(ChapterOrderError::DuplicateIndex { index: idx });
            }
            seen[idx] = true;
        }
        for (idx, present) in seen.iter().enumerate() {
            if !present {
                return Err(ChapterOrderError::MissingAbstraction { index: idx });
            }
        }
        Ok(())
    }
}

/// A single written tutorial chapter (M4 "chapters" stage).
///
/// Built from an abstraction plus LLM-generated markdown and a grounding
/// evidence footer. See `docs/best-practices.md` §4.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    /// Index into the identify-stage abstraction list.
    pub abstraction_index: usize,
    /// 1-based position in the tutorial.
    pub chapter_num: usize,
    /// Human-readable chapter title.
    pub title: String,
    /// Full chapter markdown content.
    pub markdown: String,
    /// Complexity tier (drives diagram requirements).
    pub tier: Tier,
    /// Free-form kind label (see [`AbstractionKind`]).
    pub kind: AbstractionKind,
    /// Monorepo apps this chapter touches (empty for single-app repos).
    pub apps: Vec<String>,
    /// Best real paths to open first when studying this chapter.
    pub entry_files: Vec<String>,
    /// Grounding metadata (tier, kind, apps, entry files).
    pub evidence_footer: String,
}

impl Chapter {
    /// Construct a chapter with the given identity and content, defaulting
    /// `apps` and `entry_files` to empty.
    #[must_use]
    pub fn new(
        abstraction_index: usize,
        chapter_num: usize,
        title: impl Into<String>,
        markdown: impl Into<String>,
        tier: Tier,
        kind: impl Into<AbstractionKind>,
        evidence_footer: impl Into<String>,
    ) -> Self {
        Self {
            abstraction_index,
            chapter_num,
            title: title.into(),
            markdown: markdown.into(),
            tier,
            kind: kind.into(),
            apps: Vec::new(),
            entry_files: Vec::new(),
            evidence_footer: evidence_footer.into(),
        }
    }
}

/// Output of the **chapters** stage: the list of written chapters.
///
/// Provides [`ChapterResult::to_checkpoint_value`] /
/// [`ChapterResult::from_checkpoint_value`] bridge methods so
/// [`crate::CheckpointV1`]'s `Option<serde_json::Value>` chapter field stays
/// compatible without being modified.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChapterResult {
    /// Written chapters, in tutorial order.
    pub chapters: Vec<Chapter>,
}

impl ChapterResult {
    /// Construct a result from a vector of chapters.
    #[must_use]
    pub fn new(chapters: Vec<Chapter>) -> Self {
        Self { chapters }
    }

    /// Serialize to a [`serde_json::Value`] for checkpoint storage.
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`ChapterResult`] is always serializable, but the `Result` return
    /// keeps the API panic-free and symmetric with
    /// [`ChapterResult::from_checkpoint_value`].
    pub fn to_checkpoint_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// Deserialize from a [`serde_json::Value`] stored in a checkpoint.
    ///
    /// # Errors
    ///
    /// Propagates serde_json deserialization errors.
    pub fn from_checkpoint_value(v: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_order_serde_round_trip() {
        let order = ChapterOrder::new(vec![2, 0, 1]);
        let json = serde_json::to_string(&order).unwrap();
        let back: ChapterOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(back, order);
    }

    #[test]
    fn chapter_serde_round_trip() {
        let ch = Chapter {
            abstraction_index: 1,
            chapter_num: 2,
            title: "Query Processing".into(),
            markdown: "# Query Processing\n\n...".into(),
            tier: Tier::M,
            kind: AbstractionKind::new("domain"),
            apps: vec!["nexus_hub".into(), "web".into()],
            entry_files: vec!["src/query/mod.rs".into()],
            evidence_footer: "tier: M | kind: domain".into(),
        };
        let json = serde_json::to_string(&ch).unwrap();
        let back: Chapter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ch);
    }

    #[test]
    fn chapter_result_serde_round_trip() {
        let result = ChapterResult::new(vec![
            Chapter::new(0, 1, "Intro", "markdown 0", Tier::S, "module", "footer 0"),
            Chapter::new(1, 2, "Core", "markdown 1", Tier::L, "class", "footer 1"),
        ]);
        let json = serde_json::to_string(&result).unwrap();
        let back: ChapterResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn chapter_result_checkpoint_round_trip_populated() {
        let result = ChapterResult::new(vec![
            Chapter::new(0, 1, "Intro", "markdown 0", Tier::S, "module", "footer 0"),
            Chapter::new(1, 2, "Core", "markdown 1", Tier::L, "class", "footer 1"),
        ]);
        let v = result.to_checkpoint_value().unwrap();
        let back = ChapterResult::from_checkpoint_value(v).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn chapter_result_checkpoint_round_trip_empty() {
        let result = ChapterResult::new(Vec::new());
        let v = result.to_checkpoint_value().unwrap();
        let back = ChapterResult::from_checkpoint_value(v).unwrap();
        assert_eq!(back, result);
        assert!(back.chapters.is_empty());
    }

    #[test]
    fn chapter_result_from_invalid_value_errors() {
        let bad = serde_json::json!({"nope": 1});
        assert!(ChapterResult::from_checkpoint_value(bad).is_err());
    }

    #[test]
    fn chapter_result_to_checkpoint_value_is_ok() {
        let result = ChapterResult::new(vec![Chapter::new(0, 1, "Intro", "md", Tier::S, "x", "f")]);
        assert!(result.to_checkpoint_value().is_ok());
    }

    #[test]
    fn validate_valid_order() {
        let order = ChapterOrder::new(vec![2, 0, 1]);
        assert!(order.validate(3).is_ok());
    }

    #[test]
    fn validate_missing_abstraction() {
        let order = ChapterOrder::new(vec![0, 2]);
        let err = order.validate(3).unwrap_err();
        assert_eq!(err, ChapterOrderError::MissingAbstraction { index: 1 });
    }

    #[test]
    fn validate_duplicate() {
        let order = ChapterOrder::new(vec![0, 1, 1]);
        let err = order.validate(3).unwrap_err();
        assert_eq!(err, ChapterOrderError::DuplicateIndex { index: 1 });
    }

    #[test]
    fn validate_out_of_bounds() {
        let order = ChapterOrder::new(vec![0, 1, 3]);
        let err = order.validate(3).unwrap_err();
        assert_eq!(err, ChapterOrderError::OutOfBounds { index: 3, count: 3 });
    }

    #[test]
    fn validate_empty_is_valid() {
        let order = ChapterOrder::new(Vec::new());
        assert!(order.validate(0).is_ok());
    }

    #[test]
    fn validate_single_is_valid() {
        let order = ChapterOrder::new(vec![0]);
        assert!(order.validate(1).is_ok());
    }

    #[test]
    fn validate_empty_indices_with_abstractions_is_missing() {
        let order = ChapterOrder::new(Vec::new());
        let err = order.validate(2).unwrap_err();
        assert_eq!(err, ChapterOrderError::MissingAbstraction { index: 0 });
    }

    #[test]
    fn validate_out_of_bounds_takes_precedence_over_missing() {
        let order = ChapterOrder::new(vec![5]);
        let err = order.validate(2).unwrap_err();
        assert_eq!(err, ChapterOrderError::OutOfBounds { index: 5, count: 2 });
    }

    #[test]
    fn chapter_new_defaults_empty_collections() {
        let ch = Chapter::new(0, 1, "Intro", "md", Tier::S, "module", "footer");
        assert!(ch.apps.is_empty());
        assert!(ch.entry_files.is_empty());
    }
}
