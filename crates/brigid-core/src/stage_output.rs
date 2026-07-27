//! Typed JSON output schemas for pipeline stages.
//!
//! Each pipeline stage produces a typed output struct that can be serialized
//! to JSON for machine-readable consumption. The [`StageOutput`] envelope
//! wraps stage data with metadata for consistent output across all stages.

use crate::abstraction::{Abstraction, Relationship};
use serde::{Deserialize, Serialize};

/// Schema version for forward compatibility.
pub const SCHEMA_VERSION: u32 = 1;

/// Envelope wrapping stage output data with metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct StageOutput<T> {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// The stage name (e.g., "identify", "relationships").
    pub stage: String,
    /// Status: "ok" or "error".
    pub status: StageStatus,
    /// The stage-specific output data.
    pub data: T,
    /// Optional stage statistics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<StageStats>,
}

/// Stage execution status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// Stage completed successfully.
    Ok,
    /// Stage encountered an error.
    Error,
}

impl<T> StageOutput<T> {
    /// Create a new stage output envelope with the given stage name, status,
    /// data, and optional stats.
    #[must_use]
    pub fn new(stage: &str, status: StageStatus, data: T, stats: Option<StageStats>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            stage: stage.to_string(),
            status,
            data,
            stats,
        }
    }
}

/// Stage execution statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct StageStats {
    /// Number of LLM calls made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_calls: Option<u32>,
    /// Elapsed time in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Number of items processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_processed: Option<u32>,
}

/// Output of the **identify** stage: abstractions and their relationships.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct IdentifyOutput {
    /// Identified abstractions, in model output order.
    pub abstractions: Vec<Abstraction>,
    /// Directed edges between abstractions discovered during identification.
    pub relationships: Vec<Relationship>,
}

/// Output of the **relationships** stage: edges and supporting evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct RelationshipsOutput {
    /// Directed edges between abstractions (indices into the abstraction list).
    pub relationships: Vec<Relationship>,
    /// Evidence strings supporting the relationships.
    pub evidence: Vec<String>,
}

/// Output of the **order** stage: pedagogical ordering of chapters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OrderOutput {
    /// Abstraction indices in pedagogical order.
    pub ordered_indices: Vec<usize>,
    /// Chapter titles corresponding to the ordered indices.
    pub titles: Vec<String>,
}

/// Summary of a single written chapter for JSON output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ChapterSummary {
    /// 1-based position in the tutorial.
    pub chapter_num: u32,
    /// Human-readable chapter title.
    pub title: String,
    /// Length of the chapter markdown in bytes.
    pub markdown_length: usize,
    /// Indices into the crawled file inventory backing this chapter.
    pub file_indices: Vec<usize>,
}

/// Output of the **chapters** stage: summaries of written chapters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct ChaptersOutput {
    /// Summaries of written chapters, in tutorial order.
    pub chapters: Vec<ChapterSummary>,
}

/// Output of the **setup** stage: the generated setup guide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct SetupOutput {
    /// Full setup guide markdown.
    pub markdown: String,
    /// Setup assessment score that triggered generation (0-100).
    pub score: u32,
    /// Whether a setup guide was generated (vs. skipped).
    pub generated: bool,
}

/// Output of the **overview** stage: the generated architecture overview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct OverviewOutput {
    /// Full overview markdown.
    pub markdown: String,
    /// Apps named in the overview.
    pub apps: Vec<String>,
    /// Whether an overview was generated (vs. skipped).
    pub generated: bool,
}

/// Output of the **combine** stage: metadata for the final combined tutorial.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct CombineOutput {
    /// Final `index.md` content.
    pub index: String,
    /// Number of chapters in the tutorial.
    pub chapter_count: u32,
    /// Whether the tutorial includes a setup guide.
    pub setup_present: bool,
    /// Whether the tutorial includes an architecture overview.
    pub overview_present: bool,
}

/// Summary of a single pipeline stage execution for the generate output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct StageSummary {
    /// Stage name (e.g., "identify", "chapters").
    pub name: String,
    /// Stage execution status ("ok" or "error").
    pub status: String,
    /// Duration of the stage in milliseconds.
    pub duration_ms: u64,
    /// Number of LLM calls made during the stage.
    pub llm_calls: u32,
}

/// Output of the **generate** stage: full pipeline run summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct GenerateOutput {
    /// Per-stage execution summaries.
    pub stages: Vec<StageSummary>,
    /// Path to the output directory.
    pub output_dir: String,
    /// Path to the checkpoint file.
    pub checkpoint_path: String,
    /// Total LLM calls across all stages.
    pub total_llm_calls: u32,
    /// Total elapsed time in milliseconds.
    pub elapsed_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abstraction::Tier;

    // ---- StageOutput envelope ----

    #[test]
    fn stage_output_envelope_has_required_fields() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "identify".into(),
            status: StageStatus::Ok,
            data: IdentifyOutput {
                abstractions: Vec::new(),
                relationships: Vec::new(),
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("schema_version").is_some());
        assert!(v.get("stage").is_some());
        assert!(v.get("status").is_some());
        assert!(v.get("data").is_some());
        // stats is None and skip_serializing_if => absent
        assert!(v.get("stats").is_none());
    }

    #[test]
    fn stage_output_envelope_includes_stats_when_present() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "chapters".into(),
            status: StageStatus::Ok,
            data: ChaptersOutput {
                chapters: Vec::new(),
            },
            stats: Some(StageStats {
                llm_calls: Some(3),
                elapsed_ms: Some(1500),
                items_processed: Some(2),
            }),
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("stats").is_some());
    }

    #[test]
    fn stage_output_envelope_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "identify".into(),
            status: StageStatus::Ok,
            data: IdentifyOutput {
                abstractions: Vec::new(),
                relationships: Vec::new(),
            },
            stats: Some(StageStats {
                llm_calls: Some(1),
                elapsed_ms: None,
                items_processed: Some(0),
            }),
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<IdentifyOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn stage_status_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&StageStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&StageStatus::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn stage_stats_skips_none_fields() {
        let stats = StageStats {
            llm_calls: Some(2),
            elapsed_ms: None,
            items_processed: None,
        };
        let v: serde_json::Value = serde_json::to_value(&stats).unwrap();
        assert!(v.get("llm_calls").is_some());
        assert!(v.get("elapsed_ms").is_none());
        assert!(v.get("items_processed").is_none());
    }

    #[test]
    fn stage_stats_round_trip() {
        let stats = StageStats {
            llm_calls: Some(5),
            elapsed_ms: Some(3000),
            items_processed: Some(10),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: StageStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back, stats);
    }

    #[test]
    fn stage_stats_default_all_none() {
        let stats = StageStats::default();
        assert!(stats.llm_calls.is_none());
        assert!(stats.elapsed_ms.is_none());
        assert!(stats.items_processed.is_none());
    }

    // ---- IdentifyOutput ----

    #[test]
    fn identify_output_round_trip() {
        let data = IdentifyOutput {
            abstractions: vec![
                Abstraction::new("Query Processing", "Handles queries", Tier::M, "domain"),
                Abstraction::new("Routing", "Routes requests", Tier::S, "module"),
            ],
            relationships: vec![Relationship::new(0, 1, "routes to", "calls")],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: IdentifyOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn identify_output_empty_round_trip() {
        let data = IdentifyOutput {
            abstractions: Vec::new(),
            relationships: Vec::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: IdentifyOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn identify_output_uses_snake_case_fields() {
        let data = IdentifyOutput {
            abstractions: Vec::new(),
            relationships: Vec::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&data).unwrap();
        assert!(v.get("abstractions").is_some());
        assert!(v.get("relationships").is_some());
    }

    // ---- RelationshipsOutput ----

    #[test]
    fn relationships_output_round_trip() {
        let data = RelationshipsOutput {
            relationships: vec![
                Relationship::new(0, 1, "routes to", "calls"),
                Relationship::new(1, 2, "hands off", "publishes"),
            ],
            evidence: vec!["src/router.rs:42".into(), "src/pub.rs:10".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: RelationshipsOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn relationships_output_empty_round_trip() {
        let data = RelationshipsOutput {
            relationships: Vec::new(),
            evidence: Vec::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: RelationshipsOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- OrderOutput ----

    #[test]
    fn order_output_round_trip() {
        let data = OrderOutput {
            ordered_indices: vec![2, 0, 1],
            titles: vec!["Core".into(), "Intro".into(), "Query".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: OrderOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn order_output_empty_round_trip() {
        let data = OrderOutput {
            ordered_indices: Vec::new(),
            titles: Vec::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: OrderOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- ChapterSummary + ChaptersOutput ----

    #[test]
    fn chapter_summary_round_trip() {
        let cs = ChapterSummary {
            chapter_num: 1,
            title: "Query Processing".into(),
            markdown_length: 2048,
            file_indices: vec![0, 3, 7],
        };
        let json = serde_json::to_string(&cs).unwrap();
        let back: ChapterSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cs);
    }

    #[test]
    fn chapters_output_round_trip() {
        let data = ChaptersOutput {
            chapters: vec![
                ChapterSummary {
                    chapter_num: 1,
                    title: "Intro".into(),
                    markdown_length: 100,
                    file_indices: vec![0],
                },
                ChapterSummary {
                    chapter_num: 2,
                    title: "Core".into(),
                    markdown_length: 500,
                    file_indices: vec![1, 2],
                },
            ],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: ChaptersOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn chapters_output_empty_round_trip() {
        let data = ChaptersOutput {
            chapters: Vec::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: ChaptersOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- SetupOutput ----

    #[test]
    fn setup_output_round_trip() {
        let data = SetupOutput {
            markdown: "# Setup\n\nInstall Rust...".into(),
            score: 42,
            generated: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SetupOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn setup_output_not_generated_round_trip() {
        let data = SetupOutput {
            markdown: String::new(),
            score: 0,
            generated: false,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SetupOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- OverviewOutput ----

    #[test]
    fn overview_output_round_trip() {
        let data = OverviewOutput {
            markdown: "# Architecture\n\n...".into(),
            apps: vec!["nexus_hub".into(), "web".into()],
            generated: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: OverviewOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn overview_output_empty_round_trip() {
        let data = OverviewOutput {
            markdown: String::new(),
            apps: Vec::new(),
            generated: false,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: OverviewOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- CombineOutput ----

    #[test]
    fn combine_output_round_trip() {
        let data = CombineOutput {
            index: "# Index\n\n## Chapters".into(),
            chapter_count: 5,
            setup_present: true,
            overview_present: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: CombineOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn combine_output_no_setup_no_overview_round_trip() {
        let data = CombineOutput {
            index: String::new(),
            chapter_count: 0,
            setup_present: false,
            overview_present: false,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: CombineOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- StageSummary + GenerateOutput ----

    #[test]
    fn stage_summary_round_trip() {
        let ss = StageSummary {
            name: "identify".into(),
            status: "ok".into(),
            duration_ms: 1200,
            llm_calls: 3,
        };
        let json = serde_json::to_string(&ss).unwrap();
        let back: StageSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ss);
    }

    #[test]
    fn generate_output_round_trip() {
        let data = GenerateOutput {
            stages: vec![
                StageSummary {
                    name: "identify".into(),
                    status: "ok".into(),
                    duration_ms: 1000,
                    llm_calls: 2,
                },
                StageSummary {
                    name: "chapters".into(),
                    status: "ok".into(),
                    duration_ms: 5000,
                    llm_calls: 5,
                },
            ],
            output_dir: "out/tutorial".into(),
            checkpoint_path: "out/checkpoint.json".into(),
            total_llm_calls: 7,
            elapsed_ms: 6000,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: GenerateOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn generate_output_empty_stages_round_trip() {
        let data = GenerateOutput {
            stages: Vec::new(),
            output_dir: String::new(),
            checkpoint_path: String::new(),
            total_llm_calls: 0,
            elapsed_ms: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: GenerateOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
    }

    // ---- Full envelope round-trips for each stage type ----

    #[test]
    fn envelope_with_relationships_output_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "relationships".into(),
            status: StageStatus::Ok,
            data: RelationshipsOutput {
                relationships: vec![Relationship::new(0, 1, "uses", "calls")],
                evidence: vec!["evidence line".into()],
            },
            stats: Some(StageStats {
                llm_calls: Some(1),
                elapsed_ms: Some(500),
                items_processed: None,
            }),
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<RelationshipsOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn envelope_with_order_output_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "order".into(),
            status: StageStatus::Ok,
            data: OrderOutput {
                ordered_indices: vec![0, 1],
                titles: vec!["A".into(), "B".into()],
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<OrderOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn envelope_with_setup_output_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "setup".into(),
            status: StageStatus::Ok,
            data: SetupOutput {
                markdown: "# Setup".into(),
                score: 30,
                generated: true,
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<SetupOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn envelope_with_overview_output_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "overview".into(),
            status: StageStatus::Ok,
            data: OverviewOutput {
                markdown: "# Overview".into(),
                apps: vec!["app1".into()],
                generated: true,
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<OverviewOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn envelope_with_combine_output_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "combine".into(),
            status: StageStatus::Ok,
            data: CombineOutput {
                index: "# Index".into(),
                chapter_count: 3,
                setup_present: true,
                overview_present: false,
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<CombineOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn envelope_with_generate_output_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "generate".into(),
            status: StageStatus::Ok,
            data: GenerateOutput {
                stages: vec![StageSummary {
                    name: "identify".into(),
                    status: "ok".into(),
                    duration_ms: 100,
                    llm_calls: 1,
                }],
                output_dir: "out".into(),
                checkpoint_path: "cp.json".into(),
                total_llm_calls: 1,
                elapsed_ms: 100,
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: StageOutput<GenerateOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn envelope_error_status_round_trip() {
        let out = StageOutput {
            schema_version: SCHEMA_VERSION,
            stage: "identify".into(),
            status: StageStatus::Error,
            data: IdentifyOutput {
                abstractions: Vec::new(),
                relationships: Vec::new(),
            },
            stats: None,
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "error");
        let back: StageOutput<IdentifyOutput> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out);
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
    }

    // ---- JSON schema stability tests (Issue #223) ----
    //
    // These tests serialize sample data for each stage output type and compare
    // the resulting JSON against a frozen snapshot in
    // `tests/fixtures/json-schemas/`. If a snapshot mismatch occurs, either
    // the schema changed (requires SCHEMA_VERSION bump + snapshot update) or
    // there is a regression.

    use std::path::PathBuf;

    use assert_json_diff::assert_json_eq;

    fn schema_fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/json-schemas")
            .join(format!("{name}.json"))
    }

    fn load_fixture(name: &str) -> serde_json::Value {
        let path = schema_fixture(name);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "failed to read schema fixture {name}: {e} at {}",
                path.display()
            )
        });
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("failed to parse schema fixture {name}: {e}"))
    }

    #[test]
    fn schema_stability_identify() {
        let mut abs1 = Abstraction::new(
            "Query Processing",
            "Handles incoming queries",
            Tier::M,
            "module",
        );
        abs1.file_indices = vec![0, 1];
        let mut abs2 = Abstraction::new("Routing", "Routes requests to handlers", Tier::S, "class");
        abs2.file_indices = vec![2];
        let out = StageOutput::new(
            "identify",
            StageStatus::Ok,
            IdentifyOutput {
                abstractions: vec![abs1, abs2],
                relationships: vec![Relationship::new(0, 1, "routes to", "calls")],
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("identify");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_relationships() {
        let out = StageOutput::new(
            "relationships",
            StageStatus::Ok,
            RelationshipsOutput {
                relationships: vec![
                    Relationship::new(0, 1, "uses", "calls"),
                    Relationship::new(1, 2, "hands off", "publishes"),
                ],
                evidence: vec!["src/router.rs:42".into(), "src/pub.rs:10".into()],
            },
            Some(StageStats {
                llm_calls: Some(1),
                elapsed_ms: Some(500),
                items_processed: None,
            }),
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("relationships");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_order() {
        let out = StageOutput::new(
            "order",
            StageStatus::Ok,
            OrderOutput {
                ordered_indices: vec![2, 0, 1],
                titles: vec!["Core".into(), "Intro".into(), "Query".into()],
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("order");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_chapters() {
        let out = StageOutput::new(
            "chapters",
            StageStatus::Ok,
            ChaptersOutput {
                chapters: vec![
                    ChapterSummary {
                        chapter_num: 1,
                        title: "Intro".into(),
                        markdown_length: 100,
                        file_indices: vec![0],
                    },
                    ChapterSummary {
                        chapter_num: 2,
                        title: "Core".into(),
                        markdown_length: 500,
                        file_indices: vec![1, 2],
                    },
                ],
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("chapters");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_setup() {
        let out = StageOutput::new(
            "setup",
            StageStatus::Ok,
            SetupOutput {
                markdown: "# Setup\n\nInstall Rust...".into(),
                score: 42,
                generated: true,
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("setup");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_overview() {
        let out = StageOutput::new(
            "overview",
            StageStatus::Ok,
            OverviewOutput {
                markdown: "# Architecture\n\n...".into(),
                apps: vec!["nexus_hub".into(), "web".into()],
                generated: true,
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("overview");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_combine() {
        let out = StageOutput::new(
            "combine",
            StageStatus::Ok,
            CombineOutput {
                index: "# Index\n\n## Chapters".into(),
                chapter_count: 5,
                setup_present: true,
                overview_present: true,
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("combine");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_generate() {
        let out = StageOutput::new(
            "generate",
            StageStatus::Ok,
            GenerateOutput {
                stages: vec![
                    StageSummary {
                        name: "identify".into(),
                        status: "ok".into(),
                        duration_ms: 1000,
                        llm_calls: 2,
                    },
                    StageSummary {
                        name: "chapters".into(),
                        status: "ok".into(),
                        duration_ms: 5000,
                        llm_calls: 5,
                    },
                ],
                output_dir: "out/tutorial".into(),
                checkpoint_path: "out/checkpoint.json".into(),
                total_llm_calls: 7,
                elapsed_ms: 6000,
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        let expected = load_fixture("generate");
        assert_json_eq!(actual, expected);
    }

    #[test]
    fn schema_stability_error_envelope() {
        let out = StageOutput::new(
            "identify",
            StageStatus::Error,
            IdentifyOutput {
                abstractions: Vec::new(),
                relationships: Vec::new(),
            },
            None,
        );
        let actual: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(actual["status"], "error");
        assert_eq!(actual["schema_version"], SCHEMA_VERSION);
    }
}
