//! Setup guide, architecture overview, and combine pipeline domain types
//! (M4 foundation).
//!
//! Pure types for the **setup**, **overview**, and **combine** pipeline
//! stages. **No filesystem I/O** — orchestration lives in `brigid-pipeline`.
//!
//! See `docs/move-to-rust.md` §4.1 for the full domain model.
//!
//! # Design notes
//!
//! - [`SetupGuide`], [`ArchitectureOverview`], and [`CombinedTutorial`] each
//!   provide `to_checkpoint_value` / `from_checkpoint_value` bridge methods so
//!   [`crate::CheckpointV1`]'s `Option<serde_json::Value>` fields stay
//!   compatible without being modified.
//! - [`CombinedTutorial`] intentionally does NOT inline chapter content.
//!   Chapters are stored as files in the checkpoint directory by M4-CHK-1;
//!   this type only tracks metadata.

use serde::{Deserialize, Serialize};

/// Output of the **setup** stage: the generated setup guide.
///
/// Provides [`SetupGuide::to_checkpoint_value`] /
/// [`SetupGuide::from_checkpoint_value`] bridge methods so
/// [`crate::CheckpointV1`]'s `Option<serde_json::Value>` fields stay
/// compatible without being modified.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetupGuide {
    /// Full setup guide markdown.
    pub markdown: String,
    /// Setup assessment score that triggered generation (0-100).
    pub score: i32,
    /// Detected setup doc gaps.
    pub gaps: Vec<String>,
    /// Whether generation was forced via flag or triggered by low score.
    pub forced: bool,
}

impl SetupGuide {
    /// Construct a setup guide.
    #[must_use]
    pub fn new(markdown: impl Into<String>, score: i32, gaps: Vec<String>, forced: bool) -> Self {
        Self {
            markdown: markdown.into(),
            score,
            gaps,
            forced,
        }
    }

    /// Serialize to a [`serde_json::Value`] for storage in a checkpoint.
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`SetupGuide`] is always serializable, but the `Result` return
    /// keeps the API panic-free and symmetric with
    /// [`SetupGuide::from_checkpoint_value`].
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

/// Output of the **overview** stage: the generated architecture overview.
///
/// Provides [`ArchitectureOverview::to_checkpoint_value`] /
/// [`ArchitectureOverview::from_checkpoint_value`] bridge methods so
/// [`crate::CheckpointV1`]'s `Option<serde_json::Value>` fields stay
/// compatible without being modified.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchitectureOverview {
    /// Full overview markdown.
    pub markdown: String,
    /// Apps named in the overview.
    pub app_inventory: Vec<String>,
}

impl ArchitectureOverview {
    /// Construct an architecture overview.
    #[must_use]
    pub fn new(markdown: impl Into<String>, app_inventory: Vec<String>) -> Self {
        Self {
            markdown: markdown.into(),
            app_inventory,
        }
    }

    /// Serialize to a [`serde_json::Value`] for storage in a checkpoint.
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`ArchitectureOverview`] is always serializable, but the `Result`
    /// return keeps the API panic-free and symmetric with
    /// [`ArchitectureOverview::from_checkpoint_value`].
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

/// Output of the **combine** stage: metadata for the final combined tutorial.
///
/// Intentionally does NOT inline chapter content — chapters are stored as
/// files in the checkpoint directory by M4-CHK-1. This type only tracks
/// metadata.
///
/// Provides [`CombinedTutorial::to_checkpoint_value`] /
/// [`CombinedTutorial::from_checkpoint_value`] bridge methods so
/// [`crate::CheckpointV1`]'s `Option<serde_json::Value>` fields stay
/// compatible without being modified.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CombinedTutorial {
    /// Final `index.md` content.
    pub index_markdown: String,
    /// Number of chapters.
    pub chapter_count: usize,
    /// Whether the tutorial includes a setup guide.
    pub has_setup_guide: bool,
    /// Whether the tutorial includes an architecture overview.
    pub has_architecture_overview: bool,
    /// i18n locale used (e.g. `"en"`, `"es"`).
    pub locale: String,
}

impl CombinedTutorial {
    /// Construct a combined tutorial metadata record.
    #[must_use]
    pub fn new(
        index_markdown: impl Into<String>,
        chapter_count: usize,
        has_setup_guide: bool,
        has_architecture_overview: bool,
        locale: impl Into<String>,
    ) -> Self {
        Self {
            index_markdown: index_markdown.into(),
            chapter_count,
            has_setup_guide,
            has_architecture_overview,
            locale: locale.into(),
        }
    }

    /// Serialize to a [`serde_json::Value`] for storage in a checkpoint.
    ///
    /// # Errors
    ///
    /// Propagates serde_json serialization errors. In practice
    /// [`CombinedTutorial`] is always serializable, but the `Result` return
    /// keeps the API panic-free and symmetric with
    /// [`CombinedTutorial::from_checkpoint_value`].
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
    fn setup_guide_serde_round_trip_populated() {
        let guide = SetupGuide::new(
            "# Setup\n\nInstall Rust...",
            42,
            vec!["Missing prerequisites".into(), "No env example".into()],
            true,
        );
        let json = serde_json::to_string(&guide).unwrap();
        let back: SetupGuide = serde_json::from_str(&json).unwrap();
        assert_eq!(back, guide);
    }

    #[test]
    fn setup_guide_serde_round_trip_empty() {
        let guide = SetupGuide::new(String::new(), 0, Vec::new(), false);
        let json = serde_json::to_string(&guide).unwrap();
        let back: SetupGuide = serde_json::from_str(&json).unwrap();
        assert_eq!(back, guide);
        assert!(back.markdown.is_empty());
        assert!(back.gaps.is_empty());
    }

    #[test]
    fn setup_guide_checkpoint_round_trip() {
        let guide = SetupGuide::new("# Setup", 55, vec!["gap a".into()], false);
        let v = guide.to_checkpoint_value().unwrap();
        let back = SetupGuide::from_checkpoint_value(v).unwrap();
        assert_eq!(back, guide);
    }

    #[test]
    fn setup_guide_forced_true() {
        let guide = SetupGuide::new("# Setup", 90, Vec::new(), true);
        assert!(guide.forced);
        let v = guide.to_checkpoint_value().unwrap();
        let back = SetupGuide::from_checkpoint_value(v).unwrap();
        assert!(back.forced);
    }

    #[test]
    fn setup_guide_forced_false() {
        let guide = SetupGuide::new("# Setup", 30, vec!["gap".into()], false);
        assert!(!guide.forced);
        let v = guide.to_checkpoint_value().unwrap();
        let back = SetupGuide::from_checkpoint_value(v).unwrap();
        assert!(!back.forced);
    }

    #[test]
    fn setup_guide_from_invalid_value_errors() {
        let bad = serde_json::json!({"nope": 1});
        assert!(SetupGuide::from_checkpoint_value(bad).is_err());
    }

    #[test]
    fn setup_guide_to_checkpoint_value_is_ok() {
        let guide = SetupGuide::new("# Setup", 50, Vec::new(), false);
        assert!(guide.to_checkpoint_value().is_ok());
    }

    #[test]
    fn architecture_overview_serde_round_trip_populated() {
        let overview = ArchitectureOverview::new(
            "# Architecture\n\n...",
            vec!["nexus_hub".into(), "web".into(), "api".into()],
        );
        let json = serde_json::to_string(&overview).unwrap();
        let back: ArchitectureOverview = serde_json::from_str(&json).unwrap();
        assert_eq!(back, overview);
    }

    #[test]
    fn architecture_overview_serde_round_trip_empty() {
        let overview = ArchitectureOverview::new(String::new(), Vec::new());
        let json = serde_json::to_string(&overview).unwrap();
        let back: ArchitectureOverview = serde_json::from_str(&json).unwrap();
        assert_eq!(back, overview);
        assert!(back.markdown.is_empty());
        assert!(back.app_inventory.is_empty());
    }

    #[test]
    fn architecture_overview_empty_app_inventory() {
        let overview = ArchitectureOverview::new("# Architecture", Vec::new());
        assert!(overview.app_inventory.is_empty());
        let v = overview.to_checkpoint_value().unwrap();
        let back = ArchitectureOverview::from_checkpoint_value(v).unwrap();
        assert!(back.app_inventory.is_empty());
    }

    #[test]
    fn architecture_overview_checkpoint_round_trip() {
        let overview =
            ArchitectureOverview::new("# Architecture", vec!["app1".into(), "app2".into()]);
        let v = overview.to_checkpoint_value().unwrap();
        let back = ArchitectureOverview::from_checkpoint_value(v).unwrap();
        assert_eq!(back, overview);
    }

    #[test]
    fn architecture_overview_from_invalid_value_errors() {
        let bad = serde_json::json!({"nope": 1});
        assert!(ArchitectureOverview::from_checkpoint_value(bad).is_err());
    }

    #[test]
    fn architecture_overview_to_checkpoint_value_is_ok() {
        let overview = ArchitectureOverview::new("# Architecture", Vec::new());
        assert!(overview.to_checkpoint_value().is_ok());
    }

    #[test]
    fn combined_tutorial_serde_round_trip_populated() {
        let tutorial = CombinedTutorial::new("# Index\n\n## Chapters", 5, true, true, "en");
        let json = serde_json::to_string(&tutorial).unwrap();
        let back: CombinedTutorial = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tutorial);
    }

    #[test]
    fn combined_tutorial_serde_round_trip_empty() {
        let tutorial = CombinedTutorial::new(String::new(), 0, false, false, "");
        let json = serde_json::to_string(&tutorial).unwrap();
        let back: CombinedTutorial = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tutorial);
        assert!(back.index_markdown.is_empty());
        assert_eq!(back.chapter_count, 0);
    }

    #[test]
    fn combined_tutorial_checkpoint_round_trip() {
        let tutorial = CombinedTutorial::new("# Index", 3, true, false, "es");
        let v = tutorial.to_checkpoint_value().unwrap();
        let back = CombinedTutorial::from_checkpoint_value(v).unwrap();
        assert_eq!(back, tutorial);
    }

    #[test]
    fn combined_tutorial_with_setup_and_overview() {
        let tutorial = CombinedTutorial::new("# Index", 4, true, true, "en");
        assert!(tutorial.has_setup_guide);
        assert!(tutorial.has_architecture_overview);
        let v = tutorial.to_checkpoint_value().unwrap();
        let back = CombinedTutorial::from_checkpoint_value(v).unwrap();
        assert!(back.has_setup_guide);
        assert!(back.has_architecture_overview);
    }

    #[test]
    fn combined_tutorial_without_setup_and_overview() {
        let tutorial = CombinedTutorial::new("# Index", 4, false, false, "en");
        assert!(!tutorial.has_setup_guide);
        assert!(!tutorial.has_architecture_overview);
        let v = tutorial.to_checkpoint_value().unwrap();
        let back = CombinedTutorial::from_checkpoint_value(v).unwrap();
        assert!(!back.has_setup_guide);
        assert!(!back.has_architecture_overview);
    }

    #[test]
    fn combined_tutorial_locale_preserved() {
        let tutorial = CombinedTutorial::new("# Index", 2, true, true, "es");
        let v = tutorial.to_checkpoint_value().unwrap();
        let back = CombinedTutorial::from_checkpoint_value(v).unwrap();
        assert_eq!(back.locale, "es");
    }

    #[test]
    fn combined_tutorial_from_invalid_value_errors() {
        let bad = serde_json::json!({"nope": 1});
        assert!(CombinedTutorial::from_checkpoint_value(bad).is_err());
    }

    #[test]
    fn combined_tutorial_to_checkpoint_value_is_ok() {
        let tutorial = CombinedTutorial::new("# Index", 1, false, false, "en");
        assert!(tutorial.to_checkpoint_value().is_ok());
    }
}
