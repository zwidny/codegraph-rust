// ABOUTME: Opt-in real-data parity check between fn::get_hub_nodes (SurrealQL) and
// ABOUTME: GraphFunctions::get_hub_nodes_native. Read-only, skipped unless diff env vars are set.
#![cfg(feature = "surrealdb")]

use std::collections::{BTreeMap, BTreeSet};

use codegraph_graph::{GraphFunctions, HubNode, SurrealDbConfig, SurrealDbStorage};

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
