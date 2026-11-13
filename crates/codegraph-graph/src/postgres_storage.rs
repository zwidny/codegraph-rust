// ABOUTME: Postgres/pgvector storage scaffolding for the backend migration evaluated in docs/BACKEND_EVALUATION.md.
// ABOUTME: Kept behind the `postgres` feature and out of the indexing path until parity is proven.

//! Offline proof-of-concept storage layer for Postgres + pgvector.
//!
//! This module implements the operations the migration evaluation measured (chunk
//! upsert via `COPY`, KNN search and recursive reverse dependency walks), the node
//! and symbol-embedding tables they ingest into, the DDL all of them need, and
//! catalog reconciliation. It is not wired into the indexing pipeline; see
//! `docs/BACKEND_EVALUATION.md` for the measurements motivating it.
//!
//! # Edge identity and reconciliation
//!
//! `edges` carries an `id` primary key because reconciliation has to answer "was this
//! edge emitted by the current run?". `(from_id, to_id, edge_type)` cannot serve that
//! role: the same relationship can appear several times at different spans.
//!
//! `reconcile` deletes rows that the current run did not re-emit: nodes in files that
//! disappeared or shrank, edges whose endpoints live in scanned files yet whose ids
//! are missing from the fresh set, and the chunks/symbol embeddings owned by the
//! deleted nodes. This mirrors `SurrealDbStorage::reconcile_catalog_by_file`; without
//! the edge rule a rebuild leaves behind edges whose endpoints still exist, which is
//! how 6,421 stale edges accumulated on the SurrealDB side.
//!
//! Only whole-project reconciliation is implemented. `IndexStorage::reconcile` accepts
//! the per-file `scope` parameter for partial indexing but rejects a `Some(scope)` with
//! an explicit `Configuration` error: failing loudly is safer than a prefix guess that
//! would delete rows outside the scan.
//!
//! ## Node retrieval
//!
//! `nodes` can be searched three ways: [`PgStore::knn_nodes`] (pgvector cosine
//! distance over `embedding_1024`), [`PgStore::full_text_nodes`] (`ts_rank_cd` over
//! the generated `content_tsv`) and [`PgStore::hybrid_nodes`], which fuses both lists
//! with Reciprocal Rank Fusion and keeps single-retriever hits.
//!
//! Full-text recall is made equivalent to SurrealDB's `EDGENGRAM(2,12)` by turning
//! every query term into a prefix tsquery (`term:*`); the `english` configuration
//! supplies the same stemming as `SNOWBALL`. **Ranking is not equivalent**: Postgres
//! uses `ts_rank_cd`, not BM25, so even when both backends return the same node set
//! the top-k order differs (96% vs 100% precision, 9% top-10 overlap in
//! `docs/BACKEND_EVALUATION.md`). BM25 ranking is out of scope here.
//!
//! ## Node embeddings
//!
//! `nodes` keeps a single `embedding_1024` vector column, so
//! [`NodeEmbeddingRecord::column`] is ignored on this backend: every write goes to
//! `embedding_1024` regardless of the requested `embedding_384`..`embedding_4096`
//! name. Vectors that are not 1024-dimensional are rejected by pgvector.
//!
//! ## Upgrading an existing database
//!
//! `CREATE TABLE IF NOT EXISTS` never adds columns, so the DDL also runs
//! `ALTER TABLE edges ADD COLUMN IF NOT EXISTS id text`. That backfills the column on
//! a pre-existing table but cannot add the primary key, and `upsert_edges` needs one
//! for `ON CONFLICT (id)`. An older `edges` table must therefore be **rebuilt** (drop
//! and recreate it, then re-index) before reconciliation is used; this module never
//! attempts `ADD PRIMARY KEY`.

use crate::edge::CodeEdge;
use crate::storage::{
    ChunkEmbeddingRecord, FileMetadataRecord, NodeEmbeddingRecord, ProjectMetadataRecord,
    ReconcileOutcome, ReconcileScope, SymbolEmbeddingRecord,
};
use async_trait::async_trait;
use codegraph_core::CodeNode;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use codegraph_core::{CodeGraphError, Result};
use deadpool_postgres::Pool;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::Type;

/// One row of the `chunks` table.
#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub id: String,
    pub project_id: String,
    pub chunk_index: i32,
    pub text: String,
    pub parent_node: String,
    pub embedding: Vec<f32>,
    pub embedding_model: String,
}

/// One row of the `symbol_embeddings` table.
#[derive(Debug, Clone)]
pub struct SymbolEmbeddingRow {
    pub id: String,
    pub project_id: String,
    pub symbol: String,
    pub normalized_symbol: String,
    pub embedding: Option<Vec<f32>>,
    pub embedding_model: Option<String>,
    pub node_id: Option<String>,
    pub source_edge_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// One row of the `nodes` table.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub node_type: Option<String>,
    pub language: Option<String>,
    pub file_path: Option<String>,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub content: Option<String>,
    pub complexity: Option<f64>,
    pub chunk_count: Option<i32>,
    pub embedding_model: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub metadata: Option<serde_json::Value>,
}

/// One row of the `edges` table.
#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: String,
    pub project_id: String,
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
}

/// One row of the `file_metadata` table.
///
/// `last_indexed_at` is deliberately absent: it is stamped by SQL `now()` on every
/// upsert, so a caller cannot rewind or forge the indexing timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMetadataRow {
    pub project_id: String,
    pub file_path: String,
    pub content_hash: String,
    pub modified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub file_size: Option<i64>,
    pub node_count: i32,
    pub edge_count: i32,
    pub language: Option<String>,
    pub parse_errors: Option<String>,
}

/// One fused node hit produced by [`PgStore::hybrid_nodes`].
///
/// `rrf` is the Reciprocal Rank Fusion score, `vector_rank`/`text_rank` are the
/// 1-based positions the node held in the KNN and full-text lists respectively.
/// A node found by only one retriever keeps `None` for the other rank; the RRF
/// formulation rewards appearing in both lists but never discards a single-list hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub rrf: f64,
    pub vector_rank: Option<usize>,
    pub text_rank: Option<usize>,
}

/// Per-table row counts removed by [`PgStore::reconcile`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileStats {
    pub stale_nodes: u64,
    pub stale_edges: u64,
    pub stale_chunks: u64,
    pub stale_symbols: u64,
    /// Metadata rows removed because their file disappeared from the project.
    pub stale_files: u64,
}

/// DDL for the chunk, edge, node, symbol-embedding, file-metadata and
/// project-metadata tables. Every statement is idempotent.
pub const SCHEMA_DDL: &str = r#"
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS chunks (
  id            text PRIMARY KEY,
  project_id    text NOT NULL,
  chunk_index   integer NOT NULL,
  text          text NOT NULL,
  parent_node   text NOT NULL,
  embedding     vector(1024),
  embedding_model text NOT NULL,
  content_tsv   tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(text,''))) STORED
);
CREATE INDEX IF NOT EXISTS chunks_project ON chunks(project_id);
CREATE INDEX IF NOT EXISTS chunks_parent ON chunks(parent_node);
CREATE INDEX IF NOT EXISTS chunks_hnsw ON chunks USING hnsw (embedding vector_cosine_ops) WITH (m=12, ef_construction=150);
CREATE INDEX IF NOT EXISTS chunks_tsv ON chunks USING gin(content_tsv);

CREATE TABLE IF NOT EXISTS edges (
  id         text PRIMARY KEY,
  project_id text NOT NULL,
  from_id    text NOT NULL,
  to_id      text NOT NULL,
  edge_type  text NOT NULL
);
ALTER TABLE edges ADD COLUMN IF NOT EXISTS id text;
CREATE INDEX IF NOT EXISTS edges_from ON edges(project_id, edge_type, from_id);
CREATE INDEX IF NOT EXISTS edges_to   ON edges(project_id, edge_type, to_id);
CREATE INDEX IF NOT EXISTS edges_project ON edges(project_id);

CREATE TABLE IF NOT EXISTS nodes (
  id              text PRIMARY KEY,
  project_id      text,
  name            text NOT NULL,
  node_type       text,
  language        text,
  file_path       text,
  start_line      integer,
  end_line        integer,
  content         text,
  complexity      double precision,
  chunk_count     integer,
  embedding_model text,
  embedding_1024  vector(1024),
  metadata        jsonb,
  created_at      timestamptz DEFAULT now(),
  updated_at      timestamptz DEFAULT now(),
  content_tsv     tsvector GENERATED ALWAYS AS (
    to_tsvector('english', coalesce(name,'') || ' ' || coalesce(content,''))) STORED
);
CREATE INDEX IF NOT EXISTS nodes_project ON nodes(project_id);
CREATE INDEX IF NOT EXISTS nodes_project_file ON nodes(project_id, file_path);
CREATE INDEX IF NOT EXISTS nodes_project_name ON nodes(project_id, name);
CREATE INDEX IF NOT EXISTS nodes_hnsw ON nodes USING hnsw (embedding_1024 vector_cosine_ops) WITH (m=12, ef_construction=150);
CREATE INDEX IF NOT EXISTS nodes_tsv ON nodes USING gin(content_tsv);

CREATE TABLE IF NOT EXISTS symbol_embeddings (
  id                text PRIMARY KEY,
  project_id        text,
  symbol            text NOT NULL,
  normalized_symbol text NOT NULL,
  embedding_1024    vector(1024),
  embedding_model   text,
  node_id           text,
  source_edge_id    text,
  metadata          jsonb,
  access_count      integer DEFAULT 0,
  created_at        timestamptz DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sym_project ON symbol_embeddings(project_id);
CREATE INDEX IF NOT EXISTS sym_project_norm ON symbol_embeddings(project_id, normalized_symbol);
CREATE INDEX IF NOT EXISTS sym_node ON symbol_embeddings(node_id);
CREATE INDEX IF NOT EXISTS sym_hnsw ON symbol_embeddings USING hnsw (embedding_1024 vector_cosine_ops) WITH (m=12, ef_construction=150);

CREATE TABLE IF NOT EXISTS file_metadata (
  project_id      text NOT NULL,
  file_path       text NOT NULL,
  content_hash    text NOT NULL,
  modified_at     timestamptz,
  file_size       bigint,
  last_indexed_at timestamptz DEFAULT now(),
  node_count      integer DEFAULT 0,
  edge_count      integer DEFAULT 0,
  language        text,
  parse_errors    text,
  PRIMARY KEY (project_id, file_path)
);
CREATE INDEX IF NOT EXISTS file_metadata_project ON file_metadata(project_id);

CREATE TABLE IF NOT EXISTS project_metadata (
  project_id        text PRIMARY KEY,
  name              text NOT NULL,
  root_path         text NOT NULL,
  primary_language  text,
  file_count        bigint NOT NULL DEFAULT 0,
  node_count        bigint NOT NULL DEFAULT 0,
  edge_count        bigint NOT NULL DEFAULT 0,
  avg_coverage_score real NOT NULL DEFAULT 0,
  last_analyzed     timestamptz NOT NULL DEFAULT now(),
  codegraph_version text NOT NULL DEFAULT '',
  organization_id   text,
  domain            text,
  metadata          jsonb NOT NULL DEFAULT '{}'::jsonb
);
"#;

/// Staging table for `COPY`. The embedding is carried as text so we never have to
/// encode pgvector's binary wire format; the `INSERT ... SELECT` performs the cast.
const STAGING_DDL: &str = r#"
CREATE TEMP TABLE chunks_staging (
  id text, project_id text, chunk_index integer, text text,
  parent_node text, embedding text, embedding_model text
) ON COMMIT DROP;
"#;

const COPY_INTO_STAGING: &str = r#"
COPY chunks_staging (id, project_id, chunk_index, text, parent_node, embedding, embedding_model)
FROM STDIN BINARY
"#;

const MERGE_STAGING: &str = r#"
INSERT INTO chunks (id, project_id, chunk_index, text, parent_node, embedding, embedding_model)
SELECT id, project_id, chunk_index, text, parent_node, embedding::vector, embedding_model
FROM chunks_staging
ON CONFLICT (id) DO UPDATE SET
  project_id = EXCLUDED.project_id,
  chunk_index = EXCLUDED.chunk_index,
  text = EXCLUDED.text,
  parent_node = EXCLUDED.parent_node,
  embedding = EXCLUDED.embedding,
  embedding_model = EXCLUDED.embedding_model
"#;

/// Staging table for node upserts. Like the chunk staging table, the embedding and
/// metadata are carried as text and cast during the merge.
const NODES_STAGING_DDL: &str = r#"
CREATE TEMP TABLE nodes_staging (
  id text, project_id text, name text, node_type text, language text,
  file_path text, start_line integer, end_line integer, content text,
  complexity double precision, chunk_count integer, embedding_model text,
  embedding text, metadata text
) ON COMMIT DROP;
"#;

const COPY_NODES_INTO_STAGING: &str = r#"
COPY nodes_staging (id, project_id, name, node_type, language, file_path,
  start_line, end_line, content, complexity, chunk_count, embedding_model,
  embedding, metadata)
FROM STDIN BINARY
"#;

const MERGE_NODES_STAGING: &str = r#"
INSERT INTO nodes (id, project_id, name, node_type, language, file_path,
  start_line, end_line, content, complexity, chunk_count, embedding_model,
  embedding_1024, metadata)
SELECT id, project_id, name, node_type, language, file_path,
  start_line, end_line, content, complexity, chunk_count, embedding_model,
  embedding::vector, metadata::jsonb
FROM nodes_staging
ON CONFLICT (id) DO UPDATE SET
  project_id = EXCLUDED.project_id,
  name = EXCLUDED.name,
  node_type = EXCLUDED.node_type,
  language = EXCLUDED.language,
  file_path = EXCLUDED.file_path,
  start_line = EXCLUDED.start_line,
  end_line = EXCLUDED.end_line,
  content = EXCLUDED.content,
  complexity = EXCLUDED.complexity,
  chunk_count = EXCLUDED.chunk_count,
  embedding_model = EXCLUDED.embedding_model,
  embedding_1024 = EXCLUDED.embedding_1024,
  metadata = EXCLUDED.metadata,
  updated_at = now()
"#;

const SYMBOL_EMBEDDINGS_STAGING_DDL: &str = r#"
CREATE TEMP TABLE symbol_embeddings_staging (
  id text, project_id text, symbol text, normalized_symbol text,
  embedding text, embedding_model text, node_id text, source_edge_id text,
  metadata text
) ON COMMIT DROP;
"#;

const COPY_SYMBOL_EMBEDDINGS_INTO_STAGING: &str = r#"
COPY symbol_embeddings_staging (id, project_id, symbol, normalized_symbol,
  embedding, embedding_model, node_id, source_edge_id, metadata)
FROM STDIN BINARY
"#;

const MERGE_SYMBOL_EMBEDDINGS_STAGING: &str = r#"
INSERT INTO symbol_embeddings (id, project_id, symbol, normalized_symbol,
  embedding_1024, embedding_model, node_id, source_edge_id, metadata)
SELECT id, project_id, symbol, normalized_symbol, embedding::vector,
  embedding_model, node_id, source_edge_id, metadata::jsonb
FROM symbol_embeddings_staging
ON CONFLICT (id) DO UPDATE SET
  project_id = EXCLUDED.project_id,
  symbol = EXCLUDED.symbol,
  normalized_symbol = EXCLUDED.normalized_symbol,
  embedding_1024 = EXCLUDED.embedding_1024,
  embedding_model = EXCLUDED.embedding_model,
  node_id = EXCLUDED.node_id,
  source_edge_id = EXCLUDED.source_edge_id,
  metadata = EXCLUDED.metadata
"#;

const NODES_BY_IDS_SQL: &str = r#"
SELECT id, project_id, name, node_type, language, file_path,
       start_line, end_line, content, complexity, chunk_count, embedding_model,
       embedding_1024::text, metadata::text
FROM nodes
WHERE project_id = $1 AND id = ANY($2)
"#;

const NODE_COUNT_SQL: &str = r#"
SELECT count(*) FROM nodes WHERE project_id = $1
"#;

/// Whole-project scan used by reconciliation: one row per node identity and the file
/// it came from. Deliberately unfiltered by `file_path` so nodes without one are
/// visible to the caller.
const RECONCILE_NODES_SQL: &str = r#"
SELECT id, file_path FROM nodes WHERE project_id = $1
"#;

/// Whole-project edge scan used by reconciliation. Mirrors the SurrealDB variant,
/// which reads every edge once and diffs in memory rather than building a
/// project-sized `NOT IN` list.
const RECONCILE_EDGES_SQL: &str = r#"
SELECT id, from_id, to_id FROM edges WHERE project_id = $1
"#;

const UPSERT_EDGES_SQL: &str = r#"
INSERT INTO edges (id, project_id, from_id, to_id, edge_type)
SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[])
ON CONFLICT (id) DO UPDATE SET
  project_id = EXCLUDED.project_id,
  from_id = EXCLUDED.from_id,
  to_id = EXCLUDED.to_id,
  edge_type = EXCLUDED.edge_type
"#;

const DELETE_STALE_EDGES_SQL: &str = r#"
DELETE FROM edges
 WHERE project_id = $1
   AND (from_id = ANY($2) OR to_id = ANY($2) OR id = ANY($3))
"#;

const DELETE_STALE_SYMBOLS_SQL: &str = r#"
DELETE FROM symbol_embeddings WHERE project_id = $1 AND node_id = ANY($2)
"#;

/// Chunks owned by nodes that are about to be deleted.
const DELETE_STALE_CHUNKS_SQL: &str = r#"
DELETE FROM chunks WHERE project_id = $1 AND parent_node = ANY($2)
"#;

/// Chunks whose owning node survives but which this run did not re-emit. Only run
/// when the caller actually supplied chunk ids; an empty set means "no information",
/// not "every chunk is stale".
const DELETE_UNREEMITTED_CHUNKS_SQL: &str = r#"
DELETE FROM chunks
 WHERE project_id = $1 AND id != ALL($2) AND parent_node = ANY($3)
"#;

const DELETE_STALE_NODES_SQL: &str = r#"
DELETE FROM nodes WHERE project_id = $1 AND id = ANY($2)
"#;

/// `modified_at` travels as RFC 3339 text and is cast server-side, so the Postgres
/// backend does not need `tokio-postgres`'s chrono feature for one column.
const UPSERT_FILE_METADATA_SQL: &str = r#"
INSERT INTO file_metadata (project_id, file_path, content_hash, modified_at,
  file_size, node_count, edge_count, language, parse_errors, last_indexed_at)
SELECT t.project_id, t.file_path, t.content_hash, t.modified_at_text::timestamptz,
  t.file_size, t.node_count, t.edge_count, t.language, t.parse_errors, now()
FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[],
  $6::integer[], $7::integer[], $8::text[], $9::text[]) AS t(project_id, file_path,
  content_hash, modified_at_text, file_size, node_count, edge_count, language,
  parse_errors)
ON CONFLICT (project_id, file_path) DO UPDATE SET
  content_hash = EXCLUDED.content_hash,
  modified_at = EXCLUDED.modified_at,
  file_size = EXCLUDED.file_size,
  node_count = EXCLUDED.node_count,
  edge_count = EXCLUDED.edge_count,
  language = EXCLUDED.language,
  parse_errors = EXCLUDED.parse_errors,
  last_indexed_at = now()
"#;

const FILE_HASHES_SQL: &str = r#"
SELECT file_path, content_hash FROM file_metadata WHERE project_id = $1
"#;

const DELETE_FILES_SQL: &str = r#"
DELETE FROM file_metadata WHERE project_id = $1 AND file_path = ANY($2)
"#;

/// Reads the incremental-index fingerprint out of the JSON metadata blob. A missing
/// key yields NULL, which maps to `None` rather than an error.
const PROJECT_FINGERPRINT_SQL: &str = r#"
SELECT metadata->>'input_fingerprint' FROM project_metadata WHERE project_id = $1
"#;

const DELETE_PROJECT_METADATA_SQL: &str = r#"
DELETE FROM project_metadata WHERE project_id = $1
"#;

const HAS_PROJECT_METADATA_SQL: &str = r#"
SELECT EXISTS(SELECT 1 FROM project_metadata WHERE project_id = $1) AS present
"#;

/// Full file-metadata scan. Timestamps travel as RFC 3339 text so the backend does
/// not need `tokio-postgres`'s chrono feature; both timestamp columns are nullable.
const FILE_METADATA_SQL: &str = r#"
SELECT file_path, content_hash,
       to_char(modified_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
       file_size,
       to_char(last_indexed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
       node_count, edge_count, language, parse_errors
FROM file_metadata WHERE project_id = $1 ORDER BY file_path
"#;

/// Node identities for the files a re-index touched. `node_type` is nullable, so it
/// is coalesced to an empty string to match [`crate::storage::NodeInFile`].
const NODES_IN_FILES_SQL: &str = r#"
SELECT id, file_path, name, COALESCE(node_type, '') FROM nodes
WHERE project_id = $1 AND file_path = ANY($2) ORDER BY id
"#;

/// Repoint inbound edges after a symbol's id moved. Only `to_id` is rewritten: this
/// mirrors the SurrealQL redirect, which leaves `from_id` alone.
const RETARGET_EDGES_SQL: &str = r#"
UPDATE edges SET to_id = $3 WHERE project_id = $1 AND to_id = $2
"#;

/// Node ids living in the files a delete request names.
const NODE_IDS_BY_FILE_SQL: &str = r#"
SELECT id FROM nodes WHERE project_id = $1 AND file_path = ANY($2)
"#;

const DELETE_EDGES_BY_ENDPOINTS_SQL: &str = r#"
DELETE FROM edges WHERE project_id = $1 AND (from_id = ANY($2) OR to_id = ANY($2))
"#;

/// Update node vectors in one statement. `unnest` keeps id and embedding aligned by
/// position, so a partial vector list cannot silently shift onto the wrong node.
///
/// `nodes` has a single `embedding_1024` column: [`NodeEmbeddingRecord::column`] is
/// ignored here (SurrealDB keeps one column per dimension).
///
/// There is no `project_id` predicate because [`NodeEmbeddingRecord`] does not carry
/// one; `nodes.id` is the primary key, so a matching id is unambiguous across
/// projects.
const UPDATE_NODE_EMBEDDINGS_SQL: &str = r#"
UPDATE nodes AS n
   SET embedding_1024 = v.embedding::vector
  FROM (SELECT unnest($1::text[]) AS id, unnest($2::text[]) AS embedding) AS v
 WHERE n.id = v.id
"#;

/// `last_analyzed` travels as RFC 3339 text and `metadata` as JSON text, both cast
/// server-side, matching [`UPSERT_FILE_METADATA_SQL`].
const UPSERT_PROJECT_METADATA_SQL: &str = r#"
INSERT INTO project_metadata (project_id, name, root_path, primary_language,
  file_count, node_count, edge_count, avg_coverage_score, last_analyzed,
  codegraph_version, organization_id, domain, metadata)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::text::timestamptz, $10, $11, $12, $13::text::jsonb)
ON CONFLICT (project_id) DO UPDATE SET
  name = EXCLUDED.name,
  root_path = EXCLUDED.root_path,
  primary_language = EXCLUDED.primary_language,
  file_count = EXCLUDED.file_count,
  node_count = EXCLUDED.node_count,
  edge_count = EXCLUDED.edge_count,
  avg_coverage_score = EXCLUDED.avg_coverage_score,
  last_analyzed = EXCLUDED.last_analyzed,
  codegraph_version = EXCLUDED.codegraph_version,
  organization_id = EXCLUDED.organization_id,
  domain = EXCLUDED.domain,
  metadata = EXCLUDED.metadata
"#;

const KNN_SQL: &str = r#"
SELECT id, (embedding <=> $1::text::vector) AS distance
FROM chunks
WHERE project_id = $2
ORDER BY embedding <=> $1::text::vector
LIMIT $3
"#;

/// Node-level KNN over `embedding_1024`. Nodes without a vector are skipped, so the
/// limit counts candidates rather than rows that would sort as `NaN`.
const KNN_NODES_SQL: &str = r#"
SELECT id, (embedding_1024 <=> $1::text::vector) AS distance
FROM nodes
WHERE project_id = $2 AND embedding_1024 IS NOT NULL
ORDER BY embedding_1024 <=> $1::text::vector
LIMIT $3
"#;

/// Node-level full-text retrieval. Ranking uses `ts_rank_cd`, not BM25: the recall
/// matches SurrealDB's BM25 but the ordering does not (see
/// `docs/BACKEND_EVALUATION.md`).
const FULL_TEXT_NODES_SQL: &str = r#"
SELECT id, ts_rank_cd(content_tsv, to_tsquery('english', $1))::double precision AS rank
FROM nodes
WHERE project_id = $2 AND content_tsv @@ to_tsquery('english', $1)
ORDER BY rank DESC, id ASC
LIMIT $3
"#;

const REVERSE_DEPS_SQL: &str = r#"
WITH RECURSIVE rd(id, depth) AS (
  SELECT from_id, 1 FROM edges
   WHERE project_id = $1 AND edge_type = $2 AND to_id = $3
  UNION
  SELECT e.from_id, rd.depth + 1
    FROM edges e JOIN rd ON e.to_id = rd.id
   WHERE e.project_id = $1 AND e.edge_type = $2 AND rd.depth < $4
)
SELECT DISTINCT id FROM rd
"#;

/// Default edge type for [`PgStore::reverse_deps`].
pub const DEFAULT_EDGE_TYPE: &str = "calls";

/// Maximum traversal depth accepted by [`PgStore::reverse_deps`].
pub const MAX_REVERSE_DEPTH: u32 = 10;

/// Upper bound on `k` for every node-retrieval method, so a caller cannot ask the
/// pool for an unbounded result set.
pub const MAX_RETRIEVAL_K: usize = 200;

/// `k` used by [`PgStore::hybrid_nodes`] when the caller passes 0.
pub const DEFAULT_HYBRID_K: usize = 50;

/// Damping constant of the original Reciprocal Rank Fusion formulation.
const RRF_K: f64 = 60.0;

/// Longest single term accepted by [`tsquery_prefix`]. Lexemes are capped at 2047
/// bytes by Postgres; 64 keeps generated queries short and rejects pasted blobs.
const MAX_TSQUERY_TERM_LEN: usize = 64;

/// Rows per staging transaction. Larger inputs are split so a single transaction
/// (and its temporary table) stays bounded.
const UPSERT_BATCH_SIZE: usize = 5000;

fn db_err<E: std::fmt::Display>(error: E) -> CodeGraphError {
    CodeGraphError::Database(error.to_string())
}

/// Render a vector in pgvector's text input format: `[0.1,0.25,-1.5]`.
///
/// Values are rounded to six decimals and trailing zeros are trimmed, so the
/// output is deterministic without carrying float noise into the index.
pub(crate) fn vector_text(values: &[f32]) -> String {
    let mut out = String::with_capacity(values.len() * 10 + 2);
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&vector_component(*value));
    }
    out.push(']');
    out
}

fn vector_component(value: f32) -> String {
    if !value.is_finite() {
        // pgvector accepts these spellings; let the server reject anything it cannot store.
        return if value.is_nan() {
            "NaN".to_string()
        } else if value.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }

    let mut rendered = format!("{value:.6}");
    if rendered.contains('.') {
        while rendered.ends_with('0') {
            rendered.pop();
        }
        if rendered.ends_with('.') {
            rendered.pop();
        }
    }
    if rendered == "-0" {
        rendered = "0".to_string();
    }
    rendered
}

/// Render an optional embedding for a staging column. `None` stays `NULL`, so the
/// target column keeps a null vector instead of a zero-length one.
pub(crate) fn embedding_text(values: Option<&[f32]>) -> Option<String> {
    values.map(vector_text)
}

/// Serialise optional metadata for a `text` staging column; the merge casts it to
/// `jsonb`. `None` stays `NULL`.
pub(crate) fn metadata_text(value: Option<&serde_json::Value>) -> Result<Option<String>> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

/// Build a `to_tsquery('english', ...)` argument from a free-form user query.
///
/// The query is split on everything that is not ASCII-or-Unicode alphanumeric or
/// `_`, lowercased, and every surviving term becomes `term:*` (a prefix match).
/// Prefix matching is what makes the node path equivalent to SurrealDB's
/// `EDGENGRAM(2,12)` tokenizer: `term` matches `terms`, `terminate`, ... .
///
/// Returns `None` when no usable term is left, so callers skip the database instead
/// of sending an empty tsquery (which Postgres rejects). Splitting already removes
/// `'`, `"`, `&`, `|`, `!`, `(`, `)` and `:`, so no quoting is needed.
///
/// camelCase is **not** split: `HandleFile` becomes `handlefile:*`, which would only
/// match tokens that start with the literal `handlefile`.
pub(crate) fn tsquery_prefix(query: &str) -> Option<String> {
    let mut terms = Vec::new();
    for raw in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let cleaned: String = raw
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if cleaned.is_empty() || cleaned.chars().count() > MAX_TSQUERY_TERM_LEN {
            continue;
        }
        terms.push(format!("{cleaned}:*"));
    }

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" & "))
    }
}

/// Reciprocal Rank Fusion score contribution of one 1-based rank.
fn rrf_contribution(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64)
}

/// Fuse two ranked id lists with Reciprocal Rank Fusion.
///
/// `score(id) = sum over lists containing id of 1 / (60 + rank)`, where `rank` is the
/// 1-based position in that list. Hits present in only one list are kept with the
/// other rank left `None`; that partial credit is the point of RRF.
///
/// A repeated id inside one list counts only at its first position (later occurrences
/// are ignored, and do not shift the ranks of the remaining ids). Output is sorted by
/// `rrf` descending, ties broken by `id` ascending, so equal scores are deterministic.
/// `limit == 0` means "no limit".
pub(crate) fn rrf_fuse(vector_ids: &[String], text_ids: &[String], limit: usize) -> Vec<SearchHit> {
    let mut fused: BTreeMap<String, SearchHit> = BTreeMap::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for (index, id) in vector_ids.iter().enumerate() {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let rank = index + 1;
        let hit = fused.entry(id.clone()).or_insert_with(|| SearchHit {
            id: id.clone(),
            rrf: 0.0,
            vector_rank: None,
            text_rank: None,
        });
        hit.rrf += rrf_contribution(rank);
        hit.vector_rank = Some(rank);
    }

    seen.clear();
    for (index, id) in text_ids.iter().enumerate() {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let rank = index + 1;
        let hit = fused.entry(id.clone()).or_insert_with(|| SearchHit {
            id: id.clone(),
            rrf: 0.0,
            vector_rank: None,
            text_rank: None,
        });
        hit.rrf += rrf_contribution(rank);
        hit.text_rank = Some(rank);
    }

    let mut hits: Vec<SearchHit> = fused.into_values().collect();
    hits.sort_by(|a, b| {
        b.rrf
            .partial_cmp(&a.rrf)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    if limit > 0 {
        hits.truncate(limit);
    }
    hits
}

/// Parse pgvector's text output format (`[0.1,0.25,-1.5]`) back into floats.
///
/// `tokio-postgres` has no decoder for the `vector` type, so round-tripping goes
/// through the textual form pgvector produces.
pub(crate) fn parse_vector_text(text: &str) -> Result<Vec<f32>> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| db_err(format!("invalid pgvector literal: {text}")))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|component| {
            component.trim().parse::<f32>().map_err(|error| {
                db_err(format!("invalid pgvector component {component:?}: {error}"))
            })
        })
        .collect()
}

/// Node ids the current run did not re-emit.
///
/// Two cases, both derived from the stored `(id, file_path)` snapshot:
/// * a file present in the store but absent from `fresh_nodes_by_file` disappeared,
///   so every node it holds is stale;
/// * for a surviving file, everything stored under it but missing from its fresh id
///   list shrank away.
///
/// Nodes without a `file_path` never enter `existing_by_file` and are therefore never
/// reported; they cannot be attributed to any scanned file.
fn compute_stale_nodes(
    existing_by_file: &BTreeMap<String, Vec<String>>,
    fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut stale = Vec::new();

    for (file, existing) in existing_by_file {
        if !fresh_nodes_by_file.contains_key(file) {
            stale.extend(existing.iter().cloned());
        }
    }

    for (file, fresh_ids) in fresh_nodes_by_file {
        let Some(existing) = existing_by_file.get(file) else {
            continue;
        };
        let fresh: HashSet<&str> = fresh_ids.iter().map(String::as_str).collect();
        stale.extend(
            existing
                .iter()
                .filter(|id| !fresh.contains(id.as_str()))
                .cloned(),
        );
    }

    stale
}

/// Files present in the store but absent from this run's scan.
///
/// `file_metadata` is keyed by `(project_id, file_path)`, so a vanished file has no
/// fresh node list to diff against; the key set difference is the whole answer. The
/// output is sorted so callers (and tests) see a deterministic list.
fn compute_vanished_files(
    existing_by_file: &BTreeMap<String, Vec<String>>,
    fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut vanished: Vec<String> = existing_by_file
        .keys()
        .filter(|file| !fresh_nodes_by_file.contains_key(*file))
        .cloned()
        .collect();
    vanished.sort();
    vanished
}

/// Edge ids that are stale because a scanned endpoint no longer re-emits them.
///
/// Argument order matches [`PgStore::reconcile`]: `edges` holds `(id, from_id, to_id)`
/// rows read from the store, `file_by_node` maps every stored node id to its file, and
/// `fresh_nodes_by_file` identifies the files this run scanned.
///
/// An empty `fresh_edge_ids` means the caller supplied no information, **not** that
/// every edge is stale; deleting on that inference would wipe the table. Endpoints
/// that are no longer stored are skipped here because the stale-node delete already
/// removes their edges.
fn compute_stale_edges(
    edges: &[(String, String, String)],
    file_by_node: &HashMap<String, String>,
    fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
    fresh_edge_ids: &BTreeSet<String>,
) -> Vec<String> {
    if fresh_edge_ids.is_empty() {
        return Vec::new();
    }

    let scanned = |id: &str| {
        file_by_node
            .get(id)
            .is_some_and(|file| fresh_nodes_by_file.contains_key(file))
    };

    edges
        .iter()
        .filter(|(id, from_id, to_id)| {
            !fresh_edge_ids.contains(id) && (scanned(from_id) || scanned(to_id))
        })
        .map(|(id, _, _)| id.clone())
        .collect()
}

/// Postgres-backed store used by the offline POC only.
pub struct PgStore {
    pool: Pool,
}

impl PgStore {
    /// Build a connection pool. Defaults to `CODEGRAPH_PG_URL` and a pool size of
    /// 8 (`CODEGRAPH_PG_POOL_SIZE`). Connections are opened lazily by the pool.
    pub fn connect(url: &str) -> Result<Self> {
        let url = if url.trim().is_empty() {
            std::env::var("CODEGRAPH_PG_URL").map_err(|error| {
                CodeGraphError::Configuration(format!(
                    "CODEGRAPH_PG_URL is not set and no URL was supplied: {error}"
                ))
            })?
        } else {
            url.to_string()
        };

        let pool_size = std::env::var("CODEGRAPH_PG_POOL_SIZE")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|size| *size > 0)
            .unwrap_or(8);

        // Validate up front so a malformed URL fails at construction time.
        let _: tokio_postgres::Config = url.parse().map_err(|error| {
            CodeGraphError::Configuration(format!("invalid Postgres URL: {error}"))
        })?;
        let mut config = deadpool_postgres::Config {
            url: Some(url),
            ..Default::default()
        };
        config.pool = Some(deadpool_postgres::PoolConfig {
            max_size: pool_size,
            ..Default::default()
        });

        let pool = config
            .create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .map_err(db_err)?;
        Ok(Self { pool })
    }

    /// Apply the full storage DDL (chunks, edges, nodes, symbol embeddings). Safe to
    /// run repeatedly.
    pub async fn ensure_schema(&self) -> Result<()> {
        let client = self.pool.get().await.map_err(db_err)?;
        client.batch_execute(SCHEMA_DDL).await.map_err(db_err)?;
        Ok(())
    }

    /// Insert or update chunks in one transaction: `COPY` into a temporary table,
    /// then merge with `ON CONFLICT (id) DO UPDATE`.
    pub async fn upsert_chunks(&self, rows: &[ChunkRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut client = self.pool.get().await.map_err(db_err)?;
        let transaction = client.transaction().await.map_err(db_err)?;
        transaction
            .batch_execute(STAGING_DDL)
            .await
            .map_err(db_err)?;

        let sink = transaction
            .copy_in(COPY_INTO_STAGING)
            .await
            .map_err(db_err)?;
        let types = [
            Type::TEXT,
            Type::TEXT,
            Type::INT4,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
        ];
        let mut writer = std::pin::pin!(BinaryCopyInWriter::new(sink, &types));
        for row in rows {
            let embedding = vector_text(&row.embedding);
            writer
                .as_mut()
                .write(&[
                    &row.id,
                    &row.project_id,
                    &row.chunk_index,
                    &row.text,
                    &row.parent_node,
                    &embedding,
                    &row.embedding_model,
                ])
                .await
                .map_err(db_err)?;
        }
        writer.finish().await.map_err(db_err)?;

        let merged = transaction
            .execute(MERGE_STAGING, &[])
            .await
            .map_err(db_err)?;
        transaction.commit().await.map_err(db_err)?;
        Ok(merged)
    }

    /// Insert or update nodes. Inputs larger than [`UPSERT_BATCH_SIZE`] are split
    /// into independent transactions, each with its own staging table.
    pub async fn upsert_nodes(&self, rows: &[NodeRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut merged = 0u64;
        for batch in rows.chunks(UPSERT_BATCH_SIZE) {
            merged += self.upsert_node_batch(batch).await?;
        }
        Ok(merged)
    }

    async fn upsert_node_batch(&self, rows: &[NodeRow]) -> Result<u64> {
        let mut client = self.pool.get().await.map_err(db_err)?;
        let transaction = client.transaction().await.map_err(db_err)?;
        transaction
            .batch_execute(NODES_STAGING_DDL)
            .await
            .map_err(db_err)?;

        let sink = transaction
            .copy_in(COPY_NODES_INTO_STAGING)
            .await
            .map_err(db_err)?;
        let types = [
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::INT4,
            Type::INT4,
            Type::TEXT,
            Type::FLOAT8,
            Type::INT4,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
        ];
        let mut writer = std::pin::pin!(BinaryCopyInWriter::new(sink, &types));
        for row in rows {
            let embedding = embedding_text(row.embedding.as_deref());
            let metadata = metadata_text(row.metadata.as_ref())?;
            writer
                .as_mut()
                .write(&[
                    &row.id,
                    &row.project_id,
                    &row.name,
                    &row.node_type,
                    &row.language,
                    &row.file_path,
                    &row.start_line,
                    &row.end_line,
                    &row.content,
                    &row.complexity,
                    &row.chunk_count,
                    &row.embedding_model,
                    &embedding,
                    &metadata,
                ])
                .await
                .map_err(db_err)?;
        }
        writer.finish().await.map_err(db_err)?;

        let merged = transaction
            .execute(MERGE_NODES_STAGING, &[])
            .await
            .map_err(db_err)?;
        transaction.commit().await.map_err(db_err)?;
        Ok(merged)
    }

    /// Insert or update symbol embeddings. `access_count` is left untouched by an
    /// update so usage statistics survive re-indexing. Inputs larger than
    /// [`UPSERT_BATCH_SIZE`] are split into independent transactions.
    pub async fn upsert_symbol_embeddings(&self, rows: &[SymbolEmbeddingRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut merged = 0u64;
        for batch in rows.chunks(UPSERT_BATCH_SIZE) {
            merged += self.upsert_symbol_embedding_batch(batch).await?;
        }
        Ok(merged)
    }

    async fn upsert_symbol_embedding_batch(&self, rows: &[SymbolEmbeddingRow]) -> Result<u64> {
        let mut client = self.pool.get().await.map_err(db_err)?;
        let transaction = client.transaction().await.map_err(db_err)?;
        transaction
            .batch_execute(SYMBOL_EMBEDDINGS_STAGING_DDL)
            .await
            .map_err(db_err)?;

        let sink = transaction
            .copy_in(COPY_SYMBOL_EMBEDDINGS_INTO_STAGING)
            .await
            .map_err(db_err)?;
        let types = [Type::TEXT; 9];
        let mut writer = std::pin::pin!(BinaryCopyInWriter::new(sink, &types));
        for row in rows {
            let embedding = embedding_text(row.embedding.as_deref());
            let metadata = metadata_text(row.metadata.as_ref())?;
            writer
                .as_mut()
                .write(&[
                    &row.id,
                    &row.project_id,
                    &row.symbol,
                    &row.normalized_symbol,
                    &embedding,
                    &row.embedding_model,
                    &row.node_id,
                    &row.source_edge_id,
                    &metadata,
                ])
                .await
                .map_err(db_err)?;
        }
        writer.finish().await.map_err(db_err)?;

        let merged = transaction
            .execute(MERGE_SYMBOL_EMBEDDINGS_STAGING, &[])
            .await
            .map_err(db_err)?;
        transaction.commit().await.map_err(db_err)?;
        Ok(merged)
    }

    /// Fetch nodes by id, restricted to `project_id`. Order is not guaranteed.
    pub async fn nodes_by_ids(&self, project_id: &str, ids: &[String]) -> Result<Vec<NodeRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(NODES_BY_IDS_SQL, &[&project_id, &ids])
            .await
            .map_err(db_err)?;

        let mut nodes = Vec::with_capacity(rows.len());
        for row in &rows {
            let embedding: Option<String> = row.get(12);
            let embedding = match embedding.as_deref() {
                Some(text) => Some(parse_vector_text(text)?),
                None => None,
            };
            let metadata: Option<String> = row.get(13);
            let metadata = match metadata.as_deref() {
                Some(text) => Some(serde_json::from_str::<serde_json::Value>(text)?),
                None => None,
            };

            nodes.push(NodeRow {
                id: row.get(0),
                project_id: row.get(1),
                name: row.get(2),
                node_type: row.get(3),
                language: row.get(4),
                file_path: row.get(5),
                start_line: row.get(6),
                end_line: row.get(7),
                content: row.get(8),
                complexity: row.get(9),
                chunk_count: row.get(10),
                embedding_model: row.get(11),
                embedding,
                metadata,
            });
        }
        Ok(nodes)
    }

    /// Number of nodes stored for `project_id`.
    pub async fn node_count(&self, project_id: &str) -> Result<i64> {
        let client = self.pool.get().await.map_err(db_err)?;
        let count: i64 = client
            .query_one(NODE_COUNT_SQL, &[&project_id])
            .await
            .map_err(db_err)?
            .get(0);
        Ok(count)
    }

    /// Nearest neighbours for `query`, restricted to one project. Returns
    /// `(chunk_id, cosine_distance)` ordered by increasing distance.
    pub async fn knn_chunks(
        &self,
        project_id: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>> {
        let vector = vector_text(query);
        let limit = i64::try_from(k).unwrap_or(i64::MAX);
        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(KNN_SQL, &[&vector, &project_id, &limit])
            .await
            .map_err(db_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let id: String = row.get(0);
                // pgvector's distance operators return `double precision`.
                let distance: f64 = row.get(1);
                (id, distance as f32)
            })
            .collect())
    }

    /// Nearest nodes for `query`, restricted to one project. Returns `(node_id,
    /// cosine_distance)` ordered by increasing distance. Nodes without an
    /// `embedding_1024` are excluded, and `k` is capped at [`MAX_RETRIEVAL_K`].
    pub async fn knn_nodes(
        &self,
        project_id: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>> {
        let k = k.min(MAX_RETRIEVAL_K);
        if k == 0 {
            return Ok(Vec::new());
        }

        let vector = vector_text(query);
        let limit = i64::try_from(k).unwrap_or(i64::MAX);
        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(KNN_NODES_SQL, &[&vector, &project_id, &limit])
            .await
            .map_err(db_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let id: String = row.get(0);
                // pgvector's distance operators return `double precision`.
                let distance: f64 = row.get(1);
                (id, distance as f32)
            })
            .collect())
    }

    /// Full-text node retrieval over `nodes.content_tsv` (name + content).
    ///
    /// Each query term becomes a prefix match (`term:*`), which reproduces the
    /// recall of SurrealDB's `EDGENGRAM(2,12)`; the Postgres `english` configuration
    /// provides the same stemming as `SNOWBALL`. Ranking is `ts_rank_cd`, **not**
    /// BM25, so the top-k order differs from SurrealDB even when the candidate sets
    /// agree (see `docs/BACKEND_EVALUATION.md`).
    ///
    /// A query with no usable term returns an empty list without touching the
    /// database, and `k` is capped at [`MAX_RETRIEVAL_K`].
    pub async fn full_text_nodes(
        &self,
        project_id: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<(String, f64)>> {
        let k = k.min(MAX_RETRIEVAL_K);
        if k == 0 {
            return Ok(Vec::new());
        }
        let Some(tsquery) = tsquery_prefix(query) else {
            return Ok(Vec::new());
        };

        let limit = i64::try_from(k).unwrap_or(i64::MAX);
        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(FULL_TEXT_NODES_SQL, &[&tsquery, &project_id, &limit])
            .await
            .map_err(db_err)?;

        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    /// Fuse vector and full-text node retrieval with Reciprocal Rank Fusion.
    ///
    /// Both retrievers run concurrently and each contributes up to `k` candidates;
    /// `k == 0` selects [`DEFAULT_HYBRID_K`] and larger values are capped at
    /// [`MAX_RETRIEVAL_K`]. The first `k` fused hits are returned.
    pub async fn hybrid_nodes(
        &self,
        project_id: &str,
        query: &[f32],
        text: &str,
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        let k = if k == 0 {
            DEFAULT_HYBRID_K
        } else {
            k.min(MAX_RETRIEVAL_K)
        };

        let (vector_rows, text_rows) = tokio::join!(
            self.knn_nodes(project_id, query, k),
            self.full_text_nodes(project_id, text, k)
        );

        let vector_ids: Vec<String> = vector_rows?.into_iter().map(|(id, _)| id).collect();
        let text_ids: Vec<String> = text_rows?.into_iter().map(|(id, _)| id).collect();

        Ok(rrf_fuse(&vector_ids, &text_ids, k))
    }

    /// Walk incoming `calls` edges up to `depth` hops (capped at
    /// [`MAX_REVERSE_DEPTH`]). Returns every distinct upstream node id.
    pub async fn reverse_deps(
        &self,
        project_id: &str,
        from_node: &str,
        depth: u32,
    ) -> Result<Vec<String>> {
        self.reverse_deps_with_edge_type(project_id, from_node, DEFAULT_EDGE_TYPE, depth)
            .await
    }

    /// Reverse dependency walk with an explicit edge type.
    pub async fn reverse_deps_with_edge_type(
        &self,
        project_id: &str,
        from_node: &str,
        edge_type: &str,
        depth: u32,
    ) -> Result<Vec<String>> {
        let depth = depth.min(MAX_REVERSE_DEPTH) as i32;
        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(
                REVERSE_DEPS_SQL,
                &[&project_id, &edge_type, &from_node, &depth],
            )
            .await
            .map_err(db_err)?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    /// Insert or update edges by `id`. Requires the `id` primary key; inputs larger
    /// than [`UPSERT_BATCH_SIZE`] are split into independent statements.
    ///
    /// Edge ids must be stable across runs (see `CodeEdge::set_deterministic_id`),
    /// otherwise reconciliation sees every edge as newly emitted and keeps the old
    /// copies forever.
    pub async fn upsert_edges(&self, rows: &[EdgeRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }

        let client = self.pool.get().await.map_err(db_err)?;
        let mut merged = 0u64;
        for batch in rows.chunks(UPSERT_BATCH_SIZE) {
            let ids: Vec<String> = batch.iter().map(|row| row.id.clone()).collect();
            let projects: Vec<String> = batch.iter().map(|row| row.project_id.clone()).collect();
            let from_ids: Vec<String> = batch.iter().map(|row| row.from_id.clone()).collect();
            let to_ids: Vec<String> = batch.iter().map(|row| row.to_id.clone()).collect();
            let edge_types: Vec<String> = batch.iter().map(|row| row.edge_type.clone()).collect();

            merged += client
                .execute(
                    UPSERT_EDGES_SQL,
                    &[&ids, &projects, &from_ids, &to_ids, &edge_types],
                )
                .await
                .map_err(db_err)?;
        }
        Ok(merged)
    }

    /// Insert or update file metadata, keyed by `(project_id, file_path)`.
    ///
    /// `last_indexed_at` is stamped with SQL `now()` on insert and update; the row
    /// struct intentionally carries no client-side value for it. Inputs larger than
    /// [`UPSERT_BATCH_SIZE`] are split into independent statements. A single
    /// parameterised statement is enough because the row is small (no embedding) and
    /// a full re-index of a 10k-file project is still only ~20 statements.
    pub async fn upsert_file_metadata(&self, rows: &[FileMetadataRow]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }

        let client = self.pool.get().await.map_err(db_err)?;
        let mut merged = 0u64;
        for batch in rows.chunks(UPSERT_BATCH_SIZE) {
            let project_ids: Vec<&str> = batch.iter().map(|row| row.project_id.as_str()).collect();
            let file_paths: Vec<&str> = batch.iter().map(|row| row.file_path.as_str()).collect();
            let content_hashes: Vec<&str> =
                batch.iter().map(|row| row.content_hash.as_str()).collect();
            let modified_at: Vec<Option<String>> = batch
                .iter()
                .map(|row| row.modified_at.map(|ts| ts.to_rfc3339()))
                .collect();
            let file_sizes: Vec<Option<i64>> = batch.iter().map(|row| row.file_size).collect();
            let node_counts: Vec<i32> = batch.iter().map(|row| row.node_count).collect();
            let edge_counts: Vec<i32> = batch.iter().map(|row| row.edge_count).collect();
            let languages: Vec<Option<&str>> =
                batch.iter().map(|row| row.language.as_deref()).collect();
            let parse_errors: Vec<Option<&str>> = batch
                .iter()
                .map(|row| row.parse_errors.as_deref())
                .collect();

            merged += client
                .execute(
                    UPSERT_FILE_METADATA_SQL,
                    &[
                        &project_ids,
                        &file_paths,
                        &content_hashes,
                        &modified_at,
                        &file_sizes,
                        &node_counts,
                        &edge_counts,
                        &languages,
                        &parse_errors,
                    ],
                )
                .await
                .map_err(db_err)?;
        }
        Ok(merged)
    }

    /// Map `file_path -> content_hash` for one project, for the incremental "did this
    /// file change" check.
    pub async fn file_hashes(&self, project_id: &str) -> Result<HashMap<String, String>> {
        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(FILE_HASHES_SQL, &[&project_id])
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect())
    }

    /// Delete metadata rows for paths that no longer exist in the project.
    ///
    /// `paths` is not treated as authoritative when empty: an empty slice deletes
    /// nothing and returns `Ok(0)`.
    pub async fn delete_files(&self, project_id: &str, paths: &[String]) -> Result<u64> {
        if paths.is_empty() {
            return Ok(0);
        }

        let client = self.pool.get().await.map_err(db_err)?;
        let deleted = client
            .execute(DELETE_FILES_SQL, &[&project_id, &paths])
            .await
            .map_err(db_err)?;
        Ok(deleted)
    }

    /// Delete every row owned by the named files: edges touching their nodes, then
    /// symbol embeddings and chunks, then the nodes, then their file metadata.
    ///
    /// All five statements run in one transaction, ordered so dependants go before
    /// the nodes they reference. Returns the sum of the five affected-row counts.
    /// An empty slice deletes nothing and returns `Ok(0)`.
    pub async fn delete_data_for_files(
        &self,
        project_id: &str,
        file_paths: &[String],
    ) -> Result<u64> {
        if file_paths.is_empty() {
            return Ok(0);
        }

        let mut client = self.pool.get().await.map_err(db_err)?;
        let transaction = client.transaction().await.map_err(db_err)?;

        let node_rows = transaction
            .query(NODE_IDS_BY_FILE_SQL, &[&project_id, &file_paths])
            .await
            .map_err(db_err)?;
        let node_ids: Vec<String> = node_rows.iter().map(|row| row.get(0)).collect();

        let mut deleted = 0u64;
        if !node_ids.is_empty() {
            deleted += transaction
                .execute(DELETE_EDGES_BY_ENDPOINTS_SQL, &[&project_id, &node_ids])
                .await
                .map_err(db_err)?;
            deleted += transaction
                .execute(DELETE_STALE_SYMBOLS_SQL, &[&project_id, &node_ids])
                .await
                .map_err(db_err)?;
            deleted += transaction
                .execute(DELETE_STALE_CHUNKS_SQL, &[&project_id, &node_ids])
                .await
                .map_err(db_err)?;
            deleted += transaction
                .execute(DELETE_STALE_NODES_SQL, &[&project_id, &node_ids])
                .await
                .map_err(db_err)?;
        }
        deleted += transaction
            .execute(DELETE_FILES_SQL, &[&project_id, &file_paths])
            .await
            .map_err(db_err)?;

        transaction.commit().await.map_err(db_err)?;
        Ok(deleted)
    }

    /// Write node vectors by id. [`NodeEmbeddingRecord::column`] is ignored: the
    /// `nodes` table keeps a single `embedding_1024` vector column, so every row goes
    /// there regardless of the dimension-specific name SurrealDB would use. pgvector
    /// rejects vectors that are not 1024-dimensional.
    ///
    /// An empty slice returns `Ok(0)` without touching the database.
    pub async fn update_node_embeddings(&self, rows: &[NodeEmbeddingRecord]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }

        let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
        let embeddings: Vec<String> = rows.iter().map(|row| vector_text(&row.embedding)).collect();

        let client = self.pool.get().await.map_err(db_err)?;
        let updated = client
            .execute(UPDATE_NODE_EMBEDDINGS_SQL, &[&ids, &embeddings])
            .await
            .map_err(db_err)?;
        Ok(updated)
    }

    /// Insert or replace the project-level metadata row.
    ///
    /// `project_id` is the primary key, so re-indexing replaces the previous row
    /// instead of appending one. `last_analyzed` is supplied by the caller (unlike
    /// `file_metadata`, which stamps `now()`).
    pub async fn upsert_project_metadata(&self, record: &ProjectMetadataRecord) -> Result<()> {
        let last_analyzed = record.last_analyzed.to_rfc3339();
        let metadata = metadata_text(Some(&record.metadata))?.unwrap_or_else(|| "{}".to_string());

        let client = self.pool.get().await.map_err(db_err)?;
        client
            .execute(
                UPSERT_PROJECT_METADATA_SQL,
                &[
                    &record.project_id,
                    &record.name,
                    &record.root_path,
                    &record.primary_language,
                    &record.file_count,
                    &record.node_count,
                    &record.edge_count,
                    &record.avg_coverage_score,
                    &last_analyzed,
                    &record.codegraph_version,
                    &record.organization_id,
                    &record.domain,
                    &metadata,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Remove catalog rows that this run did not re-emit, for the whole project.
    ///
    /// Mirrors `SurrealDbStorage::reconcile_catalog_by_file` without its per-file
    /// `scope`: partial indexing is not wired up yet, so every file in the project is
    /// in scope.
    ///
    /// * nodes: files that disappeared entirely, plus ids missing from a surviving
    ///   file's fresh list;
    /// * edges: endpoints of stale nodes (deleted by the same statement), plus edges
    ///   whose id is absent from `fresh_edge_ids` while at least one endpoint lives in
    ///   a scanned file;
    /// * chunks: owned by stale nodes, plus chunks of surviving nodes whose ids are
    ///   absent from `fresh_chunk_ids`;
    /// * symbol embeddings: owned by stale nodes;
    /// * file metadata: rows whose file disappeared from the project entirely.
    ///
    /// Both `fresh_edge_ids` and `fresh_chunk_ids` are treated as authoritative only
    /// when non-empty: an empty set carries no information, so the corresponding diff
    /// is skipped instead of deleting everything.
    ///
    /// All deletes run in one transaction, ordered so dependants go before the nodes
    /// they reference.
    pub async fn reconcile(
        &self,
        project_id: &str,
        fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
        fresh_chunk_ids: &BTreeSet<String>,
        fresh_edge_ids: &BTreeSet<String>,
    ) -> Result<ReconcileStats> {
        let mut client = self.pool.get().await.map_err(db_err)?;

        // Step 1: current catalog, grouped in memory.
        let node_rows = client
            .query(RECONCILE_NODES_SQL, &[&project_id])
            .await
            .map_err(db_err)?;
        let mut existing_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut file_by_node: HashMap<String, String> = HashMap::new();
        for row in &node_rows {
            let id: String = row.get(0);
            let Some(file): Option<String> = row.get(1) else {
                continue;
            };
            file_by_node.insert(id.clone(), file.clone());
            existing_by_file.entry(file).or_default().push(id);
        }

        // Step 2: nodes the run did not re-emit.
        let stale_nodes = compute_stale_nodes(&existing_by_file, fresh_nodes_by_file);

        // Step 3: edges whose endpoints survived but which were not re-emitted.
        let stale_edges = if fresh_edge_ids.is_empty() {
            Vec::new()
        } else {
            let edge_rows = client
                .query(RECONCILE_EDGES_SQL, &[&project_id])
                .await
                .map_err(db_err)?;
            let edges: Vec<(String, String, String)> = edge_rows
                .iter()
                .filter_map(|row| {
                    let id: Option<String> = row.get(0);
                    Some((id?, row.get(1), row.get(2)))
                })
                .collect();
            compute_stale_edges(&edges, &file_by_node, fresh_nodes_by_file, fresh_edge_ids)
        };

        // Step 4: the "surviving parent, un-reemitted chunk" pass needs both a fresh
        // chunk set and the parents that were scanned.
        let fresh_node_ids: Vec<String> = fresh_nodes_by_file.values().flatten().cloned().collect();
        let has_unreemitted_chunk_pass = !fresh_chunk_ids.is_empty() && !fresh_node_ids.is_empty();

        // Step 5: files whose metadata must go because they no longer exist.
        let vanished_files = compute_vanished_files(&existing_by_file, fresh_nodes_by_file);

        if stale_nodes.is_empty()
            && stale_edges.is_empty()
            && !has_unreemitted_chunk_pass
            && vanished_files.is_empty()
        {
            return Ok(ReconcileStats::default());
        }

        let fresh_chunks: Vec<String> = fresh_chunk_ids.iter().cloned().collect();
        let transaction = client.transaction().await.map_err(db_err)?;
        let mut stats = ReconcileStats::default();

        stats.stale_edges = transaction
            .execute(
                DELETE_STALE_EDGES_SQL,
                &[&project_id, &stale_nodes, &stale_edges],
            )
            .await
            .map_err(db_err)?;
        stats.stale_symbols = transaction
            .execute(DELETE_STALE_SYMBOLS_SQL, &[&project_id, &stale_nodes])
            .await
            .map_err(db_err)?;
        stats.stale_chunks = transaction
            .execute(DELETE_STALE_CHUNKS_SQL, &[&project_id, &stale_nodes])
            .await
            .map_err(db_err)?;
        if has_unreemitted_chunk_pass {
            stats.stale_chunks += transaction
                .execute(
                    DELETE_UNREEMITTED_CHUNKS_SQL,
                    &[&project_id, &fresh_chunks, &fresh_node_ids],
                )
                .await
                .map_err(db_err)?;
        }
        stats.stale_nodes = transaction
            .execute(DELETE_STALE_NODES_SQL, &[&project_id, &stale_nodes])
            .await
            .map_err(db_err)?;
        if !vanished_files.is_empty() {
            stats.stale_files = transaction
                .execute(DELETE_FILES_SQL, &[&project_id, &vanished_files])
                .await
                .map_err(db_err)?;
        }

        transaction.commit().await.map_err(db_err)?;
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// Backend-neutral adapter
// ---------------------------------------------------------------------------

/// Pick the vector from whichever embedding column is populated. `1024` wins when
/// several are set (the pgvector schema declares `vector(1024)`), otherwise the
/// first populated column in ascending dimension order is used.
fn pick_embedding(
    embedding_384: &Option<Vec<f32>>,
    embedding_768: &Option<Vec<f32>>,
    embedding_1024: &Option<Vec<f32>>,
    embedding_1536: &Option<Vec<f32>>,
    embedding_2048: &Option<Vec<f32>>,
    embedding_2560: &Option<Vec<f32>>,
    embedding_3072: &Option<Vec<f32>>,
    embedding_4096: &Option<Vec<f32>>,
) -> Option<Vec<f32>> {
    if let Some(values) = embedding_1024 {
        return Some(values.clone());
    }
    [
        embedding_384,
        embedding_768,
        embedding_1536,
        embedding_2048,
        embedding_2560,
        embedding_3072,
        embedding_4096,
    ]
    .into_iter()
    .find_map(|values| values.clone())
}

/// Map a domain node onto a `nodes` row.
///
/// `project_id` comes from `metadata.attributes["project_id"]` (the same key
/// `SurrealDbStorage::node_to_surreal` reads). Nodes that were never annotated by
/// the indexer therefore map to an empty project id.
pub(crate) fn code_node_to_row(node: &CodeNode) -> NodeRow {
    let attributes = &node.metadata.attributes;
    NodeRow {
        id: node.id.to_string(),
        project_id: attributes.get("project_id").cloned().unwrap_or_default(),
        name: node.name.to_string(),
        node_type: node.node_type.as_ref().map(|value| format!("{:?}", value)),
        language: node.language.as_ref().map(|value| format!("{:?}", value)),
        file_path: Some(node.location.file_path.clone()),
        start_line: Some(node.location.line as i32),
        end_line: node.location.end_line.map(|line| line as i32),
        content: node.content.as_ref().map(|content| content.to_string()),
        complexity: node.complexity.map(|value| value as f64),
        chunk_count: attributes
            .get("chunk_count")
            .and_then(|value| value.parse::<i32>().ok()),
        embedding_model: attributes.get("embedding_model").cloned(),
        embedding: node.embedding.clone(),
        metadata: if attributes.is_empty() {
            None
        } else {
            serde_json::to_value(attributes).ok()
        },
    }
}

/// Map a domain edge onto an `edges` row.
pub(crate) fn code_edge_to_row(edge: &CodeEdge) -> EdgeRow {
    EdgeRow {
        id: edge.id.to_string(),
        project_id: edge.project_id.clone().unwrap_or_default(),
        from_id: edge.from.to_string(),
        to_id: edge.to.to_string(),
        edge_type: edge.edge_type.to_string(),
    }
}

/// Map a chunk record onto a `chunks` row.
///
/// The record's `text` may be base64-compressed by `ChunkEmbeddingRecord::new`;
/// Postgres stores plain text for its generated `content_tsv`, so it is
/// decompressed when possible. A record with no populated embedding yields an
/// empty vector: [`ChunkRow::embedding`] is not optional.
pub(crate) fn chunk_record_to_row(record: &ChunkEmbeddingRecord) -> ChunkRow {
    let embedding = pick_embedding(
        &record.embedding_384,
        &record.embedding_768,
        &record.embedding_1024,
        &record.embedding_1536,
        &record.embedding_2048,
        &record.embedding_2560,
        &record.embedding_3072,
        &record.embedding_4096,
    )
    .unwrap_or_default();

    ChunkRow {
        id: record.id.clone(),
        project_id: record.project_id.clone(),
        chunk_index: record.chunk_index,
        text: codegraph_core::decompress_string(&record.text)
            .unwrap_or_else(|_| record.text.clone()),
        parent_node: record.parent_node.clone(),
        embedding,
        embedding_model: record.embedding_model.clone(),
    }
}

/// Map a symbol-embedding record onto a `symbol_embeddings` row.
pub(crate) fn symbol_record_to_row(record: &SymbolEmbeddingRecord) -> SymbolEmbeddingRow {
    SymbolEmbeddingRow {
        id: record.id.clone(),
        project_id: record.project_id.clone(),
        symbol: record.symbol.clone(),
        normalized_symbol: record.normalized_symbol.clone(),
        embedding: pick_embedding(
            &record.embedding_384,
            &record.embedding_768,
            &record.embedding_1024,
            &record.embedding_1536,
            &record.embedding_2048,
            &record.embedding_2560,
            &record.embedding_3072,
            &record.embedding_4096,
        ),
        embedding_model: Some(record.embedding_model.clone()),
        node_id: record.node_id.clone(),
        source_edge_id: record.source_edge_id.clone(),
        metadata: record.metadata.clone(),
    }
}

/// Map a file-metadata record onto a `file_metadata` row.
///
/// `parse_errors` becomes a JSON string because the table stores it as `text`;
/// `last_indexed_at` is intentionally dropped (SQL `now()` stamps it).
pub(crate) fn file_metadata_record_to_row(record: &FileMetadataRecord) -> FileMetadataRow {
    FileMetadataRow {
        project_id: record.project_id.clone(),
        file_path: record.file_path.clone(),
        content_hash: record.content_hash.clone(),
        modified_at: Some(record.modified_at),
        file_size: Some(record.file_size),
        node_count: record.node_count as i32,
        edge_count: record.edge_count as i32,
        language: record.language.clone(),
        parse_errors: record
            .parse_errors
            .as_ref()
            .map(|errors| serde_json::to_string(errors).unwrap_or_else(|_| "[]".to_string())),
    }
}

/// Map reconciliation statistics onto the backend-neutral outcome.
pub(crate) fn reconcile_stats_to_outcome(stats: ReconcileStats) -> ReconcileOutcome {
    ReconcileOutcome {
        stale_nodes: stats.stale_nodes,
        stale_edges: stats.stale_edges,
        stale_chunks: stats.stale_chunks,
        stale_symbols: stats.stale_symbols,
        stale_files: stats.stale_files,
    }
}

/// Postgres implementation of [`crate::storage::IndexStorage`].
///
/// Writes go straight through to the existing `PgStore` methods; there is no
/// writer queue on this side yet, so batching/backpressure remains a caller
/// concern until a Postgres writer lands.
#[async_trait]
impl crate::storage::IndexStorage for PgStore {
    async fn upsert_nodes(&self, nodes: &[CodeNode]) -> anyhow::Result<u64> {
        let rows: Vec<NodeRow> = nodes.iter().map(code_node_to_row).collect();
        Ok(PgStore::upsert_nodes(self, &rows).await?)
    }

    async fn upsert_edges(&self, edges: &[CodeEdge]) -> anyhow::Result<u64> {
        let rows: Vec<EdgeRow> = edges.iter().map(code_edge_to_row).collect();
        Ok(PgStore::upsert_edges(self, &rows).await?)
    }

    async fn upsert_chunks(&self, chunks: &[ChunkEmbeddingRecord]) -> anyhow::Result<u64> {
        let rows: Vec<ChunkRow> = chunks.iter().map(chunk_record_to_row).collect();
        Ok(PgStore::upsert_chunks(self, &rows).await?)
    }

    async fn upsert_symbols(&self, symbols: &[SymbolEmbeddingRecord]) -> anyhow::Result<u64> {
        let rows: Vec<SymbolEmbeddingRow> = symbols.iter().map(symbol_record_to_row).collect();
        Ok(PgStore::upsert_symbol_embeddings(self, &rows).await?)
    }

    async fn upsert_file_metadata(&self, files: &[FileMetadataRecord]) -> anyhow::Result<u64> {
        let rows: Vec<FileMetadataRow> = files.iter().map(file_metadata_record_to_row).collect();
        Ok(PgStore::upsert_file_metadata(self, &rows).await?)
    }

    async fn update_node_embeddings(&self, rows: &[NodeEmbeddingRecord]) -> anyhow::Result<u64> {
        Ok(PgStore::update_node_embeddings(self, rows).await?)
    }

    async fn delete_data_for_files(
        &self,
        project_id: &str,
        file_paths: &[String],
    ) -> anyhow::Result<u64> {
        Ok(PgStore::delete_data_for_files(self, project_id, file_paths).await?)
    }

    async fn upsert_project_metadata(&self, record: ProjectMetadataRecord) -> anyhow::Result<()> {
        Ok(PgStore::upsert_project_metadata(self, &record).await?)
    }

    /// Whole-project reconciliation only.
    ///
    /// A `Some(scope)` is a deliberate, explicit failure rather than a silent
    /// whole-project pass: scoped deletes need `ReconcileScope`'s path
    /// normalization (relative paths, Windows separators, `..` handling), and a
    /// prefix guess would delete more than the scan covered.
    async fn reconcile(
        &self,
        project_id: &str,
        fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
        fresh_chunk_ids: &BTreeSet<String>,
        fresh_edge_ids: &BTreeSet<String>,
        scope: Option<&ReconcileScope>,
    ) -> anyhow::Result<ReconcileOutcome> {
        if scope.is_some() {
            return Err(CodeGraphError::Configuration(
                "scoped reconciliation is not implemented for the Postgres backend yet; \
                 run a full index instead of a partial one"
                    .to_string(),
            )
            .into());
        }
        let stats = PgStore::reconcile(
            self,
            project_id,
            fresh_nodes_by_file,
            fresh_chunk_ids,
            fresh_edge_ids,
        )
        .await?;
        Ok(reconcile_stats_to_outcome(stats))
    }

    async fn file_hashes(&self, project_id: &str) -> anyhow::Result<HashMap<String, String>> {
        Ok(PgStore::file_hashes(self, project_id).await?)
    }

    async fn node_count(&self, project_id: &str) -> anyhow::Result<i64> {
        Ok(PgStore::node_count(self, project_id).await?)
    }

    async fn edge_count(&self, project_id: &str) -> anyhow::Result<i64> {
        let client = self.pool.get().await.map_err(db_err)?;
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM edges WHERE project_id = $1",
                &[&project_id],
            )
            .await
            .map_err(db_err)?
            .get(0);
        Ok(count)
    }

    async fn chunk_count(&self, project_id: &str) -> anyhow::Result<i64> {
        let client = self.pool.get().await.map_err(db_err)?;
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM chunks WHERE project_id = $1",
                &[&project_id],
            )
            .await
            .map_err(db_err)?
            .get(0);
        Ok(count)
    }

    async fn knn_chunks(
        &self,
        project_id: &str,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        Ok(PgStore::knn_chunks(self, project_id, query, k).await?)
    }

    async fn reverse_deps(
        &self,
        project_id: &str,
        from_node: &str,
        depth: u32,
    ) -> anyhow::Result<Vec<String>> {
        Ok(PgStore::reverse_deps(self, project_id, from_node, depth).await?)
    }

    /// Returns `metadata->>'input_fingerprint'`. A missing row or a NULL key both
    /// surface as `None`.
    async fn project_fingerprint(&self, project_id: &str) -> anyhow::Result<Option<String>> {
        let client = self.pool.get().await.map_err(db_err)?;
        let row = client
            .query_opt(PROJECT_FINGERPRINT_SQL, &[&project_id])
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|row| row.get::<_, Option<String>>(0)))
    }

    async fn delete_project_metadata(&self, project_id: &str) -> anyhow::Result<()> {
        let client = self.pool.get().await.map_err(db_err)?;
        client
            .execute(DELETE_PROJECT_METADATA_SQL, &[&project_id])
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn has_project_metadata(&self, project_id: &str) -> anyhow::Result<bool> {
        let client = self.pool.get().await.map_err(db_err)?;
        let present: bool = client
            .query_one(HAS_PROJECT_METADATA_SQL, &[&project_id])
            .await
            .map_err(db_err)?
            .get(0);
        Ok(present)
    }

    /// Rows are ordered by `file_path` so callers receive a deterministic list.
    ///
    /// `modified_at` and `last_indexed_at` are nullable in the table but not in
    /// [`FileMetadataRecord`], so a NULL timestamp is filled with `Utc::now()`.
    /// `parse_errors` is stored as JSON text; a value that fails to parse is
    /// returned as `Some(vec![raw])` so the original text is not lost.
    async fn file_metadata(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<crate::storage::FileMetadataRecord>> {
        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(FILE_METADATA_SQL, &[&project_id])
            .await
            .map_err(db_err)?;

        let parse_timestamp = |text: &str| {
            chrono::DateTime::parse_from_rfc3339(text)
                .map(|value| value.with_timezone(&chrono::Utc))
                .map_err(|error| {
                    CodeGraphError::Database(format!("invalid timestamp {text:?}: {error}"))
                })
        };

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let modified_at = match row.get::<_, Option<String>>(2) {
                Some(text) => parse_timestamp(&text)?,
                None => chrono::Utc::now(),
            };
            let last_indexed_at = match row.get::<_, Option<String>>(4) {
                Some(text) => parse_timestamp(&text)?,
                None => chrono::Utc::now(),
            };
            let parse_errors = match row.get::<_, Option<String>>(8) {
                Some(raw) => {
                    Some(serde_json::from_str::<Vec<String>>(&raw).unwrap_or_else(|_| vec![raw]))
                }
                None => None,
            };
            records.push(crate::storage::FileMetadataRecord {
                file_path: row.get(0),
                project_id: project_id.to_string(),
                content_hash: row.get(1),
                modified_at,
                file_size: row.get::<_, Option<i64>>(3).unwrap_or_default(),
                last_indexed_at,
                node_count: row.get::<_, i32>(5) as i64,
                edge_count: row.get::<_, i32>(6) as i64,
                language: row.get(7),
                parse_errors,
            });
        }
        Ok(records)
    }

    /// Unlike SurrealDB, Postgres stores node ids as bare strings, so no
    /// `nodes:` prefix or quoting has to be stripped here. A NULL `file_path`
    /// becomes an empty string; the `ANY($2)` predicate already excludes it.
    async fn nodes_in_files(
        &self,
        project_id: &str,
        files: &[String],
    ) -> anyhow::Result<Vec<crate::storage::NodeInFile>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        let client = self.pool.get().await.map_err(db_err)?;
        let rows = client
            .query(NODES_IN_FILES_SQL, &[&project_id, &files])
            .await
            .map_err(db_err)?;

        Ok(rows
            .into_iter()
            .map(|row| crate::storage::NodeInFile {
                id: row.get(0),
                file_path: row.get::<_, Option<String>>(1).unwrap_or_default(),
                name: row.get(2),
                node_type: row.get(3),
            })
            .collect())
    }

    /// Returns the number of edges whose `to_id` moved.
    async fn retarget_edges(&self, project_id: &str, old: &str, new: &str) -> anyhow::Result<u64> {
        let client = self.pool.get().await.map_err(db_err)?;
        Ok(client
            .execute(RETARGET_EDGES_SQL, &[&project_id, &old, &new])
            .await
            .map_err(db_err)?)
    }

    /// Always `true`: the HNSW indexes are built by [`SCHEMA_DDL`], so there is
    /// nothing to check at runtime. `dimension` and `tables` are ignored because
    /// this backend keeps a single fixed-dimension `embedding_1024` column.
    async fn ensure_vector_indexes(
        &self,
        _dimension: usize,
        _tables: &[String],
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_text_format_is_bracket_and_no_spaces() {
        assert_eq!(vector_text(&[0.1, 0.25, -1.5]), "[0.1,0.25,-1.5]");
    }

    #[test]
    fn vector_text_rounds_to_six_decimals() {
        let value = 0.123456789_f64 as f32;
        assert_eq!(vector_text(&[value]), "[0.123457]");
    }

    #[test]
    fn schema_ddl_is_idempotent() {
        let mut table_count = 0;
        let mut index_count = 0;
        for line in SCHEMA_DDL.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("CREATE TABLE") {
                table_count += 1;
                assert!(
                    trimmed.starts_with("CREATE TABLE IF NOT EXISTS"),
                    "table DDL lacks IF NOT EXISTS: {trimmed}"
                );
            } else if trimmed.starts_with("CREATE INDEX") {
                index_count += 1;
                assert!(
                    trimmed.starts_with("CREATE INDEX IF NOT EXISTS"),
                    "index DDL lacks IF NOT EXISTS: {trimmed}"
                );
            } else if trimmed.starts_with("CREATE EXTENSION") {
                assert!(
                    trimmed.starts_with("CREATE EXTENSION IF NOT EXISTS"),
                    "extension DDL lacks IF NOT EXISTS: {trimmed}"
                );
            }
        }
        assert_eq!(
            table_count, 6,
            "expected chunks + edges + nodes + symbol_embeddings + file_metadata + \
             project_metadata tables"
        );
        assert_eq!(
            index_count, 17,
            "expected 17 chunks/edges/nodes/symbol/file_metadata indexes \
             (project_metadata is keyed by its primary key and needs no extra index)"
        );
    }

    #[test]
    fn pg_store_implements_index_storage() {
        fn assert_impl<T: crate::storage::IndexStorage>() {}
        assert_impl::<PgStore>();
    }

    #[test]
    fn nodes_ddl_covers_all_indexes() {
        for index in [
            "nodes_project_file",
            "nodes_hnsw",
            "nodes_tsv",
            "sym_project_norm",
            "sym_hnsw",
        ] {
            assert!(
                SCHEMA_DDL.contains(&format!("CREATE INDEX IF NOT EXISTS {index} ")),
                "SCHEMA_DDL is missing index {index}"
            );
        }
        assert!(SCHEMA_DDL.contains("embedding_1024  vector(1024)"));
        assert!(SCHEMA_DDL.contains("GENERATED ALWAYS AS"));
    }

    #[test]
    fn node_row_metadata_roundtrip() {
        let original = serde_json::json!({"a": 1, "nested": {"b": [true, null, "x"]}});
        let text = metadata_text(Some(&original))
            .expect("serialise metadata")
            .expect("metadata is Some");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse metadata");
        assert_eq!(parsed, original);
        assert!(metadata_text(None).expect("none").is_none());
    }

    #[test]
    fn vector_text_reused_for_nodes() {
        let embedding = vec![0.5f32];
        assert_eq!(
            embedding_text(Some(embedding.as_slice())),
            Some("[0.5]".to_string())
        );
        assert_eq!(embedding_text(None), None);
        assert_eq!(parse_vector_text("[0.5]").expect("parse"), vec![0.5f32]);
    }

    // ---- Reconciliation: pure helpers ----

    fn fresh_map(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(file, ids)| {
                (
                    file.to_string(),
                    ids.iter().map(|id| id.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn stale_nodes_covers_deleted_and_shrunk_files() {
        let existing = fresh_map(&[("a.rs", &["a1", "a2"]), ("b.rs", &["b1"])]);
        let fresh = fresh_map(&[("a.rs", &["a1"])]);

        let mut stale = compute_stale_nodes(&existing, &fresh);
        stale.sort();

        assert_eq!(
            stale,
            vec!["a2".to_string(), "b1".to_string()],
            "a shrunk file keeps only its re-emitted ids; a vanished file loses all of them"
        );
    }

    #[test]
    fn vanished_files_is_difference_of_keys() {
        let existing = fresh_map(&[("a.rs", &["a1"]), ("b.rs", &["b1"]), ("c.rs", &["c1"])]);
        let fresh = fresh_map(&[("a.rs", &["a1"]), ("c.rs", &["c1"])]);

        assert_eq!(
            compute_vanished_files(&existing, &fresh),
            vec!["b.rs".to_string()],
            "only files missing from the fresh scan are vanished"
        );
    }

    #[test]
    fn file_metadata_ddl_uses_composite_key() {
        assert!(
            SCHEMA_DDL.contains("PRIMARY KEY (project_id, file_path)"),
            "file_metadata must be keyed per project: paths repeat across projects"
        );
    }

    #[test]
    fn stale_edges_skipped_when_fresh_set_empty() {
        let file_by_node: HashMap<String, String> = [
            ("a".to_string(), "a.rs".to_string()),
            ("b".to_string(), "a.rs".to_string()),
        ]
        .into_iter()
        .collect();
        let fresh = fresh_map(&[("a.rs", &["a", "b"])]);
        let edges = vec![("e1".to_string(), "a".to_string(), "b".to_string())];

        let stale = compute_stale_edges(&edges, &file_by_node, &fresh, &BTreeSet::new());

        assert!(
            stale.is_empty(),
            "no fresh edge ids means no information; deleting on that inference would wipe the table"
        );
    }

    #[test]
    fn stale_edges_includes_live_endpoint_when_not_reemitted() {
        let file_by_node: HashMap<String, String> = [
            ("a".to_string(), "a.rs".to_string()),
            ("b".to_string(), "a.rs".to_string()),
        ]
        .into_iter()
        .collect();
        let fresh = fresh_map(&[("a.rs", &["a", "b"])]);
        let fresh_edges: BTreeSet<String> = ["e_kept".to_string()].into_iter().collect();
        let edges = vec![
            ("e_kept".to_string(), "a".to_string(), "b".to_string()),
            ("e_dropped".to_string(), "a".to_string(), "b".to_string()),
        ];

        let stale = compute_stale_edges(&edges, &file_by_node, &fresh, &fresh_edges);

        assert_eq!(
            stale,
            vec!["e_dropped".to_string()],
            "an edge with live endpoints that this run did not re-emit is stale"
        );
    }

    #[test]
    fn stale_edges_excludes_edges_on_unscanned_files() {
        let file_by_node: HashMap<String, String> = [
            ("c".to_string(), "c.rs".to_string()),
            ("d".to_string(), "d.rs".to_string()),
        ]
        .into_iter()
        .collect();
        let fresh = fresh_map(&[("a.rs", &["a"])]);
        let fresh_edges: BTreeSet<String> = ["e_elsewhere".to_string()].into_iter().collect();
        let edges = vec![("e_unscanned".to_string(), "c".to_string(), "d".to_string())];

        let stale = compute_stale_edges(&edges, &file_by_node, &fresh, &fresh_edges);

        assert!(
            stale.is_empty(),
            "endpoints in files this run did not scan may belong to another scope"
        );
    }

    // ---- Node retrieval: tsquery construction ----

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn tsquery_prefix_lowercases_and_camel_splits() {
        // The name is kept for traceability with the spec; the implementation does
        // NOT split camelCase, so `HandleFile` stays one lowercase term.
        assert_eq!(
            tsquery_prefix("HandleFile"),
            Some("handlefile:*".to_string())
        );
        assert_eq!(
            tsquery_prefix("handle_file"),
            Some("handle_file:*".to_string()),
            "underscores survive the split"
        );
    }

    #[test]
    fn tsquery_prefix_multiple_terms() {
        assert_eq!(
            tsquery_prefix("parse error"),
            Some("parse:* & error:*".to_string())
        );
        assert_eq!(
            tsquery_prefix("  Parse\tERROR\n"),
            Some("parse:* & error:*".to_string()),
            "whitespace and case are normalised"
        );
    }

    #[test]
    fn tsquery_prefix_rejects_empty_and_symbols() {
        assert_eq!(tsquery_prefix(""), None);
        assert_eq!(tsquery_prefix("!!!"), None);
        assert_eq!(tsquery_prefix("   \t\n"), None);

        // Terms longer than MAX_TSQUERY_TERM_LEN are dropped rather than truncated.
        let long = "a".repeat(MAX_TSQUERY_TERM_LEN + 1);
        assert_eq!(tsquery_prefix(&long), None);
        let at_limit = "a".repeat(MAX_TSQUERY_TERM_LEN);
        assert_eq!(
            tsquery_prefix(&format!("{at_limit} ok")),
            Some(format!("{at_limit}:* & ok:*"))
        );
    }

    #[test]
    fn tsquery_prefix_strips_unsafe_chars() {
        assert_eq!(
            tsquery_prefix("a;b'"),
            Some("a:* & b:*".to_string()),
            "punctuation splits terms, so no tsquery operator can survive"
        );
        assert_eq!(
            tsquery_prefix("a&b|c!d(e)f"),
            Some("a:* & b:* & c:* & d:* & e:* & f:*".to_string())
        );
        // Non-ASCII alphanumerics split out entirely rather than reaching tsquery.
        assert_eq!(tsquery_prefix("名字"), None);
    }

    // ---- Node retrieval: RRF fusion ----

    #[test]
    fn rrf_prefers_items_in_both_lists() {
        let vector = ids(&["a", "b", "c"]);
        let text = ids(&["b", "a", "d"]);

        let hits = rrf_fuse(&vector, &text, 0);

        let order: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["a", "b", "c", "d"],
            "a and b appear in both lists; c and d in one each"
        );

        let both = 1.0 / 61.0 + 1.0 / 62.0;
        assert!((hits[0].rrf - both).abs() < 1e-12);
        assert!((hits[1].rrf - both).abs() < 1e-12);
        assert!((hits[2].rrf - 1.0 / 63.0).abs() < 1e-12);
        assert!((hits[3].rrf - 1.0 / 63.0).abs() < 1e-12);

        assert_eq!(hits[0].vector_rank, Some(1));
        assert_eq!(hits[0].text_rank, Some(2));
        assert_eq!(hits[2].vector_rank, Some(3));
        assert_eq!(hits[2].text_rank, None, "c is vector-only");
        assert_eq!(hits[3].vector_rank, None, "d is text-only");
        assert_eq!(hits[3].text_rank, Some(3));
    }

    #[test]
    fn rrf_dedupes_within_a_list() {
        let vector = ids(&["a", "a", "b"]);

        let hits = rrf_fuse(&vector, &[], 0);

        assert_eq!(hits.len(), 2, "a is counted once");
        let a = hits.iter().find(|hit| hit.id == "a").expect("a present");
        assert!(
            (a.rrf - 1.0 / 61.0).abs() < 1e-12,
            "a must score one first-place contribution, not two"
        );
        assert_eq!(a.vector_rank, Some(1));
        let b = hits.iter().find(|hit| hit.id == "b").expect("b present");
        assert_eq!(b.vector_rank, Some(3), "rank is the list position");
    }

    #[test]
    fn rrf_limits_and_is_deterministic() {
        let vector = ids(&["b", "a"]);
        let text = ids(&["a", "b"]);

        let all = rrf_fuse(&vector, &text, 0);
        assert_eq!(
            all.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "equal scores break ties by id ascending"
        );

        let limited = rrf_fuse(&vector, &text, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, "a");
    }

    #[test]
    fn rrf_empty_inputs() {
        assert!(rrf_fuse(&[], &[], 0).is_empty());
        assert!(rrf_fuse(&[], &[], 5).is_empty());
    }

    // ---- Integration tests (require CODEGRAPH_PG_URL, run with --ignored) ----

    fn store_or_skip() -> Option<PgStore> {
        if std::env::var("CODEGRAPH_PG_URL").is_err() {
            return None;
        }
        let store = PgStore::connect("").expect("connect to CODEGRAPH_PG_URL");
        Some(store)
    }

    fn project_id(tag: &str) -> String {
        // Unique per test *and* per run: a fixed id would collide on the second run
        // of the suite against the same database (primary-key conflicts on edges).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("poc_{tag}_{}_{nanos}_{n}", std::process::id())
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reconcile_rejects_scope_on_postgres() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        use crate::storage::IndexStorage;
        let project = project_id("scoped_reconcile");
        let scope = ReconcileScope {
            project_root: std::path::PathBuf::from("/repo"),
            scan_root: std::path::PathBuf::from("/repo/packages/core"),
        };
        let error = IndexStorage::reconcile(
            &store,
            &project,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some(&scope),
        )
        .await
        .expect_err("a scoped reconciliation must be rejected on the Postgres backend");
        let message = error.to_string();
        assert!(
            message.contains("scoped reconciliation is not implemented"),
            "unexpected error message: {message}"
        );
    }

    fn unit_vector(index: usize) -> Vec<f32> {
        let mut vector = vec![0.0f32; 1024];
        vector[index] = 1.0;
        vector
    }

    fn chunk(id: &str, project_id: &str, embedding: Vec<f32>) -> ChunkRow {
        ChunkRow {
            id: id.to_string(),
            project_id: project_id.to_string(),
            chunk_index: 0,
            text: format!("text for {id}"),
            parent_node: "node".to_string(),
            embedding,
            embedding_model: "test-model".to_string(),
        }
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn schema_roundtrip_and_knn() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("knn");
        let rows = vec![
            chunk("knn_a", &project, unit_vector(0)),
            chunk("knn_b", &project, unit_vector(1)),
            chunk("knn_c", &project, unit_vector(2)),
        ];
        store.upsert_chunks(&rows).await.expect("upsert_chunks");

        let hits = store
            .knn_chunks(&project, &unit_vector(0), 3)
            .await
            .expect("knn_chunks");
        assert_eq!(hits.len(), 3, "expected three neighbours");
        assert_eq!(hits[0].0, "knn_a");
        assert!(hits[0].1 <= hits[1].1 && hits[1].1 <= hits[2].1);
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reverse_deps_walks_depth() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("deps");
        let client = store.pool.get().await.expect("pool get");
        client
            .execute(
                "INSERT INTO edges (id, project_id, from_id, to_id, edge_type) VALUES ($1,$2,$3,$4,$5)",
                &[&format!("{project}_dep_ab"), &project, &"A", &"B", &DEFAULT_EDGE_TYPE],
            )
            .await
            .expect("insert A->B");
        client
            .execute(
                "INSERT INTO edges (id, project_id, from_id, to_id, edge_type) VALUES ($1,$2,$3,$4,$5)",
                &[&format!("{project}_dep_bc"), &project, &"B", &"C", &DEFAULT_EDGE_TYPE],
            )
            .await
            .expect("insert B->C");

        let mut deps = store
            .reverse_deps(&project, "C", 2)
            .await
            .expect("reverse_deps");
        deps.sort();
        assert_eq!(deps, vec!["A".to_string(), "B".to_string()]);
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn upsert_chunks_is_idempotent() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("upsert");
        let first = chunk("upsert_a", &project, unit_vector(0));
        store
            .upsert_chunks(std::slice::from_ref(&first))
            .await
            .expect("first upsert");

        let mut second = first.clone();
        second.text = "updated text".to_string();
        second.embedding = unit_vector(3);
        store
            .upsert_chunks(std::slice::from_ref(&second))
            .await
            .expect("second upsert");

        let client = store.pool.get().await.expect("pool get");
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM chunks WHERE project_id = $1",
                &[&project],
            )
            .await
            .expect("count")
            .get(0);
        assert_eq!(count, 1, "upsert must not duplicate rows");

        let text: String = client
            .query_one("SELECT text FROM chunks WHERE id = $1", &[&"upsert_a"])
            .await
            .expect("select")
            .get(0);
        assert_eq!(text, "updated text");
    }

    fn node(id: &str, project_id: &str) -> NodeRow {
        NodeRow {
            id: id.to_string(),
            project_id: project_id.to_string(),
            name: format!("name_{id}"),
            node_type: Some("function".to_string()),
            language: Some("rust".to_string()),
            file_path: Some("src/lib.rs".to_string()),
            start_line: Some(10),
            end_line: Some(20),
            content: Some(format!("content for {id}")),
            complexity: Some(1.5),
            chunk_count: Some(0),
            embedding_model: None,
            embedding: None,
            metadata: None,
        }
    }

    fn symbol_embedding(
        id: &str,
        project_id: &str,
        normalized_symbol: &str,
        embedding: Vec<f32>,
    ) -> SymbolEmbeddingRow {
        SymbolEmbeddingRow {
            id: id.to_string(),
            project_id: project_id.to_string(),
            symbol: format!("symbol_{id}"),
            normalized_symbol: normalized_symbol.to_string(),
            embedding: Some(embedding),
            embedding_model: Some("test-model".to_string()),
            node_id: Some("node".to_string()),
            source_edge_id: None,
            metadata: Some(serde_json::json!({"source": "test"})),
        }
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn nodes_roundtrip_and_count() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("nodes");
        let mut rich = node("node_rich", &project);
        rich.embedding = Some(unit_vector(0));
        rich.embedding_model = Some("test-model".to_string());
        rich.metadata = Some(serde_json::json!({"kind": "test", "score": 2}));

        let rows = vec![
            node("node_plain", &project),
            rich.clone(),
            node("node_third", &project),
        ];
        let merged = store.upsert_nodes(&rows).await.expect("upsert_nodes");
        assert_eq!(merged, 3);
        assert_eq!(store.node_count(&project).await.expect("node_count"), 3);

        let ids = vec!["node_rich".to_string(), "node_plain".to_string()];
        let fetched = store
            .nodes_by_ids(&project, &ids)
            .await
            .expect("nodes_by_ids");
        assert_eq!(fetched.len(), 2, "expected exactly the requested nodes");

        let found = fetched
            .iter()
            .find(|row| row.id == "node_rich")
            .expect("rich node present");
        assert_eq!(found.project_id, project);
        assert_eq!(found.name, rich.name);
        assert_eq!(found.node_type, rich.node_type);
        assert_eq!(found.language, rich.language);
        assert_eq!(found.file_path, rich.file_path);
        assert_eq!(found.start_line, rich.start_line);
        assert_eq!(found.end_line, rich.end_line);
        assert_eq!(found.content, rich.content);
        assert_eq!(found.complexity, rich.complexity);
        assert_eq!(found.chunk_count, rich.chunk_count);
        assert_eq!(found.embedding_model, rich.embedding_model);
        assert_eq!(found.embedding, rich.embedding);
        assert_eq!(found.metadata, rich.metadata);

        let foreign = store
            .nodes_by_ids("some_other_project", &ids)
            .await
            .expect("nodes_by_ids for other project");
        assert!(foreign.is_empty(), "nodes_by_ids must filter by project_id");
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn nodes_upsert_is_idempotent() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("node_upsert");
        let first = node("node_upsert_a", &project);
        store
            .upsert_nodes(std::slice::from_ref(&first))
            .await
            .expect("first upsert");

        let mut second = first.clone();
        second.content = Some("updated content".to_string());
        second.embedding = Some(unit_vector(1));
        store
            .upsert_nodes(std::slice::from_ref(&second))
            .await
            .expect("second upsert");

        assert_eq!(
            store.node_count(&project).await.expect("node_count"),
            1,
            "upsert must not duplicate rows"
        );

        let fetched = store
            .nodes_by_ids(&project, &["node_upsert_a".to_string()])
            .await
            .expect("nodes_by_ids");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].content.as_deref(), Some("updated content"));
        assert_eq!(fetched[0].embedding, second.embedding);
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn symbol_embeddings_upsert_and_lookup() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("symbols");
        let rows = vec![
            symbol_embedding("sym_a", &project, "call", unit_vector(4)),
            symbol_embedding("sym_b", &project, "search", unit_vector(5)),
        ];
        let merged = store
            .upsert_symbol_embeddings(&rows)
            .await
            .expect("first upsert");
        assert_eq!(merged, 2);

        let repeat = symbol_embedding("sym_a", &project, "call", unit_vector(6));
        store
            .upsert_symbol_embeddings(std::slice::from_ref(&repeat))
            .await
            .expect("second upsert");

        let client = store.pool.get().await.expect("pool get");
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM symbol_embeddings WHERE project_id = $1",
                &[&project],
            )
            .await
            .expect("count")
            .get(0);
        assert_eq!(count, 2, "re-upsert must not duplicate rows");

        let normalized: Option<String> = client
            .query_one(
                "SELECT normalized_symbol FROM symbol_embeddings
                  WHERE project_id = $1 AND normalized_symbol = $2 AND id = $3",
                &[&project, &"call", &"sym_a"],
            )
            .await
            .expect("lookup by normalized_symbol")
            .get(0);
        assert_eq!(normalized.as_deref(), Some("call"));
    }

    fn node_in(id: &str, project_id: &str, file: &str) -> NodeRow {
        let mut row = node(id, project_id);
        row.file_path = Some(file.to_string());
        row
    }

    fn edge_row(id: &str, project_id: &str, from_id: &str, to_id: &str) -> EdgeRow {
        EdgeRow {
            id: id.to_string(),
            project_id: project_id.to_string(),
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            edge_type: DEFAULT_EDGE_TYPE.to_string(),
        }
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reconcile_removes_vanished_file_and_shrunk_nodes() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("recon_nodes");
        store
            .upsert_nodes(&[
                node_in("recon_a1", &project, "a.rs"),
                node_in("recon_a2", &project, "a.rs"),
                node_in("recon_b1", &project, "b.rs"),
            ])
            .await
            .expect("upsert nodes");

        let fresh = fresh_map(&[("a.rs", &["recon_a1"])]);
        let stats = store
            .reconcile(&project, &fresh, &BTreeSet::new(), &BTreeSet::new())
            .await
            .expect("reconcile");

        assert_eq!(stats.stale_nodes, 2, "both a2 and the b.rs node are stale");
        assert_eq!(
            store.node_count(&project).await.expect("node_count"),
            1,
            "only the re-emitted node survives"
        );
        let remaining = store
            .nodes_by_ids(
                &project,
                &[
                    "recon_a1".to_string(),
                    "recon_a2".to_string(),
                    "recon_b1".to_string(),
                ],
            )
            .await
            .expect("nodes_by_ids");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "recon_a1");
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reconcile_removes_edge_with_live_endpoints() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("recon_edges");
        store
            .upsert_nodes(&[
                node_in("recon_ea", &project, "a.rs"),
                node_in("recon_eb", &project, "a.rs"),
            ])
            .await
            .expect("upsert nodes");
        store
            .upsert_edges(&[
                edge_row(
                    &format!("{project}_e_keep"),
                    &project,
                    "recon_ea",
                    "recon_eb",
                ),
                edge_row(
                    &format!("{project}_e_stale"),
                    &project,
                    "recon_ea",
                    "recon_eb",
                ),
            ])
            .await
            .expect("upsert edges");

        let fresh_nodes = fresh_map(&[("a.rs", &["recon_ea", "recon_eb"])]);
        let fresh_edges: BTreeSet<String> = [format!("{project}_e_keep")].into_iter().collect();
        let stats = store
            .reconcile(&project, &fresh_nodes, &BTreeSet::new(), &fresh_edges)
            .await
            .expect("reconcile");

        assert_eq!(
            stats.stale_edges, 1,
            "the edge whose id was not re-emitted is stale even though both endpoints survive"
        );
        let client = store.pool.get().await.expect("pool get");
        let ids: Vec<String> = client
            .query(
                "SELECT id FROM edges WHERE project_id = $1 ORDER BY id",
                &[&project],
            )
            .await
            .expect("select edges")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(ids, vec![format!("{project}_e_keep")]);
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reconcile_cascades_to_chunks_and_symbols() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("recon_cascade");
        store
            .upsert_nodes(&[
                node_in("recon_doomed", &project, "b.rs"),
                node_in("recon_kept", &project, "a.rs"),
            ])
            .await
            .expect("upsert nodes");

        let mut doomed_chunk = chunk("recon_chunk_doomed", &project, unit_vector(0));
        doomed_chunk.parent_node = "recon_doomed".to_string();
        let mut kept_chunk = chunk("recon_chunk_kept", &project, unit_vector(1));
        kept_chunk.parent_node = "recon_kept".to_string();
        store
            .upsert_chunks(&[doomed_chunk, kept_chunk])
            .await
            .expect("upsert chunks");

        let mut doomed_symbol =
            symbol_embedding("recon_sym_doomed", &project, "call", unit_vector(2));
        doomed_symbol.node_id = Some("recon_doomed".to_string());
        let mut kept_symbol = symbol_embedding("recon_sym_kept", &project, "call", unit_vector(3));
        kept_symbol.node_id = Some("recon_kept".to_string());
        store
            .upsert_symbol_embeddings(&[doomed_symbol, kept_symbol])
            .await
            .expect("upsert symbols");

        let fresh = fresh_map(&[("a.rs", &["recon_kept"])]);
        let stats = store
            .reconcile(&project, &fresh, &BTreeSet::new(), &BTreeSet::new())
            .await
            .expect("reconcile");

        assert_eq!(stats.stale_nodes, 1);
        assert_eq!(stats.stale_chunks, 1);
        assert_eq!(stats.stale_symbols, 1);

        let client = store.pool.get().await.expect("pool get");
        let chunk_ids: Vec<String> = client
            .query(
                "SELECT id FROM chunks WHERE project_id = $1 ORDER BY id",
                &[&project],
            )
            .await
            .expect("select chunks")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(chunk_ids, vec!["recon_chunk_kept".to_string()]);

        let symbol_ids: Vec<String> = client
            .query(
                "SELECT id FROM symbol_embeddings WHERE project_id = $1 ORDER BY id",
                &[&project],
            )
            .await
            .expect("select symbols")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(symbol_ids, vec!["recon_sym_kept".to_string()]);
    }

    // ---- File metadata ----

    fn file_meta(project: &str, path: &str, hash: &str) -> FileMetadataRow {
        FileMetadataRow {
            project_id: project.to_string(),
            file_path: path.to_string(),
            content_hash: hash.to_string(),
            modified_at: None,
            file_size: Some(42),
            node_count: 1,
            edge_count: 0,
            language: Some("rust".to_string()),
            parse_errors: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn file_metadata_upsert_and_hashes() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("file_meta");
        store
            .upsert_file_metadata(&[
                file_meta(&project, "src/a.rs", "hash_a1"),
                file_meta(&project, "src/b.rs", "hash_b1"),
            ])
            .await
            .expect("first upsert");

        let hashes = store.file_hashes(&project).await.expect("file_hashes");
        assert_eq!(hashes.len(), 2, "both metadata rows are visible");
        assert_eq!(hashes.get("src/a.rs").map(String::as_str), Some("hash_a1"));
        assert_eq!(hashes.get("src/b.rs").map(String::as_str), Some("hash_b1"));

        store
            .upsert_file_metadata(&[file_meta(&project, "src/a.rs", "hash_a2")])
            .await
            .expect("second upsert");

        let hashes = store.file_hashes(&project).await.expect("file_hashes");
        assert_eq!(hashes.len(), 2, "re-upsert must not duplicate rows");
        assert_eq!(hashes.get("src/a.rs").map(String::as_str), Some("hash_a2"));

        let client = store.pool.get().await.expect("pool get");
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM file_metadata WHERE project_id = $1",
                &[&project],
            )
            .await
            .expect("count")
            .get(0);
        assert_eq!(count, 2);
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn delete_files_removes_only_given_paths() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("file_delete");
        store
            .upsert_file_metadata(&[
                file_meta(&project, "src/a.rs", "hash_a"),
                file_meta(&project, "src/b.rs", "hash_b"),
                file_meta(&project, "src/c.rs", "hash_c"),
            ])
            .await
            .expect("upsert");

        let deleted = store
            .delete_files(&project, &["src/b.rs".to_string()])
            .await
            .expect("delete_files");
        assert_eq!(deleted, 1);

        let hashes = store.file_hashes(&project).await.expect("file_hashes");
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains_key("src/a.rs"));
        assert!(hashes.contains_key("src/c.rs"));
        assert!(!hashes.contains_key("src/b.rs"));
    }

    fn project_meta(project: &str, name: &str, node_count: i64) -> ProjectMetadataRecord {
        ProjectMetadataRecord {
            metadata: serde_json::json!({ "name": name }),
            project_id: project.to_string(),
            name: name.to_string(),
            root_path: format!("/tmp/{name}"),
            primary_language: Some("rust".to_string()),
            file_count: 1,
            node_count,
            edge_count: 0,
            avg_coverage_score: 0.5,
            last_analyzed: chrono::Utc::now(),
            codegraph_version: "test".to_string(),
            organization_id: None,
            domain: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn project_metadata_upsert_and_replace() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("project_meta");
        store
            .upsert_project_metadata(&project_meta(&project, "first", 3))
            .await
            .expect("first upsert");

        let client = store.pool.get().await.expect("pool get");
        let row = client
            .query_one(
                "SELECT name, root_path, node_count, metadata::text
                   FROM project_metadata WHERE project_id = $1",
                &[&project],
            )
            .await
            .expect("select");
        assert_eq!(row.get::<_, String>(0), "first");
        assert_eq!(row.get::<_, String>(1), "/tmp/first");
        assert_eq!(row.get::<_, i64>(2), 3);
        let metadata: serde_json::Value =
            serde_json::from_str(&row.get::<_, String>(3)).expect("metadata is JSON");
        assert_eq!(metadata, serde_json::json!({ "name": "first" }));

        store
            .upsert_project_metadata(&project_meta(&project, "second", 7))
            .await
            .expect("second upsert");

        let rows = client
            .query(
                "SELECT name, node_count FROM project_metadata WHERE project_id = $1",
                &[&project],
            )
            .await
            .expect("select after replace");
        assert_eq!(
            rows.len(),
            1,
            "project_id is the primary key: a re-upsert replaces the row"
        );
        assert_eq!(rows[0].get::<_, String>(0), "second");
        assert_eq!(rows[0].get::<_, i64>(1), 7);
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn update_node_embeddings_sets_vector() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("node_emb");
        store
            .upsert_nodes(&[
                node_in("emb_a", &project, "src/a.rs"),
                node_in("emb_b", &project, "src/a.rs"),
            ])
            .await
            .expect("upsert nodes");

        assert_eq!(
            store
                .update_node_embeddings(&[])
                .await
                .expect("empty batch"),
            0,
            "an empty batch touches nothing"
        );

        let rows = vec![NodeEmbeddingRecord {
            id: "emb_a".to_string(),
            column: "embedding_1024",
            embedding: unit_vector(7),
            updated_at: chrono::Utc::now(),
        }];
        let updated = store
            .update_node_embeddings(&rows)
            .await
            .expect("update embeddings");
        assert_eq!(updated, 1, "only emb_a matches");

        let client = store.pool.get().await.expect("pool get");
        let a_has_vector: bool = client
            .query_one(
                "SELECT embedding_1024 IS NOT NULL FROM nodes WHERE id = $1",
                &[&"emb_a"],
            )
            .await
            .expect("select emb_a")
            .get(0);
        let b_has_vector: bool = client
            .query_one(
                "SELECT embedding_1024 IS NOT NULL FROM nodes WHERE id = $1",
                &[&"emb_b"],
            )
            .await
            .expect("select emb_b")
            .get(0);
        assert!(a_has_vector, "emb_a received the vector");
        assert!(!b_has_vector, "emb_b must keep a NULL vector");
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn delete_data_for_files_clears_five_tables() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("file_purge");
        store
            .upsert_nodes(&[
                node_in("purge_a1", &project, "src/a.rs"),
                node_in("purge_a2", &project, "src/a.rs"),
                node_in("purge_b1", &project, "src/b.rs"),
            ])
            .await
            .expect("upsert nodes");
        store
            .upsert_edges(&[
                edge_row("purge_ab", &project, "purge_a1", "purge_b1"),
                edge_row("purge_bb", &project, "purge_b1", "purge_b1"),
            ])
            .await
            .expect("upsert edges");
        store
            .upsert_chunks(&[
                ChunkRow {
                    id: "purge_chunk_a".to_string(),
                    project_id: project.clone(),
                    chunk_index: 0,
                    text: "text a".to_string(),
                    parent_node: "purge_a1".to_string(),
                    embedding: unit_vector(0),
                    embedding_model: "test-model".to_string(),
                },
                ChunkRow {
                    id: "purge_chunk_b".to_string(),
                    project_id: project.clone(),
                    chunk_index: 0,
                    text: "text b".to_string(),
                    parent_node: "purge_b1".to_string(),
                    embedding: unit_vector(1),
                    embedding_model: "test-model".to_string(),
                },
            ])
            .await
            .expect("upsert chunks");
        let mut symbol_a = symbol_embedding("purge_sym_a", &project, "sym_a", unit_vector(2));
        symbol_a.node_id = Some("purge_a1".to_string());
        let mut symbol_b = symbol_embedding("purge_sym_b", &project, "sym_b", unit_vector(3));
        symbol_b.node_id = Some("purge_b1".to_string());
        store
            .upsert_symbol_embeddings(&[symbol_a, symbol_b])
            .await
            .expect("upsert symbols");
        store
            .upsert_file_metadata(&[
                file_meta(&project, "src/a.rs", "hash_a"),
                file_meta(&project, "src/b.rs", "hash_b"),
            ])
            .await
            .expect("upsert metadata");

        let deleted = store
            .delete_data_for_files(&project, &["src/a.rs".to_string()])
            .await
            .expect("delete_data_for_files");
        assert_eq!(
            deleted, 6,
            "1 edge + 1 symbol + 1 chunk + 2 nodes + 1 metadata row"
        );

        let client = store.pool.get().await.expect("pool get");
        let remaining_nodes: i64 = client
            .query_one(
                "SELECT count(*) FROM nodes WHERE project_id = $1",
                &[&project],
            )
            .await
            .expect("count nodes")
            .get(0);
        assert_eq!(remaining_nodes, 1, "b.rs survives the a.rs purge");
        let b_node: i64 = client
            .query_one("SELECT count(*) FROM nodes WHERE id = $1", &[&"purge_b1"])
            .await
            .expect("select b node")
            .get(0);
        assert_eq!(b_node, 1, "the b.rs node must not be deleted");

        let stale_edges: i64 = client
            .query_one(
                "SELECT count(*) FROM edges WHERE project_id = $1 AND id = $2",
                &[&project, &"purge_ab"],
            )
            .await
            .expect("count stale edges")
            .get(0);
        assert_eq!(stale_edges, 0, "the edge touching a.rs is gone");
        let surviving_edges: i64 = client
            .query_one(
                "SELECT count(*) FROM edges WHERE project_id = $1 AND id = $2",
                &[&project, &"purge_bb"],
            )
            .await
            .expect("count surviving edges")
            .get(0);
        assert_eq!(surviving_edges, 1, "the b.rs self-edge survives");

        let stale_chunks: i64 = client
            .query_one(
                "SELECT count(*) FROM chunks WHERE project_id = $1 AND parent_node = $2",
                &[&project, &"purge_a1"],
            )
            .await
            .expect("count stale chunks")
            .get(0);
        assert_eq!(stale_chunks, 0);
        let stale_symbols: i64 = client
            .query_one(
                "SELECT count(*) FROM symbol_embeddings WHERE project_id = $1 AND node_id = $2",
                &[&project, &"purge_a1"],
            )
            .await
            .expect("count stale symbols")
            .get(0);
        assert_eq!(stale_symbols, 0);

        let hashes = store.file_hashes(&project).await.expect("file_hashes");
        assert_eq!(hashes.len(), 1, "only b.rs metadata remains");
        assert!(hashes.contains_key("src/b.rs"));
        assert!(!hashes.contains_key("src/a.rs"));
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn reconcile_removes_metadata_for_vanished_files() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("recon_files");
        store
            .upsert_nodes(&[
                node_in("recon_file_a", &project, "src/a.rs"),
                node_in("recon_file_b", &project, "src/b.rs"),
            ])
            .await
            .expect("upsert nodes");
        store
            .upsert_file_metadata(&[
                file_meta(&project, "src/a.rs", "hash_a"),
                file_meta(&project, "src/b.rs", "hash_b"),
            ])
            .await
            .expect("upsert metadata");

        let fresh = fresh_map(&[("src/a.rs", &["recon_file_a"])]);
        let stats = store
            .reconcile(&project, &fresh, &BTreeSet::new(), &BTreeSet::new())
            .await
            .expect("reconcile");

        assert_eq!(stats.stale_files, 1, "b.rs disappeared from the scan");
        let hashes = store.file_hashes(&project).await.expect("file_hashes");
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes.get("src/a.rs").map(String::as_str), Some("hash_a"));
    }

    // ---- Node retrieval: integration tests ----

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn knn_nodes_orders_by_distance() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("node_knn");
        let mut near = node("nk_near", &project);
        near.embedding = Some(unit_vector(0));
        near.embedding_model = Some("test-model".to_string());
        let mut middle = node("nk_middle", &project);
        middle.embedding = Some(unit_vector(1));
        middle.embedding_model = Some("test-model".to_string());
        let mut far = node("nk_far", &project);
        far.embedding = Some(unit_vector(2));
        far.embedding_model = Some("test-model".to_string());
        // No embedding: KNN must skip it, so only three candidates exist.
        let vectorless = node("nk_no_vector", &project);
        store
            .upsert_nodes(&[near, middle, far, vectorless])
            .await
            .expect("upsert_nodes");

        let hits = store
            .knn_nodes(&project, &unit_vector(0), 10)
            .await
            .expect("knn_nodes");

        assert_eq!(hits.len(), 3, "the vectorless node is excluded");
        assert_eq!(hits[0].0, "nk_near", "the exact match ranks first");
        assert!(hits[0].1.abs() < 1e-6, "identical vectors have distance 0");
        assert!(
            hits.windows(2).all(|pair| pair[0].1 <= pair[1].1),
            "distances are non-decreasing: {hits:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn full_text_nodes_matches_prefix_and_stem() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("node_fts");
        let mut dimension = node("fts_dimension", &project);
        dimension.name = "embedding_dimension".to_string();
        dimension.content = Some("the embedding dimension used by the vector column".to_string());
        let mut config = node("fts_config", &project);
        config.name = "parse_config".to_string();
        config.content = Some("parse config file".to_string());
        store
            .upsert_nodes(&[dimension, config])
            .await
            .expect("upsert_nodes");

        let prefix = store
            .full_text_nodes(&project, "emb", 10)
            .await
            .expect("prefix query");
        let prefix_ids: Vec<&str> = prefix.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            prefix_ids,
            vec!["fts_dimension"],
            "`emb:*` matches embedding_dimension but not parse_config"
        );

        let singular = store
            .full_text_nodes(&project, "embedding", 10)
            .await
            .expect("singular query");
        let plural = store
            .full_text_nodes(&project, "embeddings", 10)
            .await
            .expect("plural query");
        assert_eq!(
            singular, plural,
            "the english stemmer maps both to the same lexeme"
        );
        assert_eq!(singular.len(), 1);
        assert_eq!(singular[0].0, "fts_dimension");

        assert!(
            store
                .full_text_nodes(&project, "!!!", 10)
                .await
                .expect("symbol query")
                .is_empty(),
            "a query with no usable term never reaches the database"
        );
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn hybrid_nodes_merges_both_sources() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        let project = project_id("node_hybrid");
        let mut both = node("hyb_both", &project);
        both.name = "hybrid needle".to_string();
        both.content = Some("needle appears in both retrievers".to_string());
        both.embedding = Some(unit_vector(0));
        both.embedding_model = Some("test-model".to_string());

        let mut vector_only = node("hyb_vector", &project);
        vector_only.name = "vector only".to_string();
        vector_only.content = Some("nothing relevant here".to_string());
        vector_only.embedding = Some(unit_vector(0));
        vector_only.embedding_model = Some("test-model".to_string());

        let mut text_only = node("hyb_text", &project);
        text_only.name = "text only".to_string();
        text_only.content = Some("needle only in this node".to_string());

        store
            .upsert_nodes(&[both, vector_only, text_only])
            .await
            .expect("upsert_nodes");

        let hits = store
            .hybrid_nodes(&project, &unit_vector(0), "needle", 10)
            .await
            .expect("hybrid_nodes");

        let order: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(
            order[0], "hyb_both",
            "the node found by both retrievers wins: {order:?}"
        );
        assert!(
            order.contains(&"hyb_vector"),
            "vector-only hit is kept: {order:?}"
        );
        assert!(
            order.contains(&"hyb_text"),
            "text-only hit is kept: {order:?}"
        );

        let by_id = |id: &str| {
            hits.iter()
                .find(|hit| hit.id == id)
                .unwrap_or_else(|| panic!("missing {id} in {order:?}"))
        };
        assert!(by_id("hyb_both").vector_rank.is_some());
        assert!(by_id("hyb_both").text_rank.is_some());
        assert!(by_id("hyb_vector").text_rank.is_none());
        assert!(by_id("hyb_text").vector_rank.is_none());
    }

    // ---- IndexStorage additions: fingerprint / metadata / retarget ----

    fn project_with_metadata(project: &str, metadata: serde_json::Value) -> ProjectMetadataRecord {
        ProjectMetadataRecord {
            metadata,
            project_id: project.to_string(),
            name: "fingerprint-fixture".to_string(),
            root_path: "/tmp/fingerprint-fixture".to_string(),
            primary_language: Some("rust".to_string()),
            file_count: 1,
            node_count: 1,
            edge_count: 0,
            avg_coverage_score: 0.0,
            last_analyzed: chrono::Utc::now(),
            codegraph_version: "test".to_string(),
            organization_id: None,
            domain: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn project_fingerprint_reads_and_clears() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        use crate::storage::IndexStorage;
        let project = project_id("fingerprint");
        store
            .upsert_project_metadata(&project_with_metadata(
                &project,
                serde_json::json!({ "input_fingerprint": "fp1" }),
            ))
            .await
            .expect("upsert project metadata");

        let fingerprint = store
            .project_fingerprint(&project)
            .await
            .expect("project_fingerprint");
        assert_eq!(fingerprint.as_deref(), Some("fp1"));
        assert!(
            store
                .has_project_metadata(&project)
                .await
                .expect("has_project_metadata"),
            "the metadata row must exist before deletion"
        );

        store
            .delete_project_metadata(&project)
            .await
            .expect("delete_project_metadata");
        assert_eq!(
            store
                .project_fingerprint(&project)
                .await
                .expect("after delete"),
            None,
            "a deleted row must not yield a fingerprint"
        );
        assert!(
            !store
                .has_project_metadata(&project)
                .await
                .expect("has_project_metadata after delete"),
            "the metadata row must be gone"
        );
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn file_metadata_roundtrips_rows() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        use crate::storage::IndexStorage;
        let project = project_id("file_metadata_rows");
        let modified_at = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05Z")
            .expect("fixture timestamp")
            .with_timezone(&chrono::Utc);

        let mut second = file_meta(&project, "src/b.rs", "hash_b");
        second.modified_at = Some(modified_at);
        second.file_size = Some(77);
        second.node_count = 4;
        second.edge_count = 2;
        second.language = Some("rust".to_string());
        second.parse_errors = Some("[\"e1\",\"e2\"]".to_string());

        store
            .upsert_file_metadata(&[file_meta(&project, "src/a.rs", "hash_a"), second.clone()])
            .await
            .expect("upsert file metadata");

        let records = store.file_metadata(&project).await.expect("file_metadata");
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .map(|record| record.file_path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a.rs", "src/b.rs"],
            "rows must be ordered by file_path"
        );

        let b = &records[1];
        assert_eq!(b.project_id, project);
        assert_eq!(b.content_hash, "hash_b");
        assert_eq!(b.modified_at, modified_at);
        assert_eq!(b.file_size, 77);
        assert_eq!(b.node_count, 4);
        assert_eq!(b.edge_count, 2);
        assert_eq!(b.language.as_deref(), Some("rust"));
        assert_eq!(
            b.parse_errors.as_deref(),
            Some(["e1".to_string(), "e2".to_string()].as_slice())
        );
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn nodes_in_files_returns_only_named_files() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        use crate::storage::IndexStorage;
        let project = project_id("nodes_in_files");
        store
            .upsert_nodes(&[
                node_in("nif_a1", &project, "a.rs"),
                node_in("nif_a2", &project, "a.rs"),
                node_in("nif_b1", &project, "b.rs"),
            ])
            .await
            .expect("upsert nodes");

        let nodes = store
            .nodes_in_files(&project, &["a.rs".to_string()])
            .await
            .expect("nodes_in_files");
        assert_eq!(nodes.len(), 2, "only a.rs nodes may be returned");
        assert!(
            nodes.iter().all(|node| node.file_path == "a.rs"),
            "no b.rs row may leak into the result"
        );
        assert!(
            nodes.iter().all(|node| !node.node_type.is_empty()),
            "node_type must carry the stored kind"
        );
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn retarget_edges_moves_to_endpoint() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.ensure_schema().await.expect("ensure_schema");

        use crate::storage::IndexStorage;
        let project = project_id("retarget");
        store
            .upsert_edges(&[edge_row("retarget_e1", &project, "A", "B")])
            .await
            .expect("upsert edge");

        let moved = store
            .retarget_edges(&project, "B", "C")
            .await
            .expect("retarget_edges");
        assert_eq!(moved, 1, "exactly one edge points at B");

        let client = store.pool.get().await.expect("pool get");
        let to_id: String = client
            .query_one(
                "SELECT to_id FROM edges WHERE project_id = $1 AND id = $2",
                &[&project, &"retarget_e1"],
            )
            .await
            .expect("select retargeted edge")
            .get(0);
        assert_eq!(to_id, "C", "the edge must now point at the new id");
    }

    #[tokio::test]
    #[ignore = "requires CODEGRAPH_PG_URL pointing at a pgvector database"]
    async fn ensure_vector_indexes_is_true() {
        let Some(store) = store_or_skip() else {
            return;
        };

        use crate::storage::IndexStorage;
        let ready = store
            .ensure_vector_indexes(1024, &["nodes".to_string(), "chunks".to_string()])
            .await
            .expect("ensure_vector_indexes");
        assert!(ready, "Postgres builds its HNSW indexes in SCHEMA_DDL");
    }
}

#[cfg(test)]
mod storage_adapter_tests {
    use super::*;
    use codegraph_core::{EdgeType, Language, Location, Metadata, NodeId, NodeType};
    use std::collections::HashMap;

    fn location() -> Location {
        Location {
            file_path: "src/lib.rs".to_string(),
            line: 10,
            column: 4,
            end_line: Some(20),
            end_column: Some(1),
        }
    }

    #[test]
    fn code_node_maps_to_node_row() {
        let mut node = CodeNode::new(
            "do_thing",
            Some(NodeType::Function),
            Some(Language::Rust),
            location(),
        );
        node.metadata = Metadata {
            attributes: HashMap::from([
                ("project_id".to_string(), "proj".to_string()),
                ("embedding_model".to_string(), "test-model".to_string()),
            ]),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        node.complexity = Some(3.5);

        let row = code_node_to_row(&node);

        assert_eq!(row.id, node.id.to_string());
        assert_eq!(row.name, "do_thing");
        assert_eq!(row.project_id, "proj");
        assert_eq!(row.file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(row.start_line, Some(10));
        assert_eq!(row.end_line, Some(20));
        assert_eq!(row.node_type.as_deref(), Some("Function"));
        assert_eq!(row.language.as_deref(), Some("Rust"));
        assert_eq!(row.embedding_model.as_deref(), Some("test-model"));
        assert_eq!(row.complexity, Some(3.5));
    }

    #[test]
    fn code_edge_maps_to_edge_row() {
        let edge = CodeEdge::new(NodeId::new_v4(), NodeId::new_v4(), EdgeType::Calls)
            .with_project_id("proj");

        let row = code_edge_to_row(&edge);

        assert_eq!(row.id, edge.id.to_string());
        assert_eq!(row.project_id, "proj");
        assert_eq!(row.from_id, edge.from.to_string());
        assert_eq!(row.to_id, edge.to.to_string());
        assert_eq!(row.edge_type, "calls");
    }

    #[test]
    fn chunk_record_picks_present_embedding() {
        let record = ChunkEmbeddingRecord {
            id: "chunk-1".to_string(),
            parent_node: "node-1".to_string(),
            chunk_index: 0,
            text: "plain text".to_string(),
            project_id: "proj".to_string(),
            embedding_384: None,
            embedding_768: Some(vec![0.5f32; 768]),
            embedding_1024: None,
            embedding_1536: None,
            embedding_2048: None,
            embedding_2560: None,
            embedding_3072: None,
            embedding_4096: None,
            embedding_model: "test-model".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let row = chunk_record_to_row(&record);

        assert_eq!(row.embedding.len(), 768);
        assert_eq!(row.project_id, "proj");
        assert_eq!(row.text, "plain text");
    }

    #[test]
    fn file_metadata_parse_errors_serialised() {
        let mut record = FileMetadataRecord {
            file_path: "src/lib.rs".to_string(),
            project_id: "proj".to_string(),
            content_hash: "hash".to_string(),
            modified_at: chrono::Utc::now(),
            file_size: 42,
            last_indexed_at: chrono::Utc::now(),
            node_count: 3,
            edge_count: 2,
            language: Some("Rust".to_string()),
            parse_errors: Some(vec!["e1".to_string()]),
        };

        let row = file_metadata_record_to_row(&record);
        assert_eq!(row.parse_errors.as_deref(), Some("[\"e1\"]"));
        assert_eq!(row.node_count, 3);
        assert_eq!(row.edge_count, 2);

        record.parse_errors = None;
        let row = file_metadata_record_to_row(&record);
        assert_eq!(row.parse_errors, None);
    }

    #[test]
    fn reconcile_stats_maps_to_outcome() {
        let stats = ReconcileStats {
            stale_nodes: 1,
            stale_edges: 2,
            stale_chunks: 3,
            stale_symbols: 4,
            stale_files: 5,
        };

        let outcome = reconcile_stats_to_outcome(stats);

        assert_eq!(outcome.stale_nodes, stats.stale_nodes);
        assert_eq!(outcome.stale_edges, stats.stale_edges);
        assert_eq!(outcome.stale_chunks, stats.stale_chunks);
        assert_eq!(outcome.stale_symbols, stats.stale_symbols);
        assert_eq!(outcome.stale_files, stats.stale_files);
    }
}
