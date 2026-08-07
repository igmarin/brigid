//! MCP server — `ServerHandler` implementation and stdio transport.
//!
//! Implements ADR 0015 §5 (Transport + CLI). The [`BrigidServer`] struct
//! implements the rmcp `ServerHandler` trait by delegating to the three
//! capability modules:
//!
//! - **Resources** — [`crate::resources`] (manual `list_resources` / `read_resource`)
//! - **Tools** — [`crate::tools`] via `ToolRouter` (manual `call_tool` / `list_tools`)
//! - **Prompts** — [`crate::prompts`] via `PromptRouter` (manual `get_prompt` / `list_prompts`)
//!
//! The server is served over stdio using [`serve`], which loads a checkpoint
//! and runs the MCP protocol until the client disconnects.

use rmcp::RoleServer;
use rmcp::handler::server::prompt::PromptContext;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, ErrorData, GetPromptRequestParams,
    GetPromptResponse, ListPromptsResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ResultType, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, ServiceExt};
use rmcp::transport::stdio;

use crate::CheckpointData;
use crate::prompts::BrigidPrompts;
use crate::resources;
use crate::tools::BrigidTools;

/// The brigid MCP server.
///
/// Holds the loaded checkpoint data and the tool/prompt handler instances.
/// Implements [`rmcp::ServerHandler`] by delegating to the capability modules.
pub struct BrigidServer {
    /// The loaded checkpoint data backing all capabilities.
    pub data: CheckpointData,
    /// Tool handler with `#[tool_router]`-generated router.
    tools: BrigidTools,
    /// Prompt handler with `#[prompt_router]`-generated router.
    prompts: BrigidPrompts,
}

impl BrigidServer {
    /// Create a new server from loaded checkpoint data.
    #[must_use]
    pub fn new(data: CheckpointData) -> Self {
        let tools = BrigidTools::new(data.clone());
        let prompts = BrigidPrompts::new(data.clone());
        Self {
            data,
            tools,
            prompts,
        }
    }
}

impl rmcp::ServerHandler for BrigidServer {
    /// Return server capabilities (tools + prompts + resources) and server info.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "brigid MCP server — exposes codebase analysis checkpoints via resources, tools, and prompts.",
        )
    }

    /// List all available `checkpoint://` resources.
    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + rmcp::service::MaybeSendFuture + '_
    {
        std::future::ready(Ok(ListResourcesResult {
            result_type: Some(ResultType::COMPLETE),
            resources: resources::list_resources(&self.data),
            meta: None,
            next_cursor: None,
            ttl_ms: None,
            cache_scope: None,
        }))
    }

    /// Read a resource by its `checkpoint://` URI.
    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>>
    + rmcp::service::MaybeSendFuture
    + '_ {
        let outcome = resources::read_resource(&request.uri, &self.data);
        std::future::ready(match outcome {
            resources::ReadOutcome::Found(result) => Ok(result.into()),
            resources::ReadOutcome::NotFound(msg) => Err(ErrorData::resource_not_found(msg, None)),
        })
    }

    /// Call a tool by name, delegating to the `ToolRouter` on [`BrigidTools`].
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tcc = ToolCallContext::new(&self.tools, request, context);
        BrigidTools::tool_router().call(tcc).await
    }

    /// List all registered tools.
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + rmcp::service::MaybeSendFuture + '_
    {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        std::future::ready(Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools: BrigidTools::tool_router().list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms: None,
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        }))
    }

    /// Get a prompt by name, delegating to the `PromptRouter` on [`BrigidPrompts`].
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let prompt_context =
            PromptContext::new(&self.prompts, request.name, request.arguments, context);
        BrigidPrompts::prompt_router()
            .get_prompt(prompt_context)
            .await
    }

    /// List all registered prompts.
    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + rmcp::service::MaybeSendFuture + '_
    {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        std::future::ready(Ok(ListPromptsResult {
            result_type: Some(ResultType::COMPLETE),
            prompts: BrigidPrompts::prompt_router().list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms: None,
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        }))
    }
}

/// Load a checkpoint and serve the MCP protocol over stdio.
///
/// # Errors
///
/// Returns an error if the checkpoint cannot be loaded or if the server
/// fails to start.
pub async fn serve(checkpoint_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let loader = crate::CheckpointLoader::new(checkpoint_path);
    let data = loader.load()?;
    let server = BrigidServer::new(data);
    let transport = stdio();
    let running = server.serve(transport).await?;
    running.waiting().await?;
    Ok(())
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
    use rmcp::ServerHandler;
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
        let dir = std::env::temp_dir().join(format!("brigid-mcp-server-{n}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn full_data() -> (PathBuf, CheckpointData) {
        let dir = temp_dir();
        let store = brigid_pipeline::CheckpointStore::new(&dir);
        let cfg = RunConfig::default();
        let mut cp = CheckpointV1::new(&cfg, cfg.redacted_for_checkpoint(), "rev1", "t0").unwrap();
        cp.mark_stage_complete(StageId::Fetch, "t1");
        let files = records_from_files(&[
            ("src/core.rs", b"fn core() {}"),
            ("src/router.rs", b"fn route() {}"),
        ]);
        store.save(cp.clone(), &files).unwrap();

        let mut core = Abstraction::new("Core", "The core system", Tier::L, "module");
        core.file_indices = vec![0];
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
                "# Core\n\nThe core system.",
                Tier::L,
                "module",
                "footer 0",
            ),
            Chapter::new(
                1,
                2,
                "Routing",
                "# Routing\n\nRoutes requests.",
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

        let guide = SetupGuide::new("# Setup\n\nInstall Rust", 42, vec![], true);
        let setup_entry = store.write_setup_guide(&dir, &guide).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Setup, vec![setup_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Setup, "t3");

        let overview = ArchitectureOverview::new("# Architecture\n", vec![]);
        let overview_entry = store.write_architecture_overview(&dir, &overview).unwrap();
        store
            .record_stage_outputs(&mut cp, StageId::Overview, vec![overview_entry])
            .unwrap();
        cp.mark_stage_complete(StageId::Overview, "t4");

        let tutorial = CombinedTutorial::new("# Index\n", 2, true, true, "en");
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
    fn server_get_info_enables_all_capabilities() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let info = server.get_info();
        let caps = &info.capabilities;
        assert!(caps.tools.is_some(), "tools capability should be enabled");
        assert!(
            caps.prompts.is_some(),
            "prompts capability should be enabled"
        );
        assert!(
            caps.resources.is_some(),
            "resources capability should be enabled"
        );
        cleanup(&dir);
    }

    #[test]
    fn server_get_info_has_instructions() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let info = server.get_info();
        assert!(info.instructions.is_some());
        assert!(info.instructions.as_ref().unwrap().contains("brigid"));
        cleanup(&dir);
    }

    #[test]
    fn server_tool_router_lists_all_tools() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let tools = BrigidTools::tool_router().list_all();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"find_abstraction_for_file".to_string()));
        assert!(names.contains(&"abstraction_dependencies".to_string()));
        assert!(names.contains(&"files_for_abstraction".to_string()));
        assert!(names.contains(&"relevance_ranked_chapters".to_string()));
        assert!(names.contains(&"chapter_for_file".to_string()));
        assert!(names.contains(&"list_abstractions".to_string()));
        assert!(names.contains(&"is_checkpoint_stale".to_string()));
        // Suppress unused warning.
        let _ = server;
        cleanup(&dir);
    }

    #[test]
    fn server_prompt_router_lists_all_prompts() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let prompts = BrigidPrompts::prompt_router().list_all();
        let names: Vec<String> = prompts.iter().map(|p| p.name.to_string()).collect();
        assert!(names.contains(&"onboard_to_codebase".to_string()));
        assert!(names.contains(&"explain_file".to_string()));
        assert!(names.contains(&"deep_dive_abstraction".to_string()));
        // Suppress unused warning.
        let _ = server;
        cleanup(&dir);
    }

    #[test]
    fn server_resources_listed_via_module() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let resources = resources::list_resources(&server.data);
        assert!(!resources.is_empty());
        let uris: Vec<String> = resources.iter().map(|r| r.uri.to_string()).collect();
        assert!(uris.contains(&"checkpoint://metadata".to_string()));
        assert!(uris.contains(&"checkpoint://abstractions".to_string()));
        assert!(uris.contains(&"checkpoint://files".to_string()));
        cleanup(&dir);
    }

    #[test]
    fn server_resource_read_via_module() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let outcome = resources::read_resource("checkpoint://metadata", &server.data);
        assert!(matches!(outcome, resources::ReadOutcome::Found(_)));
        cleanup(&dir);
    }

    #[test]
    fn server_resource_read_not_found_via_module() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let outcome = resources::read_resource("checkpoint://nonexistent", &server.data);
        assert!(matches!(outcome, resources::ReadOutcome::NotFound(_)));
        cleanup(&dir);
    }

    #[test]
    fn server_delegates_tool_call_via_tools_handler() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        // Call the tool directly through the BrigidTools handler.
        use crate::tools::FindAbstractionForFileParams;
        use rmcp::handler::server::wrapper::Parameters;
        let result =
            server
                .tools
                .find_abstraction_for_file(Parameters(FindAbstractionForFileParams {
                    file_path: "src/core.rs".to_string(),
                }));
        assert!(result.contains("Core"));
        cleanup(&dir);
    }

    #[test]
    fn server_delegates_prompt_via_prompts_handler() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        let messages = server.prompts.onboard_to_codebase();
        assert!(!messages.is_empty());
        // Should include index content.
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("# Index"));
        cleanup(&dir);
    }

    #[test]
    fn server_delegates_explain_file_prompt() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        use crate::prompts::ExplainFileParams;
        use rmcp::handler::server::wrapper::Parameters;
        let messages = server.prompts.explain_file(Parameters(ExplainFileParams {
            file_path: "src/core.rs".to_string(),
        }));
        assert!(!messages.is_empty());
        let all_text: String = messages
            .iter()
            .filter_map(|m| match &m.content {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("Core"));
        cleanup(&dir);
    }

    #[test]
    fn server_holds_checkpoint_data() {
        let (dir, data) = full_data();
        let server = BrigidServer::new(data);
        // Verify the server has the checkpoint data.
        assert!(server.data.abstractions.is_some());
        assert!(server.data.relationships.is_some());
        assert!(server.data.chapters.is_some());
        assert!(server.data.setup_guide.is_some());
        assert!(server.data.overview.is_some());
        assert!(server.data.combined.is_some());
        cleanup(&dir);
    }
}
