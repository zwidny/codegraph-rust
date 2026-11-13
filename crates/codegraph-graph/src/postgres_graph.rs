// ABOUTME: Postgres-backed `GraphQuery` implementation for the MCP graph tools
// ABOUTME: Loads rows with SQL, then delegates every algorithm to `crate::analysis`

//! Postgres implementation of [`crate::graph_types::GraphQuery`].
//!
//! The SurrealDB backend calls stored procedures; this backend loads the same rows and
//! then runs the pure functions in [`crate::analysis`]. That keeps one algorithm shared by
//! both backends: `analysis::hubs`, `analysis::expand_bfs`, `analysis::trace_bfs` and
//! `analysis::score_hotspots` are the single source of truth for ranking and traversal.
//!
//! `detect_circular_dependencies` and `semantic_search_with_context` are **not implemented**
//! here and return an explicit configuration error instead of silently returning empty data.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use codegraph_core::{CodeGraphError, Result};

use crate::analysis::{
    DEPENDENCY_EDGE_TYPES, Direction, EdgeRow as AnalysisEdgeRow, expand_bfs, hubs, safe_depth,
    safe_depth_for_call_chain, score_hotspots, trace_bfs,
};
use crate::graph_types::{
    CallChainNode, CallerInfo, CircularDependency, ComplexityHotspot, CouplingMetrics,
    CouplingMetricsResult, DependencyNode, EdgeTypeCount, HubNode, NodeEdgeSummary, NodeInfo,
    NodeLocation, NodeReference,
};
use crate::postgres_storage::PgStore;

/// Cap `fn::get_hub_nodes` applies to its ranked output.
const HUB_LIMIT: usize = 50;

/// Default `min_degree` when the caller passes a non-positive value, matching
/// `fn::get_hub_nodes` and `GraphFunctions::get_hub_nodes_native`.
const DEFAULT_HUB_MIN_DEGREE: usize = 5;

/// Default `min_complexity` when the caller passes a non-positive value, matching
/// `fn::get_complexity_hotspots` and `GraphFunctions::get_complexity_hotspots_native`.
const DEFAULT_MIN_COMPLEXITY: f32 = 5.0;

/// Dependency edges (one of [`DEPENDENCY_EDGE_TYPES`]), ordered by id for determinism.
const DEPENDENCY_EDGES_SQL: &str = "\
SELECT from_id, to_id, edge_type FROM edges \
WHERE project_id = $1 AND edge_type = ANY($2) ORDER BY id";

/// Every edge of one type, ordered by id for determinism.
const TYPED_EDGES_SQL: &str = "\
SELECT from_id, to_id, edge_type FROM edges \
WHERE project_id = $1 AND edge_type = $2 ORDER BY id";

/// Resolve one node by any of the accepted id shapes.
const NODE_BY_ID_SQL: &str = "\
SELECT id, name, node_type, language, file_path, start_line, end_line, content, metadata \
FROM nodes WHERE project_id = $1 AND id = ANY($2) ORDER BY id";

/// Hydrate a batch of node ids.
const NODES_BY_IDS_SQL: &str = "\
SELECT id, name, node_type, language, file_path, start_line, end_line, content, metadata \
FROM nodes WHERE project_id = $1 AND id = ANY($2) ORDER BY id";

/// Incoming edge sources for one node over the dependency types.
const INCOMING_SOURCES_SQL: &str = "\
SELECT from_id FROM edges \
WHERE project_id = $1 AND edge_type = ANY($2) AND to_id = $3 ORDER BY from_id";

/// Outgoing edge targets for one node over the dependency types.
const OUTGOING_TARGETS_SQL: &str = "\
SELECT to_id FROM edges \
WHERE project_id = $1 AND edge_type = ANY($2) AND from_id = $3 ORDER BY to_id";

/// Direct callers of the reached callees.
const CALLERS_SQL: &str = "\
SELECT to_id, from_id FROM edges \
WHERE project_id = $1 AND edge_type = 'calls' AND to_id = ANY($2) ORDER BY id";

/// Distinct incoming relationship types touching one node.
const INCOMING_TYPES_SQL: &str = "\
SELECT DISTINCT edge_type FROM edges \
WHERE project_id = $1 AND to_id = $2 ORDER BY edge_type";

/// Distinct outgoing relationship types touching one node.
const OUTGOING_TYPES_SQL: &str = "\
SELECT DISTINCT edge_type FROM edges \
WHERE project_id = $1 AND from_id = $2 ORDER BY edge_type";

/// Complexity candidates: functions/methods at or above the threshold, highest first.
const HOTSPOT_CANDIDATES_SQL: &str = "\
SELECT id, name, node_type, language, file_path, start_line, end_line, complexity \
FROM nodes WHERE project_id = $1 AND lower(node_type) = ANY($2) AND complexity >= $3 \
ORDER BY complexity DESC LIMIT $4";

/// Dependency edges touching the candidate set only, mirroring the stored procedure.
const HOTSPOT_EDGES_SQL: &str = "\
SELECT from_id, to_id, edge_type FROM edges \
WHERE project_id = $1 AND edge_type = ANY($2) AND (to_id = ANY($3) OR from_id = ANY($3)) \
ORDER BY id";

fn db_err<E: std::fmt::Display>(error: E) -> CodeGraphError {
    CodeGraphError::Database(error.to_string())
}

/// Accept the id shapes `fn::resolve_node` accepts: `nodes:x`, `x`, and the
/// backtick/bracket escaped forms.
fn normalize_node_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let after = trimmed.strip_prefix("nodes:").unwrap_or(trimmed);
    let clean = after
        .trim_matches('`')
        .trim_matches(|c| c == '⟨' || c == '⟩');
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

/// Postgres-backed graph query surface.
///
/// Construct with [`PgGraph::new`] and hand it to the tool executor as
/// `Arc<dyn GraphQuery>`.
pub struct PgGraph {
    store: Arc<PgStore>,
    project_id: String,
}

impl PgGraph {
    pub fn new(store: Arc<PgStore>, project_id: impl Into<String>) -> Self {
        Self {
            store,
            project_id: project_id.into(),
        }
    }

    /// All id shapes that might identify the same stored node.
    fn id_candidates(input: &str) -> Option<Vec<String>> {
        let clean = normalize_node_id(input)?;
        let mut candidates = vec![input.trim().to_string(), clean.clone()];
        let prefixed = format!("nodes:{clean}");
        if !candidates.contains(&prefixed) {
            candidates.push(prefixed);
        }
        candidates.dedup();
        Some(candidates)
    }

    /// Resolve a caller-supplied node id to the stored node row.
    ///
    /// Returns `None` for an unusable id or a node that is not in this project, matching the
    /// `fn::resolve_node` + project guard used by the SurrealDB backend.
    async fn load_node(&self, node_id: &str) -> Result<Option<NodeInfo>> {
        let Some(candidates) = Self::id_candidates(node_id) else {
            return Ok(None);
        };
        let client = self.store.pool().get().await.map_err(db_err)?;
        let rows = client
            .query(NODE_BY_ID_SQL, &[&self.project_id, &candidates])
            .await
            .map_err(db_err)?;
        Ok(rows.first().map(|row| self.node_info_from_row(row)))
    }

    /// Load every dependency edge for the project.
    async fn load_dependency_edges(&self) -> Result<Vec<AnalysisEdgeRow>> {
        let types: Vec<String> = DEPENDENCY_EDGE_TYPES
            .iter()
            .map(|t| (*t).to_string())
            .collect();
        let client = self.store.pool().get().await.map_err(db_err)?;
        let rows = client
            .query(DEPENDENCY_EDGES_SQL, &[&self.project_id, &types])
            .await
            .map_err(db_err)?;
        Ok(rows.iter().map(Self::analysis_edge_from_row).collect())
    }

    /// Load every edge of one type for the project.
    async fn load_typed_edges(&self, edge_type: &str) -> Result<Vec<AnalysisEdgeRow>> {
        let client = self.store.pool().get().await.map_err(db_err)?;
        let rows = client
            .query(TYPED_EDGES_SQL, &[&self.project_id, &edge_type])
            .await
            .map_err(db_err)?;
        Ok(rows.iter().map(Self::analysis_edge_from_row).collect())
    }

    /// Hydrate a batch of node ids into full node rows.
    async fn load_nodes(&self, ids: &[String]) -> Result<HashMap<String, NodeInfo>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let client = self.store.pool().get().await.map_err(db_err)?;
        let rows = client
            .query(NODES_BY_IDS_SQL, &[&self.project_id, &ids.to_vec()])
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                let node = self.node_info_from_row(row);
                (node.id.clone(), node)
            })
            .collect())
    }

    fn analysis_edge_from_row(row: &tokio_postgres::Row) -> AnalysisEdgeRow {
        AnalysisEdgeRow {
            from: row.get("from_id"),
            to: row.get("to_id"),
            edge_type: row.get("edge_type"),
        }
    }

    fn node_info_from_row(&self, row: &tokio_postgres::Row) -> NodeInfo {
        let file_path: Option<String> = row.get("file_path");
        let start_line: Option<i32> = row.get("start_line");
        let end_line: Option<i32> = row.get("end_line");
        NodeInfo {
            id: row.get("id"),
            name: row.get("name"),
            kind: row.get("node_type"),
            // A null file path yields no location; never construct an empty-path location.
            location: file_path.map(|file_path| NodeLocation {
                file_path,
                start_line,
                end_line,
            }),
            language: row.get("language"),
            content: row.get("content"),
            metadata: row.get("metadata"),
        }
    }

    fn node_reference(node: &NodeInfo) -> NodeReference {
        NodeReference {
            id: node.id.clone(),
            name: node.name.clone(),
            kind: node.kind.clone(),
            location: node.location.clone(),
        }
    }

    /// Shared body of the two dependency walks.
    async fn expand_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
        direction: Direction,
    ) -> Result<Vec<DependencyNode>> {
        let Some(start) = self.load_node(node_id).await? else {
            return Ok(Vec::new());
        };

        // `string::lowercase($edge_type ?? 'calls')`; an empty name also means the default.
        let edge_name = if edge_type.trim().is_empty() {
            "calls".to_string()
        } else {
            edge_type.to_ascii_lowercase()
        };

        let edges = self.load_typed_edges(&edge_name).await?;
        let reached = expand_bfs(&edges, &start.id, &edge_name, direction, safe_depth(depth));
        if reached.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = reached.iter().map(|(id, _)| id.clone()).collect();
        let by_id = self.load_nodes(&ids).await?;

        let is_reverse = direction == Direction::In;
        let mut out = Vec::with_capacity(reached.len());
        for (id, level) in reached {
            let Some(node) = by_id.get(&id) else {
                continue;
            };
            let level = Some(level as i32);
            out.push(DependencyNode {
                id: node.id.clone(),
                name: node.name.clone(),
                kind: node.kind.clone(),
                location: node.location.clone(),
                language: node.language.clone(),
                content: node.content.clone(),
                metadata: node.metadata.clone(),
                dependency_depth: if is_reverse { None } else { level },
                dependent_depth: if is_reverse { level } else { None },
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl crate::graph_types::GraphQuery for PgGraph {
    fn project_id(&self) -> &str {
        &self.project_id
    }

    fn with_project_id(&self, project_id: &str) -> Arc<dyn crate::graph_types::GraphQuery> {
        Arc::new(Self {
            store: self.store.clone(),
            project_id: project_id.to_string(),
        })
    }

    async fn get_transitive_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
    ) -> Result<Vec<DependencyNode>> {
        self.expand_dependencies(node_id, edge_type, depth, Direction::Out)
            .await
    }

    async fn get_reverse_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
    ) -> Result<Vec<DependencyNode>> {
        self.expand_dependencies(node_id, edge_type, depth, Direction::In)
            .await
    }

    async fn trace_call_chain(
        &self,
        from_node: &str,
        max_depth: i32,
    ) -> Result<Vec<CallChainNode>> {
        let Some(start) = self.load_node(from_node).await? else {
            return Ok(Vec::new());
        };

        let edges = self.load_typed_edges("calls").await?;
        let reached = trace_bfs(
            &edges,
            &start.id,
            &start.name,
            safe_depth_for_call_chain(max_depth),
        );
        if reached.is_empty() {
            return Ok(Vec::new());
        }

        // Direct callers, deduplicated and capped at ten per callee before hydration, the
        // same order as `array::slice(array::distinct(array::group(`from`)), 0, 10)`.
        let reached_ids: Vec<String> = reached.iter().map(|step| step.id.clone()).collect();
        let client = self.store.pool().get().await.map_err(db_err)?;
        let caller_rows = client
            .query(CALLERS_SQL, &[&self.project_id, &reached_ids])
            .await
            .map_err(db_err)?;
        let mut callers: HashMap<String, Vec<String>> = HashMap::new();
        for row in &caller_rows {
            let callee: String = row.get("to_id");
            let caller: String = row.get("from_id");
            let entry = callers.entry(callee).or_default();
            if !entry.contains(&caller) {
                entry.push(caller);
            }
        }
        let mut caller_ids: Vec<String> = Vec::new();
        for ids in callers.values_mut() {
            ids.sort();
            ids.truncate(10);
            caller_ids.extend(ids.iter().cloned());
        }
        caller_ids.sort();
        caller_ids.dedup();
        let caller_by_id = self.load_nodes(&caller_ids).await?;

        let by_id = self.load_nodes(&reached_ids).await?;

        let mut out = Vec::with_capacity(reached.len());
        for step in reached {
            let Some(node) = by_id.get(&step.id) else {
                continue;
            };
            let called_by: Vec<CallerInfo> = callers
                .get(&step.id)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| caller_by_id.get(id))
                        .map(|r| CallerInfo {
                            id: r.id.clone(),
                            name: r.name.clone(),
                            kind: r.kind.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(CallChainNode {
                id: node.id.clone(),
                name: node.name.clone(),
                kind: node.kind.clone(),
                location: node.location.clone(),
                language: node.language.clone(),
                content: node.content.clone(),
                metadata: node.metadata.clone(),
                call_depth: Some(step.depth as i32),
                called_by: Some(called_by),
            });
        }

        // `trace_bfs` already walks level by level; keep the sort explicit so the contract
        // ("ORDER BY call_depth") survives any future traversal change.
        out.sort_by_key(|row| row.call_depth.unwrap_or_default());
        Ok(out)
    }

    async fn calculate_coupling_metrics(&self, node_id: &str) -> Result<CouplingMetricsResult> {
        let node = self.load_node(node_id).await?.ok_or_else(|| {
            CodeGraphError::Database(format!(
                "Node not found or project_id mismatch: node_id='{}', expected project_id='{}'. \
                    Ensure the node exists and was indexed with the same project_id.",
                node_id, self.project_id
            ))
        })?;

        let types: Vec<String> = DEPENDENCY_EDGE_TYPES
            .iter()
            .map(|t| (*t).to_string())
            .collect();
        let client = self.store.pool().get().await.map_err(db_err)?;

        let dependent_rows = client
            .query(INCOMING_SOURCES_SQL, &[&self.project_id, &types, &node.id])
            .await
            .map_err(db_err)?;
        let dependency_rows = client
            .query(OUTGOING_TARGETS_SQL, &[&self.project_id, &types, &node.id])
            .await
            .map_err(db_err)?;

        // `fn::calculate_coupling_metrics` distincts before counting; keep the same rule.
        let unique = |mut ids: Vec<String>| {
            ids.sort();
            ids.dedup();
            ids
        };
        let dependents = unique(
            dependent_rows
                .iter()
                .map(|row| row.get::<_, String>("from_id"))
                .collect(),
        );
        let dependencies = unique(
            dependency_rows
                .iter()
                .map(|row| row.get::<_, String>("to_id"))
                .collect(),
        );

        let all_ids: Vec<String> = dependents
            .iter()
            .chain(dependencies.iter())
            .cloned()
            .collect();
        let hydrated = self.load_nodes(&all_ids).await?;
        // `fn::node_reference` returns NONE for unhydratable ids; those are dropped, and the
        // remaining order follows the sorted, deduplicated id lists.
        let dependents: Vec<NodeReference> = dependents
            .iter()
            .filter_map(|id| hydrated.get(id))
            .map(Self::node_reference)
            .collect();
        let dependencies: Vec<NodeReference> = dependencies
            .iter()
            .filter_map(|id| hydrated.get(id))
            .map(Self::node_reference)
            .collect();

        let afferent = dependents.len() as i32;
        let efferent = dependencies.len() as i32;
        let total = afferent + efferent;
        // Round to six decimals like the SurrealQL `math::round(x * 1000000f) / 1000000f`.
        let instability = if total > 0 {
            (((efferent as f64 / total as f64) * 1_000_000.0).round()) / 1_000_000.0
        } else {
            0.0
        };
        let category = if instability < 0.3 {
            "stable"
        } else if instability > 0.7 {
            "unstable"
        } else {
            "balanced"
        };

        Ok(CouplingMetricsResult {
            node,
            metrics: CouplingMetrics {
                afferent_coupling: afferent,
                efferent_coupling: efferent,
                total_coupling: total,
                instability,
                stability: 1.0 - instability,
                is_stable: instability < 0.3,
                is_unstable: instability > 0.7,
                coupling_category: category.to_string(),
            },
            dependents,
            dependencies,
        })
    }

    async fn get_hub_nodes(&self, min_degree: i32) -> Result<Vec<HubNode>> {
        let threshold = if min_degree > 0 {
            min_degree as usize
        } else {
            DEFAULT_HUB_MIN_DEGREE
        };

        let edges = self.load_dependency_edges().await?;
        let mut ranked = hubs(&edges, DEPENDENCY_EDGE_TYPES, threshold);
        if ranked.is_empty() {
            return Ok(Vec::new());
        }
        // `fn::get_hub_nodes` returns at most 50 rows.
        ranked.truncate(HUB_LIMIT);

        let keep: std::collections::HashSet<&str> =
            ranked.iter().map(|(id, _)| id.as_str()).collect();

        let mut incoming: std::collections::HashMap<(String, String), i32> =
            std::collections::HashMap::new();
        let mut outgoing: std::collections::HashMap<(String, String), i32> =
            std::collections::HashMap::new();
        for edge in &edges {
            if keep.contains(edge.to.as_str()) {
                *incoming
                    .entry((edge.to.clone(), edge.edge_type.to_ascii_lowercase()))
                    .or_default() += 1;
            }
            if keep.contains(edge.from.as_str()) {
                *outgoing
                    .entry((edge.from.clone(), edge.edge_type.to_ascii_lowercase()))
                    .or_default() += 1;
            }
        }

        let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
        let by_id = self.load_nodes(&ids).await?;

        let per_type = |map: &std::collections::HashMap<(String, String), i32>,
                        id: &str|
         -> Vec<EdgeTypeCount> {
            let mut rows: Vec<EdgeTypeCount> = map
                .iter()
                .filter(|((node, _), _)| node == id)
                .map(|((_, kind), count)| EdgeTypeCount {
                    edge_type: kind.clone(),
                    count: *count,
                })
                .collect();
            rows.sort_by(|a, b| a.edge_type.cmp(&b.edge_type));
            rows
        };

        let mut out = Vec::with_capacity(ranked.len());
        for (id, degrees) in ranked {
            let Some(node) = by_id.get(&id) else {
                continue;
            };
            out.push(HubNode {
                incoming_by_type: per_type(&incoming, &id),
                outgoing_by_type: per_type(&outgoing, &id),
                node_id: id.clone(),
                node: node.clone(),
                afferent_degree: degrees.afferent as i32,
                efferent_degree: degrees.efferent as i32,
                total_degree: degrees.total() as i32,
            });
        }
        Ok(out)
    }

    async fn get_complexity_hotspots(
        &self,
        min_complexity: f32,
        limit: i32,
    ) -> Result<Vec<ComplexityHotspot>> {
        // `IF $limit > 0 AND $limit <= 100 THEN $limit ELSE 20 END`.
        let safe_limit = if limit > 0 && limit <= 100 {
            limit as usize
        } else {
            20
        };
        // `IF $min_complexity > 0f THEN $min_complexity ELSE 5f END`.
        let threshold = if min_complexity > 0.0 {
            min_complexity
        } else {
            DEFAULT_MIN_COMPLEXITY
        };

        struct Candidate {
            id: String,
            name: String,
            kind: Option<String>,
            language: Option<String>,
            file_path: Option<String>,
            start_line: Option<i32>,
            end_line: Option<i32>,
            complexity: f32,
        }

        let client = self.store.pool().get().await.map_err(db_err)?;
        let candidate_rows = client
            .query(
                HOTSPOT_CANDIDATES_SQL,
                &[
                    &self.project_id,
                    &vec!["function".to_string(), "method".to_string()],
                    &(threshold as f64),
                    &((safe_limit * 2) as i64),
                ],
            )
            .await
            .map_err(db_err)?;

        let candidates: Vec<Candidate> = candidate_rows
            .iter()
            .map(|row| Candidate {
                id: row.get("id"),
                name: row.get("name"),
                kind: row.get("node_type"),
                language: row.get("language"),
                file_path: row.get("file_path"),
                start_line: row.get("start_line"),
                end_line: row.get("end_line"),
                complexity: row.get::<_, Option<f64>>("complexity").unwrap_or_default() as f32,
            })
            .collect();

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        let types: Vec<String> = DEPENDENCY_EDGE_TYPES
            .iter()
            .map(|t| (*t).to_string())
            .collect();
        let edge_rows = client
            .query(HOTSPOT_EDGES_SQL, &[&self.project_id, &types, &ids])
            .await
            .map_err(db_err)?;
        let edges: Vec<AnalysisEdgeRow> =
            edge_rows.iter().map(Self::analysis_edge_from_row).collect();

        let pairs: Vec<(String, f32)> = candidates
            .iter()
            .map(|c| (c.id.clone(), c.complexity))
            .collect();
        let scored = score_hotspots(&pairs, &edges, safe_limit);

        let by_id: HashMap<&str, &Candidate> =
            candidates.iter().map(|c| (c.id.as_str(), c)).collect();
        let mut out = Vec::with_capacity(scored.len());
        for (id, score) in scored {
            let Some(c) = by_id.get(id.as_str()) else {
                continue;
            };
            out.push(ComplexityHotspot {
                id: c.id.clone(),
                name: c.name.clone(),
                kind: c.kind.clone(),
                language: c.language.clone(),
                file_path: c.file_path.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                complexity: c.complexity,
                afferent_coupling: score.afferent as i32,
                efferent_coupling: score.efferent as i32,
                instability: score.instability,
                risk_score: score.risk_score,
            });
        }
        Ok(out)
    }

    async fn get_node_edge_summary(&self, node_id: &str) -> Result<NodeEdgeSummary> {
        let Some(node) = self.load_node(node_id).await? else {
            return Ok(NodeEdgeSummary::default());
        };

        let client = self.store.pool().get().await.map_err(db_err)?;
        let incoming_rows = client
            .query(INCOMING_TYPES_SQL, &[&self.project_id, &node.id])
            .await
            .map_err(db_err)?;
        let outgoing_rows = client
            .query(OUTGOING_TYPES_SQL, &[&self.project_id, &node.id])
            .await
            .map_err(db_err)?;

        let edge_types = |rows: &[tokio_postgres::Row]| -> Vec<String> {
            rows.iter().map(|row| row.get("edge_type")).collect()
        };

        Ok(NodeEdgeSummary {
            found: true,
            id: Some(node.id.clone()),
            name: Some(node.name.clone()),
            kind: node.kind.clone(),
            file_path: node.location.as_ref().map(|l| l.file_path.clone()),
            start_line: node
                .location
                .as_ref()
                .and_then(|l| l.start_line)
                .map(i64::from),
            incoming_edge_types: edge_types(&incoming_rows),
            outgoing_edge_types: edge_types(&outgoing_rows),
        })
    }

    async fn count_nodes_for_project(&self) -> Result<usize> {
        Ok(self.store.node_count(&self.project_id).await? as usize)
    }

    /// Not implemented on the Postgres backend yet; see the module documentation.
    async fn detect_circular_dependencies(
        &self,
        _edge_type: &str,
    ) -> Result<Vec<CircularDependency>> {
        Err(CodeGraphError::Configuration(
            "circular dependency detection is not implemented for the Postgres backend yet".into(),
        ))
    }

    /// Not implemented on the Postgres backend yet; see the module documentation.
    async fn semantic_search_with_context(
        &self,
        _query_text: &str,
        _query_embedding: &[f32],
        _dimension: usize,
        _limit: usize,
        _threshold: f32,
        _include_graph_context: bool,
    ) -> Result<Vec<serde_json::Value>> {
        Err(CodeGraphError::Configuration(
            "semantic search with graph context is not implemented for the Postgres backend yet"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_types::GraphQuery;
    use crate::postgres_storage::{EdgeRow as PgEdgeRow, NodeRow};

    fn store_or_skip() -> Option<PgStore> {
        if std::env::var("CODEGRAPH_PG_URL").is_err() {
            return None;
        }
        Some(PgStore::connect("").expect("connect to CODEGRAPH_PG_URL"))
    }

    fn project_id(tag: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("pg_graph_{tag}_{}_{nanos}_{n}", std::process::id())
    }

    fn node(id: &str, project: &str) -> NodeRow {
        NodeRow {
            id: id.to_string(),
            project_id: project.to_string(),
            name: id.to_string(),
            node_type: Some("function".to_string()),
            language: Some("Rust".to_string()),
            file_path: Some(format!("src/{id}.rs")),
            start_line: Some(1),
            end_line: Some(10),
            content: Some(format!("fn {id}() {{}}")),
            complexity: None,
            chunk_count: None,
            embedding_model: None,
            embedding: None,
            metadata: None,
        }
    }

    fn edge(id: &str, project: &str, from: &str, to: &str, edge_type: &str) -> PgEdgeRow {
        PgEdgeRow {
            id: id.to_string(),
            project_id: project.to_string(),
            from_id: from.to_string(),
            to_id: to.to_string(),
            edge_type: edge_type.to_string(),
            metadata: None,
        }
    }

    /// The hub ranking must be exactly what the pure analysis functions produce.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn hub_nodes_match_the_pure_analysis_functions() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("hubs");
        store.ensure_schema().await.expect("ensure_schema");

        let nodes = vec![
            node("hub", &project),
            node("leaf1", &project),
            node("leaf2", &project),
            node("leaf3", &project),
            node("leaf4", &project),
        ];
        let edges = vec![
            edge(&format!("{project}_e1"), &project, "leaf1", "hub", "calls"),
            edge(&format!("{project}_e2"), &project, "leaf2", "hub", "calls"),
            edge(
                &format!("{project}_e3"),
                &project,
                "hub",
                "leaf3",
                "imports",
            ),
            edge(&format!("{project}_e4"), &project, "hub", "leaf4", "calls"),
        ];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");
        store.upsert_edges(&edges).await.expect("upsert_edges");

        let analysis_edges: Vec<AnalysisEdgeRow> = edges
            .iter()
            .map(|e| AnalysisEdgeRow::new(&e.from_id, &e.to_id, &e.edge_type))
            .collect();
        let expected = hubs(&analysis_edges, DEPENDENCY_EDGE_TYPES, 1);

        let graph = PgGraph::new(Arc::new(store), project.clone());
        let actual = graph.get_hub_nodes(1).await.expect("get_hub_nodes");

        let expected_ids: Vec<&str> = expected.iter().map(|(id, _)| id.as_str()).collect();
        let actual_ids: Vec<&str> = actual.iter().map(|hub| hub.node_id.as_str()).collect();
        assert_eq!(
            actual_ids, expected_ids,
            "hub ordering must match analysis::hubs"
        );
        for (hub, (id, degrees)) in actual.iter().zip(expected.iter()) {
            assert_eq!(hub.node_id, *id);
            assert_eq!(hub.afferent_degree, degrees.afferent as i32);
            assert_eq!(hub.efferent_degree, degrees.efferent as i32);
            assert_eq!(hub.total_degree, degrees.total() as i32);
        }
    }

    /// Instability is `efferent / total`, rounded to six decimals like the stored procedure.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn coupling_metrics_instability_matches_the_formula() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("coupling");
        store.ensure_schema().await.expect("ensure_schema");

        let nodes = vec![
            node("mid", &project),
            node("in1", &project),
            node("in2", &project),
            node("out", &project),
        ];
        let edges = vec![
            edge(&format!("{project}_c1"), &project, "in1", "mid", "calls"),
            edge(&format!("{project}_c2"), &project, "in2", "mid", "imports"),
            edge(&format!("{project}_c3"), &project, "mid", "out", "uses"),
        ];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");
        store.upsert_edges(&edges).await.expect("upsert_edges");

        let graph = PgGraph::new(Arc::new(store), project.clone());
        let result = graph
            .calculate_coupling_metrics("mid")
            .await
            .expect("calculate_coupling_metrics");

        assert_eq!(result.metrics.afferent_coupling, 2);
        assert_eq!(result.metrics.efferent_coupling, 1);
        assert_eq!(result.metrics.total_coupling, 3);
        // The stored procedure rounds to six decimals, so 1/3 arrives as 0.333333.
        assert!(
            (result.metrics.instability - 1.0 / 3.0).abs() < 1e-6,
            "instability was {}",
            result.metrics.instability
        );
        assert_eq!(result.metrics.instability, 0.333333);
        assert!((result.metrics.stability - 2.0 / 3.0).abs() < 1e-6);
        assert!(!result.metrics.is_stable);
        assert!(!result.metrics.is_unstable);
        assert_eq!(result.metrics.coupling_category, "balanced");
        assert_eq!(result.dependents.len(), 2);
        assert_eq!(result.dependencies.len(), 1);
    }

    /// `calls` edges are followed forwards, depth by depth.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn call_chain_follows_calls_edges() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("chain");
        store.ensure_schema().await.expect("ensure_schema");

        let nodes = vec![
            node("A", &project),
            node("B", &project),
            node("C", &project),
        ];
        let edges = vec![
            edge(&format!("{project}_t1"), &project, "A", "B", "calls"),
            edge(&format!("{project}_t2"), &project, "B", "C", "calls"),
        ];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");
        store.upsert_edges(&edges).await.expect("upsert_edges");

        let graph = PgGraph::new(Arc::new(store), project.clone());
        let chain = graph
            .trace_call_chain("A", 2)
            .await
            .expect("trace_call_chain");

        let ids: Vec<&str> = chain.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["B", "C"]);
        assert_eq!(chain[0].call_depth, Some(1));
        assert_eq!(chain[1].call_depth, Some(2));
        let b_callers: Vec<&str> = chain[0]
            .called_by
            .as_ref()
            .expect("called_by")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(b_callers, vec!["A"]);
    }

    /// Reverse dependency walk follows edges backwards.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reverse_dependencies_walks_backwards() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("reverse");
        store.ensure_schema().await.expect("ensure_schema");

        let nodes = vec![node("A", &project), node("B", &project)];
        let edges = vec![edge(&format!("{project}_r1"), &project, "A", "B", "calls")];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");
        store.upsert_edges(&edges).await.expect("upsert_edges");

        let graph = PgGraph::new(Arc::new(store), project.clone());
        let dependents = graph
            .get_reverse_dependencies("B", "calls", 3)
            .await
            .expect("get_reverse_dependencies");

        let ids: Vec<&str> = dependents.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["A"]);
        assert_eq!(dependents[0].dependent_depth, Some(1));
        assert_eq!(dependents[0].dependency_depth, None);
    }

    /// Hotspot scoring must be the shared `analysis::score_hotspots` result.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn complexity_hotspots_use_the_shared_scorer() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("hotspots");
        store.ensure_schema().await.expect("ensure_schema");

        let mut high = node("high", &project);
        high.complexity = Some(10.0);
        let mut low = node("low", &project);
        low.complexity = Some(1.0);
        let nodes = vec![high, low, node("caller", &project)];
        let edges = vec![edge(
            &format!("{project}_h1"),
            &project,
            "caller",
            "high",
            "calls",
        )];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");
        store.upsert_edges(&edges).await.expect("upsert_edges");

        let analysis_edges: Vec<AnalysisEdgeRow> = edges
            .iter()
            .map(|e| AnalysisEdgeRow::new(&e.from_id, &e.to_id, &e.edge_type))
            .collect();
        let expected = score_hotspots(&[("high".to_string(), 10.0)], &analysis_edges, 20);

        let graph = PgGraph::new(Arc::new(store), project.clone());
        // A non-positive threshold falls back to 5.0, which still excludes `low`.
        let hotspots = graph
            .get_complexity_hotspots(0.0, 20)
            .await
            .expect("get_complexity_hotspots");

        assert_eq!(hotspots.len(), expected.len());
        assert_eq!(hotspots[0].id, expected[0].0);
        assert_eq!(hotspots[0].risk_score, expected[0].1.risk_score);
        assert_eq!(hotspots[0].afferent_coupling, expected[0].1.afferent as i32);
        assert_eq!(hotspots[0].efferent_coupling, expected[0].1.efferent as i32);
        assert_eq!(hotspots[0].instability, expected[0].1.instability);
    }

    /// The summary reports distinct relationship types per direction.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn node_edge_summary_lists_distinct_kinds() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("summary");
        store.ensure_schema().await.expect("ensure_schema");

        let nodes = vec![
            node("target", &project),
            node("caller1", &project),
            node("caller2", &project),
            node("callee", &project),
        ];
        let edges = vec![
            edge(
                &format!("{project}_s1"),
                &project,
                "caller1",
                "target",
                "calls",
            ),
            edge(
                &format!("{project}_s2"),
                &project,
                "caller2",
                "target",
                "calls",
            ),
            edge(
                &format!("{project}_s3"),
                &project,
                "target",
                "callee",
                "imports",
            ),
        ];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");
        store.upsert_edges(&edges).await.expect("upsert_edges");

        let graph = PgGraph::new(Arc::new(store), project.clone());
        let summary = graph
            .get_node_edge_summary("target")
            .await
            .expect("get_node_edge_summary");

        assert!(summary.found);
        assert_eq!(summary.id.as_deref(), Some("target"));
        assert_eq!(summary.incoming_edge_types, vec!["calls".to_string()]);
        assert_eq!(summary.outgoing_edge_types, vec!["imports".to_string()]);
    }

    /// Node count goes through `PgStore::node_count`.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn count_nodes_matches_the_store() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("count");
        store.ensure_schema().await.expect("ensure_schema");

        let nodes = vec![
            node("n1", &project),
            node("n2", &project),
            node("n3", &project),
        ];
        store.upsert_nodes(&nodes).await.expect("upsert_nodes");

        let graph = PgGraph::new(Arc::new(store), project.clone());
        assert_eq!(graph.count_nodes_for_project().await.expect("count"), 3);
    }

    /// The unimplemented methods must fail loudly, never return silent empty data.
    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn unimplemented_methods_fail_loudly() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let project = project_id("unimplemented");
        let graph = PgGraph::new(Arc::new(store), project);

        let cycle_error = graph
            .detect_circular_dependencies("calls")
            .await
            .expect_err("circular dependency detection must not be silently empty");
        assert!(
            cycle_error
                .to_string()
                .contains("not implemented for the Postgres backend"),
            "unexpected message: {cycle_error}"
        );

        let search_error = graph
            .semantic_search_with_context("query", &[0.0; 4], 4, 5, 0.5, false)
            .await
            .expect_err("semantic search must not be silently empty");
        assert!(
            search_error
                .to_string()
                .contains("not implemented for the Postgres backend"),
            "unexpected message: {search_error}"
        );
    }
}
