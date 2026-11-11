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

use std::collections::{HashMap, HashSet};

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
pub fn degrees(edges: &[EdgeRow], edge_types: &[&str]) -> HashMap<String, Degrees> {
    let wanted: HashSet<String> = edge_types.iter().map(|t| t.to_ascii_lowercase()).collect();

    let mut out: HashMap<String, Degrees> = HashMap::new();
    for edge in edges {
        if !wanted.is_empty() && !wanted.contains(&edge.edge_type.to_ascii_lowercase()) {
            continue;
        }
        if edge.from == edge.to {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, to: &str, kind: &str) -> EdgeRow {
        EdgeRow::new(from, to, kind)
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
    fn degrees_ignores_self_loops() {
        let edges = vec![edge("nodes:a", "nodes:a", "calls")];
        assert!(degrees(&edges, &[]).is_empty());
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
}
