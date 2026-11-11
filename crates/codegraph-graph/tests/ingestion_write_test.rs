// ABOUTME: Exercises repeatable bulk ingestion and project-scoped cleanup against real schemas.
// ABOUTME: Runs offline using embedded SurrealDB, without optional service skips.
#![cfg(feature = "surrealdb")]

use codegraph_core::{CodeNode, EdgeType, Language, Location, NodeType};
use codegraph_graph::{
    ChunkEmbeddingRecord, FileMetadataRecord, ReconcileScope, SurrealDbConfig, SurrealDbStorage,
    SymbolEmbeddingRecord, edge::CodeEdge,
};
use serde_json::Value;

async fn storage(schema: &str) -> SurrealDbStorage {
    let dir = tempfile::tempdir().unwrap();
    let mut config = SurrealDbConfig::embedded(dir.path());
    config.connection = "mem://".into();
    config.database = format!(
        "ingestion_{}",
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
        .query(schema.to_string())
        .await
        .unwrap()
        .check()
        .unwrap();
    storage
}

fn node(project: &str, name: &str) -> CodeNode {
    let mut node = CodeNode::new(
        name.to_string(),
        Some(NodeType::Function),
        Some(Language::Rust),
        Location {
            file_path: "shared.rs".into(),
            line: 1,
            column: 1,
            end_line: Some(1),
            end_column: None,
        },
    );
    node.metadata
        .attributes
        .insert("project_id".into(), project.into());
    node.set_deterministic_id(project);
    node
}

async fn count(storage: &SurrealDbStorage, table: &str, project: &str) -> usize {
    let mut response = storage
        .db()
        .query(format!(
            "SELECT count() AS n FROM {table} WHERE project_id = $project GROUP ALL"
        ))
        .bind(("project", project.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let rows: Vec<Value> = response.take(0).unwrap();
    rows.first().and_then(|row| row["n"].as_u64()).unwrap_or(0) as usize
}

#[tokio::test]
async fn bulk_ingestion_is_repeatable_and_cleanup_is_typed_and_scoped() {
    for schema in [
        include_str!("../../../schema/codegraph.surql"),
        include_str!("../../../schema/codegraph_v2.surql"),
    ] {
        let mut store = storage(schema).await;
        for project in ["one", "two"] {
            let nodes = [node(project, "caller"), node(project, "callee")];
            let mut edge = CodeEdge::new(nodes[0].id, nodes[1].id, EdgeType::Calls);
            edge.set_deterministic_id(project);
            let vector = vec![0.1; 384];
            let chunk = ChunkEmbeddingRecord::new(
                &nodes[0].id.to_string(),
                0,
                "fn caller() {}".into(),
                &vector,
                "fixture",
                "embedding_384",
                project,
            );
            let symbol = SymbolEmbeddingRecord::new(
                project,
                None,
                "caller",
                "caller",
                &vector,
                "fixture",
                "embedding_384",
                Some(&nodes[0].id.to_string()),
                None,
                None,
            );
            let file = FileMetadataRecord {
                file_path: "shared.rs".into(),
                project_id: project.into(),
                content_hash: "test".into(),
                modified_at: chrono::Utc::now(),
                file_size: 10,
                last_indexed_at: chrono::Utc::now(),
                node_count: 2,
                edge_count: 1,
                language: Some("rust".into()),
                parse_errors: None,
            };
            for _ in 0..2 {
                store.upsert_nodes_batch(&nodes).await.unwrap();
                store
                    .upsert_edges_batch(std::slice::from_ref(&edge))
                    .await
                    .unwrap();
                store
                    .upsert_chunk_embeddings_batch(std::slice::from_ref(&chunk))
                    .await
                    .unwrap();
                store
                    .upsert_symbol_embeddings_batch(std::slice::from_ref(&symbol))
                    .await
                    .unwrap();
                store
                    .upsert_file_metadata_batch(std::slice::from_ref(&file))
                    .await
                    .unwrap();
            }
            assert_eq!(count(&store, "nodes", project).await, 2);
            for table in ["edges", "chunks", "symbol_embeddings", "file_metadata"] {
                assert_eq!(count(&store, table, project).await, 1, "{table}");
            }
        }
        store
            .delete_data_for_files("one", &["shared.rs".into()])
            .await
            .unwrap();
        for table in [
            "nodes",
            "edges",
            "chunks",
            "symbol_embeddings",
            "file_metadata",
        ] {
            assert_eq!(count(&store, table, "one").await, 0, "{table}");
            assert!(count(&store, table, "two").await > 0, "{table}");
        }
    }
}

#[tokio::test]
async fn statement_errors_reach_ingestion_callers() {
    let mut store = storage(include_str!("../../../schema/codegraph_v2.surql")).await;
    let nodes = [node("test", "a"), node("test", "b")];
    store.upsert_nodes_batch(&nodes).await.unwrap();
    let mut edge = CodeEdge::new(nodes[0].id, nodes[1].id, EdgeType::Calls);
    edge.set_deterministic_id("test");
    store
        .db()
        .query("DEFINE FIELD OVERWRITE weight ON edges TYPE float ASSERT $value >= 0")
        .await
        .unwrap()
        .check()
        .unwrap();
    edge.weight = -1.0;
    assert!(store.upsert_edges_batch(&[edge]).await.is_err());
    let bad = SymbolEmbeddingRecord::new(
        "test",
        None,
        "bad",
        "bad",
        &[0.1; 3],
        "fixture",
        "embedding_384",
        None,
        None,
        None,
    );
    assert!(store.upsert_symbol_embeddings_batch(&[bad]).await.is_err());
}

/// A stalled SurrealKV flush previously parked an index run forever: the node and edge
/// upserts had no client-side deadline. This covers the happy path of the deadline
/// wrapper; the timeout branch itself is covered by the unit test in
/// `surrealdb_storage::write_deadline_tests`.
#[tokio::test]
async fn node_and_edge_upserts_accept_a_deadline() {
    let mut store = storage(include_str!("../../../schema/codegraph_v2.surql")).await;
    let nodes = [node("test", "a"), node("test", "b")];

    store
        .upsert_nodes_batch_with_timeout(&nodes, std::time::Duration::from_secs(30))
        .await
        .unwrap();

    let mut edge = CodeEdge::new(nodes[0].id, nodes[1].id, EdgeType::Calls);
    edge.set_deterministic_id("test");
    store
        .upsert_edges_batch_with_timeout(
            std::slice::from_ref(&edge),
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(count(&store, "nodes", "test").await, 2);
    assert_eq!(count(&store, "edges", "test").await, 1);
}

// ---------------------------------------------------------------------------
// Per-file catalog reconciliation (replaces the project-wide NOT IN variant)
// ---------------------------------------------------------------------------

mod per_file_reconciliation {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn node_in(project: &str, file: &str, name: &str, line: usize) -> CodeNode {
        let mut node = CodeNode::new(
            name.to_string(),
            Some(NodeType::Function),
            Some(Language::Rust),
            Location {
                file_path: file.into(),
                line: line as u32,
                column: 1,
                end_line: Some(line as u32),
                end_column: None,
            },
        );
        node.metadata
            .attributes
            .insert("project_id".into(), project.into());
        node.set_deterministic_id(project);
        node
    }

    fn file_meta(project: &str, file: &str) -> FileMetadataRecord {
        FileMetadataRecord {
            file_path: file.into(),
            project_id: project.into(),
            content_hash: "h".into(),
            modified_at: chrono::Utc::now(),
            file_size: 10,
            last_indexed_at: chrono::Utc::now(),
            node_count: 1,
            edge_count: 0,
            language: Some("rust".into()),
            parse_errors: None,
        }
    }

    async fn table_len(store: &SurrealDbStorage, table: &str, project: &str) -> usize {
        let mut r = store
            .db()
            .query(format!(
                "SELECT count() AS n FROM {table} WHERE project_id = $p GROUP ALL"
            ))
            .bind(("p", project.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let row: Option<Value> = r.take(0).unwrap();
        row.and_then(|v| v.get("n").and_then(|n| n.as_u64()))
            .unwrap_or(0) as usize
    }

    #[tokio::test]
    async fn removes_orphans_keeps_fresh_rows_and_drops_deleted_files() {
        let mut store = storage(include_str!("../../../schema/codegraph_v2.surql")).await;
        let project = "recon";

        // Initial state: two nodes in a.rs (lines 1 and 5), one in b.rs.
        let a1 = node_in(project, "a.rs", "alpha", 1);
        let a2 = node_in(project, "a.rs", "beta", 5);
        let b1 = node_in(project, "b.rs", "gamma", 1);
        store
            .upsert_nodes_batch(&[a1.clone(), a2.clone(), b1.clone()])
            .await
            .unwrap();
        store
            .upsert_file_metadata_batch(&[file_meta(project, "a.rs"), file_meta(project, "b.rs")])
            .await
            .unwrap();

        // 1. Nothing orphaned yet: both files fully represented.
        let mut fresh = BTreeMap::new();
        fresh.insert(
            "a.rs".to_string(),
            vec![a1.id.to_string(), a2.id.to_string()],
        );
        fresh.insert("b.rs".to_string(), vec![b1.id.to_string()]);
        store
            .reconcile_catalog_by_file(project, &fresh, &BTreeSet::new(), None)
            .await
            .unwrap();
        assert_eq!(table_len(&store, "nodes", project).await, 3, "no-op pass");

        // 2. a.rs only re-emits its first node (the second "moved" line => new id),
        //    and b.rs disappeared entirely.
        let mut shrunk = BTreeMap::new();
        shrunk.insert("a.rs".to_string(), vec![a1.id.to_string()]);
        store
            .reconcile_catalog_by_file(project, &shrunk, &BTreeSet::new(), None)
            .await
            .unwrap();

        assert_eq!(
            table_len(&store, "nodes", project).await,
            1,
            "stale node in a.rs and every node in b.rs must be removed"
        );
        assert_eq!(
            table_len(&store, "file_metadata", project).await,
            1,
            "deleted file metadata must be removed"
        );

        let mut r = store
            .db()
            .query("SELECT VALUE file_path FROM nodes WHERE project_id = $p;")
            .bind(("p", project.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let remaining: Vec<String> = r.take(0).unwrap();
        assert_eq!(
            remaining,
            vec!["a.rs".to_string()],
            "only the fresh node survives"
        );
    }

    #[tokio::test]
    async fn scope_leaves_files_outside_the_scanned_directory_untouched() {
        let mut store = storage(include_str!("../../../schema/codegraph_v2.surql")).await;
        let project = "recon_scope";

        let core = node_in(project, "packages/core/a.rs", "alpha", 1);
        let app = node_in(project, "packages/app/b.rs", "beta", 1);
        store
            .upsert_nodes_batch(&[core.clone(), app.clone()])
            .await
            .unwrap();

        // Only packages/core was scanned; packages/app is not part of this run.
        let mut fresh = BTreeMap::new();
        fresh.insert("packages/core/a.rs".to_string(), vec![core.id.to_string()]);
        let scope = ReconcileScope {
            project_root: std::path::PathBuf::from("/repo"),
            scan_root: std::path::PathBuf::from("/repo/packages/core"),
        };
        store
            .reconcile_catalog_by_file(project, &fresh, &BTreeSet::new(), Some(&scope))
            .await
            .unwrap();

        assert_eq!(
            table_len(&store, "nodes", project).await,
            2,
            "files outside the scan scope must survive a scoped reconciliation"
        );
    }

    #[tokio::test]
    async fn scope_still_removes_files_that_disappeared_within_it() {
        let mut store = storage(include_str!("../../../schema/codegraph_v2.surql")).await;
        let project = "recon_scope_delete";

        let core = node_in(project, "packages/core/a.rs", "alpha", 1);
        let app = node_in(project, "packages/app/b.rs", "beta", 1);
        store.upsert_nodes_batch(&[core, app]).await.unwrap();

        // packages/core no longer emits a.rs, so it must be removed; app stays.
        let scope = ReconcileScope {
            project_root: std::path::PathBuf::from("/repo"),
            scan_root: std::path::PathBuf::from("/repo/packages/core"),
        };
        store
            .reconcile_catalog_by_file(project, &BTreeMap::new(), &BTreeSet::new(), Some(&scope))
            .await
            .unwrap();

        assert_eq!(
            table_len(&store, "nodes", project).await,
            1,
            "a deleted file inside the scope is still reconciled"
        );
    }

    #[tokio::test]
    async fn stale_chunks_are_removed_while_fresh_ones_are_kept() {
        let mut store = storage(include_str!("../../../schema/codegraph_v2.surql")).await;
        let project = "recon_chunks";

        let n = node_in(project, "c.rs", "delta", 1);
        store
            .upsert_nodes_batch(std::slice::from_ref(&n))
            .await
            .unwrap();

        let vector: Vec<f32> = vec![0.1; 384];
        let keep = ChunkEmbeddingRecord::new(
            &n.id.to_string(),
            0,
            "fn delta() {}".into(),
            &vector,
            "fixture",
            "embedding_384",
            project,
        );
        let drop = ChunkEmbeddingRecord::new(
            &n.id.to_string(),
            1,
            "fn delta() {} // old".into(),
            &vector,
            "fixture",
            "embedding_384",
            project,
        );
        store
            .upsert_chunk_embeddings_batch(&[keep.clone(), drop.clone()])
            .await
            .unwrap();
        assert_eq!(table_len(&store, "chunks", project).await, 2);

        let mut fresh = BTreeMap::new();
        fresh.insert("c.rs".to_string(), vec![n.id.to_string()]);
        let mut fresh_chunks = BTreeSet::new();
        fresh_chunks.insert(keep.id.clone());

        store
            .reconcile_catalog_by_file(project, &fresh, &fresh_chunks, None)
            .await
            .unwrap();

        assert_eq!(
            table_len(&store, "chunks", project).await,
            1,
            "superseded chunk must be removed, fresh one kept"
        );
        assert_eq!(
            table_len(&store, "nodes", project).await,
            1,
            "node untouched"
        );
    }
}
