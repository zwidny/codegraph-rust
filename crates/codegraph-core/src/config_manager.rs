use crate::rerank_config::RerankConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{info, warn};

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Config file not found: {0}")]
    NotFound(String),

    #[error("Failed to read config: {0}")]
    ReadError(String),

    #[error("Failed to parse config: {0}")]
    ParseError(String),

    #[error("Invalid configuration: {0}")]
    ValidationError(String),

    #[error("Model not found: {0}")]
    ModelNotFound(String),
}

/// Main configuration for CodeGraph
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodeGraphConfig {
    /// Embedding provider configuration
    #[serde(default)]
    pub embedding: EmbeddingConfig,

    /// Reranking provider configuration
    #[serde(default)]
    pub rerank: RerankConfig,

    /// Performance and resource settings
    #[serde(default)]
    pub performance: PerformanceConfig,

    /// Indexing configuration
    #[serde(default)]
    pub indexing: IndexingConfig,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Daemon configuration for automatic file watching
    #[serde(default)]
    pub daemon: DaemonConfig,
}

/// Embedding provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Provider: "onnx", "ollama", "openai", "lmstudio", "jina", or "auto"
    #[serde(default = "default_embedding_provider")]
    pub provider: String,

    /// Model path or identifier
    /// For ONNX: path to model directory
    /// For Ollama: model name (e.g., "all-minilm:latest")
    /// For LM Studio: model name (e.g., "jinaai/jina-embeddings-v3")
    /// For OpenAI: model name (e.g., "text-embedding-3-small")
    /// For Jina: model name (e.g., "jina-embeddings-v4")
    #[serde(default)]
    pub model: Option<String>,

    /// LM Studio URL (if using LM Studio)
    #[serde(default = "default_lmstudio_url")]
    pub lmstudio_url: String,

    /// Ollama URL (if using Ollama)
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,

    /// OpenAI API key (if using OpenAI)
    #[serde(default)]
    pub openai_api_key: Option<String>,

    /// Jina API key (if using Jina)
    #[serde(default)]
    pub jina_api_key: Option<String>,

    /// Jina API base URL
    #[serde(default = "default_jina_api_base")]
    pub jina_api_base: String,

    /// Jina late chunking
    #[serde(default)]
    pub jina_late_chunking: bool,

    /// Jina task type; "auto" selects a passage task supported by the model.
    #[serde(default = "default_jina_task")]
    pub jina_task: String,

    /// Embedding dimension (1024 for jina-embeddings-v4, 1536 for jina-code, 384 for all-MiniLM)
    #[serde(default = "default_embedding_dimension")]
    pub dimension: usize,

    /// Batch size for embedding generation
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: default_embedding_provider(),
            model: None, // Auto-detect
            lmstudio_url: default_lmstudio_url(),
            ollama_url: default_ollama_url(),
            openai_api_key: None,
            jina_api_key: None,
            jina_api_base: default_jina_api_base(),
            jina_late_chunking: false,
            jina_task: default_jina_task(),
            dimension: default_embedding_dimension(),
            batch_size: default_batch_size(),
        }
    }
}

/// Performance and resource configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    /// Number of worker threads
    #[serde(default = "default_num_threads")]
    pub num_threads: usize,

    /// Cache size in MB
    #[serde(default = "default_cache_size_mb")]
    pub cache_size_mb: usize,

    /// Enable GPU acceleration
    #[serde(default)]
    pub enable_gpu: bool,

    /// Maximum concurrent requests
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
}

/// Indexing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    /// Indexing tier: fast | balanced | full
    #[serde(default)]
    pub tier: IndexingTier,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            tier: IndexingTier::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IndexingTier {
    Fast,
    Balanced,
    Full,
}

impl Default for IndexingTier {
    fn default() -> Self {
        IndexingTier::Fast
    }
}

impl std::str::FromStr for IndexingTier {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_lowercase().as_str() {
            "fast" => Ok(IndexingTier::Fast),
            "balanced" => Ok(IndexingTier::Balanced),
            "full" => Ok(IndexingTier::Full),
            other => Err(format!("Invalid indexing tier: {}", other)),
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            num_threads: default_num_threads(),
            cache_size_mb: default_cache_size_mb(),
            enable_gpu: false, // Conservative default
            max_concurrent_requests: default_max_concurrent(),
        }
    }
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error"
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log format: "pretty", "json", "compact"
    #[serde(default = "default_log_format")]
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
        }
    }
}

/// Daemon configuration for automatic file watching and re-indexing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Enable automatic daemon startup with MCP server
    #[serde(default)]
    pub auto_start_with_mcp: bool,

    /// Project path to watch (defaults to current directory)
    #[serde(default)]
    pub project_path: Option<PathBuf>,

    /// Debounce duration for file changes (ms)
    #[serde(default = "default_daemon_debounce_ms")]
    pub debounce_ms: u64,

    /// Batch timeout for collecting changes (ms)
    #[serde(default = "default_daemon_batch_timeout_ms")]
    pub batch_timeout_ms: u64,

    /// Health check interval (seconds)
    #[serde(default = "default_daemon_health_check_interval")]
    pub health_check_interval_secs: u64,

    /// Languages to watch (empty = all detected)
    #[serde(default)]
    pub languages: Vec<String>,

    /// Exclude patterns (gitignore format)
    #[serde(default = "default_daemon_exclude_patterns")]
    pub exclude_patterns: Vec<String>,

    /// Include patterns (gitignore format)
    #[serde(default)]
    pub include_patterns: Vec<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            auto_start_with_mcp: false, // Opt-in by default
            project_path: None,
            debounce_ms: default_daemon_debounce_ms(),
            batch_timeout_ms: default_daemon_batch_timeout_ms(),
            health_check_interval_secs: default_daemon_health_check_interval(),
            languages: vec![],
            exclude_patterns: default_daemon_exclude_patterns(),
            include_patterns: vec![],
        }
    }
}

// Default value functions
fn default_embedding_provider() -> String {
    "auto".to_string()
}
fn default_lmstudio_url() -> String {
    "http://localhost:1234".to_string()
}
fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_jina_api_base() -> String {
    "https://api.jina.ai/v1".to_string()
}
fn default_jina_task() -> String {
    "auto".to_string()
}
fn default_embedding_dimension() -> usize {
    2048
} // jina-embeddings-v4
fn default_batch_size() -> usize {
    64
}

/// Context window assumed when none is configured.
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

fn default_num_threads() -> usize {
    num_cpus::get()
}
fn default_cache_size_mb() -> usize {
    512
}
fn default_max_concurrent() -> usize {
    4
}
fn default_log_level() -> String {
    "warn".to_string()
} // Clean TUI output during indexing
fn default_log_format() -> String {
    "pretty".to_string()
}

// Daemon default functions
fn default_daemon_debounce_ms() -> u64 {
    30
}
fn default_daemon_batch_timeout_ms() -> u64 {
    200
}
fn default_daemon_health_check_interval() -> u64 {
    30
}
fn default_daemon_exclude_patterns() -> Vec<String> {
    vec![
        "**/node_modules/**".to_string(),
        "**/target/**".to_string(),
        "**/.git/**".to_string(),
        "**/build/**".to_string(),
        "**/.codegraph/**".to_string(),
        "**/dist/**".to_string(),
        "**/__pycache__/**".to_string(),
    ]
}

/// Configuration manager with smart defaults and auto-detection
pub struct ConfigManager {
    config: CodeGraphConfig,
    config_path: Option<PathBuf>,
}

impl ConfigManager {
    /// Load configuration with the following precedence:
    /// 1. Environment variables (initialize dotenv before starting workers)
    /// 2. Config file (.codegraph.toml)
    /// 3. Sensible defaults
    pub fn load() -> Result<Self, ConfigError> {
        info!("🔧 Loading CodeGraph configuration...");

        // Try to find and load config file
        let (config, config_path) = Self::load_config_file()?;

        // Override with environment variables
        let config = Self::apply_env_overrides(config);

        // Validate configuration
        Self::validate_config(&config)?;

        info!("✅ Configuration loaded successfully");
        if let Some(ref path) = config_path {
            info!("   📄 Config file: {}", path.display());
        } else {
            info!("   📄 Config file: NONE (using defaults)");
        }
        info!("   🤖 Embedding provider: {}", config.embedding.provider);
        info!("   🔧 Embedding model: {:?}", config.embedding.model);
        info!("   📐 Embedding dimension: {}", config.embedding.dimension);
        info!("   🌐 Ollama URL: {}", config.embedding.ollama_url);

        Ok(Self {
            config,
            config_path,
        })
    }

    /// Load project `.env` and user `.codegraph.env` before starting workers.
    /// `load()` itself only reads configuration and never mutates the environment.
    ///
    /// The project file is loaded first and the user file second. `dotenv` never
    /// overwrites a variable that is already set, so project values win and the user
    /// file only fills in the keys the project omitted. A machine-wide
    /// `~/.codegraph.env` therefore stops being shadowed by a small project `.env`.
    ///
    /// # Safety
    /// Call only from a single-threaded process entry point, before other threads
    /// can access the environment (including through foreign libraries).
    pub unsafe fn initialize_environment() {
        // Project overrides first.
        if Path::new(".env").exists() {
            match dotenv::from_filename(".env") {
                Ok(_) => info!("📋 Loaded .env file from current directory"),
                Err(e) => warn!("Failed to load .env file: {}", e),
            }
        }

        // User-level defaults fill whatever the project did not define.
        if let Some(home) = dirs::home_dir() {
            let home_env = home.join(".codegraph.env");
            if home_env.exists() {
                match dotenv::from_path(&home_env) {
                    Ok(_) => {
                        info!("📋 Loaded .codegraph.env from home directory (fallback)")
                    }
                    Err(e) => warn!("Failed to load .codegraph.env: {}", e),
                }
            }
        }
    }

    /// Find and load config file
    /// Search order:
    /// 1. CODEGRAPH_CONFIG_PATH (explicit CLI/environment selection)
    /// 2. ./.codegraph.toml (current directory)
    /// 3. ~/.codegraph/config.toml (user config)
    /// 4. Use defaults
    /// Locate the configuration file: an explicit CLI/environment selection, then the
    /// project file in the working directory, then the user file.
    fn config_file_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CODEGRAPH_CONFIG_PATH") {
            return Some(PathBuf::from(path));
        }
        let local_config = Path::new(".codegraph.toml");
        if local_config.exists() {
            return Some(local_config.to_path_buf());
        }
        dirs::home_dir()
            .map(|home| home.join(".codegraph").join("config.toml"))
            .filter(|user_config| user_config.exists())
    }

    fn load_config_file() -> Result<(CodeGraphConfig, Option<PathBuf>), ConfigError> {
        match Self::config_file_path() {
            Some(path) => {
                let config = Self::read_toml_file(&path)?;
                Ok((config, Some(path)))
            }
            None => {
                info!("📋 No config file found, using defaults");
                Ok((CodeGraphConfig::default(), None))
            }
        }
    }

    /// Context window the MCP client's tool-result limits are sized for:
    /// `CODEGRAPH_CONTEXT_WINDOW`, then the legacy `CODEGRAPH_LLM_CONTEXT_WINDOW`
    /// alias, then the default.
    ///
    /// Only environment variables are consulted. The `[llm] context_window` value that
    /// the removed in-process agent used to read has no effect; [`legacy_llm_context_window`]
    /// detects it so loading a config file can warn about it.
    pub fn agent_context_window() -> usize {
        std::env::var("CODEGRAPH_CONTEXT_WINDOW")
            .or_else(|_| std::env::var("CODEGRAPH_LLM_CONTEXT_WINDOW"))
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW)
    }

    fn read_toml_file(path: &Path) -> Result<CodeGraphConfig, ConfigError> {
        let content =
            std::fs::read_to_string(path).map_err(|e| ConfigError::ReadError(e.to_string()))?;

        let config: CodeGraphConfig =
            toml::from_str(&content).map_err(|e| ConfigError::ParseError(e.to_string()))?;

        if let Some(value) = legacy_llm_context_window(&content)
            && std::env::var("CODEGRAPH_CONTEXT_WINDOW").is_err()
            && std::env::var("CODEGRAPH_LLM_CONTEXT_WINDOW").is_err()
        {
            // `ConfigManager::load` can run more than once per process (startup plus a
            // command handler); warn only on the first sighting.
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let message = format!(
                    "`[llm] context_window = {value}` is no longer used: the in-process agent was \
                     removed in v0.4.0. The MCP tool-result budget now follows \
                     CODEGRAPH_CONTEXT_WINDOW (default {DEFAULT_CONTEXT_WINDOW}); set it to keep \
                     your previous ceiling."
                );
                warn!("{message}");
                // CLI commands such as `codegraph config show` do not install a tracing
                // subscriber, so a log-only warning would stay invisible there.
                if !tracing::dispatcher::has_been_set() {
                    eprintln!("{message}");
                }
            }
        }

        Ok(config)
    }

    /// Apply environment variable overrides
    fn apply_env_overrides(mut config: CodeGraphConfig) -> CodeGraphConfig {
        // Embedding configuration
        if let Ok(provider) = std::env::var("CODEGRAPH_EMBEDDING_PROVIDER") {
            config.embedding.provider = provider;
        }
        if let Ok(model) = std::env::var("CODEGRAPH_EMBEDDING_MODEL") {
            config.embedding.model = Some(model);
        }
        if let Ok(model) = std::env::var("CODEGRAPH_LOCAL_MODEL") {
            config.embedding.model = Some(model);
        }
        if let Ok(url) = std::env::var("CODEGRAPH_OLLAMA_URL") {
            config.embedding.ollama_url = url;
        }
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            config.embedding.openai_api_key = Some(key);
        }
        if let Ok(dimension) = std::env::var("CODEGRAPH_EMBEDDING_DIMENSION") {
            if let Ok(dim) = dimension.parse() {
                config.embedding.dimension = dim;
            }
        }
        // Resolve batch aliases once, before explicit CLI overrides are applied.
        if let Some(size) = [
            "CODEGRAPH_EMBEDDINGS_BATCH_SIZE",
            "CODEGRAPH_EMBEDDING_BATCH_SIZE",
        ]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|size| *size > 0)
        }) {
            config.embedding.batch_size = size;
        }

        // Jina configuration
        if let Ok(key) = std::env::var("JINA_API_KEY") {
            config.embedding.jina_api_key = Some(key);
        }
        if let Ok(base) = std::env::var("JINA_API_BASE") {
            config.embedding.jina_api_base = base;
        }
        config.rerank = config.rerank.with_env_overrides();
        if let Ok(chunking) = std::env::var("JINA_LATE_CHUNKING") {
            config.embedding.jina_late_chunking = chunking.to_lowercase() == "true";
        }
        if let Ok(task) = std::env::var("JINA_API_TASK").or_else(|_| std::env::var("JINA_TASK")) {
            config.embedding.jina_task = task;
        }

        // Indexing configuration
        if let Ok(tier) = std::env::var("CODEGRAPH_INDEX_TIER") {
            match tier.parse::<IndexingTier>() {
                Ok(parsed) => config.indexing.tier = parsed,
                Err(err) => warn!("Invalid CODEGRAPH_INDEX_TIER: {}", err),
            }
        }

        // Logging
        if let Ok(level) = std::env::var("RUST_LOG") {
            config.logging.level = level;
        }

        // Daemon configuration
        if let Ok(auto_start) = std::env::var("CODEGRAPH_DAEMON_AUTO_START") {
            config.daemon.auto_start_with_mcp =
                auto_start.to_lowercase() == "true" || auto_start == "1";
        }
        if let Ok(path) = std::env::var("CODEGRAPH_DAEMON_WATCH_PATH") {
            config.daemon.project_path = Some(PathBuf::from(path));
        }
        if let Ok(debounce) = std::env::var("CODEGRAPH_DAEMON_DEBOUNCE_MS") {
            if let Ok(ms) = debounce.parse() {
                config.daemon.debounce_ms = ms;
            }
        }
        if let Ok(batch) = std::env::var("CODEGRAPH_DAEMON_BATCH_TIMEOUT_MS") {
            if let Ok(ms) = batch.parse() {
                config.daemon.batch_timeout_ms = ms;
            }
        }

        config
    }

    /// Validate configuration
    fn validate_config(config: &CodeGraphConfig) -> Result<(), ConfigError> {
        config
            .rerank
            .validate()
            .map_err(|error| ConfigError::ValidationError(error.to_string()))?;

        // Validate embedding provider
        match config.embedding.provider.as_str() {
            "auto" | "onnx" | "ollama" | "openai" | "jina" | "lmstudio" => {}
            other => {
                return Err(ConfigError::ValidationError(format!(
                    "Invalid embedding provider: {}. Must be one of: auto, onnx, ollama, openai, jina, lmstudio",
                    other
                )));
            }
        }

        // Validate log level
        match config.logging.level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => {
                return Err(ConfigError::ValidationError(format!(
                    "Invalid log level: {}. Must be one of: trace, debug, info, warn, error",
                    other
                )));
            }
        }

        Ok(())
    }

    /// Get the loaded configuration
    pub fn config(&self) -> &CodeGraphConfig {
        &self.config
    }

    /// Get the path to the config file that was loaded, if any
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// Create a default config file
    pub fn create_default_config(path: &Path) -> Result<(), ConfigError> {
        let config = CodeGraphConfig::default();
        let toml_str =
            toml::to_string_pretty(&config).map_err(|e| ConfigError::ParseError(e.to_string()))?;

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::ReadError(e.to_string()))?;
        }

        std::fs::write(path, toml_str).map_err(|e| ConfigError::ReadError(e.to_string()))?;

        Ok(())
    }

    /// Auto-detect available embedding models
    pub fn auto_detect_embedding_model() -> Option<String> {
        // Check for Ollama models
        if Self::check_ollama_available() {
            return Some("ollama:all-minilm".to_string());
        }

        // Check for ONNX models in HuggingFace cache
        if let Some(model_path) = Self::find_onnx_model_in_cache() {
            return Some(model_path);
        }

        None
    }

    /// Check if Ollama is available
    fn check_ollama_available() -> bool {
        // Try to connect to Ollama
        std::process::Command::new("curl")
            .args(["-s", "http://localhost:11434/api/tags"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Find ONNX models in HuggingFace cache
    fn find_onnx_model_in_cache() -> Option<String> {
        let home = dirs::home_dir()?;
        let hf_cache = home.join(".cache/huggingface/hub");

        if !hf_cache.exists() {
            return None;
        }

        // Look for all-MiniLM-L6-v2-onnx model
        let pattern = "models--Qdrant--all-MiniLM-L6-v2-onnx";

        if let Ok(entries) = std::fs::read_dir(&hf_cache) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.file_name()?.to_str()?.contains(pattern) {
                    // Find snapshots directory
                    let snapshots = path.join("snapshots");
                    if let Ok(snapshot_entries) = std::fs::read_dir(&snapshots) {
                        if let Some(snapshot) = snapshot_entries.flatten().next() {
                            return Some(snapshot.path().to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        None
    }
}

/// Detect a `[llm] context_window` left behind by the removed agent config.
///
/// The value is no longer read; callers warn so an upgrade does not silently
/// multiply the MCP tool-result budget (`max_result_bytes = window * 2`).
pub(crate) fn legacy_llm_context_window(source: &str) -> Option<u64> {
    let value: toml::Value = toml::from_str(source).ok()?;
    let integer = value.get("llm")?.get("context_window")?.as_integer()?;
    u64::try_from(integer).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_llm_context_window_reads_the_removed_key() {
        assert_eq!(
            legacy_llm_context_window("[llm]\ncontext_window = 32768\n"),
            Some(32768)
        );
    }

    #[test]
    fn legacy_llm_context_window_ignores_missing_section() {
        assert_eq!(
            legacy_llm_context_window("[indexing]\ntier = \"fast\"\n"),
            None
        );
    }

    #[test]
    fn legacy_llm_context_window_ignores_non_integer() {
        assert_eq!(
            legacy_llm_context_window("[llm]\ncontext_window = \"big\"\n"),
            None
        );
    }

    #[test]
    fn test_default_config() {
        let config = CodeGraphConfig::default();
        assert_eq!(config.embedding.provider, "auto");
        assert_eq!(config.indexing.tier, IndexingTier::Fast);
    }

    #[test]
    fn test_indexing_tier_env_override() {
        if !test_env::run(
            concat!(module_path!(), "::test_indexing_tier_env_override"),
            &[("CODEGRAPH_INDEX_TIER", Some("balanced"))],
        ) {
            return;
        }

        let config = ConfigManager::apply_env_overrides(CodeGraphConfig::default());
        assert_eq!(config.indexing.tier, IndexingTier::Balanced);
    }

    #[test]
    fn rerank_enable_without_model_supplies_jina_defaults() {
        if !test_env::run(
            concat!(
                module_path!(),
                "::rerank_enable_without_model_supplies_jina_defaults"
            ),
            &[
                ("JINA_ENABLE_RERANKING", Some("true")),
                ("JINA_RERANKING_MODEL", None),
                ("CODEGRAPH_ENABLE_RERANKING", None),
                ("CODEGRAPH_RERANK_PROVIDER", None),
            ],
        ) {
            return;
        }
        let config = ConfigManager::apply_env_overrides(CodeGraphConfig::default());
        assert_eq!(config.rerank.provider, crate::RerankProvider::Jina);
        assert!(config.rerank.validate().is_ok());
        assert_eq!(config.rerank.jina.unwrap().model, "jina-reranker-v3");
    }

    #[test]
    fn rerank_provider_env_selects_ollama_without_jina_configuration() {
        if !test_env::run(
            concat!(
                module_path!(),
                "::rerank_provider_env_selects_ollama_without_jina_configuration"
            ),
            &[
                ("CODEGRAPH_RERANK_PROVIDER", Some("ollama")),
                ("JINA_ENABLE_RERANKING", Some("true")),
                ("JINA_RERANKING_MODEL", None),
                ("CODEGRAPH_ENABLE_RERANKING", None),
                ("CODEGRAPH_OLLAMA_RERANK_MODEL", Some("fixture-reranker")),
            ],
        ) {
            return;
        }
        let config = ConfigManager::apply_env_overrides(CodeGraphConfig::default());
        assert_eq!(config.rerank.provider, crate::RerankProvider::Ollama);
        assert!(config.rerank.validate().is_ok());
        assert_eq!(config.rerank.ollama.unwrap().model, "fixture-reranker");
        assert!(config.rerank.jina.is_none());
    }

    #[test]
    fn test_config_validation() {
        let config = CodeGraphConfig::default();
        assert!(ConfigManager::validate_config(&config).is_ok());

        let mut bad_config = config.clone();
        bad_config.embedding.provider = "invalid".to_string();
        assert!(ConfigManager::validate_config(&bad_config).is_err());
    }
}

#[cfg(test)]
mod test_env {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/support/env.rs"
    ));
}
