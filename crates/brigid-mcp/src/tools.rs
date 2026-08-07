//! MCP tools — graph queries and lookups over the checkpoint knowledge graph.
//!
//! Implements the tool capability matrix from ADR 0015 §4. Each tool is a
//! read-only query over the in-memory [`CheckpointData`] struct. Tools return
//! structured JSON (as a JSON string in the tool content) so the AI assistant
//! can reason over the data without parsing prose.
//!
//! # Tools
//!
//! | Tool | Input | Output |
//! |------|-------|--------|
//! | `find_abstraction_for_file` | `file_path` | Abstraction or null |
//! | `abstraction_dependencies` | `name` | Vec of relationships |
//! | `files_for_abstraction` | `name` | Vec of file paths |
//! | `relevance_ranked_chapters` | `query`, `limit` | Vec of chapter refs |
//! | `chapter_for_file` | `file_path` | Chapter ref or null |
//! | `list_abstractions` | `filter` (optional) | Vec of abstraction refs |
//! | `is_checkpoint_stale` | — | Staleness report |

use std::collections::HashMap;

use brigid_core::{current_git_head, Abstraction, Chapter, Relationship};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::CheckpointData;

// ---------------------------------------------------------------------------
// Parameter types (JSON Schema via schemars)
// ---------------------------------------------------------------------------

/// Parameters for [`BrigidTools::find_abstraction_for_file`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindAbstractionForFileParams {
    /// Relative repository path of the file to look up (POSIX `/` separators).
    pub file_path: String,
}

/// Parameters for [`BrigidTools::abstraction_dependencies`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AbstractionDependenciesParams {
    /// Name of the abstraction to query (case-insensitive).
    pub name: String,
}

/// Parameters for [`BrigidTools::files_for_abstraction`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FilesForAbstractionParams {
    /// Name of the abstraction to query (case-insensitive).
    pub name: String,
}

/// Parameters for [`BrigidTools::relevance_ranked_chapters`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RelevanceRankedChaptersParams {
    /// Natural-language query (e.g. "how does caching work").
    pub query: String,
    /// Maximum number of chapters to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Default value for the `limit` parameter.
fn default_limit() -> usize {
    3
}

/// Parameters for [`BrigidTools::chapter_for_file`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChapterForFileParams {
    /// Relative repository path of the file to look up.
    pub file_path: String,
}

/// Parameters for [`BrigidTools::list_abstractions`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListAbstractionsParams {
    /// Optional kind filter (e.g. "module", "class", "function"). When `None`,
    /// all abstractions are returned.
    pub filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Output types (serialized as JSON in tool content)
// ---------------------------------------------------------------------------

/// A reference to an abstraction in the identify result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbstractionRef {
    /// Index into the abstraction list.
    pub index: usize,
    /// Human-readable name.
    pub name: String,
    /// One-or-two sentence description.
    pub description: String,
    /// Complexity tier (`"S"`, `"M"`, `"L"`).
    pub tier: String,
    /// Free-form kind label.
    pub kind: String,
    /// Monorepo apps touched.
    pub apps: Vec<String>,
    /// Entry files to study first.
    pub entry_files: Vec<String>,
}

/// A reference to a chapter in the tutorial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterRef {
    /// 1-based chapter number.
    pub chapter_num: usize,
    /// Chapter title.
    pub title: String,
    /// Abstraction index this chapter covers.
    pub abstraction_index: usize,
    /// Complexity tier.
    pub tier: String,
    /// Relevance score (only set by `relevance_ranked_chapters`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// A relationship edge with resolved abstraction names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationshipWithNames {
    /// Source abstraction name.
    pub from_name: String,
    /// Target abstraction name.
    pub to_name: String,
    /// Human-readable edge label.
    pub label: String,
    /// Coarse edge kind.
    pub kind: String,
    /// Direction relative to the queried abstraction: `"outgoing"` or
    /// `"incoming"`.
    pub direction: String,
}

/// Staleness report returned by `is_checkpoint_stale`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessReport {
    /// Whether the checkpoint's git commit differs from the current HEAD.
    pub stale: bool,
    /// The git commit recorded in the checkpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_commit: Option<String>,
    /// The current HEAD of the source directory (if accessible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_head: Option<String>,
    /// The source directory path recorded in the checkpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<String>,
    /// Human-readable hint.
    pub hint: String,
}

// ---------------------------------------------------------------------------
// Tool handler implementation
// ---------------------------------------------------------------------------

/// Tool handler for the brigid MCP server.
///
/// Holds a reference to the loaded [`CheckpointData`] and implements all
/// ADR 0015 §4 tools as `#[tool]`-annotated methods. The methods are
/// synchronous (the checkpoint is in memory) and return JSON strings.
#[derive(Debug, Clone)]
pub struct BrigidTools {
    /// The loaded checkpoint data backing all tool queries.
    pub data: CheckpointData,
}

/// Build a file→abstraction-index lookup map from the checkpoint data.
///
/// Returns a map from file path (String) to abstraction index (usize).
fn file_to_abstraction_index(data: &CheckpointData) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    if let Some(abs) = &data.abstractions {
        for (idx, abstraction) in abs.abstractions.iter().enumerate() {
            for &file_idx in &abstraction.file_indices {
                if let Some(file) = data.files.get(file_idx) {
                    map.insert(file.path.clone(), idx);
                }
            }
            for entry_file in &abstraction.entry_files {
                map.entry(entry_file.clone()).or_insert(idx);
            }
        }
    }
    map
}

/// Find an abstraction by name (case-insensitive).
fn find_abstraction_by_name<'a>(
    data: &'a CheckpointData,
    name: &str,
) -> Option<(usize, &'a Abstraction)> {
    let abs = data.abstractions.as_ref()?;
    abs.abstractions
        .iter()
        .enumerate()
        .find(|(_, a)| a.name.eq_ignore_ascii_case(name))
}

/// Convert an [`Abstraction`] to an [`AbstractionRef`].
fn abstraction_to_ref(index: usize, abs: &Abstraction) -> AbstractionRef {
    AbstractionRef {
        index,
        name: abs.name.clone(),
        description: abs.description.clone(),
        tier: abs.tier.as_str().to_string(),
        kind: abs.kind.as_str().to_string(),
        apps: abs.apps.clone(),
        entry_files: abs.entry_files.clone(),
    }
}

/// Convert a [`Chapter`] to a [`ChapterRef`].
fn chapter_to_ref(ch: &Chapter, score: Option<f64>) -> ChapterRef {
    ChapterRef {
        chapter_num: ch.chapter_num,
        title: ch.title.clone(),
        abstraction_index: ch.abstraction_index,
        tier: ch.tier.as_str().to_string(),
        score,
    }
}

/// Collect all relationships involving the abstraction at `index`, with
/// resolved names and direction labels.
fn relationships_for_abstraction(
    data: &CheckpointData,
    index: usize,
) -> Vec<RelationshipWithNames> {
    let rels = match &data.relationships {
        Some(r) => &r.relationships,
        None => return Vec::new(),
    };
    let abs = match &data.abstractions {
        Some(a) => &a.abstractions,
        None => return Vec::new(),
    };

    rels
        .iter()
        .filter_map(|r: &Relationship| {
            if r.from == index {
                let to_name = abs.get(r.to).map(|a| a.name.clone()).unwrap_or_default();
                Some(RelationshipWithNames {
                    from_name: abs.get(index).map(|a| a.name.clone()).unwrap_or_default(),
                    to_name,
                    label: r.label.clone(),
                    kind: r.kind.clone(),
                    direction: "outgoing".to_string(),
                })
            } else if r.to == index {
                let from_name = abs.get(r.from).map(|a| a.name.clone()).unwrap_or_default();
                Some(RelationshipWithNames {
                    from_name,
                    to_name: abs.get(index).map(|a| a.name.clone()).unwrap_or_default(),
                    label: r.label.clone(),
                    kind: r.kind.clone(),
                    direction: "incoming".to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Score a chapter against a query using simple keyword matching.
///
/// Counts how many query tokens (case-insensitive) appear in the chapter's
/// title or markdown. Returns a non-negative score; higher is more relevant.
fn score_chapter(ch: &Chapter, query: &str) -> f64 {
    let query_lower = query.to_lowercase();
    let tokens: Vec<&str> = query_lower.split_whitespace().collect();
    if tokens.is_empty() {
        return 0.0;
    }
    let haystack = format!("{} {}", ch.title, ch.markdown).to_lowercase();
    let mut score = 0.0;
    for token in tokens {
        if token.len() > 2 && haystack.contains(token) {
            score += 1.0;
        }
    }
    score
}

#[tool_router]
impl BrigidTools {
    /// Create a new tool handler backed by the given checkpoint data.
    #[must_use]
    pub fn new(data: CheckpointData) -> Self {
        Self { data }
    }

    /// Find the abstraction that owns a given source file.
    ///
    /// Performs an O(1) lookup via the file→abstraction index. Returns the
    /// abstraction as JSON, or `null` if no abstraction owns the file.
    #[tool(name = "find_abstraction_for_file", description = "Find the abstraction that owns a given source file. Returns the abstraction as JSON or null.")]
    pub fn find_abstraction_for_file(
        &self,
        params: Parameters<FindAbstractionForFileParams>,
    ) -> String {
        let map = file_to_abstraction_index(&self.data);
        let result = map
            .get(&params.0.file_path)
            .and_then(|&idx| {
                self.data
                    .abstractions
                    .as_ref()
                    .and_then(|abs| abs.abstractions.get(idx))
                    .map(|abs| abstraction_to_ref(idx, abs))
            })
            .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "null".to_string())
    }

    /// Get the relationship edges for an abstraction by name.
    ///
    /// Returns both outgoing and incoming relationships, with resolved
    /// abstraction names and direction labels.
    #[tool(name = "abstraction_dependencies", description = "Get the dependency relationships for an abstraction by name. Returns outgoing and incoming edges with resolved names.")]
    pub fn abstraction_dependencies(
        &self,
        params: Parameters<AbstractionDependenciesParams>,
    ) -> String {
        let result = match find_abstraction_by_name(&self.data, &params.0.name) {
            Some((idx, _)) => {
                let rels = relationships_for_abstraction(&self.data, idx);
                serde_json::to_value(&rels).unwrap_or(serde_json::Value::Array(vec![]))
            }
            None => serde_json::Value::Array(vec![]),
        };
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "[]".to_string())
    }

    /// List the source files belonging to an abstraction by name.
    ///
    /// Returns the file paths from the crawl inventory that are mapped to
    /// the abstraction, plus any entry files.
    #[tool(name = "files_for_abstraction", description = "List the source files belonging to an abstraction by name. Returns file paths from the crawl inventory.")]
    pub fn files_for_abstraction(&self, params: Parameters<FilesForAbstractionParams>) -> String {
        let result = match find_abstraction_by_name(&self.data, &params.0.name) {
            Some((_, abs)) => {
                let mut files: Vec<String> = abs
                    .file_indices
                    .iter()
                    .filter_map(|&idx| self.data.files.get(idx).map(|f| f.path.clone()))
                    .collect();
                for ef in &abs.entry_files {
                    if !files.contains(ef) {
                        files.push(ef.clone());
                    }
                }
                serde_json::to_value(&files).unwrap_or(serde_json::Value::Array(vec![]))
            }
            None => serde_json::Value::Array(vec![]),
        };
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "[]".to_string())
    }

    /// Return the top-N chapters most relevant to a natural-language query.
    ///
    /// Uses simple keyword matching for now (counts query token occurrences
    /// in chapter titles and markdown). Chapters are returned in descending
    /// score order, up to `limit`.
    #[tool(name = "relevance_ranked_chapters", description = "Return the top-N chapters most relevant to a natural-language query. Uses keyword matching.")]
    pub fn relevance_ranked_chapters(
        &self,
        params: Parameters<RelevanceRankedChaptersParams>,
    ) -> String {
        let limit = if params.0.limit == 0 {
            default_limit()
        } else {
            params.0.limit
        };
        let result = match &self.data.chapters {
            Some(chapters) => {
                let mut scored: Vec<(f64, &Chapter)> = chapters
                    .chapters
                    .iter()
                    .map(|ch| (score_chapter(ch, &params.0.query), ch))
                    .collect();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                scored
                    .into_iter()
                    .take(limit)
                    .map(|(score, ch)| chapter_to_ref(ch, Some(score)))
                    .collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "[]".to_string())
    }

    /// Find the chapter that explains a given source file.
    ///
    /// Composes `find_abstraction_for_file` with a chapter lookup. Returns
    /// a chapter reference as JSON, or `null` if no chapter covers the file.
    #[tool(name = "chapter_for_file", description = "Find the chapter that explains a given source file. Composes file→abstraction→chapter lookup.")]
    pub fn chapter_for_file(&self, params: Parameters<ChapterForFileParams>) -> String {
        let map = file_to_abstraction_index(&self.data);
        let result = map
            .get(&params.0.file_path)
            .and_then(|&abs_idx| {
                let chapters = self.data.chapters.as_ref()?;
                chapters
                    .chapters
                    .iter()
                    .find(|ch| ch.abstraction_index == abs_idx)
                    .map(|ch| chapter_to_ref(ch, None))
            })
            .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "null".to_string())
    }

    /// List all abstractions, optionally filtered by kind.
    ///
    /// When `filter` is `Some(kind)`, only abstractions whose kind matches
    /// (case-insensitive) are returned. Otherwise all abstractions are listed.
    #[tool(name = "list_abstractions", description = "List all abstractions, optionally filtered by kind. Returns abstraction references with index, name, tier, kind.")]
    pub fn list_abstractions(&self, params: Parameters<ListAbstractionsParams>) -> String {
        let result = match &self.data.abstractions {
            Some(abs) => abs
                .abstractions
                .iter()
                .enumerate()
                .filter(|(_, a)| {
                    params
                        .0
                        .filter
                        .as_ref()
                        .is_none_or(|f| a.kind.as_str().eq_ignore_ascii_case(f))
                })
                .map(|(idx, a)| abstraction_to_ref(idx, a))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "[]".to_string())
    }

    /// Check whether the checkpoint is stale relative to the current git HEAD.
    ///
    /// Compares the `git_commit` recorded in the checkpoint against the
    /// current `HEAD` of the source directory (if it is a git repo and is
    /// accessible). Returns a [`StalenessReport`] as JSON.
    #[tool(name = "is_checkpoint_stale", description = "Check whether the checkpoint is stale by comparing its git_commit to the current HEAD of the source directory.")]
    pub fn is_checkpoint_stale(&self) -> String {
        let checkpoint_commit = self.data.checkpoint.git_commit.clone();
        let source_dir = self.data.checkpoint.source_dir.clone();

        let current_head = source_dir
            .as_ref()
            .and_then(|dir| current_git_head(Path::new(dir)));

        let stale = match (&checkpoint_commit, &current_head) {
            (Some(cp), Some(head)) => cp != head,
            _ => false,
        };

        let hint = if stale {
            "The checkpoint is stale: the codebase has changed since generation. Re-run `brigid generate` to refresh."
        } else if checkpoint_commit.is_none() {
            "No git commit recorded in the checkpoint; staleness cannot be determined."
        } else if source_dir.is_none() {
            "No source directory recorded in the checkpoint; staleness cannot be determined."
        } else {
            "The checkpoint is up to date with the current HEAD."
        };

        let report = StalenessReport {
            stale,
            checkpoint_commit,
            current_head,
            source_dir,
            hint: hint.to_string(),
        };

        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brigid_core::{
        Abstraction, Chapter, ChapterOrder, ChapterResult, CheckpointV1, IdentifyResult,
        Relationship, RelationshipsResult, RunConfig, SetupGuide, StageId, Tier,
    };
    use brigid_core::{ArchitectureOverview, CombinedTutorial};
    use brigid_pipeline::records_from_files;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brigid-mcp-tools-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a fully populated `CheckpointData` with two abstractions and files.
    fn full_data() -> (PathBuf, CheckpointData) {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[
            ("src/core.rs", b"fn core() {}"),
            ("src/router.rs", b"fn route() {}"),
            ("src/util.rs", b"fn util() {}"),
        ]);
        store.save(cp.clone(), &files).unwrap();

        // Abstraction 0 "Core" owns files 0 and 2; abstraction 1 "Routing" owns file 1.
        let mut core = Abstraction::new("Core", "The core system", Tier::M, "module");
        core.file_indices = vec![0, 2];
        core.entry_files = vec!["src/core.rs".to_string()];
        let mut routing = Abstraction::new("Routing", "Routes requests", Tier::S, "class");
        routing.file_indices = vec![1];
        routing.entry_files = vec!["src/router.rs".to_string()];

        let identify = IdentifyResult::new(vec![core, routing]);
        cp.abstractions = Some(identify.to_checkpoint_value().unwrap());

        let relationships = RelationshipsResult::new(
            "A small web framework.",
            vec![Relationship::new(0, 1, "routes to", "calls")],
        );
        cp.relationships = Some(relationships.to_checkpoint_value().unwrap());

        let order = ChapterOrder::new(vec![0, 1]);
        cp.order = Some(order.to_checkpoint_value().unwrap());

        let chapters = ChapterResult::new(vec![
            Chapter::new(
                0,
                1,
                "Core",
                "# Core\n\nThe core system handles caching and state.",
                Tier::M,
                "module",
                "footer 0",
            ),
            Chapter::new(
                1,
                2,
                "Routing",
                "# Routing\n\nThe routing layer dispatches requests.",
                Tier::S,
                "class",
                "footer 1",
            ),
        ]);
        let chapter_entries = store.write_chapters(&dir, &chapters).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Chapters, chapter_entries)
            .unwrap();
        cp.mark_stage_complete(StageId::Chapters, "t2");

        let guide = SetupGuide::new("# Setup", 42, vec![], true);
        let setup_entry = store.write_setup_guide(&dir, &guide).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Setup, vec![setup_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Setup, "t3");

        let overview = ArchitectureOverview::new("# Architecture", vec![]);
        let overview_entry = store.write_architecture_overview(&dir, &overview).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Overview, vec![overview_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Overview, "t4");

        let tutorial = CombinedTutorial::new("# Index", 2, true, true, "en");
        let combine_entry = store.write_combined_index(&dir, &tutorial).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Combine, vec![combine_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Combine, "t5");

        store.save(cp, &files).unwrap();

        let loader = crate::CheckpointLoader::new(&dir);
        let data = loader.load().expect("checkpoint should load");
        (dir, data)
    }

    fn cleanup(dir: &PathBuf) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn find_abstraction_for_file_returns_matching_abstraction() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.find_abstraction_for_file(Parameters(FindAbstractionForFileParams {
            file_path: "src/core.rs".to_string(),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["name"], "Core");
        assert_eq!(parsed["index"], 0);
        cleanup(&dir);
    }

    #[test]
    fn find_abstraction_for_file_unknown_returns_null() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.find_abstraction_for_file(Parameters(FindAbstractionForFileParams {
            file_path: "nonexistent.rs".to_string(),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_null());
        cleanup(&dir);
    }

    #[test]
    fn abstraction_dependencies_returns_edges() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.abstraction_dependencies(Parameters(AbstractionDependenciesParams {
            name: "Core".to_string(),
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["from_name"], "Core");
        assert_eq!(parsed[0]["to_name"], "Routing");
        assert_eq!(parsed[0]["direction"], "outgoing");
        cleanup(&dir);
    }

    #[test]
    fn abstraction_dependencies_case_insensitive() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.abstraction_dependencies(Parameters(AbstractionDependenciesParams {
            name: "routing".to_string(),
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["direction"], "incoming");
        cleanup(&dir);
    }

    #[test]
    fn abstraction_dependencies_unknown_returns_empty() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.abstraction_dependencies(Parameters(AbstractionDependenciesParams {
            name: "Nonexistent".to_string(),
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn files_for_abstraction_returns_file_paths() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.files_for_abstraction(Parameters(FilesForAbstractionParams {
            name: "Core".to_string(),
        }));
        let parsed: Vec<String> = serde_json::from_str(&result).unwrap();
        assert!(parsed.contains(&"src/core.rs".to_string()));
        assert!(parsed.contains(&"src/util.rs".to_string()));
        cleanup(&dir);
    }

    #[test]
    fn files_for_abstraction_unknown_returns_empty() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.files_for_abstraction(Parameters(FilesForAbstractionParams {
            name: "Nonexistent".to_string(),
        }));
        let parsed: Vec<String> = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn relevance_ranked_chapters_returns_sorted_results() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.relevance_ranked_chapters(Parameters(RelevanceRankedChaptersParams {
            query: "caching state".to_string(),
            limit: 2,
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(!parsed.is_empty());
        // "Core" chapter mentions "caching" and "state" — should be first.
        assert_eq!(parsed[0]["title"], "Core");
        assert!(parsed[0]["score"].as_f64().unwrap() > 0.0);
        cleanup(&dir);
    }

    #[test]
    fn relevance_ranked_chapters_respects_limit() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.relevance_ranked_chapters(Parameters(RelevanceRankedChaptersParams {
            query: "routing dispatches".to_string(),
            limit: 1,
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn relevance_ranked_chapters_no_chapters_returns_empty() {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}")]);
        store.save(cp, &files).unwrap();
        let data = crate::CheckpointLoader::new(&dir).load().unwrap();
        let tools = BrigidTools::new(data);
        let result = tools.relevance_ranked_chapters(Parameters(RelevanceRankedChaptersParams {
            query: "anything".to_string(),
            limit: 5,
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn chapter_for_file_returns_chapter_ref() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.chapter_for_file(Parameters(ChapterForFileParams {
            file_path: "src/core.rs".to_string(),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["title"], "Core");
        assert_eq!(parsed["chapter_num"], 1);
        cleanup(&dir);
    }

    #[test]
    fn chapter_for_file_unknown_returns_null() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.chapter_for_file(Parameters(ChapterForFileParams {
            file_path: "nonexistent.rs".to_string(),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_null());
        cleanup(&dir);
    }

    #[test]
    fn list_abstractions_returns_all() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.list_abstractions(Parameters(ListAbstractionsParams {
            filter: None,
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["name"], "Core");
        assert_eq!(parsed[1]["name"], "Routing");
        cleanup(&dir);
    }

    #[test]
    fn list_abstractions_filtered_by_kind() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.list_abstractions(Parameters(ListAbstractionsParams {
            filter: Some("class".to_string()),
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["name"], "Routing");
        cleanup(&dir);
    }

    #[test]
    fn list_abstractions_no_abstractions_returns_empty() {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[("a.rs", b"fn a() {}")]);
        store.save(cp, &files).unwrap();
        let data = crate::CheckpointLoader::new(&dir).load().unwrap();
        let tools = BrigidTools::new(data);
        let result = tools.list_abstractions(Parameters(ListAbstractionsParams {
            filter: None,
        }));
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert!(parsed.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn is_checkpoint_stale_no_git_commit_returns_unknown() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.is_checkpoint_stale();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // No git commit recorded in test checkpoint.
        assert_eq!(parsed["stale"], false);
        assert!(parsed["hint"].as_str().unwrap().contains("No git commit"));
        cleanup(&dir);
    }

    #[test]
    fn is_checkpoint_stale_returns_report_structure() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let result = tools.is_checkpoint_stale();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["hint"].is_string());
        assert!(parsed["stale"].is_boolean());
        cleanup(&dir);
    }

    #[test]
    fn tool_router_generates_tool_definitions() {
        let (dir, data) = full_data();
        let tools = BrigidTools::new(data);
        let router = BrigidTools::tool_router();
        let all = router.list_all();
        let names: Vec<String> = all.iter().map(|t| t.name.to_string()).collect::<Vec<_>>();
        assert!(names.contains(&"find_abstraction_for_file".to_string()));
        assert!(names.contains(&"abstraction_dependencies".to_string()));
        assert!(names.contains(&"files_for_abstraction".to_string()));
        assert!(names.contains(&"relevance_ranked_chapters".to_string()));
        assert!(names.contains(&"chapter_for_file".to_string()));
        assert!(names.contains(&"list_abstractions".to_string()));
        assert!(names.contains(&"is_checkpoint_stale".to_string()));
        // Suppress unused warning.
        let _ = tools;
        cleanup(&dir);
    }
}
