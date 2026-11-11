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
    // A self-loop edge must count in both directions, like the stored procedure's
    // independent GROUP BY `to` / GROUP BY `from` counts.
    db.query(
        "CREATE type::record('nodes', 'm') SET name = 'self_loop', node_type = 'Function', \
         language = 'rust', file_path = 'src/loop.rs', start_line = 1, end_line = 2, \
         project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
    db.query(
        "UPSERT type::record('edges', 'e7') SET `from` = type::record('nodes', 'm'), \
         `to` = type::record('nodes', 'm'), edge_type = 'calls', project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
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

    // Fixture degrees: a 2/1, b 1/2, c 1/1, d 1/1; m's self-loop makes it 1/1.
    let hubs = functions.get_hub_nodes_native(3).await.unwrap();
    assert_eq!(
        key(&hubs),
        vec![
            ("nodes:a".to_string(), 2, 1, 3),
            ("nodes:b".to_string(), 1, 2, 3),
        ]
    );

    let with_loop = functions.get_hub_nodes_native(2).await.unwrap();
    let expected_with_loop = functions.get_hub_nodes(2).await.unwrap();
    assert_eq!(
        key(&with_loop),
        key(&expected_with_loop),
        "hub ranking diverged with a self-loop"
    );
    let m = with_loop
        .iter()
        .find(|h| h.node_id == "nodes:m")
        .expect("self-loop node must rank at min_degree 2");
    assert_eq!(
        (m.afferent_degree, m.efferent_degree, m.total_degree),
        (1, 1, 2)
    );
    assert_eq!(m.incoming_by_type.len(), 1, "{:?}", m.incoming_by_type);
    assert_eq!(m.incoming_by_type[0].edge_type, "calls");
    assert_eq!(m.incoming_by_type[0].count, 1);
    assert_eq!(m.outgoing_by_type.len(), 1, "{:?}", m.outgoing_by_type);
    assert_eq!(m.outgoing_by_type[0].count, 1);

    let surreal_m = expected_with_loop
        .iter()
        .find(|h| h.node_id == "nodes:m")
        .expect("surrealql ranks the self-loop node too");
    assert_eq!(
        (surreal_m.afferent_degree, surreal_m.efferent_degree),
        (1, 1)
    );
    assert_eq!(surreal_m.incoming_by_type[0].count, 1);
    assert_eq!(surreal_m.outgoing_by_type[0].count, 1);
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
    // Self-loop on m: `distinct(from)` and `distinct(to)` both contain m, so m becomes its
    // own dependent and dependency (1/1), just like the stored procedure.
    db.query(
        "CREATE type::record('nodes', 'm') SET name = 'self_loop', node_type = 'Function', \
         language = 'rust', file_path = 'src/loop.rs', start_line = 1, end_line = 2, \
         project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
    db.query(
        "UPSERT type::record('edges', 'e7') SET `from` = type::record('nodes', 'm'), \
         `to` = type::record('nodes', 'm'), edge_type = 'calls', project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
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

    // Self-loop: 1/1 on both paths, instability 0.5, and m appears in both neighbour sets.
    let expected_m = functions
        .calculate_coupling_metrics("m")
        .await
        .expect("surrealql path");
    let native_m = functions
        .calculate_coupling_metrics_native("m")
        .await
        .unwrap()
        .expect("native path");
    assert_eq!(native_m.metrics.afferent_coupling, 1);
    assert_eq!(native_m.metrics.efferent_coupling, 1);
    assert_eq!(
        native_m.metrics.afferent_coupling,
        expected_m.metrics.afferent_coupling
    );
    assert_eq!(
        native_m.metrics.efferent_coupling,
        expected_m.metrics.efferent_coupling
    );
    assert!((native_m.metrics.instability - 0.5).abs() < 1e-9);
    assert!(
        (native_m.metrics.instability - expected_m.metrics.instability).abs() < 1e-9,
        "{} vs {}",
        native_m.metrics.instability,
        expected_m.metrics.instability
    );
    assert_eq!(ids(&native_m.dependents), vec!["nodes:m".to_string()]);
    assert_eq!(ids(&native_m.dependencies), vec!["nodes:m".to_string()]);
    assert_eq!(ids(&native_m.dependents), ids(&expected_m.dependents));
    assert_eq!(ids(&native_m.dependencies), ids(&expected_m.dependencies));
}

/// Differential test for `fn::expand` in the `out` direction: the Rust BFS must return the
/// same node ids, depths and hydrated fields as `fn::get_transitive_dependencies`.
#[tokio::test]
async fn native_transitive_dependencies_match_surrealdb_function() {
    use codegraph_graph::graph_functions::{DependencyNode, GraphFunctions};
    use std::sync::Arc;

    let db = db_with_fixture().await;
    db.query(
        "CREATE type::record('nodes', 'z') SET name = 'foreign_node', node_type = 'Function', \
         project_id = 'other-project'",
    )
    .await
    .unwrap()
    .check()
    .unwrap();
    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);

    let surreal_key = |rows: &Value| -> Vec<(String, i64, String, String)> {
        let mut v: Vec<(String, i64, String, String)> = rows
            .as_array()
            .expect("array result")
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["dependency_depth"].as_i64().unwrap_or_default(),
                    r["name"].as_str().unwrap_or_default().to_string(),
                    r["location"]["file_path"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let native_key = |rows: &[DependencyNode]| -> Vec<(String, i64, String, String)> {
        let mut v: Vec<(String, i64, String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.id.clone(),
                    r.dependency_depth.unwrap_or_default() as i64,
                    r.name.clone(),
                    r.location
                        .as_ref()
                        .map(|l| l.file_path.clone())
                        .unwrap_or_default(),
                )
            })
            .collect();
        v.sort();
        v
    };

    // a -> b -> c -> a and b -> d means: b@1, c@2, d@2. Depths above 2 add nothing.
    let calls_from_a = |depth: i64| -> Vec<(String, i64, String, String)> {
        if depth == 1 {
            vec![("nodes:b".into(), 1, "load_file".into(), "src/io.rs".into())]
        } else {
            vec![
                ("nodes:b".into(), 1, "load_file".into(), "src/io.rs".into()),
                ("nodes:c".into(), 2, "read".into(), "src/io.rs".into()),
                (
                    "nodes:d".into(),
                    2,
                    "ConfigLoader".into(),
                    "src/loader.rs".into(),
                ),
            ]
        }
    };

    for depth in [1_i32, 2, 3, 5, 0, 11] {
        let expected = call(
            &db,
            "RETURN fn::get_transitive_dependencies($p, $n, 'calls', $d)",
            vec![
                ("p", json!(PROJECT)),
                ("n", json!("nodes:a")),
                ("d", json!(depth)),
            ],
        )
        .await;
        let actual = functions
            .get_transitive_dependencies_native("nodes:a", "calls", depth)
            .await
            .expect("native path");

        // dep 0 and dep 11 are both out of range, so they must behave like the default of 3.
        let effective = if (1..=10).contains(&depth) {
            depth as i64
        } else {
            3
        };
        assert_eq!(
            native_key(&actual),
            calls_from_a(effective),
            "unexpected native rows at depth {depth}"
        );
        assert_eq!(
            native_key(&actual),
            surreal_key(&expected),
            "transitive dependencies diverged at depth {depth}"
        );
    }

    // Hydrated fields carry the same shape as fn::node_info.
    let rows = functions
        .get_transitive_dependencies_native("nodes:a", "calls", 3)
        .await
        .unwrap();
    assert_eq!(rows[0].language.as_deref(), Some("rust"));
    assert_eq!(
        rows[0].content.as_deref(),
        Some("fn load_file(path: &str) -> String { read(path) }")
    );
    assert!(rows.iter().all(|r| r.dependent_depth.is_none()));

    // edge_type filtering: d imports a, and d has no outgoing calls.
    let imports = functions
        .get_transitive_dependencies_native("nodes:d", "imports", 3)
        .await
        .unwrap();
    assert_eq!(
        native_key(&imports),
        vec![(
            "nodes:a".into(),
            1,
            "parse_config".into(),
            "src/config.rs".into()
        )]
    );
    let expected = call(
        &db,
        "RETURN fn::get_transitive_dependencies($p, $n, 'imports', 3)",
        vec![("p", json!(PROJECT)), ("n", json!("nodes:d"))],
    )
    .await;
    assert_eq!(native_key(&imports), surreal_key(&expected));

    let no_calls = functions
        .get_transitive_dependencies_native("nodes:d", "calls", 3)
        .await
        .unwrap();
    assert!(no_calls.is_empty(), "{no_calls:?}");

    // Unknown node, foreign project node, and empty id all yield [] like the stored procedure.
    for probe in ["nodes:does_not_exist", "does_not_exist", "nodes:z", "z"] {
        let actual = functions
            .get_transitive_dependencies_native(probe, "calls", 3)
            .await
            .unwrap();
        assert!(actual.is_empty(), "{probe} should expand to nothing");
        let expected = call(
            &db,
            "RETURN fn::get_transitive_dependencies($p, $n, 'calls', 3)",
            vec![("p", json!(PROJECT)), ("n", json!(probe))],
        )
        .await;
        assert!(surreal_key(&expected).is_empty(), "{probe} (surrealql)");
    }

    // fn::resolve_node accepts the bare and escaped id shapes too.
    for alternate in ["a", "nodes:`a`"] {
        let actual = functions
            .get_transitive_dependencies_native(alternate, "calls", 3)
            .await
            .unwrap();
        assert_eq!(
            native_key(&actual),
            calls_from_a(3),
            "id shape {alternate} must resolve to node a"
        );
    }

    // An empty edge name falls back to `calls`, matching `$edge_type ?? 'calls'` intent.
    let defaulted = functions
        .get_transitive_dependencies_native("nodes:a", "", 3)
        .await
        .unwrap();
    assert_eq!(native_key(&defaulted), calls_from_a(3));

    // An empty id has no node to resolve and must not query the datastore.
    assert!(
        functions
            .get_transitive_dependencies_native("", "calls", 3)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Differential test for `fn::expand` in the `in` direction: the reverse walk must match
/// `fn::get_reverse_dependencies` and report depth through `dependent_depth`.
#[tokio::test]
async fn native_reverse_dependencies_match_surrealdb_function() {
    use codegraph_graph::graph_functions::{DependencyNode, GraphFunctions};
    use std::sync::Arc;

    let db = db_with_fixture().await;
    db.query(
        "CREATE type::record('nodes', 'z') SET name = 'foreign_node', node_type = 'Function', \
         project_id = 'other-project'",
    )
    .await
    .unwrap()
    .check()
    .unwrap();
    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);

    let surreal_key = |rows: &Value| -> Vec<(String, i64, String)> {
        let mut v: Vec<(String, i64, String)> = rows
            .as_array()
            .expect("array result")
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["dependent_depth"].as_i64().unwrap_or_default(),
                    r["name"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let native_key = |rows: &[DependencyNode]| -> Vec<(String, i64, String)> {
        let mut v: Vec<(String, i64, String)> = rows
            .iter()
            .map(|r| {
                (
                    r.id.clone(),
                    r.dependent_depth.unwrap_or_default() as i64,
                    r.name.clone(),
                )
            })
            .collect();
        v.sort();
        v
    };

    // Reverse calls from c: b calls c (@1), a calls b (@2). The cycle stops there.
    let expected_c = vec![
        ("nodes:a".into(), 2, "parse_config".into()),
        ("nodes:b".into(), 1, "load_file".into()),
    ];
    // Reverse calls from d: b (@1), a (@2), c (@3).
    let expected_d = vec![
        ("nodes:a".into(), 2, "parse_config".into()),
        ("nodes:b".into(), 1, "load_file".into()),
        ("nodes:c".into(), 3, "read".into()),
    ];

    for (node, depth) in [("c", 1_i32), ("c", 2), ("c", 3), ("c", 0), ("c", 11)] {
        let expected = call(
            &db,
            "RETURN fn::get_reverse_dependencies($p, $n, 'calls', $d)",
            vec![
                ("p", json!(PROJECT)),
                ("n", json!(format!("nodes:{node}"))),
                ("d", json!(depth)),
            ],
        )
        .await;
        let actual = functions
            .get_reverse_dependencies_native(node, "calls", depth)
            .await
            .expect("native path");

        let want = if depth == 1 {
            vec![("nodes:b".into(), 1, "load_file".into())]
        } else {
            expected_c.clone()
        };
        assert_eq!(
            native_key(&actual),
            want,
            "reverse of {node} at depth {depth}"
        );
        assert_eq!(
            native_key(&actual),
            surreal_key(&expected),
            "reverse dependencies diverged for {node} at depth {depth}"
        );
    }

    for depth in [2_i32, 3, 10] {
        let expected = call(
            &db,
            "RETURN fn::get_reverse_dependencies($p, $n, 'calls', $d)",
            vec![
                ("p", json!(PROJECT)),
                ("n", json!("nodes:d")),
                ("d", json!(depth)),
            ],
        )
        .await;
        let actual = functions
            .get_reverse_dependencies_native("d", "calls", depth)
            .await
            .unwrap();
        let want = if depth == 2 {
            expected_d[..2].to_vec()
        } else {
            expected_d.clone()
        };
        assert_eq!(native_key(&actual), want, "reverse of d at depth {depth}");
        assert_eq!(native_key(&actual), surreal_key(&expected));
    }

    // Reverse of the cycle start only sees one level back before the visited set closes it.
    let from_a = functions
        .get_reverse_dependencies_native("a", "calls", 3)
        .await
        .unwrap();
    assert_eq!(
        native_key(&from_a),
        vec![
            ("nodes:b".into(), 2, "load_file".into()),
            ("nodes:c".into(), 1, "read".into()),
        ]
    );

    // The reverse path stores its level in dependent_depth only.
    let rows = functions
        .get_reverse_dependencies_native("c", "calls", 3)
        .await
        .unwrap();
    assert!(rows.iter().all(|r| r.dependency_depth.is_none()));
    assert!(rows.iter().all(|r| r.dependent_depth.is_some()));

    // Imports edge runs d -> a, so reverse-imports from a reaches d.
    let imports = functions
        .get_reverse_dependencies_native("a", "imports", 1)
        .await
        .unwrap();
    assert_eq!(
        native_key(&imports),
        vec![("nodes:d".into(), 1, "ConfigLoader".into())]
    );
    let expected = call(
        &db,
        "RETURN fn::get_reverse_dependencies($p, $n, 'imports', 1)",
        vec![("p", json!(PROJECT)), ("n", json!("nodes:a"))],
    )
    .await;
    assert_eq!(native_key(&imports), surreal_key(&expected));

    // Unknown / foreign start nodes return [].
    for probe in ["nodes:does_not_exist", "does_not_exist", "nodes:z", "z"] {
        let actual = functions
            .get_reverse_dependencies_native(probe, "calls", 3)
            .await
            .unwrap();
        assert!(actual.is_empty(), "{probe} should expand to nothing");
        let expected = call(
            &db,
            "RETURN fn::get_reverse_dependencies($p, $n, 'calls', 3)",
            vec![("p", json!(PROJECT)), ("n", json!(probe))],
        )
        .await;
        assert!(surreal_key(&expected).is_empty(), "{probe} (surrealql)");
    }

    assert!(
        functions
            .get_reverse_dependencies_native("", "calls", 3)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Differential test for `fn::trace_call_chain`: ids, depths and direct callers must match
/// the stored procedure. `called_by` ordering is not part of the contract, so both sides are
/// sorted before comparison; the result itself is sorted by id.
#[tokio::test]
async fn native_trace_call_chain_matches_surrealdb_function() {
    use codegraph_graph::graph_functions::{CallChainNode, GraphFunctions};
    use std::sync::Arc;

    let db = db_with_fixture().await;
    db.query(
        "CREATE type::record('nodes', 'z') SET name = 'foreign_node', node_type = 'Function', \
         project_id = 'other-project'",
    )
    .await
    .unwrap()
    .check()
    .unwrap();
    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);

    let surreal_key = |rows: &Value| -> Vec<(String, i64, Vec<String>)> {
        let mut v: Vec<(String, i64, Vec<String>)> = rows
            .as_array()
            .expect("array result")
            .iter()
            .map(|r| {
                let mut called_by: Vec<String> = r["called_by"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|c| c["id"].as_str().map(ToOwned::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                called_by.sort();
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["call_depth"].as_i64().unwrap_or_default(),
                    called_by,
                )
            })
            .collect();
        v.sort();
        v
    };
    let native_key = |rows: &[CallChainNode]| -> Vec<(String, i64, Vec<String>)> {
        let mut v: Vec<(String, i64, Vec<String>)> = rows
            .iter()
            .map(|r| {
                let mut called_by: Vec<String> = r
                    .called_by
                    .as_ref()
                    .map(|items| items.iter().map(|c| c.id.clone()).collect())
                    .unwrap_or_default();
                called_by.sort();
                (
                    r.id.clone(),
                    r.call_depth.unwrap_or_default() as i64,
                    called_by,
                )
            })
            .collect();
        v.sort();
        v
    };

    // a -> b -> c -> a and b -> d. Depths 1 and 2; nothing new after that.
    // A callee's direct callers are the `calls` edges pointing at it (d -> a is imports).
    let expected_from_a = |depth: i32| -> Vec<(String, i64, Vec<String>)> {
        let mut v = vec![("nodes:b".to_string(), 1, vec!["nodes:a".to_string()])];
        if depth != 1 {
            v.push(("nodes:c".to_string(), 2, vec!["nodes:b".to_string()]));
            v.push(("nodes:d".to_string(), 2, vec!["nodes:b".to_string()]));
        }
        v.sort();
        v
    };

    // 0 and 11 are out of range, so both fall back to the call chain default of 5.
    for start in ["nodes:a", "a"] {
        for depth in [1_i32, 2, 3, 0, 11] {
            let expected = call(
                &db,
                "RETURN fn::trace_call_chain($p, $n, $d)",
                vec![
                    ("p", json!(PROJECT)),
                    ("n", json!(start)),
                    ("d", json!(depth)),
                ],
            )
            .await;
            let actual = functions
                .trace_call_chain_native(start, depth)
                .await
                .expect("native path");

            let effective = if depth == 1 { 1 } else { 5 };
            assert_eq!(
                native_key(&actual),
                expected_from_a(effective),
                "unexpected native rows from {start} at depth {depth}"
            );
            assert_eq!(
                native_key(&actual),
                surreal_key(&expected),
                "call chain diverged from {start} at depth {depth}"
            );
        }
    }

    // Hydrated fields carry the same shape as fn::node_info.
    let rows = functions
        .trace_call_chain_native("nodes:a", 2)
        .await
        .unwrap();
    assert_eq!(rows[0].id, "nodes:b");
    assert_eq!(rows[0].language.as_deref(), Some("rust"));
    assert_eq!(
        rows[0].content.as_deref(),
        Some("fn load_file(path: &str) -> String { read(path) }")
    );
    assert!(rows.iter().all(|r| r.call_depth.is_some()));

    // A node with no outgoing calls traces to nothing; so does an unknown node.
    for probe in ["nodes:does_not_exist", "does_not_exist", "nodes:z", "z"] {
        let actual = functions.trace_call_chain_native(probe, 3).await.unwrap();
        assert!(actual.is_empty(), "{probe} should trace to nothing");
        let expected = call(
            &db,
            "RETURN fn::trace_call_chain($p, $n, 3)",
            vec![("p", json!(PROJECT)), ("n", json!(probe))],
        )
        .await;
        assert!(surreal_key(&expected).is_empty(), "{probe} (surrealql)");
    }
    assert!(
        functions
            .trace_call_chain_native("", 3)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        functions
            .trace_call_chain_native("nodes:d", 3)
            .await
            .unwrap()
            .is_empty()
    );

    // Twelve direct callers of d (plus b) exercise the ten-caller cap. `called_by` order is
    // not part of the contract, so only the count is asserted, not which ten are kept.
    for i in 0..12 {
        let node = format!("c{i:02}");
        let edge_id = format!("t{i:02}");
        db.query(
            "CREATE type::record('nodes', $id) SET name = $name, node_type = 'Function', \
             language = 'rust', file_path = 'src/caller.rs', start_line = 1, end_line = 2, \
             project_id = $project",
        )
        .bind(("id", node.clone()))
        .bind(("name", format!("caller_{i}")))
        .bind(("project", PROJECT.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
        db.query(
            "CREATE type::record('edges', $eid) SET `from` = type::record('nodes', $from), \
             `to` = type::record('nodes', 'd'), edge_type = 'calls', project_id = $project",
        )
        .bind(("eid", edge_id))
        .bind(("from", node))
        .bind(("project", PROJECT.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    let rows = functions
        .trace_call_chain_native("nodes:a", 2)
        .await
        .unwrap();
    let d = rows.iter().find(|r| r.id == "nodes:d").expect("d reached");
    let d_callers = d.called_by.as_ref().expect("called_by present");
    assert_eq!(d_callers.len(), 10, "called_by must be capped at ten");
    let unique: std::collections::HashSet<&str> = d_callers.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(unique.len(), d_callers.len(), "called_by must be distinct");

    // The stored procedure caps at ten as well.
    let expected = call(
        &db,
        "RETURN fn::trace_call_chain($p, $n, 2)",
        vec![("p", json!(PROJECT)), ("n", json!("nodes:a"))],
    )
    .await;
    let surreal_d = expected
        .as_array()
        .expect("array result")
        .iter()
        .find(|r| r["id"] == "nodes:d")
        .expect("d reached (surrealql)");
    assert_eq!(
        surreal_d["called_by"].as_array().map(Vec::len),
        Some(10),
        "surrealql called_by cap"
    );
}

/// Differential test for `fn::get_complexity_hotspots`: candidate selection, coupling
/// counts, instability and risk must match the stored procedure. Integer coupling columns
/// are compared exactly; the three float columns use a 1e-4 relative tolerance because the
/// schema computes them as f64 while `ComplexityHotspot` stores f32.
#[tokio::test]
async fn native_complexity_hotspots_match_surrealdb_function() {
    use codegraph_graph::graph_functions::{ComplexityHotspot, GraphFunctions};
    use std::sync::Arc;

    let db = db_with_fixture().await;
    let functions = GraphFunctions::new_with_project_id(Arc::new(db.clone()), PROJECT);

    let native_key = |rows: &[ComplexityHotspot]| -> Vec<(String, i32, i32)> {
        let mut v: Vec<(String, i32, i32)> = rows
            .iter()
            .map(|r| (r.id.clone(), r.afferent_coupling, r.efferent_coupling))
            .collect();
        v.sort();
        v
    };
    let surreal_key = |rows: &Value| -> Vec<(String, i32, i32)> {
        let mut v: Vec<(String, i32, i32)> = rows
            .as_array()
            .expect("array result")
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["afferent_coupling"].as_i64().unwrap_or_default() as i32,
                    r["efferent_coupling"].as_i64().unwrap_or_default() as i32,
                )
            })
            .collect();
        v.sort();
        v
    };

    // Relative tolerance, with |expected| clamped to 1 so an exact zero still passes.
    let close = |native: f32, expected: &Value| -> bool {
        let want = expected.as_f64().expect("float column");
        (native as f64 - want).abs() <= 1e-4 * want.abs().max(1.0)
    };

    for min_complexity in [0.0_f32, 5.0, 100.0, -1.0] {
        for limit in [1_i32, 5, 200] {
            let expected = call(
                &db,
                "RETURN fn::get_complexity_hotspots($p, $mc, $l)",
                vec![
                    ("p", json!(PROJECT)),
                    ("mc", json!(min_complexity)),
                    ("l", json!(limit)),
                ],
            )
            .await;
            let actual = functions
                .get_complexity_hotspots_native(min_complexity, limit)
                .await
                .expect("native path");

            let label = format!("min_complexity={min_complexity}, limit={limit}");
            assert_eq!(
                native_key(&actual),
                surreal_key(&expected),
                "coupling diverged at {label}"
            );

            let expected_rows = expected.as_array().expect("array result");
            for row in &actual {
                let want = expected_rows
                    .iter()
                    .find(|r| r["id"] == row.id)
                    .unwrap_or_else(|| panic!("{label}: {} missing (surrealql)", row.id));
                assert!(
                    close(row.complexity, &want["complexity"]),
                    "{label}: {} complexity {} vs {}",
                    row.id,
                    row.complexity,
                    want["complexity"]
                );
                assert!(
                    close(row.instability, &want["instability"]),
                    "{label}: {} instability {} vs {}",
                    row.id,
                    row.instability,
                    want["instability"]
                );
                assert!(
                    close(row.risk_score, &want["risk_score"]),
                    "{label}: {} risk_score {} vs {}",
                    row.id,
                    row.risk_score,
                    want["risk_score"]
                );
            }
        }
    }

    // Fixture: a=12 with c->a and d->a incoming (aff 2) and a->b outgoing (eff 1);
    // c=7 with b->c (aff 1) and c->a (eff 1). b=4 is below the default threshold of 5.
    let hotspots = functions
        .get_complexity_hotspots_native(0.0, 5)
        .await
        .unwrap();
    let summary: Vec<(&str, i32, i32, f32, f32, f32)> = hotspots
        .iter()
        .map(|h| {
            (
                h.id.as_str(),
                h.afferent_coupling,
                h.efferent_coupling,
                h.complexity,
                h.instability,
                h.risk_score,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("nodes:a", 2, 1, 12.0, 0.3333, 36.0),
            ("nodes:c", 1, 1, 7.0, 0.5, 14.0),
        ]
    );
    assert_eq!(
        hotspots[0].file_path.as_deref(),
        Some("src/config.rs"),
        "candidate rows carry file_path"
    );

    // limit 1 keeps only the top risk; limit 200 falls back to the safe cap of 20.
    let top = functions
        .get_complexity_hotspots_native(0.0, 1)
        .await
        .unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].id, "nodes:a");
    let capped = functions
        .get_complexity_hotspots_native(0.0, 200)
        .await
        .unwrap();
    assert_eq!(
        capped.len(),
        2,
        "safe limit 20 still bounds by candidate count"
    );

    // threshold 100 matches nothing.
    assert!(
        functions
            .get_complexity_hotspots_native(100.0, 5)
            .await
            .unwrap()
            .is_empty()
    );

    // A `Method` node with a self-loop: both paths count the loop once as afferent and once
    // as efferent, and the case-insensitive node_type match must include `Method`.
    db.query(
        "CREATE type::record('nodes', 'm') SET name = 'method_with_loop', node_type = 'Method', \
         language = 'rust', file_path = 'src/m.rs', start_line = 1, end_line = 2, \
         complexity = 20.0, project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();
    db.query(
        "UPSERT type::record('edges', 'e7') SET `from` = type::record('nodes', 'm'), \
         `to` = type::record('nodes', 'm'), edge_type = 'calls', project_id = $p",
    )
    .bind(("p", PROJECT.to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();

    let expected = call(
        &db,
        "RETURN fn::get_complexity_hotspots($p, 0.0, 5)",
        vec![("p", json!(PROJECT))],
    )
    .await;
    let actual = functions
        .get_complexity_hotspots_native(0.0, 5)
        .await
        .unwrap();
    assert_eq!(
        native_key(&actual),
        surreal_key(&expected),
        "self-loop coupling diverged"
    );

    let row_m = actual
        .iter()
        .find(|h| h.id == "nodes:m")
        .expect("native must match the Method node type");
    assert_eq!((row_m.afferent_coupling, row_m.efferent_coupling), (1, 1));
    assert_eq!(row_m.instability, 0.5);
    assert!(
        close(row_m.risk_score, &json!(40.0)),
        "20 * (1 + 1) = 40, got {}",
        row_m.risk_score
    );
    // 40 beats a's 36, so the self-loop node now ranks first on both paths.
    assert_eq!(actual[0].id, "nodes:m");
    assert_eq!(expected[0]["id"], "nodes:m");
}
