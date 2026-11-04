// ABOUTME: Verifies get_node_edge_summary reports node identity and present edge types.
// ABOUTME: Runs offline against the bundled v2 schema so empty dependency results can be explained.

#![cfg(feature = "surrealdb")]

use codegraph_core::{CodeNode, EdgeType, Language, Location, NodeType};
use codegraph_graph::{CodeEdge, GraphFunctions, SurrealDbConfig, SurrealDbStorage};

async fn storage() -> (SurrealDbStorage, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = SurrealDbConfig::embedded(dir.path());
    config.connection = "mem://".into();
    config.database = format!(
        "node_edge_summary_{}",
        dir.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('.', "_")
    );
    config.auto_migrate = false;
    let storage = SurrealDbStorage::new(config).await.unwrap();
    storage
        .db()
        .query(include_str!("../../../schema/codegraph_v2.surql").to_string())
        .await
        .unwrap()
        .check()
        .unwrap();
    (storage, dir)
}

fn node(project: &str, name: &str, node_type: NodeType) -> CodeNode {
    let mut node = CodeNode::new(
        name.to_string(),
        Some(node_type),
        Some(Language::Rust),
        Location {
            file_path: "src/lib.rs".into(),
            line: 7,
            column: 1,
            end_line: Some(9),
            end_column: None,
        },
    );
    node.metadata
        .attributes
        .insert("project_id".into(), project.into());
    node.set_deterministic_id(project);
    node
}

#[tokio::test]
async fn missing_node_is_reported_as_not_found() {
    let (store, _dir) = storage().await;
    let graph = GraphFunctions::new_with_project_id(store.db(), "proj-summary");

    let summary = graph
        .get_node_edge_summary("nodes:does-not-exist")
        .await
        .unwrap();

    assert!(!summary.found);
    assert!(summary.incoming_edge_types.is_empty());
    assert!(summary.outgoing_edge_types.is_empty());
}

#[tokio::test]
async fn existing_node_reports_kind_and_incoming_edge_types() {
    let (mut store, _dir) = storage().await;
    let project = "proj-summary";

    let callee = node(project, "callee", NodeType::Function);
    let caller = node(project, "caller", NodeType::Function);
    let container = node(project, "Widget", NodeType::Struct);

    let mut edge = CodeEdge::new(caller.id, callee.id, EdgeType::Calls);
    edge.set_deterministic_id(project);
    let mut contains = CodeEdge::new(container.id, callee.id, EdgeType::Contains);
    contains.set_deterministic_id(project);

    store
        .upsert_nodes_batch(&[callee.clone(), caller, container.clone()])
        .await
        .unwrap();
    store.upsert_edges_batch(&[edge, contains]).await.unwrap();

    let graph = GraphFunctions::new_with_project_id(store.db(), project);
    let summary = graph
        .get_node_edge_summary(&callee.id.to_string())
        .await
        .unwrap();

    assert!(summary.found);
    assert_eq!(summary.kind.as_deref(), Some("Function"));
    assert_eq!(summary.name.as_deref(), Some("callee"));
    assert!(
        summary.incoming_edge_types.iter().any(|t| t == "calls"),
        "incoming types: {:?}",
        summary.incoming_edge_types
    );
    assert!(
        summary.incoming_edge_types.iter().any(|t| t == "contains"),
        "incoming types: {:?}",
        summary.incoming_edge_types
    );
    assert!(summary.outgoing_edge_types.is_empty());

    let container_summary = graph
        .get_node_edge_summary(&container.id.to_string())
        .await
        .unwrap();
    assert!(container_summary.found);
    assert_eq!(container_summary.kind.as_deref(), Some("Struct"));
    assert!(
        container_summary
            .outgoing_edge_types
            .iter()
            .any(|t| t == "contains"),
        "outgoing types: {:?}",
        container_summary.outgoing_edge_types
    );
}

#[tokio::test]
async fn summary_is_scoped_to_the_project() {
    let (mut store, _dir) = storage().await;
    let mine = node("project-a", "shared_name", NodeType::Function);
    let theirs = node("project-b", "shared_name", NodeType::Function);

    store
        .upsert_nodes_batch(&[mine.clone(), theirs])
        .await
        .unwrap();

    let graph = GraphFunctions::new_with_project_id(store.db(), "project-a");
    let summary = graph
        .get_node_edge_summary(&mine.id.to_string())
        .await
        .unwrap();

    assert!(summary.found);
    assert_eq!(summary.name.as_deref(), Some("shared_name"));

    let other_graph = GraphFunctions::new_with_project_id(store.db(), "project-b");
    let other_summary = other_graph
        .get_node_edge_summary(&"nodes:missing".to_string())
        .await
        .unwrap();
    assert!(!other_summary.found);
}
