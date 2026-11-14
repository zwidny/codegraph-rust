// ABOUTME: LLM tool executor for SurrealDB graph analysis functions
// ABOUTME: Executes graph analysis tools by calling Rust SDK wrappers with validated parameters

use codegraph_core::config_manager::CodeGraphConfig;
use codegraph_graph::GraphQuery;
use codegraph_mcp_core::debug_logger::DebugLogger;
use codegraph_mcp_core::error::{McpError, Result};
use codegraph_vector::EmbeddingGenerator;
use codegraph_vector::reranking::{RerankDocument, Reranker, factory::create_reranker};
use lru::LruCache;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

const TOOL_PROGRESS_LOG_TARGET: &str = "codegraph::mcp::tools";

use crate::graph_tool_schemas::GraphToolSchemas;

/// Statistics about LRU cache performance
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheStats {
    /// Number of cache hits (successful lookups)
    pub hits: u64,
    /// Number of cache misses (lookups that required SurrealDB call)
    pub misses: u64,
    /// Number of entries evicted due to LRU policy
    pub evictions: u64,
    /// Current number of entries in cache
    pub current_size: usize,
    /// Maximum cache size (capacity)
    pub max_size: usize,
}

impl CacheStats {
    /// Calculate cache hit rate as percentage
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            (self.hits as f64 / total as f64) * 100.0
        }
    }
}

/// Executor for graph analysis tools
/// Receives tool calls from LLM and executes appropriate SurrealDB functions
pub struct GraphToolExecutor {
    graph: Arc<dyn GraphQuery>,
    /// Configuration for embedding and reranking
    config: Arc<CodeGraphConfig>,
    /// Shared embedding generator (created once, reused for all queries)
    embedding_generator: Arc<EmbeddingGenerator>,
    /// LRU cache for tool results (function_name + params → result)
    cache: Arc<Mutex<LruCache<String, JsonValue>>>,
    /// Cache statistics for observability
    cache_stats: Arc<Mutex<CacheStats>>,
    /// Whether caching is enabled
    cache_enabled: bool,
    /// Reranker for semantic search result refinement
    reranker: Option<Arc<dyn Reranker>>,
    /// Maximum result size in bytes (derived from LLM context window)
    /// Prevents sending oversized results that would exceed model limits
    max_result_bytes: usize,
}

/// Default max result bytes when context window not specified (~200KB)
const DEFAULT_MAX_RESULT_BYTES: usize = 200_000;

/// Ceiling for one tool result regardless of the configured context window. A window of a
/// million tokens does not make a multi-megabyte tool result useful to the model.
const MAX_RESULT_BYTES_CEILING: usize = 200_000;

/// Longest `content` a result row carries. Rows keep their file path and line range, so the
/// client can read the rest; whole documents and impl blocks otherwise dominate the context.
const DEFAULT_CONTENT_SNIPPET_CHARS: usize = 2_000;

/// `CODEGRAPH_TOOL_CONTENT_CHARS` overrides the snippet length; 0 disables shortening.
fn content_snippet_chars() -> usize {
    std::env::var("CODEGRAPH_TOOL_CONTENT_CHARS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_CONTENT_SNIPPET_CHARS)
}

/// Remove `build_context` dependency rows whose manifest lives outside the project tree.
///
/// `cargo metadata` records every locked crate, so registry manifests become graph nodes with
/// no project source behind them. They carry no useful content yet rank highly in
/// natural-language searches. The check only runs when the project id is an absolute path;
/// custom identifiers leave the rows untouched instead of guessing.
fn retain_in_project_rows(rows: Vec<JsonValue>, project_root: &Path) -> Vec<JsonValue> {
    if !project_root.is_absolute() {
        return rows;
    }
    rows.into_iter()
        .filter(|row| {
            let from_build_context = row
                .get("metadata")
                .and_then(|metadata| metadata.get("analyzer"))
                .and_then(|analyzer| analyzer.as_str())
                == Some("build_context");
            if !from_build_context {
                return true;
            }
            row.get("file_path")
                .and_then(|path| path.as_str())
                .map(|path| Path::new(path).starts_with(project_root))
                .unwrap_or(true)
        })
        .collect()
}

/// Explain what each score field from `semantic_code_search` means.
///
/// `combined_score` is an RRF rank value (`1/(60+rank)`), not a similarity, so callers must
/// not read it as confidence and `threshold` does not filter on it.
fn score_semantics() -> JsonValue {
    json!({
        "vector_score": "Cosine similarity from the HNSW chunk search (0..1). The `threshold` parameter filters on this score.",
        "text_score": "BM25 over node name+content+doc (IDF and length normalization; falls back to ts_rank_cd when CODEGRAPH_TEXT_SCORE=ts_rank_cd).",
        "combined_score": "Reciprocal Rank Fusion (RRF) of the vector and text lists, computed as 1/(60+rank). Use it for ordering only; it is not a similarity value.",
        "threshold_applies_to": "vector_score",
        "ordering": "Rows are ordered by combined_score descending."
    })
}

/// Explain an empty dependency result instead of returning a bare empty array.
///
/// Distinguishes a missing node from a node that exists but has no edges of the requested
/// type, suggests the edge types that are actually present, and proposes a concrete retry.
fn empty_result_diagnostics(
    summary: &codegraph_graph::NodeEdgeSummary,
    requested_edge_type: &str,
    requested_node_id: &str,
) -> JsonValue {
    let requested_lower = requested_edge_type.to_ascii_lowercase();
    let available_incoming = &summary.incoming_edge_types;
    let node_id = summary.id.as_deref().unwrap_or(requested_node_id);

    let hint = if !summary.found {
        format!(
            "Node not found: no node with id `{node_id}` exists in this project's index. Check the id, or locate the symbol first with semantic_code_search."
        )
    } else if available_incoming.is_empty() {
        format!(
            "The {} node exists but has no incoming edges of any type. Type/container nodes often keep their relationships on child or member nodes; query a Function, Method, or Import node instead.",
            summary.kind.as_deref().unwrap_or("requested")
        )
    } else {
        format!(
            "The {} node has no incoming `{}` edges. Available incoming edge types: {}. Retry with one of those, or query a Function/Method/Import node.",
            summary.kind.as_deref().unwrap_or("requested"),
            requested_lower,
            available_incoming.join(", ")
        )
    };

    // A retry the caller can issue verbatim beats making it reconstruct the call.
    let suggested_call = available_incoming.first().map(|edge_type| {
        json!({
            "tool": "get_reverse_dependencies",
            "node_id": node_id,
            "edge_type": edge_type,
            "depth": 1,
        })
    });

    json!({
        "node_found": summary.found,
        "node_kind": summary.kind,
        "node_id": node_id,
        "file_path": summary.file_path,
        "start_line": summary.start_line,
        "requested_edge_type": requested_lower,
        "available_incoming_edge_types": available_incoming,
        "available_outgoing_edge_types": &summary.outgoing_edge_types,
        "suggested_call": suggested_call,
        "hint": hint,
    })
}

/// Shorten long `content` strings anywhere in a tool result to a leading snippet that ends
/// on a line boundary where possible, recording the original length next to it.
/// Metadata keys a caller can act on; the rest is index bookkeeping.
const KEPT_METADATA_KEYS: [&str; 2] = ["imports", "qualified_name"];

/// Drop `metadata` keys that only describe the index itself.
///
/// `embedding_model`, `project_id`, `chunk_count`, `analyzer_confidence` and friends tell a
/// model nothing it can use, yet they are the largest field in dependency responses (about a
/// third of `trace_call_chain`). The key disappears entirely when nothing useful remains.
fn trim_metadata(value: &mut JsonValue) {
    match value {
        JsonValue::Array(items) => items.iter_mut().for_each(trim_metadata),
        JsonValue::Object(map) => {
            let empty = match map.get_mut("metadata") {
                Some(JsonValue::Object(metadata)) => {
                    metadata.retain(|key, _| KEPT_METADATA_KEYS.contains(&key.as_str()));
                    metadata.is_empty()
                }
                // The store returns metadata as a JSON string, so filter it in place.
                Some(JsonValue::String(raw)) => match serde_json::from_str::<JsonValue>(raw) {
                    Ok(JsonValue::Object(mut parsed)) => {
                        parsed.retain(|key, _| KEPT_METADATA_KEYS.contains(&key.as_str()));
                        if parsed.is_empty() {
                            true
                        } else {
                            if let Ok(compact) = serde_json::to_string(&parsed) {
                                *raw = compact;
                            }
                            false
                        }
                    }
                    _ => false,
                },
                _ => false,
            };
            if empty {
                map.remove("metadata");
            }
            for child in map.values_mut() {
                trim_metadata(child);
            }
        }
        _ => {}
    }
}

/// The response envelope echoed the request parameters back; the caller still has them.
fn drop_echoed_parameters(value: &mut JsonValue) {
    if let Some(object) = value.as_object_mut() {
        object.remove("parameters");
    }
}

/// `trace_call_chain` repeated every direct caller as a full node object.
///
/// The chain already answers "who calls this", so render the callers as one `id -> name` map:
/// the id stays available for follow-up queries, the name keeps it readable, and the repeated
/// per-entry keys (`id`, `name`, `kind`, ...) disappear.
fn compact_called_by(value: &mut JsonValue) {
    let Some(rows) = value.as_array_mut() else {
        return;
    };
    for row in rows {
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        let mut callers = serde_json::Map::new();
        {
            let Some(entries) = object.get("called_by").and_then(|value| value.as_array()) else {
                continue;
            };
            for entry in entries {
                let Some(name) = entry.get("name").and_then(|name| name.as_str()) else {
                    continue;
                };
                let key = entry
                    .get("id")
                    .and_then(|id| id.as_str())
                    .unwrap_or(name)
                    .to_string();
                callers.insert(key, JsonValue::String(name.to_string()));
            }
        }
        object.insert("called_by".to_string(), JsonValue::Object(callers));
    }
}

/// Resolve the snippet length for one call.
///
/// Precedence: the call's `content_chars`, then `CODEGRAPH_TOOL_CONTENT_CHARS`, then 2000.
/// `0` keeps the full snippet, matching the environment variable's meaning.
fn resolve_content_chars(parameters: &JsonValue) -> usize {
    parameters
        .get("content_chars")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or_else(content_snippet_chars)
}

fn shorten_content(value: &mut JsonValue, max_chars: usize) {
    if max_chars == 0 {
        return;
    }
    match value {
        JsonValue::Array(items) => items
            .iter_mut()
            .for_each(|item| shorten_content(item, max_chars)),
        JsonValue::Object(map) => {
            let original_chars = match map.get("content") {
                Some(JsonValue::String(content)) if content.chars().count() > max_chars => {
                    Some(content.chars().count())
                }
                _ => None,
            };
            if let Some(original_chars) = original_chars {
                if let Some(JsonValue::String(content)) = map.get_mut("content") {
                    let cut = content
                        .char_indices()
                        .nth(max_chars)
                        .map_or(content.len(), |(index, _)| index);
                    // Prefer ending on a complete line when that keeps most of the snippet.
                    let cut = content[..cut]
                        .rfind('\n')
                        .filter(|line_end| *line_end >= cut / 2)
                        .unwrap_or(cut);
                    content.truncate(cut);
                }
                map.insert("content_truncated".to_string(), JsonValue::Bool(true));
                map.insert("content_chars".to_string(), json!(original_chars));
            }
            map.iter_mut()
                .filter(|(key, _)| key.as_str() != "content")
                .for_each(|(_, child)| shorten_content(child, max_chars));
        }
        _ => {}
    }
}

impl GraphToolExecutor {
    /// Create a new tool executor with shared EmbeddingGenerator
    /// Derives max result size from the agent's context window (environment, then
    /// `[llm] context_window` in the config file, then the default)
    pub fn new(
        graph: Arc<dyn GraphQuery>,
        config: Arc<CodeGraphConfig>,
        embedding_generator: Arc<EmbeddingGenerator>,
    ) -> Self {
        let context_window = codegraph_core::config_manager::ConfigManager::agent_context_window();

        Self::with_context_window(graph, config, embedding_generator, context_window)
    }

    /// Create a new tool executor with explicit context window for result size limiting
    /// max_result_bytes = context_window * 2 (about half the window at 4 bytes/token), capped at
    /// `MAX_RESULT_BYTES_CEILING`
    pub fn with_context_window(
        graph: Arc<dyn GraphQuery>,
        config: Arc<CodeGraphConfig>,
        embedding_generator: Arc<EmbeddingGenerator>,
        context_window: usize,
    ) -> Self {
        // Calculate max result bytes: use ~50% of context window, assuming ~4 chars per token
        // This leaves room for system prompt, conversation history, and response
        let max_result_bytes = context_window
            .saturating_mul(2)
            .min(MAX_RESULT_BYTES_CEILING);

        Self::with_limits(
            graph,
            config,
            embedding_generator,
            true,
            100,
            max_result_bytes,
        )
    }

    /// Create a new tool executor with custom cache and result size configuration
    pub fn with_limits(
        graph: Arc<dyn GraphQuery>,
        config: Arc<CodeGraphConfig>,
        embedding_generator: Arc<EmbeddingGenerator>,
        cache_enabled: bool,
        cache_size: usize,
        max_result_bytes: usize,
    ) -> Self {
        let capacity = NonZeroUsize::new(cache_size).unwrap_or(NonZeroUsize::new(100).unwrap());
        let cache = Arc::new(Mutex::new(LruCache::new(capacity)));
        let cache_stats = Arc::new(Mutex::new(CacheStats {
            hits: 0,
            misses: 0,
            evictions: 0,
            current_size: 0,
            max_size: cache_size,
        }));

        // Initialize reranker from config
        let reranker = match create_reranker(&config.rerank) {
            Ok(reranker) => reranker,
            Err(error) => {
                warn!("Configured reranker could not initialize; reranking disabled: {error:#}");
                None
            }
        };

        if let Some(ref reranker) = reranker {
            info!(
                "Reranker initialized: {} ({})",
                reranker.model_name(),
                reranker.provider_name()
            );
        }

        info!(
            "GraphToolExecutor initialized with max_result_bytes: {} ({:.1}MB)",
            max_result_bytes,
            max_result_bytes as f64 / 1_000_000.0
        );

        Self {
            graph,
            config,
            embedding_generator,
            cache,
            cache_stats,
            cache_enabled,
            reranker,
            max_result_bytes,
        }
    }

    /// Create a new tool executor with custom cache configuration (legacy compatibility)
    pub fn with_cache(
        graph: Arc<dyn GraphQuery>,
        config: Arc<CodeGraphConfig>,
        embedding_generator: Arc<EmbeddingGenerator>,
        cache_enabled: bool,
        cache_size: usize,
    ) -> Self {
        Self::with_limits(
            graph,
            config,
            embedding_generator,
            cache_enabled,
            cache_size,
            DEFAULT_MAX_RESULT_BYTES,
        )
    }

    /// Get current cache statistics
    pub fn cache_stats(&self) -> CacheStats {
        self.cache_stats.lock().clone()
    }

    /// Expose underlying graph functions for read-only summaries
    pub fn graph_functions(&self) -> Arc<dyn GraphQuery> {
        self.graph.clone()
    }

    /// Clear the cache and reset statistics
    pub fn clear_cache(&self) {
        let mut cache = self.cache.lock();
        cache.clear();
        let mut stats = self.cache_stats.lock();
        stats.hits = 0;
        stats.misses = 0;
        stats.evictions = 0;
        stats.current_size = 0;
    }

    /// Truncate result if it exceeds max_result_bytes to prevent context window overflow
    /// Returns truncated result with warning metadata if truncation occurred
    fn truncate_if_oversized(&self, tool_name: &str, result: JsonValue) -> JsonValue {
        let serialized = result.to_string();
        let result_bytes = serialized.len();

        if result_bytes <= self.max_result_bytes {
            return result;
        }

        // Result is oversized - need to truncate
        let overflow_ratio = result_bytes as f64 / self.max_result_bytes as f64;
        tracing::warn!(
            tool = tool_name,
            result_bytes = result_bytes,
            max_bytes = self.max_result_bytes,
            overflow_ratio = format!("{:.1}x", overflow_ratio),
            "Tool result exceeds max_result_bytes limit, truncating"
        );

        // Strategy: Try to truncate the "result" field if it's an array
        if let Some(result_array) = result.get("result").and_then(|r| r.as_array()) {
            // Calculate how many items we can keep
            let item_count = result_array.len();
            if item_count == 0 {
                return result;
            }

            // Estimate bytes per item (rough average)
            let bytes_per_item = result_bytes / item_count;
            let max_items = self.max_result_bytes / bytes_per_item.max(1);
            let keep_items = max_items.min(item_count).max(1); // Keep at least 1

            // Create truncated result
            let truncated_array: Vec<JsonValue> =
                result_array.iter().take(keep_items).cloned().collect();
            let truncated_count = item_count - keep_items;

            tracing::info!(
                tool = tool_name,
                original_items = item_count,
                kept_items = keep_items,
                truncated_items = truncated_count,
                "Truncated result array to fit context window"
            );

            // Reconstruct the result with truncation metadata
            let mut truncated_result = result.clone();
            if let Some(obj) = truncated_result.as_object_mut() {
                obj.insert("result".to_string(), JsonValue::Array(truncated_array));
                obj.insert(
                    "_truncated".to_string(),
                    json!({
                        "original_items": item_count,
                        "kept_items": keep_items,
                        "truncated_items": truncated_count,
                        "reason": "Result exceeded context window limit",
                        "max_bytes": self.max_result_bytes
                    }),
                );
            }

            return truncated_result;
        }

        // Fallback: If we can't smart-truncate, just return with a warning
        // This shouldn't happen often since most results are arrays
        tracing::error!(
            tool = tool_name,
            result_bytes = result_bytes,
            "Cannot truncate non-array result, returning as-is (may cause context overflow)"
        );
        result
    }

    /// Generate a cache key from project, tool name, and parameters
    fn cache_key(project_id: &str, tool_name: &str, parameters: &JsonValue) -> String {
        // Create deterministic key from project + function name + serialized params
        format!("{}:{}:{}", project_id, tool_name, parameters.to_string())
    }

    /// Execute a tool call from LLM
    ///
    /// # Arguments
    /// * `tool_name` - Name of the tool to execute
    /// * `parameters` - JSON parameters for the tool
    ///
    /// # Returns
    /// JSON result from the tool execution
    pub async fn execute(&self, tool_name: &str, parameters: JsonValue) -> Result<JsonValue> {
        log_tool_call_start(tool_name, &parameters);

        let exec_result: Result<JsonValue> = async {
            // Validate tool exists
            let _schema = GraphToolSchemas::get_by_name(tool_name)
                .ok_or_else(|| McpError::Protocol(format!("Unknown tool: {}", tool_name)))?;

            let project_id = self.graph.project_id();

            // Check cache if enabled
            if self.cache_enabled {
                let cache_key = Self::cache_key(project_id, tool_name, &parameters);

                // Try cache lookup
                {
                    let mut cache = self.cache.lock();
                    if let Some(cached_result) = cache.get(&cache_key) {
                        // Cache hit
                        let mut stats = self.cache_stats.lock();
                        stats.hits += 1;
                        debug!("Cache hit for {}: {}", tool_name, cache_key);
                        let cached = cached_result.clone();
                        log_tool_call_finish(tool_name, &cached);
                        return Ok(cached);
                    }
                }

                // Cache miss - record it
                {
                    let mut stats = self.cache_stats.lock();
                    stats.misses += 1;
                }
                debug!("Cache miss for {}: {}", tool_name, cache_key);
            }

            // Execute based on tool name
            let result = match tool_name {
                "get_transitive_dependencies" => {
                    self.execute_get_transitive_dependencies(parameters.clone())
                        .await?
                }
                "detect_circular_dependencies" => {
                    self.execute_detect_circular_dependencies(parameters.clone())
                        .await?
                }
                "trace_call_chain" => self.execute_trace_call_chain(parameters.clone()).await?,
                "calculate_coupling_metrics" => {
                    self.execute_calculate_coupling_metrics(parameters.clone())
                        .await?
                }
                "get_hub_nodes" => self.execute_get_hub_nodes(parameters.clone()).await?,
                "get_reverse_dependencies" => {
                    self.execute_get_reverse_dependencies(parameters.clone())
                        .await?
                }
                "semantic_code_search" => {
                    self.execute_semantic_code_search(parameters.clone())
                        .await?
                }
                "find_complexity_hotspots" => {
                    self.execute_find_complexity_hotspots(parameters.clone())
                        .await?
                }
                _ => {
                    return Err(
                        McpError::Protocol(format!("Tool not implemented: {}", tool_name)).into(),
                    );
                }
            };

            // Shape the result for a model: content snippets first, then the size limit.
            let mut result = result;
            shorten_content(&mut result, resolve_content_chars(&parameters));
            trim_metadata(&mut result);
            drop_echoed_parameters(&mut result);
            let result = self.truncate_if_oversized(tool_name, result);

            // Cache the result if enabled
            if self.cache_enabled {
                let cache_key = Self::cache_key(project_id, tool_name, &parameters);
                let mut cache = self.cache.lock();
                let was_evicted = cache.len() >= cache.cap().get();
                cache.put(cache_key, result.clone());

                // Update stats
                let mut stats = self.cache_stats.lock();
                if was_evicted {
                    stats.evictions += 1;
                }
                stats.current_size = cache.len();
            }

            Ok(result)
        }
        .await;

        match exec_result {
            Ok(result) => {
                log_tool_call_finish(tool_name, &result);
                Ok(result)
            }
            Err(err) => {
                warn!(
                    target: TOOL_PROGRESS_LOG_TARGET,
                    tool = tool_name,
                    parameters = %parameters,
                    error = %err,
                    "Tool call failed"
                );
                DebugLogger::log_tool_error(tool_name, &parameters, &format!("{}", err));
                Err(err)
            }
        }
    }

    /// Execute get_transitive_dependencies
    async fn execute_get_transitive_dependencies(&self, params: JsonValue) -> Result<JsonValue> {
        let node_id = params["node_id"]
            .as_str()
            .ok_or_else(|| McpError::Protocol("Missing node_id".to_string()))?;

        // Default to "calls" if edge_type not provided (for LATS compatibility)
        let edge_type = params["edge_type"].as_str().unwrap_or("calls");

        let depth = params["depth"].as_i64().unwrap_or(3) as i32;

        let result = self
            .graph
            .get_transitive_dependencies(node_id, edge_type, depth)
            .await
            .map_err(|e| {
                McpError::Protocol(format!("get_transitive_dependencies failed: {}", e))
            })?;

        Ok(json!({
            "tool": "get_transitive_dependencies",
            "parameters": {
                "node_id": node_id,
                "edge_type": edge_type,
                "depth": depth
            },
            "result": result
        }))
    }

    /// Execute detect_circular_dependencies
    async fn execute_detect_circular_dependencies(&self, params: JsonValue) -> Result<JsonValue> {
        let edge_type = params["edge_type"]
            .as_str()
            .ok_or_else(|| McpError::Protocol("Missing edge_type".to_string()))?;

        let result = self
            .graph
            .detect_circular_dependencies(edge_type)
            .await
            .map_err(|e| {
                McpError::Protocol(format!("detect_circular_dependencies failed: {}", e))
            })?;

        Ok(json!({
            "tool": "detect_circular_dependencies",
            "parameters": {
                "edge_type": edge_type
            },
            "result": result
        }))
    }

    /// Execute trace_call_chain
    async fn execute_trace_call_chain(&self, params: JsonValue) -> Result<JsonValue> {
        // Accept both "from_node" (canonical) and "node_id" (common pattern) for compatibility
        let from_node = params["from_node"]
            .as_str()
            .or_else(|| params["node_id"].as_str())
            .ok_or_else(|| McpError::Protocol("Missing from_node or node_id".to_string()))?;

        let max_depth = params["max_depth"].as_i64().unwrap_or(5) as i32;

        let chain = self
            .graph
            .trace_call_chain(from_node, max_depth)
            .await
            .map_err(|e| McpError::Protocol(format!("trace_call_chain failed: {}", e)))?;
        let mut result = serde_json::to_value(&chain)
            .map_err(|e| McpError::Protocol(format!("trace_call_chain encode failed: {}", e)))?;
        compact_called_by(&mut result);

        Ok(json!({
            "tool": "trace_call_chain",
            "parameters": {
                "from_node": from_node,
                "max_depth": max_depth
            },
            "result": result
        }))
    }

    /// Execute calculate_coupling_metrics
    async fn execute_calculate_coupling_metrics(&self, params: JsonValue) -> Result<JsonValue> {
        let node_id = params["node_id"]
            .as_str()
            .ok_or_else(|| McpError::Protocol("Missing node_id".to_string()))?;

        // Optional project_id override to reduce mismatches between indexing and runtime
        let gf = if let Some(project_id) = params["project_id"].as_str() {
            self.graph.with_project_id(project_id)
        } else {
            Arc::clone(&self.graph)
        };

        let result = gf
            .calculate_coupling_metrics(node_id)
            .await
            .map_err(|e| McpError::Protocol(format!("calculate_coupling_metrics failed: {}", e)))?;

        Ok(json!({
            "tool": "calculate_coupling_metrics",
            "parameters": {
                "node_id": node_id
            },
            "result": result
        }))
    }

    /// Execute get_hub_nodes
    async fn execute_get_hub_nodes(&self, params: JsonValue) -> Result<JsonValue> {
        let min_degree = params["min_degree"].as_i64().unwrap_or(5) as i32;

        let result = self
            .graph
            .get_hub_nodes(min_degree)
            .await
            .map_err(|e| McpError::Protocol(format!("get_hub_nodes failed: {}", e)))?;

        Ok(json!({
            "tool": "get_hub_nodes",
            "parameters": {
                "min_degree": min_degree
            },
            "result": result
        }))
    }

    /// Execute get_reverse_dependencies
    async fn execute_get_reverse_dependencies(&self, params: JsonValue) -> Result<JsonValue> {
        let node_id = params["node_id"]
            .as_str()
            .ok_or_else(|| McpError::Protocol("Missing node_id".to_string()))?;

        // Default to "calls" if edge_type not provided (for LATS compatibility)
        let edge_type = params["edge_type"].as_str().unwrap_or("calls");

        let depth = params["depth"].as_i64().unwrap_or(3) as i32;

        let result = self
            .graph
            .get_reverse_dependencies(node_id, edge_type, depth)
            .await
            .map_err(|e| McpError::Protocol(format!("get_reverse_dependencies failed: {}", e)))?;

        let is_empty = result.is_empty();
        let mut response = json!({
            "tool": "get_reverse_dependencies",
            "parameters": {
                "node_id": node_id,
                "edge_type": edge_type,
                "depth": depth
            },
            "result": result
        });

        // An empty array cannot distinguish "no such node" from "no edges of this type".
        // Attach a summary so the caller can pick a usable edge type instead of retrying
        // blind. Diagnostics must never turn a successful empty result into an error.
        if is_empty {
            match self.graph.get_node_edge_summary(node_id).await {
                Ok(summary) => {
                    response["diagnostics"] =
                        empty_result_diagnostics(&summary, edge_type, node_id);
                }
                Err(error) => {
                    debug!("reverse dependency diagnostics unavailable: {}", error);
                }
            }
        }

        Ok(response)
    }

    /// Execute semantic code search with HNSW, full-text, and graph enrichment
    /// Accepts natural language queries for comprehensive semantic search
    async fn execute_semantic_code_search(&self, params: JsonValue) -> Result<JsonValue> {
        let query_text = params["query"]
            .as_str()
            .ok_or_else(|| McpError::Protocol("Missing query".to_string()))?;

        let limit = params["limit"].as_i64().unwrap_or(10) as usize;
        let threshold = params["threshold"]
            .as_f64()
            .or_else(|| {
                std::env::var("CODEGRAPH_SEMSEARCH_THRESHOLD")
                    .ok()?
                    .parse::<f64>()
                    .ok()
            })
            .map(|v| v.clamp(0.0, 1.0))
            .unwrap_or(0.4);

        // Step 1: Generate embedding using shared EmbeddingGenerator
        let t_embed = std::time::Instant::now();
        let query_embedding = self
            .embedding_generator
            .generate_text_embedding(query_text)
            .await
            .map_err(|e| McpError::Protocol(format!("Embedding generation failed: {}", e)))?;
        let embed_us = t_embed.elapsed().as_micros() as u64;

        // Step 2: Get embedding dimension from shared generator (auto-detected)
        let dimension = self.embedding_generator.dimension();

        // Step 3: Call semantic search function, optionally with graph enrichment.
        // Graph context is off by default to keep responses small; callers opt in
        // with `include_graph_context: true` when they need one-hop edges.
        let include_graph_context = params["include_graph_context"].as_bool().unwrap_or(false);

        let t_search = std::time::Instant::now();
        let candidates = self
            .graph
            .semantic_search_with_context(
                query_text,
                &query_embedding,
                dimension,
                limit,
                threshold as f32,
                include_graph_context,
            )
            .await
            .map_err(|e| {
                McpError::Protocol(format!("semantic_search_with_context failed: {}", e))
            })?;

        let search_us = t_search.elapsed().as_micros() as u64;
        tracing::debug!(
            target: "codegraph::semsearch",
            embed_us,
            search_us,
            "semantic search: embedding vs retrieval"
        );

        // Step 4: Apply reranking if configured (Jina OR LM Studio)
        let final_results = self.apply_reranking(query_text, candidates).await?;

        // Step 5: Drop dependency-graph rows that point outside the project. `cargo metadata`
        // records every locked crate, so registry manifests otherwise surface as search hits
        // with no project source behind them.
        let project_id = self.graph.project_id().to_string();
        let final_results = retain_in_project_rows(final_results, Path::new(&project_id));

        Ok(json!({
            "tool": "semantic_code_search",
            "parameters": {
                "query": query_text,
                "limit": limit,
                "dimension": dimension,
                "threshold": threshold
            },
            // `combined_score` is an RRF rank (1/(60+rank)), so name each score explicitly
            // instead of leaving callers to guess whether it is a similarity.
            "score_semantics": score_semantics(),
            "result": final_results
        }))
    }

    /// Execute find_complexity_hotspots - find functions with high complexity and coupling
    async fn execute_find_complexity_hotspots(&self, params: JsonValue) -> Result<JsonValue> {
        let min_complexity = params["min_complexity"].as_f64().unwrap_or(5.0) as f32;
        let limit = params["limit"].as_i64().unwrap_or(20) as i32;

        let result = self
            .graph
            .get_complexity_hotspots(min_complexity, limit)
            .await
            .map_err(|e| McpError::Protocol(format!("find_complexity_hotspots failed: {}", e)))?;

        Ok(json!({
            "tool": "find_complexity_hotspots",
            "parameters": {
                "min_complexity": min_complexity,
                "limit": limit
            },
            "result": result
        }))
    }

    /// Apply reranking if configured using text-based reranking system
    async fn apply_reranking(
        &self,
        query: &str,
        candidates: Vec<serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>> {
        if let Some(ref reranker) = self.reranker {
            let top_n = self.config.rerank.top_n;

            // Convert candidates to RerankDocuments
            let documents: Vec<RerankDocument> = candidates
                .iter()
                .enumerate()
                .map(|(idx, candidate)| RerankDocument {
                    id: idx.to_string(),
                    text: Self::extract_text_from_candidate(candidate),
                    metadata: Some(candidate.clone()),
                })
                .collect();

            // Rerank using the configured provider
            let results = reranker
                .rerank(query, documents, top_n)
                .await
                .map_err(|e| McpError::Protocol(format!("Reranking failed: {}", e)))?;

            // Convert back to original format
            let reranked: Vec<serde_json::Value> =
                results.into_iter().filter_map(|r| r.metadata).collect();

            Ok(reranked)
        } else {
            // No reranking configured
            Ok(candidates)
        }
    }

    /// Extract text content from a candidate for reranking
    fn extract_text_from_candidate(candidate: &serde_json::Value) -> String {
        let mut text_parts = Vec::new();

        if let Some(name) = candidate.get("name").and_then(|v| v.as_str()) {
            text_parts.push(name.to_string());
        }
        if let Some(content) = candidate.get("content").and_then(|v| v.as_str()) {
            text_parts.push(content.to_string());
        }
        if let Some(file_path) = candidate.get("file_path").and_then(|v| v.as_str()) {
            text_parts.push(format!("File: {}", file_path));
        }

        text_parts.join(" ")
    }

    /// Get all available tool schemas for registration
    pub fn get_tool_schemas() -> Vec<crate::ToolSchema> {
        GraphToolSchemas::all()
    }

    /// Get tool names for listing
    pub fn get_tool_names() -> Vec<String> {
        GraphToolSchemas::tool_names()
    }
}

fn log_tool_call_start(tool_name: &str, parameters: &JsonValue) {
    info!(
        target: TOOL_PROGRESS_LOG_TARGET,
        tool = tool_name,
        "Tool call started"
    );
    debug!(
        target: TOOL_PROGRESS_LOG_TARGET,
        tool = tool_name,
        "Tool input payload: {}",
        parameters
    );

    // Debug logging to file if enabled
    DebugLogger::log_tool_start(tool_name, parameters);
}

fn log_tool_call_finish(tool_name: &str, result: &JsonValue) {
    info!(
        target: TOOL_PROGRESS_LOG_TARGET,
        tool = tool_name,
        result_bytes = result.to_string().len(),
        "Tool call completed"
    );
    debug!(
        target: TOOL_PROGRESS_LOG_TARGET,
        tool = tool_name,
        "Tool output payload: {}",
        result
    );

    // Debug logging to file if enabled
    DebugLogger::log_tool_finish(tool_name, result);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_content_chars_prefers_the_call_argument() {
        let with_arg = serde_json::json!({"content_chars": 500});
        assert_eq!(super::resolve_content_chars(&with_arg), 500);
        let zero = serde_json::json!({"content_chars": 0});
        assert_eq!(super::resolve_content_chars(&zero), 0);
    }

    #[test]
    fn resolve_content_chars_falls_back_to_the_environment_default() {
        let without = serde_json::json!({"node_id": "x"});
        assert_eq!(
            super::resolve_content_chars(&without),
            super::content_snippet_chars()
        );
    }

    #[test]
    fn compact_called_by_becomes_an_id_to_name_map() {
        let mut value = serde_json::json!([{
            "name": "reached",
            "called_by": [
                {"id": "nodes:`a`", "kind": "Function", "name": "caller_a"},
                {"id": "nodes:`b`", "kind": "Function", "name": "caller_b"}
            ]
        }]);
        super::compact_called_by(&mut value);
        assert_eq!(
            value[0]["called_by"],
            serde_json::json!({"nodes:`a`": "caller_a", "nodes:`b`": "caller_b"})
        );
        assert_eq!(value[0]["name"], "reached");
    }

    #[test]
    fn compact_called_by_drops_kind_and_extra_fields() {
        let mut value = serde_json::json!([{
            "called_by": [{"name": "c", "id": "nodes:`c`", "kind": "Function",
                            "language": "Rust", "qualified_name": "x::c"}]
        }]);
        super::compact_called_by(&mut value);
        let map = value[0]["called_by"].as_object().expect("map");
        assert_eq!(map.len(), 1);
        assert_eq!(map["nodes:`c`"], "c");
    }

    #[test]
    fn compact_called_by_falls_back_to_the_name_as_key() {
        let mut value = serde_json::json!([{"called_by": [{"name": "only_name"}]}]);
        super::compact_called_by(&mut value);
        assert_eq!(
            value[0]["called_by"],
            serde_json::json!({"only_name": "only_name"})
        );
    }

    #[test]
    fn compact_called_by_ignores_entries_without_a_name() {
        let mut value = serde_json::json!([{"called_by": [{"id": "nodes:`a`"}, {"name": "kept", "id": "nodes:`k`"}]}]);
        super::compact_called_by(&mut value);
        assert_eq!(
            value[0]["called_by"],
            serde_json::json!({"nodes:`k`": "kept"})
        );
    }

    #[test]
    fn compact_called_by_leaves_empty_and_missing_fields_alone() {
        let mut value = serde_json::json!([
            {"name": "a", "called_by": []},
            {"name": "b"}
        ]);
        super::compact_called_by(&mut value);
        assert_eq!(value[0]["called_by"], serde_json::json!({}));
        assert!(value[1].get("called_by").is_none());
    }

    #[test]
    fn compact_called_by_is_a_no_op_for_non_arrays() {
        let mut value = serde_json::json!({"called_by": [{"name": "x", "id": "nodes:`x`"}]});
        super::compact_called_by(&mut value);
        assert_eq!(value["called_by"][0]["name"], "x");
    }

    #[test]
    fn trim_metadata_keeps_only_actionable_keys() {
        let mut value = serde_json::json!({
            "result": [{
                "name": "a",
                "metadata": {
                    "imports": ["b"],
                    "qualified_name": "crate::a",
                    "embedding_model": "F2LLM",
                    "project_id": "/repo",
                    "chunk_count": "3",
                    "analyzer_confidence": "1.0"
                }
            }]
        });
        super::trim_metadata(&mut value);
        let metadata = &value["result"][0]["metadata"];
        assert_eq!(metadata["imports"][0], "b");
        assert_eq!(metadata["qualified_name"], "crate::a");
        assert!(metadata.get("embedding_model").is_none());
        assert!(metadata.get("project_id").is_none());
        assert!(metadata.get("chunk_count").is_none());
    }

    #[test]
    fn trim_metadata_filters_the_json_string_form_the_store_returns() {
        let mut value = serde_json::json!({
            "result": [{
                "name": "a",
                "metadata": "{\"imports\":[\"b\"],\"qualified_name\":\"crate::a\",\"embedding_model\":\"F2LLM\",\"project_id\":\"/repo\"}"
            }]
        });
        super::trim_metadata(&mut value);
        let raw = value["result"][0]["metadata"]
            .as_str()
            .expect("string metadata kept");
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed["imports"][0], "b");
        assert!(parsed.get("embedding_model").is_none());
        assert!(parsed.get("project_id").is_none());
    }

    #[test]
    fn trim_metadata_removes_a_string_form_with_no_useful_keys() {
        let mut value = serde_json::json!({
            "metadata": "{\"project_id\":\"/repo\",\"chunk_count\":\"2\"}"
        });
        super::trim_metadata(&mut value);
        assert!(value.get("metadata").is_none());
    }

    #[test]
    fn trim_metadata_removes_the_key_when_nothing_useful_remains() {
        let mut value =
            serde_json::json!({"metadata": {"project_id": "/repo", "chunk_count": "1"}});
        super::trim_metadata(&mut value);
        assert!(value.get("metadata").is_none());
    }

    #[test]
    fn trim_metadata_reaches_nested_objects_and_arrays() {
        let mut value = serde_json::json!({
            "result": {"node": {"metadata": {"embedding_model": "x", "imports": ["i"]}},
                       "dependents": [{"metadata": {"project_id": "p"}}]}
        });
        super::trim_metadata(&mut value);
        assert_eq!(value["result"]["node"]["metadata"]["imports"][0], "i");
        assert!(
            value["result"]["node"]["metadata"]
                .get("embedding_model")
                .is_none()
        );
        assert!(value["result"]["dependents"][0].get("metadata").is_none());
    }

    #[test]
    fn drop_echoed_parameters_removes_only_the_envelope_field() {
        let mut value = serde_json::json!({"tool": "get_hub_nodes", "parameters": {"min_degree": 3},
                                            "result": [{"name": "a"}]});
        super::drop_echoed_parameters(&mut value);
        assert!(value.get("parameters").is_none());
        assert_eq!(value["tool"], "get_hub_nodes");
        assert_eq!(value["result"][0]["name"], "a");
    }

    #[test]
    fn drop_echoed_parameters_is_a_no_op_without_the_key() {
        let mut value = serde_json::json!({"result": []});
        super::drop_echoed_parameters(&mut value);
        assert_eq!(value, serde_json::json!({"result": []}));
    }

    #[test]
    fn test_shorten_content_keeps_location_and_marks_truncation() {
        let long = (0..200)
            .map(|i| format!("line {i} of a long document"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = json!({
            "tool": "semantic_code_search",
            "result": [
                { "name": "doc", "file_path": "README.md", "start_line": 1, "content": long },
                { "name": "small", "content": "fn small() {}" },
                { "node": { "name": "nested", "content": "x".repeat(5_000) } }
            ]
        });

        shorten_content(&mut result, 300);

        let rows = result["result"].as_array().unwrap();
        let snippet = rows[0]["content"].as_str().unwrap();
        assert!(snippet.chars().count() <= 300, "{}", snippet.len());
        assert!(
            snippet.ends_with("of a long document"),
            "ends on a full line"
        );
        assert_eq!(rows[0]["content_truncated"], true);
        assert!(rows[0]["content_chars"].as_u64().unwrap() > 4_000);
        assert_eq!(rows[0]["file_path"], "README.md");
        assert_eq!(rows[1]["content"], "fn small() {}");
        assert!(rows[1].get("content_truncated").is_none());
        assert_eq!(rows[2]["node"]["content"].as_str().unwrap().len(), 300);
        assert_eq!(rows[2]["node"]["content_truncated"], true);
    }

    #[test]
    fn test_shorten_content_respects_char_boundaries_and_zero_disables() {
        let mut result = json!({ "content": "é".repeat(50) });
        shorten_content(&mut result, 10);
        assert_eq!(result["content"].as_str().unwrap().chars().count(), 10);

        let mut untouched = json!({ "content": "y".repeat(50) });
        shorten_content(&mut untouched, 0);
        assert_eq!(untouched["content"].as_str().unwrap().len(), 50);
    }

    #[test]
    fn test_tool_schemas_available() {
        let schemas = GraphToolExecutor::get_tool_schemas();
        assert_eq!(schemas.len(), 8);
    }

    #[test]
    fn test_tool_names() {
        let names = GraphToolExecutor::get_tool_names();
        assert_eq!(names.len(), 8);
        assert!(names.contains(&"get_transitive_dependencies".to_string()));
        assert!(names.contains(&"find_complexity_hotspots".to_string()));
    }

    #[test]
    fn test_parameter_extraction() {
        let params = json!({
            "node_id": "nodes:123",
            "edge_type": "calls",
            "depth": 5
        });

        assert_eq!(params["node_id"].as_str().unwrap(), "nodes:123");
        assert_eq!(params["edge_type"].as_str().unwrap(), "calls");
        assert_eq!(params["depth"].as_i64().unwrap(), 5);
    }

    // === Result interpretability tests ===

    #[test]
    fn test_score_semantics_explains_every_score() {
        let semantics = score_semantics();

        let vector = semantics["vector_score"].as_str().unwrap();
        assert!(
            vector.to_lowercase().contains("cosine"),
            "vector_score must name its metric: {vector}"
        );

        let text = semantics["text_score"].as_str().unwrap();
        assert!(
            text.to_lowercase().contains("ts_rank_cd"),
            "text_score must name ts_rank_cd: {text}"
        );

        let combined = semantics["combined_score"].as_str().unwrap();
        assert!(
            combined.contains("RRF"),
            "combined_score must say it is a fusion rank: {combined}"
        );
        assert!(
            combined.contains("1/(60+rank)"),
            "combined_score must give its formula: {combined}"
        );

        assert_eq!(semantics["threshold_applies_to"], "vector_score");
    }

    #[test]
    fn test_retain_in_project_rows_drops_external_dependency_manifests() {
        let rows = vec![
            json!({
                "name": "indexmap",
                "file_path": "/home/user/.cargo/registry/src/index.crates.io-1/indexmap/Cargo.toml",
                "metadata": {"analyzer": "build_context"}
            }),
            json!({
                "name": "app::default",
                "file_path": "/repo/app/Cargo.toml",
                "metadata": {"analyzer": "build_context"}
            }),
            json!({
                "name": "parse_config",
                "file_path": "/repo/app/src/config.rs",
                "metadata": {"api_visibility": "private"}
            }),
        ];

        let kept = retain_in_project_rows(rows, Path::new("/repo/app"));

        let names: Vec<&str> = kept
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["app::default", "parse_config"]);
    }

    #[test]
    fn test_retain_in_project_rows_leaves_custom_project_ids_alone() {
        // Custom identifiers are not paths, so nothing can be classified as external.
        let rows = vec![json!({
            "name": "indexmap",
            "file_path": "/home/user/.cargo/registry/src/indexmap/Cargo.toml",
            "metadata": {"analyzer": "build_context"}
        })];

        let kept = retain_in_project_rows(rows, Path::new("my-project"));

        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn test_empty_diagnostics_reports_missing_node() {
        let summary = codegraph_graph::NodeEdgeSummary::default();
        let diagnostics = empty_result_diagnostics(&summary, "calls", "nodes:`missing-id`");

        assert_eq!(diagnostics["node_found"], false);
        assert_eq!(diagnostics["requested_edge_type"], "calls");
        assert_eq!(diagnostics["node_id"], "nodes:`missing-id`");
        assert!(diagnostics["suggested_call"].is_null());
        let hint = diagnostics["hint"].as_str().unwrap();
        assert!(hint.to_lowercase().contains("not found"));
        assert!(
            hint.contains("nodes:`missing-id`"),
            "hint should echo the requested id: {hint}"
        );
    }

    #[test]
    fn test_empty_diagnostics_lists_available_edge_types() {
        let summary = codegraph_graph::NodeEdgeSummary {
            found: true,
            kind: Some("Struct".to_string()),
            id: Some("nodes:`abc`".to_string()),
            incoming_edge_types: vec!["contains".to_string()],
            ..Default::default()
        };
        let diagnostics = empty_result_diagnostics(&summary, "calls", "nodes:`abc`");

        assert_eq!(diagnostics["node_found"], true);
        assert_eq!(diagnostics["node_kind"], "Struct");
        assert_eq!(
            diagnostics["available_incoming_edge_types"],
            json!(["contains"])
        );
        assert_eq!(
            diagnostics["suggested_call"],
            json!({
                "tool": "get_reverse_dependencies",
                "node_id": "nodes:`abc`",
                "edge_type": "contains",
                "depth": 1
            })
        );
        let hint = diagnostics["hint"].as_str().unwrap();
        assert!(hint.contains("contains"), "hint should suggest: {hint}");
    }

    #[test]
    fn test_empty_diagnostics_explains_container_nodes() {
        let summary = codegraph_graph::NodeEdgeSummary {
            found: true,
            kind: Some("Trait".to_string()),
            ..Default::default()
        };
        let diagnostics = empty_result_diagnostics(&summary, "calls", "nodes:`t`");

        let hint = diagnostics["hint"].as_str().unwrap().to_lowercase();
        assert!(
            hint.contains("child") || hint.contains("member"),
            "container nodes need guidance, got: {hint}"
        );
    }

    // === Cache Tests ===

    #[test]
    fn test_cache_key_generation() {
        let params1 = json!({
            "node_id": "nodes:123",
            "edge_type": "calls",
            "depth": 3
        });
        let params2 = json!({
            "node_id": "nodes:123",
            "edge_type": "calls",
            "depth": 3
        });
        let params3 = json!({
            "node_id": "nodes:456",
            "edge_type": "calls",
            "depth": 3
        });

        let project = "proj-a";

        let key1 = GraphToolExecutor::cache_key(project, "get_transitive_dependencies", &params1);
        let key2 = GraphToolExecutor::cache_key(project, "get_transitive_dependencies", &params2);
        let key3 = GraphToolExecutor::cache_key(project, "get_transitive_dependencies", &params3);

        // Same params should generate same key
        assert_eq!(key1, key2);
        // Different params should generate different key
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_cache_key_includes_project_scope() {
        let params = json!({
            "node_id": "nodes:123"
        });

        let key_a = GraphToolExecutor::cache_key("proj-a", "get_hub_nodes", &params);
        let key_b = GraphToolExecutor::cache_key("proj-b", "get_hub_nodes", &params);

        assert_ne!(key_a, key_b);
        assert!(
            key_a.starts_with("proj-a:"),
            "Project scope should prefix cache key"
        );
    }

    #[test]
    fn test_cache_stats_initialization() {
        let stats = CacheStats {
            hits: 0,
            misses: 0,
            evictions: 0,
            current_size: 0,
            max_size: 100,
        };

        assert_eq!(stats.hit_rate(), 0.0);
    }

    #[test]
    fn test_cache_stats_hit_rate_calculation() {
        let stats = CacheStats {
            hits: 75,
            misses: 25,
            evictions: 5,
            current_size: 50,
            max_size: 100,
        };

        assert_eq!(stats.hit_rate(), 75.0);
    }

    #[test]
    fn test_cache_stats_hit_rate_no_requests() {
        let stats = CacheStats {
            hits: 0,
            misses: 0,
            evictions: 0,
            current_size: 0,
            max_size: 100,
        };

        assert_eq!(stats.hit_rate(), 0.0);
    }

    #[test]
    fn test_log_tool_call_start_captures_info_and_debug() {
        let logs = capture_logs(|| {
            let params = serde_json::json!({
                "node_id": "nodes:123",
                "edge_type": "calls"
            });
            log_tool_call_start("get_transitive_dependencies", &params);
        });

        assert!(logs.contains("Tool call started"));
        assert!(logs.contains("Tool input payload"));
    }

    #[test]
    fn test_log_tool_call_finish_captures_info_and_debug() {
        let logs = capture_logs(|| {
            let result = serde_json::json!({
                "tool": "detect_cycles",
                "result": "ok"
            });
            log_tool_call_finish("detect_cycles", &result);
        });

        assert!(logs.contains("Tool call completed"));
        assert!(logs.contains("Tool output payload"));
    }

    fn capture_logs<F>(f: F) -> String
    where
        F: FnOnce(),
    {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing::subscriber::with_default;
        use tracing_subscriber::EnvFilter;

        #[derive(Clone)]
        struct BufferWriter {
            inner: Arc<Mutex<Vec<u8>>>,
        }

        impl BufferWriter {
            fn new() -> Self {
                Self {
                    inner: Arc::new(Mutex::new(Vec::new())),
                }
            }

            fn into_string(&self) -> String {
                let bytes = self.inner.lock().unwrap().clone();
                String::from_utf8(bytes).unwrap()
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
            type Writer = BufferGuard;

            fn make_writer(&'a self) -> Self::Writer {
                BufferGuard {
                    inner: self.inner.clone(),
                }
            }
        }

        struct BufferGuard {
            inner: Arc<Mutex<Vec<u8>>>,
        }

        impl Write for BufferGuard {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.inner.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let writer = BufferWriter::new();

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("debug"))
            .with_ansi(false)
            .without_time()
            .with_writer(writer.clone())
            .finish();

        with_default(subscriber, f);

        writer.into_string()
    }
}
