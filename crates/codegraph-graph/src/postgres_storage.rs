// ABOUTME: Postgres/pgvector storage scaffolding for the backend migration evaluated in docs/BACKEND_EVALUATION.md.
// ABOUTME: Kept behind the `postgres` feature and out of the indexing path until parity is proven.

//! Offline proof-of-concept storage layer for Postgres + pgvector.
//!
//! This module intentionally implements only the three operations the migration
//! evaluation measured (chunk upsert via `COPY`, KNN search and recursive reverse
//! dependency walks) plus the DDL they need. It is not wired into the indexing
//! pipeline; see `docs/BACKEND_EVALUATION.md` for the measurements motivating it.

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

/// DDL for the chunk and edge tables. Every statement is idempotent.
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

    /// Apply the chunk/edge DDL. Safe to run repeatedly.
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
        assert_eq!(table_count, 2, "expected chunks + edges tables");
        assert_eq!(index_count, 6, "expected 6 chunks/edges indexes");
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
}
