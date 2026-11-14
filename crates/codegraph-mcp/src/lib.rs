pub mod analyzers;
pub mod connection;
pub mod estimation;
pub mod heartbeat;
pub mod indexer;
pub mod policy;
mod reconciliation;
mod resolution;
pub mod scip_auto;
#[cfg(any(test, feature = "ai-enhanced"))]
mod semantic_scoring;
pub mod transport;
pub mod utils;
mod writer;

pub use codegraph_mcp_core::{
    debug_logger::*,
    error::{McpError, Result},
    message::*,
    process::{ProcessInfo, ProcessManager, ProcessStatus},
    protocol::*,
    version::*,
};
#[cfg(feature = "embeddings")]
pub use codegraph_mcp_tools::{CacheStats, GraphToolExecutor, GraphToolSchemas, ToolSchema};
pub use connection::*;
pub use estimation::{
    EmbeddingThroughputConfig, RepositoryEstimate, RepositoryEstimator, TimeEstimates,
    build_symbol_index,
};
pub use heartbeat::*;
pub use indexer::{
    IndexStats, IndexerConfig, ProjectIndexer, StorageBackend, StorageMode, open_storage,
};
pub use transport::*;

// Server modules have moved to codegraph-mcp-server; no re-exports to avoid dependency cycles.
