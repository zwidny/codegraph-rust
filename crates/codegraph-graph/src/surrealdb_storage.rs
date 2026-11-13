use crate::edge::CodeEdge;
use crate::graph_functions::GraphFunctions;
use crate::graph_types::{
    CallChainNode, CircularDependency, ComplexityHotspot, CouplingMetricsResult, DependencyNode,
    HubNode, NodeEdgeSummary,
};
use async_trait::async_trait;
use chrono::Utc;
use codegraph_core::{CodeGraphError, CodeNode, GraphStore, NodeId, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use surrealdb::types::SurrealValue;
use surrealdb::{
    Error as SurrealError, Surreal,
    engine::any::Any,
    opt::auth::{Database, Root},
};
use tracing::{debug, info, warn};

/// Bundled schemas, applied automatically to a fresh embedded store. v2 is the default;
/// CODEGRAPH_SCHEMA=v1 selects the original file.
const BUNDLED_SCHEMA_MAIN: &str = include_str!("../../../schema/codegraph.surql");
const BUNDLED_SCHEMA_EXPERIMENTAL: &str =
    include_str!("../../../schema/codegraph_graph_experimental.surql");
const BUNDLED_SCHEMA_V2: &str = include_str!("../../../schema/codegraph_v2.surql");

/// Directory under the project root that holds the embedded store.
pub const EMBEDDED_DB_DIR: &str = ".codegraph";
/// Name of the embedded store inside [`EMBEDDED_DB_DIR`].
pub const EMBEDDED_DB_NAME: &str = "db";

/// Namespace and database used for embedded stores (one store per project,
/// so these never need to vary).
const EMBEDDED_NAMESPACE: &str = "codegraph";
const EMBEDDED_DATABASE: &str = "codegraph";
const EMBEDDED_GRAPH_DATABASE: &str = "codegraph_graph";

/// Connection schemes handled by an in-process engine rather than a server.
const EMBEDDED_SCHEMES: [&str; 4] = ["surrealkv://", "file://", "rocksdb://", "mem://"];

/// Shared server used when `CODEGRAPH_SURREALDB_URL` is unset.
///
/// The embedded store is a compatibility fallback: it is substantially slower and allows
/// only one process per project, so it is opt-in through an explicit `surrealkv://` URL.
pub const DEFAULT_SERVER_URL: &str = "ws://127.0.0.1:8000";

/// True when the connection string opens an in-process engine.
pub fn is_embedded_connection(connection: &str) -> bool {
    EMBEDDED_SCHEMES
        .iter()
        .any(|scheme| connection.starts_with(scheme))
}

/// True for in-process engines backed by a directory. These lock the
/// directory, so a process must share one handle per store, and they get
/// the bundled schema on first open. `mem://` is excluded: every open is a
/// fresh, empty database, which tests rely on.
fn is_persistent_embedded_connection(connection: &str) -> bool {
    is_embedded_connection(connection) && !connection.starts_with("mem://")
}

/// Handles to embedded stores already opened by this process, keyed by connection string.
static EMBEDDED_HANDLES: OnceLock<tokio::sync::Mutex<HashMap<String, Surreal<Any>>>> =
    OnceLock::new();

/// Drop top-level `null` keys from batch payloads. A JSON `null` binds as
/// SurrealQL `NULL`, which `option<T>` fields reject; a missing key reads as
/// `NONE`, which they accept.
fn strip_null_fields(payloads: Vec<JsonValue>) -> Vec<JsonValue> {
    payloads
        .into_iter()
        .map(|mut payload| {
            if let JsonValue::Object(map) = &mut payload {
                map.retain(|_, v| !v.is_null());
            }
            payload
        })
        .collect()
}

/// The schema statements that are safe to run against an existing store: functions and
/// params declared with `OVERWRITE`. Function bodies end with `} PERMISSIONS FULL;` on
/// its own line; params end at the first `;`.
fn refreshable_definitions(schema: &str) -> String {
    const FUNCTION: &str = "\nDEFINE FUNCTION OVERWRITE fn::";
    const PARAM: &str = "\nDEFINE PARAM OVERWRITE $";
    const FUNCTION_END: &str = "\n} PERMISSIONS FULL;";

    let mut statements = Vec::new();
    let mut rest = schema;
    // Definitions start at the beginning of a line, which keeps prose in comments out.
    while let Some(start) = [rest.find(FUNCTION), rest.find(PARAM)]
        .into_iter()
        .flatten()
        .min()
    {
        let from = &rest[start + 1..];
        let end = if from.starts_with("DEFINE FUNCTION") {
            from.find(FUNCTION_END).map(|i| i + FUNCTION_END.len())
        } else {
            from.find(';').map(|i| i + 1)
        };
        let Some(end) = end else { break };
        statements.push(&from[..end]);
        rest = &from[end..];
    }
    statements.join("\n")
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Resolve the connection string for a project.
///
/// Split out so the default can be tested without mutating the process environment.
fn project_connection(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
}

/// One database per project on a shared server (default).
///
/// Set `CODEGRAPH_DATABASE_PER_PROJECT=0` to fall back to the shared
/// `CODEGRAPH_SURREALDB_DATABASE` (the old behaviour).
fn database_per_project() -> bool {
    database_per_project_with(env::var("CODEGRAPH_DATABASE_PER_PROJECT").ok().as_deref())
}

fn database_per_project_with(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// Stable, identifier-safe database name for a project root.
///
/// The suffix keeps two checkouts with the same directory name apart while the prefix
/// keeps the name readable in `INFO FOR DB` and SurrealDB tooling.
fn project_database_name(project_root: &Path) -> String {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let base = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let mut sanitized: String = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(32);
    if sanitized.is_empty() {
        sanitized.push_str("project");
    }
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hash = digest
        .iter()
        .take(6)
        .map(|byte| format!("{:02x}", *byte))
        .collect::<String>();
    format!("{sanitized}_{hash}")
}

/// SurrealDB storage implementation with flexible schema support
#[derive(Clone)]
pub struct SurrealDbStorage {
    db: Arc<Surreal<Any>>,
    config: SurrealDbConfig,
    // In-memory cache for performance
    node_cache: Arc<DashMap<NodeId, CodeNode>>,
    schema_version: Arc<std::sync::RwLock<u32>>,
}

#[derive(Debug, Clone)]
pub struct SurrealDbConfig {
    pub connection: String,
    pub namespace: String,
    pub database: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub strict_mode: bool,
    pub auto_migrate: bool,
    pub cache_enabled: bool,
}

impl SurrealDbConfig {
    /// Connection settings for a project.
    ///
    /// With `CODEGRAPH_SURREALDB_URL` set, this targets that server using the
    /// `CODEGRAPH_SURREALDB_*` settings. Otherwise it targets the shared server at
    /// [`DEFAULT_SERVER_URL`]; the embedded per-project store is opt-in by pointing the
    /// URL at a `surrealkv://` path.
    pub fn for_project(project_root: &Path) -> Self {
        let connection = project_connection(env_value("CODEGRAPH_SURREALDB_URL"));
        let mut config = Self::remote(connection);
        // Shared servers keep every project in one `chunks` table today, so one HNSW
        // index covers all projects. Derive one database per project instead; embedded
        // stores are already isolated by their file path.
        if !config.is_embedded() && database_per_project() {
            config.database = project_database_name(project_root);
            debug!(
                "Per-project database '{}' for {}",
                config.database,
                project_root.display()
            );
        }
        config
    }

    /// Settings for the embedded store under `project_root`.
    pub fn embedded(project_root: &Path) -> Self {
        let root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        Self {
            connection: format!("surrealkv://{}", Self::embedded_db_path(&root).display()),
            namespace: EMBEDDED_NAMESPACE.to_string(),
            database: if env_flag("CODEGRAPH_USE_GRAPH_SCHEMA") {
                EMBEDDED_GRAPH_DATABASE.to_string()
            } else {
                EMBEDDED_DATABASE.to_string()
            },
            username: None,
            password: None,
            strict_mode: false,
            auto_migrate: true,
            cache_enabled: true,
        }
    }

    /// Where the embedded store for `project_root` lives.
    pub fn embedded_db_path(project_root: &Path) -> PathBuf {
        project_root.join(EMBEDDED_DB_DIR).join(EMBEDDED_DB_NAME)
    }

    /// Settings for a SurrealDB server, with namespace, database and
    /// credentials taken from the `CODEGRAPH_SURREALDB_*` variables.
    pub fn remote(connection: String) -> Self {
        let namespace =
            env_value("CODEGRAPH_SURREALDB_NAMESPACE").unwrap_or_else(|| "ouroboros".to_string());

        // Optional toggle to point at experimental graph schema DB without changing table names.
        let database = if env_flag("CODEGRAPH_USE_GRAPH_SCHEMA") {
            env_value("CODEGRAPH_GRAPH_DB_DATABASE")
                .unwrap_or_else(|| EMBEDDED_GRAPH_DATABASE.to_string())
        } else {
            env_value("CODEGRAPH_SURREALDB_DATABASE").unwrap_or_else(|| "codegraph".to_string())
        };

        Self {
            connection,
            namespace,
            database,
            username: env_value("CODEGRAPH_SURREALDB_USERNAME"),
            password: env_value("CODEGRAPH_SURREALDB_PASSWORD"),
            strict_mode: false,
            auto_migrate: true,
            cache_enabled: true,
        }
    }

    /// True when this configuration opens an in-process engine.
    pub fn is_embedded(&self) -> bool {
        is_embedded_connection(&self.connection)
    }
}

impl Default for SurrealDbConfig {
    /// Settings for the project in the current working directory (see [`Self::for_project`]).
    fn default() -> Self {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::for_project(&cwd)
    }
}

/// Schema version tracking for migrations
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SchemaVersion {
    version: u32,
    applied_at: String,
    description: String,
}

/// Statistics from orphan cleanup operation
#[derive(Debug, Clone, Default)]
pub struct OrphanCleanupStats {
    pub chunks_deleted: usize,
    pub edges_deleted: usize,
    pub symbols_deleted: usize,
}

impl SurrealDbStorage {
    /// Reconcile stale identities against the complete prepared project catalog.
    /// Unchanged nodes retain incoming relationships and vectors.
    pub async fn reconcile_catalog(
        &self,
        project: &str,
        nodes: Vec<String>,
        edges: Vec<String>,
        chunks: Vec<String>,
        files: Vec<String>,
    ) -> Result<()> {
        self.db.query(r#"
            BEGIN TRANSACTION;
            LET $node_ids = $nodes.map(|$id| type::record('nodes', $id));
            LET $edge_ids = $edges.map(|$id| type::record('edges', $id));
            LET $chunk_ids = $chunks.map(|$id| type::record('chunks', $id));
            DELETE symbol_embeddings WHERE project_id = $project AND node_id != NONE AND node_id NOT IN $node_ids;
            DELETE edges WHERE project_id = $project AND (id NOT IN $edge_ids OR from NOT IN $node_ids OR to NOT IN $node_ids);
            DELETE chunks WHERE project_id = $project AND (id NOT IN $chunk_ids OR parent_node NOT IN $node_ids);
            DELETE nodes WHERE project_id = $project AND id NOT IN $node_ids;
            DELETE file_metadata WHERE project_id = $project AND file_path NOT IN $files;
            COMMIT TRANSACTION;
        "#).bind(("project", project.to_string())).bind(("nodes", nodes)).bind(("edges", edges))
            .bind(("chunks", chunks)).bind(("files", files)).await
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?
            .check().map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation statement failed: {e}")))?;
        Ok(())
    }

    /// Per-file catalog reconciliation, single-scan variant.
    ///
    /// The original `reconcile_catalog` issues project-wide `NOT IN` deletes whose
    /// parameter lists hold every node, edge and chunk id (tens of thousands of
    /// elements). `NOT IN` cannot use an index, so each statement scans the whole
    /// table while probing an oversized array — measured at 24+ minutes on this
    /// project, on every run.
    ///
    /// A previous per-file variant shrank each scan but still ran two queries per
    /// indexed file (measured ~0.4s each, ~4 minutes for 295 files) and could not
    /// see orphans whose `file_path` no longer matches what the parser emits
    /// (e.g. absolute-path leftovers from earlier runs).
    ///
    /// This variant reads `(id, file_path)` for the whole project in one scan
    /// (measured 145ms for ~26k rows), groups in memory, and reconciles by
    /// difference: files present in the store but absent from `fresh_nodes_by_file`
    /// are removed entirely via `delete_data_for_files`; surviving files keep only
    /// the ids this run emitted. Chunk orphans are reconciled the same way in a
    /// second single scan over `(id, parent_node)`.
    pub async fn reconcile_catalog_by_file(
        &self,
        project: &str,
        fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
        fresh_chunks: &BTreeSet<String>,
        fresh_edge_ids: &BTreeSet<String>,
        scope: Option<&ReconcileScope>,
    ) -> Result<()> {
        let decode_id = |raw: &str| {
            raw.strip_prefix("nodes:")
                .unwrap_or(raw)
                .trim_matches('`')
                .to_string()
        };

        // Without a scope every file in the project is in scope; with one, only
        // files under `scan_root` may be treated as deleted or re-emitted.
        let in_scope = |file: &str| -> bool {
            let Some(scope) = scope else {
                return true;
            };
            let candidate = Path::new(file);
            let absolute = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                scope.project_root.join(candidate)
            };
            let absolute = crate::storage::lexical_normalize_path(&absolute);
            absolute == scope.scan_root || absolute.starts_with(&scope.scan_root)
        };

        // --- Single scan over nodes: (raw id, file_path) -------------------
        let mut response = self
            .db
            .query("SELECT VALUE [<string> id, file_path] FROM nodes WHERE project_id = $project;")
            .bind(("project", project.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?;
        let node_rows: Vec<Vec<Option<String>>> = response.take(0).unwrap_or_default();

        let mut existing_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut file_by_node: BTreeMap<String, String> = BTreeMap::new();
        for row in &node_rows {
            if let (Some(raw_id), Some(file)) = (
                row.first().and_then(|v| v.as_deref()),
                row.get(1).and_then(|v| v.as_deref()),
            ) {
                let id = decode_id(raw_id);
                file_by_node.insert(id.clone(), file.to_string());
                existing_by_file
                    .entry(file.to_string())
                    .or_default()
                    .push(id);
            }
        }

        // --- Files that disappeared entirely (including stale path formats) --
        let deleted_files: Vec<String> = existing_by_file
            .keys()
            .filter(|file| in_scope(file) && !fresh_nodes_by_file.contains_key(*file))
            .cloned()
            .collect();
        if !deleted_files.is_empty() {
            self.delete_data_for_files(project, &deleted_files).await?;
        }

        // --- Surviving files: drop ids this run did not re-emit -------------
        let mut stale_nodes: Vec<String> = Vec::new();
        for (file, fresh_ids) in fresh_nodes_by_file {
            let Some(existing) = existing_by_file.get(file) else {
                continue;
            };
            let fresh: std::collections::HashSet<&str> =
                fresh_ids.iter().map(String::as_str).collect();
            stale_nodes.extend(
                existing
                    .iter()
                    .filter(|id| !fresh.contains(id.as_str()))
                    .cloned(),
            );
        }

        // --- Single scan over chunks: (raw id, parent raw id) ----------------
        let mut response = self
            .db
            .query("SELECT VALUE [<string> id, <string> parent_node] FROM chunks WHERE project_id = $project;")
            .bind(("project", project.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?;
        let chunk_rows: Vec<Vec<Option<String>>> = response.take(0).unwrap_or_default();

        let decode_chunk = |raw: &str| {
            raw.strip_prefix("chunks:")
                .unwrap_or(raw)
                .trim_matches('`')
                .to_string()
        };
        let mut stale_chunks: Vec<String> = Vec::new();
        if !chunk_rows.is_empty() {
            let fresh_chunk_ids: std::collections::HashSet<&str> =
                fresh_chunks.iter().map(String::as_str).collect();
            // Nodes that this reconciliation is about to delete orphan their chunks.
            let stale_node_ids: std::collections::HashSet<&str> =
                stale_nodes.iter().map(String::as_str).collect();
            let fresh_parent_ids: std::collections::HashSet<&str> = fresh_nodes_by_file
                .values()
                .flatten()
                .map(String::as_str)
                .collect();
            for row in &chunk_rows {
                let (Some(raw_id), Some(raw_parent)) = (
                    row.first().and_then(|v| v.as_deref()),
                    row.get(1).and_then(|v| v.as_deref()),
                ) else {
                    continue;
                };
                let id = decode_chunk(raw_id);
                let parent = decode_id(raw_parent);
                // Chunks whose owning node lives outside the scanned scope are not
                // this run's to delete.
                if let Some(file) = file_by_node.get(&parent)
                    && !in_scope(file)
                {
                    continue;
                }
                if !fresh_chunk_ids.contains(id.as_str())
                    || stale_node_ids.contains(parent.as_str())
                    || !fresh_parent_ids.contains(parent.as_str())
                {
                    stale_chunks.push(id);
                }
            }
        }

        // --- Single scan over edges: (raw id, from raw id, to raw id) --------
        // Every edge in the project is read once (`project_id` is indexed) and
        // diffed against `fresh_edge_ids` in Rust. `id NOT IN $fresh` would inline
        // ~96k ids and push the project-level reconcile past 10 minutes (see the
        // "Per-file reconciliation" comment in indexer.rs), which is exactly why
        // this per-file variant exists. An edge whose endpoint lives in a file
        // this run scanned, yet whose id is absent from `fresh_edge_ids`, was not
        // re-emitted by this run and is therefore stale.
        let mut stale_edges: Vec<String> = Vec::new();
        if fresh_edge_ids.is_empty() {
            // An empty set means the caller supplied no information, not that
            // every edge is stale. Deleting on that inference would wipe the table.
            debug!("Skipping stale edge reconciliation for {project}: no fresh edge ids supplied");
        } else {
            let mut response = self
                .db
                .query("SELECT VALUE [<string> id, <string> from, <string> to] FROM edges WHERE project_id = $project;")
                .bind(("project", project.to_string()))
                .await
                .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?
                .check()
                .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?;
            let edge_rows: Vec<Vec<Option<String>>> = response.take(0).unwrap_or_default();

            let decode_edge_id = |raw: &str| {
                raw.strip_prefix("edges:")
                    .unwrap_or(raw)
                    .trim_matches('`')
                    .to_string()
            };
            let scanned = |id: &str| {
                file_by_node
                    .get(id)
                    .is_some_and(|file| fresh_nodes_by_file.contains_key(file))
            };
            for row in &edge_rows {
                let (Some(raw_id), Some(raw_from), Some(raw_to)) = (
                    row.first().and_then(|v| v.as_deref()),
                    row.get(1).and_then(|v| v.as_deref()),
                    row.get(2).and_then(|v| v.as_deref()),
                ) else {
                    continue;
                };
                let id = decode_edge_id(raw_id);
                if fresh_edge_ids.contains(&id) {
                    continue;
                }
                let from_id = decode_id(raw_from);
                let to_id = decode_id(raw_to);
                // Endpoints in files this run did not scan may belong to another
                // scope, so those edges stay put. Edges hanging off nodes that no
                // longer exist are already covered by the stale-node delete.
                if scanned(&from_id) || scanned(&to_id) {
                    stale_edges.push(id);
                }
            }
        }

        if stale_nodes.is_empty() && stale_chunks.is_empty() && stale_edges.is_empty() {
            return Ok(());
        }

        let stale_records: Vec<String> = stale_nodes.clone();
        let stale_chunk_records: Vec<String> = stale_chunks.clone();
        let stale_edge_records: Vec<String> = stale_edges.clone();
        self.db
            .query(
                r#"
                BEGIN TRANSACTION;
                LET $stale_nodes = $stale.map(|$id| type::record('nodes', $id));
                LET $stale_chunk_ids = $stale_chunks.map(|$id| type::record('chunks', $id));
                LET $stale_edge_ids = $stale_edges.map(|$id| type::record('edges', $id));
                DELETE edges WHERE project_id = $project AND $has_nodes AND (from IN $stale_nodes OR to IN $stale_nodes);
                DELETE edges WHERE project_id = $project AND $has_stale_edges AND id IN $stale_edge_ids;
                DELETE symbol_embeddings WHERE project_id = $project AND $has_nodes AND node_id IN $stale_nodes;
                DELETE chunks WHERE project_id = $project AND id IN $stale_chunk_ids;
                DELETE nodes WHERE project_id = $project AND id IN $stale_nodes;
                COMMIT TRANSACTION;
            "#,
            )
            .bind(("project", project.to_string()))
            .bind(("stale", stale_records))
            .bind(("stale_chunks", stale_chunk_records))
            .bind(("stale_edges", stale_edge_records))
            .bind(("has_nodes", !stale_nodes.is_empty()))
            .bind(("has_stale_edges", !stale_edges.is_empty()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("Catalog reconciliation statement failed: {e}")))?;
        Ok(())
    }

    pub fn disable_ingestion_cache(&mut self) {
        self.config.cache_enabled = false;
        self.node_cache.clear();
    }

    /// Symbol columns used to seed resolution when only part of a project is re-indexed.
    ///
    /// A partial scan only parses the scanned files, so edges from those files can only
    /// resolve against the rest of the project if its symbols are loaded as a base.
    pub async fn load_symbols_excluding(
        &self,
        project: &str,
        excluded_files: &[String],
    ) -> Result<Vec<SymbolRow>> {
        let mut response = self
            .db
            .query(
                "SELECT <string> id AS id, file_path, name, node_type, \
                 metadata.qualified_name AS qualified FROM nodes \
                 WHERE project_id = $project AND file_path NOT IN $files;",
            )
            .bind(("project", project.to_string()))
            .bind(("files", excluded_files.to_vec()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Symbol load failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("Symbol load failed: {e}")))?;
        let rows: Vec<JsonValue> = response.take(0).unwrap_or_default();

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(SymbolRow {
                    id: row.get("id")?.as_str()?.to_string(),
                    file_path: row.get("file_path")?.as_str()?.to_string(),
                    name: row.get("name")?.as_str()?.to_string(),
                    node_type: row
                        .get("node_type")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    qualified_name: row
                        .get("qualified")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                })
            })
            .collect())
    }

    /// Delete dependent rows before nodes, with native record IDs and project scope.
    /// One transaction prevents partial cleanup when a statement fails.
    pub async fn delete_data_for_files(
        &self,
        project_id: &str,
        file_paths: &[String],
    ) -> Result<()> {
        if file_paths.is_empty() {
            return Ok(());
        }
        self.db.query(r#"
            BEGIN TRANSACTION;
            LET $ids = SELECT VALUE id FROM nodes WHERE project_id = $project_id AND file_path IN $file_paths;
            DELETE symbol_embeddings WHERE project_id = $project_id AND node_id IN $ids;
            DELETE edges WHERE project_id = $project_id AND (from IN $ids OR to IN $ids);
            DELETE chunks WHERE project_id = $project_id AND parent_node IN $ids;
            DELETE nodes WHERE project_id = $project_id AND file_path IN $file_paths;
            DELETE file_metadata WHERE project_id = $project_id AND file_path IN $file_paths;
            COMMIT TRANSACTION;
        "#).bind(("project_id", project_id.to_string())).bind(("file_paths", file_paths.to_vec()))
            .await.map_err(|e| CodeGraphError::Database(format!("File cleanup failed: {e}")))?
            .check().map_err(|e| CodeGraphError::Database(format!("File cleanup returned error: {e}")))?;
        self.node_cache.retain(|_, node| {
            node.metadata
                .attributes
                .get("project_id")
                .map(String::as_str)
                != Some(project_id)
                || !file_paths.contains(&node.location.file_path)
        });
        Ok(())
    }

    /// Get the underlying SurrealDB connection
    /// This is useful for advanced operations like graph functions
    pub fn db(&self) -> Arc<Surreal<Any>> {
        Arc::clone(&self.db)
    }

    /// Create a new SurrealDB storage instance
    pub async fn new(config: SurrealDbConfig) -> Result<Self> {
        info!(
            "Initializing SurrealDB storage with connection: {}",
            config.connection
        );

        let persistent_embedded = is_persistent_embedded_connection(&config.connection);
        let db = if persistent_embedded {
            Self::open_embedded(&config).await?
        } else {
            Self::connect_remote(&config).await?
        };

        // Select namespace and database
        db.use_ns(&config.namespace)
            .use_db(&config.database)
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to select namespace/database: {}", e))
            })?;

        let per_project_database = database_per_project() && !config.is_embedded();
        if config.auto_migrate && (persistent_embedded || per_project_database) {
            Self::ensure_bundled_schema(&db).await?;
        }

        let storage = Self {
            db: Arc::new(db),
            config: config.clone(),
            node_cache: Arc::new(DashMap::new()),
            schema_version: Arc::new(std::sync::RwLock::new(0)),
        };

        info!("SurrealDB storage initialized successfully");
        Ok(storage)
    }

    /// Connect to a SurrealDB server (or a fresh `mem://` engine) and authenticate if credentials are set.
    async fn connect_remote(config: &SurrealDbConfig) -> Result<Surreal<Any>> {
        let db: Surreal<Any> = Surreal::init();
        db.connect(&config.connection).await.map_err(|e| {
            let mut message = format!(
                "Failed to connect to SurrealDB at {}: {}",
                config.connection, e
            );
            if config.connection == DEFAULT_SERVER_URL {
                message.push_str(&format!(
                    "\nCodeGraph now defaults to a shared SurrealDB server. Either start one \
                         (see docs/INSTALLATION_GUIDE.md) and apply schema/codegraph_v2.surql, or \
                         set CODEGRAPH_SURREALDB_URL to your server. For the slower single-process \
                         embedded store, set CODEGRAPH_SURREALDB_URL=surrealkv://<project>/{}/{}.",
                    EMBEDDED_DB_DIR, EMBEDDED_DB_NAME
                ));
            }
            CodeGraphError::Database(message)
        })?;

        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            let auth_result = if config.connection.starts_with("wss://") {
                // Prefer database-scoped auth for hosted / cloud setups
                db.signin(Database {
                    namespace: config.namespace.clone(),
                    database: config.database.clone(),
                    username: username.clone(),
                    password: password.clone(),
                })
                .await
            } else {
                db.signin(Root {
                    username: username.clone(),
                    password: password.clone(),
                })
                .await
            };

            auth_result
                .map_err(|e| CodeGraphError::Database(format!("Authentication failed: {}", e)))?;
        }

        Ok(db)
    }

    /// Open an embedded store, reusing the handle if this process already has it open.
    ///
    /// The engine locks its directory, so a second open of the same path from
    /// this process would fail; a second open from another process does fail.
    async fn open_embedded(config: &SurrealDbConfig) -> Result<Surreal<Any>> {
        let handles = EMBEDDED_HANDLES.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
        let mut handles = handles.lock().await;

        if let Some(db) = handles.get(&config.connection) {
            debug!(
                "Reusing embedded SurrealDB handle for {}",
                config.connection
            );
            return Ok(db.clone());
        }

        if let Some(path) = config.connection.strip_prefix("surrealkv://") {
            if let Some(parent) = Path::new(path).parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    CodeGraphError::Database(format!(
                        "Failed to create {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
                Self::write_gitignore(parent);
            }
        }

        let db: Surreal<Any> = Surreal::init();
        db.connect(&config.connection).await.map_err(|e| {
            let message = e.to_string();
            if message.contains("already locked") {
                CodeGraphError::Database(format!(
                    "The embedded database {} is open in another process. Stop the other \
                     codegraph process (for example a running `codegraph start`) or set \
                     CODEGRAPH_SURREALDB_URL to use a shared SurrealDB server. ({})",
                    config.connection, message
                ))
            } else {
                CodeGraphError::Database(format!("Failed to open embedded database: {}", message))
            }
        })?;

        handles.insert(config.connection.clone(), db.clone());
        Ok(db)
    }

    /// Keep the store out of version control: `.codegraph/.gitignore` ignoring everything.
    fn write_gitignore(dir: &Path) {
        let gitignore = dir.join(".gitignore");
        if gitignore.exists() {
            return;
        }
        if let Err(e) = std::fs::write(&gitignore, "*\n") {
            warn!("Could not write {}: {}", gitignore.display(), e);
        }
    }

    /// Apply the bundled schema to a store that has none, and warn when the
    /// store was created from a different schema revision.
    async fn ensure_bundled_schema(db: &Surreal<Any>) -> Result<()> {
        let (name, schema) = if env_flag("CODEGRAPH_USE_GRAPH_SCHEMA") {
            ("experimental", BUNDLED_SCHEMA_EXPERIMENTAL)
        } else if env_value("CODEGRAPH_SCHEMA").is_some_and(|v| v.eq_ignore_ascii_case("v1")) {
            ("main", BUNDLED_SCHEMA_MAIN)
        } else {
            ("v2", BUNDLED_SCHEMA_V2)
        };
        let checksum = Sha256::digest(schema.as_bytes())
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();

        if Self::has_table(db, "nodes").await? {
            return Self::refresh_or_warn(db, name, schema, &checksum).await;
        }

        info!("Applying bundled {} schema to new database", name);
        let schema = crate::vector_indexes::VectorIndexMode::from_env()?.initial_schema(schema);
        db.query(schema)
            .await
            .map_err(|e| CodeGraphError::Database(format!("Schema parsing failed: {}", e)))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("Schema application failed: {}", e)))?;

        db.query(
            "CREATE schema_versions:bundled SET version = 0, name = $name, checksum = $checksum, \
             description = 'Bundled schema applied on first open';",
        )
        .bind(("name", name.to_string()))
        .bind(("checksum", checksum))
        .await
        .map_err(|e| CodeGraphError::Database(format!("Failed to record schema: {}", e)))?
        .check()
        .map_err(|e| CodeGraphError::Database(format!("Failed to record schema: {}", e)))?;

        Ok(())
    }

    async fn has_table(db: &Surreal<Any>, table: &str) -> Result<bool> {
        let mut response = db
            .query("INFO FOR DB")
            .await
            .map_err(|e| CodeGraphError::Database(format!("INFO FOR DB failed: {}", e)))?;
        let info: Option<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("INFO FOR DB unreadable: {}", e)))?;
        Ok(info
            .and_then(|v| v.get("tables").cloned())
            .and_then(|t| t.get(table).cloned())
            .is_some())
    }

    /// Bring an existing store up to date with the bundled schema where that is safe.
    ///
    /// Functions and params are defined with `OVERWRITE`, so when the store was created
    /// from an earlier revision of the same schema they are re-applied in place and the
    /// recorded checksum is updated; query fixes then reach existing indexes without a
    /// re-index. Tables, fields and indexes are never touched (vector indexes in
    /// particular are managed separately). A store created from a different schema, or a
    /// schema without re-appliable definitions, only gets a warning.
    async fn refresh_or_warn(
        db: &Surreal<Any>,
        name: &str,
        schema: &str,
        checksum: &str,
    ) -> Result<()> {
        let recorded = async {
            let mut response = db
                .query("SELECT name, checksum FROM schema_versions:bundled")
                .await
                .ok()?;
            let rows: Vec<JsonValue> = response.take(0).ok()?;
            let row = rows.into_iter().next()?;
            Some((
                row.get("name")?.as_str()?.to_string(),
                row.get("checksum")?.as_str()?.to_string(),
            ))
        }
        .await;

        let Some((recorded_name, recorded_checksum)) = recorded else {
            debug!("Embedded database has no bundled-schema record; skipping check");
            return Ok(());
        };
        if recorded_name == name && recorded_checksum == checksum {
            return Ok(());
        }

        let refresh = refreshable_definitions(schema);
        if recorded_name != name || refresh.is_empty() {
            warn!(
                "Embedded database was created from the {} schema at a different revision than \
                 this binary bundles ({}). The stored schema stays in use; to switch, stop all \
                 codegraph processes, delete <project>/.codegraph/db and run `codegraph index` again.",
                recorded_name, name
            );
            return Ok(());
        }

        info!(
            "Updating {} schema functions in the embedded database to this binary's revision",
            name
        );
        db.query(refresh)
            .await
            .map_err(|e| CodeGraphError::Database(format!("Schema function update failed: {}", e)))?
            .check()
            .map_err(|e| {
                CodeGraphError::Database(format!("Schema function update failed: {}", e))
            })?;
        db.query(
            "UPDATE schema_versions:bundled SET checksum = $checksum, \
             description = 'Bundled schema; functions refreshed on open';",
        )
        .bind(("checksum", checksum.to_string()))
        .await
        .map_err(|e| CodeGraphError::Database(format!("Failed to record schema: {}", e)))?
        .check()
        .map_err(|e| CodeGraphError::Database(format!("Failed to record schema: {}", e)))?;
        Ok(())
    }

    /// Initialize database schema with flexible design (unused when schema managed externally)
    #[allow(dead_code)]
    async fn initialize_schema(&self) -> Result<()> {
        info!("Initializing SurrealDB schema");

        // Define flexible schema for nodes table
        // This schema is intentionally loose to allow for easy modifications
        let node_schema = r#"
            DEFINE TABLE IF NOT EXISTS nodes SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS id ON TABLE nodes TYPE string;
            DEFINE FIELD IF NOT EXISTS name ON TABLE nodes TYPE string;
            DEFINE FIELD IF NOT EXISTS node_type ON TABLE nodes TYPE option<string>;
            DEFINE FIELD IF NOT EXISTS language ON TABLE nodes TYPE option<string>;
            DEFINE FIELD IF NOT EXISTS content ON TABLE nodes TYPE option<string>;
            DEFINE FIELD IF NOT EXISTS file_path ON TABLE nodes TYPE option<string>;
            DEFINE FIELD IF NOT EXISTS start_line ON TABLE nodes TYPE option<number>;
            DEFINE FIELD IF NOT EXISTS end_line ON TABLE nodes TYPE option<number>;
            DEFINE FIELD IF NOT EXISTS embedding ON TABLE nodes TYPE option<array<float>>;
            DEFINE FIELD IF NOT EXISTS complexity ON TABLE nodes TYPE option<float>;
            DEFINE FIELD IF NOT EXISTS metadata ON TABLE nodes TYPE option<object>;
            DEFINE FIELD IF NOT EXISTS created_at ON TABLE nodes TYPE datetime DEFAULT time::now();
            DEFINE FIELD IF NOT EXISTS updated_at ON TABLE nodes TYPE datetime DEFAULT time::now();

            -- Indexes for efficient queries
            DEFINE INDEX IF NOT EXISTS idx_nodes_id ON TABLE nodes COLUMNS id UNIQUE;
            DEFINE INDEX IF NOT EXISTS idx_nodes_name ON TABLE nodes COLUMNS name;
            DEFINE INDEX IF NOT EXISTS idx_nodes_type ON TABLE nodes COLUMNS node_type;
            DEFINE INDEX IF NOT EXISTS idx_nodes_language ON TABLE nodes COLUMNS language;
            DEFINE INDEX IF NOT EXISTS idx_nodes_file_path ON TABLE nodes COLUMNS file_path;
        "#;

        // Define edges table for relationships
        let edge_schema = r#"
            DEFINE TABLE IF NOT EXISTS edges SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS id ON TABLE edges TYPE string;
            DEFINE FIELD IF NOT EXISTS from ON TABLE edges TYPE record<nodes>;
            DEFINE FIELD IF NOT EXISTS to ON TABLE edges TYPE record<nodes>;
            DEFINE FIELD IF NOT EXISTS edge_type ON TABLE edges TYPE string;
            DEFINE FIELD IF NOT EXISTS weight ON TABLE edges TYPE float DEFAULT 1.0;
            DEFINE FIELD IF NOT EXISTS metadata ON TABLE edges TYPE option<object>;
            DEFINE FIELD IF NOT EXISTS created_at ON TABLE edges TYPE datetime DEFAULT time::now();

            -- Indexes for graph traversal
            DEFINE INDEX IF NOT EXISTS idx_edges_from ON TABLE edges COLUMNS from;
            DEFINE INDEX IF NOT EXISTS idx_edges_to ON TABLE edges COLUMNS to;
            DEFINE INDEX IF NOT EXISTS idx_edges_type ON TABLE edges COLUMNS edge_type;
        "#;

        // Schema version tracking
        let version_schema = r#"
            DEFINE TABLE IF NOT EXISTS schema_versions SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS version ON TABLE schema_versions TYPE number;
            DEFINE FIELD IF NOT EXISTS applied_at ON TABLE schema_versions TYPE datetime DEFAULT time::now();
            DEFINE FIELD IF NOT EXISTS description ON TABLE schema_versions TYPE string;

            DEFINE INDEX IF NOT EXISTS idx_schema_version ON TABLE schema_versions COLUMNS version UNIQUE;
        "#;

        // Metadata table for system information
        let metadata_schema = r#"
            DEFINE TABLE IF NOT EXISTS metadata SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS key ON TABLE metadata TYPE string;
            DEFINE FIELD IF NOT EXISTS value ON TABLE metadata TYPE option<string | number | bool | object | array>;
            DEFINE FIELD IF NOT EXISTS updated_at ON TABLE metadata TYPE datetime DEFAULT time::now();

            DEFINE INDEX IF NOT EXISTS idx_metadata_key ON TABLE metadata COLUMNS key UNIQUE;
        "#;

        // Execute schema definitions
        self.db.query(node_schema).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to create nodes schema: {}", e))
        })?;

        self.db.query(edge_schema).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to create edges schema: {}", e))
        })?;

        self.db.query(version_schema).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to create versions schema: {}", e))
        })?;

        self.db.query(metadata_schema).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to create metadata schema: {}", e))
        })?;

        // Initialize schema version if not exists
        let _: Option<SchemaVersion> = self
            .db
            .create(("schema_versions", "current"))
            .content(SchemaVersion {
                version: 1,
                applied_at: chrono::Utc::now().to_rfc3339(),
                description: "Initial schema".to_string(),
            })
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to set schema version: {}", e))
            })?;

        *self.schema_version.write().unwrap() = 1;

        info!("Schema initialized successfully");
        Ok(())
    }

    /// Run database migrations (unused when schema managed externally)
    #[allow(dead_code)]
    async fn migrate(&self) -> Result<()> {
        let _current_version = *self.schema_version.read().unwrap();
        info!("Running migrations from version {}", _current_version);
        // TODO: Fix lifetime issues with async closures in migrations
        // Migrations temporarily disabled
        Ok(())
    }

    /// Vector search using SurrealDB HNSW indexes
    /// Returns node IDs and similarity scores
    pub async fn vector_search_knn(
        &self,
        embedding_column: &str,
        query_embedding: Vec<f32>,
        limit: usize,
        ef_search: usize,
    ) -> Result<Vec<(String, f32)>> {
        let column = normalized_embedding_column(embedding_column).ok_or_else(|| {
            CodeGraphError::Configuration(format!(
                "Unsupported embedding column '{}'",
                embedding_column
            ))
        })?;

        info!(
            "Executing HNSW vector search on {} with limit={}, ef_search={}",
            column, limit, ef_search
        );

        // Convert f32 to f64 for SurrealDB
        let query_vec: Vec<f64> = query_embedding.iter().map(|&f| f as f64).collect();

        // SurrealDB HNSW search using <|K,EF|> operator
        // vector::distance::knn() reuses pre-computed distance from HNSW
        let query = format!(
            r#"
            SELECT id, vector::distance::knn() AS score
            FROM nodes
            WHERE {column} <|$limit,$ef_search|> $query_embedding
            ORDER BY score ASC
            LIMIT $limit
        "#
        );

        let mut result = self
            .db
            .query(&query)
            .bind(("query_embedding", query_vec))
            .bind(("limit", limit))
            .bind(("ef_search", ef_search))
            .await
            .map_err(|e| CodeGraphError::Database(format!("HNSW search failed: {}", e)))?;

        #[derive(Serialize, Deserialize, surrealdb::types::SurrealValue)]
        struct SearchResult {
            id: String,
            score: f64,
        }

        let results: Vec<SearchResult> = result.take(0).map_err(|e| {
            CodeGraphError::Database(format!("Failed to extract search results: {}", e))
        })?;

        Ok(results
            .into_iter()
            .map(|r| (r.id, r.score as f32))
            .collect())
    }

    /// Vector search with metadata filtering
    pub async fn vector_search_with_metadata(
        &self,
        embedding_column: &str,
        query_embedding: Vec<f32>,
        limit: usize,
        ef_search: usize,
        node_type: Option<String>,
        language: Option<String>,
        file_path_pattern: Option<String>,
    ) -> Result<Vec<(String, f32)>> {
        let column = normalized_embedding_column(embedding_column).ok_or_else(|| {
            CodeGraphError::Configuration(format!(
                "Unsupported embedding column '{}'",
                embedding_column
            ))
        })?;

        info!(
            "Executing filtered HNSW search on {}: type={:?}, lang={:?}, path={:?}",
            column, node_type, language, file_path_pattern
        );

        let query_vec: Vec<f64> = query_embedding.iter().map(|&f| f as f64).collect();

        // Build dynamic WHERE clause
        let mut where_clauses = vec![format!("{column} <|$limit,$ef_search|> $query_embedding")];

        if let Some(ref nt) = node_type {
            where_clauses.push(format!("node_type = '{}'", nt));
        }

        if let Some(ref lang) = language {
            where_clauses.push(format!("language = '{}'", lang));
        }

        if let Some(ref path) = file_path_pattern {
            // Support OR patterns like "src/|lib/"
            if path.contains('|') {
                let patterns: Vec<String> = path
                    .split('|')
                    .map(|p| format!("file_path CONTAINS '{}'", p))
                    .collect();
                where_clauses.push(format!("({})", patterns.join(" OR ")));
            } else {
                where_clauses.push(format!("file_path CONTAINS '{}'", path));
            }
        }

        let where_clause = where_clauses.join(" AND ");

        let query = format!(
            r#"
            SELECT id, vector::distance::knn() AS score
            FROM nodes
            WHERE {}
            ORDER BY score ASC
            LIMIT $limit
        "#,
            where_clause
        );

        let mut result = self
            .db
            .query(&query)
            .bind(("query_embedding", query_vec))
            .bind(("limit", limit))
            .bind(("ef_search", ef_search))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Filtered HNSW search failed: {}", e)))?;

        #[derive(Serialize, Deserialize, surrealdb::types::SurrealValue)]
        struct SearchResult {
            id: String,
            score: f64,
        }

        let results: Vec<SearchResult> = result.take(0).map_err(|e| {
            CodeGraphError::Database(format!("Failed to extract filtered results: {}", e))
        })?;

        Ok(results
            .into_iter()
            .map(|r| (r.id, r.score as f32))
            .collect())
    }

    /// Get multiple nodes by their IDs in one query
    pub async fn get_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<CodeNode>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        debug!("Getting {} nodes by IDs", ids.len());

        // Check cache first for all IDs
        let mut nodes = Vec::new();
        let mut missing_ids = Vec::new();

        if self.config.cache_enabled {
            for id_str in ids {
                if let Ok(id) = NodeId::parse_str(id_str) {
                    match self.node_cache.get(&id) {
                        Some(cached) => {
                            nodes.push(cached.clone());
                        }
                        _ => {
                            missing_ids.push(id_str.clone());
                        }
                    }
                }
            }
        } else {
            missing_ids = ids.to_vec();
        }

        // Fetch missing nodes from database
        if !missing_ids.is_empty() {
            let query = "SELECT * FROM nodes WHERE id IN $ids";
            let mut result = self
                .db
                .query(query)
                .bind(("ids", missing_ids))
                .await
                .map_err(|e| CodeGraphError::Database(format!("Failed to query nodes: {}", e)))?;

            let db_nodes: Vec<HashMap<String, JsonValue>> = result.take(0).map_err(|e| {
                CodeGraphError::Database(format!("Failed to extract query results: {}", e))
            })?;

            for data in db_nodes {
                let node = self.surreal_to_node(data)?;

                // Update cache
                if self.config.cache_enabled {
                    self.node_cache.insert(node.id, node.clone());
                }

                nodes.push(node);
            }
        }

        Ok(nodes)
    }

    /// Convert CodeNode to SurrealDB-compatible format
    fn node_to_surreal(&self, node: &CodeNode) -> Result<SurrealNodeRecord> {
        let metadata = if node.metadata.attributes.is_empty() {
            None
        } else {
            let metadata_json =
                serde_json::to_value(&node.metadata.attributes).unwrap_or(JsonValue::Null);
            let compressed = codegraph_core::compress_json(&metadata_json);
            Some(JsonValue::String(compressed))
        };

        let (
            embedding_384,
            embedding_768,
            embedding_1024,
            embedding_2048,
            embedding_2560,
            embedding_4096,
        ) = if let Some(values) = &node.embedding {
            // Keep f32: the provider emits f32, SurrealDB stores F32, and serialising a
            // widened f64 roughly doubles each element's JSON text (0.12345679 vs
            // 0.12345679104328156), i.e. ~7 KB extra per 1024-d row.
            let embedding_vec: Vec<f32> = values.to_vec();
            match values.len() {
                384 => (Some(embedding_vec), None, None, None, None, None),
                768 => (None, Some(embedding_vec), None, None, None, None),
                1024 => (None, None, Some(embedding_vec), None, None, None),
                2560 => (None, None, None, None, Some(embedding_vec), None),
                4096 => (None, None, None, None, None, Some(embedding_vec)),
                _ => (None, None, None, Some(embedding_vec), None, None),
            }
        } else {
            (None, None, None, None, None, None)
        };

        let embedding_model = node.metadata.attributes.get("embedding_model").cloned();

        let content = node
            .content
            .as_ref()
            .map(|c| codegraph_core::compress_to_string(&c));

        Ok(SurrealNodeRecord {
            id: node.id.to_string(),
            name: node.name.to_string(),
            node_type: node.node_type.as_ref().map(|value| format!("{:?}", value)),
            language: node.language.as_ref().map(|value| format!("{:?}", value)),
            content,
            file_path: node.location.file_path.to_string(),
            start_line: node.location.line,
            end_line: node.location.end_line,
            embedding_384,
            embedding_768,
            embedding_1024,
            embedding_2048,
            embedding_2560,
            embedding_4096,
            embedding_model,
            complexity: node.complexity,
            metadata,
            project_id: node.metadata.attributes.get("project_id").cloned(),
            organization_id: node.metadata.attributes.get("organization_id").cloned(),
            repository_url: node.metadata.attributes.get("repository_url").cloned(),
            domain: node.metadata.attributes.get("domain").cloned(),
            chunk_count: node
                .metadata
                .attributes
                .get("chunk_count")
                .and_then(|v| v.parse::<i64>().ok()),
        })
    }

    pub async fn upsert_nodes_batch(&mut self, nodes: &[CodeNode]) -> Result<()> {
        self.upsert_nodes_batch_with_timeout(nodes, write_timeout_from_env())
            .await
    }

    /// Upsert nodes with an explicit client-side deadline.
    ///
    /// A stalled SurrealKV flush makes the server stop answering; without a deadline the
    /// index run waits forever (see `deploy/README.md`). The deadline turns that into a
    /// loud error the writer and the indexer can surface.
    pub async fn upsert_nodes_batch_with_timeout(
        &mut self,
        nodes: &[CodeNode],
        timeout: std::time::Duration,
    ) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }

        let mut records = Vec::with_capacity(nodes.len());
        for node in nodes {
            records.push(self.node_to_surreal(node)?);
        }

        let record_count = records.len();
        let response = with_write_deadline(
            timeout,
            self.db.query(UPSERT_NODES_QUERY).bind(("data", records)),
            "Node batch upsert",
            record_count,
        )
        .await?
        .map_err(|e| {
            CodeGraphError::Database(format!(
                "Failed to upsert node batch ({} items): {}",
                record_count,
                truncate_surreal_error(&e)
            ))
        })?;
        response.check().map_err(|e| {
            CodeGraphError::Database(format!(
                "Node batch upsert returned error ({} items): {}",
                record_count,
                truncate_surreal_error(&e)
            ))
        })?;

        if self.config.cache_enabled {
            for node in nodes {
                self.node_cache.insert(node.id, node.clone());
            }
        }

        Ok(())
    }

    pub async fn upsert_edges_batch(&mut self, edges: &[CodeEdge]) -> Result<()> {
        self.upsert_edges_batch_with_timeout(edges, write_timeout_from_env())
            .await
    }

    /// Upsert edges with an explicit client-side deadline (see
    /// [`Self::upsert_nodes_batch_with_timeout`]).
    pub async fn upsert_edges_batch_with_timeout(
        &mut self,
        edges: &[CodeEdge],
        timeout: std::time::Duration,
    ) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }

        let payloads: Vec<JsonValue> = edges
            .iter()
            .map(|record| {
                let metadata_value =
                    serde_json::to_value(&record.metadata).unwrap_or_else(|_| JsonValue::Null);
                json!({
                    "id": record.id.to_string(),
                    "from": record.from.to_string(),
                    "to": record.to.to_string(),
                    "edge_type": record.edge_type.to_string(),
                    "weight": record.weight,
                    "metadata": metadata_value,
                    "project_id": record.project_id.clone(),
                })
            })
            .collect();

        let record_count = payloads.len();
        let response = with_write_deadline(
            timeout,
            self.db
                .query(UPSERT_EDGES_QUERY)
                .bind(("data", strip_null_fields(payloads))),
            "Edge batch upsert",
            record_count,
        )
        .await?
        .map_err(|e| {
            CodeGraphError::Database(format!(
                "Failed to upsert edge batch ({} items): {}",
                record_count,
                truncate_surreal_error(&e)
            ))
        })?;
        response.check().map_err(|e| {
            CodeGraphError::Database(format!(
                "Edge batch upsert returned error ({} items): {}",
                record_count,
                truncate_surreal_error(&e)
            ))
        })?;

        Ok(())
    }

    pub async fn upsert_symbol_embeddings_batch(
        &self,
        records: &[SymbolEmbeddingRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        // SurrealKV reports concurrent write-write conflicts on overlapping keys as
        // "Transaction write conflict ... can be retried". Those are transient: retry
        // with backoff instead of failing the whole indexing run. Non-retryable
        // errors still fail fast.
        const MAX_ATTEMPTS: u32 = 6;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;

            let outcome = self
                .db
                .query(UPSERT_SYMBOL_EMBEDDINGS_QUERY)
                .bind(("data", records.to_vec()))
                .await
                .map_err(|e| {
                    CodeGraphError::Database(format!(
                        "Failed to upsert symbol embedding batch ({} items): {}",
                        records.len(),
                        truncate_surreal_error(&e)
                    ))
                })
                .and_then(|resp| {
                    resp.check().map(|_| ()).map_err(|e| {
                        CodeGraphError::Database(format!(
                            "Symbol batch upsert returned error: {}",
                            truncate_surreal_error(&e)
                        ))
                    })
                });

            match outcome {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let retryable = is_retryable_write_conflict(&err.to_string());
                    if retryable && attempt < MAX_ATTEMPTS {
                        let backoff_ms = 20u64 << (attempt - 1);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    pub async fn upsert_chunk_embeddings_batch(
        &self,
        records: &[ChunkEmbeddingRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        // Serialize to owned values to satisfy static requirement of Surreal bindings
        let owned: Vec<ChunkEmbeddingRecord> = records.to_vec();

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.db
                .query(UPSERT_CHUNK_EMBEDDINGS_QUERY)
                .bind(("data", owned)),
        )
        .await
        .map_err(|_| {
            CodeGraphError::Database(format!(
                "Chunk upsert timed out after 10s ({} items)",
                records.len()
            ))
        })?
        .map_err(|e| {
            CodeGraphError::Database(format!(
                "Failed to upsert chunk embedding batch ({} items): {}",
                records.len(),
                truncate_surreal_error(&e)
            ))
        })?;

        // Ensure Surreal didn’t return per-statement errors that would otherwise be hidden
        resp.check().map_err(|e| {
            CodeGraphError::Database(format!(
                "Surreal chunk batch returned error ({} items): {}",
                records.len(),
                truncate_surreal_error(&e)
            ))
        })?;

        Ok(())
    }

    /// Resilient chunk upsert: on Surreal computation depth or connection reset, split batch and retry.
    pub async fn upsert_chunk_embeddings_resilient(
        &self,
        records: &[ChunkEmbeddingRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        // First, try a bulk INSERT (shallow query, no FOR loop)
        if let Err(err) = self.insert_chunk_embeddings_batch(records).await {
            let msg = err.to_string();
            let duplicate = msg.to_lowercase().contains("duplicate");
            let depth_hit = msg.contains("excessive computation depth")
                || msg.contains("ComputationDepth")
                || msg.contains("connection reset");

            // Only fall through to upsert/backoff on duplicate or depth issues; otherwise fail fast
            if !duplicate && !depth_hit {
                return Err(err);
            }
        } else {
            return Ok(());
        }

        // Iterative backoff to avoid recursive async futures
        let mut queue: Vec<(Vec<ChunkEmbeddingRecord>, u8)> = vec![(records.to_vec(), 3)];

        while let Some((mut batch, remaining)) = queue.pop() {
            if batch.is_empty() {
                continue;
            }

            match self.upsert_chunk_embeddings_batch(&batch).await {
                Ok(()) => {}
                Err(err) => {
                    let msg = err.to_string();
                    let depth_hit = msg.contains("excessive computation depth")
                        || msg.contains("ComputationDepth")
                        || msg.contains("connection reset");

                    if depth_hit && remaining > 0 && batch.len() > 1 {
                        let mid = batch.len() / 2;
                        let right = batch.split_off(mid);
                        queue.push((right, remaining - 1));
                        queue.push((batch, remaining - 1));
                    } else if depth_hit {
                        // Fall back to per-record upserts to keep queries shallow
                        for rec in batch {
                            self.upsert_chunk_embedding_single(&rec).await?;
                        }
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        Ok(())
    }

    async fn insert_chunk_embeddings_batch(&self, records: &[ChunkEmbeddingRecord]) -> Result<()> {
        let owned: Vec<ChunkEmbeddingRecord> = records.to_vec();
        let resp = self
            .db
            .query(INSERT_CHUNK_EMBEDDINGS_QUERY)
            .bind(("batch", owned))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!(
                    "Failed to insert chunk embedding batch ({} items): {}",
                    records.len(),
                    truncate_surreal_error(&e)
                ))
            })?;

        resp.check().map_err(|e| {
            CodeGraphError::Database(format!(
                "Surreal chunk insert returned error ({} items): {}",
                records.len(),
                truncate_surreal_error(&e)
            ))
        })?;
        Ok(())
    }

    async fn upsert_chunk_embedding_single(&self, record: &ChunkEmbeddingRecord) -> Result<()> {
        let owned = record.clone();
        let resp = self
            .db
            .query(UPSERT_CHUNK_EMBEDDING_SINGLE_QUERY)
            .bind(("doc", owned))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!(
                    "Failed to upsert chunk embedding: {}",
                    truncate_surreal_error(&e)
                ))
            })?;

        resp.check().map_err(|e| {
            CodeGraphError::Database(format!(
                "Surreal chunk upsert returned error: {}",
                truncate_surreal_error(&e)
            ))
        })?;

        Ok(())
    }

    pub async fn update_node_embeddings_batch(
        &self,
        records: &[NodeEmbeddingRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let mut by_column: std::collections::BTreeMap<&str, Vec<JsonValue>> = Default::default();
        for record in records {
            let column = normalized_embedding_column(record.column).ok_or_else(|| {
                CodeGraphError::Database(format!(
                    "Unsupported embedding column '{}'",
                    record.column
                ))
            })?;
            by_column
                .entry(column)
                .or_default()
                .push(json!({ "id": record.id, "embedding": record.embedding }));
        }
        for (column, payload) in by_column {
            let query = format!(
                "FOR $doc IN $data {{ UPDATE type::record('nodes', $doc.id) SET {column} = $doc.embedding RETURN NONE; }}"
            );
            self.db
                .query(query)
                .bind(("data", payload))
                .await
                .map_err(|e| {
                    CodeGraphError::Database(format!(
                        "Node embedding batch failed: {}",
                        truncate_surreal_error(&e)
                    ))
                })?
                .check()
                .map_err(|e| {
                    CodeGraphError::Database(format!(
                        "Node embedding batch returned error: {}",
                        truncate_surreal_error(&e)
                    ))
                })?;
        }

        Ok(())
    }

    /// Convert SurrealDB result to CodeNode
    fn surreal_to_node(&self, data: HashMap<String, JsonValue>) -> Result<CodeNode> {
        use codegraph_core::{Language, Location, Metadata, NodeType, SharedStr};

        let id_str = data
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CodeGraphError::Parse("Missing node id".to_string()))?;
        let id = NodeId::parse_str(id_str)
            .map_err(|e| CodeGraphError::Parse(format!("Invalid node id: {}", e)))?;

        let name = data
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CodeGraphError::Parse("Missing node name".to_string()))?;

        let node_type = data
            .get("node_type")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<NodeType>(&format!("\"{}\"", s)).ok());

        let language = data
            .get("language")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Language>(&format!("\"{}\"", s)).ok());

        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| SharedStr::from(s));

        let file_path = data.get("file_path").and_then(|v| v.as_str()).unwrap_or("");

        let start_line = data.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let end_line = data.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let embedding = [
            "embedding_4096",
            "embedding_2048",
            "embedding_1024",
            "embedding_384",
        ]
        .iter()
        .find_map(|field| data.get(*field).and_then(|v| v.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect()
        });

        let complexity = data
            .get("complexity")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32);

        let mut attributes = std::collections::HashMap::new();
        if let Some(meta_obj) = data.get("metadata").and_then(|v| v.as_object()) {
            for (key, value) in meta_obj {
                if let Some(val_str) = value.as_str() {
                    attributes.insert(key.clone(), val_str.to_string());
                }
            }
        }

        let metadata = Metadata {
            attributes,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        Ok(CodeNode {
            id,
            name: SharedStr::from(name),
            node_type,
            language,
            location: Location {
                file_path: file_path.to_string(),
                line: start_line as u32,
                column: 0,
                end_line: if end_line > 0 {
                    Some(end_line as u32)
                } else {
                    None
                },
                end_column: None,
            },
            span: None,
            content,
            metadata,
            embedding,
            complexity,
        })
    }

    pub async fn add_code_edge(&mut self, edge: CodeEdge) -> Result<()> {
        self.upsert_edges_batch(std::slice::from_ref(&edge)).await
    }

    pub async fn add_code_edges(&mut self, edges: Vec<CodeEdge>) -> Result<()> {
        self.upsert_edges_batch(&edges).await
    }

    pub async fn upsert_symbol_embedding(&self, record: SymbolEmbeddingUpsert<'_>) -> Result<()> {
        let owned = SymbolEmbeddingRecord::from(&record);
        self.upsert_symbol_embeddings_batch(std::slice::from_ref(&owned))
            .await
    }

    pub async fn upsert_project_metadata(&self, record: ProjectMetadataRecord) -> Result<()> {
        let query = r#"
            UPSERT type::record('project_metadata', $id) SET
                project_id = $pid,
                name = $name,
                root_path = $root,
                primary_language = $lang,
                file_count = $files,
                node_count = $nodes,
                edge_count = $edges,
                avg_coverage_score = $cov,
                last_analyzed = <datetime>$last_analyzed,
                codegraph_version = $ver,
                organization_id = $org,
                domain = $dom,
                metadata = $meta,
                updated_at = time::now();
        "#;

        self.db
            .query(query)
            .bind(("id", record.project_id.clone()))
            .bind(("pid", record.project_id))
            .bind(("name", record.name))
            .bind(("root", record.root_path))
            .bind(("lang", record.primary_language))
            .bind(("files", record.file_count))
            .bind(("nodes", record.node_count))
            .bind(("edges", record.edge_count))
            .bind(("cov", record.avg_coverage_score))
            .bind(("last_analyzed", record.last_analyzed))
            .bind(("ver", record.codegraph_version))
            .bind(("org", record.organization_id))
            .bind(("dom", record.domain))
            .bind(("meta", record.metadata))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to upsert project metadata: {}", e))
            })?
            .check()
            .map_err(|e| {
                CodeGraphError::Database(format!("Project metadata upsert error: {}", e))
            })?;

        Ok(())
    }

    /// Upsert single file metadata record
    pub async fn upsert_file_metadata(&self, record: &FileMetadataRecord) -> Result<()> {
        let query = r#"
            UPSERT type::record('file_metadata', $id) SET
                file_path = $file_path,
                project_id = $project_id,
                content_hash = $content_hash,
                modified_at = <datetime>$modified_at,
                file_size = $file_size,
                last_indexed_at = <datetime>$last_indexed_at,
                node_count = $node_count,
                edge_count = $edge_count,
                language = $language,
                parse_errors = $parse_errors,
                updated_at = time::now();
        "#;

        let resp = self
            .db
            .query(query)
            .bind(("id", record.id()))
            .bind(("file_path", record.file_path.clone()))
            .bind(("project_id", record.project_id.clone()))
            .bind(("content_hash", record.content_hash.clone()))
            .bind(("modified_at", record.modified_at))
            .bind(("file_size", record.file_size))
            .bind(("last_indexed_at", record.last_indexed_at))
            .bind(("node_count", record.node_count))
            .bind(("edge_count", record.edge_count))
            .bind(("language", record.language.clone()))
            .bind(("parse_errors", record.parse_errors.clone()))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to upsert file metadata: {}", e))
            })?;

        resp.check().map_err(|e| {
            CodeGraphError::Database(format!("Failed to upsert file metadata: {}", e))
        })?;

        Ok(())
    }

    /// Batch upsert file metadata records
    pub async fn upsert_file_metadata_batch(&self, records: &[FileMetadataRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let query = r#"
LET $batch = array::map($data, |$doc| {
    id: type::record('file_metadata', $doc.id),
    file_path: $doc.file_path,
    project_id: $doc.project_id,
    content_hash: $doc.content_hash,
    modified_at: <datetime>$doc.modified_at,
    file_size: $doc.file_size,
    last_indexed_at: <datetime>$doc.last_indexed_at,
    node_count: $doc.node_count,
    edge_count: $doc.edge_count,
    language: $doc.language,
    parse_errors: $doc.parse_errors,
    updated_at: time::now(),
});
INSERT INTO file_metadata $batch ON DUPLICATE KEY UPDATE
    file_path = $input.file_path,
    project_id = $input.project_id,
    content_hash = $input.content_hash,
    modified_at = $input.modified_at,
    file_size = $input.file_size,
    last_indexed_at = $input.last_indexed_at,
    node_count = $input.node_count,
    edge_count = $input.edge_count,
    language = $input.language,
    parse_errors = $input.parse_errors,
    updated_at = $input.updated_at
RETURN NONE;
"#;

        let payloads: Vec<JsonValue> = records
            .iter()
            .map(|r| {
                json!({
                    "id": r.id(),
                    "file_path": r.file_path,
                    "project_id": r.project_id,
                    "content_hash": r.content_hash,
                    "modified_at": r.modified_at,
                    "file_size": r.file_size,
                    "last_indexed_at": r.last_indexed_at,
                    "node_count": r.node_count,
                    "edge_count": r.edge_count,
                    "language": r.language,
                    "parse_errors": r.parse_errors,
                })
            })
            .collect();

        self.db
            .query(query)
            .bind(("data", strip_null_fields(payloads)))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!(
                    "Failed to batch upsert file metadata ({} items): {}",
                    records.len(),
                    truncate_surreal_error(&e)
                ))
            })?
            .check()
            .map_err(|e| {
                CodeGraphError::Database(format!(
                    "File metadata batch upsert returned error: {}",
                    truncate_surreal_error(&e)
                ))
            })?;

        debug!("Batch upserted {} file metadata records", records.len());
        Ok(())
    }

    /// Get all file metadata for a project
    pub async fn get_file_metadata_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<FileMetadataRecord>> {
        let query = "SELECT * FROM file_metadata WHERE project_id = $project_id";
        let mut result = self
            .db
            .query(query)
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to query file metadata: {}", e))
            })?;

        let records: Vec<FileMetadataRecord> = result.take(0).map_err(|e| {
            CodeGraphError::Database(format!("Failed to extract file metadata results: {}", e))
        })?;

        debug!(
            "Retrieved {} file metadata records for project {}",
            records.len(),
            project_id
        );
        Ok(records)
    }

    /// Delete file metadata records for specific files
    pub async fn delete_file_metadata_for_files(
        &self,
        project_id: &str,
        file_paths: &[String],
    ) -> Result<()> {
        if file_paths.is_empty() {
            return Ok(());
        }

        let query =
            "DELETE file_metadata WHERE project_id = $project_id AND file_path IN $file_paths";
        self.db
            .query(query)
            .bind(("project_id", project_id.to_string()))
            .bind(("file_paths", file_paths.to_vec()))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to delete file metadata: {}", e))
            })?;

        debug!(
            "Deleted {} file metadata records for project {}",
            file_paths.len(),
            project_id
        );
        Ok(())
    }

    /// Delete all nodes for a project
    pub async fn delete_nodes_for_project(&self, project_id: &str) -> Result<usize> {
        let query = "DELETE nodes WHERE project_id = $project_id RETURN BEFORE";
        let mut result = self
            .db
            .query(query)
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to delete nodes: {}", e)))?;

        let deleted: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let count = deleted.len();

        // Clear cache for deleted nodes
        if self.config.cache_enabled {
            for data in deleted {
                if let Some(id_str) = data.get("id").and_then(|v| v.as_str()) {
                    if let Ok(id) = NodeId::parse_str(id_str) {
                        self.node_cache.remove(&id);
                    }
                }
            }
        }

        info!("Deleted {} nodes for project {}", count, project_id);
        Ok(count)
    }

    /// Delete all edges where from or to node belongs to project
    pub async fn delete_edges_for_project(&self, project_id: &str) -> Result<usize> {
        let mut result = self
            .db
            .query("DELETE edges WHERE project_id = $project_id RETURN BEFORE")
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to delete edges: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("Edge deletion returned error: {e}")))?;

        let deleted: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let count = deleted.len();

        info!("Deleted {} edges for project {}", count, project_id);
        Ok(count)
    }

    /// Delete all symbol embeddings for a project
    pub async fn delete_symbol_embeddings_for_project(&self, project_id: &str) -> Result<usize> {
        let query = "DELETE symbol_embeddings WHERE project_id = $project_id RETURN BEFORE";
        let mut result = self
            .db
            .query(query)
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to delete symbol embeddings: {}", e))
            })?;

        let deleted: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let count = deleted.len();

        info!(
            "Deleted {} symbol embeddings for project {}",
            count, project_id
        );
        Ok(count)
    }

    /// Delete all file metadata for a project
    pub async fn delete_file_metadata_for_project(&self, project_id: &str) -> Result<usize> {
        let query = "DELETE file_metadata WHERE project_id = $project_id RETURN BEFORE";
        let mut result = self
            .db
            .query(query)
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| {
                CodeGraphError::Database(format!("Failed to delete file metadata: {}", e))
            })?;

        let deleted: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let count = deleted.len();

        info!(
            "Deleted {} file metadata records for project {}",
            count, project_id
        );
        Ok(count)
    }

    /// Delete all chunks for a project
    pub async fn delete_chunks_for_project(&self, project_id: &str) -> Result<usize> {
        let query = "DELETE chunks WHERE project_id = $project_id RETURN BEFORE";
        let mut result = self
            .db
            .query(query)
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to delete chunks: {}", e)))?;

        let deleted: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let count = deleted.len();

        info!("Deleted {} chunks for project {}", count, project_id);
        Ok(count)
    }

    /// Clean slate: Delete ALL data for a project (nodes, edges, chunks, embeddings, file metadata)
    /// Used when --force flag is set
    pub async fn clean_project_data(&self, project_id: &str) -> Result<()> {
        self.db
            .query(
                r#"
            BEGIN TRANSACTION;
            DELETE edges WHERE project_id = $project;
            DELETE chunks WHERE project_id = $project;
            DELETE symbol_embeddings WHERE project_id = $project;
            DELETE nodes WHERE project_id = $project;
            DELETE file_metadata WHERE project_id = $project;
            DELETE project_metadata WHERE project_id = $project;
            COMMIT TRANSACTION;
        "#,
            )
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Project cleanup failed: {e}")))?
            .check()
            .map_err(|e| {
                CodeGraphError::Database(format!("Project cleanup returned error: {e}"))
            })?;
        self.node_cache.retain(|_, node| {
            node.metadata
                .attributes
                .get("project_id")
                .map(String::as_str)
                != Some(project_id)
        });
        Ok(())
    }

    /// Clean up orphaned records (chunks without parent nodes, edges with missing endpoints)
    /// Returns counts of deleted orphans
    pub async fn cleanup_orphans(&self) -> Result<OrphanCleanupStats> {
        info!("🧹 Starting orphan cleanup");

        // Delete chunks where parent_node doesn't exist in nodes table
        let orphan_chunks_query =
            "DELETE chunks WHERE parent_node NOT IN (SELECT VALUE id FROM nodes) RETURN BEFORE";
        let mut result = self.db.query(orphan_chunks_query).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to delete orphan chunks: {}", e))
        })?;
        let orphan_chunks: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let chunks_deleted = orphan_chunks.len();

        // Delete edges where from node doesn't exist
        let orphan_edges_from_query =
            "DELETE edges WHERE from NOT IN (SELECT VALUE id FROM nodes) RETURN BEFORE";
        let mut result = self.db.query(orphan_edges_from_query).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to delete orphan edges (from): {}", e))
        })?;
        let orphan_edges_from: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();

        // Delete edges where to node doesn't exist
        let orphan_edges_to_query =
            "DELETE edges WHERE to NOT IN (SELECT VALUE id FROM nodes) RETURN BEFORE";
        let mut result = self.db.query(orphan_edges_to_query).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to delete orphan edges (to): {}", e))
        })?;
        let orphan_edges_to: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let edges_deleted = orphan_edges_from.len() + orphan_edges_to.len();

        // Delete symbol_embeddings where node doesn't exist
        let orphan_symbols_query = "DELETE symbol_embeddings WHERE node_id NOT IN (SELECT VALUE id FROM nodes) RETURN BEFORE";
        let mut result = self.db.query(orphan_symbols_query).await.map_err(|e| {
            CodeGraphError::Database(format!("Failed to delete orphan symbol embeddings: {}", e))
        })?;
        let orphan_symbols: Vec<HashMap<String, JsonValue>> = result.take(0).unwrap_or_default();
        let symbols_deleted = orphan_symbols.len();

        let stats = OrphanCleanupStats {
            chunks_deleted,
            edges_deleted,
            symbols_deleted,
        };

        info!(
            "🧹 Orphan cleanup complete: {} chunks, {} edges, {} symbols deleted",
            stats.chunks_deleted, stats.edges_deleted, stats.symbols_deleted
        );

        Ok(stats)
    }

    pub async fn update_node_embedding(&self, node_id: NodeId, embedding: &[f32]) -> Result<()> {
        let embedding_column = surreal_embedding_column_for_dimension(embedding.len());
        let record = NodeEmbeddingRecord {
            id: node_id.to_string(),
            column: embedding_column,
            embedding: embedding.to_vec(),
            updated_at: Utc::now(),
        };
        self.update_node_embeddings_batch(std::slice::from_ref(&record))
            .await
    }
}

#[async_trait]
impl GraphStore for SurrealDbStorage {
    async fn add_node(&mut self, node: CodeNode) -> Result<()> {
        self.upsert_nodes_batch(std::slice::from_ref(&node)).await
    }

    async fn get_node(&self, id: NodeId) -> Result<Option<CodeNode>> {
        // Check cache first
        if self.config.cache_enabled {
            if let Some(cached) = self.node_cache.get(&id) {
                return Ok(Some(cached.clone()));
            }
        }

        let node_id = id.to_string();
        let result: Option<HashMap<String, JsonValue>> = self
            .db
            .select(("nodes", node_id.as_str()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to get node: {}", e)))?;

        match result {
            Some(data) => {
                let node = self.surreal_to_node(data)?;

                // Update cache
                if self.config.cache_enabled {
                    self.node_cache.insert(id, node.clone());
                }

                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    async fn update_node(&mut self, node: CodeNode) -> Result<()> {
        debug!("Updating node: {}", node.id);

        let data = self.node_to_surreal(&node)?;
        let node_id = node.id.to_string();

        let _: Option<HashMap<String, JsonValue>> = self
            .db
            .update(("nodes", node_id.as_str()))
            .content(data)
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to update node: {}", e)))?;

        // Update cache
        if self.config.cache_enabled {
            self.node_cache.insert(node.id, node);
        }

        Ok(())
    }

    async fn remove_node(&mut self, id: NodeId) -> Result<()> {
        debug!("Removing node: {}", id);

        let node_id = id.to_string();
        let _: Option<HashMap<String, JsonValue>> = self
            .db
            .delete(("nodes", node_id.as_str()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to delete node: {}", e)))?;

        // Remove from cache
        if self.config.cache_enabled {
            self.node_cache.remove(&id);
        }

        Ok(())
    }

    async fn find_nodes_by_name(&self, name: &str) -> Result<Vec<CodeNode>> {
        debug!("Finding nodes by name: {}", name);

        let query = "SELECT * FROM nodes WHERE name = $name";
        let name_owned = name.to_string();
        let mut result = self
            .db
            .query(query)
            .bind(("name", name_owned))
            .await
            .map_err(|e| CodeGraphError::Database(format!("Failed to query nodes: {}", e)))?;

        let nodes: Vec<HashMap<String, JsonValue>> = result.take(0).map_err(|e| {
            CodeGraphError::Database(format!("Failed to extract query results: {}", e))
        })?;

        nodes
            .into_iter()
            .map(|data| self.surreal_to_node(data))
            .collect()
    }
}

#[cfg(test)]
pub(crate) use crate::storage::hex_lower;
pub use crate::storage::{
    ChunkEmbeddingRecord, FileMetadataRecord, NodeEmbeddingRecord, ProjectMetadataRecord,
    SURR_EMBEDDING_COLUMN_384, SURR_EMBEDDING_COLUMN_768, SURR_EMBEDDING_COLUMN_1024,
    SURR_EMBEDDING_COLUMN_1536, SURR_EMBEDDING_COLUMN_2048, SURR_EMBEDDING_COLUMN_2560,
    SURR_EMBEDDING_COLUMN_3072, SURR_EMBEDDING_COLUMN_4096, SymbolEmbeddingRecord,
};

pub struct SymbolEmbeddingUpsert<'a> {
    pub symbol: &'a str,
    pub normalized_symbol: &'a str,
    pub project_id: &'a str,
    pub organization_id: Option<&'a str>,
    pub embedding: &'a [f32],
    pub embedding_model: &'a str,
    pub embedding_column: &'a str,
    pub node_id: Option<&'a str>,
    pub source_edge_id: Option<&'a str>,
    pub metadata: Option<JsonValue>,
}

impl<'a> From<&SymbolEmbeddingUpsert<'a>> for SymbolEmbeddingRecord {
    fn from(record: &SymbolEmbeddingUpsert<'a>) -> Self {
        SymbolEmbeddingRecord::new(
            record.project_id,
            record.organization_id,
            record.symbol,
            record.normalized_symbol,
            record.embedding,
            record.embedding_model,
            record.embedding_column,
            record.node_id,
            record.source_edge_id,
            record.metadata.clone(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SurrealNodeRecord {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>, // Back to String (Base64 encoded if compressed)
    file_path: String,
    start_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_384: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_768: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_1024: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_2048: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_2560: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_4096: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    complexity: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<JsonValue>, // JsonValue but contains strings if compressed
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_count: Option<i64>,
}

fn normalized_embedding_column(column: &str) -> Option<&'static str> {
    match column {
        SURR_EMBEDDING_COLUMN_384 => Some(SURR_EMBEDDING_COLUMN_384),
        SURR_EMBEDDING_COLUMN_768 => Some(SURR_EMBEDDING_COLUMN_768),
        SURR_EMBEDDING_COLUMN_1024 => Some(SURR_EMBEDDING_COLUMN_1024),
        SURR_EMBEDDING_COLUMN_1536 => Some(SURR_EMBEDDING_COLUMN_1536),
        SURR_EMBEDDING_COLUMN_2048 => Some(SURR_EMBEDDING_COLUMN_2048),
        SURR_EMBEDDING_COLUMN_2560 => Some(SURR_EMBEDDING_COLUMN_2560),
        SURR_EMBEDDING_COLUMN_3072 => Some(SURR_EMBEDDING_COLUMN_3072),
        SURR_EMBEDDING_COLUMN_4096 => Some(SURR_EMBEDDING_COLUMN_4096),
        _ => None,
    }
}

pub fn surreal_embedding_column_for_dimension(dim: usize) -> &'static str {
    match dim {
        384 => SURR_EMBEDDING_COLUMN_384,
        768 => SURR_EMBEDDING_COLUMN_768,
        1024 => SURR_EMBEDDING_COLUMN_1024,
        1536 => SURR_EMBEDDING_COLUMN_1536,
        2048 => SURR_EMBEDDING_COLUMN_2048,
        2560 => SURR_EMBEDDING_COLUMN_2560,
        3072 => SURR_EMBEDDING_COLUMN_3072,
        4096 => SURR_EMBEDDING_COLUMN_4096,
        _ => {
            // Fallback to 2048 for unsupported dimensions
            tracing::warn!(
                "Unsupported embedding dimension {}, falling back to 2048. \
                 Supported dimensions: 384, 768, 1024, 1536, 2048, 2560, 3072, 4096",
                dim
            );
            SURR_EMBEDDING_COLUMN_2048
        }
    }
}

const UPSERT_NODES_QUERY: &str = r#"
LET $batch = array::map($data, |$doc| {
    id: type::record('nodes', $doc.id),
    name: $doc.name,
    node_type: $doc.node_type,
    language: $doc.language,
    content: $doc.content,
    file_path: $doc.file_path,
    start_line: $doc.start_line,
    end_line: $doc.end_line,
    embedding_384: $doc.embedding_384,
    embedding_768: $doc.embedding_768,
    embedding_1024: $doc.embedding_1024,
    embedding_1536: $doc.embedding_1536,
    embedding_2048: $doc.embedding_2048,
    embedding_2560: $doc.embedding_2560,
    embedding_3072: $doc.embedding_3072,
    embedding_4096: $doc.embedding_4096,
    embedding_model: $doc.embedding_model,
    complexity: $doc.complexity,
    metadata: $doc.metadata,
    project_id: $doc.project_id,
    organization_id: $doc.organization_id,
    repository_url: $doc.repository_url,
    domain: $doc.domain,
    chunk_count: $doc.chunk_count,
    updated_at: time::now(),
});
INSERT INTO nodes $batch ON DUPLICATE KEY UPDATE
    name = $input.name,
    node_type = $input.node_type,
    language = $input.language,
    content = $input.content,
    file_path = $input.file_path,
    start_line = $input.start_line,
    end_line = $input.end_line,
    embedding_384 = $input.embedding_384,
    embedding_768 = $input.embedding_768,
    embedding_1024 = $input.embedding_1024,
    embedding_1536 = $input.embedding_1536,
    embedding_2048 = $input.embedding_2048,
    embedding_2560 = $input.embedding_2560,
    embedding_3072 = $input.embedding_3072,
    embedding_4096 = $input.embedding_4096,
    embedding_model = $input.embedding_model,
    complexity = $input.complexity,
    metadata = $input.metadata,
    project_id = $input.project_id,
    organization_id = $input.organization_id,
    repository_url = $input.repository_url,
    domain = $input.domain,
    chunk_count = $input.chunk_count,
    updated_at = $input.updated_at
RETURN NONE;
"#;

const UPSERT_EDGES_QUERY: &str = r#"
LET $batch = array::map($data, |$doc| {
    id: type::record('edges', $doc.id),
    from: type::record('nodes', $doc.from),
    to: type::record('nodes', $doc.to),
    edge_type: $doc.edge_type,
    weight: $doc.weight,
    metadata: $doc.metadata,
    project_id: $doc.project_id,
});
INSERT INTO edges $batch ON DUPLICATE KEY UPDATE
    from = $input.from,
    to = $input.to,
    edge_type = $input.edge_type,
    weight = $input.weight,
    metadata = $input.metadata,
    project_id = $input.project_id
RETURN NONE;
"#;

const UPSERT_SYMBOL_EMBEDDINGS_QUERY: &str = r#"
LET $batch = array::map($data, |$doc| {
    id: type::record('symbol_embeddings', $doc.id),
    symbol: $doc.symbol,
    normalized_symbol: $doc.normalized_symbol,
    project_id: $doc.project_id,
    organization_id: $doc.organization_id,
    embedding_384: $doc.embedding_384,
    embedding_768: $doc.embedding_768,
    embedding_1024: $doc.embedding_1024,
    embedding_2048: $doc.embedding_2048,
    embedding_2560: $doc.embedding_2560,
    embedding_4096: $doc.embedding_4096,
    embedding_1536: $doc.embedding_1536,
    embedding_3072: $doc.embedding_3072,
    embedding_model: $doc.embedding_model,
    node_id: if $doc.node_id != NONE { type::record('nodes', $doc.node_id) } else { NONE },
    source_edge_id: if $doc.source_edge_id != NONE { type::record('edges', $doc.source_edge_id) } else { NONE },
    metadata: $doc.metadata,
    access_count: $doc.access_count,
});
INSERT INTO symbol_embeddings $batch ON DUPLICATE KEY UPDATE
    symbol = $input.symbol,
    normalized_symbol = $input.normalized_symbol,
    project_id = $input.project_id,
    organization_id = $input.organization_id,
    embedding_384 = $input.embedding_384,
    embedding_768 = $input.embedding_768,
    embedding_1024 = $input.embedding_1024,
    embedding_2048 = $input.embedding_2048,
    embedding_2560 = $input.embedding_2560,
    embedding_4096 = $input.embedding_4096,
    embedding_1536 = $input.embedding_1536,
    embedding_3072 = $input.embedding_3072,
    embedding_model = $input.embedding_model,
    node_id = $input.node_id,
    source_edge_id = $input.source_edge_id,
    metadata = $input.metadata,
    access_count = $input.access_count
RETURN NONE;
"#;

const UPSERT_CHUNK_EMBEDDINGS_QUERY: &str = r#"
LET $batch = array::map($data, |$doc| {
    id: type::record('chunks', $doc.id),
    parent_node: type::record('nodes', $doc.parent_node),
    chunk_index: $doc.chunk_index,
    text: $doc.text,
    project_id: $doc.project_id,
    embedding_384: $doc.embedding_384,
    embedding_768: $doc.embedding_768,
    embedding_1024: $doc.embedding_1024,
    embedding_1536: $doc.embedding_1536,
    embedding_2048: $doc.embedding_2048,
    embedding_2560: $doc.embedding_2560,
    embedding_3072: $doc.embedding_3072,
    embedding_4096: $doc.embedding_4096,
    embedding_model: $doc.embedding_model,
    updated_at: time::now(),
});
INSERT INTO chunks $batch ON DUPLICATE KEY UPDATE
    parent_node = $input.parent_node,
    chunk_index = $input.chunk_index,
    text = $input.text,
    project_id = $input.project_id,
    embedding_384 = $input.embedding_384,
    embedding_768 = $input.embedding_768,
    embedding_1024 = $input.embedding_1024,
    embedding_1536 = $input.embedding_1536,
    embedding_2048 = $input.embedding_2048,
    embedding_2560 = $input.embedding_2560,
    embedding_3072 = $input.embedding_3072,
    embedding_4096 = $input.embedding_4096,
    embedding_model = $input.embedding_model,
    updated_at = $input.updated_at
RETURN NONE;
"#;

const INSERT_CHUNK_EMBEDDINGS_QUERY: &str = r#"
LET $batch = array::map($batch, |$doc| {
    id: type::record('chunks', $doc.id),
    parent_node: type::record('nodes', $doc.parent_node),
    chunk_index: $doc.chunk_index,
    text: $doc.text,
    project_id: $doc.project_id,
    embedding_384: $doc.embedding_384,
    embedding_768: $doc.embedding_768,
    embedding_1024: $doc.embedding_1024,
    embedding_1536: $doc.embedding_1536,
    embedding_2048: $doc.embedding_2048,
    embedding_2560: $doc.embedding_2560,
    embedding_3072: $doc.embedding_3072,
    embedding_4096: $doc.embedding_4096,
    embedding_model: $doc.embedding_model,
    created_at: time::now(),
    updated_at: time::now(),
});
INSERT INTO chunks $batch ON DUPLICATE KEY UPDATE
    parent_node = $input.parent_node,
    chunk_index = $input.chunk_index,
    text = $input.text,
    embedding_384 = $input.embedding_384,
    embedding_768 = $input.embedding_768,
    embedding_1024 = $input.embedding_1024,
    embedding_1536 = $input.embedding_1536,
    embedding_2048 = $input.embedding_2048,
    embedding_2560 = $input.embedding_2560,
    embedding_3072 = $input.embedding_3072,
    embedding_4096 = $input.embedding_4096,
    embedding_model = $input.embedding_model,
    updated_at = time::now()
RETURN NONE;
"#;

const UPSERT_CHUNK_EMBEDDING_SINGLE_QUERY: &str = r#"
LET $doc = $doc;
UPSERT type::record('chunks', $doc.id) SET
    parent_node = type::record('nodes', $doc.parent_node),
    chunk_index = $doc.chunk_index,
    text = $doc.text,
    project_id = $doc.project_id,
    embedding_384 = $doc.embedding_384,
    embedding_768 = $doc.embedding_768,
    embedding_1024 = $doc.embedding_1024,
    embedding_1536 = $doc.embedding_1536,
    embedding_2048 = $doc.embedding_2048,
    embedding_2560 = $doc.embedding_2560,
    embedding_3072 = $doc.embedding_3072,
    embedding_4096 = $doc.embedding_4096,
    embedding_model = $doc.embedding_model,
    updated_at = time::now();
"#;

fn truncate_surreal_error(e: &SurrealError) -> String {
    const MAX_LEN: usize = 512;
    let mut msg = e.to_string();
    if msg.len() > MAX_LEN {
        msg.truncate(MAX_LEN);
        msg.push_str("…");
    }
    msg
}

/// Wrap a query future in the graph-write deadline and report a diagnosable timeout.
///
/// `surrealdb::method::Query` implements `IntoFuture`, not `Future`, so callers pass the
/// builder directly (same shape as the existing chunk-embedding timeout).
async fn with_write_deadline<T>(
    timeout: std::time::Duration,
    future: impl std::future::IntoFuture<Output = T>,
    label: &str,
    items: usize,
) -> Result<T> {
    tokio::time::timeout(timeout, future.into_future())
        .await
        .map_err(|_| {
            CodeGraphError::Database(format!(
                "{label} timed out after {timeout:?} ({items} items). \
             The SurrealDB write path stalled; check the server's memtable flush/compaction."
            ))
        })
}

/// Client-side deadline for graph node/edge writes.
///
/// Defaults to 120s; override with `CODEGRAPH_DB_WRITE_TIMEOUT_SECS`. A non-positive
/// value is ignored so a typo cannot disable the protection.
fn write_timeout_from_env() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 120;
    let secs = std::env::var("CODEGRAPH_DB_WRITE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// SurrealKV reports concurrent transaction conflicts as retryable. Detect those
/// so callers can retry instead of aborting a long indexing run.
fn is_retryable_write_conflict(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("transaction write conflict")
        || lower.contains("write conflict")
        || lower.contains("transaction conflict")
        || lower.contains("can be retried")
}

#[allow(dead_code)]
fn parse_count(values: Vec<JsonValue>) -> Result<i64> {
    let Some(first) = values.into_iter().next() else {
        return Ok(0);
    };

    match first {
        JsonValue::Number(n) => n.as_i64().ok_or_else(|| {
            CodeGraphError::Database(format!("Count value is not an integer: {}", n))
        }),
        JsonValue::Object(map) => map.get("count").and_then(|v| v.as_i64()).ok_or_else(|| {
            CodeGraphError::Database("Count object missing integer 'count' field".to_string())
        }),
        other => Err(CodeGraphError::Database(format!(
            "Unexpected count shape: {}",
            other
        ))),
    }
}

/// Strip the record prefix and quoting from a `nodes` id returned by SurrealDB.
///
/// Mirrors `codegraph_mcp::indexer::decode_node_id`: SurrealDB renders an id as
/// `nodes:<id>` (and quotes ids that need it with backticks or angle brackets),
/// while the rest of the pipeline keys nodes by the bare id. Postgres stores the
/// bare id already, so its backend does not call this.
fn decode_node_id(raw: &str) -> &str {
    raw.strip_prefix("nodes:")
        .unwrap_or(raw)
        .trim_matches('`')
        .trim_matches('⟨')
        .trim_matches('⟩')
}

/// A SurrealDB string column as an owned `String`, defaulting to empty.
fn json_string(row: &JsonValue, key: &str) -> String {
    row.get(key)
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A nullable SurrealDB string column; absent or non-string values are `None`.
fn json_optional_string(row: &JsonValue, key: &str) -> Option<String> {
    row.get(key).and_then(JsonValue::as_str).map(str::to_string)
}

/// A nullable SurrealDB integer column. `nodes.start_line`/`end_line` are
/// `option<number>`, so a value outside `i32` is treated as absent rather than
/// wrapped.
fn json_optional_i32(row: &JsonValue, key: &str) -> Option<i32> {
    row.get(key)
        .and_then(JsonValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

/// Count rows of one project-scoped table.
async fn surreal_count(
    db: Arc<Surreal<Any>>,
    table: &str,
    project_id: &str,
) -> anyhow::Result<i64> {
    let mut response = db
        .query(format!(
            "SELECT count() AS count FROM {table} WHERE project_id = $project_id GROUP ALL;"
        ))
        .bind(("project_id", project_id.to_string()))
        .await
        .map_err(|e| CodeGraphError::Database(format!("{table} count failed: {e}")))?;
    let rows: Vec<JsonValue> = response
        .take(0)
        .map_err(|e| CodeGraphError::Database(format!("{table} count decode failed: {e}")))?;
    Ok(rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(|count| count.as_i64())
        .unwrap_or(0))
}

/// Thin adapter over the existing SurrealDB write/read methods.
///
/// `upsert_nodes_batch` / `upsert_edges_batch` take `&mut self` only to update the
/// shared `node_cache`; the clone below shares the same `Arc` fields, so the cache
/// and connection are identical while the trait can stay `&self`.
#[async_trait]
impl crate::storage::IndexStorage for SurrealDbStorage {
    async fn upsert_nodes(&self, nodes: &[CodeNode]) -> anyhow::Result<u64> {
        let mut storage = self.clone();
        storage.upsert_nodes_batch(nodes).await?;
        Ok(nodes.len() as u64)
    }

    async fn upsert_edges(&self, edges: &[CodeEdge]) -> anyhow::Result<u64> {
        let mut storage = self.clone();
        storage.upsert_edges_batch(edges).await?;
        Ok(edges.len() as u64)
    }

    async fn upsert_chunks(
        &self,
        chunks: &[crate::storage::ChunkEmbeddingRecord],
    ) -> anyhow::Result<u64> {
        self.upsert_chunk_embeddings_resilient(chunks).await?;
        Ok(chunks.len() as u64)
    }

    async fn upsert_symbols(
        &self,
        symbols: &[crate::storage::SymbolEmbeddingRecord],
    ) -> anyhow::Result<u64> {
        self.upsert_symbol_embeddings_batch(symbols).await?;
        Ok(symbols.len() as u64)
    }

    async fn upsert_file_metadata(
        &self,
        files: &[crate::storage::FileMetadataRecord],
    ) -> anyhow::Result<u64> {
        self.upsert_file_metadata_batch(files).await?;
        Ok(files.len() as u64)
    }

    async fn update_node_embeddings(
        &self,
        rows: &[crate::storage::NodeEmbeddingRecord],
    ) -> anyhow::Result<u64> {
        self.update_node_embeddings_batch(rows).await?;
        Ok(rows.len() as u64)
    }

    async fn delete_data_for_files(
        &self,
        project_id: &str,
        file_paths: &[String],
    ) -> anyhow::Result<u64> {
        SurrealDbStorage::delete_data_for_files(self, project_id, file_paths).await?;
        Ok(file_paths.len() as u64)
    }

    async fn upsert_project_metadata(
        &self,
        record: crate::storage::ProjectMetadataRecord,
    ) -> anyhow::Result<()> {
        SurrealDbStorage::upsert_project_metadata(self, record).await?;
        Ok(())
    }

    async fn reconcile(
        &self,
        project_id: &str,
        fresh_nodes_by_file: &BTreeMap<String, Vec<String>>,
        fresh_chunk_ids: &BTreeSet<String>,
        fresh_edge_ids: &BTreeSet<String>,
        scope: Option<&ReconcileScope>,
    ) -> anyhow::Result<crate::storage::ReconcileOutcome> {
        // `reconcile_catalog_by_file` reports nothing back, so the outcome is zeroed
        // rather than guessed; adding counts would change an existing signature.
        self.reconcile_catalog_by_file(
            project_id,
            fresh_nodes_by_file,
            fresh_chunk_ids,
            fresh_edge_ids,
            scope,
        )
        .await?;
        Ok(crate::storage::ReconcileOutcome::default())
    }

    async fn file_hashes(&self, project_id: &str) -> anyhow::Result<HashMap<String, String>> {
        #[derive(Serialize, Deserialize, surrealdb::types::SurrealValue)]
        struct FileHashRow {
            file_path: String,
            content_hash: String,
        }

        let mut response = self
            .db
            .query(
                "SELECT file_path, content_hash FROM file_metadata WHERE project_id = $project_id;",
            )
            .bind(("project_id", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("file hash query failed: {e}")))?;
        let rows: Vec<FileHashRow> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("file hash decode failed: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|row| (row.file_path, row.content_hash))
            .collect())
    }

    async fn node_count(&self, project_id: &str) -> anyhow::Result<i64> {
        surreal_count(self.db(), "nodes", project_id).await
    }

    async fn edge_count(&self, project_id: &str) -> anyhow::Result<i64> {
        surreal_count(self.db(), "edges", project_id).await
    }

    async fn chunk_count(&self, project_id: &str) -> anyhow::Result<i64> {
        surreal_count(self.db(), "chunks", project_id).await
    }

    async fn knn_chunks(
        &self,
        project_id: &str,
        query: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        if k == 0 || query.is_empty() {
            return Ok(Vec::new());
        }

        #[derive(Serialize, Deserialize, surrealdb::types::SurrealValue)]
        struct KnnRow {
            id: String,
            distance: f64,
        }

        let mut response = self
            .db
            .query("RETURN fn::knn_chunks($project_id, $dimension, $query_embedding)")
            .bind(("project_id", project_id.to_string()))
            .bind(("dimension", query.len() as i64))
            .bind(("query_embedding", query.to_vec()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("chunk KNN query failed: {e}")))?;
        let rows: Vec<KnnRow> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("chunk KNN decode failed: {e}")))?;
        Ok(rows
            .into_iter()
            .take(k)
            .map(|row| (row.id, row.distance as f32))
            .collect())
    }

    async fn reverse_deps(
        &self,
        project_id: &str,
        from_node: &str,
        depth: u32,
    ) -> anyhow::Result<Vec<String>> {
        let graph = crate::graph_functions::GraphFunctions::new_with_project_id(
            self.db(),
            project_id.to_string(),
        );
        let deps = graph
            .get_reverse_dependencies(from_node, "calls", depth as i32)
            .await?;
        Ok(deps.into_iter().map(|dependency| dependency.id).collect())
    }

    async fn project_fingerprint(&self, project_id: &str) -> anyhow::Result<Option<String>> {
        let mut response = self
            .db
            .query(
                "SELECT metadata.input_fingerprint AS fingerprint FROM project_metadata \
                 WHERE project_id = $project",
            )
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("fingerprint query failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("fingerprint query failed: {e}")))?;
        let markers: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("fingerprint decode failed: {e}")))?;
        Ok(markers
            .first()
            .and_then(|value| value.get("fingerprint"))
            .and_then(|value| value.as_str())
            .map(str::to_string))
    }

    async fn delete_project_metadata(&self, project_id: &str) -> anyhow::Result<()> {
        self.db
            .query("DELETE project_metadata WHERE project_id = $project")
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("project metadata delete failed: {e}")))?
            .check()
            .map_err(|e| {
                CodeGraphError::Database(format!("project metadata delete failed: {e}"))
            })?;
        Ok(())
    }

    async fn has_project_metadata(&self, project_id: &str) -> anyhow::Result<bool> {
        // Reuses `surreal_count` rather than duplicating the same
        // `count() ... GROUP ALL` probe; its `unwrap_or(0)` already covers "no row".
        let count = surreal_count(self.db(), "project_metadata", project_id).await?;
        Ok(count > 0)
    }

    async fn file_metadata(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<crate::storage::FileMetadataRecord>> {
        Ok(self.get_file_metadata_for_project(project_id).await?)
    }

    async fn nodes_in_files(
        &self,
        project_id: &str,
        files: &[String],
    ) -> anyhow::Result<Vec<crate::storage::NodeInFile>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        let mut response = self
            .db
            .query(
                "SELECT <string> id AS id, file_path, name, node_type FROM nodes \
                 WHERE project_id = $project AND file_path IN $files;",
            )
            .bind(("project", project_id.to_string()))
            .bind(("files", files.to_vec()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("nodes-in-files query failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("nodes-in-files query failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("nodes-in-files decode failed: {e}")))?;

        let mut nodes = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(raw_id) = row.get("id").and_then(|value| value.as_str()) else {
                continue;
            };
            nodes.push(crate::storage::NodeInFile {
                id: decode_node_id(raw_id).to_string(),
                file_path: row
                    .get("file_path")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: row
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                node_type: row
                    .get("node_type")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(nodes)
    }

    /// Delegates to [`SurrealDbStorage::load_symbols_excluding`], whose ids still
    /// carry the `nodes:` record prefix. The trait contract says ids are decoded,
    /// so the prefix is stripped here, exactly like [`Self::nodes_in_files`].
    async fn symbols_excluding_files(
        &self,
        project_id: &str,
        excluded_files: &[String],
    ) -> anyhow::Result<Vec<crate::storage::SymbolInProject>> {
        let rows = self
            .load_symbols_excluding(project_id, excluded_files)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::storage::SymbolInProject {
                id: decode_node_id(&row.id).to_string(),
                file_path: row.file_path,
                name: row.name,
                node_type: row.node_type,
                qualified_name: row.qualified_name,
            })
            .collect())
    }

    async fn retarget_edges(&self, project_id: &str, old: &str, new: &str) -> anyhow::Result<u64> {
        self.db
            .query(
                "UPDATE edges SET to = type::record('nodes', $new) \
                 WHERE project_id = $project AND to = type::record('nodes', $old);",
            )
            .bind(("project", project_id.to_string()))
            .bind(("old", old.to_string()))
            .bind(("new", new.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("edge retarget failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("edge retarget failed: {e}")))?;
        // This statement does not report an affected-row count, so the caller
        // cannot learn how many edges moved. It returns 0 by contract rather
        // than inventing a number.
        Ok(0)
    }

    async fn ensure_vector_indexes(
        &self,
        dimension: usize,
        tables: &[String],
    ) -> anyhow::Result<bool> {
        // `vector_indexes::ensure_ready` keys its DDL on `&'static str` table
        // names, so the caller's owned strings are mapped back onto the three
        // supported tables. An unknown name is a configuration error, mirroring
        // the check inside `definitions`.
        let tables: Result<Vec<&'static str>> = tables
            .iter()
            .map(|table| match table.as_str() {
                "nodes" => Ok("nodes"),
                "chunks" => Ok("chunks"),
                "symbol_embeddings" => Ok("symbol_embeddings"),
                other => Err(CodeGraphError::Configuration(format!(
                    "Unsupported vector table: {other}"
                ))),
            })
            .collect();
        let tables = tables?;
        Ok(crate::vector_indexes::ensure_ready(&self.db(), dimension, &tables).await?)
    }

    /// Port of the SurrealQL `codegraph query` used to run inline. `ORDER BY name`
    /// was added here (the original had no ordering, so equal-prefix results came
    /// back in an unspecified order) to match the Postgres backend.
    async fn find_nodes_by_name(
        &self,
        project_id: &str,
        symbol: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<crate::storage::SymbolMatch>> {
        let limit = crate::storage::clamp_lookup_limit(limit);
        let mut response = self
            .db
            .query(
                r#"
                SELECT name, node_type, language, file_path, start_line, end_line
                FROM nodes
                WHERE project_id = $project
                  AND (name = $symbol OR string::starts_with(name, $symbol))
                ORDER BY name ASC
                LIMIT $limit;
                "#,
            )
            .bind(("project", project_id.to_string()))
            .bind(("symbol", symbol.to_string()))
            .bind(("limit", limit))
            .await
            .map_err(|e| CodeGraphError::Database(format!("symbol lookup failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("symbol lookup failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("symbol lookup decode failed: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|row| crate::storage::SymbolMatch {
                name: json_string(&row, "name"),
                node_type: json_optional_string(&row, "node_type"),
                language: json_optional_string(&row, "language"),
                file_path: json_string(&row, "file_path"),
                start_line: json_optional_i32(&row, "start_line"),
                end_line: json_optional_i32(&row, "end_line"),
            })
            .collect())
    }

    /// Callers of every node named `target`, over `calls` edges.
    async fn callers_of_name(
        &self,
        project_id: &str,
        target: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<crate::storage::CallRelation>> {
        let limit = crate::storage::clamp_lookup_limit(limit);
        let mut response = self
            .db
            .query(
                r#"
                SELECT
                    from.name AS symbol,
                    from.node_type AS node_type,
                    from.file_path AS file,
                    from.start_line AS line,
                    metadata.resolution_method AS resolution
                FROM edges
                WHERE project_id = $project
                  AND to.name = $target
                  AND edge_type = 'calls'
                LIMIT $limit;
                "#,
            )
            .bind(("project", project_id.to_string()))
            .bind(("target", target.to_string()))
            .bind(("limit", limit))
            .await
            .map_err(|e| CodeGraphError::Database(format!("callers lookup failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("callers lookup failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("callers lookup decode failed: {e}")))?;
        Ok(rows.into_iter().map(call_relation_from_row).collect())
    }

    /// Callees of every node named `target`: the mirror image of
    /// [`Self::callers_of_name`].
    async fn callees_of_name(
        &self,
        project_id: &str,
        target: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<crate::storage::CallRelation>> {
        let limit = crate::storage::clamp_lookup_limit(limit);
        let mut response = self
            .db
            .query(
                r#"
                SELECT
                    to.name AS symbol,
                    to.node_type AS node_type,
                    to.file_path AS file,
                    to.start_line AS line,
                    metadata.resolution_method AS resolution
                FROM edges
                WHERE project_id = $project
                  AND from.name = $target
                  AND edge_type = 'calls'
                LIMIT $limit;
                "#,
            )
            .bind(("project", project_id.to_string()))
            .bind(("target", target.to_string()))
            .bind(("limit", limit))
            .await
            .map_err(|e| CodeGraphError::Database(format!("callees lookup failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("callees lookup failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("callees lookup decode failed: {e}")))?;
        Ok(rows.into_iter().map(call_relation_from_row).collect())
    }

    /// Node ids, names and file paths for `plan verify`. `metadata` is always
    /// `None` here: `verify` only compares identities.
    async fn node_identities(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<crate::storage::NodeIdentityRow>> {
        let mut response = self
            .db
            .query(
                "SELECT <string> id AS id, name, file_path FROM nodes \
                 WHERE project_id = $project;",
            )
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("node identities failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("node identities failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("node identities decode: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|row| crate::storage::NodeIdentityRow {
                id: decode_node_id(&json_string(&row, "id")).to_string(),
                name: json_string(&row, "name"),
                file_path: json_string(&row, "file_path"),
                metadata: None,
            })
            .collect())
    }

    /// Every edge's endpoints, decoded from their `nodes:` record form.
    async fn edge_endpoints(&self, project_id: &str) -> anyhow::Result<Vec<(String, String)>> {
        let mut response = self
            .db
            .query(
                "SELECT <string> from AS from_id, <string> to AS to_id FROM edges \
                 WHERE project_id = $project;",
            )
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("edge endpoints failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("edge endpoints failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("edge endpoints decode: {e}")))?;

        // SurrealDB renders the endpoints as `nodes:<id>`; `plan verify` maps them
        // against `node_identities` ids, which are decoded, so strip the prefix.
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    decode_node_id(&json_string(&row, "from_id")).to_string(),
                    decode_node_id(&json_string(&row, "to_id")).to_string(),
                )
            })
            .collect())
    }

    /// Node ids, names and decoded metadata for `plan show`.
    ///
    /// SurrealDB stores `nodes.metadata` as a compressed JSON string. A blob that
    /// cannot be decompressed is reported as `None`; the caller then renders the
    /// node without `layer`/`draft_kind` rather than failing the whole command.
    async fn nodes_with_metadata(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<crate::storage::NodeIdentityRow>> {
        let mut response = self
            .db
            .query(
                "SELECT <string>id AS rid, name, metadata FROM nodes \
                 WHERE project_id = $project",
            )
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("plan nodes failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("plan nodes failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("plan nodes decode: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|row| crate::storage::NodeIdentityRow {
                id: decode_node_id(&json_string(&row, "rid")).to_string(),
                name: json_string(&row, "name"),
                file_path: String::new(),
                metadata: row
                    .get("metadata")
                    .and_then(JsonValue::as_str)
                    .and_then(|encoded| codegraph_core::decompress_json(encoded).ok()),
            })
            .collect())
    }

    /// The whole `project_metadata.metadata` object. `plan` stores its constraints
    /// under `metadata.plan_constraints`; returning the object keeps that decision
    /// out of this layer.
    async fn project_metadata_value(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let mut response = self
            .db
            .query("SELECT metadata FROM project_metadata WHERE project_id = $project LIMIT 1;")
            .bind(("project", project_id.to_string()))
            .await
            .map_err(|e| CodeGraphError::Database(format!("project metadata failed: {e}")))?
            .check()
            .map_err(|e| CodeGraphError::Database(format!("project metadata failed: {e}")))?;
        let rows: Vec<JsonValue> = response
            .take(0)
            .map_err(|e| CodeGraphError::Database(format!("project metadata decode: {e}")))?;
        let Some(value) = rows
            .first()
            .and_then(|row| row.get("metadata"))
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        Ok(serde_json::from_value(value.clone()).ok())
    }

    /// Delegates to [`SurrealDbStorage::clean_project_data`], which deletes the six
    /// project-scoped tables in one transaction. That statement reports no affected
    /// rows, so this returns 0 by contract instead of inventing a count.
    async fn delete_project(&self, project_id: &str) -> anyhow::Result<u64> {
        self.clean_project_data(project_id).await?;
        Ok(0)
    }
}

/// Project a caller/callee row into [`crate::storage::CallRelation`].
fn call_relation_from_row(row: JsonValue) -> crate::storage::CallRelation {
    crate::storage::CallRelation {
        symbol: json_string(&row, "symbol"),
        node_type: json_optional_string(&row, "node_type"),
        file: json_string(&row, "file"),
        line: json_optional_i32(&row, "line"),
        resolution: json_optional_string(&row, "resolution"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_index_storage<T: crate::storage::IndexStorage>() {}

    #[test]
    fn surrealdb_storage_implements_index_storage() {
        assert_index_storage::<SurrealDbStorage>();
    }

    fn assert_graph_query<T: crate::graph_types::GraphQuery>() {}

    #[test]
    fn surreal_graph_implements_graph_query() {
        assert_graph_query::<SurrealGraph>();
    }

    /// `ProjectMetadataRecord` now lives in `crate::storage`; the SurrealDB adapter
    /// re-exports it so existing `codegraph_graph::ProjectMetadataRecord` paths keep
    /// resolving.
    #[test]
    fn project_metadata_record_is_reexported_by_surrealdb() {
        let _: crate::storage::ProjectMetadataRecord = ProjectMetadataRecord {
            metadata: serde_json::json!({}),
            project_id: "p".into(),
            name: "n".into(),
            root_path: "/r".into(),
            primary_language: None,
            file_count: 0,
            node_count: 0,
            edge_count: 0,
            avg_coverage_score: 0.0,
            last_analyzed: chrono::Utc::now(),
            codegraph_version: "v".into(),
            organization_id: None,
            domain: None,
        };
    }

    /// A stalled server write must surface as a timeout, not park the run forever.
    #[tokio::test]
    async fn write_deadline_reports_stalled_queries() {
        let err = with_write_deadline(
            std::time::Duration::ZERO,
            std::future::pending::<()>(),
            "Node batch upsert",
            2,
        )
        .await
        .expect_err("a pending query must time out");
        let message = err.to_string();
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("2 items"), "{message}");
        assert!(message.contains("Node batch upsert"), "{message}");
    }

    /// Regression for the indexing crash on SurrealKV write-write conflicts.
    /// `upsert_symbol_embeddings_batch` retries only when the classifier says the
    /// error is transient; everything else must still fail fast so real problems
    /// are not masked by six silent retries.
    #[test]
    fn retryable_write_conflict_classifier_matches_surrealkv_wording() {
        let retryable = [
            "There was a problem with the key-value store: Transaction conflict: \
             Transaction write conflict. This transaction can be retried",
            "Transaction write conflict",
            "write conflict",
            "transaction conflict",
            "THIS TRANSACTION CAN BE RETRIED",
        ];
        for message in retryable {
            assert!(
                is_retryable_write_conflict(message),
                "expected retryable: {message}"
            );
        }

        let fatal = [
            "",
            "Symbol batch upsert returned error: missing field project_id",
            "excessive computation depth",
            "connection reset by peer",
            "Serialization error: invalid UTF-8",
            "Failed to upsert symbol embedding batch (3 items): permission denied",
            "Edge batch upsert timed out after 120s (512 items). The SurrealDB write path \
             stalled; check the server's memtable flush/compaction.",
        ];
        for message in fatal {
            assert!(
                !is_retryable_write_conflict(message),
                "expected fatal (fail fast): {message}"
            );
        }
    }

    /// A retried batch must converge on the same rows, never duplicate them.
    /// The writer replays a batch after a transient conflict, so idempotency is
    /// part of the retry contract.
    #[tokio::test]
    async fn symbol_embedding_batch_upsert_is_idempotent_across_retries() {
        let storage = SurrealDbStorage::new(SurrealDbConfig {
            connection: "mem://".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

        let vector: Vec<f32> = vec![0.25; 384];
        let record = SymbolEmbeddingRecord::new(
            "retry-project",
            None,
            "build_chunk_plan",
            "build_chunk_plan",
            &vector,
            "fixture",
            "embedding_384",
            None,
            None,
            None,
        );

        let batch = std::slice::from_ref(&record);
        storage.upsert_symbol_embeddings_batch(batch).await.unwrap();
        // Simulate the retry that follows a write conflict.
        storage.upsert_symbol_embeddings_batch(batch).await.unwrap();
        storage.upsert_symbol_embeddings_batch(batch).await.unwrap();

        let mut response = storage
            .db()
            .query("SELECT count() AS n FROM symbol_embeddings WHERE project_id = 'retry-project' GROUP ALL")
            .await
            .unwrap()
            .check()
            .unwrap();
        let row: Option<serde_json::Value> = response.take(0).unwrap();
        let total = row
            .and_then(|value| value.get("n").and_then(|n| n.as_u64()))
            .unwrap_or(0);
        assert_eq!(total, 1, "retried upsert must not duplicate symbol rows");
    }

    #[tokio::test]
    async fn test_surrealdb_storage_creation() {
        let config = SurrealDbConfig {
            connection: "mem://".to_string(),
            ..Default::default()
        };

        let storage = SurrealDbStorage::new(config).await;
        assert!(storage.is_ok());
    }

    #[tokio::test]
    async fn upsert_project_metadata_creates_row_with_project_id() {
        let config = SurrealDbConfig {
            connection: "mem://".to_string(),
            ..Default::default()
        };

        let storage = SurrealDbStorage::new(config).await.unwrap();

        storage
            .db
            .query(
                r#"
                DEFINE TABLE project_metadata TYPE NORMAL SCHEMAFULL PERMISSIONS FULL;
                DEFINE FIELD project_id ON project_metadata TYPE string PERMISSIONS FULL;
                DEFINE FIELD name ON project_metadata TYPE string PERMISSIONS FULL;
                DEFINE FIELD root_path ON project_metadata TYPE string PERMISSIONS FULL;
                DEFINE FIELD primary_language ON project_metadata TYPE option<string> PERMISSIONS FULL;
                DEFINE FIELD file_count ON project_metadata TYPE int PERMISSIONS FULL;
                DEFINE FIELD node_count ON project_metadata TYPE int PERMISSIONS FULL;
                DEFINE FIELD edge_count ON project_metadata TYPE int PERMISSIONS FULL;
                DEFINE FIELD avg_coverage_score ON project_metadata TYPE float PERMISSIONS FULL;
                DEFINE FIELD last_analyzed ON project_metadata TYPE option<datetime> PERMISSIONS FULL;
                DEFINE FIELD codegraph_version ON project_metadata TYPE option<string> PERMISSIONS FULL;
                DEFINE FIELD organization_id ON project_metadata TYPE option<string> PERMISSIONS FULL;
                DEFINE FIELD domain ON project_metadata TYPE option<string> PERMISSIONS FULL;
                DEFINE FIELD metadata ON project_metadata TYPE option<object> FLEXIBLE PERMISSIONS FULL;
                DEFINE FIELD created_at ON project_metadata TYPE datetime DEFAULT time::now() READONLY PERMISSIONS FULL;
                DEFINE FIELD updated_at ON project_metadata TYPE datetime VALUE time::now() PERMISSIONS FULL;
            "#,
            )
            .await
            .unwrap()
            .check()
            .unwrap();

        let project_id = "project-a".to_string();
        storage
            .upsert_project_metadata(ProjectMetadataRecord {
                metadata: serde_json::json!({}),
                project_id: project_id.clone(),
                name: "Project A".to_string(),
                root_path: "/tmp/project-a".to_string(),
                primary_language: Some("rust".to_string()),
                file_count: 3,
                node_count: 10,
                edge_count: 20,
                avg_coverage_score: 0.5,
                last_analyzed: chrono::Utc::now(),
                codegraph_version: "0.0.0-test".to_string(),
                organization_id: None,
                domain: None,
            })
            .await
            .unwrap();

        let mut resp = storage
            .db
            .query(
                "SELECT count() AS count FROM project_metadata WHERE project_id = $project_id GROUP ALL;",
            )
            .bind(("project_id", project_id))
            .await
            .unwrap();
        let rows: Vec<JsonValue> = resp.take(0).unwrap();
        let count = rows
            .first()
            .and_then(|v| v.get("count"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        assert_eq!(count, 1);
    }

    #[test]
    fn normalized_embedding_column_supports_2560() {
        assert_eq!(
            normalized_embedding_column(SURR_EMBEDDING_COLUMN_2560),
            Some(SURR_EMBEDDING_COLUMN_2560)
        );
    }

    #[test]
    fn normalized_embedding_column_rejects_unknown_column() {
        assert!(normalized_embedding_column("embedding_9999").is_none());
    }

    #[test]
    fn project_connection_defaults_to_the_shared_server() {
        assert_eq!(project_connection(None), DEFAULT_SERVER_URL);
        assert_eq!(DEFAULT_SERVER_URL, "ws://127.0.0.1:8000");
        assert!(!is_embedded_connection(DEFAULT_SERVER_URL));
    }

    #[test]
    fn project_database_names_are_stable_and_distinct() {
        let happy = project_database_name(Path::new("/repo/happy"));
        assert_eq!(happy, project_database_name(Path::new("/repo/happy")));
        assert!(happy.starts_with("happy_"));
        assert!(
            happy
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        );
        assert_ne!(happy, project_database_name(Path::new("/repo/opencode")));
    }

    #[test]
    fn database_per_project_can_be_disabled() {
        assert!(database_per_project_with(None));
        assert!(database_per_project_with(Some("1")));
        assert!(!database_per_project_with(Some("0")));
        assert!(!database_per_project_with(Some("false")));
        assert!(!database_per_project_with(Some("off")));
    }

    #[test]
    fn project_connection_keeps_an_explicit_url() {
        assert_eq!(
            project_connection(Some("surrealkv:///tmp/x/.codegraph/db".into())),
            "surrealkv:///tmp/x/.codegraph/db"
        );
        assert!(is_embedded_connection("surrealkv:///tmp/x/.codegraph/db"));
    }

    #[test]
    fn hex_lower_matches_format_encoding() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    #[test]
    fn hex_lower_matches_sha256_of_known_input() {
        assert_eq!(
            hex_lower(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn symbol_embedding_record_keeps_f32_precision() {
        let embedding = [0.1f32, 0.25, -1.5];
        let record = SymbolEmbeddingRecord::new(
            "f32-project",
            None,
            "symbol",
            "symbol",
            &embedding,
            "fixture",
            SURR_EMBEDDING_COLUMN_1024,
            None,
            None,
            None,
        );

        assert_eq!(record.embedding_1024, Some(vec![0.1f32, 0.25, -1.5]));

        let stored = serde_json::to_string(&record.embedding_1024).unwrap();
        assert!(stored.contains("0.1,"), "{stored}");
        assert!(!stored.contains("0.100000001"), "{stored}");

        // Widening the same provider output to f64 would inflate the JSON payload.
        let as_f32 = serde_json::to_string(&vec![0.1f32, 0.25, -1.5]).unwrap();
        let widened = serde_json::to_string(
            &vec![0.1f32, 0.25, -1.5]
                .into_iter()
                .map(f64::from)
                .collect::<Vec<f64>>(),
        )
        .unwrap();
        assert!(as_f32.len() < widened.len(), "{as_f32} vs {widened}");
    }
}

crate::impl_surreal_serde!(
    SchemaVersion,
    SymbolEmbeddingRecord,
    FileMetadataRecord,
    SurrealNodeRecord,
    ChunkEmbeddingRecord
);

/// `GraphFunctions` behind the backend-neutral query trait.
pub struct SurrealGraph {
    functions: GraphFunctions,
}

impl SurrealGraph {
    pub fn new(functions: GraphFunctions) -> Self {
        Self { functions }
    }
}

#[async_trait]
impl crate::graph_types::GraphQuery for SurrealGraph {
    fn project_id(&self) -> &str {
        self.functions.project_id()
    }

    fn with_project_id(&self, project_id: &str) -> Arc<dyn crate::graph_types::GraphQuery> {
        Arc::new(Self {
            functions: self.functions.with_project_id(project_id.to_string()),
        })
    }

    async fn get_transitive_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
    ) -> Result<Vec<DependencyNode>> {
        self.functions
            .get_transitive_dependencies(node_id, edge_type, depth)
            .await
    }

    async fn get_reverse_dependencies(
        &self,
        node_id: &str,
        edge_type: &str,
        depth: i32,
    ) -> Result<Vec<DependencyNode>> {
        self.functions
            .get_reverse_dependencies(node_id, edge_type, depth)
            .await
    }

    async fn trace_call_chain(
        &self,
        from_node: &str,
        max_depth: i32,
    ) -> Result<Vec<CallChainNode>> {
        self.functions.trace_call_chain(from_node, max_depth).await
    }

    async fn calculate_coupling_metrics(&self, node_id: &str) -> Result<CouplingMetricsResult> {
        self.functions.calculate_coupling_metrics(node_id).await
    }

    async fn get_hub_nodes(&self, min_degree: i32) -> Result<Vec<HubNode>> {
        self.functions.get_hub_nodes(min_degree).await
    }

    async fn get_complexity_hotspots(
        &self,
        min_complexity: f32,
        limit: i32,
    ) -> Result<Vec<ComplexityHotspot>> {
        self.functions
            .get_complexity_hotspots(min_complexity, limit)
            .await
    }

    async fn get_node_edge_summary(&self, node_id: &str) -> Result<NodeEdgeSummary> {
        self.functions.get_node_edge_summary(node_id).await
    }

    async fn count_nodes_for_project(&self) -> Result<usize> {
        self.functions.count_nodes_for_project().await
    }

    async fn detect_circular_dependencies(
        &self,
        edge_type: &str,
    ) -> Result<Vec<CircularDependency>> {
        self.functions.detect_circular_dependencies(edge_type).await
    }

    async fn semantic_search_with_context(
        &self,
        query_text: &str,
        query_embedding: &[f32],
        dimension: usize,
        limit: usize,
        threshold: f32,
        include_graph_context: bool,
    ) -> Result<Vec<serde_json::Value>> {
        self.functions
            .semantic_search_with_context(
                query_text,
                query_embedding,
                dimension,
                limit,
                threshold,
                include_graph_context,
            )
            .await
    }
}

/// A symbol row loaded from storage to seed a partial re-index's resolution catalog.
#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub id: String,
    pub file_path: String,
    pub name: String,
    pub node_type: Option<String>,
    pub qualified_name: Option<String>,
}

pub use crate::storage::ReconcileScope;
