// ABOUTME: Postgres/pgvector storage scaffolding for the backend migration evaluated in docs/BACKEND_EVALUATION.md.
// ABOUTME: Kept behind the `postgres` feature and out of the indexing path until parity is proven.

//! Offline proof-of-concept storage layer for Postgres + pgvector.
//!
//! This module implements the operations the migration evaluation measured (chunk
//! upsert via `COPY`, KNN search and recursive reverse dependency walks), the node
//! and symbol-embedding tables they ingest into, and the DDL all of them need. It is
//! not wired into the indexing pipeline; see `docs/BACKEND_EVALUATION.md` for the
//! measurements motivating it.

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

/// DDL for the chunk, edge, node and symbol-embedding tables. Every statement is
/// idempotent.
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
  project_id text NOT NULL,
  from_id    text NOT NULL,
  to_id      text NOT NULL,
  edge_type  text NOT NULL
);
CREATE INDEX IF NOT EXISTS edges_from ON edges(project_id, edge_type, from_id);
CREATE INDEX IF NOT EXISTS edges_to   ON edges(project_id, edge_type, to_id);

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

const KNN_SQL: &str = r#"
SELECT id, (embedding <=> $1::text::vector) AS distance
FROM chunks
WHERE project_id = $2
ORDER BY embedding <=> $1::text::vector
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

    // TODO(migration): full-text ranking via `ts_rank_cd(content_tsv, query)`
    // once the POC grows a BM25/tsquery path.
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
            table_count, 4,
            "expected chunks + edges + nodes + symbol_embeddings tables"
        );
        assert_eq!(
            index_count, 15,
            "expected 15 chunks/edges/nodes/symbol indexes"
        );
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

    // ---- Integration tests (require CODEGRAPH_PG_URL, run with --ignored) ----

    fn store_or_skip() -> Option<PgStore> {
        if std::env::var("CODEGRAPH_PG_URL").is_err() {
            return None;
        }
        let store = PgStore::connect("").expect("connect to CODEGRAPH_PG_URL");
        Some(store)
    }

    fn project_id(tag: &str) -> String {
        format!("poc_{tag}_{}", std::process::id())
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
                "INSERT INTO edges (project_id, from_id, to_id, edge_type) VALUES ($1,$2,$3,$4)",
                &[&project, &"A", &"B", &DEFAULT_EDGE_TYPE],
            )
            .await
            .expect("insert A->B");
        client
            .execute(
                "INSERT INTO edges (project_id, from_id, to_id, edge_type) VALUES ($1,$2,$3,$4)",
                &[&project, &"B", &"C", &DEFAULT_EDGE_TYPE],
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
}
