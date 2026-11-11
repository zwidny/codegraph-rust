// ABOUTME: Pure Rust graph algorithms lifted out of the SurrealQL `fn::*` functions.
// ABOUTME: No database access, so every rule here is covered by ordinary unit tests.

//! Pure graph algorithms that used to live in `schema/codegraph_v2.surql` as `fn::*`
//! stored procedures.
//!
//! Reasons for moving them here:
//!
//! - **Testable.** A SurrealQL function can only be exercised through a live datastore;
//!   these are plain functions with ordinary unit tests.
//! - **No array output ceiling.** `fn::get_hub_nodes` has to rank only the top 500
//!   candidates because `array::concat` output is capped at 1 MiB. Rust has no such cap,
//!   so degrees are exact over the whole project.
//! - **Backend portable.** The database becomes storage plus indexes; swapping it out no
//!   longer means re-implementing the analysis logic.
//!
//! The SurrealQL functions stay in the schema until each Rust replacement is verified
//! against it by a differential test, so the two can be compared directly.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub mod clustering;

/// Edge types that count as dependencies, mirroring
/// `DEFINE PARAM $dependency_edge_types` in `schema/codegraph_v2.surql`.
///
/// This is the single source of truth; `GraphFunctions::DEPENDENCY_EDGE_TYPES`
/// re-exports it so both the SQL bind list and the pure scoring agree.
pub const DEPENDENCY_EDGE_TYPES: &[&str] = &[
    "calls",
    "defines",
    "imports",
    "uses",
    "extends",
    "implements",
    "references",
    "depends_on",
    "exports",
    "reexports",
    "enables",
    "generates",
    "flows_to",
    "returns",
    "captures",
    "mutates",
];

/// One edge as loaded from storage, with record ids kept as strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EdgeRow {
    pub from: String,
    pub to: String,
    pub edge_type: String,
}

impl EdgeRow {
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        edge_type: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            edge_type: edge_type.into(),
        }
    }
}

#[cfg(feature = "surrealdb")]
crate::impl_surreal_serde!(EdgeRow);

/// Degree counts for a single node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Degrees {
    /// Edges pointing at the node.
    pub afferent: usize,
    /// Edges leaving the node.
    pub efferent: usize,
}

impl Degrees {
    pub fn total(&self) -> usize {
        self.afferent + self.efferent
    }
}

/// Mutual (bidirectional) dependency pairs for one edge type.
///
/// Mirrors `fn::detect_circular_dependencies`:
/// - self-loops are ignored;
/// - each pair is reported once, with the lexicographically smaller id first;
/// - duplicate edges between the same pair collapse to one cycle.
pub fn mutual_pairs(edges: &[EdgeRow], edge_type: &str) -> Vec<(String, String)> {
    let wanted = edge_type.to_ascii_lowercase();
    let mut directed: HashSet<(&str, &str)> = HashSet::new();
    for edge in edges {
        if edge.edge_type.to_ascii_lowercase() != wanted {
            continue;
        }
        if edge.from == edge.to {
            continue;
        }
        directed.insert((edge.from.as_str(), edge.to.as_str()));
    }

    let mut pairs: Vec<(String, String)> = directed
        .iter()
        .filter(|(from, to)| directed.contains(&(*to, *from)))
        .map(|(from, to)| {
            if from < to {
                ((*from).to_string(), (*to).to_string())
            } else {
                ((*to).to_string(), (*from).to_string())
            }
        })
        .collect();

    pairs.sort();
    pairs.dedup();
    pairs
}

/// Degree counts for every node touched by one of `edge_types`.
///
/// Unlike the SurrealQL implementation there is no output-size limit, so this is exact
/// for the whole project rather than a bounded candidate set.
///
/// Self-loops are counted in both directions, matching the stored procedures: a
/// `x -> x` edge contributes one afferent and one efferent degree to `x`.
pub fn degrees(edges: &[EdgeRow], edge_types: &[&str]) -> HashMap<String, Degrees> {
    let wanted: HashSet<String> = edge_types.iter().map(|t| t.to_ascii_lowercase()).collect();

    let mut out: HashMap<String, Degrees> = HashMap::new();
    for edge in edges {
        if !wanted.is_empty() && !wanted.contains(&edge.edge_type.to_ascii_lowercase()) {
            continue;
        }
        out.entry(edge.to.clone()).or_default().afferent += 1;
        out.entry(edge.from.clone()).or_default().efferent += 1;
    }
    out
}

/// Nodes ranked by total degree, descending, ties broken by id for determinism.
///
/// `min_degree` filters after counting, matching `fn::get_hub_nodes($p, $min_degree)`.
pub fn hubs(edges: &[EdgeRow], edge_types: &[&str], min_degree: usize) -> Vec<(String, Degrees)> {
    let mut ranked: Vec<(String, Degrees)> = degrees(edges, edge_types)
        .into_iter()
        .filter(|(_, d)| d.total() >= min_degree)
        .collect();
    ranked.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// Coupling and risk for one complexity candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComplexityScore {
    /// Dependency edges pointing at the node.
    pub afferent: usize,
    /// Dependency edges leaving the node.
    pub efferent: usize,
    /// `efferent / (afferent + efferent)` rounded to four decimals; 0 when isolated.
    pub instability: f32,
    /// `complexity * (afferent + 1)`.
    pub risk_score: f32,
}

/// Score complexity candidates exactly like the tail of `fn::get_complexity_hotspots`.
///
/// `complexities` must already be the SQL-side candidate set (`node_type` matched and
/// `complexity >= threshold`, ordered by complexity and bounded to `limit * 2`): this
/// function only adds coupling, instability and risk, then sorts by risk and truncates.
///
/// Scoring reads coupling from [`degrees`], so a self-loop counts once as afferent and
/// once as efferent, matching the stored procedure's `GROUP BY` + `count()`.
pub fn score_hotspots(
    complexities: &[(String, f32)],
    edges: &[EdgeRow],
    limit: usize,
) -> Vec<(String, ComplexityScore)> {
    if complexities.is_empty() || limit == 0 {
        return Vec::new();
    }

    let degrees = degrees(edges, DEPENDENCY_EDGE_TYPES);
    let mut scored: Vec<(String, ComplexityScore)> = complexities
        .iter()
        .map(|(id, complexity)| {
            let coupled = degrees.get(id).copied().unwrap_or_default();
            let total = coupled.total();
            // `math::round(x * 10000f) / 10000f`.
            let instability = if total > 0 {
                ((coupled.efferent as f32 / total as f32) * 10_000.0).round() / 10_000.0
            } else {
                0.0
            };
            let risk_score = *complexity * (coupled.afferent as f32 + 1.0);
            (
                id.clone(),
                ComplexityScore {
                    afferent: coupled.afferent,
                    efferent: coupled.efferent,
                    instability,
                    risk_score,
                },
            )
        })
        .collect();

    // `ORDER BY risk_score DESC`; ties fall back to the id so the output is deterministic.
    scored.sort_by(|a, b| {
        b.1.risk_score
            .partial_cmp(&a.1.risk_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(limit);
    scored
}

/// Traversal direction for [`expand_bfs`].
///
/// `fn::expand($project_id, $start, $edge_name, $direction, $max_depth)` accepts `'out'`
/// for dependencies and `'in'` for dependents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow edges backwards (`to` -> `from`), i.e. dependents.
    In,
    /// Follow edges forwards (`from` -> `to`), i.e. dependencies.
    Out,
}

/// Depth clamp used by `fn::get_transitive_dependencies` and `fn::get_reverse_dependencies`:
/// values in `1..=10` are honoured, anything else falls back to 3.
pub fn safe_depth(requested: i32) -> usize {
    if (1..=10).contains(&requested) {
        requested as usize
    } else {
        3
    }
}

/// Breadth-first expansion over one edge type, mirroring `fn::expand`.
///
/// Returns `(node_id, depth)` pairs with the first depth each node was reached at, in
/// discovery order. The start node is seeded into `visited`, so a cycle never re-visits it
/// and a self-loop is dropped.
///
/// Unlike the SurrealQL fold this walks every edge row once per level; filtering by
/// `edge_type` happens here rather than in the query so the function stays pure.
pub fn expand_bfs(
    edges: &[EdgeRow],
    start: &str,
    edge_type: &str,
    direction: Direction,
    max_depth: usize,
) -> Vec<(String, usize)> {
    let wanted = edge_type.to_ascii_lowercase();

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.to_string());
    let mut frontier: Vec<String> = vec![start.to_string()];
    let mut reached: Vec<(String, usize)> = Vec::new();

    for depth in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let current: HashSet<&str> = frontier.iter().map(String::as_str).collect();

        let mut next: Vec<String> = Vec::new();
        let mut queued: HashSet<String> = HashSet::new();
        for edge in edges {
            if edge.edge_type.to_ascii_lowercase() != wanted {
                continue;
            }
            // `in` walks `to` -> `from`; `out` walks `from` -> `to`.
            let (anchor, candidate) = match direction {
                Direction::In => (&edge.to, &edge.from),
                Direction::Out => (&edge.from, &edge.to),
            };
            if !current.contains(anchor.as_str()) {
                continue;
            }
            if visited.contains(candidate) || !queued.insert(candidate.clone()) {
                continue;
            }
            next.push(candidate.clone());
        }

        for id in &next {
            visited.insert(id.clone());
        }
        reached.extend(next.iter().map(|id| (id.clone(), depth)));
        frontier = next;
    }

    reached
}

/// Depth clamp used by `fn::trace_call_chain`: values in `1..=10` are honoured, anything
/// else falls back to 5.
///
/// This is deliberately separate from [`safe_depth`]: the call-chain walk defaults to 5,
/// while the dependency walks default to 3.
pub fn safe_depth_for_call_chain(requested: i32) -> usize {
    if (1..=10).contains(&requested) {
        requested as usize
    } else {
        5
    }
}

/// One node reached by [`trace_bfs`], with the name path that led to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceStep {
    pub id: String,
    pub path: Vec<String>,
    pub depth: usize,
}

/// Breadth-first call walk mirroring `fn::trace_call_chain`.
///
/// Only `calls` edges are followed, in the `out` direction. The start node is seeded into
/// `visited`, so a cycle never re-visits it and a self-loop is dropped. A callee reached by
/// several edges keeps the first edge encountered, which fixes its parent and therefore its
/// path.
///
/// `path` starts with `start_name`; later entries are the reached node ids, because an
/// [`EdgeRow`] carries no callee name. The native call chain never exposes the path
/// (`CallChainNode` has no `call_path` field), so this only matters to callers of this
/// function.
pub fn trace_bfs(
    edges: &[EdgeRow],
    start: &str,
    start_name: &str,
    max_depth: usize,
) -> Vec<TraceStep> {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.to_string());
    let mut frontier: Vec<(String, Vec<String>)> =
        vec![(start.to_string(), vec![start_name.to_string()])];
    let mut reached: Vec<TraceStep> = Vec::new();

    for depth in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }

        let parent_paths: HashMap<&str, &[String]> = frontier
            .iter()
            .map(|(id, path)| (id.as_str(), path.as_slice()))
            .collect();

        let mut next: Vec<(String, Vec<String>)> = Vec::new();
        let mut queued: HashSet<String> = HashSet::new();
        for edge in edges {
            if edge.edge_type.to_ascii_lowercase() != "calls" {
                continue;
            }
            let Some(parent_path) = parent_paths.get(edge.from.as_str()) else {
                continue;
            };
            if visited.contains(&edge.to) || !queued.insert(edge.to.clone()) {
                continue;
            }
            let mut path = (*parent_path).to_vec();
            path.push(edge.to.clone());
            next.push((edge.to.clone(), path));
        }

        for (id, _) in &next {
            visited.insert(id.clone());
        }
        reached.extend(next.iter().map(|(id, path)| TraceStep {
            id: id.clone(),
            path: path.clone(),
            depth,
        }));
        frontier = next;
    }

    reached
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, to: &str, kind: &str) -> EdgeRow {
        EdgeRow::new(from, to, kind)
    }

    fn expand(
        edges: &[EdgeRow],
        start: &str,
        kind: &str,
        direction: Direction,
        depth: i32,
    ) -> Vec<(String, usize)> {
        expand_bfs(edges, start, kind, direction, safe_depth(depth))
    }

    /// Fixture mirroring the differential test: a -> b -> c -> a (calls), b -> d (calls),
    /// d -> a (imports).
    fn chain() -> Vec<EdgeRow> {
        vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:b", "nodes:c", "calls"),
            edge("nodes:c", "nodes:a", "calls"),
            edge("nodes:b", "nodes:d", "calls"),
            edge("nodes:d", "nodes:a", "imports"),
        ]
    }

    #[test]
    fn expand_bfs_honours_the_depth_limit() {
        let edges = chain();
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 1),
            vec![("nodes:b".to_string(), 1)]
        );
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 2),
            vec![
                ("nodes:b".to_string(), 1),
                ("nodes:c".to_string(), 2),
                ("nodes:d".to_string(), 2)
            ]
        );
        // Nothing new appears at depth 3, so it matches depth 2 rather than repeating.
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 3),
            expand(&edges, "nodes:a", "calls", Direction::Out, 2)
        );
    }

    #[test]
    fn safe_depth_falls_back_to_three() {
        assert_eq!(safe_depth(1), 1);
        assert_eq!(safe_depth(10), 10);
        for rejected in [0, -1, 11, 100] {
            assert_eq!(safe_depth(rejected), 3, "depth {rejected}");
        }
        // The default is what the stored procedure actually walks.
        let edges = chain();
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 0),
            expand(&edges, "nodes:a", "calls", Direction::Out, 3)
        );
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 11),
            expand(&edges, "nodes:a", "calls", Direction::Out, 3)
        );
    }

    #[test]
    fn expand_bfs_does_not_revisit_nodes_in_a_cycle() {
        let edges = chain();
        let reached = expand(&edges, "nodes:c", "calls", Direction::Out, 10);
        // c -> a -> b -> c would loop forever without the visited set.
        assert_eq!(
            reached,
            vec![
                ("nodes:a".to_string(), 1),
                ("nodes:b".to_string(), 2),
                ("nodes:d".to_string(), 3),
            ]
        );
        let ids: Vec<&str> = reached.iter().map(|(id, _)| id.as_str()).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "each node is reached once");
        assert!(
            !ids.contains(&"nodes:c"),
            "the start node must stay excluded"
        );
    }

    #[test]
    fn expand_bfs_direction_in_and_out_differ() {
        let edges = chain();
        let out = expand(&edges, "nodes:c", "calls", Direction::Out, 3);
        let incoming = expand(&edges, "nodes:c", "calls", Direction::In, 3);
        assert_eq!(
            out,
            vec![
                ("nodes:a".to_string(), 1),
                ("nodes:b".to_string(), 2),
                ("nodes:d".to_string(), 3),
            ]
        );
        assert_eq!(
            incoming,
            vec![("nodes:b".to_string(), 1), ("nodes:a".to_string(), 2),]
        );
    }

    #[test]
    fn expand_bfs_filters_by_edge_type() {
        let edges = chain();
        assert!(expand(&edges, "nodes:a", "imports", Direction::Out, 3).is_empty());
        assert_eq!(
            expand(&edges, "nodes:d", "imports", Direction::Out, 1),
            vec![("nodes:a".to_string(), 1)]
        );
        // The imports edge must not leak into a calls walk.
        assert!(expand(&edges, "nodes:d", "calls", Direction::Out, 3).is_empty());
        // Type matching is case-insensitive, like `string::lowercase($edge_type)`.
        assert_eq!(
            expand(&edges, "nodes:d", "IMPORTS", Direction::Out, 1),
            vec![("nodes:a".to_string(), 1)]
        );
    }

    #[test]
    fn expand_bfs_drops_self_loops() {
        let edges = vec![
            edge("nodes:a", "nodes:a", "calls"),
            edge("nodes:a", "nodes:b", "calls"),
        ];
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 10),
            vec![("nodes:b".to_string(), 1)]
        );
        // A start node with only a self-loop reaches nothing at all.
        let only_loop = vec![edge("nodes:z", "nodes:z", "calls")];
        assert!(expand(&only_loop, "nodes:z", "calls", Direction::Out, 10).is_empty());
        assert!(expand(&only_loop, "nodes:z", "calls", Direction::In, 10).is_empty());
    }

    #[test]
    fn expand_bfs_on_an_empty_graph_returns_nothing() {
        assert!(expand(&[], "nodes:a", "calls", Direction::Out, 3).is_empty());
        assert!(expand(&[], "nodes:a", "calls", Direction::In, 3).is_empty());
    }

    #[test]
    fn expand_bfs_records_the_first_depth_a_node_is_reached_at() {
        // a reaches b directly at depth 1 and again through x at depth 2.
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:a", "nodes:x", "calls"),
            edge("nodes:x", "nodes:b", "calls"),
        ];
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 3),
            vec![("nodes:b".to_string(), 1), ("nodes:x".to_string(), 1),]
        );
    }

    #[test]
    fn expand_bfs_stops_when_the_frontier_runs_dry() {
        let edges = vec![edge("nodes:a", "nodes:b", "calls")];
        assert_eq!(
            expand(&edges, "nodes:a", "calls", Direction::Out, 10),
            vec![("nodes:b".to_string(), 1)]
        );
    }

    #[test]
    fn mutual_pairs_reports_each_cycle_once_in_canonical_order() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:b", "nodes:a", "calls"),
        ];
        assert_eq!(
            mutual_pairs(&edges, "calls"),
            vec![("nodes:a".into(), "nodes:b".into())]
        );
    }

    #[test]
    fn mutual_pairs_ignores_one_way_edges_self_loops_and_other_types() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:c", "nodes:c", "calls"),
            edge("nodes:d", "nodes:e", "calls"),
            edge("nodes:e", "nodes:d", "imports"),
        ];
        assert!(mutual_pairs(&edges, "calls").is_empty());
        assert_eq!(
            mutual_pairs(&edges, "imports"),
            Vec::<(String, String)>::new(),
        );
    }

    #[test]
    fn mutual_pairs_collapses_duplicate_edges() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:b", "nodes:a", "calls"),
            edge("nodes:b", "nodes:a", "calls"),
        ];
        assert_eq!(
            mutual_pairs(&edges, "calls"),
            vec![("nodes:a".into(), "nodes:b".into())]
        );
    }

    #[test]
    fn mutual_pairs_is_case_insensitive_on_edge_type() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "Calls"),
            edge("nodes:b", "nodes:a", "calls"),
        ];
        assert_eq!(mutual_pairs(&edges, "calls").len(), 1);
    }

    #[test]
    fn degrees_counts_both_directions_and_filters_by_type() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:c", "nodes:b", "calls"),
            edge("nodes:b", "nodes:d", "calls"),
            edge("nodes:e", "nodes:b", "imports"),
        ];
        let calls = degrees(&edges, &["calls"]);
        assert_eq!(
            calls["nodes:b"],
            Degrees {
                afferent: 2,
                efferent: 1
            }
        );
        assert_eq!(
            calls["nodes:a"],
            Degrees {
                afferent: 0,
                efferent: 1
            }
        );
        assert!(!calls.contains_key("nodes:e"));

        let all = degrees(&edges, &[]);
        assert_eq!(
            all["nodes:b"],
            Degrees {
                afferent: 3,
                efferent: 1
            }
        );
    }

    #[test]
    fn degrees_counts_self_loops() {
        let edges = vec![edge("nodes:a", "nodes:a", "calls")];
        let counted = degrees(&edges, &[]);
        assert_eq!(
            counted["nodes:a"],
            Degrees {
                afferent: 1,
                efferent: 1,
            }
        );
        assert_eq!(counted["nodes:a"].total(), 2);
    }

    #[test]
    fn hubs_filters_by_min_degree_and_orders_deterministically() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:c", "nodes:b", "calls"),
            edge("nodes:b", "nodes:d", "calls"),
            edge("nodes:x", "nodes:y", "calls"),
        ];
        let ranked = hubs(&edges, &["calls"], 2);
        assert_eq!(ranked[0].0, "nodes:b");
        assert_eq!(ranked[0].1.total(), 3);
        assert_eq!(
            ranked.iter().filter(|(id, _)| id == "nodes:x").count(),
            0,
            "degree-1 nodes must be filtered out"
        );
    }

    #[test]
    fn hubs_is_exact_beyond_the_surrealdb_array_limit() {
        // 1200 distinct leaves -> 1201 nodes with degree > 0. The SurrealQL version can
        // only rank a bounded candidate set; this must count every node.
        let mut edges = Vec::new();
        for i in 0..1200 {
            edges.push(edge("nodes:centre", &format!("nodes:leaf{i}"), "calls"));
        }
        let ranked = hubs(&edges, &["calls"], 1);
        assert_eq!(ranked.len(), 1201);
        assert_eq!(ranked[0].0, "nodes:centre");
        assert_eq!(ranked[0].1.efferent, 1200);
    }

    fn depths(steps: &[TraceStep]) -> Vec<(String, usize)> {
        steps.iter().map(|s| (s.id.clone(), s.depth)).collect()
    }

    #[test]
    fn safe_depth_for_call_chain_falls_back_to_five() {
        assert_eq!(safe_depth_for_call_chain(1), 1);
        assert_eq!(safe_depth_for_call_chain(10), 10);
        for rejected in [0, -1, 11, 100] {
            assert_eq!(safe_depth_for_call_chain(rejected), 5, "depth {rejected}");
        }
        // The dependency walk still defaults to 3; the call-chain walk must not share it.
        assert_eq!(safe_depth_for_call_chain(0), 5);
        assert_eq!(safe_depth(0), 3);
    }

    #[test]
    fn trace_bfs_honours_the_depth_limit() {
        let edges = chain();
        assert_eq!(
            depths(&trace_bfs(&edges, "nodes:a", "a", 1)),
            vec![("nodes:b".to_string(), 1)]
        );
        // Depth 2 adds c (via b) and d (via b). c -> a is a cycle back to the visited start.
        assert_eq!(
            depths(&trace_bfs(&edges, "nodes:a", "a", 2)),
            vec![
                ("nodes:b".to_string(), 1),
                ("nodes:c".to_string(), 2),
                ("nodes:d".to_string(), 2),
            ]
        );
        // Depth 3 discovers nothing new: the only remaining edge is d -> a (imports).
        assert_eq!(
            trace_bfs(&edges, "nodes:a", "a", 3),
            trace_bfs(&edges, "nodes:a", "a", 2)
        );
    }

    #[test]
    fn trace_bfs_records_the_path_from_the_start() {
        let edges = chain();
        let reached = trace_bfs(&edges, "nodes:a", "parse_config", 2);
        assert_eq!(
            reached,
            vec![
                TraceStep {
                    id: "nodes:b".into(),
                    path: vec!["parse_config".into(), "nodes:b".into()],
                    depth: 1,
                },
                TraceStep {
                    id: "nodes:c".into(),
                    path: vec!["parse_config".into(), "nodes:b".into(), "nodes:c".into()],
                    depth: 2,
                },
                TraceStep {
                    id: "nodes:d".into(),
                    path: vec!["parse_config".into(), "nodes:b".into(), "nodes:d".into()],
                    depth: 2,
                },
            ]
        );
    }

    #[test]
    fn trace_bfs_terminates_on_a_cycle() {
        let edges = chain();
        // c -> a -> b -> c would loop forever without the visited set.
        assert_eq!(
            depths(&trace_bfs(&edges, "nodes:c", "c", 10)),
            vec![
                ("nodes:a".to_string(), 1),
                ("nodes:b".to_string(), 2),
                ("nodes:d".to_string(), 3),
            ]
        );
    }

    #[test]
    fn trace_bfs_diamond_keeps_only_the_first_path() {
        // a reaches d twice; the b edge is seen first, so b is d's parent.
        let edges = vec![
            edge("nodes:a", "nodes:b", "calls"),
            edge("nodes:a", "nodes:c", "calls"),
            edge("nodes:b", "nodes:d", "calls"),
            edge("nodes:c", "nodes:d", "calls"),
        ];
        let reached = trace_bfs(&edges, "nodes:a", "a", 5);
        assert_eq!(
            depths(&reached),
            vec![
                ("nodes:b".to_string(), 1),
                ("nodes:c".to_string(), 1),
                ("nodes:d".to_string(), 2),
            ]
        );
        let d = reached
            .iter()
            .find(|s| s.id == "nodes:d")
            .expect("d reached");
        assert_eq!(d.path, vec!["a", "nodes:b", "nodes:d"]);
    }

    #[test]
    fn trace_bfs_drops_self_loops() {
        let edges = vec![
            edge("nodes:a", "nodes:a", "calls"),
            edge("nodes:a", "nodes:b", "calls"),
        ];
        assert_eq!(
            depths(&trace_bfs(&edges, "nodes:a", "a", 10)),
            vec![("nodes:b".to_string(), 1)]
        );
        let only_loop = vec![edge("nodes:z", "nodes:z", "calls")];
        assert!(trace_bfs(&only_loop, "nodes:z", "z", 10).is_empty());
    }

    #[test]
    fn trace_bfs_on_an_empty_graph_returns_nothing() {
        assert!(trace_bfs(&[], "nodes:a", "a", 5).is_empty());
    }

    #[test]
    fn trace_bfs_ignores_other_edge_types() {
        let edges = vec![
            edge("nodes:a", "nodes:b", "imports"),
            edge("nodes:a", "nodes:c", "calls"),
        ];
        assert_eq!(
            depths(&trace_bfs(&edges, "nodes:a", "a", 5)),
            vec![("nodes:c".to_string(), 1)]
        );
    }

    fn candidates(rows: &[(&str, f32)]) -> Vec<(String, f32)> {
        rows.iter()
            .map(|(id, complexity)| ((*id).to_string(), *complexity))
            .collect()
    }

    #[test]
    fn score_hotspots_counts_coupling_and_rounds_instability() {
        let edges = vec![
            edge("nodes:x", "nodes:a", "calls"),
            edge("nodes:y", "nodes:a", "imports"),
            edge("nodes:a", "nodes:z", "uses"),
        ];
        let scored = score_hotspots(&candidates(&[("nodes:a", 10.0)]), &edges, 10);
        assert_eq!(
            scored,
            vec![(
                "nodes:a".to_string(),
                ComplexityScore {
                    afferent: 2,
                    efferent: 1,
                    instability: 0.3333,
                    risk_score: 30.0,
                }
            )]
        );
    }

    #[test]
    fn score_hotspots_counts_self_loops_and_isolated_nodes_report_zero() {
        let edges = vec![
            edge("nodes:a", "nodes:a", "calls"),
            edge("nodes:b", "nodes:b", "imports"),
        ];
        let scored = score_hotspots(
            &candidates(&[("nodes:a", 8.0), ("nodes:c", 2.0)]),
            &edges,
            10,
        );
        let scores: HashMap<&str, ComplexityScore> = scored
            .iter()
            .map(|(id, score)| (id.as_str(), *score))
            .collect();
        assert_eq!(
            scores["nodes:a"],
            ComplexityScore {
                afferent: 1,
                efferent: 1,
                instability: 0.5,
                risk_score: 16.0,
            }
        );
        assert_eq!(scores["nodes:c"].risk_score, 2.0);
        assert_eq!(scores["nodes:c"].afferent, 0);
        assert_eq!(scores["nodes:c"].efferent, 0);
        assert_eq!(scores["nodes:c"].instability, 0.0);
    }

    #[test]
    fn score_hotspots_only_counts_dependency_edges() {
        let edges = vec![
            edge("nodes:x", "nodes:a", "contains"),
            edge("nodes:y", "nodes:a", "calls"),
        ];
        let scored = score_hotspots(&candidates(&[("nodes:a", 6.0)]), &edges, 10);
        assert_eq!(scored[0].1.afferent, 1);
        assert_eq!(scored[0].1.risk_score, 12.0);
    }

    #[test]
    fn score_hotspots_orders_by_risk_and_truncates_to_limit() {
        // a: 10 * (1 + 1) = 20; d: 9 * (1 + 1) = 18; b: 4 * (3 + 1) = 16; c: 1 * (0 + 1) = 1.
        let edges = vec![
            edge("nodes:x", "nodes:a", "calls"),
            edge("nodes:w", "nodes:b", "calls"),
            edge("nodes:v", "nodes:b", "calls"),
            edge("nodes:u", "nodes:b", "calls"),
            edge("nodes:t", "nodes:d", "calls"),
            // u/v/w/x/t are not candidates; edges to non-candidates must not leak in.
            edge("nodes:b", "nodes:y", "calls"),
        ];
        let all = score_hotspots(
            &candidates(&[
                ("nodes:a", 10.0),
                ("nodes:b", 4.0),
                ("nodes:c", 1.0),
                ("nodes:d", 9.0),
            ]),
            &edges,
            10,
        );
        let ids: Vec<&str> = all.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["nodes:a", "nodes:d", "nodes:b", "nodes:c"]);

        let top2 = score_hotspots(
            &candidates(&[("nodes:c", 1.0), ("nodes:d", 9.0), ("nodes:a", 10.0)]),
            &edges,
            2,
        );
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].0, "nodes:a");
        assert_eq!(top2[1].0, "nodes:d");
    }

    #[test]
    fn score_hotspots_ties_break_by_id_and_limit_zero_returns_nothing() {
        // Equal complexity and no coupling -> equal risk; ids must order the tie.
        let scored = score_hotspots(&candidates(&[("nodes:z", 3.0), ("nodes:a", 3.0)]), &[], 10);
        let ids: Vec<&str> = scored.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["nodes:a", "nodes:z"]);

        assert!(score_hotspots(&candidates(&[("nodes:a", 3.0)]), &[], 0).is_empty());
        assert!(score_hotspots(&[], &[], 10).is_empty());
    }
}
