//! Abstraction + Relationship domain types (M3 foundation).
//!
//! Pure types for the **identify** and **relationships** pipeline stages.
//! **No filesystem I/O** — orchestration lives in `decon-pipeline`.
//!
//! See `docs/move-to-rust.md` §4.1 for the full domain model and
//! `docs/best-practices.md` §4 for abstraction-quality guidance.
//!
//! # Design notes
//!
//! - [`Tier`] mirrors the [`crate::StageId`] enum pattern: a closed enum with
//!   `as_str()` / `parse()` and a `#[serde(rename_all = "snake_case")]`
//!   attribute. The Python reference emits the single-letter strings `"S"`,
//!   `"M"`, `"L"`, so we override each variant with `#[serde(rename = ...)]`
//!   to keep the wire format byte-compatible.
//! - [`AbstractionKind`] is a [`String`] newtype rather than a closed enum.
//!   The Python reference uses free-form strings (`"class"`, `"module"`,
//!   `"function"`, `"config"`, `"documentation"`, ...) and new kinds are
//!   expected to appear as heuristics evolve. A newtype keeps serde
//!   round-trips lossless while still giving us a named type (and room to
//!   add validation later) without forcing every consumer to update when a
//!   new kind appears.
//! - [`IdentifyResult`] provides `to_checkpoint_value` /
//!   `from_checkpoint_value` bridge methods so [`crate::CheckpointV1`]'s
//!   `abstractions: Option<serde_json::Value>` field stays compatible
//!   without being modified.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Complexity tier for an [`Abstraction`].
///
/// Drives tutorial depth and diagram requirements — see
/// `docs/best-practices.md` §4.3.
///
/// Wire format is the single-letter strings `"S"`, `"M"`, `"L"` to match the
/// Python reference exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// Small: few files, leaf utility. Short chapter; diagrams optional.
    #[serde(rename = "S")]
    S,
    /// Medium: several modules, clear API. Full outline; ≥1 standard diagram.
    #[serde(rename = "M")]
    M,
    /// Large: many files, multi-app, hubs/orchestrators. Full outline with
    /// structure + sequence diagrams.
    #[serde(rename = "L")]
    L,
}

impl Tier {
    /// Canonical wire string (`"S"`, `"M"`, or `"L"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S => "S",
            Self::M => "M",
            Self::L => "L",
        }
    }

    /// Parse a wire string into a known tier.
    ///
    /// Returns `None` for unrecognized input. Prefer [`Tier::from_str`] when
    /// you need a `Result`-based API.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "S" => Some(Self::S),
            "M" => Some(Self::M),
            "L" => Some(Self::L),
            _ => None,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string cannot be parsed into a [`Tier`].
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("invalid tier: {0:?} (expected \"S\", \"M\", or \"L\")")]
pub struct TierParseError(String);

impl FromStr for Tier {
    type Err = TierParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| TierParseError(s.to_owned()))
    }
}

/// Free-form kind label for an [`Abstraction`].
///
/// The Python reference uses free strings like `"class"`, `"module"`,
/// `"function"`, `"config"`, `"documentation"`, and new kinds are expected
/// to appear as heuristics evolve. This newtype keeps serde round-trips
/// lossless while giving us a named type (and room to add validation
/// later) without forcing every consumer to update when a new kind appears.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AbstractionKind(pub String);

impl AbstractionKind {
    /// Construct a kind from any string-like input.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for AbstractionKind {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AbstractionKind {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// A core concept identified in the codebase (M3 "identify" stage).
///
/// Built from LLM output plus heuristic enrichment (tier, kind, apps,
/// entry files). See `docs/best-practices.md` §4.2.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Abstraction {
    /// Human-readable name (e.g. `"Query Processing"`).
    pub name: String,
    /// One-or-two sentence description of the concept.
    pub description: String,
    /// Indices into the crawled file inventory backing this abstraction.
    pub file_indices: Vec<usize>,
    /// Complexity tier controlling tutorial depth and diagram requirements.
    pub tier: Tier,
    /// Free-form kind label (see [`AbstractionKind`]).
    pub kind: AbstractionKind,
    /// Monorepo apps this abstraction touches (empty for single-app repos).
    pub apps: Vec<String>,
    /// Best real paths to open first when studying this abstraction.
    pub entry_files: Vec<String>,
}

impl Abstraction {
    /// Construct an abstraction with the given name, description, tier, and
    /// kind, defaulting `file_indices`, `apps`, and `entry_files` to empty.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        tier: Tier,
        kind: impl Into<AbstractionKind>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            file_indices: Vec::new(),
            tier,
            kind: kind.into(),
            apps: Vec::new(),
            entry_files: Vec::new(),
        }
    }
}

/// A directed edge between two abstractions (M3 "relationships" stage).
///
/// `from` and `to` are indices into the [`IdentifyResult::abstractions`] list.
/// See `docs/best-practices.md` §5.1 for kind vocabulary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    /// Source abstraction index.
    pub from: usize,
    /// Target abstraction index.
    pub to: usize,
    /// Human-readable edge label.
    pub label: String,
    /// Coarse edge kind (`"calls"`, `"owns"`, `"publishes"`, ...).
    pub kind: String,
}

impl Relationship {
    /// Construct a relationship.
    #[must_use]
    pub fn new(from: usize, to: usize, label: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            from,
            to,
            label: label.into(),
            kind: kind.into(),
        }
    }
}

/// Output of the **identify** stage: the list of abstractions.
///
/// Provides [`IdentifyResult::to_checkpoint_value`] /
/// [`IdentifyResult::from_checkpoint_value`] bridge methods so
/// [`crate::CheckpointV1`]'s `abstractions: Option<serde_json::Value>` field
/// stays compatible without being modified.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentifyResult {
    /// Identified abstractions, in model output order.
    pub abstractions: Vec<Abstraction>,
}

impl IdentifyResult {
    /// Construct a result from a vector of abstractions.
    #[must_use]
    pub fn new(abstractions: Vec<Abstraction>) -> Self {
        Self { abstractions }
    }

    /// Serialize to a [`serde_json::Value`] for storage in
    /// [`crate::CheckpointV1::abstractions`].
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`IdentifyResult`] is always serializable, but the `Result` return
    /// keeps the API panic-free and symmetric with
    /// [`IdentifyResult::from_checkpoint_value`].
    pub fn to_checkpoint_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// Deserialize from a [`serde_json::Value`] stored in
    /// [`crate::CheckpointV1::abstractions`].
    ///
    /// # Errors
    ///
    /// Propagates serde_json deserialization errors.
    pub fn from_checkpoint_value(v: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(v)
    }
}

/// Output of the **relationships** stage: the LLM-generated project summary and
/// the directed edges between abstractions.
///
/// Provides [`RelationshipsResult::to_checkpoint_value`] /
/// [`RelationshipsResult::from_checkpoint_value`] bridge methods so
/// [`crate::CheckpointV1`]'s `relationships: Option<serde_json::Value>` field
/// stays compatible without being modified.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelationshipsResult {
    /// LLM-generated project description.
    pub project_summary: String,
    /// Directed edges between abstractions (indices into
    /// [`IdentifyResult::abstractions`]).
    pub relationships: Vec<Relationship>,
}

impl RelationshipsResult {
    /// Construct a result from a project summary and a vector of
    /// relationships.
    #[must_use]
    pub fn new(project_summary: impl Into<String>, relationships: Vec<Relationship>) -> Self {
        Self {
            project_summary: project_summary.into(),
            relationships,
        }
    }

    /// Serialize to a [`serde_json::Value`] for storage in
    /// [`crate::CheckpointV1::relationships`].
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`RelationshipsResult`] is always serializable, but the `Result` return
    /// keeps the API panic-free and symmetric with
    /// [`RelationshipsResult::from_checkpoint_value`].
    pub fn to_checkpoint_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// Deserialize from a [`serde_json::Value`] stored in
    /// [`crate::CheckpointV1::relationships`].
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
    fn tier_round_trip_wire_names() {
        for tier in [Tier::S, Tier::M, Tier::L] {
            let json = serde_json::to_string(&tier).unwrap();
            let back: Tier = serde_json::from_str(&json).unwrap();
            assert_eq!(back, tier);
            assert_eq!(Tier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(Tier::parse("nope"), None);
    }

    #[test]
    fn tier_serializes_to_single_letter() {
        assert_eq!(serde_json::to_string(&Tier::S).unwrap(), "\"S\"");
        assert_eq!(serde_json::to_string(&Tier::M).unwrap(), "\"M\"");
        assert_eq!(serde_json::to_string(&Tier::L).unwrap(), "\"L\"");
    }

    #[test]
    fn abstraction_serde_round_trip() {
        let a = Abstraction {
            name: "Query Processing".into(),
            description: "Handles incoming queries".into(),
            file_indices: vec![0, 3, 7],
            tier: Tier::M,
            kind: AbstractionKind::new("domain"),
            apps: vec!["nexus_hub".into(), "web".into()],
            entry_files: vec!["src/query/mod.rs".into()],
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: Abstraction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn relationship_serde_round_trip() {
        let r = Relationship {
            from: 0,
            to: 1,
            label: "routes to".into(),
            kind: "calls".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Relationship = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn identify_result_checkpoint_round_trip() {
        let result = IdentifyResult::new(vec![
            Abstraction::new("A", "desc a", Tier::S, "module"),
            Abstraction::new("B", "desc b", Tier::L, "class"),
        ]);
        let v = result.to_checkpoint_value().unwrap();
        let back = IdentifyResult::from_checkpoint_value(v).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn identify_result_empty_round_trip() {
        let result = IdentifyResult::new(Vec::new());
        let v = result.to_checkpoint_value().unwrap();
        let back = IdentifyResult::from_checkpoint_value(v).unwrap();
        assert_eq!(back, result);
        assert!(back.abstractions.is_empty());
    }

    #[test]
    fn abstraction_empty_collections_round_trip() {
        let a = Abstraction::new("Leaf", "tiny util", Tier::S, "function");
        assert!(a.file_indices.is_empty());
        assert!(a.apps.is_empty());
        assert!(a.entry_files.is_empty());
        let json = serde_json::to_string(&a).unwrap();
        let back: Abstraction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn abstraction_kind_newtype_transparent() {
        let k = AbstractionKind::new("documentation");
        assert_eq!(k.as_str(), "documentation");
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(json, "\"documentation\"");
        let back: AbstractionKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn identify_result_from_invalid_value_errors() {
        let bad = serde_json::json!({"nope": 1});
        assert!(IdentifyResult::from_checkpoint_value(bad).is_err());
    }

    #[test]
    fn tier_display_and_from_str() {
        for tier in [Tier::S, Tier::M, Tier::L] {
            assert_eq!(tier.to_string(), tier.as_str());
            assert_eq!(tier.as_str().parse::<Tier>().unwrap(), tier);
        }
        assert!("nope".parse::<Tier>().is_err());
    }

    #[test]
    fn identify_result_to_checkpoint_value_is_ok() {
        let result = IdentifyResult::new(vec![Abstraction::new("A", "d", Tier::S, "x")]);
        assert!(result.to_checkpoint_value().is_ok());
    }

    #[test]
    fn relationships_result_checkpoint_round_trip() {
        let result = RelationshipsResult::new(
            "A small web framework with a routing core and middleware pipeline.",
            vec![
                Relationship::new(0, 1, "routes to", "calls"),
                Relationship::new(1, 2, "hands off", "publishes"),
            ],
        );
        let v = result.to_checkpoint_value().unwrap();
        let back = RelationshipsResult::from_checkpoint_value(v).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn relationships_result_empty_round_trip() {
        let result = RelationshipsResult::new(String::new(), Vec::new());
        let v = result.to_checkpoint_value().unwrap();
        let back = RelationshipsResult::from_checkpoint_value(v).unwrap();
        assert_eq!(back, result);
        assert!(back.relationships.is_empty());
        assert!(back.project_summary.is_empty());
    }

    #[test]
    fn relationships_result_from_invalid_value_errors() {
        let bad = serde_json::json!({"nope": 1});
        assert!(RelationshipsResult::from_checkpoint_value(bad).is_err());
    }

    #[test]
    fn relationships_result_to_checkpoint_value_is_ok() {
        let result =
            RelationshipsResult::new("summary", vec![Relationship::new(0, 1, "label", "kind")]);
        assert!(result.to_checkpoint_value().is_ok());
    }
}
