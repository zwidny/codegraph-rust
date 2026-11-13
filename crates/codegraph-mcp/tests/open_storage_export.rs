// ABOUTME: Compile-time check that codegraph-mcp exports the backend selector.
// ABOUTME: Verifies `open_storage`, `StorageBackend` and `StorageMode` reach the crate root.

/// Constrains a future to the exact shape the read-only CLIs rely on:
/// `anyhow::Result<Arc<dyn IndexStorage>>`.
fn assert_storage_future<'a, F>(_f: F)
where
    F: std::future::Future<
            Output = anyhow::Result<std::sync::Arc<dyn codegraph_graph::storage::IndexStorage>>,
        > + 'a,
{
}

#[test]
fn open_storage_is_reexported_at_the_crate_root() {
    // Constructing the future performs no I/O: an `async fn` body only runs when
    // polled, so this stays a pure type check without touching a database.
    let root = std::path::Path::new(".");
    let pending = codegraph_mcp::open_storage(
        codegraph_mcp::StorageBackend::Surreal,
        root,
        codegraph_mcp::StorageMode::ReadOnly,
    );
    assert_storage_future(pending);

    // Both modes must be reachable from the crate root: the query CLI asks for
    // ReadOnly, the plan CLI for ReadWrite.
    assert_ne!(
        codegraph_mcp::StorageMode::ReadOnly,
        codegraph_mcp::StorageMode::ReadWrite
    );
}
