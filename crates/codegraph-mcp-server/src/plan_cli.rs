// ABOUTME: Offline "plan" checker that validates an LLM's draft logic graph
// ABOUTME: (nodes, edges, constraints) without touching SurrealDB or providers.
//
// The point of this command is to externalise reasoning that does not exist in
// code yet. An LLM writes down the entities, the relations and the invariants it
// is planning, then the same structural operators used against a real code graph
// (reachability, cycles, layering, degree) check that reasoning for consistency.
use anyhow::{Context, Result, bail};
use codegraph_core::{CodeNode, EdgeType, Location, NodeId, NodeType};
use codegraph_graph::CodeEdge;
use codegraph_graph::storage::{
    EdgeEndpoint, IndexStorage, NodeIdentityRow, ProjectMetadataRecord,
};
use codegraph_mcp::{StorageBackend, StorageMode, open_storage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum PlanAction {
    /// Validate a plan file offline (no database, no provider).
    Check {
        /// Plan file (JSON or YAML); `-` reads JSON from stdin.
        file: String,
        #[arg(short, long, value_enum, default_value_t = PlanFormat::Human)]
        format: PlanFormat,
    },
    /// Persist a plan's nodes/edges under `project_id = plan:<id>`.
    Store {
        /// Plan file (JSON or YAML).
        file: String,
        #[arg(long, help = "Plan id (defaults to the file stem)")]
        plan_id: Option<String>,
        #[arg(long, help = "Replace an existing stored plan")]
        force: bool,
    },
    /// Read a stored plan back as JSON/YAML-shaped nodes and edges.
    Show {
        plan_id: String,
        #[arg(short, long, value_enum, default_value_t = PlanFormat::Human)]
        format: PlanFormat,
    },
    /// Delete a stored plan and its nodes/edges.
    Drop { plan_id: String },
    /// Compare a plan against the code graph of a project.
    ///
    /// Reports planned nodes that already exist (possible duplicate work), planned
    /// edges already present, planned edges not implemented yet, and code dependencies
    /// between planned nodes that the plan forgot to record.
    Verify {
        /// Plan file (JSON or YAML); `-` reads JSON from stdin.
        file: String,
        #[arg(short, long, value_enum, default_value_t = PlanFormat::Human)]
        format: PlanFormat,
    },
}

impl PlanAction {
    /// `check` must run before configuration/provider loading.
    pub fn is_offline(&self) -> bool {
        matches!(self, PlanAction::Check { .. })
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct PlanArgs {
    /// Project root used to open the store and compare plans.
    ///
    /// Defaults to the current directory. A subdirectory (for example
    /// `crates/codegraph-core/benches`) resolves to its enclosing `.git` or
    /// `.codegraph` root, so every `plan` subcommand uses the same project as
    /// `codegraph index` and the MCP server.
    #[arg(long, global = true)]
    pub project: Option<PathBuf>,
    #[command(subcommand)]
    pub action: PlanAction,
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

/// Offline entry point (used before configuration is loaded).
pub fn run_offline(action: &PlanAction) -> Result<()> {
    match action {
        PlanAction::Check { file, format } => check(file, *format),
        _ => bail!("this plan action needs configuration; only `check` runs offline"),
    }
}

/// Persistent entry point (runs after configuration is loaded).
pub async fn run_async(action: PlanAction, project: Option<PathBuf>) -> Result<()> {
    match action {
        PlanAction::Check { file, format } => check(&file, format),
        PlanAction::Store {
            file,
            plan_id,
            force,
        } => store(&file, plan_id, force, project).await,
        PlanAction::Show { plan_id, format } => show(&plan_id, format, project).await,
        PlanAction::Drop { plan_id } => drop_plan(&plan_id, project).await,
        PlanAction::Verify { file, format } => verify(&file, project, format).await,
    }
}

fn check(file: &str, format: PlanFormat) -> Result<()> {
    let (doc, source) = load(file)?;
    let report = analyze(&doc);

    match format {
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

fn draft_project_id(plan_id: &str) -> String {
    format!("plan:{plan_id}")
}

fn default_plan_id(file: &str) -> String {
    if file == "-" {
        return "stdin".to_string();
    }
    PathBuf::from(file)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "plan".to_string())
}

/// Open one storage handle for a `plan` subcommand.
///
/// The backend comes from `CODEGRAPH_BACKEND` (the same selector `codegraph index`
/// uses); `mode` keeps read-only commands from issuing schema DDL.
async fn connect_storage_at(
    project_root: &Path,
    mode: StorageMode,
) -> Result<Arc<dyn IndexStorage>> {
    let backend = StorageBackend::from_env()?;
    open_storage(backend, project_root, mode).await
}

/// Resolve the project a `plan` command operates on.
///
/// A subdirectory (for example `crates/codegraph-core/benches`) resolves to its
/// enclosing `.git` or `.codegraph` root, matching how `codegraph index` and the MCP
/// server determine the project. This keeps `plan verify` and the store connection on
/// the same `project_id` as the indexed graph.
fn resolve_project_root(project: Option<PathBuf>) -> Result<PathBuf> {
    let start = match project {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let start = start.canonicalize().unwrap_or(start);
    let start_dir = if start.is_dir() {
        start.clone()
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| start.clone())
    };
    for marker in [".git", ".codegraph"] {
        if let Some(root) = start_dir.ancestors().find(|dir| dir.join(marker).exists()) {
            return Ok(root.to_path_buf());
        }
    }
    Ok(if start.is_dir() { start } else { start_dir })
}

async fn count_plan_nodes(storage: &Arc<dyn IndexStorage>, project: &str) -> Result<usize> {
    Ok(storage
        .node_count(project)
        .await
        .context("count plan nodes")? as usize)
}

async fn write_plan_constraints(
    storage: &Arc<dyn IndexStorage>,
    project: &str,
    constraints: &[Constraint],
) -> Result<()> {
    let constraints = serde_json::to_value(constraints)?;
    // The constraints live in `project_metadata.metadata.plan_constraints`, the same
    // place `plan show` reads them from. The remaining fields are the record's type
    // contract; a draft plan has no file/node/edge statistics of its own.
    let record = ProjectMetadataRecord {
        metadata: json!({ "plan_constraints": constraints }),
        project_id: project.to_string(),
        name: project.to_string(),
        root_path: project.to_string(),
        primary_language: None,
        file_count: 0,
        node_count: 0,
        edge_count: 0,
        avg_coverage_score: 0.0,
        last_analyzed: chrono::Utc::now(),
        codegraph_version: env!("CARGO_PKG_VERSION").to_string(),
        organization_id: None,
        domain: None,
    };
    storage
        .upsert_project_metadata(record)
        .await
        .context("write plan constraints")?;
    Ok(())
}

async fn read_plan_constraints(
    storage: &Arc<dyn IndexStorage>,
    project: &str,
) -> Result<Vec<Constraint>> {
    let Some(metadata) = storage
        .project_metadata_value(project)
        .await
        .context("read plan constraints")?
    else {
        return Ok(Vec::new());
    };
    let Some(value) = metadata
        .get("plan_constraints")
        .filter(|value| !value.is_null())
        .cloned()
    else {
        return Ok(Vec::new());
    };
    serde_json::from_value(value).context("parse stored plan constraints")
}

async fn store(
    file: &str,
    plan_id: Option<String>,
    force: bool,
    project_path: Option<PathBuf>,
) -> Result<()> {
    let (doc, source) = load(file)?;
    let plan_id = plan_id.unwrap_or_else(|| default_plan_id(file));
    let project = draft_project_id(&plan_id);

    let root = resolve_project_root(project_path)?;
    let storage = connect_storage_at(&root, StorageMode::ReadWrite).await?;
    let existing = count_plan_nodes(&storage, &project).await?;
    if existing > 0 {
        if !force {
            bail!("plan '{plan_id}' already stored ({existing} nodes); pass --force to replace it");
        }
        storage
            .delete_project(&project)
            .await
            .map_err(|error| anyhow::anyhow!("clear existing plan '{plan_id}': {error}"))?;
    }

    // Draft nodes reuse the `nodes` table but are scoped to `plan:<id>`, so a real
    // project query never sees them and they share no indexes with code.
    let mut id_by_name: HashMap<String, NodeId> = HashMap::new();
    let mut nodes = Vec::with_capacity(doc.nodes.len());
    for planned in &doc.nodes {
        let mut node = CodeNode::new(
            planned.id.clone(),
            Some(NodeType::Other("Draft".to_string())),
            None,
            Location {
                file_path: source.clone(),
                line: 0,
                column: 0,
                end_line: None,
                end_column: None,
            },
        );
        node.metadata
            .attributes
            .insert("project_id".to_string(), project.clone());
        node.metadata
            .attributes
            .insert("draft".to_string(), "true".to_string());
        if let Some(layer) = &planned.layer {
            node.metadata
                .attributes
                .insert("layer".to_string(), layer.clone());
        }
        if let Some(kind) = &planned.kind {
            node.metadata
                .attributes
                .insert("draft_kind".to_string(), kind.clone());
        }
        id_by_name.insert(planned.id.clone(), node.id);
        nodes.push(node);
    }
    storage
        .upsert_nodes(&nodes)
        .await
        .map_err(|error| anyhow::anyhow!("write plan nodes: {error}"))?;

    let mut edges = Vec::with_capacity(doc.edges.len());
    for planned in &doc.edges {
        let (Some(from), Some(to)) = (
            id_by_name.get(&planned.from).cloned(),
            id_by_name.get(&planned.to).cloned(),
        ) else {
            continue;
        };
        let edge_type: EdgeType = planned.kind.parse().unwrap_or(EdgeType::References);
        edges.push(CodeEdge::new(from, to, edge_type).with_project_id(project.clone()));
    }
    storage
        .upsert_edges(&edges)
        .await
        .map_err(|error| anyhow::anyhow!("write plan edges: {error}"))?;
    write_plan_constraints(&storage, &project, &doc.constraints).await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "stored": true,
            "plan_id": plan_id,
            "project_id": project,
            "nodes": nodes.len(),
            "edges": edges.len(),
            "constraints": doc.constraints.len(),
            "next": format!("codegraph plan show {plan_id}"),
        }))?
    );
    Ok(())
}

async fn show(plan_id: &str, format: PlanFormat, project_path: Option<PathBuf>) -> Result<()> {
    let project = draft_project_id(plan_id);
    let root = resolve_project_root(project_path)?;
    let storage = connect_storage_at(&root, StorageMode::ReadOnly).await?;
    if count_plan_nodes(&storage, &project).await? == 0 {
        bail!(
            "plan '{plan_id}' is not stored; run `codegraph plan store <file> --plan-id {plan_id}` first"
        );
    }

    let constraints = read_plan_constraints(&storage, &project).await?;

    // `nodes_with_metadata` already decodes the backend representation: SurrealDB
    // stores a compressed JSON string, Postgres a `jsonb` value.
    let node_rows = storage
        .nodes_with_metadata(&project)
        .await
        .context("read plan nodes")?;

    let mut name_by_record = HashMap::new();
    let mut nodes = Vec::with_capacity(node_rows.len());
    for row in node_rows {
        let layer = row
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("layer"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let kind = row
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("draft_kind"))
            .and_then(Value::as_str)
            .map(str::to_string);
        name_by_record.insert(row.id, row.name.clone());
        nodes.push(PlanNode {
            id: row.name,
            layer,
            kind,
        });
    }

    let edge_rows = storage
        .edge_endpoints(&project)
        .await
        .context("read plan edges")?;
    let edges = plan_edges_from_rows(&edge_rows, &name_by_record);

    // Constraints are persisted next to the draft graph in `project_metadata`.
    let doc = PlanDoc {
        nodes,
        edges,
        constraints,
    };

    match format {
        PlanFormat::Json => println!("{}", serde_json::to_string_pretty(&doc)?),
        PlanFormat::Human => {
            println!(
                "Plan '{plan_id}' (project_id={})",
                draft_project_id(plan_id)
            );
            for node in &doc.nodes {
                match &node.layer {
                    Some(layer) => println!("  node {} [layer={layer}]", node.id),
                    None => println!("  node {}", node.id),
                }
            }
            for edge in &doc.edges {
                println!("  edge {} -[{}]-> {}", edge.from, edge.kind, edge.to);
            }
            println!("Nodes: {}  Edges: {}", doc.nodes.len(), doc.edges.len());
            println!(
                "Constraints: {} (re-run `codegraph plan check` after editing the plan file)",
                doc.constraints.len()
            );
        }
    }
    Ok(())
}

async fn drop_plan(plan_id: &str, project_path: Option<PathBuf>) -> Result<()> {
    let project = draft_project_id(plan_id);
    let root = resolve_project_root(project_path)?;
    let storage = connect_storage_at(&root, StorageMode::ReadWrite).await?;
    let nodes = count_plan_nodes(&storage, &project).await?;
    if nodes == 0 {
        bail!("plan '{plan_id}' is not stored");
    }
    storage
        .delete_project(&project)
        .await
        .map_err(|error| anyhow::anyhow!("delete plan '{plan_id}': {error}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "dropped": true,
            "plan_id": plan_id,
            "project_id": project,
            "nodes": nodes,
        }))?
    );
    Ok(())
}

/// Code-graph view used to compare a plan against what already exists.
#[derive(Default)]
struct CodeIndex {
    names: HashMap<String, Vec<String>>,
    edges: HashSet<(String, String)>,
    nodes: usize,
    edge_count: usize,
}

/// Differences between a plan and the project's code graph.
#[derive(Default)]
struct CodeComparison {
    /// Planned node names that already exist in code (possible duplicate work).
    existing_planned_nodes: Vec<(String, Vec<String>)>,
    /// Planned dependencies already present in code.
    edges_already_present: Vec<(String, String)>,
    /// Planned dependencies not implemented yet.
    missing_code_edges: Vec<(String, String)>,
    /// Code dependencies between planned nodes that the plan does not record.
    unplanned_code_edges: Vec<(String, String)>,
}

/// Map stored edge rows onto the plan's `from`/`to`/`kind` triple.
///
/// The `kind` is the stored `edge_type`, so `plan show` round-trips exactly what
/// `plan store` wrote (including an explicit `"type"` from the plan file). An edge
/// whose endpoint is not a stored plan node is skipped, matching the SurrealQL this
/// replaced.
fn plan_edges_from_rows(
    edges: &[EdgeEndpoint],
    name_by_record: &HashMap<String, String>,
) -> Vec<PlanEdge> {
    let mut plan_edges = Vec::with_capacity(edges.len());
    for edge in edges {
        let (Some(from), Some(to)) = (name_by_record.get(&edge.from), name_by_record.get(&edge.to))
        else {
            continue;
        };
        plan_edges.push(PlanEdge {
            from: from.clone(),
            to: to.clone(),
            kind: edge.edge_type.clone(),
        });
    }
    plan_edges
}

/// Project `(id, name, file_path)` rows plus `(from_id, to_id)` endpoints into the
/// comparison view `plan verify` uses.
///
/// Kept pure so the mapping is testable without a live store. Its contract mirrors
/// the SurrealQL this replaced:
///
/// - a node with an empty name is skipped and does not count towards `nodes`;
/// - same-name nodes append their file, so one name can list several files;
/// - an edge whose endpoint is not a known node id is skipped, but every edge is
///   still counted in `edge_count`.
fn index_from_rows(nodes: &[NodeIdentityRow], edges: &[EdgeEndpoint]) -> CodeIndex {
    let mut id_to_name: HashMap<String, String> = HashMap::new();
    let mut index = CodeIndex::default();
    for row in nodes {
        if row.name.is_empty() {
            continue;
        }
        id_to_name.insert(row.id.clone(), row.name.clone());
        index
            .names
            .entry(row.name.clone())
            .or_default()
            .push(row.file_path.clone());
        index.nodes += 1;
    }

    let mut pairs = Vec::new();
    for edge in edges {
        index.edge_count += 1;
        let from = id_to_name.get(&edge.from);
        let to = id_to_name.get(&edge.to);
        if let (Some(from), Some(to)) = (from, to) {
            pairs.push((from.clone(), to.clone()));
        }
    }
    index.edges.extend(pairs);
    index
}

async fn load_code_index(storage: &Arc<dyn IndexStorage>, project: &str) -> Result<CodeIndex> {
    let nodes = storage
        .node_identities(project)
        .await
        .context("read code nodes")?;
    let edges = storage
        .edge_endpoints(project)
        .await
        .context("read code edges")?;
    Ok(index_from_rows(&nodes, &edges))
}

fn compare_with_code(doc: &PlanDoc, code: &CodeIndex) -> CodeComparison {
    let mut comparison = CodeComparison::default();
    let planned: HashSet<&str> = doc.nodes.iter().map(|node| node.id.as_str()).collect();

    for node in &doc.nodes {
        if let Some(files) = code.names.get(&node.id) {
            let mut files = files.clone();
            files.sort();
            files.dedup();
            comparison
                .existing_planned_nodes
                .push((node.id.clone(), files));
        }
    }

    let planned_edges: HashSet<(String, String)> = doc
        .edges
        .iter()
        .map(|edge| (edge.from.clone(), edge.to.clone()))
        .collect();
    for edge in &doc.edges {
        let pair = (edge.from.clone(), edge.to.clone());
        if code.edges.contains(&pair) {
            comparison.edges_already_present.push(pair);
        } else {
            comparison.missing_code_edges.push(pair);
        }
    }

    for (from, to) in &code.edges {
        if planned.contains(from.as_str())
            && planned.contains(to.as_str())
            && !planned_edges.contains(&(from.clone(), to.clone()))
        {
            comparison
                .unplanned_code_edges
                .push((from.clone(), to.clone()));
        }
    }

    comparison
}

fn print_verify_human(
    source: &str,
    project_id: &str,
    code: &CodeIndex,
    report: &PlanReport,
    comparison: &CodeComparison,
) {
    println!("Plan verify: {source}");
    println!(
        "Code graph: project_id={project_id} nodes={} edges={}",
        code.nodes, code.edge_count
    );
    for check in &report.checks {
        let marker = if check.ok { "PASS" } else { "FAIL" };
        match &check.detail {
            Some(detail) => println!("  [{marker}] {}: {detail}", check.rule),
            None => println!("  [{marker}] {}", check.rule),
        }
    }
    for (node, files) in &comparison.existing_planned_nodes {
        println!(
            "  [WARN] planned node '{node}' already exists in code: {}",
            files.join(", ")
        );
    }
    for (from, to) in &comparison.edges_already_present {
        println!("  [INFO] dependency '{from}' -> '{to}' already exists in code");
    }
    for (from, to) in &comparison.unplanned_code_edges {
        println!("  [WARN] code depends '{from}' -> '{to}' but the plan does not record it");
    }
    for (from, to) in &comparison.missing_code_edges {
        println!("  [INFO] planned dependency '{from}' -> '{to}' is not in the code yet");
    }
    if comparison.existing_planned_nodes.is_empty() && code.nodes > 0 {
        println!(
            "  [NOTE] no planned node name matched the code graph; if these symbols already \
             exist, re-run `codegraph index` so the index is current"
        );
    }
    println!(
        "Plan: {} nodes, {} edges | code: {} nodes, {} edges",
        report.nodes, report.edges, code.nodes, code.edge_count
    );
}

async fn verify(file: &str, project: Option<PathBuf>, format: PlanFormat) -> Result<()> {
    let (doc, source) = load(file)?;
    let report = analyze(&doc);

    let root = resolve_project_root(project)?;
    let project_id = std::env::var("CODEGRAPH_PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| root.display().to_string());

    let storage = connect_storage_at(&root, StorageMode::ReadOnly).await?;
    let code = load_code_index(&storage, &project_id).await?;
    let comparison = compare_with_code(&doc, &code);

    match format {
        PlanFormat::Json => {
            let output = json!({
                "ok": report.ok,
                "plan": {
                    "nodes": doc.nodes.len(),
                    "edges": doc.edges.len(),
                    "constraints": doc.constraints.len(),
                },
                "code": {
                    "project_id": project_id,
                    "nodes": code.nodes,
                    "edges": code.edge_count,
                },
                "checks": &report.checks,
                "violations": &report.violations,
                "warnings": &report.warnings,
                "existing_planned_nodes": comparison.existing_planned_nodes,
                "edges_already_present": comparison.edges_already_present,
                "unplanned_code_edges": comparison.unplanned_code_edges,
                "missing_code_edges": comparison.missing_code_edges,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        PlanFormat::Human => {
            print_verify_human(&source, &project_id, &code, &report, &comparison);
        }
    }

    if !report.ok {
        bail!(
            "plan verify reported {} plan violation(s); see the output above",
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

/// Deserialize a JSON plan object and evaluate it. Shared by the CLI and the
/// MCP `plan_check` tool so both report identically.
pub fn analyze_value(plan: Value) -> Result<PlanReport> {
    let doc: PlanDoc = serde_json::from_value(plan).context("parse plan object")?;
    Ok(analyze(&doc))
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
                    if let Some(layer) = &node.layer
                        && !index.contains_key(layer.as_str())
                    {
                        warnings.push(format!(
                            "node '{}' has layer '{}' not listed in layering order",
                            node.id, layer
                        ));
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

    fn code_index(nodes: &[(&str, &str)], edges: &[(&str, &str)]) -> CodeIndex {
        let mut index = CodeIndex::default();
        for (name, file) in nodes {
            index
                .names
                .entry(name.to_string())
                .or_default()
                .push(file.to_string());
            index.nodes += 1;
        }
        for (from, to) in edges {
            index.edges.insert((from.to_string(), to.to_string()));
            index.edge_count += 1;
        }
        index
    }

    #[test]
    fn verify_flags_duplicates_missing_and_unplanned_edges() {
        let doc = PlanDoc {
            nodes: vec![node("existing"), node("new"), node("other")],
            edges: vec![edge("new", "existing"), edge("other", "new")],
            constraints: Vec::new(),
        };
        let code = code_index(&[("existing", "src/a.rs")], &[("other", "existing")]);

        let comparison = compare_with_code(&doc, &code);
        assert_eq!(comparison.existing_planned_nodes.len(), 1);
        assert!(comparison.edges_already_present.is_empty());
        assert_eq!(
            comparison.missing_code_edges,
            vec![
                ("new".to_string(), "existing".to_string()),
                ("other".to_string(), "new".to_string()),
            ]
        );
        assert_eq!(
            comparison.unplanned_code_edges,
            vec![("other".to_string(), "existing".to_string())],
            "a code dependency between planned nodes must be reported when the plan omits it"
        );
    }

    #[test]
    fn verify_recognises_dependencies_that_already_exist() {
        let doc = PlanDoc {
            nodes: vec![node("a"), node("b")],
            edges: vec![edge("a", "b")],
            constraints: Vec::new(),
        };
        let code = code_index(&[], &[("a", "b")]);

        let comparison = compare_with_code(&doc, &code);
        assert_eq!(
            comparison.edges_already_present,
            vec![("a".to_string(), "b".to_string())]
        );
        assert!(comparison.missing_code_edges.is_empty());
        assert!(
            comparison.unplanned_code_edges.is_empty(),
            "a planned edge is not an unplanned one"
        );
    }

    fn identity(id: &str, name: &str, file: &str) -> NodeIdentityRow {
        NodeIdentityRow {
            id: id.to_string(),
            name: name.to_string(),
            file_path: file.to_string(),
            metadata: None,
        }
    }

    fn endpoint(from: &str, to: &str, edge_type: &str) -> EdgeEndpoint {
        EdgeEndpoint {
            from: from.to_string(),
            to: to.to_string(),
            edge_type: edge_type.to_string(),
        }
    }

    #[test]
    fn load_code_index_shape_matches_the_surreal_contract() {
        let nodes = vec![
            identity("n1", "shared", "src/a.rs"),
            identity("n2", "shared", "src/b.rs"),
            identity("n3", "", "src/empty.rs"),
            identity("n4", "only", "src/c.rs"),
        ];
        let edges = vec![
            endpoint("n1", "n2", "calls"),
            endpoint("n1", "ghost", "calls"),
            endpoint("n4", "n1", "calls"),
        ];

        let index = index_from_rows(&nodes, &edges);

        assert_eq!(
            index.nodes, 3,
            "a node without a name is skipped and does not count"
        );
        assert_eq!(
            index.names.get("shared"),
            Some(&vec!["src/a.rs".to_string(), "src/b.rs".to_string()]),
            "one name in two files keeps both paths"
        );
        assert_eq!(
            index.names.get(""),
            None,
            "an empty name never enters the index"
        );
        assert_eq!(
            index.edge_count, 3,
            "every edge counts, including one with an unknown endpoint"
        );
        assert_eq!(
            index.edges.len(),
            2,
            "an edge whose endpoint is unknown is dropped"
        );
        assert!(
            index
                .edges
                .contains(&("shared".to_string(), "shared".to_string()))
        );
        assert!(
            index
                .edges
                .contains(&("only".to_string(), "shared".to_string()))
        );
    }

    #[test]
    fn plan_show_round_trips_edge_kind() {
        let rows = vec![
            identity("n1", "entry", "plan.yaml"),
            identity("n2", "store", "plan.yaml"),
            identity("n3", "orphan", "plan.yaml"),
        ];
        let mut name_by_record = HashMap::new();
        for row in &rows {
            name_by_record.insert(row.id.clone(), row.name.clone());
        }

        let endpoints = vec![
            endpoint("n1", "n2", "depends"),
            endpoint("n1", "missing", "depends"),
            endpoint("n3", "n1", "calls"),
        ];
        let edges = plan_edges_from_rows(&endpoints, &name_by_record);
        assert_eq!(
            edges.len(),
            2,
            "an edge with an unknown endpoint never reaches the plan"
        );

        let doc = PlanDoc {
            nodes: vec![node("entry"), node("store"), node("orphan")],
            edges,
            constraints: Vec::new(),
        };
        let json = serde_json::to_value(&doc).expect("serialize plan");
        assert_eq!(json["edges"][0]["from"], "entry");
        assert_eq!(json["edges"][0]["to"], "store");
        assert_eq!(
            json["edges"][0]["type"], "depends",
            "the stored kind survives `plan show`, including the default `depends`"
        );
        assert_eq!(
            json["edges"][1]["type"], "calls",
            "an explicit kind is preserved verbatim"
        );
    }
}
