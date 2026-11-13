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

    async fn reconcile(
        &self,
        project_id: &str,
        fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
        fresh_chunk_ids: &BTreeSet<String>,
        fresh_edge_ids: &BTreeSet<String>,
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

    /// Point every edge whose `to_id` is `old` at `new` instead. `from_id` is
    /// deliberately untouched: this mirrors the existing SurrealQL redirect.
    async fn retarget_edges(&self, project_id: &str, old: &str, new: &str) -> Result<u64>;

    /// Make sure the vector indexes exist for `dimension`. `tables` names the
    /// tables to check; backends that create indexes in their DDL may ignore it.
    /// Returns whether the indexes are ready.
    async fn ensure_vector_indexes(&self, dimension: usize, tables: &[String]) -> Result<bool>;
}
