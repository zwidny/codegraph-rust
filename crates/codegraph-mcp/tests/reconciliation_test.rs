// ABOUTME: Exercises complete-project reconciliation without network services or inference.
// ABOUTME: Checks cached callers, deletion, empty sources, docs invalidation and no-change runs.
#![cfg(not(feature = "embeddings"))]
#![cfg(feature = "surrealdb")]
use anyhow::Result;
use codegraph_core::config_manager::{CodeGraphConfig, IndexingTier};
use codegraph_mcp::indexer::{IndexerConfig, ProjectIndexer};
use indicatif::{MultiProgress, ProgressDrawTarget};
use serde_json::Value;

#[tokio::test]
async fn incremental_catalog_matches_full_graph_and_removes_stale_definitions() -> Result<()> {
    if !test_env::run(
        concat!(
            module_path!(),
            "::incremental_catalog_matches_full_graph_and_removes_stale_definitions"
        ),
        &[
            ("CODEGRAPH_SURREALDB_URL", Some("mem://")),
            ("CODEGRAPH_NO_PROGRESS", Some("1")),
            ("CODEGRAPH_ANALYZERS", Some("0")),
            ("CODEGRAPH_ANALYZERS_REQUIRE_TOOLS", Some("0")),
        ],
    ) {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let caller = dir.path().join("caller.rs");
    let target = dir.path().join("target.rs");
    std::fs::write(&caller, "fn caller() { target(); }\n")?;
    std::fs::write(&target, "fn target() {}\n")?;
    let config = IndexerConfig {
        project_root: dir.path().into(),
        indexing_tier: IndexingTier::Fast,
        ..Default::default()
    };
    let indexer = ProjectIndexer::new(
        config,
        &CodeGraphConfig::default(),
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden()),
    )
    .await?;
    indexer
        .surreal_storage()
        .await
        .lock()
        .await
        .db()
        .query(include_str!("../../../schema/codegraph_v2.surql"))
        .await?
        .check()?;
    let cold = indexer.index_project(dir.path()).await?;
    assert!(cold.complete);
    assert_eq!(cold.files, 2);
    let storage = indexer.surreal_storage().await;
    let mut response = storage.lock().await.db().query("SELECT file_path, last_indexed_at FROM file_metadata ORDER BY file_path; SELECT metadata FROM edges WHERE edge_type = 'calls'").await?.check()?;
    let before_metadata: Vec<Value> = response.take(0)?;
    let cold_edges: Vec<Value> = response.take(1)?;
    assert!(!cold_edges.is_empty());
    assert!(
        cold_edges
            .iter()
            .all(|edge| edge["metadata"]["resolution_method"] == "exact")
    );
    let warm = indexer.index_project(dir.path()).await?;
    assert_eq!(warm.nodes, cold.nodes);
    assert_eq!(warm.edges, cold.edges);
    assert_eq!(warm.skipped, 2);
    std::fs::write(&target, "fn replacement() {}\n")?;
    let edited = indexer.reconcile_project(dir.path(), false).await?;
    assert_eq!(edited.cached_files, 1);
    assert!(edited.writer_rows_acked < cold.writer_rows_acked);
    let mut response = storage
        .lock()
        .await
        .db()
        .query("SELECT file_path, last_indexed_at FROM file_metadata ORDER BY file_path")
        .await?
        .check()?;
    let after_metadata: Vec<Value> = response.take(0)?;
    let caller_metadata = |rows: &[Value]| {
        rows.iter()
            .find(|row| row["file_path"] == caller.to_string_lossy().as_ref())
            .unwrap()
            .clone()
    };
    assert_eq!(
        caller_metadata(&before_metadata),
        caller_metadata(&after_metadata)
    );
    let graph = |storage: std::sync::Arc<tokio::sync::Mutex<codegraph_graph::SurrealDbStorage>>| async move {
        let mut response = storage.lock().await.db().query("SELECT name, file_path FROM nodes ORDER BY file_path, name; SELECT from, to, edge_type, metadata FROM edges ORDER BY id").await?.check()?;
        Ok::<_, anyhow::Error>((
            response.take::<Vec<Value>>(0)?,
            response.take::<Vec<Value>>(1)?,
        ))
    };
    let incremental = graph(storage.clone()).await?;
    assert!(!incremental.0.iter().any(|row| row["name"] == "target"));
    indexer.reconcile_project(dir.path(), true).await?;
    assert_eq!(incremental, graph(storage.clone()).await?);
    std::fs::write(dir.path().join("README.md"), "new documentation")?;
    let docs = indexer.index_project(dir.path()).await?;
    assert_eq!(docs.cached_files, 2);
    assert_eq!(docs.skipped, 2);
    std::fs::remove_file(&target)?;
    indexer.delete_file_data(&target).await?;
    assert!(
        !graph(storage.clone())
            .await?
            .0
            .iter()
            .any(|row| row["name"] == "replacement")
    );
    std::fs::write(&caller, "")?;
    indexer.index_single_file(&caller).await?;
    assert!(
        !graph(storage)
            .await?
            .0
            .iter()
            .any(|row| row["name"] == "caller")
    );
    Ok(())
}

#[tokio::test]
async fn inbound_edges_follow_symbols_that_only_shifted_lines() -> Result<()> {
    if !test_env::run(
        concat!(
            module_path!(),
            "::inbound_edges_follow_symbols_that_only_shifted_lines"
        ),
        &[
            ("CODEGRAPH_SURREALDB_URL", Some("mem://")),
            ("CODEGRAPH_NO_PROGRESS", Some("1")),
            ("CODEGRAPH_ANALYZERS", Some("0")),
            ("CODEGRAPH_ANALYZERS_REQUIRE_TOOLS", Some("0")),
        ],
    ) {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let caller = dir.path().join("caller.rs");
    let target = dir.path().join("target.rs");
    std::fs::write(&caller, "fn caller() { target(); }\n")?;
    std::fs::write(&target, "fn target() {}\n")?;

    let config = IndexerConfig {
        project_root: dir.path().into(),
        indexing_tier: IndexingTier::Fast,
        ..Default::default()
    };
    let indexer = ProjectIndexer::new(
        config,
        &CodeGraphConfig::default(),
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden()),
    )
    .await?;
    indexer
        .surreal_storage()
        .await
        .lock()
        .await
        .db()
        .query(include_str!("../../../schema/codegraph_v2.surql"))
        .await?
        .check()?;
    indexer.index_project(dir.path()).await?;

    // Only the position changes: `target` moves from line 1 to line 3, so its node id
    // changes even though the symbol is unchanged. The caller edge must follow it.
    std::fs::write(&target, "\n\nfn target() {}\n")?;
    indexer.index_single_file(&target).await?;

    let storage = indexer.surreal_storage().await;
    let mut response = storage
        .lock()
        .await
        .db()
        .query(
            "SELECT <string> id AS id FROM nodes WHERE name = 'target'; \
             SELECT <string> to AS to FROM edges WHERE edge_type = 'calls';",
        )
        .await?
        .check()?;
    let targets: Vec<Value> = response.take(0)?;
    let edges: Vec<Value> = response.take(1)?;

    assert_eq!(targets.len(), 1, "expected exactly one target node");
    let target_id = targets[0]["id"]
        .as_str()
        .expect("target node id is a string");
    assert!(
        edges
            .iter()
            .any(|edge| edge["to"].as_str() == Some(target_id)),
        "caller edge must follow the shifted target id; target={target_id}, edges={edges:?}"
    );
    Ok(())
}

#[tokio::test]
async fn single_file_index_resolves_edges_against_the_project_catalog() -> Result<()> {
    if !test_env::run(
        concat!(
            module_path!(),
            "::single_file_index_resolves_edges_against_the_project_catalog"
        ),
        &[
            ("CODEGRAPH_SURREALDB_URL", Some("mem://")),
            ("CODEGRAPH_NO_PROGRESS", Some("1")),
            ("CODEGRAPH_ANALYZERS", Some("0")),
            ("CODEGRAPH_ANALYZERS_REQUIRE_TOOLS", Some("0")),
        ],
    ) {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let caller = dir.path().join("caller.rs");
    let helper = dir.path().join("helper.rs");
    std::fs::write(&caller, "fn caller() { helper(); }\n")?;
    std::fs::write(&helper, "fn helper() {}\n")?;

    let config = IndexerConfig {
        project_root: dir.path().into(),
        indexing_tier: IndexingTier::Fast,
        ..Default::default()
    };
    let indexer = ProjectIndexer::new(
        config,
        &CodeGraphConfig::default(),
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden()),
    )
    .await?;
    indexer
        .surreal_storage()
        .await
        .lock()
        .await
        .db()
        .query(include_str!("../../../schema/codegraph_v2.surql"))
        .await?
        .check()?;
    indexer.index_project(dir.path()).await?;

    // Re-index only the caller, which still references helper() from another file.
    std::fs::write(&caller, "fn caller() {\n    helper();\n}\n")?;
    indexer.index_single_file(&caller).await?;

    let storage = indexer.surreal_storage().await;
    let mut response = storage
        .lock()
        .await
        .db()
        .query(
            "SELECT <string> id AS id FROM nodes WHERE name = 'helper'; \
             SELECT <string> to AS to FROM edges WHERE edge_type = 'calls';",
        )
        .await?
        .check()?;
    let helpers: Vec<Value> = response.take(0)?;
    let edges: Vec<Value> = response.take(1)?;

    assert_eq!(helpers.len(), 1, "expected exactly one helper node");
    let helper_id = helpers[0]["id"]
        .as_str()
        .expect("helper node id is a string");
    assert!(
        edges
            .iter()
            .any(|edge| edge["to"].as_str() == Some(helper_id)),
        "a partial re-index must resolve cross-file calls from the project catalog; \
         helper={helper_id}, edges={edges:?}"
    );
    Ok(())
}

mod test_env {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/support/env.rs"
    ));
}
