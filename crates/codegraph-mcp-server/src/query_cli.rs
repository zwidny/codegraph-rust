// ABOUTME: Direct graph queries without the LLM agent workflow — milliseconds, not minutes.
// ABOUTME: Backs `codegraph query`; read-only, no schema writes, no providers contacted.
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::json;

/// Fast direct graph queries.
///
/// The four `agent` commands always run a full ReAct workflow (measured 22s–2.5min
/// per question, dominated by LLM latency, with the graph lookup itself under
/// 100ms). For everyday questions — "where is this symbol", "who calls it" —
/// that round-trip is the bottleneck. These subcommands query the store
/// directly through SurrealQL and return in milliseconds.
#[derive(Args, Debug)]
pub struct QueryArgs {
    /// Symbol name to search (exact match first, then prefix matches)
    pub symbol: String,

    /// Show who calls this symbol (incoming `calls` edges)
    #[arg(long, short)]
    pub callers: bool,

    /// Show what this symbol calls (outgoing `calls` edges)
    #[arg(long)]
    pub callees: bool,

    #[arg(
        long,
        short,
        default_value_t = 20,
        help = "Maximum results per section"
    )]
    pub limit: usize,

    /// Project directory (defaults to the current directory)
    #[arg(long)]
    pub project: Option<std::path::PathBuf>,

    /// Override the project id used to scope the query (for example `plan:<id>`)
    #[arg(long)]
    pub project_id: Option<String>,
}

pub fn run(args: QueryArgs) -> Result<()> {
    // SAFETY: no worker threads exist before the runtime is created.
    unsafe { codegraph_core::config_manager::ConfigManager::initialize_environment() };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async(args))
}

async fn run_async(args: QueryArgs) -> Result<()> {
    let project_root = match &args.project {
        Some(path) => path
            .canonicalize()
            .with_context(|| path.display().to_string())?,
        None => std::env::current_dir()?.canonicalize()?,
    };
    let project_id = args
        .project_id
        .clone()
        .or_else(|| std::env::var("CODEGRAPH_PROJECT_ID").ok())
        .unwrap_or_else(|| project_root.display().to_string());

    // Resolve the store exactly like `index`, `agent`, and the MCP server do:
    // `CODEGRAPH_SURREALDB_URL` when set (shared server), otherwise the default
    // shared server. The embedded per-project store is opt-in via a `surrealkv://`
    // URL and must not be assumed just because the project directory exists.
    let config = codegraph_graph::SurrealDbConfig::for_project(&project_root);

    // Only the embedded store needs an on-disk existence check; a server can hold
    // the project even when there is no local `.codegraph` directory.
    if let Some(embedded_path) = config.connection.strip_prefix("surrealkv://") {
        if !std::path::Path::new(embedded_path).exists() {
            bail!(
                "project not indexed: embedded store {} does not exist. Run `codegraph index {}` first, or set CODEGRAPH_SURREALDB_URL to the server that holds this project.",
                embedded_path,
                project_root.display()
            );
        }
    }

    let connection = config.connection.clone();
    let storage = codegraph_graph::SurrealDbStorage::new(config)
        .await
        .map_err(|e| anyhow::anyhow!("connect to SurrealDB at {connection}: {e}"))?;
    let db = storage.db();

    // --- Symbol lookup -------------------------------------------------
    let mut response = db
        .query(
            r#"
            SELECT name, node_type, language, file_path, start_line, end_line
            FROM nodes
            WHERE project_id = $project
              AND (name = $symbol OR string::starts_with(name, $symbol))
            LIMIT $limit;
            "#,
        )
        .bind(("project", project_id.clone()))
        .bind(("symbol", args.symbol.clone()))
        .bind(("limit", args.limit))
        .await
        .context("symbol lookup")?
        .check()
        .context("symbol lookup")?;
    let matches: Vec<serde_json::Value> = response.take(0).unwrap_or_default();

    if matches.is_empty() {
        println!(
            "{}",
            json!({"symbol": args.symbol, "matches": 0, "hint": "no exact or prefix match; try the full name, or use `codegraph agent context` for semantic search"})
        );
        return Ok(());
    }

    // --- Callers / callees over `calls` edges ---------------------------
    let mut relations = serde_json::Map::new();
    if args.callers || args.callees {
        let first = &matches[0];
        let target = first["name"].as_str().unwrap_or_default().to_string();

        if args.callers {
            let mut response = db
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
                .bind(("project", project_id.clone()))
                .bind(("target", target.clone()))
                .bind(("limit", args.limit))
                .await
                .context("callers lookup")?
                .check()
                .context("callers lookup")?;
            let rows: Vec<serde_json::Value> = response.take(0).unwrap_or_default();
            relations.insert("callers".into(), json!(rows));
        }

        if args.callees {
            let mut response = db
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
                .bind(("project", project_id.clone()))
                .bind(("target", target.clone()))
                .bind(("limit", args.limit))
                .await
                .context("callees lookup")?
                .check()
                .context("callees lookup")?;
            let rows: Vec<serde_json::Value> = response.take(0).unwrap_or_default();
            relations.insert("callees".into(), json!(rows));
        }
    }

    let output = json!({
        "symbol": args.symbol,
        "matches": matches,
        "relations": relations,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
