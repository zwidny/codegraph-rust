// ABOUTME: Applies schema/codegraph_v2.surql to an in-memory SurrealDB and exercises every fn::.
// ABOUTME: Guards the v2 schema against parse regressions and result-shape drift from v1.
#![cfg(feature = "surrealdb")]

use serde_json::{Value, json};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

const SCHEMA: &str = include_str!("../../../schema/codegraph_v2.surql");
const PROJECT: &str = "proj";

fn vec384(seed: f32) -> Vec<f32> {
    (0..384).map(|i| ((i as f32) * 0.01 + seed).sin()).collect()
}

async fn db_with_fixture() -> Surreal<Any> {
    let db: Surreal<Any> = Surreal::init();
    db.connect("mem://").await.unwrap();
    db.use_ns("v2").use_db("v2").await.unwrap();
    db.query(SCHEMA)
        .await
        .expect("v2 schema parses")
        .check()
        .expect("v2 schema applies");

    // a -> b -> c -> a (calls cycle), b -> d, e isolated; d imports a.
    let nodes = [
        (
            "a",
            "parse_config",
            "src/config.rs",
            "fn parse_config(path: &str) -> Config { load_file(path) }",
            12.0,
        ),
        (
            "b",
            "load_file",
            "src/io.rs",
            "fn load_file(path: &str) -> String { read(path) }",
            4.0,
        ),
        (
            "c",
            "read",
            "src/io.rs",
            "fn read(path: &str) -> String { parse_config(path); String::new() }",
            7.0,
        ),
        (
            "d",
            "ConfigLoader",
            "src/loader.rs",
            "struct ConfigLoader { cache: HashMap<String, Config> }",
            1.0,
        ),
        (
            "e",
            "unrelated_helper",
            "src/misc.rs",
            "fn unrelated_helper() {}",
            2.0,
        ),
    ];
    for (id, name, path, content, complexity) in nodes {
        db.query("CREATE type::record('nodes', $id) SET name = $name, node_type = 'Function', language = 'rust', file_path = $path, start_line = 1, end_line = 5, content = $content, complexity = $complexity, project_id = $project, embedding_384 = $emb")
            .bind(("id", id.to_string())).bind(("name", name.to_string())).bind(("path", path.to_string()))
            .bind(("content", content.to_string())).bind(("complexity", complexity)).bind(("project", PROJECT.to_string()))
            .bind(("emb", vec384(id.as_bytes()[0] as f32)))
            .await.unwrap().check().unwrap();
    }
    for (eid, from, to, kind) in [
        ("e1", "a", "b", "calls"),
        ("e2", "b", "c", "calls"),
        ("e3", "c", "a", "calls"),
        ("e4", "b", "d", "calls"),
        ("e5", "d", "a", "imports"),
    ] {
        db.query("UPSERT type::record('edges', $eid) SET `from` = type::record('nodes', $from), `to` = type::record('nodes', $to), edge_type = $kind, project_id = $project")
            .bind(("eid", eid.to_string())).bind(("from", from.to_string())).bind(("to", to.to_string()))
            .bind(("kind", kind.to_string())).bind(("project", PROJECT.to_string()))
            .await.unwrap().check().unwrap();
    }
    for (cid, parent, idx, text) in [
        ("c1", "a", 0, "parse config from a path"),
        ("c2", "a", 1, "return the Config struct"),
        ("c3", "b", 0, "load a file from disk"),
        ("c4", "e", 0, "nothing relevant here"),
    ] {
        db.query("CREATE type::record('chunks', $cid) SET parent_node = type::record('nodes', $parent), chunk_index = $idx, text = $text, project_id = $project, embedding_384 = $emb")
            .bind(("cid", cid.to_string())).bind(("parent", parent.to_string())).bind(("idx", idx))
            .bind(("text", text.to_string())).bind(("project", PROJECT.to_string()))
            .bind(("emb", vec384(parent.as_bytes()[0] as f32 + idx as f32 * 0.1)))
            .await.unwrap().check().unwrap();
    }
    db
}

/// Runs `RETURN <expr>` and returns the value. Wrapping in an object keeps array results
/// readable through a single `take`.
async fn call(db: &Surreal<Any>, query: &str, binds: Vec<(&'static str, Value)>) -> Value {
    let expr = query.strip_prefix("RETURN ").unwrap_or(query);
    let wrapped = format!("RETURN {{ r: ({expr}) }}");
    let mut q = db.query(&wrapped);
    for (k, v) in binds {
        q = q.bind((k, v));
    }
    let mut response = q.await.unwrap_or_else(|e| panic!("{query}: {e}"));
    let v: Option<Value> = response.take(0).unwrap_or_else(|e| panic!("{query}: {e}"));
    v.map(|mut o| o["r"].take()).unwrap_or(Value::Null)
}

fn ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn v2_graph_functions() {
    let db = db_with_fixture().await;
    let p = json!(PROJECT);

    let deps = call(
        &db,
        "RETURN fn::get_transitive_dependencies($p, $n, 'calls', 5)",
        vec![("p", p.clone()), ("n", json!("nodes:a"))],
    )
    .await;
    assert_eq!(ids(&deps), vec!["nodes:b", "nodes:c", "nodes:d"], "{deps}");
    assert_eq!(deps[0]["dependency_depth"], 1);
    assert_eq!(deps[1]["dependency_depth"], 2);
    assert_eq!(deps[0]["location"]["file_path"], "src/io.rs");

    let rdeps = call(
        &db,
        "RETURN fn::get_reverse_dependencies($p, $n, 'calls', 5)",
        vec![("p", p.clone()), ("n", json!("nodes:⟨c⟩"))],
    )
    .await;
    assert_eq!(ids(&rdeps), vec!["nodes:b", "nodes:a"], "{rdeps}");
    assert_eq!(rdeps[1]["dependent_depth"], 2);

    let chain = call(
        &db,
        "RETURN fn::trace_call_chain($p, $n, 5)",
        vec![("p", p.clone()), ("n", json!("nodes:a"))],
    )
    .await;
    assert_eq!(
        ids(&chain),
        vec!["nodes:b", "nodes:c", "nodes:d"],
        "{chain}"
    );
    assert_eq!(chain[1]["call_depth"], 2);
    assert_eq!(
        chain[1]["call_path"],
        json!(["parse_config", "load_file", "read"])
    );
    assert_eq!(chain[0]["called_by"][0]["name"], "parse_config", "{chain}");

    let cycles = call(
        &db,
        "RETURN fn::detect_circular_dependencies($p, 'Calls')",
        vec![("p", p.clone())],
    )
    .await;
    assert_eq!(
        cycles.as_array().unwrap().len(),
        0,
        "a->b->c->a is a 3-cycle, not a pair: {cycles}"
    );
    db.query("UPSERT edges:e6 SET `from` = nodes:b, `to` = nodes:a, edge_type = 'calls', project_id = $p").bind(("p", PROJECT.to_string())).await.unwrap().check().unwrap();
    let cycles = call(
        &db,
        "RETURN fn::detect_circular_dependencies($p, 'calls')",
        vec![("p", p.clone())],
    )
    .await;
    assert_eq!(cycles.as_array().unwrap().len(), 1, "{cycles}");
    assert_eq!(cycles[0]["node1"]["name"], "parse_config", "{cycles}");

    // Models sometimes append stray characters after the quoted id; only the quoted part counts.
    let garbled = call(
        &db,
        "RETURN fn::calculate_coupling_metrics($p, $n)",
        vec![("p", p.clone()), ("n", json!("nodes:`a`}},{"))],
    )
    .await;
    assert_eq!(garbled["node"]["name"], "parse_config", "{garbled}");

    let coupling = call(
        &db,
        "RETURN fn::calculate_coupling_metrics($p, $n)",
        vec![("p", p.clone()), ("n", json!("nodes:a"))],
    )
    .await;
    assert_eq!(coupling["metrics"]["afferent_coupling"], 3, "{coupling}"); // c calls, d imports, b calls (e6)
    assert_eq!(coupling["metrics"]["efferent_coupling"], 1);
    // 1 outgoing of 4 total. Integer division would report 0.0 here.
    assert_eq!(coupling["metrics"]["instability"], 0.25, "{coupling}");
    assert_eq!(coupling["metrics"]["coupling_category"], "stable");
    assert_eq!(coupling["node"]["name"], "parse_config");

    let hubs = call(
        &db,
        "RETURN fn::get_hub_nodes($p, 2)",
        vec![("p", p.clone())],
    )
    .await;
    let hub_names: Vec<&str> = hubs
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(hub_names[0], "parse_config", "{hubs}");
    assert_eq!(hubs[0]["total_degree"], 4);
    assert_eq!(hubs[0]["afferent_degree"], 3);
    assert!(
        hubs[0]["node"].get("content").is_none(),
        "hub rows carry no source text: {hubs}"
    );
    assert_eq!(hubs[0]["node"]["location"]["file_path"], "src/config.rs");
    assert_eq!(hubs[0]["efferent_degree"], 1);
    let none = call(
        &db,
        "RETURN fn::get_hub_nodes($p, 100)",
        vec![("p", p.clone())],
    )
    .await;
    assert_eq!(none.as_array().unwrap().len(), 0, "{none}");
    assert!(
        hubs[0]["incoming_by_type"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["edge_type"] == "imports")
    );

    let hotspots = call(
        &db,
        "RETURN fn::get_complexity_hotspots($p, 3.0, 10)",
        vec![("p", p.clone())],
    )
    .await;
    assert_eq!(hotspots[0]["name"], "parse_config", "{hotspots}");
    assert_eq!(hotspots[0]["afferent_coupling"], 3);
    assert_eq!(hotspots[0]["risk_score"], 48.0);
    assert_eq!(hotspots[0]["instability"], 0.25, "{hotspots}");
    assert!(
        hotspots
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["complexity"].as_f64().unwrap() >= 3.0)
    );

    let found = call(
        &db,
        "RETURN fn::find_nodes_by_name($p, 'LOAD', 5)",
        vec![("p", p.clone())],
    )
    .await;
    let names: Vec<&str> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["ConfigLoader", "load_file"], "{found}");

    let dirs = call(
        &db,
        "RETURN fn::get_top_directories($p, 5)",
        vec![("p", p.clone())],
    )
    .await;
    assert_eq!(dirs[0]["directory"], "src");
    let counts = call(
        &db,
        "RETURN fn::count_nodes_for_project($p)",
        vec![("p", p.clone())],
    )
    .await;
    assert_eq!(counts, json!({"nodes": 5, "edges": 6, "chunks": 4}));

    let ctx = call(&db, "RETURN fn::edge_context(nodes:b)", vec![]).await;
    assert_eq!(ctx["outgoing"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn v2_search_functions() {
    let db = db_with_fixture().await;
    let p = json!(PROJECT);
    let q = json!(vec384('a' as u8 as f32));

    let chunks = call(
        &db,
        "RETURN fn::knn_chunks($p, 384, $q)",
        vec![("p", p.clone()), ("q", q.clone())],
    )
    .await;
    assert_eq!(chunks[0]["id"], "chunks:c1", "{chunks}");

    let hybrid = call(
        &db,
        "RETURN fn::semantic_search_nodes_via_chunks($p, 'config', 384, 5, 0.0, $q)",
        vec![("p", p.clone()), ("q", q.clone())],
    )
    .await;
    let rows = hybrid.as_array().unwrap();
    assert!(!rows.is_empty(), "{hybrid}");
    assert_eq!(rows[0]["node_id"], "nodes:a", "{hybrid}");
    for key in [
        "name",
        "kind",
        "file_path",
        "start_line",
        "vector_score",
        "text_score",
        "combined_score",
        "match_sources",
        "outgoing_edges",
        "incoming_edges",
    ] {
        assert!(rows[0].get(key).is_some(), "missing {key}: {}", rows[0]);
    }
    assert!(rows[0]["combined_score"].as_f64().unwrap() > 0.0);
    assert_eq!(
        rows[0]["outgoing_edges"][0]["name"], "load_file",
        "{hybrid}"
    );
    assert_eq!(
        rows[0]["incoming_edges"].as_array().unwrap().len(),
        2,
        "{hybrid}"
    );
    assert!(
        rows[0]["match_sources"]
            .as_array()
            .unwrap()
            .contains(&json!("chunk"))
    );
    assert!(
        rows[0]["match_sources"]
            .as_array()
            .unwrap()
            .contains(&json!("text"))
    );
    assert!(
        rows.iter().any(|r| r["node_id"] == "nodes:d"),
        "ConfigLoader should match 'config' via camel tokenizer: {hybrid}"
    );

    let chunk_ctx = call(
        &db,
        "RETURN fn::semantic_search_chunks_with_context($p, $q, 'config', 384, 3, 0.0, true)",
        vec![("p", p.clone()), ("q", q.clone())],
    )
    .await;
    assert_eq!(chunk_ctx[0]["parent_name"], "parse_config", "{chunk_ctx}");
    assert_eq!(
        chunk_ctx[0]["outgoing_edges"][0]["name"], "load_file",
        "{chunk_ctx}"
    );
    let chunk_plain = call(
        &db,
        "RETURN fn::semantic_search_chunks_with_context($p, $q, 'config', 384, 3, 0.0, false)",
        vec![("p", p.clone()), ("q", q.clone())],
    )
    .await;
    assert!(chunk_plain[0].get("outgoing_edges").is_none());

    let snippets = call(
        &db,
        "RETURN fn::search_snippets($p, 'parse', 5)",
        vec![("p", p.clone())],
    )
    .await;
    assert!(
        snippets[0]["highlighted"].as_str().unwrap().contains("[["),
        "{snippets}"
    );

    let rows: Vec<Value> = (0..6).map(|i| json!({"id": format!("r{i}"), "vector": if i < 3 { vec![0.0 + i as f32 * 0.01, 0.0] } else { vec![10.0 + i as f32 * 0.01, 10.0] }})).collect();
    let clusters = call(
        &db,
        "RETURN fn::kmeans($rows, 2, 5)",
        vec![("rows", json!(rows))],
    )
    .await;
    let groups: Vec<Vec<String>> = clusters
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            c["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.as_str().unwrap().to_string())
                .collect()
        })
        .collect();
    assert_eq!(groups.len(), 2, "{clusters}");
    assert!(
        groups.iter().any(|g| g == &["r0", "r1", "r2"]),
        "{clusters}"
    );
}

#[tokio::test]
async fn v2_cascade_and_index_use() {
    let db = db_with_fixture().await;
    db.query("DELETE nodes:a").await.unwrap().check().unwrap();
    let left = call(
        &db,
        "RETURN { edges: (SELECT VALUE id FROM edges), chunks: (SELECT VALUE id FROM chunks) }",
        vec![],
    )
    .await;
    assert_eq!(left["chunks"], json!(["chunks:c3", "chunks:c4"]), "{left}");
    // Edges survive a node delete on purpose: incremental re-indexing recreates the node under
    // the same id and the edges from unchanged files must still point at it.
    assert_eq!(left["edges"].as_array().unwrap().len(), 5, "{left}");

    let plan = call(&db, "SELECT VALUE `to` FROM edges WHERE project_id = $p AND edge_type = 'calls' AND `from` INSIDE [nodes:b] EXPLAIN", vec![("p", json!(PROJECT))]).await;
    let plan_text = plan.to_string();
    assert!(
        plan_text.contains("idx_edges_project_type_from"),
        "expected compound edge index in plan: {plan_text}"
    );
}

/// Differential test for hub ranking: the Rust path must agree with `fn::get_hub_nodes`
/// on the fixture, and must be exact rather than capped at the top-500 candidates.
#[tokio::test]
async fn native_hub_nodes_match_surrealdb_function() {
    use codegraph_graph::graph_functions::{GraphFunctions, HubNode};
    use std::sync::Arc;

    let db = db_with_fixture().await;
    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);

    let key = |rows: &[HubNode]| {
        let mut v: Vec<(String, i32, i32, i32)> = rows
            .iter()
            .map(|h| {
                (
                    h.node_id.clone(),
                    h.afferent_degree,
                    h.efferent_degree,
                    h.total_degree,
                )
            })
            .collect();
        v.sort();
        v
    };

    for min_degree in [2, 3, 5, 100] {
        let expected = functions
            .get_hub_nodes(min_degree)
            .await
            .expect("surrealql path");
        let actual = functions
            .get_hub_nodes_native(min_degree)
            .await
            .expect("native path");
        assert_eq!(
            key(&expected),
            key(&actual),
            "hub ranking diverged at min_degree={min_degree}"
        );
    }

    // Fixture degrees (self-loops excluded): a 2/1, b 1/2, c 1/1, d 1/1.
    let hubs = functions.get_hub_nodes_native(3).await.unwrap();
    assert_eq!(
        key(&hubs),
        vec![
            ("nodes:a".to_string(), 2, 1, 3),
            ("nodes:b".to_string(), 1, 2, 3),
        ]
    );
}

/// Differential test for the `fn::*` -> Rust migration: the Rust cycle search must return
/// exactly the pairs the SurrealQL function returns, for the same fixture project.
#[tokio::test]
async fn native_cycle_detection_matches_surrealdb_function() {
    use codegraph_graph::graph_functions::{CircularDependency, GraphFunctions};
    use std::sync::Arc;

    let db = db_with_fixture().await;
    // The fixture's calls graph is a 3-cycle (a->b->c->a) and contains no mutual pair, which
    // fn::detect_circular_dependencies reports as empty. Add b->a so a real pair exists.
    db.query(
        "UPSERT type::record('edges', 'e6') SET `from` = type::record('nodes','b'), \
         `to` = type::record('nodes','a'), edge_type = 'calls', project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();

    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);
    let expected = functions
        .detect_circular_dependencies("calls")
        .await
        .expect("surrealql path");
    let actual = functions
        .detect_circular_dependencies_native("calls")
        .await
        .expect("native path");

    let pairs = |rows: &[CircularDependency]| {
        let mut v: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r.node1_id.clone(), r.node2_id.clone()))
            .collect();
        v.sort();
        v
    };

    assert_eq!(pairs(&expected), pairs(&actual));
    assert_eq!(
        actual.len(),
        1,
        "fixture must expose exactly one mutual pair"
    );
    assert_eq!(actual[0].dependency_type, "calls");
    assert_eq!(actual[0].node1.name, "parse_config");
    assert_eq!(actual[0].node2.name, "load_file");
}

/// Differential test for coupling metrics: same afferent/efferent counts, same instability
/// and the same distinct neighbour sets as `fn::calculate_coupling_metrics`.
#[tokio::test]
async fn native_coupling_metrics_match_surrealdb_function() {
    use codegraph_graph::graph_functions::GraphFunctions;
    use std::sync::Arc;

    let db = db_with_fixture().await;
    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);

    let ids = |rows: &[codegraph_graph::graph_functions::NodeReference]| {
        let mut v: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        v.sort();
        v
    };

    // Fixture: a<-c, a<-d, a->b, b->c, b->d, c->a; e is isolated.
    for (node, afferent, efferent) in [("a", 2, 1), ("b", 1, 2), ("c", 1, 1), ("d", 1, 1)] {
        let expected = functions
            .calculate_coupling_metrics(node)
            .await
            .expect("surrealql path");
        let actual = functions
            .calculate_coupling_metrics_native(node)
            .await
            .expect("native path")
            .expect("node must resolve");

        assert_eq!(actual.metrics.afferent_coupling, afferent, "{node}");
        assert_eq!(actual.metrics.efferent_coupling, efferent, "{node}");
        assert_eq!(actual.metrics.total_coupling, afferent + efferent, "{node}");
        assert_eq!(
            actual.metrics.afferent_coupling, expected.metrics.afferent_coupling,
            "{node}"
        );
        assert_eq!(
            actual.metrics.efferent_coupling, expected.metrics.efferent_coupling,
            "{node}"
        );
        assert_eq!(
            actual.metrics.total_coupling, expected.metrics.total_coupling,
            "{node}"
        );
        assert!(
            (actual.metrics.instability - expected.metrics.instability).abs() < 1e-9,
            "{node}: instability {} vs {}",
            actual.metrics.instability,
            expected.metrics.instability
        );
        assert_eq!(
            actual.metrics.coupling_category, expected.metrics.coupling_category,
            "{node}"
        );
        assert_eq!(
            ids(&actual.dependents),
            ids(&expected.dependents),
            "{node} dependents"
        );
        assert_eq!(
            ids(&actual.dependencies),
            ids(&expected.dependencies),
            "{node} dependencies"
        );
    }

    // Isolated node: no neighbours, instability 0, category "stable".
    let isolated = functions
        .calculate_coupling_metrics_native("e")
        .await
        .unwrap()
        .expect("isolated node resolves");
    assert_eq!(isolated.metrics.total_coupling, 0);
    assert_eq!(isolated.metrics.instability, 0.0);
    assert_eq!(isolated.metrics.coupling_category, "stable");

    // Prefix and bare-id forms resolve to the same node, like fn::resolve_node.
    let prefixed = functions
        .calculate_coupling_metrics_native("nodes:a")
        .await
        .unwrap()
        .expect("prefixed id resolves");
    assert_eq!(prefixed.node.name, "parse_config");
    assert!(
        functions
            .calculate_coupling_metrics_native("does_not_exist")
            .await
            .unwrap()
            .is_none()
    );
}
