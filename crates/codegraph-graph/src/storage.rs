// ABOUTME: Backend-neutral storage records and the IndexStorage trait shared by the SurrealDB and Postgres adapters.
// ABOUTME: Deliberately free of feature gates so both backends are checked against the same interface.

//! Backend-neutral storage interface for the indexing pipeline.
//!
//! The record types used by bulk writes live here rather than in
//! `surrealdb_storage` so they compile without any backend feature. The
//! [`IndexStorage`] trait describes only the operations `ProjectIndexer` calls
//! today; both `SurrealDbStorage` and `PgStore` implement it.
//!
//! The writer queue (backpressure, batching, flush) in `codegraph-mcp` drives this
//! trait, so it is backend-neutral: wiring Postgres into it is a storage-pool
//! change, not a queue change.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use codegraph_core::CodeNode;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::edge::CodeEdge;

pub const SURR_EMBEDDING_COLUMN_384: &str = "embedding_384";
pub const SURR_EMBEDDING_COLUMN_768: &str = "embedding_768";
pub const SURR_EMBEDDING_COLUMN_1024: &str = "embedding_1024";
pub const SURR_EMBEDDING_COLUMN_1536: &str = "embedding_1536";
pub const SURR_EMBEDDING_COLUMN_2048: &str = "embedding_2048";
pub const SURR_EMBEDDING_COLUMN_2560: &str = "embedding_2560";
pub const SURR_EMBEDDING_COLUMN_3072: &str = "embedding_3072";
pub const SURR_EMBEDDING_COLUMN_4096: &str = "embedding_4096";

/// Dimension → embedding column mapping shared by every backend adapter.
///
/// Despite the `SURR_`-prefixed column constants, the mapping itself is
/// backend-neutral: it names the per-dimension vector field on the shared
/// embedding records. Kept out of the SurrealDB module so Postgres-only builds
/// can resolve a column without compiling the SurrealDB backend.
#[derive(Clone, Copy, Debug)]
pub enum EmbeddingColumn {
    Embedding384,
    Embedding768,
    Embedding1024,
    Embedding1536,
    Embedding2048,
    Embedding2560,
    Embedding3072,
    Embedding4096,
}

impl EmbeddingColumn {
    pub fn column_name(&self) -> &'static str {
        match self {
            EmbeddingColumn::Embedding384 => SURR_EMBEDDING_COLUMN_384,
            EmbeddingColumn::Embedding768 => SURR_EMBEDDING_COLUMN_768,
            EmbeddingColumn::Embedding1024 => SURR_EMBEDDING_COLUMN_1024,
            EmbeddingColumn::Embedding1536 => SURR_EMBEDDING_COLUMN_1536,
            EmbeddingColumn::Embedding2048 => SURR_EMBEDDING_COLUMN_2048,
            EmbeddingColumn::Embedding2560 => SURR_EMBEDDING_COLUMN_2560,
            EmbeddingColumn::Embedding3072 => SURR_EMBEDDING_COLUMN_3072,
            EmbeddingColumn::Embedding4096 => SURR_EMBEDDING_COLUMN_4096,
        }
    }

    pub fn dimension(&self) -> usize {
        match self {
            EmbeddingColumn::Embedding384 => 384,
            EmbeddingColumn::Embedding768 => 768,
            EmbeddingColumn::Embedding1024 => 1024,
            EmbeddingColumn::Embedding1536 => 1536,
            EmbeddingColumn::Embedding2048 => 2048,
            EmbeddingColumn::Embedding2560 => 2560,
            EmbeddingColumn::Embedding3072 => 3072,
            EmbeddingColumn::Embedding4096 => 4096,
        }
    }
}

/// Lowercase hex without per-byte `String` allocation.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 0x0f) as usize]);
    }
    // SAFETY: every pushed byte is ASCII hex.
    String::from_utf8(out).expect("hex is ASCII")
}

fn symbol_embedding_record_id(project_id: &str, normalized_symbol: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b":");
    hasher.update(normalized_symbol.as_bytes());
    hex_lower(&hasher.finalize())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEmbeddingRecord {
    pub id: String,
    pub symbol: String,
    pub normalized_symbol: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_384: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_768: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_1024: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_1536: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_2048: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_2560: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_3072: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_4096: Option<Vec<f32>>,
    pub embedding_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_edge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
    pub last_computed_at: DateTime<Utc>,
    pub access_count: i64,
}

impl SymbolEmbeddingRecord {
    pub fn new(
        project_id: &str,
        organization_id: Option<&str>,
        symbol: &str,
        normalized_symbol: &str,
        embedding: &[f32],
        embedding_model: &str,
        embedding_column: &str,
        node_id: Option<&str>,
        source_edge_id: Option<&str>,
        metadata: Option<JsonValue>,
    ) -> Self {
        // Keep f32: the provider emits f32, SurrealDB stores F32, and serialising a
        // widened f64 roughly doubles each element's JSON text (0.12345679 vs
        // 0.12345679104328156), i.e. ~7 KB extra per 1024-d row.
        let embedding_vec: Vec<f32> = embedding.to_vec();
        let (
            embedding_384,
            embedding_768,
            embedding_1024,
            embedding_1536,
            embedding_2048,
            embedding_2560,
            embedding_3072,
            embedding_4096,
        ) = match embedding_column {
            SURR_EMBEDDING_COLUMN_384 => (
                Some(embedding_vec),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_768 => (
                None,
                Some(embedding_vec),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_1024 => (
                None,
                None,
                Some(embedding_vec),
                None,
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_1536 => (
                None,
                None,
                None,
                Some(embedding_vec),
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_2048 => (
                None,
                None,
                None,
                None,
                Some(embedding_vec),
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_2560 => (
                None,
                None,
                None,
                None,
                None,
                Some(embedding_vec),
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_3072 => (
                None,
                None,
                None,
                None,
                None,
                None,
                Some(embedding_vec),
                None,
            ),
            SURR_EMBEDDING_COLUMN_4096 => (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(embedding_vec),
            ),
            _ => (None, None, None, None, None, None, None, None),
        };

        SymbolEmbeddingRecord {
            id: symbol_embedding_record_id(project_id, normalized_symbol),
            symbol: symbol.to_string(),
            normalized_symbol: normalized_symbol.to_string(),
            project_id: project_id.to_string(),
            organization_id: organization_id.map(|s| s.to_string()),
            embedding_384,
            embedding_768,
            embedding_1024,
            embedding_1536,
            embedding_2048,
            embedding_2560,
            embedding_3072,
            embedding_4096,
            embedding_model: embedding_model.to_string(),
            node_id: node_id.map(|s| s.to_string()),
            source_edge_id: source_edge_id.map(|s| s.to_string()),
            metadata,
            last_computed_at: Utc::now(),
            access_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadataRecord {
    pub file_path: String,
    pub project_id: String,
    pub content_hash: String,
    pub modified_at: DateTime<Utc>,
    pub file_size: i64,
    pub last_indexed_at: DateTime<Utc>,
    pub node_count: i64,
    pub edge_count: i64,
    pub language: Option<String>,
    pub parse_errors: Option<Vec<String>>,
}

impl FileMetadataRecord {
    /// Generate a unique record ID based on project_id and file_path
    pub fn id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.project_id.as_bytes());
        hasher.update(b":");
        hasher.update(self.file_path.as_bytes());
        hex_lower(&hasher.finalize())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMetadataRecord {
    pub metadata: JsonValue,
    pub project_id: String,
    pub name: String,
    pub root_path: String,
    pub primary_language: Option<String>,
    pub file_count: i64,
    pub node_count: i64,
    pub edge_count: i64,
    pub avg_coverage_score: f32,
    pub last_analyzed: DateTime<Utc>,
    pub codegraph_version: String,
    pub organization_id: Option<String>,
    pub domain: Option<String>,
}

/// One node's identity as seen from a file scan, used to redirect edges whose
/// endpoint id changed because a line moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInFile {
    pub id: String,
    pub file_path: String,
    pub name: String,
    pub node_type: String,
}

/// A stored symbol outside the files a partial re-index scanned. Edge resolution
/// needs their ids so edges emitted by the scanned files can still resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInProject {
    /// Decoded to the plain stored form; the backend strips its own id prefix.
    pub id: String,
    pub file_path: String,
    pub name: String,
    pub node_type: Option<String>,
    pub qualified_name: Option<String>,
}

/// One node matched by a name lookup, as `codegraph query` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolMatch {
    pub name: String,
    pub node_type: Option<String>,
    pub language: Option<String>,
    pub file_path: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
}

/// One caller or callee edge, projected for `codegraph query --callers/--callees`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallRelation {
    pub symbol: String,
    pub node_type: Option<String>,
    pub file: String,
    pub line: Option<i32>,
    /// `edges.metadata["resolution_method"]` when the edge carries it.
    pub resolution: Option<String>,
}

/// Node identity used by `codegraph plan verify` and `plan show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeIdentityRow {
    pub id: String,
    pub name: String,
    pub file_path: String,
    /// Already decoded: SurrealDB stores a compressed JSON string, Postgres a
    /// `jsonb` value. `None` when the node has no metadata.
    pub metadata: Option<serde_json::Value>,
}

/// One edge's endpoints plus its relationship kind, for `plan show`/`verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeEndpoint {
    pub from: String,
    pub to: String,
    pub edge_type: String,
}

/// Ceiling every name/edge lookup applies to its caller-supplied `limit`.
///
/// `limit <= 0` means "no explicit bound" and maps to the ceiling instead of
/// asking either backend for zero rows.
// Only the two backend modules apply the clamp today; without either feature the
// helper is unreachable, so keep the default build warning-free.
#[cfg_attr(
    not(any(feature = "surrealdb", feature = "postgres")),
    allow(dead_code)
)]
pub(crate) const LOOKUP_LIMIT_MAX: i64 = 1000;

/// Clamp a lookup `limit` to `1..=1000`, treating `<= 0` as the ceiling.
///
/// Both backends apply the same rule so `codegraph query --limit` behaves
/// identically over SurrealQL `LIMIT` and Postgres `LIMIT`.
#[cfg_attr(
    not(any(feature = "surrealdb", feature = "postgres")),
    allow(dead_code)
)]
pub(crate) fn clamp_lookup_limit(limit: i64) -> i64 {
    if limit <= 0 {
        LOOKUP_LIMIT_MAX
    } else {
        limit.min(LOOKUP_LIMIT_MAX)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeEmbeddingRecord {
    pub id: String,
    pub column: &'static str,
    pub embedding: Vec<f32>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkEmbeddingRecord {
    pub id: String,
    pub parent_node: String,
    pub chunk_index: i32,
    pub text: String, // Back to String (Base64 encoded if compressed)
    pub project_id: String,
    pub embedding_384: Option<Vec<f32>>,
    pub embedding_768: Option<Vec<f32>>,
    pub embedding_1024: Option<Vec<f32>>,
    pub embedding_1536: Option<Vec<f32>>,
    pub embedding_2048: Option<Vec<f32>>,
    pub embedding_2560: Option<Vec<f32>>,
    pub embedding_3072: Option<Vec<f32>>,
    pub embedding_4096: Option<Vec<f32>>,
    pub embedding_model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ChunkEmbeddingRecord {
    pub fn identity(parent_node: &str, chunk_index: usize) -> String {
        let mut hasher = Sha256::new();
        hasher.update(parent_node.as_bytes());
        hasher.update(b":");
        hasher.update(chunk_index.to_le_bytes());
        hex_lower(&hasher.finalize())
    }
    pub fn new(
        parent_node: &str,
        chunk_index: usize,
        text: String,
        embedding: &[f32],
        embedding_model: &str,
        embedding_column: &str,
        project_id: &str,
    ) -> Self {
        // Keep f32: the provider emits f32, SurrealDB stores F32, and serialising a
        // widened f64 roughly doubles each element's JSON text (0.12345679 vs
        // 0.12345679104328156), i.e. ~7 KB extra per 1024-d row.
        let embedding_vec: Vec<f32> = embedding.to_vec();

        // Use Base64 encoding for compression to satisfy String type
        let text_val = codegraph_core::compress_to_string(&text);

        let (
            embedding_384,
            embedding_768,
            embedding_1024,
            embedding_1536,
            embedding_2048,
            embedding_2560,
            embedding_3072,
            embedding_4096,
        ) = match embedding_column {
            // ... match arms ...
            SURR_EMBEDDING_COLUMN_384 => (
                Some(embedding_vec),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_768 => (
                None,
                Some(embedding_vec),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_1024 => (
                None,
                None,
                Some(embedding_vec),
                None,
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_1536 => (
                None,
                None,
                None,
                Some(embedding_vec),
                None,
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_2048 => (
                None,
                None,
                None,
                None,
                Some(embedding_vec),
                None,
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_2560 => (
                None,
                None,
                None,
                None,
                None,
                Some(embedding_vec),
                None,
                None,
            ),
            SURR_EMBEDDING_COLUMN_3072 => (
                None,
                None,
                None,
                None,
                None,
                None,
                Some(embedding_vec),
                None,
            ),
            SURR_EMBEDDING_COLUMN_4096 => (
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(embedding_vec),
            ),
            _ => (None, None, None, None, None, None, None, None),
        };

        let id = Self::identity(parent_node, chunk_index);

        Self {
            id,
            parent_node: parent_node.to_string(),
            chunk_index: chunk_index as i32,
            text: text_val,
            project_id: project_id.to_string(),
            embedding_384,
            embedding_768,
            embedding_1024,
            embedding_1536,
            embedding_2048,
            embedding_2560,
            embedding_3072,
            embedding_4096,
            embedding_model: embedding_model.to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

/// Restricts a catalog reconciliation to files under one scan root.
///
/// Without a scope, every node the current scan did not re-emit is treated as deleted.
/// Scoped reconciliation only touches files under `scan_root`, so indexing a single
/// file or a subdirectory cannot wipe the rest of the project's graph.
#[derive(Debug, Clone)]
pub struct ReconcileScope {
    /// Canonical project root that stored relative `file_path` values resolve against.
    pub project_root: PathBuf,
    /// Canonical directory or file the current scan covered.
    pub scan_root: PathBuf,
}

/// Resolve `.` and `..` lexically without touching the filesystem, so paths for
/// deleted files still normalize the same way as paths for files that exist.
///
/// Both backends use it: SurrealDB and Postgres each decide `in_scope` from the
/// same normalized path comparison.
#[cfg_attr(
    not(any(feature = "surrealdb", feature = "postgres")),
    allow(dead_code)
)]
pub(crate) fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Row counts removed by a reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub stale_nodes: u64,
    pub stale_edges: u64,
    pub stale_chunks: u64,
    pub stale_symbols: u64,
    pub stale_files: u64,
}

/// The storage operations the indexing pipeline needs, independent of backend.
///
/// Deliberately narrow: only what `ProjectIndexer` calls today. Bulk writes are
/// expressed as `upsert_*` batches, and the writer queue in `codegraph-mcp`
/// (backpressure, batching, flush) drives this trait rather than a concrete store.
#[async_trait]
pub trait IndexStorage: Send + Sync {
    async fn upsert_nodes(&self, nodes: &[CodeNode]) -> Result<u64>;
    async fn upsert_edges(&self, edges: &[CodeEdge]) -> Result<u64>;
    async fn upsert_chunks(&self, chunks: &[ChunkEmbeddingRecord]) -> Result<u64>;
    async fn upsert_symbols(&self, symbols: &[SymbolEmbeddingRecord]) -> Result<u64>;
    async fn upsert_file_metadata(&self, files: &[FileMetadataRecord]) -> Result<u64>;

    async fn update_node_embeddings(&self, rows: &[NodeEmbeddingRecord]) -> Result<u64>;
    async fn delete_data_for_files(&self, project_id: &str, file_paths: &[String]) -> Result<u64>;
    async fn upsert_project_metadata(&self, record: ProjectMetadataRecord) -> Result<()>;

    /// Remove rows the current run did not re-emit.
    ///
    /// `scope` limits the reconciliation to files under `scan_root`; `None` means
    /// the whole project, while `Some` means only files below `scan_root` may be
    /// treated as deleted or re-emitted.
    async fn reconcile(
        &self,
        project_id: &str,
        fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
        fresh_chunk_ids: &BTreeSet<String>,
        fresh_edge_ids: &BTreeSet<String>,
        scope: Option<&ReconcileScope>,
    ) -> Result<ReconcileOutcome>;

    async fn file_hashes(&self, project_id: &str) -> Result<HashMap<String, String>>;
    async fn node_count(&self, project_id: &str) -> Result<i64>;
    async fn edge_count(&self, project_id: &str) -> Result<i64>;
    async fn chunk_count(&self, project_id: &str) -> Result<i64>;

    async fn knn_chunks(
        &self,
        project_id: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>>;
    async fn reverse_deps(
        &self,
        project_id: &str,
        from_node: &str,
        depth: u32,
    ) -> Result<Vec<String>>;

    /// The `metadata.input_fingerprint` stored with the project's metadata row,
    /// or `None` when the row (or the key) is absent.
    async fn project_fingerprint(&self, project_id: &str) -> Result<Option<String>>;

    /// Drop the project metadata row before a re-index mutates the graph.
    async fn delete_project_metadata(&self, project_id: &str) -> Result<()>;

    /// Whether a project metadata row exists (`is_indexed` / verification probes).
    async fn has_project_metadata(&self, project_id: &str) -> Result<bool>;

    /// Every stored file metadata row for the project, keyed by path on the
    /// caller's side. Used by incremental change detection.
    async fn file_metadata(&self, project_id: &str) -> Result<Vec<FileMetadataRecord>>;

    /// Nodes whose `file_path` is one of `files`, with ids already decoded to
    /// the plain stored form. Backends differ on whether the raw id carries a
    /// table prefix, so decoding is the backend's job.
    async fn nodes_in_files(&self, project_id: &str, files: &[String]) -> Result<Vec<NodeInFile>>;

    /// Symbols whose `file_path` is not one of `excluded_files`, with ids already
    /// decoded. Used by a partial re-index to resolve edges against the rest of
    /// the project without loading the files it just scanned again.
    async fn symbols_excluding_files(
        &self,
        project_id: &str,
        excluded_files: &[String],
    ) -> Result<Vec<SymbolInProject>>;

    /// Point every edge whose `to_id` is `old` at `new` instead. `from_id` is
    /// deliberately untouched: this mirrors the existing SurrealQL redirect.
    async fn retarget_edges(&self, project_id: &str, old: &str, new: &str) -> Result<u64>;

    /// Make sure the vector indexes exist for `dimension`. `tables` names the
    /// tables to check; backends that create indexes in their DDL may ignore it.
    /// Returns whether the indexes are ready.
    async fn ensure_vector_indexes(&self, dimension: usize, tables: &[String]) -> Result<bool>;

    /// Exact-or-prefix match on node name, ordered by name for determinism.
    ///
    /// `limit` is clamped to `1..=1000`; `limit <= 0` means 1000.
    async fn find_nodes_by_name(
        &self,
        project_id: &str,
        symbol: &str,
        limit: i64,
    ) -> Result<Vec<SymbolMatch>>;

    /// Callers: edges with `edge_type = 'calls'` whose target node has this name.
    ///
    /// `limit` is clamped to `1..=1000`; `limit <= 0` means 1000.
    async fn callers_of_name(
        &self,
        project_id: &str,
        target: &str,
        limit: i64,
    ) -> Result<Vec<CallRelation>>;

    /// Callees: the mirror image of [`IndexStorage::callers_of_name`].
    async fn callees_of_name(
        &self,
        project_id: &str,
        target: &str,
        limit: i64,
    ) -> Result<Vec<CallRelation>>;

    /// Every node's id, name and file path, for `plan verify`.
    async fn node_identities(&self, project_id: &str) -> Result<Vec<NodeIdentityRow>>;

    /// Every edge's endpoints and kind, for `plan verify` (which only needs the
    /// pair) and `plan show` (which round-trips the kind).
    async fn edge_endpoints(&self, project_id: &str) -> Result<Vec<EdgeEndpoint>>;

    /// Every node's id, name and decoded metadata, for `plan show`.
    async fn nodes_with_metadata(&self, project_id: &str) -> Result<Vec<NodeIdentityRow>>;

    /// `project_metadata.metadata` as a JSON value (`plan` stores constraints there).
    async fn project_metadata_value(&self, project_id: &str) -> Result<Option<serde_json::Value>>;

    /// Delete every row of this project from all six tables, in one transaction.
    /// Used by `plan store --force` and `plan drop`; the caller is responsible for
    /// only passing `plan:`-scoped ids.
    async fn delete_project(&self, project_id: &str) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalize_path_collapses_dot_and_dotdot() {
        assert_eq!(
            lexical_normalize_path(Path::new("./a/../b")),
            PathBuf::from("b"),
            "a parent component must cancel the segment before it"
        );
        assert_eq!(
            lexical_normalize_path(Path::new("a/./b")),
            PathBuf::from("a/b"),
            "a current-dir component must be dropped"
        );
    }
}
