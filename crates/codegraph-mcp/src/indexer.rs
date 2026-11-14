// ABOUTME: Drives the indexing pipeline for the Codegraph MCP CLI.
// ABOUTME: Coordinates parsing, embeddings, and persistence into SurrealDB.
#![allow(dead_code, unused_variables, unused_imports)]

//! Storage backend selection for an indexing run.
//!
//! `CODEGRAPH_BACKEND` picks where indexing writes go. Unset / empty means the
//! default SurrealDB backend; `surreal` and `surrealdb` are accepted aliases.
//! `postgres` / `pg` select the pgvector-backed `PgStore` instead. Any other
//! value is a configuration error rather than a silent fallback.
//!
//! Known limitations of the Postgres backend today:
//!
//! - it requires a build with `--features postgres`;
//! - it reads the connection URL from `CODEGRAPH_PG_URL`;
//! - **partial indexing (single file / subdirectory) fails**: `reconcile` with
//!   `Some(scope)` returns `scoped reconciliation is not implemented for the
//!   Postgres backend yet`;
//! - the retrieval side (MCP tools / `GraphQueries`) still talks to SurrealDB,
//!   so switching the write backend does not change query paths.

use crate::analyzers::AnalyzerSettings;
use crate::estimation::parse_files_with_unified_extraction as shared_unified_parse;
use anyhow::{Context, Result, anyhow, bail};
use codegraph_core::{CodeNode, EdgeRelationship, EdgeType, NodeId, NodeType};
use codegraph_graph::edge::CodeEdge;
use codegraph_graph::storage::{
    ChunkEmbeddingRecord, EmbeddingColumn, FileMetadataRecord, IndexStorage, NodeEmbeddingRecord,
    ProjectMetadataRecord, ReconcileScope, SymbolEmbeddingRecord,
};
#[cfg(feature = "surrealdb")]
use codegraph_graph::{SurrealDbConfig, SurrealDbStorage};
use codegraph_parser::TreeSitterParser;
#[cfg(feature = "embeddings")]
use codegraph_vector::prep::chunker::ChunkPlan;
#[cfg(feature = "ai-enhanced")]
use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use num_cpus;
use rayon::prelude::*;
use regex::Regex;
use rustc_demangle::try_demangle;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use symbolic_demangle::demangle;
use syn::{Path as SynPath, PathArguments, parse_str as parse_syn_path};
use tokio::fs as tokio_fs;
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use url::Url;
use walkdir::WalkDir;

use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

const SYMBOL_EMBEDDING_DB_BATCH_LIMIT: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub enum FileChangeType {
    Added,
    Modified,
    Deleted,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub file_path: String,
    pub change_type: FileChangeType,
    pub current_hash: Option<String>,
    pub previous_hash: Option<String>,
}

static WATCH_TEST_NOTIFIER: OnceLock<Mutex<Option<tokio::sync::mpsc::UnboundedSender<PathBuf>>>> =
    OnceLock::new();

pub fn set_watch_test_notifier(sender: tokio::sync::mpsc::UnboundedSender<PathBuf>) {
    let mut guard = WATCH_TEST_NOTIFIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap();
    *guard = Some(sender);
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexerConfig {
    #[serde(default)]
    pub complete_deferred: bool,
    /// Generate SCIP indexes for detected languages before indexing.
    #[serde(default)]
    pub scip_auto: bool,
    /// With `scip_auto`, fail instead of falling back to LSP.
    #[serde(default)]
    pub scip_strict: bool,
    pub languages: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub include_patterns: Vec<String>,
    pub recursive: bool,
    pub force_reindex: bool,
    pub watch: bool,
    pub workers: usize,
    pub batch_size: usize,
    pub max_concurrent: usize,
    pub vector_dimension: usize,
    pub device: Option<String>,
    pub max_seq_len: usize,
    pub symbol_batch_size: Option<usize>,
    pub symbol_max_concurrent: Option<usize>,
    pub indexing_tier: codegraph_core::config_manager::IndexingTier,
    /// Root directory of the project being indexed (where .codegraph/ will be created)
    /// Defaults to current directory if not specified
    pub project_root: PathBuf,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            complete_deferred: false,
            scip_auto: false,
            scip_strict: false,
            languages: vec![],
            exclude_patterns: vec![],
            include_patterns: vec![],
            recursive: true,
            force_reindex: false,
            watch: false,
            workers: 4,
            batch_size: 100,
            max_concurrent: 10,
            vector_dimension: 384, // Match EmbeddingGenerator default (all-MiniLM-L6-v2)
            device: None,
            max_seq_len: 512,
            symbol_batch_size: None,
            symbol_max_concurrent: None,
            indexing_tier: codegraph_core::config_manager::IndexingTier::default(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

impl From<&IndexerConfig> for codegraph_parser::file_collect::FileCollectionConfig {
    fn from(config: &IndexerConfig) -> Self {
        codegraph_parser::file_collect::FileCollectionConfig {
            recursive: config.recursive,
            languages: config.languages.clone(),
            include_patterns: config.include_patterns.clone(),
            exclude_patterns: config.exclude_patterns.clone(),
        }
    }
}

pub(crate) fn filter_edges_for_tier(
    tier: codegraph_core::config_manager::IndexingTier,
    edges: &mut Vec<EdgeRelationship>,
) -> usize {
    let before = edges.len();
    match tier {
        codegraph_core::config_manager::IndexingTier::Full => {}
        codegraph_core::config_manager::IndexingTier::Balanced => {
            edges.retain(|edge| !matches!(edge.edge_type, EdgeType::References));
        }
        codegraph_core::config_manager::IndexingTier::Fast => {
            edges.retain(|edge| !matches!(edge.edge_type, EdgeType::Uses | EdgeType::References));
        }
    }
    before.saturating_sub(edges.len())
}

pub(crate) fn extraction_policy_for_tier(
    tier: codegraph_core::config_manager::IndexingTier,
) -> codegraph_parser::languages::ExtractionPolicy {
    use codegraph_core::config_manager::IndexingTier;
    codegraph_parser::languages::ExtractionPolicy {
        uses: tier != IndexingTier::Fast,
        references: tier == IndexingTier::Full,
    }
}

/// Storage backend for one indexing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageBackend {
    /// Compiled only when the crate carries the `surrealdb` feature.
    #[cfg(feature = "surrealdb")]
    Surreal,
    Postgres,
}

impl StorageBackend {
    /// `CODEGRAPH_BACKEND` selects the backend: empty / `surreal` / `surrealdb`
    /// mean SurrealDB, `postgres` / `pg` mean Postgres. Anything else is a
    /// configuration error rather than a silent fallback.
    ///
    /// A build that carries SurrealDB keeps it as the default so existing
    /// deployments do not change backend on upgrade; a Postgres-only build
    /// defaults to Postgres and rejects an explicit `surreal` request.
    pub fn from_env() -> Result<Self> {
        let raw = std::env::var("CODEGRAPH_BACKEND").unwrap_or_default();
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => {
                #[cfg(feature = "surrealdb")]
                {
                    Ok(Self::Surreal)
                }
                #[cfg(not(feature = "surrealdb"))]
                {
                    Ok(Self::Postgres)
                }
            }
            #[cfg(feature = "surrealdb")]
            "surreal" | "surrealdb" => Ok(Self::Surreal),
            #[cfg(not(feature = "surrealdb"))]
            "surreal" | "surrealdb" => Err(anyhow!(
                "CODEGRAPH_BACKEND=surreal requires a build with --features surrealdb"
            )),
            "postgres" | "pg" => Ok(Self::Postgres),
            other => Err(anyhow!(
                "unknown CODEGRAPH_BACKEND `{other}`; expected `surreal` or `postgres`"
            )),
        }
    }
}

/// Whether a caller may create schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// `codegraph query`: never issue DDL. A missing schema is an error that
    /// tells the user to index first, not something to repair silently.
    ReadOnly,
    /// `codegraph plan store` / `plan drop`: apply the schema before writing.
    ReadWrite,
}

/// Open one storage handle for callers that only read the graph (the `query` and
/// `plan` CLIs). Unlike the indexer this does not build a writer pool: those
/// callers issue no bulk writes.
///
/// SurrealDB opens a fresh connection from the project's configuration; Postgres
/// reads `CODEGRAPH_PG_URL` and applies the schema so a first run works against an
/// empty database.
pub async fn open_storage(
    backend: StorageBackend,
    project_root: &std::path::Path,
    mode: StorageMode,
) -> Result<Arc<dyn IndexStorage>> {
    match backend {
        // `SurrealDbStorage::new` applies its own migration policy for both modes;
        // this does not change existing behaviour for the CLI.
        #[cfg(feature = "surrealdb")]
        StorageBackend::Surreal => {
            let (_primary, pool) = ProjectIndexer::connect_surreal(project_root).await?;
            let handle = pool
                .first()
                .ok_or_else(|| anyhow!("surreal storage pool is empty"))?;
            let storage = handle.lock().await.clone();
            Ok(Arc::new(storage))
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres => {
            let store = codegraph_graph::postgres_storage::PgStore::connect("")?;
            match mode {
                StorageMode::ReadWrite => store.ensure_schema().await?,
                StorageMode::ReadOnly => {
                    if !store.schema_present().await? {
                        return Err(anyhow!(
                            "project not indexed: the Postgres schema is missing at CODEGRAPH_PG_URL; \
                             run `codegraph index` first (or set CODEGRAPH_BACKEND=surreal)"
                        ));
                    }
                }
            }
            Ok(Arc::new(store))
        }
        #[cfg(not(feature = "postgres"))]
        StorageBackend::Postgres => Err(anyhow!(
            "CODEGRAPH_BACKEND=postgres requires a build with --features postgres"
        )),
    }
}

pub struct ProjectIndexer {
    config: IndexerConfig,
    global_config: codegraph_core::config_manager::CodeGraphConfig,
    progress: MultiProgress,
    parser: TreeSitterParser,
    /// Present on the SurrealDB backend; `None` when `CODEGRAPH_BACKEND=postgres`.
    #[cfg(feature = "surrealdb")]
    surreal: Option<Arc<TokioMutex<SurrealDbStorage>>>,
    /// Backend-neutral view of the same store `surreal` points at, so the
    /// indexing run no longer issues SurrealQL directly.
    storage: Arc<dyn codegraph_graph::storage::IndexStorage>,
    surreal_writer: Option<SurrealWriterHandle>,
    project_id: String,
    organization_id: Option<String>,
    repository_url: Option<String>,
    domain: Option<String>,
    embedding_model: String,
    vector_dim: usize,
    embedding_column: EmbeddingColumn,
    project_root: PathBuf,
    reconcile_lock: TokioMutex<()>,
    policies: crate::policy::InferencePolicies,
    startup_ms: u64,
    #[cfg(feature = "embeddings")]
    cpu_pool: Arc<rayon::ThreadPool>,
    #[cfg(feature = "embeddings")]
    embedder: Option<codegraph_vector::EmbeddingGenerator>,
}

use crate::writer::SurrealWriterHandle;

impl ProjectIndexer {
    #[cfg(feature = "ai-enhanced")]
    async fn compute_node_degrees(&self) -> Result<std::collections::HashMap<NodeId, i32>> {
        // Lightweight degree map using already-enqueued edges would require DB reads; to keep indexing fast
        // and avoid extra I/O, return empty map for now (degrees default to 0 in tie-breaks).
        Ok(std::collections::HashMap::new())
    }
    #[cfg(feature = "ai-enhanced")]
    fn symbol_embedding_batch_settings(&self) -> (usize, usize) {
        let config_batch = self
            .config
            .symbol_batch_size
            .unwrap_or(self.config.batch_size);
        let config_concurrent = self
            .config
            .symbol_max_concurrent
            .unwrap_or_else(|| self.config.max_concurrent.max(2));

        let batch_size = std::env::var("CODEGRAPH_SYMBOL_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(config_batch)
            .max(1);

        let max_concurrent = std::env::var("CODEGRAPH_SYMBOL_MAX_CONCURRENT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(config_concurrent)
            .clamp(1, 32);

        (batch_size, max_concurrent)
    }

    pub fn new(
        mut config: IndexerConfig,
        global_config: &codegraph_core::config_manager::CodeGraphConfig,
        multi_progress: MultiProgress,
    ) -> futures::future::BoxFuture<'_, Result<Self>> {
        Box::pin(async move {
            let startup = std::time::Instant::now();
            let project_id = std::env::var("CODEGRAPH_PROJECT_ID")
                .unwrap_or_else(|_| config.project_root.display().to_string());
            let policies = if config.complete_deferred {
                crate::policy::completion_policies(&config.project_root, &project_id)?
            } else {
                crate::policy::InferencePolicies::from_env()?
            };
            // Cap Rayon threads to a sensible default: leave one core free and obey user overrides
            let available = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let env_workers = std::env::var("CODEGRAPH_WORKERS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
            let requested = std::env::var("RAYON_NUM_THREADS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|threads| *threads > 0)
                .or(env_workers)
                .unwrap_or(config.workers);
            #[cfg(feature = "embeddings")]
            let cpu_pool = Arc::new(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(requested.max(1).min(available.max(1)))
                    .build()?,
            );

            // Environment defaults are resolved by ConfigManager. Keep the caller's
            // batch authoritative and pass the same value into every provider.
            config.batch_size = config.batch_size.max(1);
            let mut global_config = global_config.clone();
            global_config.embedding.batch_size = config.batch_size;

            config.workers = requested.max(1).min(available.max(1));
            let parser = TreeSitterParser::new()
                .with_concurrency(config.workers)
                .with_project_root(&config.project_root)
                .with_extraction_policy(extraction_policy_for_tier(config.indexing_tier));
            let project_root = config.project_root.clone();
            let backend = StorageBackend::from_env()?;
            #[cfg(feature = "surrealdb")]
            let (surreal, storage, storage_pool) = match backend {
                StorageBackend::Surreal => {
                    let (surreal, surreal_pool) = Self::connect_surreal(&project_root).await?;
                    // The queue is backend-neutral. `SurrealDbStorage` is
                    // `#[derive(Clone)]` over `Arc` fields, so each handle clones a
                    // shared connection rather than opening a new one; the trait
                    // impl clones again per write.
                    let mut storage_pool: Vec<Arc<dyn IndexStorage>> =
                        Vec::with_capacity(surreal_pool.len());
                    for handle in &surreal_pool {
                        let cloned = handle.lock().await.clone();
                        storage_pool.push(Arc::new(cloned) as Arc<dyn IndexStorage>);
                    }
                    let storage = Arc::clone(
                        storage_pool
                            .first()
                            .ok_or_else(|| anyhow!("surreal storage pool is empty"))?,
                    );
                    (Some(surreal), storage, storage_pool)
                }
                #[cfg(feature = "postgres")]
                StorageBackend::Postgres => {
                    let store = codegraph_graph::postgres_storage::PgStore::connect("")?;
                    store.ensure_schema().await?;
                    let storage: Arc<dyn IndexStorage> = Arc::new(store);
                    info!("🗄️ Storage backend: Postgres (pgvector)");
                    (None, Arc::clone(&storage), vec![storage])
                }
                #[cfg(not(feature = "postgres"))]
                StorageBackend::Postgres => {
                    return Err(anyhow!(
                        "CODEGRAPH_BACKEND=postgres requires a build with --features postgres"
                    ));
                }
            };
            #[cfg(not(feature = "surrealdb"))]
            let (storage, storage_pool) = match backend {
                #[cfg(feature = "postgres")]
                StorageBackend::Postgres => {
                    let store = codegraph_graph::postgres_storage::PgStore::connect("")?;
                    store.ensure_schema().await?;
                    let storage: Arc<dyn IndexStorage> = Arc::new(store);
                    info!("🗄️ Storage backend: Postgres (pgvector)");
                    (Arc::clone(&storage), vec![storage])
                }
                #[cfg(not(feature = "postgres"))]
                StorageBackend::Postgres => {
                    return Err(anyhow!(
                        "CODEGRAPH_BACKEND=postgres requires a build with --features postgres"
                    ));
                }
            };
            let surreal_writer = SurrealWriterHandle::new(storage_pool);
            let project_id = std::env::var("CODEGRAPH_PROJECT_ID")
                .unwrap_or_else(|_| project_root.display().to_string());
            let organization_id = std::env::var("CODEGRAPH_ORGANIZATION_ID").ok();
            let repository_url = std::env::var("CODEGRAPH_REPOSITORY_URL").ok();
            let domain = std::env::var("CODEGRAPH_DOMAIN").ok();
            #[cfg(feature = "embeddings")]
            let embedder = if policies.needs_provider() {
                let embedder = {
                    use codegraph_vector::EmbeddingGenerator;
                    // Use global config for embedding provider
                    let provider = global_config.embedding.provider.to_lowercase();
                    if provider == "local" {
                        #[cfg(feature = "embeddings-local")]
                        {
                            use codegraph_vector::embeddings::generator::{
                                AdvancedEmbeddingGenerator, EmbeddingEngineConfig,
                                LocalDeviceTypeCompat, LocalEmbeddingConfigCompat,
                                LocalPoolingCompat,
                            };
                            let mut cfg = EmbeddingEngineConfig {
                                prefer_local_first: true,
                                batch_size: config.batch_size,
                                ..Default::default()
                            };
                            let device = match config
                                .device
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .as_str()
                            {
                                "metal" => LocalDeviceTypeCompat::Metal,
                                d if d.starts_with("cuda:") => {
                                    let id =
                                        d.trim_start_matches("cuda:").parse::<usize>().unwrap_or(0);
                                    LocalDeviceTypeCompat::Cuda(id)
                                }
                                _ => LocalDeviceTypeCompat::Cpu,
                            };
                            let model_name =
                                global_config.embedding.model.clone().unwrap_or_else(|| {
                                    "sentence-transformers/all-MiniLM-L6-v2".to_string()
                                });
                            cfg.local = Some(LocalEmbeddingConfigCompat {
                                model_name,
                                device,
                                cache_dir: None,
                                max_sequence_length: config.max_seq_len.max(32),
                                pooling_strategy: LocalPoolingCompat::Mean,
                            });
                            // Try to construct advanced engine; fall back to simple generator on error
                            match AdvancedEmbeddingGenerator::new(cfg).await {
                                Ok(engine) => {
                                    if !engine.has_provider() {
                                        return Err(anyhow::anyhow!(
                                            "Local embedding provider constructed without a backend. Ensure the model is BERT-compatible with safetensors and try --device metal or --device cpu"
                                        ));
                                    }
                                    let mut g = EmbeddingGenerator::default();
                                    g.set_advanced_engine(std::sync::Arc::new(engine));
                                    tracing::info!(
                                        target: "codegraph_mcp::indexer",
                                        "Active embeddings: Local (device: {}, max_seq_len: {}, batch_size: {})",
                                        config.device.as_deref().unwrap_or("cpu"),
                                        config.max_seq_len,
                                        config.batch_size
                                    );
                                    g
                                }
                                Err(e) => {
                                    return Err(anyhow::anyhow!(
                                        "Failed to initialize local embedding provider: {}",
                                        e
                                    ));
                                }
                            }
                        }
                        #[cfg(not(feature = "embeddings-local"))]
                        {
                            tracing::warn!(
                                target: "codegraph_mcp::indexer",
                                "CODEGRAPH_EMBEDDING_PROVIDER=local requested but the 'embeddings-local' feature is not enabled; using auto provider"
                            );
                            #[allow(unused_mut)]
                            let mut g = EmbeddingGenerator::with_auto_from_env().await?;
                            // Set batch_size and max_concurrent for Jina provider if applicable
                            #[cfg(feature = "embeddings-jina")]
                            {
                                g.set_jina_batch_size(config.batch_size);
                                g.set_jina_max_concurrent(config.max_concurrent);
                            }
                            g
                        }
                    } else {
                        #[allow(unused_mut)]
                        let mut g = EmbeddingGenerator::with_config(&global_config).await?;
                        // Set batch_size and max_concurrent for Jina provider if applicable
                        #[cfg(feature = "embeddings-jina")]
                        {
                            g.set_jina_batch_size(config.batch_size);
                            g.set_jina_max_concurrent(config.max_concurrent);
                        }
                        tracing::info!(
                            target: "codegraph_mcp::indexer",
                            "Active embeddings: {} (batch_size: {}, max_concurrent: {})",
                            global_config.embedding.provider,
                            config.batch_size,
                            config.max_concurrent
                        );
                        g
                    }
                };
                if !embedder.has_provider() {
                    return Err(anyhow!(
                        "No embedding backend initialized for provider {}. Configure a supported provider or set both inference policies to off/deferred.",
                        global_config.embedding.provider
                    ));
                }
                Some(embedder)
            } else {
                None
            };
            let embedding_model_name = global_config
                .embedding
                .model
                .clone()
                .unwrap_or_else(|| "jina-embeddings-v4".to_string());

            let embedder_dimension = {
                #[cfg(feature = "embeddings")]
                {
                    embedder
                        .as_ref()
                        .map(|embedder| embedder.dimension())
                        .unwrap_or(global_config.embedding.dimension)
                }
                #[cfg(not(feature = "embeddings"))]
                {
                    config.vector_dimension
                }
            };

            let env_vector_dim = std::env::var("CODEGRAPH_EMBEDDING_DIMENSION")
                .ok()
                .and_then(|v| v.parse::<usize>().ok());
            let vector_dim = env_vector_dim.unwrap_or(embedder_dimension);
            let embedding_column = resolve_embedding_column(vector_dim).with_context(|| {
            format!(
                "Unsupported embedding dimension {}. Supported dimensions: 384, 768, 1024, 2048, 2560, 4096.",
                vector_dim
            )
        })?;

            if let Some(v) = env_vector_dim {
                info!(
                    "🧭 Embedding dimension override: CODEGRAPH_EMBEDDING_DIMENSION={} → Surreal column {}",
                    v,
                    embedding_column.column_name()
                );
            } else {
                info!(
                    "🧭 Embedding dimension resolved from provider ({}): {} → Surreal column {}",
                    global_config.embedding.provider,
                    vector_dim,
                    embedding_column.column_name()
                );
            }

            #[cfg(feature = "surrealdb")]
            {
                let vector_mode = codegraph_graph::vector_indexes::VectorIndexMode::from_env()?;
                let vector_tables = Self::vector_tables(policies);
                if vector_mode == codegraph_graph::vector_indexes::VectorIndexMode::Selected {
                    // SurrealDB vector indexes are created here; Postgres applies its
                    // own pgvector DDL in `PgStore::ensure_schema`.
                    if let Some(surreal) = surreal.as_ref() {
                        codegraph_graph::vector_indexes::ensure_ready(
                            &surreal.lock().await.db(),
                            vector_dim,
                            &vector_tables,
                        )
                        .await?;
                    } else {
                        debug!(
                            "vector index setup skipped: Postgres backend uses its own pgvector DDL"
                        );
                    }
                }
            }
            #[cfg(not(feature = "surrealdb"))]
            {
                // A Postgres-only build applies its pgvector DDL in
                // `PgStore::ensure_schema`; there is no SurrealDB index to prepare.
            }
            #[cfg(feature = "embeddings")]
            let embedder = if let Some(mut embedder) = embedder {
                let mut identity = serde_json::to_value(&global_config.embedding)?;
                identity.as_object_mut().unwrap().remove("openai_api_key");
                identity.as_object_mut().unwrap().remove("jina_api_key");
                identity.as_object_mut().unwrap().insert(
                    "active_model".into(),
                    serde_json::json!(embedding_model_name),
                );
                identity.as_object_mut().unwrap().insert(
                    "runtime".into(),
                    serde_json::json!([
                        std::env::var("CODEGRAPH_LOCAL_DTYPE").unwrap_or_default(),
                        std::env::var("CODEGRAPH_ONNX_MODEL_FILE").unwrap_or_default(),
                        std::env::var("CODEGRAPH_ONNX_EP").unwrap_or_default(),
                        std::env::var("CODEGRAPH_COREML_LOW_PRECISION").unwrap_or_default(),
                    ]),
                );
                embedder.configure_index_cache(
                    project_root.join(".codegraph/index-cache"),
                    &identity,
                    vector_dim,
                    &global_config.embedding.provider,
                    config.batch_size,
                )?;
                Some(embedder)
            } else {
                None
            };
            Ok(Self {
                config,
                global_config: global_config.clone(),
                progress: multi_progress,
                parser,
                #[cfg(feature = "surrealdb")]
                surreal,
                storage,
                surreal_writer: Some(surreal_writer),
                project_id,
                organization_id,
                repository_url,
                domain,
                embedding_model: embedding_model_name,
                vector_dim,
                embedding_column,
                project_root,
                reconcile_lock: TokioMutex::new(()),
                policies,
                startup_ms: startup.elapsed().as_millis() as u64,
                #[cfg(feature = "embeddings")]
                cpu_pool,
                #[cfg(feature = "embeddings")]
                embedder,
            })
        })
    }

    pub fn set_indexing_tier(&mut self, tier: codegraph_core::config_manager::IndexingTier) {
        self.config.indexing_tier = tier;
        self.parser = TreeSitterParser::new()
            .with_concurrency(self.config.workers)
            .with_project_root(&self.project_root)
            .with_extraction_policy(extraction_policy_for_tier(tier));
    }
    fn vector_tables(policies: crate::policy::InferencePolicies) -> Vec<&'static str> {
        let mut tables = Vec::new();
        if policies.embeddings == crate::policy::StagePolicy::Sync {
            tables.push("chunks");
        }
        if policies.semantic == crate::policy::StagePolicy::Sync {
            tables.push("symbol_embeddings");
        }
        tables
    }
    pub async fn index_project(&self, path: impl AsRef<Path>) -> Result<IndexStats> {
        self.reconcile_project(path.as_ref(), self.config.force_reindex)
            .await
    }

    pub fn reconcile_project<'a>(
        &'a self,
        path: &'a Path,
        force: bool,
    ) -> futures::future::BoxFuture<'a, Result<IndexStats>> {
        Box::pin(async move {
            let _run = self.reconcile_lock.lock().await;
            let start = std::time::Instant::now();
            let cache_root = self.project_root.join(".codegraph/index-cache");
            let cache_budget = std::env::var("CODEGRAPH_INDEX_CACHE_BYTES")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(4 * 1024 * 1024 * 1024);
            if let Err(error) = tokio::task::spawn_blocking(move || {
                codegraph_core::artifact_cache::ArtifactCache::prune_tree(
                    &cache_root,
                    cache_budget,
                    &["pending-inference-v1"],
                )
            })
            .await?
            {
                warn!("Index artifact eviction failed: {error}");
            }
            let mut phase_ms = std::collections::BTreeMap::new();
            phase_ms.insert(
                "cache_eviction".to_owned(),
                start.elapsed().as_millis() as u64,
            );
            let writer_before = self
                .surreal_writer
                .as_ref()
                .map(|writer| writer.metrics())
                .unwrap_or_default();
            #[cfg(feature = "embeddings")]
            let inference_before = self
                .embedder
                .as_ref()
                .map(|embedder| embedder.inference_stats())
                .unwrap_or_default();
            info!("Starting project indexing: {:?}", path);
            self.log_surrealdb_status("pre-parse");

            let capture_start = std::time::Instant::now();
            let file_config: codegraph_parser::file_collect::FileCollectionConfig =
                (&self.config).into();
            let all_files = codegraph_parser::file_collect::collect_source_files_with_config(
                path,
                &file_config,
            )?;
            let source_snapshots = codegraph_parser::SourceSnapshots::capture(
                &all_files,
                crate::estimation::source_memory_budget(),
                self.parser.concurrency(),
            )
            .await?;

            phase_ms.insert(
                "source_capture".to_owned(),
                capture_start.elapsed().as_millis() as u64,
            );
            let inputs_start = std::time::Instant::now();
            use codegraph_core::artifact_cache::{ArtifactCache, fingerprint};
            let state_cache = ArtifactCache::new(
                self.project_root.join(".codegraph/index-cache"),
                "project-catalog-v1",
            );
            let root = self.project_root.clone();
            let support = tokio::task::spawn_blocking(move || {
                crate::reconciliation::support_fingerprints(&root)
            })
            .await??;
            let mut embedding_config = serde_json::to_value(&self.global_config.embedding)?;
            #[cfg(feature = "embeddings")]
            if let Some(embedder) = &self.embedder {
                embedding_config.as_object_mut().unwrap().insert(
                    "input_policy".into(),
                    serde_json::json!(embedder.input_identity()?),
                );
            }
            embedding_config
                .as_object_mut()
                .unwrap()
                .remove("openai_api_key");
            embedding_config
                .as_object_mut()
                .unwrap()
                .remove("jina_api_key");
            let policy: std::collections::BTreeMap<String, String> = [
                "CODEGRAPH_SEMANTIC_CANDIDATES",
                "CODEGRAPH_LOCAL_DTYPE",
                "CODEGRAPH_ONNX_MODEL_FILE",
                "CODEGRAPH_ONNX_EP",
                "CODEGRAPH_COREML_LOW_PRECISION",
                "CODEGRAPH_MODEL_REVISION",
                "CODEGRAPH_TOKENIZER_PATH",
                "CODEGRAPH_CHUNK_SPLITTER",
                "CODEGRAPH_CHUNK_MAX_TOKENS",
                "CODEGRAPH_MAX_CHUNK_TOKENS",
                "CODEGRAPH_MODEL_MAX_TOKENS",
                "CODEGRAPH_OLLAMA_NUM_CTX",
                "CODEGRAPH_TOKENIZER_REPO",
                "CODEGRAPH_TOKENIZER_REVISION",
                "CODEGRAPH_EMBEDDING_DOCUMENT_PREFIX",
                "CODEGRAPH_EMBEDDING_QUERY_PREFIX",
                "CODEGRAPH_CHUNK_OVERLAP_TOKENS",
                "CODEGRAPH_CHUNK_SMART_SPLIT",
                "CODEGRAPH_EMBEDDING_SKIP_CHUNKING",
                "CODEGRAPH_ANALYZERS",
                "CODEGRAPH_ANALYZERS_REQUIRE_TOOLS",
                "CODEGRAPH_PROJECT_ID",
                "CODEGRAPH_ORGANIZATION_ID",
                "CODEGRAPH_REPOSITORY_URL",
                "CODEGRAPH_DOMAIN",
                "RUSTFLAGS",
                "CARGO_BUILD_TARGET",
                "RUSTUP_TOOLCHAIN",
                "CODEGRAPH_VECTOR_INDEX_MODE",
            ]
            .into_iter()
            .map(|key| (key.to_owned(), std::env::var(key).unwrap_or_default()))
            .collect();
            let mut external_support =
                crate::analyzers::build_context::external_input_fingerprints(&self.project_root)?;
            let mutable_model_epoch = self.policies.model_epoch(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
                std::env::var("CODEGRAPH_EMBEDDING_CACHE_TTL_SECONDS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(3600),
                std::env::var_os("CODEGRAPH_MODEL_REVISION").is_some(),
            );
            let tokenizer_fingerprint = std::env::var_os("CODEGRAPH_TOKENIZER_PATH")
                .map(PathBuf::from)
                .map(|path| crate::reconciliation::file_fingerprint(&path))
                .transpose()?;
            let scip_specs = if self.config.scip_auto || self.config.scip_strict {
                let explicit = std::env::var("CODEGRAPH_SCIP_INDEX")
                    .ok()
                    .filter(|value| !value.trim().is_empty());
                if explicit.is_some() {
                    info!("CODEGRAPH_SCIP_INDEX is set; ignoring --scip-auto");
                    parse_scip_indexes(explicit.as_deref())?
                } else {
                    let scip_dir = self.project_root.join(".codegraph/scip");
                    let entries = crate::scip_auto::ensure_indexes(
                        &self.project_root,
                        &scip_dir,
                        self.config.scip_strict,
                    )?;
                    if entries.is_empty() {
                        warn!("--scip-auto produced no indexes; no SCIP enrichment will run");
                        Vec::new()
                    } else {
                        info!(
                            "SCIP auto-generation produced {} index(es): {}",
                            entries.len(),
                            entries.join(", ")
                        );
                        parse_scip_indexes(Some(&entries.join(",")))?
                    }
                }
            } else {
                parse_scip_indexes(std::env::var("CODEGRAPH_SCIP_INDEX").ok().as_deref())?
            };
            // Print on every indexing path (fast, partial, balanced): a harness that only
            // runs `codegraph index` should still learn that SCIP exists.
            if scip_specs.is_empty() {
                info!(
                    "💡 hint: no SCIP index supplied, so no compiler-index enrichment will run. \
                     Generate one (`rust-analyzer scip .`, `scip-typescript index`, `scip-go`) \
                     and set CODEGRAPH_SCIP_INDEX, or run scripts/scip-index.sh. \
                     See `codegraph index --help`."
                );
            }
            let scip_fingerprint = if scip_specs.is_empty() {
                None
            } else {
                let mut indexes = Vec::with_capacity(scip_specs.len());
                for spec in &scip_specs {
                    indexes.push((
                        crate::reconciliation::file_fingerprint(&spec.path)?,
                        if spec.path.with_extension("sources.json").exists() {
                            Some(crate::reconciliation::file_fingerprint(
                                &spec.path.with_extension("sources.json"),
                            )?)
                        } else {
                            None
                        },
                        spec.path_prefix.clone(),
                    ));
                }
                Some(fingerprint(&(
                    indexes,
                    std::env::var("CODEGRAPH_SCIP_TRUST_SOURCE").unwrap_or_default(),
                ))?)
            };
            let make_fingerprint =
                |external_support: &std::collections::BTreeMap<String, String>| {
                    fingerprint(&(
                        "project-input-v7",
                        &self.project_id,
                        source_snapshots
                            .iter()
                            .map(|source| (&source.path, &source.content_hash))
                            .collect::<std::collections::BTreeMap<_, _>>(),
                        &support,
                        &external_support,
                        &tokenizer_fingerprint,
                        mutable_model_epoch,
                        &scip_fingerprint,
                        self.config.indexing_tier,
                        &embedding_config,
                        &policy,
                        self.policies.enabled_identity(),
                        self.config.max_seq_len,
                        cfg!(feature = "embeddings"),
                        cfg!(feature = "ai-enhanced"),
                    ))
                };
            let mut input_fingerprint = make_fingerprint(&external_support)?;
            let stored_fingerprint = self.storage.project_fingerprint(&self.project_id).await?;
            let marker = stored_fingerprint.as_deref();
            let previous = state_cache
                .get::<crate::reconciliation::Catalog>(&self.project_id)
                .filter(|state| !force && marker == Some(state.fingerprint.as_str()));
            if let Some(state) = &previous
                && state.fingerprint == input_fingerprint
                && self
                    .policies
                    .satisfied_by(&state.stats.embedding_status, &state.stats.semantic_status)
            {
                source_snapshots.validate_current().await?;
                let mut stats = state.stats.clone();
                stats.skipped = all_files.len();
                stats.cached_files = all_files.len();
                stats.index_ms = start.elapsed().as_millis() as u64;
                stats.startup_ms = self.startup_ms;
                stats.inference_texts = 0;
                stats.inference_tokens = 0;
                stats.embedding_cache_hits = 0;
                stats.source_read_operations = source_snapshots.read_operations;
                stats.source_read_bytes = source_snapshots.read_bytes;
                stats.writer_jobs_acked = 0;
                stats.writer_rows_acked = 0;
                stats.writer_payload_bytes_acked = 0;
                phase_ms.insert(
                    "input_validation".into(),
                    inputs_start.elapsed().as_millis() as u64,
                );
                stats.phase_ms = phase_ms;
                return Ok(stats);
            }
            phase_ms.insert(
                "input_validation".into(),
                inputs_start.elapsed().as_millis() as u64,
            );
            // Every run has the full symbol universe. Unchanged source uses cached ASTs.
            let files_to_index = all_files.clone();
            let mut next_catalog = crate::reconciliation::Catalog {
                fingerprint: input_fingerprint.clone(),
                ..Default::default()
            };

            let reconcile_scope = Self::reconcile_scope(&self.project_root, path);
            let analyzer_settings = AnalyzerSettings::from_env(self.config.indexing_tier);
            let mut analyzer_languages: HashSet<codegraph_core::Language> = HashSet::new();
            let needs_language_scan = analyzer_settings.build_context || analyzer_settings.dataflow;
            if needs_language_scan {
                let registry = codegraph_parser::LanguageRegistry::new();
                for (p, _) in &files_to_index {
                    if let Some(lang) = registry.detect_language(&p.to_string_lossy()) {
                        analyzer_languages.insert(lang);
                    }
                }
            }
            let analyzer_languages: Vec<codegraph_core::Language> =
                analyzer_languages.into_iter().collect();

            info!(
                "🧭 Indexing tier: {:?} | analyzers: build_context={} enrichment={} module_linking={} dataflow={} docs_contracts={} architecture={}",
                self.config.indexing_tier,
                analyzer_settings.build_context,
                analyzer_settings.enrichment,
                analyzer_settings.module_linking,
                analyzer_settings.dataflow,
                analyzer_settings.docs_contracts,
                analyzer_settings.architecture
            );

            let build_context_task = if analyzer_settings.build_context
                && analyzer_languages.contains(&codegraph_core::Language::Rust)
            {
                let root = self.project_root.clone();
                let project = self.project_id.clone();
                Some(tokio::task::spawn_blocking(move || {
                    crate::analyzers::build_context::analyze_cargo_workspace(&root, &project)
                }))
            } else {
                None
            };

            // STAGE 1: File Collection & Parsing
            let files = files_to_index;
            let total_files = files.len();

            // Single progress bar for unified AST + fast_ml extraction
            let ast_pb = self.create_progress_bar(
                total_files as u64,
                "🌳 Parsing & extracting (TreeSitter + FastML)",
            );

            // REVOLUTIONARY: Use unified extraction for nodes + edges in single pass (FASTEST approach)
            // Clone files for parsing (we need them again for metadata persistence)
            let ast_cache = codegraph_core::artifact_cache::ArtifactCache::new(
                self.project_root.join(".codegraph/index-cache"),
                "unified-ast-v5",
            );
            let parse_start = std::time::Instant::now();
            let (mut nodes, mut edges, pstats) = crate::estimation::parse_snapshots_with_cache(
                &self.parser,
                files.clone(),
                total_files as u64,
                &source_snapshots,
                Some(&ast_cache),
            )
            .await?;
            phase_ms.insert("ast_parse".into(), parse_start.elapsed().as_millis() as u64);
            let analyzers_start = std::time::Instant::now();
            let cache_budget = std::env::var("CODEGRAPH_AST_CACHE_BYTES")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(2 * 1024 * 1024 * 1024);
            let cache_for_prune = ast_cache.clone();
            if let Err(error) =
                tokio::task::spawn_blocking(move || cache_for_prune.prune(cache_budget)).await?
            {
                warn!("AST cache eviction failed: {error}");
            }
            if pstats.failed_files > 0 {
                return Err(anyhow!(
                    "{} source files failed parsing; file metadata will not be marked current",
                    pstats.failed_files
                ));
            }

            let build_context_out = match build_context_task {
                Some(task) => task.await??,
                None => Default::default(),
            };
            external_support =
                crate::analyzers::build_context::external_input_fingerprints(&self.project_root)?;
            input_fingerprint = make_fingerprint(&external_support)?;
            next_catalog.fingerprint = input_fingerprint.clone();
            let build_context_nodes = build_context_out.nodes.len();
            let build_context_edges = build_context_out.edges.len();
            if analyzer_settings.build_context
                && (!build_context_out.nodes.is_empty() || !build_context_out.edges.is_empty())
            {
                nodes.extend(build_context_out.nodes);
                edges.extend(build_context_out.edges);
            }

            // Stable project-relative posix file paths: node ids embed the path, so the
            // same file must not appear as `./src/a.rs`, `src/a.rs` or an absolute path
            // depending on how `codegraph index` was invoked. The normalized -> original
            // map keeps chunk source lookups working against the captured snapshots.
            let mut source_by_normalized: std::collections::HashMap<String, PathBuf> =
                std::collections::HashMap::new();
            for (path, _) in &files {
                source_by_normalized
                    .entry(normalize_file_path(
                        &self.project_root,
                        &path.to_string_lossy(),
                    ))
                    .or_insert_with(|| path.clone());
            }
            for node in nodes.iter_mut() {
                node.location.file_path =
                    normalize_file_path(&self.project_root, &node.location.file_path);
            }

            let removed_edges = filter_edges_for_tier(self.config.indexing_tier, &mut edges);
            if removed_edges > 0 {
                info!(
                    "🧹 Tier edge filter removed {} edges (tier: {:?})",
                    removed_edges, self.config.indexing_tier
                );
            }

            self.finish_bar(
                ast_pb,
                format!(
                    "🌳 Parsed {} files | nodes: {} | edges: {}",
                    pstats.parsed_files,
                    nodes.len(),
                    edges.len()
                ),
            )?;

            // Generate deterministic IDs and build old_id -> new_id mapping to update edge references
            let mut id_mapping: std::collections::HashMap<NodeId, NodeId> =
                std::collections::HashMap::with_capacity(nodes.len());
            let mut identities: std::collections::HashMap<
                NodeId,
                std::collections::BTreeSet<String>,
            > = std::collections::HashMap::new();
            for node in &nodes {
                let id = codegraph_core::generate_node_id(
                    &self.project_id,
                    &node.location.file_path,
                    node.name.as_str(),
                    &node
                        .node_type
                        .as_ref()
                        .map(|kind| format!("{kind:?}"))
                        .unwrap_or_else(|| "Unknown".into()),
                    node.location.line,
                );
                identities.entry(id).or_default().insert(fingerprint(&(
                    &node.location,
                    &node.span,
                    node.metadata.attributes.get("qualified_name"),
                ))?);
            }
            for node in nodes.iter_mut() {
                let old_id = node.id;
                node.set_deterministic_id(&self.project_id);
                if identities
                    .get(&node.id)
                    .is_some_and(|identities| identities.len() > 1)
                {
                    let identity = format!(
                        "{}#{}:{}",
                        node.name,
                        node.location.column,
                        node.metadata
                            .attributes
                            .get("qualified_name")
                            .map(String::as_str)
                            .unwrap_or("")
                    );
                    node.id = codegraph_core::generate_node_id(
                        &self.project_id,
                        &node.location.file_path,
                        &identity,
                        &node
                            .node_type
                            .as_ref()
                            .map(|kind| format!("{kind:?}"))
                            .unwrap_or_else(|| "Unknown".into()),
                        node.location.line,
                    );
                }
                id_mapping.insert(old_id, node.id);
                self.annotate_node(node);
            }

            // Update edge.from references to use new deterministic IDs
            for edge in edges.iter_mut() {
                if let Some(new_id) = id_mapping.get(&edge.from) {
                    edge.from = *new_id;
                }
                if let Some(target) = edge
                    .metadata
                    .get("target_node_id")
                    .and_then(|id| id.parse::<NodeId>().ok())
                    .and_then(|id| id_mapping.get(&id))
                {
                    edge.metadata
                        .insert("target_node_id".into(), target.to_string());
                }
                if let Some(span) = &edge.span {
                    edge.metadata.insert(
                        "source_span".to_string(),
                        format!("{}:{}", span.start_byte, span.end_byte),
                    );
                }
            }

            nodes.sort_by_key(|node| node.id);
            nodes.dedup_by_key(|node| node.id);

            let mut enrichment_stats = crate::analyzers::lsp::LspEnrichmentStats::default();
            let mut scip_covered: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            if !scip_specs.is_empty() {
                let start = std::time::Instant::now();
                for spec in &scip_specs {
                    let index_start = std::time::Instant::now();
                    let scip_stats = crate::analyzers::scip::import(
                        &spec.path,
                        spec.path_prefix.as_deref(),
                        &self.project_root,
                        &source_snapshots,
                        &mut nodes,
                        &mut edges,
                        analyzer_settings.lsp_definitions_enabled(),
                    )?;
                    scip_covered.extend(scip_stats.covered_files.iter().cloned());
                    enrichment_stats.nodes_enriched += scip_stats.nodes_enriched;
                    enrichment_stats.edges_resolved += scip_stats.edges_resolved;
                    info!(
                        "🧠 SCIP import complete for {}{}: {} nodes enriched + {} edges resolved from {} of {} documents ({} documents had indexed nodes) in {:.1?}",
                        spec.path.display(),
                        spec.path_prefix
                            .as_ref()
                            .map(|prefix| format!(" (path prefix: {prefix})"))
                            .unwrap_or_default(),
                        scip_stats.nodes_enriched,
                        scip_stats.edges_resolved,
                        scip_stats.documents_matched,
                        scip_stats.documents,
                        scip_stats.documents_with_nodes,
                        index_start.elapsed()
                    );
                }
                info!(
                    "🧠 SCIP import complete: {} indexes, {} nodes enriched + {} edges resolved in {:.1?}",
                    scip_specs.len(),
                    enrichment_stats.nodes_enriched,
                    enrichment_stats.edges_resolved,
                    start.elapsed()
                );
                info!(
                    "🧠 SCIP coverage: {}/{} files; {} remain AST-only",
                    scip_covered.len(),
                    files.len(),
                    files.len().saturating_sub(scip_covered.len())
                );
            }

            if !scip_specs.is_empty() && scip_covered.is_empty() {
                warn!(
                    "SCIP indexes were supplied but covered none of the {} scanned files; \
                     check the `@subdir` path prefix in CODEGRAPH_SCIP_INDEX ({} indexes: {})",
                    files.len(),
                    scip_specs.len(),
                    scip_specs
                        .iter()
                        .map(|spec| spec.path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            if scip_specs.is_empty() {
                warn!(
                    "No SCIP index was supplied: no compiler-index enrichment will run. \
                     Generate indexes with `scripts/scip-index.sh --export` and set \
                     CODEGRAPH_SCIP_INDEX."
                );
            }

            let files_without_enrichment =
                files_without_enrichment(files.len().saturating_sub(scip_covered.len()));
            let mut basic_enrichment_stats =
                crate::analyzers::enrichment::EnrichmentStats::default();
            if analyzer_settings.enrichment {
                let start = std::time::Instant::now();
                info!("🧾 Enrichment analysis starting (rustdoc + api surface)");
                let project_root = self.project_root.clone();

                let mut nodes_moved = Vec::new();
                std::mem::swap(&mut nodes_moved, &mut nodes);
                let mut edges_moved = Vec::new();
                std::mem::swap(&mut edges_moved, &mut edges);

                let sources = source_snapshots.clone();
                let (mut nodes_updated, mut edges_updated, enrich_stats) =
                tokio::task::spawn_blocking(move || -> Result<(Vec<CodeNode>, Vec<EdgeRelationship>, crate::analyzers::enrichment::EnrichmentStats)> {
                    let mut nodes = nodes_moved;
                    let mut edges = edges_moved;
                    let stats =
                        crate::analyzers::enrichment::apply_basic_enrichment_with_sources(&project_root, &mut nodes, &mut edges, Some(&sources))?;
                    Ok((nodes, edges, stats))
                })
                .await??;

                std::mem::swap(&mut nodes, &mut nodes_updated);
                std::mem::swap(&mut edges, &mut edges_updated);

                basic_enrichment_stats = enrich_stats;
                info!(
                    "🧾 Enrichment analysis complete: docs={} api_marked={} exports={} reexports={} feature_enables={} lsp_uses={} in {:.1?}",
                    basic_enrichment_stats.docs_attached,
                    basic_enrichment_stats.api_marked,
                    basic_enrichment_stats.export_edges_added,
                    basic_enrichment_stats.reexport_edges_added,
                    basic_enrichment_stats.feature_enables_edges_added,
                    basic_enrichment_stats.uses_edges_derived,
                    start.elapsed()
                );
            }

            let mut module_linker_stats =
                crate::analyzers::module_linker::ModuleLinkerStats::default();
            if analyzer_settings.module_linking {
                let start = std::time::Instant::now();
                info!("🧭 Module linking starting (modules, imports, containment)");
                let project_root = self.project_root.clone();
                let project_id = self.project_id.clone();

                let mut nodes_moved = Vec::new();
                std::mem::swap(&mut nodes_moved, &mut nodes);
                let mut edges_moved = Vec::new();
                std::mem::swap(&mut edges_moved, &mut edges);

                let (mut nodes_updated, mut edges_updated, stats) =
                tokio::task::spawn_blocking(move || -> Result<(Vec<CodeNode>, Vec<EdgeRelationship>, crate::analyzers::module_linker::ModuleLinkerStats)> {
                    let mut nodes = nodes_moved;
                    let mut edges = edges_moved;
                    let stats = crate::analyzers::module_linker::link_modules(
                        &project_root,
                        &project_id,
                        &mut nodes,
                        &mut edges,
                    )?;
                    Ok((nodes, edges, stats))
                })
                .await??;

                std::mem::swap(&mut nodes, &mut nodes_updated);
                std::mem::swap(&mut edges, &mut edges_updated);
                for node in nodes.iter_mut() {
                    self.annotate_node(node);
                }

                module_linker_stats = stats;
                info!(
                    "🧭 Module linking complete: modules={} contains={} imports={} in {:.1?}",
                    module_linker_stats.module_nodes_added,
                    module_linker_stats.contains_edges_added,
                    module_linker_stats.module_import_edges_added,
                    start.elapsed()
                );
            }

            let mut dataflow_stats = crate::analyzers::dataflow::DataflowStats::default();
            if analyzer_settings.dataflow
                && analyzer_languages.contains(&codegraph_core::Language::Rust)
            {
                let start = std::time::Instant::now();
                info!("🌊 Dataflow enrichment starting (local def-use)");
                let project_root = self.project_root.clone();
                let project_id = self.project_id.clone();

                let mut nodes_moved = Vec::new();
                std::mem::swap(&mut nodes_moved, &mut nodes);
                let mut edges_moved = Vec::new();
                std::mem::swap(&mut edges_moved, &mut edges);

                let (mut nodes_updated, mut edges_updated, stats) =
                tokio::task::spawn_blocking(move || -> Result<(Vec<CodeNode>, Vec<EdgeRelationship>, crate::analyzers::dataflow::DataflowStats)> {
                    let mut nodes = nodes_moved;
                    let mut edges = edges_moved;
                    let stats = crate::analyzers::dataflow::enrich_rust_dataflow(
                        &project_root,
                        &project_id,
                        &mut nodes,
                        &mut edges,
                    )?;
                    Ok((nodes, edges, stats))
                })
                .await??;

                std::mem::swap(&mut nodes, &mut nodes_updated);
                std::mem::swap(&mut edges, &mut edges_updated);
                for node in nodes.iter_mut() {
                    self.annotate_node(node);
                }

                dataflow_stats = stats;
                info!(
                    "🌊 Dataflow enrichment complete: vars={} defines={} uses={} flows={} returns={} mutates={} in {:.1?}",
                    dataflow_stats.variable_nodes_added,
                    dataflow_stats.defines_edges_added,
                    dataflow_stats.uses_edges_added,
                    dataflow_stats.flows_to_edges_added,
                    dataflow_stats.returns_edges_added,
                    dataflow_stats.mutates_edges_added,
                    start.elapsed()
                );
            }

            let mut docs_contracts_stats =
                crate::analyzers::docs_contracts::DocsContractsStats::default();
            if analyzer_settings.docs_contracts {
                let start = std::time::Instant::now();
                info!("📚 Docs/contracts linking starting");
                let project_root = self.project_root.clone();
                let project_id = self.project_id.clone();

                let mut nodes_moved = Vec::new();
                std::mem::swap(&mut nodes_moved, &mut nodes);
                let mut edges_moved = Vec::new();
                std::mem::swap(&mut edges_moved, &mut edges);

                let (mut nodes_updated, mut edges_updated, stats) =
                tokio::task::spawn_blocking(move || -> Result<(Vec<CodeNode>, Vec<EdgeRelationship>, crate::analyzers::docs_contracts::DocsContractsStats)> {
                    let mut nodes = nodes_moved;
                    let mut edges = edges_moved;
                    let stats = crate::analyzers::docs_contracts::link_docs_and_contracts(
                        &project_root,
                        &project_id,
                        &mut nodes,
                        &mut edges,
                    )?;
                    Ok((nodes, edges, stats))
                })
                .await??;

                std::mem::swap(&mut nodes, &mut nodes_updated);
                std::mem::swap(&mut edges, &mut edges_updated);
                for node in nodes.iter_mut() {
                    self.annotate_node(node);
                }

                docs_contracts_stats = stats;
                info!(
                    "📚 Docs/contracts linking complete: docs={} documents={} specifies={} in {:.1?}",
                    docs_contracts_stats.document_nodes_added,
                    docs_contracts_stats.document_edges_added,
                    docs_contracts_stats.specification_edges_added,
                    start.elapsed()
                );
            }

            let mut architecture_stats =
                crate::analyzers::architecture::ArchitectureStats::default();
            if analyzer_settings.architecture {
                let start = std::time::Instant::now();
                info!("🏛️  Architecture analysis starting (cycles, boundaries)");

                architecture_stats = crate::analyzers::architecture::analyze_architecture(
                    &self.project_root,
                    &nodes,
                    &mut edges,
                )?;
                info!(
                    "🏛️  Architecture analysis complete: package_cycles={} boundary_violations={} in {:.1?}",
                    architecture_stats.package_cycles_detected,
                    architecture_stats.boundary_violations_added,
                    start.elapsed()
                );
            }

            phase_ms.insert(
                "analyzers".into(),
                analyzers_start.elapsed().as_millis() as u64,
            );
            let chunks_start = std::time::Instant::now();
            // Analyzers can add nodes with raw paths; normalize again before chunking and
            // before reconciliation groups nodes by file.
            for node in nodes.iter_mut() {
                node.location.file_path =
                    normalize_file_path(&self.project_root, &node.location.file_path);
            }
            // Store counts for final summary (before consumption)
            let total_nodes_extracted = nodes.len();
            let total_edges_extracted = edges.len();

            // Build chunk plan early so we can annotate nodes with chunk counts before persistence
            #[cfg(feature = "embeddings")]
            let chunk_plan: ChunkPlan = if self.policies.embeddings
                == crate::policy::StagePolicy::Sync
            {
                let start = std::time::Instant::now();
                // Spinner to show chunking progress (Rayon internal; no granular ticks available)
                let chunk_pb = self.progress.add(ProgressBar::new_spinner());
                chunk_pb.set_style(
                    ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] {msg}")
                        .unwrap()
                        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
                );
                chunk_pb.set_message("🧩 Building chunk plan (chunking nodes)");
                chunk_pb.enable_steady_tick(std::time::Duration::from_millis(120));

                let chunker = || {
                    self.embedder
                        .as_ref()
                        .expect("sync provider initialized")
                        .chunk_nodes_with_source_lookup(&nodes, |file| {
                            source_by_normalized
                                .get(file)
                                .and_then(|path| source_snapshots.get(path))
                                .or_else(|| source_snapshots.get(file))
                                .and_then(|snapshot| snapshot.contents().ok())
                        })
                };
                let plan = self.cpu_pool.install(chunker)?;
                let elapsed = start.elapsed();
                self.finish_bar(
                    chunk_pb,
                    format!(
                        "🧩 Chunk plan built: {} nodes → {} chunks in {:.1?}",
                        plan.stats.total_nodes, plan.stats.total_chunks, elapsed
                    ),
                )?;
                if plan.stats.total_chunks == 0 && !nodes.is_empty() {
                    warn!(
                        "Chunking produced zero chunks. Check CODEGRAPH_EMBEDDING_SKIP_CHUNKING, embedding provider availability, and model max_tokens settings."
                    );
                }
                plan
            } else {
                ChunkPlan::empty()
            };
            #[cfg(not(feature = "embeddings"))]
            {
                info!("⚠️ Embeddings feature disabled at compile time; chunking skipped");
                let _chunk_plan: Option<()> = None;
            }

            #[cfg(feature = "embeddings")]
            {
                let mut node_chunk_counts: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::new();
                for meta in &chunk_plan.metas {
                    *node_chunk_counts.entry(meta.node_index).or_insert(0) += 1;
                }

                for (idx, node) in nodes.iter_mut().enumerate() {
                    let count = node_chunk_counts.get(&idx).cloned().unwrap_or(0);
                    node.metadata
                        .attributes
                        .insert("chunk_count".to_string(), count.to_string());
                }
            }

            phase_ms.insert(
                "chunk_plan".into(),
                chunks_start.elapsed().as_millis() as u64,
            );
            let persist_start = std::time::Instant::now();
            let success_rate = if pstats.total_files > 0 {
                (pstats.parsed_files as f64 / pstats.total_files as f64) * 100.0
            } else {
                100.0
            };

            let parse_completion_msg = format!(
                "🌳 Unified fast_ml + AST extraction complete: {}/{} files (✅ {:.1}% success) | 📊 {} nodes + {} edges | ⚡ {:.0} lines/s",
                pstats.parsed_files,
                pstats.total_files,
                success_rate,
                total_nodes_extracted,
                total_edges_extracted,
                pstats.lines_per_second
            );

            // Enhanced parsing statistics
            info!("🌳 TreeSitter AST parsing results:");
            info!(
                "   📊 Semantic nodes extracted: {} (functions, structs, classes, etc.)",
                total_nodes_extracted
            );
            info!(
                "   🔗 Code relationships extracted: {} (calls, imports, dependencies)",
                total_edges_extracted
            );
            info!(
                "   📈 Extraction efficiency: {:.1} nodes/file | {:.1} edges/file",
                total_nodes_extracted as f64 / pstats.parsed_files.max(1) as f64,
                total_edges_extracted as f64 / pstats.parsed_files.max(1) as f64
            );
            info!(
                "   🎯 Sample nodes: {:?}",
                nodes.iter().take(3).map(|n| &n.name).collect::<Vec<_>>()
            );
            self.log_surrealdb_status("post-parse");

            if nodes.is_empty() {
                warn!("No nodes generated from parsing! Check parser implementation.");
                warn!(
                    "Parsing stats: {} files, {} lines processed",
                    pstats.parsed_files, pstats.total_lines
                );
            }

            // STAGE 4: Persist nodes before embedding so SurrealDB reflects progress
            // Parsing, analyzer validation and chunk preparation must succeed before replacing
            // the last usable graph. A failed parse must never erase the previous index.
            self.flush_surreal_writer().await?;
            // Remove the ready marker before the first mutation; interrupted runs cannot skip.
            self.storage
                .delete_project_metadata(&self.project_id)
                .await?;
            // Per-file node ids, used by the per-file reconciliation below. Node ids
            // embed the source line number, so moving a line orphans the old id even
            // when the file still exists.
            let mut fresh_nodes_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for node in &nodes {
                next_catalog.nodes.insert(
                    node.id.to_string(),
                    crate::reconciliation::node_digest(node)?,
                );
                fresh_nodes_by_file
                    .entry(node.location.file_path.clone())
                    .or_default()
                    .push(node.id.to_string());
            }
            let store_nodes_pb = self.create_progress_bar(nodes.len() as u64, "📈 Storing nodes");
            let mut stats = IndexStats {
                files: pstats.parsed_files,
                lines: pstats.total_lines,
                skipped: pstats.cached_files,
                cached_files: pstats.cached_files,
                analyzers_enabled: analyzer_settings.any_enabled(),
                build_context_nodes,
                build_context_edges,
                lsp_nodes_enriched: enrichment_stats.nodes_enriched,
                lsp_edges_resolved: enrichment_stats.edges_resolved,
                files_without_enrichment,
                docs_attached: basic_enrichment_stats.docs_attached,
                export_edges_added: basic_enrichment_stats.export_edges_added,
                reexport_edges_added: basic_enrichment_stats.reexport_edges_added,
                feature_enables_edges_added: basic_enrichment_stats.feature_enables_edges_added,
                uses_edges_derived: basic_enrichment_stats.uses_edges_derived,
                module_nodes_added: module_linker_stats.module_nodes_added,
                module_contains_edges_added: module_linker_stats.contains_edges_added,
                module_import_edges_added: module_linker_stats.module_import_edges_added,
                dataflow_variable_nodes_added: dataflow_stats.variable_nodes_added,
                dataflow_defines_edges_added: dataflow_stats.defines_edges_added,
                dataflow_uses_edges_added: dataflow_stats.uses_edges_added,
                dataflow_flows_to_edges_added: dataflow_stats.flows_to_edges_added,
                dataflow_returns_edges_added: dataflow_stats.returns_edges_added,
                dataflow_mutates_edges_added: dataflow_stats.mutates_edges_added,
                doc_nodes_added: docs_contracts_stats.document_nodes_added,
                document_edges_added: docs_contracts_stats.document_edges_added,
                specification_edges_added: docs_contracts_stats.specification_edges_added,
                package_cycles_detected: architecture_stats.package_cycles_detected,
                boundary_violations_added: architecture_stats.boundary_violations_added,
                ..Default::default()
            };

            for node in nodes.iter() {
                match node.node_type {
                    Some(NodeType::Function) => stats.functions += 1,
                    Some(NodeType::Class) => stats.classes += 1,
                    Some(NodeType::Struct) => stats.structs += 1,
                    Some(NodeType::Trait) => stats.traits += 1,
                    _ => {}
                }
            }
            // Node persistence batch, decoupled from the inference batch size:
            // one SurrealDB transaction per batch means one fsync, and the UPSERT
            // uses array::map + INSERT (no per-row FOR loop), so larger batches are
            // safe and cut the 26k-node persist phase measured at 137s down
            // proportionally.
            let storage_batch = node_db_batch_size(self.config.batch_size);
            // Periodic, file-visible progress. The indicatif spinners above are lost when
            // stdout is not a TTY (nohup, systemd, CI), which made a stalled write look
            // like total silence for hours.
            let total_node_batches = nodes.len().div_ceil(storage_batch.max(1));
            let node_log_every = (total_node_batches / 10).max(1);
            let mut written_nodes = 0usize;
            for (batch_index, chunk) in nodes.chunks(storage_batch).enumerate() {
                let dirty: Vec<_> = chunk
                    .iter()
                    .filter(|node| {
                        previous
                            .as_ref()
                            .and_then(|state| state.nodes.get(&node.id.to_string()))
                            != next_catalog.nodes.get(&node.id.to_string())
                    })
                    .cloned()
                    .collect();
                if !dirty.is_empty() {
                    self.persist_nodes_batch(&dirty).await?;
                }
                written_nodes += chunk.len();
                store_nodes_pb.inc(chunk.len() as u64);
                let batch_number = batch_index + 1;
                if batch_number % node_log_every == 0 || batch_number == total_node_batches {
                    info!(
                        "🗄️ Persisted {}/{} nodes (batch {}/{})",
                        written_nodes,
                        nodes.len(),
                        batch_number,
                        total_node_batches
                    );
                }
            }
            self.flush_surreal_writer().await?;
            self.finish_bar(store_nodes_pb, "📈 Nodes stored")?;
            self.log_surreal_node_count(total_nodes_extracted).await;

            #[cfg(feature = "embeddings")]
            let total_chunks = chunk_plan.chunks.len() as u64;
            #[cfg(not(feature = "embeddings"))]
            let total_chunks = 0u64;
            let embed_pb = self.create_batch_progress_bar(
                total_chunks,
                self.config.batch_size,
                "🧠 Embedding chunks (vector batch)",
            );
            let chunk_store_pb = self.create_batch_progress_bar(
                total_chunks,
                chunk_embedding_db_batch_size(self.config.batch_size),
                "🧩 Persisting chunk embeddings",
            );
            let batch = self.config.batch_size.max(1);
            info!(
                "   ⚙️ Embedding batch row limit: {} | Chunk DB write row limit: {} (inference token/byte and provider limits may split requests)",
                batch,
                chunk_embedding_db_batch_size(batch)
            );
            #[allow(unused_mut)]
            #[cfg(feature = "embeddings")]
            let mut processed: u64 = 0;
            #[cfg(not(feature = "embeddings"))]
            let processed: u64 = 0;

            // Enhanced embedding phase logging
            let provider = &self.global_config.embedding.provider;
            info!("💾 Starting semantic embedding generation:");
            info!(
                "   🤖 Provider: {} ({}-dimensional embeddings)",
                provider, self.vector_dim
            );
            info!(
                "   🗄️ SurrealDB column: {}",
                self.embedding_column.column_name()
            );

            let total_nodes = nodes.len() as u64;
            info!("   📊 Nodes to embed: {} semantic entities", total_nodes);
            info!(
                "   ⚡ Embedding row limit: {} (estimated system memory: {})",
                batch,
                self.estimate_system_memory()
            );
            info!("   🎯 Target: Enable similarity search and AI-powered analysis");
            phase_ms.insert(
                "enqueue_nodes".into(),
                persist_start.elapsed().as_millis() as u64,
            );
            let embedding_start = std::time::Instant::now();
            // Embed chunks and persist chunk embeddings
            #[cfg(feature = "embeddings")]
            if self.policies.embeddings == crate::policy::StagePolicy::Sync {
                use futures::stream::{self, StreamExt};
                use std::sync::atomic::{AtomicU64, Ordering};

                // Inference row limits are independent of the smaller database write batches.
                let chunk_batch_size = batch.max(1);

                // `fingerprint` serialises its input and recursively sorts every object
                // key (canonicalisation) before hashing. Doing that per chunk re-encodes
                // the whole constant configuration 169k+ times for identical bytes, so
                // the fixed tail is encoded and canonicalised once here and each chunk
                // only hashes `[text,<tail>]` — the exact byte stream `fingerprint`
                // would produce. Cache keys and stored vectors stay valid; only the
                // order of evaluation changes. Results keep their original index so
                // iteration order stays deterministic.
                let fingerprint_tail = codegraph_core::artifact_cache::FingerprintTail::new(&(
                    &embedding_config,
                    &policy,
                    &self.embedding_model,
                    self.vector_dim,
                    mutable_model_epoch,
                ))?;
                let computed_hashes: Vec<(usize, String, String)> =
                    self.cpu_pool.install(|| {
                        chunk_plan
                            .chunks
                            .par_iter()
                            .enumerate()
                            .map(|(index, chunk)| {
                                let meta = &chunk_plan.metas[index];
                                let node = &nodes[meta.node_index];
                                let id = ChunkEmbeddingRecord::identity(
                                    &node.id.to_string(),
                                    meta.chunk_index,
                                );
                                let hash = fingerprint_tail.hash(&chunk.text)?;
                                Ok::<_, serde_json::Error>((index, id, hash))
                            })
                            .collect::<std::result::Result<Vec<_>, _>>()
                    })?;

                let mut pending_chunks = Vec::new();
                for (index, id, hash) in computed_hashes {
                    if previous
                        .as_ref()
                        .and_then(|state| state.chunk_hashes.get(&id))
                        != Some(&hash)
                    {
                        pending_chunks.push(index);
                    }
                    next_catalog.chunks.insert(id.clone());
                    next_catalog.chunk_hashes.insert(id, hash);
                }
                let reused_chunks = chunk_plan.chunks.len() - pending_chunks.len();
                let chunk_batches =
                    (0..pending_chunks.len())
                        .step_by(chunk_batch_size)
                        .map(|start| {
                            let indices = &pending_chunks
                                [start..(start + chunk_batch_size).min(pending_chunks.len())];
                            (
                                indices
                                    .iter()
                                    .map(|index| chunk_plan.chunks[*index].text.clone())
                                    .collect::<Vec<_>>(),
                                indices
                                    .iter()
                                    .map(|index| chunk_plan.metas[*index].clone())
                                    .collect::<Vec<_>>(),
                            )
                        });

                let processed_atomic = Arc::new(AtomicU64::new(0));
                let max_concurrent = self.config.max_concurrent.max(1);
                // DB batch size: prefer explicit env override, otherwise match embedding batch size
                let chunk_db_batch = chunk_embedding_db_batch_size(batch);
                let embedder = self.embedder.as_ref().expect("sync provider initialized");

                let mut batch_stream =
                    stream::iter(chunk_batches.map(|(texts, metas)| async move {
                        let embs = embedder.embed_texts_batched(&texts).await;
                        (texts, metas, embs)
                    }))
                    .buffer_unordered(max_concurrent);

                while let Some((texts, metas, embs_result)) = batch_stream.next().await {
                    let embs: Vec<Vec<f32>> = embs_result?;
                    if embs.len() != metas.len() {
                        return Err(anyhow!(
                            "Chunk embedding cardinality mismatch: {} vectors for {} inputs",
                            embs.len(),
                            metas.len()
                        ));
                    }

                    let mut records: Vec<ChunkEmbeddingRecord> = Vec::with_capacity(metas.len());
                    for ((meta, text), emb) in metas.iter().zip(texts.iter()).zip(embs.iter()) {
                        if let Some(node) = nodes.get(meta.node_index) {
                            if emb.len() != self.vector_dim
                                || emb.iter().any(|value| !value.is_finite())
                            {
                                return Err(anyhow!(
                                    "Invalid chunk vector for {}: dimension {}, expected {}",
                                    node.id,
                                    emb.len(),
                                    self.vector_dim
                                ));
                            }
                            records.push(ChunkEmbeddingRecord::new(
                                &node.id.to_string(),
                                meta.chunk_index,
                                text.clone(),
                                emb,
                                &self.embedding_model,
                                self.embedding_column.column_name(),
                                &self.project_id,
                            ));
                        }
                    }

                    next_catalog
                        .chunks
                        .extend(records.iter().map(|record| record.id.clone()));
                    for batch in records.chunks(chunk_db_batch) {
                        self.enqueue_chunk_embeddings(batch.to_vec()).await?;
                    }

                    let done = processed_atomic.fetch_add(metas.len() as u64, Ordering::Relaxed)
                        + metas.len() as u64;
                    embed_pb.set_position(done.min(total_chunks));
                    chunk_store_pb.set_position(done.min(total_chunks));
                }

                processed = processed_atomic.load(Ordering::Relaxed) + reused_chunks as u64;
            }
            #[cfg(not(feature = "embeddings"))]
            let processed: u64 = 0;
            let embedding_rate = if total_chunks > 0 {
                processed as f64 / total_chunks as f64 * 100.0
            } else {
                100.0
            };

            let provider = &self.global_config.embedding.provider;
            let embed_completion_msg = format!(
                "💾 Semantic embeddings complete: {}/{} chunks (✅ {:.1}% success) | 🤖 {} | 📐 {}-dim | 🚀 Row limit: {}",
                processed,
                total_chunks,
                embedding_rate,
                provider,
                self.vector_dim,
                self.config.batch_size
            );
            self.finish_bar(embed_pb, embed_completion_msg.clone())?;
            self.finish_bar(
                chunk_store_pb,
                format!(
                    "🧩 Chunk embeddings queued for persistence: {}/{} chunks",
                    total_chunks, total_chunks
                ),
            )?;

            #[cfg(feature = "embeddings")]
            {
                // Ensure all chunk embeddings are flushed to SurrealDB before continuing
                self.flush_surreal_writer().await?;
                self.log_surreal_chunk_count(total_chunks as usize).await;
            }

            // Node embedding: keep pooled embedding as average of its chunks
            #[cfg(feature = "embeddings")]
            {
                // For now, skip storing node-level embedding when chunking is enabled.
                // Retrieval should use chunk embeddings directly.
            }

            stats.embeddings = processed as usize;

            // Enhanced embedding completion statistics
            info!("💾 Semantic embedding generation results:");
            info!(
                "   🎯 Vector search enabled: {} nodes embedded for similarity matching",
                processed
            );
            info!("   📐 Embedding dimensions: {}", self.vector_dim);
            info!(
                "   🤖 Provider performance: {} with batch optimization",
                provider
            );
            info!(
                "   🔍 Capabilities unlocked: Vector search, semantic analysis, AI-powered tools"
            );

            // CRITICAL FIX: Preserve working ONNX embedding session for AI semantic matching
            // Original reset caused fresh embedder creation to fail with ONNX resource conflicts,
            // falling back to random hash embeddings (0% AI effectiveness).
            // Keeping the working ONNX session ensures real embeddings for AI semantic matching.
            // Tradeoff: Slightly more memory usage during post-processing (acceptable on M4 Max).
            #[cfg(feature = "embeddings")]
            {
                // self.embedder = codegraph_vector::EmbeddingGenerator::default();
                tracing::info!(
                    "🔧 Preserving working ONNX embedder session for AI semantic matching"
                );
            }

            tracing::info!(
                target: "codegraph_mcp::indexer",
                "Vector indexing handled by SurrealDB; local FAISS generation removed"
            );

            // Resolve exact/contextual targets once before any semantic inference.
            phase_ms.insert(
                "embeddings_and_writes".into(),
                embedding_start.elapsed().as_millis() as u64,
            );
            let resolution_start = std::time::Instant::now();
            let catalog_build_start = std::time::Instant::now();
            // A partial re-index only sees the scanned files. Load the rest of the
            // project's symbols so edges emitted by the scanned files still resolve.
            let catalog = if reconcile_scope.is_some() {
                let excluded: Vec<String> = fresh_nodes_by_file.keys().cloned().collect();
                let rows = self
                    .storage
                    .symbols_excluding_files(&self.project_id, &excluded)
                    .await?;
                let external: Vec<crate::resolution::ExternalSymbol> = rows
                    .into_iter()
                    .filter_map(|row| {
                        let id = codegraph_core::NodeId::parse_str(&row.id).ok()?;
                        Some(crate::resolution::ExternalSymbol {
                            id,
                            file: row.file_path,
                            name: row.name,
                            kind: row.node_type,
                            qualified: row.qualified_name,
                        })
                    })
                    .collect();
                crate::resolution::SymbolCatalog::new_with_external(&nodes, &external)
            } else {
                crate::resolution::SymbolCatalog::new(&nodes)
            };
            phase_ms.insert(
                "resolution_catalog_build".into(),
                catalog_build_start.elapsed().as_millis() as u64,
            );
            let unique_aliases_start = std::time::Instant::now();
            let symbol_map = catalog.unique_aliases();
            phase_ms.insert(
                "resolution_unique_aliases".into(),
                unique_aliases_start.elapsed().as_millis() as u64,
            );
            let mut targets = Vec::with_capacity(edges.len());
            let mut resolution_provenance = Vec::with_capacity(edges.len());
            // Resolution over 96k edges was a serial loop measured at ~200s on
            // one core. `SymbolCatalog`'s lookup methods are `&self`, so the loop
            // parallelises with rayon (order preserved by collect); the two
            // memoisation maps become DashMaps, and the counters are derived from
            // provenance afterwards so no atomics are needed.
            let exact_loop_start = std::time::Instant::now();
            {
                let normalized: dashmap::DashMap<String, Vec<String>> = dashmap::DashMap::new();
                let lexical_cache: dashmap::DashMap<(NodeId, String), Option<(NodeId, f64)>> =
                    dashmap::DashMap::new();
                let results: Vec<(Option<NodeId>, (&'static str, f64))> = edges
                    .par_iter()
                    .map(|edge| {
                        let mut target = catalog.exact(edge, &[]);
                        let mut provenance = (
                            if target.is_some_and(|id| {
                                edge.metadata.get("target_node_id") == Some(&id.to_string())
                            }) {
                                "definition"
                            } else {
                                "exact"
                            },
                            1.0f64,
                        );
                        if target.is_none() {
                            let variants = normalized.entry(edge.to.clone()).or_insert_with(|| {
                                let mut variants = Self::normalize_symbol_target(&edge.to);
                                if edge.to.starts_with("_R")
                                    || edge.to.starts_with("_ZN")
                                    || edge.to.contains('<')
                                {
                                    variants.extend(Self::normalize_rust_symbol(&edge.to));
                                    variants.sort();
                                    variants.dedup();
                                }
                                variants
                            });
                            target = catalog.exact(edge, &variants);
                            provenance.0 = "normalized";
                        }
                        if target.is_none() && !catalog.ambiguous(&edge.to) {
                            let lexical = *lexical_cache
                                .entry((edge.from, edge.to.clone()))
                                .or_insert_with(|| catalog.fuzzy(edge));
                            if let Some((id, score)) = lexical {
                                target = Some(id);
                                provenance = ("lexical", score);
                            }
                        }
                        (target, provenance)
                    })
                    .collect();
                for (target, provenance) in results {
                    targets.push(target);
                    resolution_provenance.push(provenance);
                }
            }
            phase_ms.insert(
                "resolution_exact_loop".into(),
                exact_loop_start.elapsed().as_millis() as u64,
            );
            info!(
                "⏱️ Resolution exact loop: {} ms on {} workers",
                exact_loop_start.elapsed().as_millis(),
                rayon::current_num_threads()
            );
            // Fuzzy fallback diagnostics: calls and summed CPU time across rayon
            // workers. `resolution_fuzzy_ms` is a sum, not wall-clock time.
            let fuzzy = catalog.fuzzy_stats();
            phase_ms.insert("resolution_fuzzy_calls".into(), fuzzy.calls);
            phase_ms.insert("resolution_fuzzy_ms".into(), fuzzy.nanos / 1_000_000);
            phase_ms.insert("resolution_fuzzy_candidates".into(), fuzzy.candidates);
            phase_ms.insert("resolution_fuzzy_len_pruned".into(), fuzzy.len_pruned);
            phase_ms.insert(
                "resolution_fuzzy_levenshtein_calls".into(),
                fuzzy.levenshtein_calls,
            );
            phase_ms.insert("resolution_fuzzy_above_cutoff".into(), fuzzy.above_cutoff);
            phase_ms.insert(
                "resolution_fuzzy_trigram_ms".into(),
                fuzzy.trigram_nanos / 1_000_000,
            );
            phase_ms.insert(
                "resolution_fuzzy_counts_ms".into(),
                fuzzy.counts_nanos / 1_000_000,
            );
            phase_ms.insert(
                "resolution_fuzzy_lev_ms".into(),
                fuzzy.lev_nanos / 1_000_000,
            );
            info!(
                "🔤 Fuzzy resolution: {} calls, {} candidates ({} length-pruned), {} \
                 levenshtein calls, {} above cutoff, {} ms summed across workers \
                 [trigrams {} ms, counts {} ms, loop {} ms] (limit {})",
                fuzzy.calls,
                fuzzy.candidates,
                fuzzy.len_pruned,
                fuzzy.levenshtein_calls,
                fuzzy.above_cutoff,
                fuzzy.nanos / 1_000_000,
                fuzzy.trigram_nanos / 1_000_000,
                fuzzy.counts_nanos / 1_000_000,
                fuzzy.lev_nanos / 1_000_000,
                fuzzy.limit
            );
            // Diagnostic only: the rayon pool size actually used by the loop above.
            // Recorded as a raw number (not a duration) so it is never summed.
            phase_ms.insert(
                "resolution_threads".into(),
                rayon::current_num_threads() as u64,
            );
            let resolution_tail_start = std::time::Instant::now();
            phase_ms.insert(
                "resolution_tail".into(),
                resolution_tail_start.elapsed().as_millis() as u64,
            );
            phase_ms.insert(
                "resolution_exact_lexical".into(),
                resolution_start.elapsed().as_millis() as u64,
            );
            #[cfg(feature = "ai-enhanced")]
            {
                if self.policies.semantic == crate::policy::StagePolicy::Sync {
                    let candidates_start = std::time::Instant::now();
                    let limit = std::env::var("CODEGRAPH_SEMANTIC_CANDIDATES")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    let mut candidates_by_target = HashMap::new();
                    let mut candidate_symbols = HashMap::new();
                    for (edge, target) in edges.iter().zip(&targets) {
                        if target.is_some()
                            || catalog.ambiguous(&edge.to)
                            || Self::semantic_stop_symbol(&edge.to)
                        {
                            continue;
                        }
                        let candidates = candidates_by_target
                            .entry(edge.to.clone())
                            .or_insert_with(|| catalog.semantic_candidates(&edge.to, limit));
                        for alias in candidates {
                            candidate_symbols.insert(alias.clone(), symbol_map[alias]);
                        }
                    }
                    let unresolved: Vec<_> = candidates_by_target
                        .iter()
                        .filter(|(_, candidates)| !candidates.is_empty())
                        .map(|(target, _)| target.clone())
                        .collect();
                    phase_ms.insert(
                        "resolution_semantic_candidates".into(),
                        candidates_start.elapsed().as_millis() as u64,
                    );
                    if !unresolved.is_empty() {
                        info!(
                            "🧠 Relationship symbol embeddings starting: {} candidate names, {} unresolved names",
                            candidate_symbols.len(),
                            unresolved.len()
                        );
                        let symbol_embedding_start = std::time::Instant::now();
                        let known = self
                            .embed_symbol_texts(
                                candidate_symbols.keys().cloned().collect(),
                                &candidate_symbols,
                            )
                            .await?;
                        let unknown = self.embed_symbol_texts(unresolved, &HashMap::new()).await?;
                        info!(
                            "🧠 Relationship symbol embeddings complete in {:?}",
                            symbol_embedding_start.elapsed()
                        );
                        phase_ms.insert(
                            "resolution_symbol_embeddings".into(),
                            symbol_embedding_start.elapsed().as_millis() as u64,
                        );
                        info!(
                            "🔎 Semantic scoring starting: {} unresolved names, {} candidate vectors, {} workers",
                            unknown.len(),
                            known.len(),
                            self.cpu_pool.current_num_threads()
                        );
                        let scoring_start = std::time::Instant::now();
                        let pool = Arc::clone(&self.cpu_pool);
                        let scoring = tokio::task::spawn_blocking(move || {
                            pool.install(|| {
                                crate::semantic_scoring::resolve_targets(
                                    &candidates_by_target,
                                    &candidate_symbols,
                                    &known,
                                    &unknown,
                                )
                            })
                        })
                        .await
                        .context("Semantic scoring task failed")?;
                        let scoring_elapsed = scoring_start.elapsed();
                        phase_ms.insert(
                            "resolution_semantic_scoring".into(),
                            scoring_elapsed.as_millis() as u64,
                        );
                        info!(
                            "🔎 Semantic scoring complete: {} matches, {} comparisons, {} cached vector norms in {:?}",
                            scoring.targets.len(),
                            scoring.comparisons,
                            scoring.vector_norms,
                            scoring_elapsed
                        );
                        let semantic_targets = scoring.targets;
                        for (index, (edge, target)) in edges.iter().zip(&mut targets).enumerate() {
                            if target.is_none()
                                && let Some((id, score)) = semantic_targets.get(&edge.to).copied()
                            {
                                *target = Some(id);
                                resolution_provenance[index] = ("semantic", score as f64);
                            }
                        }
                        let symbol_flush_start = std::time::Instant::now();
                        self.flush_surreal_writer().await?;
                        phase_ms.insert(
                            "resolution_symbol_writes_flush".into(),
                            symbol_flush_start.elapsed().as_millis() as u64,
                        );
                    }
                }
            }
            let edge_persistence_start = std::time::Instant::now();
            let resolved_count = targets.iter().filter(|target| target.is_some()).count();
            let (exact_count, lexical_count, semantic_count) =
                classify_resolution_counts(&targets, &resolution_provenance);
            let mut resolved_edges = Vec::with_capacity(resolved_count);
            for ((edge, target), (method, score)) in
                edges.iter().zip(targets).zip(resolution_provenance)
            {
                if let Some(target) = target {
                    let mut record = CodeEdge::new(edge.from, target, edge.edge_type.clone());
                    record.metadata = edge.metadata.clone();
                    record
                        .metadata
                        .insert("resolution_method".into(), method.into());
                    record
                        .metadata
                        .insert("resolution_score".into(), format!("{score:.6}"));
                    if let Some(span) = &edge.span {
                        record.metadata.insert(
                            "source_span".into(),
                            format!("{}:{}", span.start_byte, span.end_byte),
                        );
                    }
                    record.set_deterministic_id(&self.project_id);
                    resolved_edges.push(record);
                }
            }
            resolved_edges.sort_by_key(|edge| edge.id);
            resolved_edges.dedup_by_key(|edge| edge.id);
            let stored_edges = resolved_edges.len();
            next_catalog
                .edges
                .extend(resolved_edges.iter().map(|edge| edge.id.to_string()));
            resolved_edges.retain(|edge| {
                previous
                    .as_ref()
                    .is_none_or(|state| !state.edges.contains(&edge.id.to_string()))
            });
            self.enqueue_edges(resolved_edges).await?;
            self.flush_surreal_writer().await?;
            phase_ms.insert(
                "resolution_edge_preparation_and_writes".into(),
                edge_persistence_start.elapsed().as_millis() as u64,
            );
            let resolution_rate = if edges.is_empty() {
                100.0
            } else {
                100.0 * resolved_count as f64 / edges.len() as f64
            };
            info!(
                "Resolved {} of {} relationships (exact/contextual={}, lexical={}, semantic={}) in {:?}",
                resolved_count,
                edges.len(),
                exact_count,
                lexical_count,
                semantic_count,
                resolution_start.elapsed()
            );

            phase_ms.insert(
                "resolve_and_enqueue_edges".into(),
                resolution_start.elapsed().as_millis() as u64,
            );
            let finish_start = std::time::Instant::now();
            // ELIMINATED: No separate edge processing phase needed - edges extracted during parsing!
            self.log_surreal_edge_count(stored_edges).await;

            // Task 3.3: Update file metadata for incremental indexing
            info!("💾 Updating file metadata for change tracking");
            let file_paths_only: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
            next_catalog.file_metadata_hashes = self
                .persist_file_metadata(
                    &file_paths_only,
                    &nodes,
                    &edges,
                    &source_snapshots,
                    previous.as_ref().map(|state| &state.file_metadata_hashes),
                )
                .await?;
            self.flush_surreal_writer().await?;
            self.verify_file_metadata_count(file_paths_only.len())
                .await?;

            // COMPREHENSIVE INDEXING COMPLETION SUMMARY
            let avg_nodes_per_file = if stats.files > 0 {
                total_nodes_extracted as f64 / stats.files as f64
            } else {
                0.0
            };
            let avg_edges_per_file = if stats.files > 0 {
                total_edges_extracted as f64 / stats.files as f64
            } else {
                0.0
            };
            let avg_embeddings_per_node = if total_nodes_extracted > 0 {
                stats.embeddings as f64 / total_nodes_extracted as f64
            } else {
                0.0
            };

            let total_elapsed = start.elapsed().as_secs_f64();

            info!("🎉 INDEXING COMPLETE");
            info!(
                "📂 Files {} ({} skipped) | Lines {} | Time {:.1}s",
                pstats.parsed_files, stats.skipped, stats.lines, total_elapsed
            );
            info!(
                "🌳 Graph coverage: nodes {} | edges {} | nodes/file {:.1} | edges/file {:.1} | resolved {:.1}%",
                total_nodes_extracted,
                stored_edges,
                avg_nodes_per_file,
                avg_edges_per_file,
                resolution_rate
            );
            info!(
                "🧠 Embeddings: chunks {} | dim {} | provider {} | per-node {:.1}",
                stats.embeddings, self.vector_dim, provider, avg_embeddings_per_node
            );
            info!(
                "⚡ Throughput: {:.1} files/s | {:.1} nodes/s | {:.1} edges/s",
                pstats.parsed_files as f64 / total_elapsed.max(1e-3),
                total_nodes_extracted as f64 / total_elapsed.max(1e-3),
                stored_edges as f64 / total_elapsed.max(1e-3)
            );

            // Populate extended stats for CLI reporting
            stats.nodes = total_nodes_extracted;
            stats.edges = stored_edges;
            stats.chunks = stats.embeddings; // chunks == embeddings in current impl
            stats.embedding_dimension = self.vector_dim;
            stats.embedding_provider = provider.clone();
            stats.resolved_edges = resolved_count;
            stats.unresolved_edges = total_edges_extracted.saturating_sub(resolved_count);
            stats.resolution_rate = resolution_rate;

            self.flush_surreal_writer().await?;
            // Per-file reconciliation: the project-wide variant issues `NOT IN`
            // deletes over every node/edge/chunk id and cannot use an index, which
            // measured at 10+ minutes per run on this project.
            // Point already-stored inbound edges at the new node ids before reconciliation
            // deletes the old rows. Only symbols that still exist in the same file with the
            // same name and kind move; genuinely removed symbols keep their edges until
            // reconciliation drops them.
            if reconcile_scope.is_some() && !fresh_nodes_by_file.is_empty() {
                let mut by_identity = std::collections::HashMap::new();
                for node in &nodes {
                    by_identity.insert(Self::node_identity(node), node.id);
                }
                let new_ids: std::collections::HashSet<codegraph_core::NodeId> =
                    nodes.iter().map(|node| node.id).collect();
                self.redirect_inbound_edges(&fresh_nodes_by_file, &new_ids, &by_identity)
                    .await?;
            }
            self.storage
                .reconcile(
                    &self.project_id,
                    &fresh_nodes_by_file,
                    &next_catalog.chunks,
                    &next_catalog.edges,
                    reconcile_scope.as_ref(),
                )
                .await?;
            source_snapshots.validate_current().await?;
            let current_files = codegraph_parser::file_collect::collect_source_files_with_config(
                path,
                &file_config,
            )?;
            // Language servers (jdtls/KLS) generate IDE/Gradle files while importing the
            // project, so the second walk can legitimately differ from the first. The graph
            // already written is a consistent snapshot; mark it incomplete instead of
            // failing the whole run and dropping the stats file.
            let inputs_changed = current_files != all_files
                || crate::reconciliation::support_fingerprints(&self.project_root)? != support
                || crate::analyzers::build_context::external_input_fingerprints(
                    &self.project_root,
                )? != external_support;
            if inputs_changed {
                warn!(
                    "Project inputs changed during indexing ({} files now, {} scanned); \
                     graph is a consistent snapshot, rerun `codegraph index` to reconcile",
                    current_files.len(),
                    all_files.len()
                );
            }
            let tables: Vec<String> = Self::vector_tables(self.policies)
                .into_iter()
                .map(str::to_string)
                .collect();
            stats.vector_index_ready = self
                .storage
                .ensure_vector_indexes(self.vector_dim, &tables)
                .await?;
            stats.index_ms = start.elapsed().as_millis() as u64;
            phase_ms.insert(
                "durable_reconcile_and_indexes".into(),
                finish_start.elapsed().as_millis() as u64,
            );
            stats.phase_ms = phase_ms;
            let writer_after = self
                .surreal_writer
                .as_ref()
                .map(|writer| writer.metrics())
                .unwrap_or_default();
            stats.writer_jobs_acked = writer_after.0.saturating_sub(writer_before.0);
            stats.writer_rows_acked = writer_after.1.saturating_sub(writer_before.1);
            stats.writer_payload_bytes_acked = writer_after.2.saturating_sub(writer_before.2);
            stats.source_read_operations = source_snapshots.read_operations;
            stats.source_read_bytes = source_snapshots.read_bytes;
            stats.source_spilled_bytes = source_snapshots.spilled_bytes;
            stats.startup_ms = self.startup_ms;
            stats.graph_complete = true;
            stats.embedding_status = self.policies.embeddings.status().into();
            stats.semantic_status = self.policies.semantic.status().into();
            stats.complete = !self.policies.pending() && !inputs_changed;
            if self.policies.pending() {
                crate::policy::pending_cache(&self.project_root).put(
                    &self.project_id,
                    &crate::policy::DeferredJob {
                        input_fingerprint: input_fingerprint.clone(),
                        policies: self.policies,
                    },
                )?;
            }
            #[cfg(feature = "embeddings")]
            {
                let after = self
                    .embedder
                    .as_ref()
                    .map(|embedder| embedder.inference_stats())
                    .unwrap_or_default();
                stats.embedding_cache_hits = after.0.saturating_sub(inference_before.0);
                stats.inference_texts = after.1.saturating_sub(inference_before.1);
                stats.inference_tokens = after.2.saturating_sub(inference_before.2);
            }
            self.persist_project_metadata(&stats, stats.nodes, stats.edges, &input_fingerprint)
                .await?;
            self.flush_surreal_writer().await?;
            if !self.policies.pending() {
                crate::policy::pending_cache(&self.project_root).remove(&self.project_id)?;
            }
            next_catalog.stats = stats.clone();
            if let Err(error) = state_cache.put(&self.project_id, &next_catalog) {
                warn!("Project catalog cache unavailable; next run will reconcile fully: {error}");
            }
            Ok(stats)
        })
    }

    /// REVOLUTIONARY: Parse files with unified node+edge extraction for maximum speed
    async fn parse_files_with_unified_extraction(
        &self,
        files: Vec<(PathBuf, u64)>,
        total_files: u64,
    ) -> Result<(
        Vec<CodeNode>,
        Vec<codegraph_core::EdgeRelationship>,
        codegraph_parser::ParsingStatistics,
    )> {
        shared_unified_parse(&self.parser, files, total_files).await
    }

    /// Estimate available system memory for informative logging
    fn estimate_system_memory(&self) -> String {
        #[cfg(target_os = "macos")]
        {
            if let Ok(output) = std::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
                && let Ok(memsize_str) = String::from_utf8(output.stdout)
                && let Ok(memsize) = memsize_str.trim().parse::<u64>()
            {
                let gb = memsize / 1024 / 1024 / 1024;
                return format!("{}GB", gb);
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
                if let Some(line) = contents.lines().find(|line| line.starts_with("MemTotal:")) {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<u64>() {
                            let gb = kb / 1024 / 1024;
                            return format!("{}GB", gb);
                        }
                    }
                }
            }
        }

        "Unknown".to_string()
    }

    fn semantic_stop_symbol(symbol: &str) -> bool {
        matches!(
            symbol.to_lowercase().as_str(),
            "into"
                | "unwrap"
                | "ok"
                | "err"
                | "some"
                | "none"
                | "new"
                | "from"
                | "default"
                | "clone"
                | "drop"
                | "len"
                | "push"
                | "pop"
                | "to_string"
                | "println"
                | "debug"
                | "info"
                | "warn"
                | "error"
                | "fmt"
                | "str"
        )
    }

    #[cfg(feature = "ai-enhanced")]
    async fn embed_symbol_texts(
        &self,
        mut texts: Vec<String>,
        node_ids: &HashMap<String, NodeId>,
    ) -> Result<HashMap<String, Vec<f32>>> {
        texts.sort();
        texts.dedup();
        // 分段计时开关：未设置 CODEGRAPH_TRACE_SYMBOL_EMBED 时只多几个原子变量与一次 env 查询，
        // 不产生日志、不改变任何行为。
        let trace = std::env::var_os("CODEGRAPH_TRACE_SYMBOL_EMBED").is_some();
        let embed_ns = std::sync::atomic::AtomicU64::new(0);
        let record_ns = std::sync::atomic::AtomicU64::new(0);
        let persist_ns = std::sync::atomic::AtomicU64::new(0);
        let batch_count = std::sync::atomic::AtomicU64::new(0);
        let wall_started = std::time::Instant::now();
        let (batch_size, concurrency) = self.symbol_embedding_batch_settings();
        let mut results = HashMap::new();
        let texts = &texts;
        // `async move` 块会移动捕获值，这里显式按引用捕获计时器，保持下方 `load` 仍用原变量。
        let embed_ns_ref = &embed_ns;
        let batch_count_ref = &batch_count;
        let mut batches = stream::iter((0..texts.len()).step_by(batch_size).map(
            |start| async move {
                let batch = &texts[start..(start + batch_size).min(texts.len())];
                let embed_started = std::time::Instant::now();
                let vectors = self
                    .embedder
                    .as_ref()
                    .expect("sync provider initialized")
                    .embed_texts_batched(batch)
                    .await?;
                if trace {
                    embed_ns_ref.fetch_add(
                        embed_started.elapsed().as_nanos() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    batch_count_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if vectors.len() != batch.len()
                    || vectors.iter().any(|vector| {
                        vector.len() != self.vector_dim
                            || vector.iter().any(|value| !value.is_finite())
                    })
                {
                    return Err(anyhow!(
                        "Symbol provider returned invalid cardinality, dimensions or values"
                    ));
                }
                Ok::<_, anyhow::Error>((batch, vectors))
            },
        ))
        .buffer_unordered(concurrency);
        while let Some(result) = batches.next().await {
            let (batch, vectors) = result?;
            let record_started = std::time::Instant::now();
            let mut records = Vec::with_capacity(batch.len());
            for (text, vector) in batch.iter().zip(vectors) {
                records.push(self.build_symbol_embedding_record(
                    text,
                    node_ids.get(text).copied(),
                    None,
                    &vector,
                ));
                results.insert(text.clone(), vector);
            }
            if trace {
                record_ns.fetch_add(
                    record_started.elapsed().as_nanos() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            let persist_started = std::time::Instant::now();
            self.persist_symbol_embedding_records(records).await?;
            if trace {
                persist_ns.fetch_add(
                    persist_started.elapsed().as_nanos() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
        if trace {
            let ms = |ns: u64| ns as f64 / 1_000_000.0;
            info!(
                "SYMBOL_EMBED_TRACE texts={} batches={} embed={:.0}ms record={:.0}ms persist={:.0}ms wall={:.0}ms",
                texts.len(),
                batch_count.load(std::sync::atomic::Ordering::Relaxed),
                ms(embed_ns.load(std::sync::atomic::Ordering::Relaxed)),
                ms(record_ns.load(std::sync::atomic::Ordering::Relaxed)),
                ms(persist_ns.load(std::sync::atomic::Ordering::Relaxed)),
                wall_started.elapsed().as_millis(),
            );
        }
        Ok(results)
    }

    async fn index_file(
        path: PathBuf,
        parse_pb: ProgressBar,
        embed_pb: ProgressBar,
    ) -> Result<FileStats> {
        debug!("Indexing file: {:?}", path);
        let mut stats = FileStats::default();

        // Read file content
        let content = tokio_fs::read_to_string(&path).await?;
        stats.lines = content.lines().count();

        // Very rough heuristics for functions/classes counts per common languages
        let ext = path.extension().and_then(OsStr::to_str).unwrap_or("");
        let (fn_regex, class_regex) = match ext {
            "rs" => (Some(Regex::new(r"\bfn\s+\w+").unwrap()), None),
            "py" => (
                Some(Regex::new(r"\bdef\s+\w+\s*\(").unwrap()),
                Some(Regex::new(r"\bclass\s+\w+\s*:").unwrap()),
            ),
            "ts" | "js" => (
                Some(Regex::new(r"\bfunction\s+\w+|\b\w+\s*=\s*\(.*\)\s*=>").unwrap()),
                Some(Regex::new(r"\bclass\s+\w+").unwrap()),
            ),
            "go" => (Some(Regex::new(r"\bfunc\s+\w+\s*\(").unwrap()), None),
            "java" => (
                Some(Regex::new(r"\b\w+\s+\w+\s*\(.*\)\s*\{").unwrap()),
                Some(Regex::new(r"\bclass\s+\w+").unwrap()),
            ),
            "cpp" | "cc" | "cxx" | "hpp" | "h" | "c" => (
                Some(Regex::new(r"\b\w+\s+\w+\s*\(.*\)\s*\{").unwrap()),
                None,
            ),
            _ => (None, None),
        };

        parse_pb.set_message(format!("Parsing {}", path.display()));
        parse_pb.inc(1);

        if let Some(re) = fn_regex {
            stats.functions = re.find_iter(&content).count();
        }
        if let Some(re) = class_regex {
            stats.classes = re.find_iter(&content).count();
        }

        // Pretend to generate embeddings by counting tokens roughly
        embed_pb.set_message(format!("Embedding {}", path.display()));
        stats.embeddings = content.split_whitespace().count() / 100; // 1 per ~100 tokens
        embed_pb.inc(1);

        Ok(stats)
    }

    async fn collect_files(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();

        let walker = if self.config.recursive {
            WalkDir::new(path)
        } else {
            WalkDir::new(path).max_depth(1)
        };

        for entry in walker {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            if self.should_index(path) {
                files.push(path.to_path_buf());
            }
        }

        Ok(files)
    }

    fn should_index(&self, path: &Path) -> bool {
        let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
        let path_str = path.to_string_lossy();

        // Exclude patterns (simple substring match)
        for pat in &self.config.exclude_patterns {
            if path_str.contains(pat) || file_name.contains(pat) {
                return false;
            }
        }

        // Include patterns (if provided, must match at least one)
        if !self.config.include_patterns.is_empty()
            && !self
                .config
                .include_patterns
                .iter()
                .any(|p| path_str.contains(p) || file_name.contains(p))
        {
            return false;
        }

        // Language filtering by extension
        if !self.config.languages.is_empty() {
            let ext = path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or("")
                .to_lowercase();
            let lang_matches = |langs: &Vec<String>, e: &str| -> bool {
                let lang = e;
                langs.iter().any(|l| match l.as_str() {
                    "rust" | "rs" => matches!(lang, "rs"),
                    "python" | "py" => matches!(lang, "py"),
                    "js" | "javascript" | "jsx" => matches!(lang, "js" | "jsx"),
                    "ts" | "typescript" | "tsx" => matches!(lang, "ts" | "tsx"),
                    "go" => matches!(lang, "go"),
                    "java" => matches!(lang, "java"),
                    "kotlin" | "kt" => matches!(lang, "kt"),
                    "cpp" | "c++" | "cc" | "cxx" | "hpp" | "h" | "c" => {
                        matches!(lang, "cpp" | "cc" | "cxx" | "hpp" | "h" | "c")
                    }
                    _ => false,
                })
            };
            if !lang_matches(&self.config.languages, &ext) {
                return false;
            }
        }

        // Default excludes
        for dir in [
            ".git",
            "node_modules",
            "target",
            ".codegraph",
            "dist",
            "build",
        ] {
            if path_str.contains(dir) {
                return false;
            }
        }

        true
    }

    async fn is_indexed(&self, _path: &Path) -> Result<bool> {
        Ok(self.storage.has_project_metadata(&self.project_id).await?)
    }

    async fn has_file_metadata(&self) -> Result<bool> {
        Ok(!self
            .storage
            .file_metadata(&self.project_id)
            .await?
            .is_empty())
    }

    /// Calculate SHA-256 hash of file content
    fn calculate_file_hash(file_path: &Path) -> Result<String> {
        // Resolve symlinks to actual file
        let canonical_path = fs::canonicalize(file_path)
            .with_context(|| format!("Failed to resolve path: {:?}", file_path))?;

        let mut file = fs::File::open(&canonical_path)
            .with_context(|| format!("Failed to open file: {:?}", canonical_path))?;

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];

        loop {
            let bytes_read = file
                .read(&mut buffer)
                .with_context(|| format!("Failed to read file: {:?}", canonical_path))?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        Ok(hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>())
    }

    /// Detect changes between current filesystem and stored file metadata
    async fn detect_file_changes(&self, current_files: &[PathBuf]) -> Result<Vec<FileChange>> {
        let files: Vec<_> = current_files.iter().map(|p| (p.clone(), 0)).collect();
        let snapshots = codegraph_parser::SourceSnapshots::capture(
            &files,
            crate::estimation::source_memory_budget(),
            self.parser.concurrency(),
        )
        .await?;
        self.detect_snapshot_changes(current_files, &snapshots)
            .await
    }

    async fn detect_snapshot_changes(
        &self,
        current_files: &[PathBuf],
        snapshots: &codegraph_parser::SourceSnapshots,
    ) -> Result<Vec<FileChange>> {
        let stored_metadata = self.storage.file_metadata(&self.project_id).await?;

        // Create lookup map of stored files
        let stored_map: HashMap<String, FileMetadataRecord> = stored_metadata
            .into_iter()
            .map(|record| (record.file_path.clone(), record))
            .collect();

        let mut changes = Vec::new();
        let mut current_file_set = HashSet::new();

        // Check current files for additions or modifications
        for file_path in current_files {
            let file_path_str = file_path.to_string_lossy().to_string();
            current_file_set.insert(file_path_str.clone());

            let current_hash = snapshots
                .get(file_path)
                .ok_or_else(|| anyhow!("Missing source snapshot: {}", file_path.display()))?
                .content_hash
                .clone();

            match stored_map.get(&file_path_str) {
                Some(stored) => {
                    if stored.content_hash != current_hash {
                        changes.push(FileChange {
                            file_path: file_path_str,
                            change_type: FileChangeType::Modified,
                            current_hash: Some(current_hash),
                            previous_hash: Some(stored.content_hash.clone()),
                        });
                    } else {
                        changes.push(FileChange {
                            file_path: file_path_str,
                            change_type: FileChangeType::Unchanged,
                            current_hash: Some(current_hash),
                            previous_hash: Some(stored.content_hash.clone()),
                        });
                    }
                }
                None => {
                    changes.push(FileChange {
                        file_path: file_path_str,
                        change_type: FileChangeType::Added,
                        current_hash: Some(current_hash),
                        previous_hash: None,
                    });
                }
            }
        }

        // Check for deleted files
        for (stored_path, stored_record) in stored_map {
            if !current_file_set.contains(&stored_path) {
                changes.push(FileChange {
                    file_path: stored_path,
                    change_type: FileChangeType::Deleted,
                    current_hash: None,
                    previous_hash: Some(stored_record.content_hash),
                });
            }
        }

        Ok(changes)
    }

    /// Delete nodes and edges for specific files
    async fn delete_data_for_files(&self, file_paths: &[String]) -> Result<()> {
        if file_paths.is_empty() {
            return Ok(());
        }

        // Use writer to delete nodes, edges, and file metadata as one ordered operation
        self.surreal_writer_handle()?
            .enqueue_delete_nodes_by_file(file_paths.to_vec(), &self.project_id)
            .await
    }

    /// Persist file metadata for incremental indexing change tracking
    async fn persist_file_metadata(
        &self,
        files: &[PathBuf],
        nodes: &[CodeNode],
        edges: &[EdgeRelationship],
        snapshots: &codegraph_parser::SourceSnapshots,
        previous: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mut file_metadata_records = Vec::new();
        let mut hashes = std::collections::BTreeMap::new();

        // Create progress bar for file metadata
        let metadata_pb = self.progress.add(ProgressBar::new(files.len() as u64));
        metadata_pb.set_style(
            ProgressStyle::with_template("{spinner:.blue} {msg} [{bar:40.cyan/blue}] {pos}/{len}")
                .unwrap()
                .progress_chars("█▓▒░  "),
        );
        metadata_pb.set_message("💾 Processing file metadata");

        // Build HashMap for O(1) lookups instead of O(N) iterations
        let mut file_stats: HashMap<String, (i64, i64)> = HashMap::new();

        // Count nodes per file - O(nodes)
        for node in nodes {
            let entry = file_stats
                .entry(node.location.file_path.clone())
                .or_insert((0, 0));
            entry.0 += 1;
        }

        // Build node-to-file mapping for edge counting - O(nodes)
        let node_file_map: HashMap<NodeId, String> = nodes
            .iter()
            .map(|n| (n.id, n.location.file_path.clone()))
            .collect();

        // Count edges per file - O(edges)
        for edge in edges {
            if let Some(file_path) = node_file_map.get(&edge.from) {
                let entry = file_stats.entry(file_path.clone()).or_insert((0, 0));
                entry.1 += 1;
            }
        }

        for file_path in files {
            let file_path_str = file_path.to_string_lossy().to_string();

            let snapshot = snapshots
                .get(file_path)
                .ok_or_else(|| anyhow!("Missing source snapshot: {}", file_path.display()))?;
            let content_hash = snapshot.content_hash.clone();
            let file_size = snapshot.size as i64;
            let modified_at = chrono::DateTime::<chrono::Utc>::from(snapshot.modified_at);

            // Get counts from HashMap - O(1) lookup
            let (node_count, edge_count) =
                file_stats.get(&file_path_str).copied().unwrap_or((0, 0));

            let hash = codegraph_core::artifact_cache::fingerprint(&(
                &file_path_str,
                &content_hash,
                file_size,
                modified_at,
                node_count,
                edge_count,
            ))?;
            let unchanged = previous.and_then(|hashes| hashes.get(&file_path_str)) == Some(&hash);
            hashes.insert(file_path_str.clone(), hash);
            metadata_pb.inc(1);
            if unchanged {
                continue;
            }
            file_metadata_records.push(FileMetadataRecord {
                file_path: file_path_str,
                project_id: self.project_id.clone(),
                content_hash,
                modified_at,
                file_size,
                last_indexed_at: chrono::Utc::now(),
                node_count,
                edge_count,
                language: None, // Will be inferred from file extension if needed
                parse_errors: None,
            });
        }

        // Batch upsert file metadata
        let updated = file_metadata_records.len();
        if updated > 0 {
            self.surreal_writer_handle()?
                .enqueue_file_metadata(file_metadata_records)
                .await?;
        }

        self.finish_bar(
            metadata_pb,
            format!("💾 File metadata complete: {} files tracked", files.len()),
        )?;

        info!("💾 Updated metadata for {updated} of {} files", files.len());
        Ok(hashes)
    }

    async fn persist_project_metadata(
        &self,
        stats: &IndexStats,
        node_count: usize,
        edge_count: usize,
        fingerprint: &str,
    ) -> Result<()> {
        let project_name = self
            .project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.project_id)
            .to_string();
        let root_path = self.project_root.to_string_lossy().to_string();
        let primary_language = self.config.languages.first().cloned();
        let record = ProjectMetadataRecord {
            metadata: serde_json::json!({"input_fingerprint": fingerprint, "complete": stats.complete, "stats": stats}),
            project_id: self.project_id.clone(),
            name: project_name,
            root_path,
            primary_language,
            file_count: stats.files as i64,
            node_count: node_count as i64,
            edge_count: edge_count as i64,
            avg_coverage_score: 0.0,
            last_analyzed: chrono::Utc::now(),
            codegraph_version: env!("CARGO_PKG_VERSION").to_string(),
            organization_id: self.organization_id.clone(),
            domain: self.domain.clone(),
        };

        self.enqueue_project_metadata_record(record).await
    }

    fn annotate_node(&self, node: &mut CodeNode) {
        node.metadata
            .attributes
            .insert("project_id".to_string(), self.project_id.clone());
        if let Some(org) = &self.organization_id {
            node.metadata
                .attributes
                .insert("organization_id".to_string(), org.clone());
        }
        if let Some(repo) = &self.repository_url {
            node.metadata
                .attributes
                .insert("repository_url".to_string(), repo.clone());
        }
        if let Some(domain) = &self.domain {
            node.metadata
                .attributes
                .insert("domain".to_string(), domain.clone());
        }
        // Add embedding model name for tracking which embedding provider was used
        node.metadata
            .attributes
            .insert("embedding_model".to_string(), self.embedding_model.clone());
    }

    async fn persist_nodes_batch(&self, chunk: &[CodeNode]) -> Result<()> {
        self.enqueue_nodes_chunk(chunk).await
    }

    /// Name kept for history; it now reads through IndexStorage so it is backend-neutral.
    async fn log_surreal_node_count(&self, expected: usize) {
        match self.storage.node_count(&self.project_id).await {
            Ok(count) => {
                info!(
                    "🗄️ SurrealDB nodes persisted: {} (expected ≈ {})",
                    count, expected
                );
            }
            Err(e) => {
                warn!("⚠️ SurrealDB node count query failed: {}", e);
            }
        }
    }

    /// SurrealDB handle for the running indexer.
    ///
    /// Only the SurrealDB backend owns one: calling this with
    /// `CODEGRAPH_BACKEND=postgres` panics, because there is no SurrealDB handle.
    #[cfg(feature = "surrealdb")]
    pub async fn surreal_storage(&self) -> Arc<TokioMutex<SurrealDbStorage>> {
        self.surreal
            .as_ref()
            .expect("surreal_storage() requires the SurrealDB backend")
            .clone()
    }

    /// Name kept for history; it now reads through IndexStorage so it is backend-neutral.
    #[cfg(feature = "embeddings")]
    async fn log_surreal_chunk_count(&self, expected: usize) {
        match self.storage.chunk_count(&self.project_id).await {
            Ok(count) => {
                info!(
                    "🧩 SurrealDB chunks persisted: {} (expected ≈ {})",
                    count, expected
                );
            }
            Err(e) => {
                warn!("⚠️ SurrealDB chunk count query failed: {}", e);
            }
        }
    }

    #[cfg(all(test, feature = "surrealdb"))]
    pub async fn test_fetch_file_metadata(
        &self,
        file_path: &str,
    ) -> Result<Option<serde_json::Value>> {
        let db = {
            let storage = self
                .surreal
                .as_ref()
                .expect("test helper requires the SurrealDB backend")
                .lock()
                .await;
            storage.db()
        };

        let mut resp = db
	            .query(
	                "SELECT file_path, last_indexed_at, node_count FROM file_metadata WHERE file_path = $file_path",
	            )
	            .bind(("file_path", file_path.to_string()))
	            .await?;
        let rows: Vec<serde_json::Value> = resp.take(0)?;
        Ok(rows.into_iter().next())
    }

    /// Name kept for history; it now reads through IndexStorage so it is backend-neutral.
    async fn log_surreal_edge_count(&self, expected: usize) {
        match self.storage.edge_count(&self.project_id).await {
            Ok(count) => {
                info!(
                    "🗄️ SurrealDB edges persisted: {} (expected ≈ {})",
                    count, expected
                );
                if count < expected as i64 {
                    warn!(
                        "⚠️ Edge count ({}) is lower than resolved edges ({}). Verify SurrealDB schema and filters.",
                        count, expected
                    );
                }
            }
            Err(e) => warn!("⚠️ SurrealDB edge count query failed: {}", e),
        }
    }

    async fn verify_file_metadata_count(&self, expected_files: usize) -> Result<()> {
        if expected_files == 0 {
            return Ok(());
        }

        let mut last_count = 0i64;
        let mut stored = Vec::new();
        for attempt in 1..=3 {
            stored = self.storage.file_metadata(&self.project_id).await?;
            last_count = stored.len() as i64;

            if last_count >= expected_files as i64 {
                return Ok(());
            }

            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
            }
        }

        // Fetch a small sample to aid debugging
        let sample_paths: Vec<String> = stored
            .into_iter()
            .take(5)
            .map(|row| row.file_path)
            .collect();

        Err(anyhow!(
            "file_metadata count {} is less than expected {} for project {}. Sample stored file_paths: {:?}",
            last_count,
            expected_files,
            self.project_id,
            sample_paths
        ))
    }

    async fn verify_project_metadata_present(&self) -> Result<()> {
        if !self.storage.has_project_metadata(&self.project_id).await? {
            bail!(
                "project_metadata missing for project {}; ensure schema applied and permissions allow writes",
                self.project_id
            );
        }
        Ok(())
    }

    async fn persist_node_embeddings(&self, nodes: &[CodeNode]) -> Result<()> {
        let mut records = Vec::new();
        for node in nodes {
            if let Some(embedding) = &node.embedding {
                records.push(NodeEmbeddingRecord {
                    id: node.id.to_string(),
                    column: self.embedding_column.column_name(),
                    embedding: embedding.to_vec(),
                    updated_at: chrono::Utc::now(),
                });
            }
        }
        if records.is_empty() {
            return Ok(());
        }
        self.surreal_writer_handle()?
            .enqueue_node_embeddings(records)
            .await
    }

    async fn store_symbol_embedding(
        &self,
        symbol: &str,
        node_id: Option<NodeId>,
        source_edge_id: Option<&str>,
        embedding: &[f32],
    ) -> Result<()> {
        let record = self.build_symbol_embedding_record(symbol, node_id, source_edge_id, embedding);
        self.persist_symbol_embedding_records(vec![record]).await
    }

    fn build_symbol_embedding_record(
        &self,
        symbol: &str,
        node_id: Option<NodeId>,
        source_edge_id: Option<&str>,
        embedding: &[f32],
    ) -> SymbolEmbeddingRecord {
        let normalized = Self::normalize_symbol(symbol);
        let node_id_string = node_id.map(|id| id.to_string());
        SymbolEmbeddingRecord::new(
            &self.project_id,
            self.organization_id.as_deref(),
            symbol,
            &normalized,
            embedding,
            &self.embedding_model,
            self.embedding_column.column_name(),
            node_id_string.as_deref(),
            source_edge_id,
            None,
        )
    }

    async fn persist_symbol_embedding_records(
        &self,
        records: Vec<SymbolEmbeddingRecord>,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let batch_size = symbol_embedding_db_batch_size();
        let handle = self.surreal_writer_handle()?;
        for chunk in records.chunks(batch_size) {
            handle.enqueue_symbol_embeddings(chunk.to_vec()).await?;
        }
        Ok(())
    }

    async fn enqueue_chunk_embeddings(&self, records: Vec<ChunkEmbeddingRecord>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        debug!(
            "🧩 Queueing {} chunk embeddings for SurrealDB",
            records.len()
        );
        let batch_size = chunk_embedding_db_batch_size(self.config.batch_size.max(1));
        let handle = self.surreal_writer_handle()?;
        for chunk in records.chunks(batch_size) {
            handle.enqueue_chunk_embeddings(chunk.to_vec()).await?;
        }
        Ok(())
    }

    fn surreal_writer_handle(&self) -> Result<&SurrealWriterHandle> {
        self.surreal_writer
            .as_ref()
            .ok_or_else(|| anyhow!("Surreal writer not initialized"))
    }

    async fn enqueue_nodes_chunk(&self, nodes: &[CodeNode]) -> Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let batch: Vec<CodeNode> = nodes.to_vec();
        self.surreal_writer_handle()?.enqueue_nodes(batch).await
    }

    async fn enqueue_edges(&self, edges: Vec<CodeEdge>) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        self.surreal_writer_handle()?.enqueue_edges(edges).await
    }

    async fn enqueue_project_metadata_record(&self, record: ProjectMetadataRecord) -> Result<()> {
        self.surreal_writer_handle()?
            .enqueue_project_metadata(record)
            .await
    }

    async fn flush_surreal_writer(&self) -> Result<()> {
        if let Some(writer) = &self.surreal_writer {
            writer.flush().await
        } else {
            Ok(())
        }
    }

    async fn shutdown_surreal_writer(&mut self) -> Result<()> {
        match self.surreal_writer.take() {
            Some(writer) => writer.shutdown().await,
            _ => Ok(()),
        }
    }

    #[cfg(feature = "surrealdb")]
    async fn connect_surreal(
        project_root: &Path,
    ) -> Result<(
        Arc<TokioMutex<SurrealDbStorage>>,
        Vec<Arc<TokioMutex<SurrealDbStorage>>>,
    )> {
        let mut config = SurrealDbConfig::for_project(project_root);
        config.cache_enabled = false;

        info!(
            "🗄️ Connecting to SurrealDB: {} namespace={} database={}",
            Self::sanitize_surreal_url(&config.connection),
            config.namespace,
            config.database
        );

        // An embedded store is one engine shared by the process, so a pool adds nothing.
        let pool_size = if config.is_embedded() {
            1
        } else {
            std::env::var("CODEGRAPH_SURREAL_POOL_SIZE")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .map(|n| n.clamp(1, 4))
                .unwrap_or(1)
        };

        let mut pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let storage = SurrealDbStorage::new(config.clone())
                .await
                .with_context(|| {
                    format!("Failed to connect to SurrealDB at {}", config.connection)
                })?;
            pool.push(Arc::new(TokioMutex::new(storage)));
        }

        let storage = pool
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("Surreal pool empty after initialization"))?;

        info!(
            "🗄️ SurrealDB connection established: {} namespace={} database={}",
            Self::sanitize_surreal_url(&config.connection),
            config.namespace,
            config.database
        );

        Ok((storage, pool))
    }

    #[cfg(feature = "surrealdb")]
    fn log_surrealdb_status(&self, phase: &str) {
        let config = SurrealDbConfig::for_project(&self.project_root);
        let auth_state = if config.username.is_some() || config.password.is_some() {
            "credentials configured"
        } else {
            "no auth"
        };
        info!(
            "🗄️ SurrealDB ({}): target={} namespace={} database={} auth={}",
            phase,
            Self::sanitize_surreal_url(&config.connection),
            config.namespace,
            config.database,
            auth_state
        );
    }

    #[cfg(not(feature = "surrealdb"))]
    fn log_surrealdb_status(&self, _phase: &str) {}

    #[cfg(feature = "surrealdb")]
    fn sanitize_surreal_url(raw: &str) -> String {
        if let Ok(mut url) = Url::parse(raw) {
            if !url.username().is_empty() {
                let _ = url.set_username("****");
            }
            if url.password().is_some() {
                let _ = url.set_password(Some("****"));
            }
            url.to_string()
        } else {
            raw.to_string()
        }
    }

    fn normalize_symbol(symbol: &str) -> String {
        symbol.trim().to_lowercase()
    }

    fn normalize_symbol_target(target: &str) -> Vec<String> {
        // Normalize edge targets to match codegraph-parser emitted symbol keys.
        let mut variants = Vec::new();

        let mut base = target.trim();

        // Strip trailing macro bang
        if let Some(stripped) = base.strip_suffix('!') {
            base = stripped;
        }

        // Strip call parentheses and args: take up to first '('
        if let Some(idx) = base.find('(') {
            base = &base[..idx];
        }

        // Strip generics by removing everything from first '<' that has a matching '>'
        let mut generic_stripped = String::new();
        let mut depth = 0;
        for ch in base.chars() {
            if ch == '<' {
                depth += 1;
                continue;
            }
            if ch == '>' && depth > 0 {
                depth -= 1;
                continue;
            }
            if depth == 0 {
                generic_stripped.push(ch);
            }
        }
        let mut cleaned = generic_stripped.trim().to_string();

        // Strip trait qualification: "Type as Trait::method" -> "Type::method"
        if let Some(pos) = cleaned.find(" as ") {
            let after = &cleaned[pos + 4..];
            if let Some(idx) = after.find("::") {
                cleaned = format!("{}{}", &cleaned[..pos], &after[idx..]);
            }
        }

        // Align with parser naming: drop self./this./super./crate:: prefixes
        let prefixes = ["self::", "self.", "this.", "super::", "super.", "crate::"];
        for p in prefixes {
            if let Some(stripped) = cleaned.strip_prefix(p) {
                cleaned = stripped.to_string();
                break;
            }
        }

        // Generate both module separators used by emitters (Rust uses ::, Python/JS often .)
        let dotted = cleaned.replace("::", ".");
        let coloned = cleaned.replace('.', "::");

        let lower_clean = cleaned.to_lowercase();
        let lower_dotted = dotted.to_lowercase();
        let lower_coloned = coloned.to_lowercase();

        variants.push(cleaned.clone());
        variants.push(lower_clean.clone());
        variants.push(dotted.clone());
        variants.push(lower_dotted.clone());
        variants.push(coloned.clone());
        variants.push(lower_coloned.clone());

        // Push last path segment variants
        for candidate in [&cleaned, &dotted, &coloned] {
            if let Some(last) = candidate.rsplit(['.', ':']).next() {
                variants.push(last.to_string());
                variants.push(last.to_lowercase());
            }
        }

        variants.sort();
        variants.dedup();
        variants
    }

    #[cfg(test)]
    pub(crate) fn normalize_symbol_target_for_tests(target: &str) -> Vec<String> {
        Self::normalize_symbol_target(target)
    }

    fn normalize_rust_symbol(target: &str) -> Vec<String> {
        // Demangle if possible (rustc, then Symbolic as fallback to strip hashes)
        let demangled = try_demangle(target)
            .map(|d| d.to_string())
            .unwrap_or_else(|_| target.to_string());

        let symbolic = demangle(&demangled).into_owned();

        let mut out = Vec::new();

        // Strip generics/path args via syn
        if let Ok(path) = parse_syn_path::<SynPath>(&symbolic) {
            let mut simple = Vec::new();
            for seg in path.segments {
                let name = seg.ident.to_string();
                simple.push(name);
            }
            if !simple.is_empty() {
                let joined = simple.join("::");
                out.push(joined.clone());
                out.push(joined.to_lowercase());
                if let Some(last) = simple.last() {
                    out.push(last.clone());
                    out.push(last.to_lowercase());
                }
            }
        }

        // Fallback: return demangled text plus lowered forms
        if out.is_empty() {
            out.push(symbolic.clone());
            out.push(symbolic.to_lowercase());
        }

        out
    }

    fn normalize_js_symbol(target: &str) -> Vec<String> {
        // Lightweight heuristic until JS parser dependency is restored
        Self::normalize_symbol_target(target)
    }

    fn normalize_python_symbol(target: &str) -> Vec<String> {
        // Fallback: simple split heuristics for Python until AST parser is re-enabled
        Self::normalize_symbol_target(target)
    }

    fn progress_enabled() -> bool {
        if let Ok(v) = std::env::var("CODEGRAPH_NO_PROGRESS") {
            let v = v.trim();
            if v == "1" || v.eq_ignore_ascii_case("true") {
                return false;
            }
        }
        if let Ok(v) = std::env::var("CODEGRAPH_PROGRESS") {
            let v = v.trim();
            if v == "0" || v.eq_ignore_ascii_case("false") {
                return false;
            }
        }
        true
    }

    fn progress_draw_target() -> ProgressDrawTarget {
        if Self::progress_enabled() {
            ProgressDrawTarget::stderr_with_hz(4)
        } else {
            ProgressDrawTarget::hidden()
        }
    }

    fn create_progress_bar(&self, total: u64, message: &str) -> ProgressBar {
        let pb = self.progress.add(ProgressBar::new(total));
        pb.set_draw_target(Self::progress_draw_target());
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.blue} [{elapsed_precise}] [{bar:34.cyan/blue}] {pos}/{len} ({percent}%) {msg} | {per_sec}/s | ETA:{eta}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        pb.set_message(message.to_string());
        pb
    }

    #[cfg(feature = "embeddings")]
    fn load_file_sources(
        &self,
        files: &Vec<(PathBuf, u64)>,
    ) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        for (p, _) in files {
            if let Ok(content) = std::fs::read_to_string(p) {
                map.insert(p.to_string_lossy().to_string(), content);
            }
        }
        map
    }

    /// Create enhanced progress bar with dual metrics for files and success rates
    fn create_dual_progress_bar(
        &self,
        total: u64,
        primary_msg: &str,
        secondary_msg: &str,
    ) -> ProgressBar {
        let pb = self.progress.add(ProgressBar::new(total));
        pb.set_draw_target(Self::progress_draw_target());
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.blue} [{elapsed_precise}] [{bar:34.cyan/blue}] {pos}/{len} {msg} | Success: {percent}% | {per_sec}/s | ETA:{eta}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        pb.set_message(format!("{} | {}", primary_msg, secondary_msg));
        pb
    }

    /// Create high-performance progress bar for batch processing
    fn create_batch_progress_bar(&self, total: u64, batch_size: usize, label: &str) -> ProgressBar {
        let pb = self.progress.add(ProgressBar::new(total));
        pb.set_draw_target(Self::progress_draw_target());
        let batch_info = format!("row limit: {}", batch_size);

        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.blue} [{elapsed_precise}] [{bar:34.cyan/blue}] {pos}/{len} items | {msg} | {percent}% | {per_sec}/s | ETA:{eta}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        pb.set_message(format!("{} | {}", label, batch_info));
        pb
    }

    fn finish_bar(&self, pb: ProgressBar, message: impl Into<String>) -> Result<()> {
        pb.finish_and_clear();
        if Self::progress_enabled() {
            self.progress.println(message.into())?;
        }
        Ok(())
    }

    /// Limit reconciliation to the scanned path when it is narrower than the project.
    ///
    /// Indexing a subdirectory or a single file must not treat every other file in the
    /// project as deleted, which is what the unscoped reconciliation would do.
    fn reconcile_scope(project_root: &Path, scanned: &Path) -> Option<ReconcileScope> {
        let project_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let scan_root = scanned.canonicalize().unwrap_or_else(|_| {
            if scanned.is_absolute() {
                scanned.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(scanned)
            }
        });
        if scan_root == project_root {
            return None;
        }
        Some(ReconcileScope {
            project_root,
            scan_root,
        })
    }

    /// Repoint stored inbound edges from old node ids to the new ids of symbols that
    /// survived this re-index under the same file, name and kind.
    ///
    /// Node ids embed the source line, so moving a symbol changes its id even when the
    /// symbol itself is unchanged. Callers in other files still hold the old id; without
    /// this they would lose the edge when reconciliation deletes the stale node.
    async fn redirect_inbound_edges(
        &self,
        fresh_nodes_by_file: &std::collections::BTreeMap<String, Vec<String>>,
        new_ids: &std::collections::HashSet<codegraph_core::NodeId>,
        by_identity: &std::collections::HashMap<(String, String, String), codegraph_core::NodeId>,
    ) -> Result<()> {
        let files: Vec<String> = fresh_nodes_by_file.keys().cloned().collect();
        if files.is_empty() {
            return Ok(());
        }

        let rows = self
            .storage
            .nodes_in_files(&self.project_id, &files)
            .await?;

        let mut updates: Vec<(String, String)> = Vec::new();
        for row in rows {
            let old = row.id;
            let old_id = codegraph_core::NodeId::parse_str(&old).ok();
            if old_id.is_some_and(|id| new_ids.contains(&id)) {
                continue; // already a current id
            }
            if let Some(new_id) = by_identity.get(&(row.file_path, row.name, row.node_type)) {
                let new = new_id.to_string();
                if new != old {
                    updates.push((old, new));
                }
            }
        }

        for (old, new) in updates {
            self.storage
                .retarget_edges(&self.project_id, &old, &new)
                .await?;
        }

        Ok(())
    }

    /// Identity used to match a symbol across re-indexes: file, name and node kind.
    fn node_identity(node: &codegraph_core::CodeNode) -> (String, String, String) {
        (
            node.location.file_path.clone(),
            node.name.to_string(),
            node.node_type
                .as_ref()
                .map(|kind| format!("{kind:?}"))
                .unwrap_or_default(),
        )
    }

    /// Index a single file, leaving every other file in the project untouched.
    /// Uses upsert semantics - no duplicate records created.
    pub async fn index_single_file(&self, path: &Path) -> Result<()> {
        self.reconcile_project(path, false).await.map(|_| ())
    }

    /// Drop the stored graph rows for one file only.
    pub async fn delete_file_data(&self, path: &Path) -> Result<()> {
        let relative = path
            .strip_prefix(&self.project_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf());
        let mut key = relative.to_string_lossy().replace('\\', "/");
        while let Some(stripped) = key.strip_prefix("./") {
            key = stripped.to_string();
        }
        self.delete_data_for_files(&[key]).await?;
        self.flush_surreal_writer().await
    }

    /// Detect language from file extension
    fn detect_language(&self, path: &Path) -> Option<codegraph_core::Language> {
        use codegraph_core::Language;

        let ext = path.extension()?.to_str()?.to_lowercase();
        match ext.as_str() {
            "rs" => Some(Language::Rust),
            "py" => Some(Language::Python),
            "ts" => Some(Language::TypeScript),
            "tsx" => Some(Language::TypeScript),
            "js" => Some(Language::JavaScript),
            "jsx" => Some(Language::JavaScript),
            "go" => Some(Language::Go),
            "java" => Some(Language::Java),
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "c" | "h" => Some(Language::Cpp),
            "swift" => Some(Language::Swift),
            "kt" | "kts" => Some(Language::Kotlin),
            "cs" => Some(Language::CSharp),
            "rb" => Some(Language::Ruby),
            "php" => Some(Language::Php),
            "dart" => Some(Language::Dart),
            _ => None,
        }
    }

    pub fn should_reconcile(&self, path: &Path) -> bool {
        if path.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some(".codegraph" | ".git" | "target" | "node_modules")
            )
        }) {
            return false;
        }
        self.should_index(path) || crate::reconciliation::support_file(path)
    }

    pub async fn watch_for_changes(&self, path: impl AsRef<Path>) -> Result<()> {
        use notify::{Event, RecursiveMode, Watcher};
        let path = path.as_ref().to_path_buf();
        let (tx, mut rx) = mpsc::channel(100);
        let debounce = std::time::Duration::from_millis(
            std::env::var("CODEGRAPH_WATCH_DEBOUNCE_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        );
        let mut watcher =
            notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                let _ = tx.blocking_send(res);
            })?;
        watcher.watch(&path, RecursiveMode::Recursive)?;
        while let Some(first) = rx.recv().await {
            let relevant = |event: &std::result::Result<Event, notify::Error>| {
                event.as_ref().map_or(true, |event| {
                    !matches!(event.kind, notify::EventKind::Access(_))
                        && (event.need_rescan()
                            || event.paths.iter().any(|path| self.should_reconcile(path)))
                })
            };
            let mut dirty = relevant(&first);
            // Trailing debounce retains the final save, renames, deletes and overflow rescans.
            while let Ok(Some(event)) = tokio::time::timeout(debounce, rx.recv()).await {
                dirty |= relevant(&event);
            }
            if dirty && let Err(error) = self.reconcile_project(&path, false).await {
                warn!("Watch reconciliation failed: {error}");
            }
        }
        Ok(())
    }
}

impl ProjectIndexer {
    async fn handle_file_event(
        &self,
        event: notify::Event,
        _last_events: &mut std::collections::HashMap<PathBuf, std::time::Instant>,
        _debounce_ms: u64,
    ) {
        if !matches!(event.kind, notify::EventKind::Access(_))
            && (event.need_rescan() || event.paths.iter().any(|path| self.should_reconcile(path)))
        {
            if let Err(error) = self.reconcile_project(&self.project_root, false).await {
                warn!("Watch reconciliation failed: {error}");
                return;
            }
            if let Some(tx) = WATCH_TEST_NOTIFIER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap()
                .as_ref()
            {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        }
    }

    pub async fn simulate_file_event(
        &self,
        event: notify::Event,
        last_events: &mut std::collections::HashMap<PathBuf, std::time::Instant>,
        debounce_ms: u64,
    ) {
        self.handle_file_event(event, last_events, debounce_ms)
            .await;
    }
}

fn symbol_embedding_db_batch_size() -> usize {
    parse_symbol_embedding_db_batch_size(
        std::env::var("CODEGRAPH_SYMBOL_DB_BATCH_SIZE")
            .ok()
            .as_deref(),
    )
}

fn parse_symbol_embedding_db_batch_size(value: Option<&str>) -> usize {
    const MAX: usize = 512;
    value
        .and_then(|value| value.parse::<usize>().ok())
        .map(|parsed| parsed.clamp(1, MAX))
        .unwrap_or(SYMBOL_EMBEDDING_DB_BATCH_LIMIT)
}

fn node_db_batch_size(fallback: usize) -> usize {
    const MAX: usize = 512;
    std::env::var("CODEGRAPH_NODE_DB_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|parsed| parsed.clamp(1, MAX))
        .unwrap_or(fallback.clamp(1, MAX))
}

fn chunk_embedding_db_batch_size(fallback: usize) -> usize {
    const MAX: usize = 512;
    const DEFAULT_CAP: usize = 32; // Surreal query depth is limited; keep chunk writes modest by default
    std::env::var("CODEGRAPH_CHUNK_DB_BATCH_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|parsed| parsed.clamp(1, MAX))
        .unwrap_or(fallback.min(DEFAULT_CAP).min(MAX))
}

fn resolve_embedding_column(dim: usize) -> Result<EmbeddingColumn> {
    match dim {
        384 => Ok(EmbeddingColumn::Embedding384),
        768 => Ok(EmbeddingColumn::Embedding768),
        1024 => Ok(EmbeddingColumn::Embedding1024),
        1536 => Ok(EmbeddingColumn::Embedding1536),
        2048 => Ok(EmbeddingColumn::Embedding2048),
        2560 => Ok(EmbeddingColumn::Embedding2560),
        3072 => Ok(EmbeddingColumn::Embedding3072),
        4096 => Ok(EmbeddingColumn::Embedding4096),
        other => Err(anyhow!(
            "Unsupported embedding dimension {}. Supported: 384, 768, 1024, 1536, 2048, 2560, 3072, 4096",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_embedding_batch_size_defaults() {
        assert_eq!(
            parse_symbol_embedding_db_batch_size(None),
            SYMBOL_EMBEDDING_DB_BATCH_LIMIT
        );
    }

    #[test]
    fn symbol_embedding_batch_size_respects_env_and_clamps() {
        assert_eq!(parse_symbol_embedding_db_batch_size(Some("1024")), 512);
        assert_eq!(parse_symbol_embedding_db_batch_size(Some("0")), 1);
        assert_eq!(
            parse_symbol_embedding_db_batch_size(Some("invalid")),
            SYMBOL_EMBEDDING_DB_BATCH_LIMIT
        );
    }

    #[test]
    fn embedding_column_supports_2560_dimension() {
        let column = resolve_embedding_column(2560).expect("2560-d embeddings should be supported");
        assert_eq!(column.column_name(), "embedding_2560");
        assert_eq!(column.dimension(), 2560);
    }

    #[test]
    fn embedding_column_supports_1536_dimension() {
        let column = resolve_embedding_column(1536).expect("1536-d embeddings should be supported");
        assert_eq!(column.column_name(), "embedding_1536");
        assert_eq!(column.dimension(), 1536);
    }

    #[test]
    fn embedding_column_supports_3072_dimension() {
        let column = resolve_embedding_column(3072).expect("3072-d embeddings should be supported");
        assert_eq!(column.column_name(), "embedding_3072");
        assert_eq!(column.dimension(), 3072);
    }

    #[test]
    fn fast_tier_filters_noisy_edges() {
        let mut edges = vec![
            EdgeRelationship {
                from: NodeId::new_v4(),
                to: "a".to_string(),
                edge_type: EdgeType::Calls,
                metadata: std::collections::HashMap::new(),
                span: None,
            },
            EdgeRelationship {
                from: NodeId::new_v4(),
                to: "b".to_string(),
                edge_type: EdgeType::Uses,
                metadata: std::collections::HashMap::new(),
                span: None,
            },
            EdgeRelationship {
                from: NodeId::new_v4(),
                to: "c".to_string(),
                edge_type: EdgeType::References,
                metadata: std::collections::HashMap::new(),
                span: None,
            },
            EdgeRelationship {
                from: NodeId::new_v4(),
                to: "d".to_string(),
                edge_type: EdgeType::Other("flows_to".to_string()),
                metadata: std::collections::HashMap::new(),
                span: None,
            },
        ];

        let removed = filter_edges_for_tier(
            codegraph_core::config_manager::IndexingTier::Fast,
            &mut edges,
        );
        assert_eq!(removed, 2);
        assert!(edges.iter().any(|e| e.edge_type == EdgeType::Calls));
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Other("flows_to".to_string()))
        );
        assert!(!edges.iter().any(|e| e.edge_type == EdgeType::Uses));
        assert!(!edges.iter().any(|e| e.edge_type == EdgeType::References));
    }

    #[test]
    fn metadata_text_formatter_never_truncates_unicode_or_long_source() {
        let content = "let café = \"🚀\";\n".repeat(1000);
        let node = CodeNode::new(
            "fixture",
            Some(NodeType::Function),
            Some(codegraph_core::Language::Rust),
            codegraph_core::Location {
                file_path: "fixture.rs".into(),
                line: 1,
                column: 0,
                end_line: None,
                end_column: None,
            },
        )
        .with_content(content.clone());
        assert!(prepare_node_text(&node).ends_with(&content));
    }
}

pub fn prepare_node_text(node: &CodeNode) -> String {
    let lang = node
        .language
        .as_ref()
        .map(|l| format!("{:?}", l))
        .unwrap_or_else(|| "unknown".to_string());
    let kind = node
        .node_type
        .as_ref()
        .map(|t| format!("{:?}", t))
        .unwrap_or_else(|| "unknown".to_string());
    let mut text = format!("{} {} {}", lang, kind, node.name);
    if let Some(c) = &node.content {
        text.push(' ');
        text.push_str(c);
    }

    text
}

pub fn simple_text_embedding(text: &str, dimension: usize) -> Vec<f32> {
    let mut embedding = vec![0.0f32; dimension];
    let mut hash = 5381u32;
    for b in text.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    let mut state = hash;
    for value in &mut embedding {
        state = state.wrapping_mul(1103515245).wrapping_add(12345);
        *value = ((state as f32 / u32::MAX as f32) - 0.5) * 2.0;
    }
    embedding
}

pub fn normalize(v: &[f32]) -> Vec<f32> {
    let mut out = v.to_vec();
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut out {
            *x /= norm;
        }
    }
    out
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IndexStats {
    pub cached_files: usize,
    pub index_ms: u64,
    pub startup_ms: u64,
    pub phase_ms: std::collections::BTreeMap<String, u64>,
    pub writer_jobs_acked: u64,
    pub writer_rows_acked: u64,
    pub writer_payload_bytes_acked: u64,
    pub source_read_operations: u64,
    pub source_read_bytes: u64,
    pub source_spilled_bytes: u64,
    pub graph_complete: bool,
    pub vector_index_ready: bool,
    pub embedding_status: String,
    pub semantic_status: String,
    pub complete: bool,
    pub inference_texts: u64,
    pub inference_tokens: u64,
    pub embedding_cache_hits: u64,
    pub files: usize,
    pub skipped: usize,
    pub lines: usize,
    pub functions: usize,
    pub classes: usize,
    pub structs: usize,
    pub traits: usize,
    pub embeddings: usize,
    pub errors: usize,
    // Extended stats for detailed reporting
    pub nodes: usize,
    pub edges: usize,
    pub chunks: usize,
    pub embedding_dimension: usize,
    pub embedding_provider: String,
    pub resolved_edges: usize,
    pub unresolved_edges: usize,
    pub resolution_rate: f64,
    // Analyzer stats
    pub analyzers_enabled: bool,
    pub build_context_nodes: usize,
    pub build_context_edges: usize,
    pub lsp_nodes_enriched: usize,
    pub lsp_edges_resolved: usize,
    /// Files this run could not enrich: no SCIP coverage and (by default) no
    /// language-server fallback. Zero means every scanned file got compiler-index
    /// or language-server data.
    #[serde(default)]
    pub files_without_enrichment: usize,
    pub docs_attached: usize,
    pub export_edges_added: usize,
    pub reexport_edges_added: usize,
    pub feature_enables_edges_added: usize,
    pub uses_edges_derived: usize,
    pub module_nodes_added: usize,
    pub module_contains_edges_added: usize,
    pub module_import_edges_added: usize,
    pub dataflow_variable_nodes_added: usize,
    pub dataflow_defines_edges_added: usize,
    pub dataflow_uses_edges_added: usize,
    pub dataflow_flows_to_edges_added: usize,
    pub dataflow_returns_edges_added: usize,
    pub dataflow_mutates_edges_added: usize,
    pub doc_nodes_added: usize,
    pub document_edges_added: usize,
    pub specification_edges_added: usize,
    pub package_cycles_detected: usize,
    pub boundary_violations_added: usize,
}

impl IndexStats {
    fn merge(&mut self, other: FileStats) {
        self.files += 1;
        self.lines += other.lines;
        self.functions += other.functions;
        self.classes += other.classes;
        self.structs += other.structs;
        self.traits += other.traits;
        self.embeddings += other.embeddings;
    }
}

#[derive(Debug, Default, Clone)]
struct FileStats {
    lines: usize,
    functions: usize,
    classes: usize,
    structs: usize,
    traits: usize,
    embeddings: usize,
}
#[cfg(feature = "embeddings")]
use codegraph_vector::prep::chunker::ChunkMeta;

/// Normalize a source path to a project-relative, posix-style key.
///
/// Node ids embed this path, so the same file must resolve to one key regardless of
/// whether `codegraph index` was given `.`, `./src`, an absolute path, or a path from a
/// different working directory. Paths outside the project root keep their original
/// form (minus `./`), so external analyzer inputs are not misrepresented as project
/// files.
pub(crate) fn normalize_file_path(project_root: &Path, raw: &str) -> String {
    let raw_path = Path::new(raw);
    let absolute = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        project_root.join(raw_path)
    };
    let normalized = lexical_normalize(&absolute);

    let relative = match normalized.strip_prefix(project_root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => normalized,
    };

    let mut out = relative.to_string_lossy().replace('\\', "/");
    while let Some(stripped) = out.strip_prefix("./") {
        out = stripped.to_string();
    }
    if out.is_empty() { ".".to_string() } else { out }
}

/// Files this run leaves AST-only: every scanned file that no supplied SCIP index
/// covers. The caller passes `files.len() - scip_covered.len()`; the named helper keeps
/// that contract explicit (and testable) instead of an anonymous subtraction.
fn files_without_enrichment(uncovered_scip_files: usize) -> usize {
    uncovered_scip_files
}

/// One `CODEGRAPH_SCIP_INDEX` entry: a SCIP artifact plus the optional directory
/// prefix its document paths are relative to (for indexes generated in a subdirectory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScipIndexSpec {
    pub path: PathBuf,
    pub path_prefix: Option<String>,
}

/// Parse `CODEGRAPH_SCIP_INDEX` as comma-separated `<path>[@<subdir>]` entries.
///
/// Unset or blank values yield no indexes (no compiler-index enrichment runs). A malformed
/// entry is a hard error: silently treating `"a.scip,b.scip"` as one path would
/// produce a confusing "file not found" far from the real mistake.
pub(crate) fn parse_scip_indexes(raw: Option<&str>) -> Result<Vec<ScipIndexSpec>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut specs = Vec::new();
    for (index, entry) in raw.split(',').enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(anyhow!(
                "invalid CODEGRAPH_SCIP_INDEX: entry {} is empty; expected `<path>[@<subdir>]` entries separated by commas",
                index + 1
            ));
        }
        let (path, path_prefix) = match entry.rsplit_once('@') {
            Some((path, prefix)) => {
                let path = path.trim();
                let prefix = prefix.trim();
                if path.is_empty() {
                    return Err(anyhow!(
                        "invalid CODEGRAPH_SCIP_INDEX entry {entry:?}: missing index path before `@`"
                    ));
                }
                if prefix.is_empty() {
                    return Err(anyhow!(
                        "invalid CODEGRAPH_SCIP_INDEX entry {entry:?}: missing subdirectory after `@`"
                    ));
                }
                (path, Some(normalize_scip_prefix(prefix)?))
            }
            None => (entry, None),
        };
        specs.push(ScipIndexSpec {
            path: PathBuf::from(path),
            path_prefix,
        });
    }
    Ok(specs)
}

/// Normalise a `@<subdir>` prefix to the same posix form as project-relative paths,
/// rejecting prefixes that would escape the project root.
fn normalize_scip_prefix(prefix: &str) -> Result<String> {
    if Path::new(prefix).is_absolute() {
        return Err(anyhow!(
            "invalid CODEGRAPH_SCIP_INDEX subdirectory {prefix:?}: must be relative to the project root"
        ));
    }
    let mut parts = Vec::new();
    let normalized = prefix.replace('\\', "/");
    for part in normalized.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(anyhow!(
                    "invalid CODEGRAPH_SCIP_INDEX subdirectory {prefix:?}: must stay inside the project root"
                ));
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(anyhow!(
            "invalid CODEGRAPH_SCIP_INDEX subdirectory {prefix:?}: resolves to the project root; omit `@` instead"
        ));
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod scip_index_tests {
    use super::*;

    #[test]
    fn unset_or_blank_means_no_scip_index() {
        assert!(parse_scip_indexes(None).unwrap().is_empty());
        assert!(parse_scip_indexes(Some("")).unwrap().is_empty());
        assert!(parse_scip_indexes(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn single_path_keeps_legacy_behavior() {
        assert_eq!(
            parse_scip_indexes(Some("/tmp/rust.scip")).unwrap(),
            vec![ScipIndexSpec {
                path: PathBuf::from("/tmp/rust.scip"),
                path_prefix: None,
            }]
        );
    }

    #[test]
    fn multiple_paths_and_prefixes_parse() {
        assert_eq!(
            parse_scip_indexes(Some(
                "/tmp/rust.scip, /tmp/go.scip@easytier-go ,/tmp/ts.scip@ui/web"
            ))
            .unwrap(),
            vec![
                ScipIndexSpec {
                    path: PathBuf::from("/tmp/rust.scip"),
                    path_prefix: None,
                },
                ScipIndexSpec {
                    path: PathBuf::from("/tmp/go.scip"),
                    path_prefix: Some("easytier-go".into()),
                },
                ScipIndexSpec {
                    path: PathBuf::from("/tmp/ts.scip"),
                    path_prefix: Some("ui/web".into()),
                },
            ]
        );
    }

    #[test]
    fn prefixes_are_normalized() {
        assert_eq!(
            parse_scip_indexes(Some("/tmp/go.scip@./easytier-go/"))
                .unwrap()
                .remove(0)
                .path_prefix,
            Some("easytier-go".into())
        );
        assert_eq!(
            parse_scip_indexes(Some("/tmp/go.scip@easytier-go\\sub"))
                .unwrap()
                .remove(0)
                .path_prefix,
            Some("easytier-go/sub".into())
        );
    }

    #[test]
    fn malformed_entries_are_errors_not_one_path() {
        // Empty middle entry.
        assert!(parse_scip_indexes(Some("/tmp/a.scip,,/tmp/b.scip")).is_err());
        // Trailing separator.
        assert!(parse_scip_indexes(Some("/tmp/a.scip,")).is_err());
        // Missing path or prefix around `@`.
        assert!(parse_scip_indexes(Some("@easytier-go")).is_err());
        assert!(parse_scip_indexes(Some("/tmp/go.scip@")).is_err());
        assert!(parse_scip_indexes(Some("/tmp/go.scip@.")).is_err());
        // Prefix must not escape the project root.
        assert!(parse_scip_indexes(Some("/tmp/go.scip@..")).is_err());
        assert!(parse_scip_indexes(Some("/tmp/go.scip@a/../b")).is_err());
        assert!(parse_scip_indexes(Some("/tmp/go.scip@/abs")).is_err());
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod normalize_file_path_tests {
    use super::*;

    #[test]
    fn strips_dot_prefix_and_absolute_roots() {
        let root = Path::new("/repo");
        assert_eq!(normalize_file_path(root, "./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_file_path(root, "src/a.rs"), "src/a.rs");
        assert_eq!(normalize_file_path(root, "/repo/src/a.rs"), "src/a.rs");
        assert_eq!(
            normalize_file_path(root, "./src/./nested/../a.rs"),
            "src/a.rs"
        );
    }

    #[test]
    fn keeps_paths_outside_the_project_root_absolute() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize_file_path(root, "/usr/local/crate/src/lib.rs"),
            "/usr/local/crate/src/lib.rs"
        );
    }

    #[test]
    fn is_idempotent() {
        let root = Path::new("/repo");
        let once = normalize_file_path(root, "./packages/core/src/tool/mcp.ts");
        assert_eq!(once, "packages/core/src/tool/mcp.ts");
        assert_eq!(normalize_file_path(root, &once), once);
    }
}

/// Classify resolved relationships into `(exact, lexical, semantic)` counts.
///
/// Iterates over `targets` and `provenance` pairwise. If the two slices have
/// different lengths, iteration stops at the shorter slice, so the extra
/// trailing entries of the longer slice are ignored and the function never
/// panics.
///
/// * `targets[i]` being `None` (unresolved) is never counted in any category,
///   even when the provenance string at that index looks like a resolved method.
/// * A resolved target is classified by `provenance[i].0`:
///   `"lexical"` → lexical, `"definition" | "exact" | "normalized"` → exact,
///   `"semantic"` → semantic, and any other string → not counted.
fn classify_resolution_counts(
    targets: &[Option<NodeId>],
    provenance: &[(&'static str, f64)],
) -> (usize, usize, usize) {
    let mut exact = 0;
    let mut lexical = 0;
    let mut semantic = 0;
    for (target, (method, _score)) in targets.iter().zip(provenance.iter()) {
        if target.is_none() {
            continue;
        }
        match *method {
            "lexical" => lexical += 1,
            "definition" | "exact" | "normalized" => exact += 1,
            "semantic" => semantic += 1,
            _ => {}
        }
    }
    (exact, lexical, semantic)
}

#[cfg(test)]
mod resolution_count_tests {
    use super::*;

    #[test]
    fn unresolved_edges_are_not_counted() {
        let targets = [Some(NodeId::new_v4()), None, None, Some(NodeId::new_v4())];
        let provenance = [
            ("exact", 1.0),
            ("exact", 1.0),
            ("normalized", 1.0),
            ("lexical", 1.0),
        ];
        assert_eq!(classify_resolution_counts(&targets, &provenance), (1, 1, 0));
    }

    #[test]
    fn semantic_matches_are_counted() {
        let targets = [
            Some(NodeId::new_v4()),
            Some(NodeId::new_v4()),
            Some(NodeId::new_v4()),
        ];
        let provenance = [("definition", 1.0), ("semantic", 0.78), ("semantic", 0.81)];
        assert_eq!(classify_resolution_counts(&targets, &provenance), (1, 0, 2));
    }

    #[test]
    fn unresolved_semantic_is_not_counted() {
        let targets = [None, Some(NodeId::new_v4())];
        let provenance = [("semantic", 0.0), ("normalized", 1.0)];
        assert_eq!(classify_resolution_counts(&targets, &provenance), (1, 0, 0));
    }

    #[test]
    fn mismatched_lengths_do_not_panic() {
        let targets = [Some(NodeId::new_v4()), Some(NodeId::new_v4())];
        let provenance = [("exact", 1.0)];
        assert_eq!(classify_resolution_counts(&targets, &provenance), (1, 0, 0));
    }
}

#[cfg(test)]
mod enrichment_coverage_tests {
    use super::*;

    #[test]
    fn files_without_enrichment_counts_uncovered_files_only() {
        assert_eq!(
            files_without_enrichment(287),
            287,
            "without SCIP coverage every scanned file stays AST-only"
        );
        assert_eq!(
            files_without_enrichment(0),
            0,
            "full SCIP coverage reports no AST-only file"
        );
    }
}

#[cfg(test)]
mod storage_backend_tests {
    use super::*;

    /// `std::env::set_var` is process-wide, so backend-selection tests must not
    /// run concurrently with each other.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Serialises the test and restores `CODEGRAPH_BACKEND` to "unset" on drop,
    /// including when an assertion panics.
    struct BackendEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl BackendEnvGuard {
        fn new(value: Option<&str>) -> Self {
            let lock = ENV_MUTEX
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match value {
                Some(value) => unsafe { std::env::set_var("CODEGRAPH_BACKEND", value) },
                None => unsafe { std::env::remove_var("CODEGRAPH_BACKEND") },
            }
            Self { _lock: lock }
        }
    }

    impl Drop for BackendEnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("CODEGRAPH_BACKEND") };
        }
    }

    #[cfg(feature = "surrealdb")]
    #[test]
    fn backend_from_env_defaults_to_surreal() {
        let _guard = BackendEnvGuard::new(None);
        assert_eq!(
            StorageBackend::from_env().expect("unset CODEGRAPH_BACKEND must be valid"),
            StorageBackend::Surreal
        );
    }

    #[cfg(not(feature = "surrealdb"))]
    #[test]
    fn backend_from_env_defaults_to_postgres() {
        let _guard = BackendEnvGuard::new(None);
        assert_eq!(
            StorageBackend::from_env().expect("unset CODEGRAPH_BACKEND must be valid"),
            StorageBackend::Postgres
        );
    }

    #[cfg(feature = "surrealdb")]
    #[test]
    fn backend_from_env_accepts_surreal_aliases() {
        for value in ["surreal", "surrealdb", " Surreal "] {
            let _guard = BackendEnvGuard::new(Some(value));
            assert_eq!(
                StorageBackend::from_env().expect("surreal alias must be accepted"),
                StorageBackend::Surreal,
                "`{value}` must select the SurrealDB backend"
            );
        }
    }

    #[cfg(not(feature = "surrealdb"))]
    #[test]
    fn backend_from_env_rejects_surreal_without_the_feature() {
        let _guard = BackendEnvGuard::new(Some("surreal"));
        let error = StorageBackend::from_env().expect_err("surreal must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("requires a build with --features surrealdb"),
            "unexpected error message: {message}"
        );
    }

    #[test]
    fn backend_from_env_accepts_postgres_aliases() {
        for value in ["postgres", "PG"] {
            let _guard = BackendEnvGuard::new(Some(value));
            assert_eq!(
                StorageBackend::from_env().expect("postgres alias must be accepted"),
                StorageBackend::Postgres,
                "`{value}` must select the Postgres backend"
            );
        }
    }

    #[test]
    fn backend_from_env_rejects_unknown() {
        let _guard = BackendEnvGuard::new(Some("mysql"));
        let error = StorageBackend::from_env().expect_err("`mysql` must not be accepted");
        let message = error.to_string();
        assert!(
            message.contains("unknown CODEGRAPH_BACKEND"),
            "unexpected error message: {message}"
        );
    }

    /// Without the `postgres` feature the backend exists as a variant but must
    /// fail loudly instead of silently falling back to SurrealDB.
    #[cfg(not(feature = "postgres"))]
    #[tokio::test]
    async fn open_storage_rejects_postgres_without_the_feature() {
        let result = open_storage(
            StorageBackend::Postgres,
            Path::new("."),
            StorageMode::ReadOnly,
        )
        .await;
        let Err(error) = result else {
            panic!("Postgres must not open without --features postgres");
        };
        let message = error.to_string();
        assert!(
            message.contains("requires a build with --features postgres"),
            "unexpected error message: {message}"
        );
    }
}
