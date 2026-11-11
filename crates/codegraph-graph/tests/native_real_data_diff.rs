// ABOUTME: Opt-in real-data parity check between fn::get_hub_nodes (SurrealQL) and
// ABOUTME: GraphFunctions::get_hub_nodes_native. Read-only, skipped unless diff env vars are set.
#![cfg(feature = "surrealdb")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use codegraph_graph::graph_functions::NodeReference;
use codegraph_graph::{GraphFunctions, HubNode, SurrealDbConfig, SurrealDbStorage};

/// `CODEGRAPH_NATIVE_ANALYSIS` is process-global, so the two new diff tests take this lock
/// and touch the variable only inside themselves. The pre-existing hub test also toggles it
/// without the lock; to keep the SurrealQL path pinned there too, every SurrealQL call below
/// is immediately preceded by an explicit `remove_var`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// (node_id, afferent, efferent, total, incoming_by_type, outgoing_by_type)
type HubKey = (
    String,
    i32,
    i32,
    i32,
    Vec<(String, i32)>,
    Vec<(String, i32)>,
);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn key(row: &HubNode) -> HubKey {
    let mut incoming: Vec<(String, i32)> = row
        .incoming_by_type
        .iter()
        .map(|e| (e.edge_type.clone(), e.count))
        .collect();
    let mut outgoing: Vec<(String, i32)> = row
        .outgoing_by_type
        .iter()
        .map(|e| (e.edge_type.clone(), e.count))
        .collect();
    incoming.sort();
    outgoing.sort();
    (
        row.node_id.clone(),
        row.afferent_degree,
        row.efferent_degree,
        row.total_degree,
        incoming,
        outgoing,
    )
}

fn keyed(rows: &[HubNode]) -> BTreeMap<String, HubKey> {
    rows.iter()
        .map(|row| (row.node_id.clone(), key(row)))
        .collect()
}

#[tokio::test]
#[ignore = "requires a live SurrealDB server with real project data"]
async fn hub_nodes_match_on_real_project() {
    // Serialize with the other differential tests: CODEGRAPH_NATIVE_ANALYSIS is process-global,
    // so another test setting it mid-run would make this compare native against native.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(url), Some(namespace), Some(username), Some(password)) = (
        env("CODEGRAPH_SURREALDB_URL"),
        env("CODEGRAPH_SURREALDB_NAMESPACE"),
        env("CODEGRAPH_SURREALDB_USERNAME"),
        env("CODEGRAPH_SURREALDB_PASSWORD"),
    ) else {
        eprintln!(
            "skipping: set CODEGRAPH_SURREALDB_URL/NAMESPACE/USERNAME/PASSWORD to run this diff"
        );
        return;
    };
    let (Some(project_id), Some(database)) = (
        env("CODEGRAPH_DIFF_PROJECT"),
        env("CODEGRAPH_DIFF_DATABASE"),
    ) else {
        eprintln!(
            "skipping: set CODEGRAPH_DIFF_PROJECT and CODEGRAPH_DIFF_DATABASE to run this diff"
        );
        return;
    };

    let mut config = SurrealDbConfig::remote(url);
    config.namespace = namespace;
    config.database = database.clone();
    config.username = Some(username);
    config.password = Some(password);
    // Read-only diff: never apply schema or write anything to the live database.
    config.auto_migrate = false;
    config.cache_enabled = false;
    let storage = SurrealDbStorage::new(config).await.expect("connect");
    let db = storage.db();

    let mut counts = db
        .query(
            "RETURN { \
                 nodes: (SELECT count() AS c FROM nodes WHERE project_id = $p GROUP ALL)[0].c, \
                 edges: (SELECT count() AS c FROM edges WHERE project_id = $p GROUP ALL)[0].c \
             }",
        )
        .bind(("p", project_id.clone()))
        .await
        .expect("count query");
    let counts: Option<serde_json::Value> = counts.take(0).expect("decode counts");
    let counts = counts.unwrap_or_default();
    println!(
        "project={project_id} database={database} nodes={} edges={}",
        counts.get("nodes").and_then(|v| v.as_u64()).unwrap_or(0),
        counts.get("edges").and_then(|v| v.as_u64()).unwrap_or(0),
    );

    let graph = GraphFunctions::new_with_project_id(db, project_id.clone());

    // `CODEGRAPH_NATIVE_ANALYSIS` is process-global, so capture and clear it around the
    // SurrealQL call to guarantee that path really runs, then restore it afterwards.
    let previous = std::env::var("CODEGRAPH_NATIVE_ANALYSIS").ok();
    // SAFETY: this ignored test runs in its own process and no other thread reads this
    // variable; the edit is only there to pin the SurrealQL branch for the comparison.
    unsafe {
        std::env::remove_var("CODEGRAPH_NATIVE_ANALYSIS");
    }

    let mut saw_rows = false;
    let mut failures = 0usize;
    // 1..50 all return the same top-50 rows on EasyTier (the 50th hub has degree 75), so
    // the list also includes thresholds that actually filter: 200 -> 9, 500 -> 4, 1000 -> 3.
    // Without these the min_degree filter path would never be exercised on real data.
    for min_degree in [1, 2, 3, 5, 10, 50, 200, 500, 1000] {
        let sql = graph
            .get_hub_nodes(min_degree)
            .await
            .unwrap_or_else(|e| panic!("surrealql get_hub_nodes({min_degree}) failed: {e}"));
        let native = graph
            .get_hub_nodes_native(min_degree)
            .await
            .unwrap_or_else(|e| panic!("native get_hub_nodes_native({min_degree}) failed: {e}"));
        saw_rows |= !sql.is_empty() || !native.is_empty();

        let sql_map = keyed(&sql);
        let native_map = keyed(&native);
        let ids: BTreeSet<&String> = sql_map.keys().chain(native_map.keys()).collect();
        let mut diffs: Vec<String> = Vec::new();
        for id in ids {
            match (sql_map.get(id), native_map.get(id)) {
                (Some(a), Some(b)) if a == b => {}
                (a, b) => diffs.push(format!(
                    "  node {id}\n    surrealdb: {a:?}\n    native:    {b:?}"
                )),
            }
        }

        println!(
            "min_degree={min_degree}: surrealdb={} native={} mismatches={}",
            sql.len(),
            native.len(),
            diffs.len()
        );
        if diffs.is_empty() {
            continue;
        }
        failures += 1;
        for line in diffs.iter().take(5) {
            println!("{line}");
        }
    }

    if let Some(value) = previous {
        // SAFETY: same rationale as the removal above.
        unsafe {
            std::env::set_var("CODEGRAPH_NATIVE_ANALYSIS", value);
        }
    }

    assert!(
        saw_rows,
        "both implementations returned no hubs; the diff did not exercise real data"
    );
    assert_eq!(failures, 0, "{failures} min_degree values disagreed");
}

/// Open a read-only handle to the live diff database, or `None` when the diff env vars are
/// absent. Returns the project id alongside the graph because the sampling queries bind it.
async fn connect_readonly() -> Option<(GraphFunctions, String)> {
    let (Some(url), Some(namespace), Some(username), Some(password)) = (
        env("CODEGRAPH_SURREALDB_URL"),
        env("CODEGRAPH_SURREALDB_NAMESPACE"),
        env("CODEGRAPH_SURREALDB_USERNAME"),
        env("CODEGRAPH_SURREALDB_PASSWORD"),
    ) else {
        eprintln!(
            "skipping: set CODEGRAPH_SURREALDB_URL/NAMESPACE/USERNAME/PASSWORD to run this diff"
        );
        return None;
    };
    let (Some(project_id), Some(database)) = (
        env("CODEGRAPH_DIFF_PROJECT"),
        env("CODEGRAPH_DIFF_DATABASE"),
    ) else {
        eprintln!(
            "skipping: set CODEGRAPH_DIFF_PROJECT and CODEGRAPH_DIFF_DATABASE to run this diff"
        );
        return None;
    };

    let mut config = SurrealDbConfig::remote(url);
    config.namespace = namespace;
    config.database = database;
    config.username = Some(username);
    config.password = Some(password);
    // Read-only diff: never apply schema or write anything to the live database.
    config.auto_migrate = false;
    config.cache_enabled = false;
    let storage = SurrealDbStorage::new(config).await.expect("connect");
    Some((
        GraphFunctions::new_with_project_id(storage.db(), project_id.clone()),
        project_id,
    ))
}

fn pinned_surrealql_path() {
    // SAFETY: the caller holds `ENV_LOCK` and no other thread in the test binary relies on
    // this variable being set during the call; the whole point is to pin the SurrealQL branch.
    unsafe {
        std::env::remove_var("CODEGRAPH_NATIVE_ANALYSIS");
    }
}

fn restore_native_flag(previous: Option<String>) {
    // SAFETY: same rationale as `pinned_surrealql_path`.
    unsafe {
        match previous {
            Some(value) => std::env::set_var("CODEGRAPH_NATIVE_ANALYSIS", value),
            None => std::env::remove_var("CODEGRAPH_NATIVE_ANALYSIS"),
        }
    }
}

fn sorted_ref_ids(refs: &[NodeReference]) -> Vec<String> {
    let mut ids: Vec<String> = refs.iter().map(|r| r.id.clone()).collect();
    ids.sort();
    ids
}

fn json_string(row: &serde_json::Value, field: &str) -> Option<String> {
    row.get(field).and_then(|v| v.as_str()).map(str::to_string)
}

/// Differential test for `calculate_coupling_metrics` on real data. Samples self-loop
/// endpoints, high-degree hubs and ordinary nodes so the self-coupling case, the heavy
/// fan-in/fan-out case and a plain case are all compared.
#[tokio::test]
#[ignore = "requires a live SurrealDB server with real project data"]
async fn coupling_metrics_match_on_real_project() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((graph, project_id)) = connect_readonly().await else {
        return;
    };
    let db = graph.db();

    // Self-loop endpoints: `from = to`, where dependents and dependencies are the node itself.
    let loop_rows: Vec<serde_json::Value> = db
        .query(
            "SELECT <string>`from` AS id FROM edges \
             WHERE project_id = $p AND `from` = `to` LIMIT 3",
        )
        .bind(("p", project_id.clone()))
        .await
        .expect("self-loop sample query")
        .take(0)
        .expect("decode self-loop sample");

    // High-degree hubs, the case where afferent/efferent counts are large.
    let hub_rows: Vec<serde_json::Value> = db
        .query("RETURN fn::get_hub_nodes($p, 1000)")
        .bind(("p", project_id.clone()))
        .await
        .expect("hub sample query")
        .take(0)
        .expect("decode hub sample");

    // Ordinary nodes.
    let plain_rows: Vec<serde_json::Value> = db
        .query("SELECT <string>id AS id FROM nodes WHERE project_id = $p LIMIT 3")
        .bind(("p", project_id.clone()))
        .await
        .expect("plain node sample query")
        .take(0)
        .expect("decode plain node sample");

    let mut samples: Vec<(String, &'static str)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut add = |id: Option<String>, source: &'static str, samples: &mut Vec<_>| {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            if seen.insert(id.clone()) {
                samples.push((id, source));
            }
        }
    };
    for row in &loop_rows {
        add(json_string(row, "id"), "self-loop", &mut samples);
    }
    for row in hub_rows.iter().take(5) {
        add(json_string(row, "node_id"), "hub", &mut samples);
    }
    for row in &plain_rows {
        add(json_string(row, "id"), "plain", &mut samples);
    }
    samples.sort();

    println!(
        "coupling samples={} {:?}",
        samples.len(),
        samples.iter().map(|(id, s)| (id, s)).collect::<Vec<_>>()
    );

    let previous = std::env::var("CODEGRAPH_NATIVE_ANALYSIS").ok();
    let mut failures = 0usize;
    for (id, source) in &samples {
        pinned_surrealql_path();
        let sql = graph
            .calculate_coupling_metrics(id)
            .await
            .unwrap_or_else(|e| panic!("surrealql coupling({id}) failed: {e}"));
        let native = graph
            .calculate_coupling_metrics_native(id)
            .await
            .unwrap_or_else(|e| panic!("native coupling({id}) failed: {e}"));
        let Some(native) = native else {
            failures += 1;
            println!("{source} {id}: native returned None but SurrealQL returned a row");
            continue;
        };

        let mut diffs: Vec<String> = Vec::new();
        let a = (
            sql.metrics.afferent_coupling,
            sql.metrics.efferent_coupling,
            sql.metrics.total_coupling,
        );
        let b = (
            native.metrics.afferent_coupling,
            native.metrics.efferent_coupling,
            native.metrics.total_coupling,
        );
        if a != b {
            diffs.push(format!("coupling tuple surrealdb={a:?} native={b:?}"));
        }
        if (sql.metrics.instability - native.metrics.instability).abs() > 1e-6 {
            diffs.push(format!(
                "instability surrealdb={} native={}",
                sql.metrics.instability, native.metrics.instability
            ));
        }
        if sql.metrics.coupling_category != native.metrics.coupling_category {
            diffs.push(format!(
                "category surrealdb={:?} native={:?}",
                sql.metrics.coupling_category, native.metrics.coupling_category
            ));
        }
        let sql_dependents = sorted_ref_ids(&sql.dependents);
        let native_dependents = sorted_ref_ids(&native.dependents);
        if sql_dependents != native_dependents {
            diffs.push(format!(
                "dependents surrealdb={sql_dependents:?} native={native_dependents:?}"
            ));
        }
        let sql_dependencies = sorted_ref_ids(&sql.dependencies);
        let native_dependencies = sorted_ref_ids(&native.dependencies);
        if sql_dependencies != native_dependencies {
            diffs.push(format!(
                "dependencies surrealdb={sql_dependencies:?} native={native_dependencies:?}"
            ));
        }

        println!(
            "{source} {id}: afferent={} efferent={} total={} instability={} category={} \
             dependents={} dependencies={} match={}",
            sql.metrics.afferent_coupling,
            sql.metrics.efferent_coupling,
            sql.metrics.total_coupling,
            sql.metrics.instability,
            sql.metrics.coupling_category,
            sql_dependents.len(),
            sql_dependencies.len(),
            diffs.is_empty()
        );
        if !diffs.is_empty() {
            failures += 1;
            for line in diffs.iter().take(5) {
                println!("  {line}");
            }
        }
    }
    restore_native_flag(previous);

    assert!(
        !samples.is_empty(),
        "no coupling samples taken; the diff did not exercise real data"
    );
    assert_eq!(failures, 0, "{failures} sampled nodes disagreed");
}

/// Differential test for `detect_circular_dependencies` on real data, across every edge
/// type the callers use. Empty results are allowed, but both paths must agree.
#[tokio::test]
#[ignore = "requires a live SurrealDB server with real project data"]
async fn circular_dependencies_match_on_real_project() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((graph, project_id)) = connect_readonly().await else {
        return;
    };
    let db = graph.db();

    let edge_types = ["calls", "imports", "uses"];
    // Guard against a vacuous run: at least one compared edge type must have edges.
    let edge_counts: Vec<serde_json::Value> = db
        .query(
            "SELECT edge_type, count() AS c FROM edges \
             WHERE project_id = $p AND edge_type INSIDE $types GROUP BY edge_type",
        )
        .bind(("p", project_id.clone()))
        .bind((
            "types",
            edge_types.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
        ))
        .await
        .expect("edge count query")
        .take(0)
        .expect("decode edge counts");

    let mut total_edges = 0i64;
    for row in &edge_counts {
        let edge_type = json_string(row, "edge_type").unwrap_or_default();
        let count = row.get("c").and_then(|v| v.as_i64()).unwrap_or(0);
        total_edges += count;
        println!("edge_type={edge_type} edges={count}");
    }

    let previous = std::env::var("CODEGRAPH_NATIVE_ANALYSIS").ok();
    let mut failures = 0usize;
    for edge_type in edge_types {
        pinned_surrealql_path();
        let sql = graph
            .detect_circular_dependencies(edge_type)
            .await
            .unwrap_or_else(|e| panic!("surrealql cycles({edge_type}) failed: {e}"));
        let native = graph
            .detect_circular_dependencies_native(edge_type)
            .await
            .unwrap_or_else(|e| panic!("native cycles({edge_type}) failed: {e}"));

        let pairs = |rows: &[codegraph_graph::graph_functions::CircularDependency]| {
            let mut v: Vec<(String, String)> = rows
                .iter()
                .map(|r| (r.node1_id.clone(), r.node2_id.clone()))
                .collect();
            v.sort();
            v
        };
        // The contract under test is the pair *set*: the SurrealQL function emits one row per
        // matching edge record, so duplicate edges surface as repeated pairs, while the Rust
        // path reports each mutual pair once. Compare sorted de-duplicated sets and print the
        // raw multiplicities separately so that difference stays visible.
        let sql_raw = pairs(&sql);
        let native_raw = pairs(&native);
        let mut sql_pairs = sql_raw.clone();
        sql_pairs.dedup();
        let mut native_pairs = native_raw.clone();
        native_pairs.dedup();
        let matches = sql_pairs == native_pairs;
        println!(
            "edge_type={edge_type}: surrealdb_rows={} (unique={}) native_rows={} (unique={}) \
             match={matches}",
            sql_raw.len(),
            sql_pairs.len(),
            native_raw.len(),
            native_pairs.len()
        );
        if !matches {
            failures += 1;
            let sql_set: BTreeSet<&(String, String)> = sql_pairs.iter().collect();
            let native_set: BTreeSet<&(String, String)> = native_pairs.iter().collect();
            for pair in sql_set.difference(&native_set).take(5) {
                println!("  only surrealdb: {pair:?}");
            }
            for pair in native_set.difference(&sql_set).take(5) {
                println!("  only native:    {pair:?}");
            }
        }
    }
    restore_native_flag(previous);

    assert!(
        total_edges > 0,
        "no edges for calls/imports/uses; the cycle diff did not exercise real data"
    );
    assert_eq!(failures, 0, "{failures} edge types disagreed");
}
