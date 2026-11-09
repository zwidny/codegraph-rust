// ABOUTME: Offline "plan" checker that validates an LLM's draft logic graph
// ABOUTME: (nodes, edges, constraints) without touching SurrealDB or providers.
//
// The point of this command is to externalise reasoning that does not exist in
// code yet. An LLM writes down the entities, the relations and the invariants it
// is planning, then the same structural operators used against a real code graph
// (reachability, cycles, layering, degree) check that reasoning for consistency.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub struct PlanArgs {
    /// Plan file (JSON or YAML); `-` reads JSON from stdin.
    pub file: String,

    #[arg(short, long, value_enum, default_value_t = PlanFormat::Human)]
    pub format: PlanFormat,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanFormat {
    Human,
    Json,
}

fn default_edge_kind() -> String {
    "depends".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PlanDoc {
    #[serde(default)]
    pub nodes: Vec<PlanNode>,
    #[serde(default)]
    pub edges: Vec<PlanEdge>,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PlanNode {
    pub id: String,
    /// Optional abstraction level used by the `layering` constraint.
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PlanEdge {
    pub from: String,
    pub to: String,
    #[serde(rename = "type", default = "default_edge_kind")]
    pub kind: String,
}

/// An invariant the planned design must satisfy. Every rule is evaluated on the
/// draft graph alone, so it applies before a single line of code exists.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Constraint {
    /// No cycles, optionally only over one edge kind.
    Acyclic {
        #[serde(default)]
        edge_kind: Option<String>,
    },
    /// Edges must not point from a later layer back to an earlier one.
    /// `order` is low -> high; edges inside the same layer are allowed.
    Layering { order: Vec<String> },
    /// Every `source` must reach `target` (transitively).
    Coverage {
        sources: Vec<String>,
        target: String,
        #[serde(default)]
        edge_kind: Option<String>,
    },
    /// `from` must not reach `to`; the allowed direction is the other way.
    NoPath {
        from: String,
        to: String,
        #[serde(default)]
        edge_kind: Option<String>,
    },
    /// No node may have more than `threshold` incoming edges.
    MaxFanIn { threshold: usize },
    /// No node may have more than `threshold` outgoing edges.
    MaxFanOut { threshold: usize },
}

#[derive(Debug, Serialize)]
pub struct CheckOutcome {
    pub rule: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Violation {
    pub rule: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct PlanReport {
    pub ok: bool,
    pub nodes: usize,
    pub edges: usize,
    pub checks: Vec<CheckOutcome>,
    pub violations: Vec<Violation>,
    pub warnings: Vec<String>,
}

pub fn run(args: PlanArgs) -> Result<()> {
    let (doc, source) = load(&args.file)?;
    let report = analyze(&doc);

    match args.format {
        PlanFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        PlanFormat::Human => print_human(&source, &report),
    }

    if !report.ok {
        bail!(
            "plan check reported {} violation(s); see the output above",
            report.violations.len()
        );
    }
    Ok(())
}

fn load(file: &str) -> Result<(PlanDoc, String)> {
    let (text, source) = if file == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .context("read plan from stdin")?;
        (buffer, "<stdin>".to_string())
    } else {
        let path = PathBuf::from(file);
        (
            std::fs::read_to_string(&path)
                .with_context(|| format!("read plan file {}", path.display()))?,
            path.display().to_string(),
        )
    };

    let doc = if file.ends_with(".yaml") || file.ends_with(".yml") {
        serde_yaml::from_str(&text).context("parse plan YAML")?
    } else {
        serde_json::from_str(&text).context("parse plan JSON")?
    };
    Ok((doc, source))
}

/// Evaluate every declared constraint plus the always-on structural checks.
pub fn analyze(doc: &PlanDoc) -> PlanReport {
    let node_ids: HashSet<&str> = doc.nodes.iter().map(|node| node.id.as_str()).collect();
    let mut violations = Vec::new();
    let mut warnings = Vec::new();
    let mut checks = Vec::new();

    // Always-on: edges must reference declared nodes. A dangling reference is the
    // most common sign that the reasoning is still incomplete or has a typo.
    let mut dangling = Vec::new();
    for edge in &doc.edges {
        if !node_ids.contains(edge.from.as_str()) {
            dangling.push(format!(
                "edge {} -> {}: unknown node '{}'",
                edge.from, edge.to, edge.from
            ));
        }
        if !node_ids.contains(edge.to.as_str()) {
            dangling.push(format!(
                "edge {} -> {}: unknown node '{}'",
                edge.from, edge.to, edge.to
            ));
        }
    }
    checks.push(CheckOutcome {
        rule: "known_nodes".to_string(),
        ok: dangling.is_empty(),
        detail: (!dangling.is_empty()).then(|| dangling.join("; ")),
    });
    for detail in dangling {
        violations.push(Violation {
            rule: "known_nodes".to_string(),
            detail,
        });
    }

    for constraint in &doc.constraints {
        match constraint {
            Constraint::Acyclic { edge_kind } => {
                let adjacency = adjacency(&doc.edges, edge_kind.as_deref());
                match find_cycle(&adjacency) {
                    Some(cycle) => {
                        let detail = format!("cycle: {}", cycle.join(" -> "));
                        checks.push(CheckOutcome {
                            rule: "acyclic".to_string(),
                            ok: false,
                            detail: Some(detail.clone()),
                        });
                        violations.push(Violation {
                            rule: "acyclic".to_string(),
                            detail,
                        });
                    }
                    None => checks.push(CheckOutcome {
                        rule: "acyclic".to_string(),
                        ok: true,
                        detail: None,
                    }),
                }
            }
            Constraint::Layering { order } => {
                let index: HashMap<&str, usize> = order
                    .iter()
                    .enumerate()
                    .map(|(position, layer)| (layer.as_str(), position))
                    .collect();
                let mut local = Vec::new();
                for node in &doc.nodes {
                    if let Some(layer) = &node.layer {
                        if !index.contains_key(layer.as_str()) {
                            warnings.push(format!(
                                "node '{}' has layer '{}' not listed in layering order",
                                node.id, layer
                            ));
                        }
                    }
                }
                for edge in &doc.edges {
                    let (Some(from_layer), Some(to_layer)) = (
                        layer_of(&doc.nodes, &edge.from),
                        layer_of(&doc.nodes, &edge.to),
                    ) else {
                        continue;
                    };
                    let (Some(&from_rank), Some(&to_rank)) =
                        (index.get(from_layer), index.get(to_layer))
                    else {
                        continue;
                    };
                    if from_rank > to_rank {
                        local.push(format!(
                            "edge {} -> {} goes from layer '{}' to '{}' (must not go high -> low)",
                            edge.from, edge.to, from_layer, to_layer
                        ));
                    }
                }
                checks.push(CheckOutcome {
                    rule: "layering".to_string(),
                    ok: local.is_empty(),
                    detail: (!local.is_empty()).then(|| local.join("; ")),
                });
                for detail in local {
                    violations.push(Violation {
                        rule: "layering".to_string(),
                        detail,
                    });
                }
            }
            Constraint::Coverage {
                sources,
                target,
                edge_kind,
            } => {
                let adjacency = adjacency(&doc.edges, edge_kind.as_deref());
                let mut local = Vec::new();
                for source in sources {
                    if source == target || !reaches(&adjacency, source, target) {
                        local.push(format!("'{source}' cannot reach '{target}'"));
                    }
                }
                checks.push(CheckOutcome {
                    rule: "coverage".to_string(),
                    ok: local.is_empty(),
                    detail: (!local.is_empty()).then(|| local.join("; ")),
                });
                for detail in local {
                    violations.push(Violation {
                        rule: "coverage".to_string(),
                        detail,
                    });
                }
            }
            Constraint::NoPath {
                from,
                to,
                edge_kind,
            } => {
                let adjacency = adjacency(&doc.edges, edge_kind.as_deref());
                let conflict = from == to || reaches(&adjacency, from, to);
                checks.push(CheckOutcome {
                    rule: "no_path".to_string(),
                    ok: !conflict,
                    detail: conflict
                        .then(|| format!("'{from}' reaches '{to}', which is forbidden")),
                });
                if conflict {
                    violations.push(Violation {
                        rule: "no_path".to_string(),
                        detail: format!("'{from}' reaches '{to}', which is forbidden"),
                    });
                }
            }
            Constraint::MaxFanIn { threshold } => {
                let mut local = Vec::new();
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for edge in &doc.edges {
                    *counts.entry(edge.to.as_str()).or_default() += 1;
                }
                for (node, count) in counts {
                    if count > *threshold {
                        local.push(format!("'{node}' has fan-in {count} > {threshold}"));
                    }
                }
                checks.push(CheckOutcome {
                    rule: "max_fan_in".to_string(),
                    ok: local.is_empty(),
                    detail: (!local.is_empty()).then(|| local.join("; ")),
                });
                for detail in local {
                    violations.push(Violation {
                        rule: "max_fan_in".to_string(),
                        detail,
                    });
                }
            }
            Constraint::MaxFanOut { threshold } => {
                let mut local = Vec::new();
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for edge in &doc.edges {
                    *counts.entry(edge.from.as_str()).or_default() += 1;
                }
                for (node, count) in counts {
                    if count > *threshold {
                        local.push(format!("'{node}' has fan-out {count} > {threshold}"));
                    }
                }
                checks.push(CheckOutcome {
                    rule: "max_fan_out".to_string(),
                    ok: local.is_empty(),
                    detail: (!local.is_empty()).then(|| local.join("; ")),
                });
                for detail in local {
                    violations.push(Violation {
                        rule: "max_fan_out".to_string(),
                        detail,
                    });
                }
            }
        }
    }

    PlanReport {
        ok: violations.is_empty(),
        nodes: doc.nodes.len(),
        edges: doc.edges.len(),
        checks,
        violations,
        warnings,
    }
}

fn layer_of<'a>(nodes: &'a [PlanNode], id: &str) -> Option<&'a str> {
    nodes
        .iter()
        .find(|node| node.id == id)
        .and_then(|node| node.layer.as_deref())
}

fn adjacency(edges: &[PlanEdge], kind: Option<&str>) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for edge in edges {
        if kind.is_some_and(|kind| edge.kind != kind) {
            continue;
        }
        map.entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }
    map
}

fn reaches(adjacency: &HashMap<String, Vec<String>>, from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(from);
    seen.insert(from);
    while let Some(current) = queue.pop_front() {
        if let Some(nexts) = adjacency.get(current) {
            for next in nexts {
                if next == to {
                    return true;
                }
                if seen.insert(next.as_str()) {
                    queue.push_back(next.as_str());
                }
            }
        }
    }
    false
}

/// Depth-first search returning the first cycle as a path, if the graph has one.
fn find_cycle(adjacency: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        New,
        Visiting,
        Done,
    }

    let mut state: HashMap<&str, State> = HashMap::new();
    // Iterative DFS keeps deep plan graphs from blowing the stack.
    for start in adjacency.keys() {
        if state.get(start.as_str()).copied().unwrap_or(State::New) != State::New {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
        let mut path: Vec<&str> = vec![start.as_str()];
        state.insert(start.as_str(), State::Visiting);

        while let Some((node, next_index)) = stack.last_mut() {
            let neighbors = adjacency.get(*node).map(Vec::as_slice).unwrap_or(&[]);
            if *next_index >= neighbors.len() {
                state.insert(node, State::Done);
                stack.pop();
                path.pop();
                continue;
            }
            let neighbor = neighbors[*next_index].as_str();
            *next_index += 1;

            match state.get(neighbor).copied().unwrap_or(State::New) {
                State::New => {
                    state.insert(neighbor, State::Visiting);
                    path.push(neighbor);
                    stack.push((neighbor, 0));
                }
                State::Visiting => {
                    if let Some(position) = path.iter().position(|entry| *entry == neighbor) {
                        let mut cycle: Vec<String> = path[position..]
                            .iter()
                            .map(|entry| entry.to_string())
                            .collect();
                        cycle.push(neighbor.to_string());
                        return Some(cycle);
                    }
                }
                State::Done => {}
            }
        }
    }
    None
}

fn print_human(source: &str, report: &PlanReport) {
    println!("Plan check: {source}");
    println!("Nodes: {}  Edges: {}", report.nodes, report.edges);
    for check in &report.checks {
        let marker = if check.ok { "PASS" } else { "FAIL" };
        match &check.detail {
            Some(detail) => println!("  [{marker}] {}: {detail}", check.rule),
            None => println!("  [{marker}] {}", check.rule),
        }
    }
    for warning in &report.warnings {
        println!("  [WARN] {warning}");
    }
    if report.ok {
        println!("Result: OK (no violations)");
    } else {
        println!("Result: {} violation(s)", report.violations.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> PlanNode {
        PlanNode {
            id: id.to_string(),
            layer: None,
            kind: None,
        }
    }

    fn layered(id: &str, layer: &str) -> PlanNode {
        PlanNode {
            id: id.to_string(),
            layer: Some(layer.to_string()),
            kind: None,
        }
    }

    fn edge(from: &str, to: &str) -> PlanEdge {
        PlanEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: "depends".to_string(),
        }
    }

    #[test]
    fn acyclic_flags_cycles() {
        let doc = PlanDoc {
            nodes: vec![node("a"), node("b")],
            edges: vec![edge("a", "b"), edge("b", "a")],
            constraints: vec![Constraint::Acyclic { edge_kind: None }],
        };
        let report = analyze(&doc);
        assert!(!report.ok);
        assert_eq!(report.violations[0].rule, "acyclic");
    }

    #[test]
    fn coverage_requires_reachability() {
        let doc = PlanDoc {
            nodes: vec![node("entry"), node("cache"), node("db")],
            edges: vec![edge("entry", "cache")],
            constraints: vec![Constraint::Coverage {
                sources: vec!["entry".to_string()],
                target: "db".to_string(),
                edge_kind: None,
            }],
        };
        let report = analyze(&doc);
        assert!(!report.ok);
        assert_eq!(report.violations[0].rule, "coverage");
    }

    #[test]
    fn layering_rejects_backward_edges() {
        let doc = PlanDoc {
            nodes: vec![layered("ui", "presentation"), layered("store", "infra")],
            edges: vec![edge("store", "ui")],
            constraints: vec![Constraint::Layering {
                order: vec!["presentation".to_string(), "infra".to_string()],
            }],
        };
        let report = analyze(&doc);
        assert!(!report.ok);
        assert_eq!(report.violations[0].rule, "layering");
    }

    #[test]
    fn layering_allows_edges_inside_a_layer() {
        let doc = PlanDoc {
            nodes: vec![layered("a", "vector"), layered("b", "vector")],
            edges: vec![edge("a", "b")],
            constraints: vec![Constraint::Layering {
                order: vec!["cli".to_string(), "vector".to_string()],
            }],
        };
        assert!(analyze(&doc).ok);
    }

    #[test]
    fn dangling_edges_are_reported() {
        let doc = PlanDoc {
            nodes: vec![node("a")],
            edges: vec![edge("a", "missing")],
            constraints: vec![],
        };
        let report = analyze(&doc);
        assert!(!report.ok);
        assert_eq!(report.violations[0].rule, "known_nodes");
    }

    #[test]
    fn no_path_rejects_reachability() {
        let doc = PlanDoc {
            nodes: vec![node("cli"), node("internals")],
            edges: vec![edge("cli", "internals")],
            constraints: vec![Constraint::NoPath {
                from: "cli".to_string(),
                to: "internals".to_string(),
                edge_kind: None,
            }],
        };
        let report = analyze(&doc);
        assert!(!report.ok);
        assert_eq!(report.violations[0].rule, "no_path");
    }

    #[test]
    fn fanned_out_hubs_are_flagged() {
        let doc = PlanDoc {
            nodes: vec![node("hub"), node("a"), node("b")],
            edges: vec![edge("hub", "a"), edge("hub", "b")],
            constraints: vec![Constraint::MaxFanOut { threshold: 1 }],
        };
        let report = analyze(&doc);
        assert!(!report.ok);
        assert_eq!(report.violations[0].rule, "max_fan_out");
    }

    #[test]
    fn clean_plan_is_ok() {
        let doc = PlanDoc {
            nodes: vec![node("a"), node("b")],
            edges: vec![edge("a", "b")],
            constraints: vec![Constraint::Acyclic { edge_kind: None }],
        };
        assert!(analyze(&doc).ok);
    }
}
