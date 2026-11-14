// ABOUTME: MCP server implementation for CodeGraph code intelligence tools
// ABOUTME: Provides semantic search and direct graph analysis via MCP protocol
#![allow(dead_code, unused_variables, unused_imports)]

use futures::future::BoxFuture;
/// Clean Official MCP SDK Implementation for CodeGraph
/// Following exact Counter pattern from rmcp SDK documentation
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock as Content, GetPromptRequestParams, GetPromptResponse,
        GetPromptResult, ListPromptsResult, NumberOrString, PaginatedRequestParams, Prompt,
        PromptMessage, Role as PromptMessageRole, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::prompts::{INITIAL_INSTRUCTIONS, INITIAL_INSTRUCTIONS_PROMPT_NAME};
use codegraph_mcp_tools::GraphToolExecutor;
use codegraph_vector::EmbeddingGenerator;

/// Parameter structs following official rmcp SDK pattern
// #[derive(Deserialize, JsonSchema)]
// struct IncrementRequest {
//     /// Optional amount to increment (defaults to 1)
//     #[serde(default = "default_increment")]
//     amount: i32,
// }

// fn default_increment() -> i32 { 1 }

#[derive(Deserialize, JsonSchema)]
struct SearchRequest {
    /// The search query for semantic analysis
    query: String,
    /// Maximum number of results to return
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize, JsonSchema)]
struct PlanCheckRequest {
    /// Draft plan object with `nodes`, `edges` and `constraints`
    plan: Value,
}

fn default_limit() -> usize {
    5 // Reduced from 10 for faster agent responses
}

#[derive(Deserialize, JsonSchema)]
struct VectorSearchRequest {
    /// Search query text for vector similarity matching
    query: String,
    /// Optional file paths to restrict search (e.g., ["src/", "lib/"])
    #[serde(default)]
    paths: Option<Vec<String>>,
    /// Optional programming languages to filter (e.g., ["rust", "typescript"])
    #[serde(default)]
    langs: Option<Vec<String>>,
    /// Maximum number of results to return
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize, JsonSchema)]
struct GraphNeighborsRequest {
    /// Node UUID to find neighbors for
    node: String,
    /// Maximum number of neighbors to return
    #[serde(default = "default_neighbor_limit")]
    limit: usize,
}

fn default_neighbor_limit() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
struct GraphTraverseRequest {
    /// Starting node UUID for traversal
    start: String,
    /// Maximum depth to traverse (default: 2)
    #[serde(default = "default_depth")]
    depth: usize,
    /// Maximum number of nodes to return
    #[serde(default = "default_traverse_limit")]
    limit: usize,
}

fn default_depth() -> usize {
    2
}
fn default_traverse_limit() -> usize {
    20 // Reduced from 100 to prevent overwhelming agent responses
}

// #[derive(Deserialize, JsonSchema)]
// struct CodeReadRequest {
//     /// File path to read
//     path: String,
//     /// Starting line number (default: 1)
//     #[serde(default = "default_start_line")]
//     start: usize,
//     /// Optional ending line number (default: end of file)
//     #[serde(default)]
//     end: Option<usize>,
// }

// fn default_start_line() -> usize { 1 }

// #[derive(Deserialize, JsonSchema)]
// struct CodePatchRequest {
//     /// File path to modify
//     path: String,
//     /// Text to find and replace
//     find: String,
//     /// Replacement text
//     replace: String,
//     /// Perform dry run without making changes (default: false)
//     #[serde(default)]
//     dry_run: bool,
// }

// #[derive(Deserialize, JsonSchema)]
// struct TestRunRequest {
//     /// Optional package name to test (e.g., "codegraph-core")
//     #[serde(default)]
//     package: Option<String>,
//     /// Additional cargo test arguments
//     #[serde(default)]
//     args: Option<Vec<String>>,
// }

#[derive(Deserialize, JsonSchema)]
struct SemanticIntelligenceRequest {
    /// Analysis query or focus area for comprehensive codebase analysis
    query: String,
    /// Type of analysis to perform (default: "semantic_search")
    #[serde(default = "default_task_type")]
    task_type: String,
    /// Maximum context tokens to use from 128K available (default: 20000 for faster responses)
    #[serde(default = "default_max_context_tokens")]
    max_context_tokens: usize,
}

fn default_task_type() -> String {
    "semantic_search".to_string()
}
fn default_max_context_tokens() -> usize {
    20000 // Reduced from 80000 for faster responses (30-60s instead of 60-120s)
}

#[derive(Deserialize, JsonSchema)]
struct ImpactAnalysisRequest {
    /// Name of the function to analyze for impact
    target_function: String,
    /// Path to the file containing the target function
    file_path: String,
    /// Type of change being proposed (default: "modify")
    #[serde(default = "default_change_type")]
    change_type: String,
}

fn default_change_type() -> String {
    "modify".to_string()
}

#[derive(Deserialize, JsonSchema)]
struct EmptyRequest {
    /// No parameters required
    #[serde(default)]
    _unused: Option<String>,
}

/// REVOLUTIONARY: Request for intelligent codebase Q&A using RAG
#[derive(Deserialize, JsonSchema)]
struct CodebaseQaRequest {
    /// Natural language question about the codebase
    question: String,
    /// Maximum number of results to consider (default: 5 for faster responses)
    #[serde(default)]
    max_results: Option<usize>,
    /// Enable streaming response (default: false for MCP compatibility)
    #[serde(default)]
    streaming: Option<bool>,
}

/// REVOLUTIONARY: Request for intelligent code documentation generation
#[derive(Deserialize, JsonSchema)]
struct CodeDocumentationRequest {
    /// Function, class, or module name to document
    target_name: String,
    /// Optional file path to focus documentation scope
    #[serde(default)]
    file_path: Option<String>,
    /// Documentation style (default: "comprehensive")
    #[serde(default = "default_doc_style")]
    style: String,
}

fn default_doc_style() -> String {
    "comprehensive".to_string()
}

/// Static parameter object for a direct graph tool.
///
/// Sourced from the shared `GraphToolSchemas` definitions so the MCP `inputSchema`
/// matches what the executor actually accepts (`query`, `node_id`, `edge_type`, the
/// `required` list and enums), instead of an open object that only mentions the
/// shared `content_chars` control. `GraphToolSchemas::all()` already injects
/// `content_chars` into every tool, so it stays available here too.
fn graph_tool_parameters(tool_name: &str) -> Value {
    codegraph_mcp_tools::GraphToolSchemas::all()
        .into_iter()
        .find(|schema| schema.name == tool_name)
        .map(|schema| schema.parameters)
        .unwrap_or_else(|| {
            json!({
                "type": "object",
                "additionalProperties": true,
                "description": "Tool-specific parameters; see the tool description."
            })
        })
}

/// Declare a `Parameters<T>` wrapper whose JSON schema is one graph tool's
/// documented parameters, while still deserializing the raw JSON object unchanged.
///
/// `Parameters<serde_json::Value>` cannot be used directly: schemars emits no root
/// `"type"` for `Value`, and MCP requires `inputSchema.type == "object"`.
macro_rules! graph_tool_params {
    ($ty:ident, $tool:literal) => {
        #[derive(Debug, Clone)]
        pub struct $ty(pub Value);

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Value::deserialize(deserializer).map($ty)
            }
        }

        impl JsonSchema for $ty {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($ty).into()
            }

            fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                graph_tool_parameters($tool)
                    .try_into()
                    .expect("graph tool schema literal is valid")
            }
        }
    };
}

graph_tool_params!(SemanticCodeSearchParams, "semantic_code_search");
graph_tool_params!(GetReverseDependenciesParams, "get_reverse_dependencies");
graph_tool_params!(
    GetTransitiveDependenciesParams,
    "get_transitive_dependencies"
);
graph_tool_params!(TraceCallChainParams, "trace_call_chain");
graph_tool_params!(CalculateCouplingMetricsParams, "calculate_coupling_metrics");
graph_tool_params!(
    DetectCircularDependenciesParams,
    "detect_circular_dependencies"
);
graph_tool_params!(FindComplexityHotspotsParams, "find_complexity_hotspots");
graph_tool_params!(GetHubNodesParams, "get_hub_nodes");

/// Clean CodeGraph MCP server following official Counter pattern
#[derive(Clone)]

pub struct CodeGraphMCPServer {
    /// Simple counter for demonstration
    counter: Arc<Mutex<i32>>,
    /// Official MCP tool router (required by macros)
    tool_router: ToolRouter<Self>,
    /// Lazily built graph-tool executor backing the direct (LLM-free) query tools.
    /// Building it opens the project store and the embedding generator, so it is
    /// created on first use and reused for the server's lifetime.
    graph_tool_executor: Arc<Mutex<Option<Arc<GraphToolExecutor>>>>,
}

#[tool_router]
impl CodeGraphMCPServer {
    pub fn new() -> Self {
        Self {
            counter: Arc::new(Mutex::new(0)),
            tool_router: Self::tool_router(),
            graph_tool_executor: Arc::new(Mutex::new(None)),
        }
    }

    // /// Increment counter with proper parameter schema (DISABLED - redundant for development)
    // #[tool(description = "Increment the counter by a specified amount")]
    // async fn increment(&self, params: Parameters<IncrementRequest>) -> Result<CallToolResult, McpError> {
    //     let request = params.0; // Extract the inner value
    //     let mut counter = self.counter.lock().await;
    //     *counter += request.amount;
    //     Ok(CallToolResult::success(vec![Content::text(format!(
    //         "Counter incremented by {} to: {}",
    //         request.amount,
    //         *counter
    //     ))]))
    // }

    /// Enhanced semantic search with AI-powered analysis for finding code patterns and architectural insights
    // #[tool(
    //     description = "Search code with AI insights (2-5s). Returns relevant code + analysis of patterns and architecture. Use for: understanding code behavior, finding related functionality, discovering patterns. Fast alternative: vector_search. Required: query. Optional: limit (default 5)."
    // )]
    async fn read_initial_instructions(
        &self,
        _params: Parameters<EmptyRequest>,
    ) -> Result<CallToolResult, McpError> {
        let content = format!(
            "{}\n\n---\n\n**Tip:** These instructions are also available as the MCP prompt '{}'.",
            INITIAL_INSTRUCTIONS, INITIAL_INSTRUCTIONS_PROMPT_NAME
        );

        Ok(CallToolResult::success(vec![Content::text(content)]))
    }

    // ----- Direct graph tools (no LLM) ----------------------------------
    //
    // These eight expose graph queries directly to the client, in 0.3–5s,
    // with no LLM in the loop. A client can compose them itself instead of
    // paying for a second LLM pass.

    async fn graph_tool_executor(&self) -> Result<Arc<GraphToolExecutor>, McpError> {
        let mut guard = self.graph_tool_executor.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        let executor = build_graph_tool_executor().await?;
        *guard = Some(executor.clone());
        Ok(executor)
    }

    async fn run_graph_tool(&self, name: &str, params: Value) -> Result<CallToolResult, McpError> {
        let started = std::time::Instant::now();
        let executor = match self.graph_tool_executor().await {
            Ok(executor) => executor,
            Err(error) => {
                let message = error.message.to_string();
                if is_recoverable_tool_error(&message) {
                    // Success-shaped guidance, never isError: an agent that sees errors
                    // early stops calling the tool entirely.
                    log_tool_call(name, "recoverable", started.elapsed().as_millis());
                    return Ok(CallToolResult::success(vec![Content::text(message)]));
                }
                log_tool_call(name, "error", started.elapsed().as_millis());
                return Err(error);
            }
        };
        match executor.execute(name, params).await {
            Ok(result) => {
                log_tool_call(name, "ok", started.elapsed().as_millis());
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()),
                )]))
            }
            Err(error) => {
                let message = format!("{} failed: {}", name, error);
                if is_recoverable_tool_error(&message) {
                    log_tool_call(name, "recoverable", started.elapsed().as_millis());
                    Ok(CallToolResult::success(vec![Content::text(message)]))
                } else {
                    log_tool_call(name, "error", started.elapsed().as_millis());
                    Err(McpError {
                        code: rmcp::model::ErrorCode(-32603),
                        message: message.into(),
                        data: None,
                    })
                }
            }
        }
    }

    #[tool(
        description = "Vector similarity search over indexed code (no LLM). Params: query (string), limit (int, default 10), threshold (number), include_graph_context (bool, optional, default false; set true to also return one-hop incoming_edges/outgoing_edges per row). Each row has vector_score (cosine, filtered by threshold), text_score (BM25 over name+content+doc; ts_rank_cd fallback), and combined_score (RRF rank 1/(60+rank), ordering only); score_semantics restates this."
    )]
    async fn semantic_code_search(
        &self,
        params: Parameters<SemanticCodeSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("semantic_code_search", params.0.0)
            .await
    }

    #[tool(
        description = "Reverse dependencies: who points at this node (no LLM). Params: node_id (string, required, form nodes:<uuid> or a bare uuid), edge_type (string, required, e.g. calls), depth (int). An empty result includes diagnostics with the node's kind, the edge types that do exist, and a suggested call."
    )]
    async fn get_reverse_dependencies(
        &self,
        params: Parameters<GetReverseDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("get_reverse_dependencies", params.0.0)
            .await
    }

    #[tool(
        description = "Transitive dependency closure of a node (no LLM). Params: node_id (string, required), edge_type (string, required), depth (int)."
    )]
    async fn get_transitive_dependencies(
        &self,
        params: Parameters<GetTransitiveDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("get_transitive_dependencies", params.0.0)
            .await
    }

    #[tool(
        description = "Trace the call chain starting at a node (no LLM). Params: node_id (string, required), max_depth (int)."
    )]
    async fn trace_call_chain(
        &self,
        params: Parameters<TraceCallChainParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("trace_call_chain", params.0.0).await
    }

    #[tool(
        description = "Coupling metrics for a node/module (no LLM). Params: node_id (string, required)."
    )]
    async fn calculate_coupling_metrics(
        &self,
        params: Parameters<CalculateCouplingMetricsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("calculate_coupling_metrics", params.0.0)
            .await
    }

    #[tool(
        description = "Detect circular dependency cycles (no LLM). Params: edge_type (string, required, e.g. calls)."
    )]
    async fn detect_circular_dependencies(
        &self,
        params: Parameters<DetectCircularDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("detect_circular_dependencies", params.0.0)
            .await
    }

    #[tool(
        description = "Find complexity hotspots (no LLM). Params: min_complexity (number), limit (int)."
    )]
    async fn find_complexity_hotspots(
        &self,
        params: Parameters<FindComplexityHotspotsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("find_complexity_hotspots", params.0.0)
            .await
    }

    #[tool(description = "Find high-degree hub nodes (no LLM). Params: min_degree (int).")]
    async fn get_hub_nodes(
        &self,
        params: Parameters<GetHubNodesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_graph_tool("get_hub_nodes", params.0.0).await
    }

    #[tool(
        description = "Validate a draft logic graph (no LLM, no DB). Use it to externalise your reasoning while you work — investigating a multi-module behaviour, designing an approach, or comparing options — as well as before writing a new module, a cross-layer change (e.g. cli -> core -> vector), a multi-hop dependency, or any edit to the public interfaces of 3+ files. Write the planned nodes (modules/types), edges (calls/implements) and constraints you believe exist, then call this tool: known_nodes catches references you never declared; layering and acyclic catch design errors; coverage, no_path and max_fan_* check reachability and degree. A rough graph is fine and cheap to iterate on because the check is offline. Params: plan (object with nodes, edges, constraints). Returns ok, checks, violations and warnings, using the same rules as `codegraph plan check`: known_nodes, acyclic, layering, coverage, no_path, max_fan_in, max_fan_out. To compare the plan with the existing code graph use the CLI `codegraph plan --project <root> verify <file>`; `codegraph plan store <file> --plan-id <id>` keeps it for a later session."
    )]
    async fn plan_check(
        &self,
        params: Parameters<PlanCheckRequest>,
    ) -> Result<CallToolResult, McpError> {
        let started = std::time::Instant::now();
        let report = match crate::plan_cli::analyze_value(params.0.plan) {
            Ok(report) => report,
            Err(error) => {
                log_tool_call("plan_check", "recoverable", started.elapsed().as_millis());
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "plan_check could not parse the plan: {error}. Fix the `nodes`/`edges`/`constraints` object and call plan_check again."
                ))]));
            }
        };
        log_tool_call("plan_check", "ok", started.elapsed().as_millis());
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }
}

/// Official MCP ServerHandler implementation (following Counter pattern)
#[tool_handler]
impl ServerHandler for CodeGraphMCPServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_instructions(INITIAL_INSTRUCTIONS)
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, McpError>> + Send + '_ {
        async move {
            Ok(ListPromptsResult {
                prompts: vec![initial_instructions_prompt()],
                next_cursor: None,
                ..Default::default()
            })
        }
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResponse, McpError>> + Send + '_ {
        let name = request.name.clone();
        async move {
            match name.as_str() {
                INITIAL_INSTRUCTIONS_PROMPT_NAME => Ok(GetPromptResult::new(vec![
                    PromptMessage::new_text(PromptMessageRole::User,
                        "Please read the CodeGraph Initial Instructions below. These guidelines will help you use CodeGraph tools efficiently and avoid wasting context by reading unnecessary files."),
                    PromptMessage::new_text(PromptMessageRole::Assistant, INITIAL_INSTRUCTIONS),
                ]).with_description(
                    "MANDATORY: CodeGraph Usage Protocol - You MUST read and follow these instructions before using any CodeGraph tools"
                ).into()),
                _ => Err(McpError::invalid_params(
                    format!("Unknown prompt: {}", name),
                    None
                )),
            }
        }
    }
}

fn initial_instructions_prompt() -> Prompt {
    Prompt::new(
        INITIAL_INSTRUCTIONS_PROMPT_NAME,
        Some("MANDATORY: CodeGraph Usage Protocol - Read before using any CodeGraph tools"),
        None,
    )
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn initial_instructions_prompt_name_is_mcp_compatible() {
        assert_eq!(
            initial_instructions_prompt().name,
            "codegraph:initial_instructions"
        );
    }
}

#[cfg(test)]
mod graph_tool_input_schema_tests {
    use super::*;

    /// Every direct graph tool must advertise its real parameters in the MCP
    /// `inputSchema`. A shared open-object schema that only lists `content_chars`
    /// hides `query`/`node_id`/`edge_type` from LLM clients, so they cannot build a
    /// valid call without guessing from prose.
    #[test]
    fn direct_graph_tools_advertise_their_parameters() {
        let server = CodeGraphMCPServer::new();
        let router = &server.tool_router;

        let cases: &[(&str, &[&str])] = &[
            ("semantic_code_search", &["query", "limit", "threshold"]),
            (
                "get_reverse_dependencies",
                &["node_id", "edge_type", "depth"],
            ),
            (
                "get_transitive_dependencies",
                &["node_id", "edge_type", "depth"],
            ),
            ("trace_call_chain", &["node_id", "max_depth"]),
            ("calculate_coupling_metrics", &["node_id"]),
            ("detect_circular_dependencies", &["edge_type"]),
            ("find_complexity_hotspots", &["min_complexity", "limit"]),
            ("get_hub_nodes", &["min_degree"]),
        ];

        for (name, expected) in cases {
            let tool = router
                .get(name)
                .unwrap_or_else(|| panic!("router is missing `{name}`"));
            let schema = &tool.input_schema;
            let properties = schema
                .get("properties")
                .and_then(|value| value.as_object())
                .unwrap_or_else(|| panic!("`{name}` inputSchema has no properties: {schema:?}"));
            for parameter in *expected {
                assert!(
                    properties.contains_key(*parameter),
                    "`{name}` inputSchema is missing `{parameter}`: {schema:?}"
                );
            }
            assert!(
                properties.contains_key("content_chars"),
                "`{name}` inputSchema dropped the shared `content_chars` control: {schema:?}"
            );
        }
    }

    /// `edge_type` was advertised as `"Calls"`/`"Imports"` while every other surface
    /// (stored values, `Display`, tool descriptions, benchmark scripts) uses
    /// lowercase. The MCP schema must agree and list the full relationship set.
    #[test]
    fn direct_graph_tools_advertise_lowercase_edge_types() {
        let server = CodeGraphMCPServer::new();
        let router = &server.tool_router;

        let expected = [
            "calls",
            "defines",
            "uses",
            "imports",
            "extends",
            "implements",
            "contains",
            "references",
        ];

        let tool_names = [
            "get_reverse_dependencies",
            "get_transitive_dependencies",
            "detect_circular_dependencies",
        ];

        for name in tool_names {
            let tool = router
                .get(name)
                .unwrap_or_else(|| panic!("router is missing `{name}`"));
            let edge_type = tool
                .input_schema
                .get("properties")
                .and_then(|properties| properties.get("edge_type"))
                .unwrap_or_else(|| {
                    panic!(
                        "`{name}` inputSchema has no edge_type: {:?}",
                        tool.input_schema
                    )
                });
            let values: Vec<&str> = edge_type
                .get("enum")
                .and_then(|value| value.as_array())
                .unwrap_or_else(|| panic!("`{name}` edge_type has no enum: {edge_type:?}"))
                .iter()
                .map(|value| value.as_str().unwrap_or_default())
                .collect();

            assert_eq!(values, expected, "`{name}` edge_type enum");
        }
    }
}

/// Build the graph query object for the configured backend.
///
/// `CODEGRAPH_BACKEND` decides: SurrealDB by default, Postgres when selected.
/// Both paths derive the same project id from `CODEGRAPH_PROJECT_ID` or the
/// canonical working directory, because the tools are keyed by it.
async fn build_graph_query(
    project_root: &std::path::Path,
) -> Result<Arc<dyn codegraph_graph::GraphQuery>, McpError> {
    let backend = codegraph_mcp::StorageBackend::from_env().map_err(|error| McpError {
        code: rmcp::model::ErrorCode(-32603),
        message: format!("Invalid CODEGRAPH_BACKEND: {error}").into(),
        data: None,
    })?;

    let env_project = std::env::var("CODEGRAPH_PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let cwd_fallback = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    let raw_project = env_project
        .or(cwd_fallback)
        .unwrap_or_else(|| "default-project".to_string());
    let canonical_project = std::path::Path::new(&raw_project)
        .canonicalize()
        .map(|p| p.display().to_string())
        .unwrap_or(raw_project);

    match backend {
        #[cfg(feature = "surrealdb")]
        codegraph_mcp::StorageBackend::Surreal => {
            use codegraph_graph::{GraphFunctions, SurrealDbStorage, SurrealGraph};
            let config = codegraph_graph::SurrealDbConfig::for_project(project_root);
            let storage = SurrealDbStorage::new(config)
                .await
                .map_err(|error| McpError {
                    code: rmcp::model::ErrorCode(-32603),
                    message: format!("Failed to open SurrealDB storage: {error}").into(),
                    data: None,
                })?;
            Ok(Arc::new(SurrealGraph::new(
                GraphFunctions::new_with_project_id(storage.db(), canonical_project),
            )))
        }
        #[cfg(feature = "postgres")]
        codegraph_mcp::StorageBackend::Postgres => {
            use codegraph_graph::postgres_graph::PgGraph;
            use codegraph_graph::postgres_storage::PgStore;
            let store = PgStore::connect("").map_err(|error| McpError {
                code: rmcp::model::ErrorCode(-32603),
                message: format!("Failed to open Postgres storage: {error}").into(),
                data: None,
            })?;
            // The tools read the graph; they must not create schema.
            if !store.schema_present().await.map_err(|error| McpError {
                code: rmcp::model::ErrorCode(-32603),
                message: format!("Failed to check Postgres schema: {error}").into(),
                data: None,
            })? {
                return Err(McpError {
                    code: rmcp::model::ErrorCode(-32603),
                    message:
                        "project not indexed: the Postgres schema is missing at CODEGRAPH_PG_URL; \
                              run `codegraph index` first"
                            .into(),
                    data: None,
                });
            }
            Ok(Arc::new(PgGraph::new(Arc::new(store), canonical_project)))
        }
        #[cfg(not(feature = "postgres"))]
        codegraph_mcp::StorageBackend::Postgres => Err(McpError {
            code: rmcp::model::ErrorCode(-32603),
            message: "CODEGRAPH_BACKEND=postgres requires a build with --features postgres".into(),
            data: None,
        }),
    }
}

/// Build a `GraphToolExecutor` bound to the project store in the working directory.
///
/// The direct tools build it lazily and share it through `graph_tool_executor`.
async fn build_graph_tool_executor() -> Result<Arc<GraphToolExecutor>, McpError> {
    let config_manager =
        codegraph_core::config_manager::ConfigManager::load().map_err(|e| McpError {
            code: rmcp::model::ErrorCode(-32603),
            message: format!("Failed to load configuration: {e}").into(),
            data: None,
        })?;
    let config = config_manager.config();

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // The tools talk to the backend-neutral trait; the backend decides the adapter.
    let graph = build_graph_query(&project_root).await?;

    let embedding_generator = Arc::new(
        codegraph_vector::EmbeddingGenerator::with_config(&config)
            .await
            .map_err(|e| McpError {
                code: rmcp::model::ErrorCode(-32603),
                message: format!("Embedding input policy initialization failed: {e}").into(),
                data: None,
            })?,
    );

    Ok(Arc::new(GraphToolExecutor::new(
        graph,
        Arc::new(config.clone()),
        embedding_generator,
    )))
}

/// Render one JSONL record for an optional structured tool-call log. Keeping this
/// pure makes the format testable without touching the filesystem or the environment.
fn tool_log_line(tool: &str, outcome: &str, duration_ms: u128, ts: u128) -> String {
    // `tool` and `outcome` are internal literals; strip quotes defensively so a
    // future dynamic name cannot break the one-record-per-line format.
    format!(
        "{{\"ts\":{ts},\"tool\":\"{}\",\"outcome\":\"{}\",\"duration_ms\":{duration_ms}}}\n",
        tool.replace('"', ""),
        outcome.replace('"', "")
    )
}

/// Append one structured record per tool call when `CODEGRAPH_MCP_TOOL_LOG` points
/// at a file. Unset or empty means no I/O and no extra work. Never logs parameters,
/// inputs or outputs. Failures (unwritable path, clock before the epoch) are silent.
fn log_tool_call(tool: &str, outcome: &str, duration_ms: u128) {
    let Ok(path) = std::env::var("CODEGRAPH_MCP_TOOL_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = tool_log_line(tool, outcome, duration_ms, ts);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Conditions the caller can recover from. They must come back as a normal
/// (success-shaped) text result; returning `isError` teaches the agent to stop
/// calling the tool. `isError` stays for real malfunctions (storage down, a
/// backend that is not built in, internal errors).
fn is_recoverable_tool_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("not found")
        || m.contains("not indexed")
        || m.contains("schema is missing")
        || m.contains("no node with id")
        || m.contains("no such node")
        || m.contains("invalid node id")
}

#[cfg(test)]
mod recoverable_error_tests {
    use super::*;

    #[test]
    fn recoverable_conditions_are_classified_as_recoverable() {
        let cases = [
            "Node not found: no node with id `x` in this project",
            "project not indexed: the Postgres schema is missing at CODEGRAPH_PG_URL",
            "no such node",
            "invalid node id",
        ];
        for message in cases {
            assert!(
                is_recoverable_tool_error(message),
                "expected recoverable: {message}"
            );
        }
    }

    #[test]
    fn real_malfunctions_are_not_classified_as_recoverable() {
        let cases = [
            "Failed to open Postgres storage: connection refused",
            "requires a build with --features postgres",
            "internal error",
        ];
        for message in cases {
            assert!(
                !is_recoverable_tool_error(message),
                "expected non-recoverable: {message}"
            );
        }
    }
}

#[cfg(test)]
mod tool_log_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn tool_log_line_is_valid_json() {
        let line = tool_log_line("semantic_code_search", "ok", 42, 1_700_000_000_000);
        assert!(line.ends_with('\n'), "each record is one JSONL line");

        let value: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("tool log line must be valid JSON");
        let object = value.as_object().expect("record must be a JSON object");
        assert_eq!(object.len(), 4, "only the four documented keys are written");
        assert_eq!(value["ts"], 1_700_000_000_000u64);
        assert_eq!(value["tool"], "semantic_code_search");
        assert_eq!(value["outcome"], "ok");
        assert_eq!(value["duration_ms"], 42);
    }

    #[test]
    #[serial]
    fn tool_log_is_disabled_when_env_is_unset() {
        let previous = std::env::var("CODEGRAPH_MCP_TOOL_LOG").ok();
        // SAFETY: `#[serial]` keeps this test from interleaving with the other
        // environment-mutating test in this module, and the value is restored below.
        unsafe { std::env::remove_var("CODEGRAPH_MCP_TOOL_LOG") };

        let dir = tempfile::tempdir().expect("temp dir");
        log_tool_call("get_hub_nodes", "ok", 1);
        let entries = std::fs::read_dir(dir.path())
            .expect("read temp dir")
            .count();
        assert_eq!(entries, 0, "unset CODEGRAPH_MCP_TOOL_LOG must not write");

        match previous {
            Some(value) => unsafe { std::env::set_var("CODEGRAPH_MCP_TOOL_LOG", value) },
            None => unsafe { std::env::remove_var("CODEGRAPH_MCP_TOOL_LOG") },
        }
    }

    #[test]
    #[serial]
    fn tool_log_appends_one_line_when_enabled() {
        let previous = std::env::var("CODEGRAPH_MCP_TOOL_LOG").ok();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tool-calls.jsonl");
        // SAFETY: `#[serial]` serializes environment mutation; restored below.
        unsafe { std::env::set_var("CODEGRAPH_MCP_TOOL_LOG", &path) };

        log_tool_call("plan_check", "recoverable", 7);

        let written = std::fs::read_to_string(&path).expect("log file should exist");
        let last_line = written.lines().last().expect("one JSONL record");
        let value: serde_json::Value =
            serde_json::from_str(last_line).expect("record must be valid JSON");
        assert_eq!(value["tool"], "plan_check");
        assert_eq!(value["outcome"], "recoverable");
        assert_eq!(value["duration_ms"], 7);
        assert!(value["ts"].as_u64().is_some(), "ts is a Unix ms number");

        match previous {
            Some(value) => unsafe { std::env::set_var("CODEGRAPH_MCP_TOOL_LOG", value) },
            None => unsafe { std::env::remove_var("CODEGRAPH_MCP_TOOL_LOG") },
        }
    }
}
