// ABOUTME: Backend-neutral graph query results and the GraphQuery trait
// ABOUTME: Keeps the MCP tool contract free of any concrete database driver

use async_trait::async_trait;
use codegraph_core::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Node with dependency depth information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyNode {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub location: Option<NodeLocation>,
    pub language: Option<String>,
    #[serde(
        default,
        deserialize_with = "codegraph_core::deserialize_content_string"
    )]
    pub content: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub dependency_depth: Option<i32>,
    pub dependent_depth: Option<i32>,
}

/// Node identity plus the relationship types that touch it.
///
/// Produced by a node edge summary query so an empty dependency result can be explained
/// rather than returned as a bare empty array.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeEdgeSummary {
    /// Whether the node exists in this project's index.
    pub found: bool,
    pub id: Option<String>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub file_path: Option<String>,
    pub start_line: Option<i64>,
    #[serde(default)]
    pub incoming_edge_types: Vec<String>,
    #[serde(default)]
    pub outgoing_edge_types: Vec<String>,
}

/// Circular dependency pair
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircularDependency {
    pub node1_id: String,
    pub node2_id: String,
    pub node1: NodeInfo,
    pub node2: NodeInfo,
    pub dependency_type: String,
}

/// Call chain node with caller information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallChainNode {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub location: Option<NodeLocation>,
    pub language: Option<String>,
    #[serde(
        default,
        deserialize_with = "codegraph_core::deserialize_content_string"
    )]
    pub content: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub call_depth: Option<i32>,
    pub called_by: Option<Vec<CallerInfo>>,
}

/// Coupling metrics result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouplingMetricsResult {
    pub node: NodeInfo,
    pub metrics: CouplingMetrics,
    pub dependents: Vec<NodeReference>,
    pub dependencies: Vec<NodeReference>,
}

/// Coupling metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouplingMetrics {
    pub afferent_coupling: i32,
    pub efferent_coupling: i32,
    pub total_coupling: i32,
    pub instability: f64,
    pub stability: f64,
    pub is_stable: bool,
    pub is_unstable: bool,
    pub coupling_category: String,
}

/// Hub node with degree information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubNode {
    pub node_id: String,
    pub node: NodeInfo,
    pub afferent_degree: i32,
    pub efferent_degree: i32,
    pub total_degree: i32,
    pub incoming_by_type: Vec<EdgeTypeCount>,
    pub outgoing_by_type: Vec<EdgeTypeCount>,
}

/// Edge type count
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeTypeCount {
    pub edge_type: String,
    pub count: i32,
}

/// Node information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub location: Option<NodeLocation>,
    pub language: Option<String>,
    #[serde(
        default,
        deserialize_with = "codegraph_core::deserialize_content_string"
    )]
    pub content: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Node reference (minimal info)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeReference {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub location: Option<NodeLocation>,
}

/// Caller information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallerInfo {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
}

/// Node location information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLocation {
    pub file_path: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
}

/// Complexity hotspot with risk metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityHotspot {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub language: Option<String>,
    pub file_path: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub complexity: f32,
    pub afferent_coupling: i32,
    pub efferent_coupling: i32,
    pub instability: f32,
    pub risk_score: f32,
}

/// The graph queries the MCP tools need, independent of backend.
///
/// Signatures mirror `GraphFunctions` one-for-one so the existing tool executor
/// keeps its call sites; only the concrete type changes.
#[async_trait]
pub trait GraphQuery: Send + Sync {
    fn project_id(&self) -> &str;

    /// A copy bound to another project id (the tools accept a per-call override).
    fn with_project_id(&self, project_id: &str) -> Arc<dyn GraphQuery>;

    async fn get_transitive_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
    ) -> Result<Vec<DependencyNode>>;

    async fn get_reverse_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
    ) -> Result<Vec<DependencyNode>>;

    async fn trace_call_chain(&self, from_node: &str, max_depth: i32)
    -> Result<Vec<CallChainNode>>;

    async fn calculate_coupling_metrics(&self, node_id: &str) -> Result<CouplingMetricsResult>;

    async fn get_hub_nodes(&self, min_degree: i32) -> Result<Vec<HubNode>>;

    async fn get_complexity_hotspots(
        &self,
        min_complexity: f32,
        limit: i32,
    ) -> Result<Vec<ComplexityHotspot>>;

    async fn get_node_edge_summary(&self, node_id: &str) -> Result<NodeEdgeSummary>;

    async fn count_nodes_for_project(&self) -> Result<usize>;

    async fn detect_circular_dependencies(
        &self,
        edge_type: &str,
    ) -> Result<Vec<CircularDependency>>;

    /// Semantic search over indexed nodes, enriched with graph context.
    async fn semantic_search_with_context(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        dimension: usize,
        limit: usize,
        threshold: f32,
        include_graph_context: bool,
    ) -> Result<Vec<serde_json::Value>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moving the DTO out of `graph_functions` must not rename or reorder a single JSON key,
    /// because the MCP tool payloads are part of the public answer shape.
    #[test]
    fn node_info_keeps_its_json_shape() {
        let value = serde_json::to_value(NodeInfo {
            id: "nodes:abc".to_string(),
            name: "abc".to_string(),
            kind: Some("function".to_string()),
            location: None,
            language: Some("rust".to_string()),
            content: None,
            metadata: None,
        })
        .expect("NodeInfo serializes");

        let mut keys: Vec<&str> = value
            .as_object()
            .expect("NodeInfo serializes to an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();

        assert_eq!(
            keys,
            [
                "content", "id", "kind", "language", "location", "metadata", "name"
            ]
        );
        // `deserialize_with` does not affect serialization: an absent content is `null`,
        // not an omitted key.
        assert!(value["content"].is_null());
    }
}
