// ABOUTME: Direct graph queries without the LLM agent workflow — milliseconds, not minutes.
// ABOUTME: Backs `codegraph query`; read-only, no schema writes, no providers contacted.
#[cfg(feature = "surrealdb")]
use anyhow::bail;
use anyhow::{Context, Result};
use clap::Args;
use codegraph_mcp::{StorageBackend, StorageMode, open_storage};
use serde_json::{Value, json};

/// Printed when a lookup matches nothing, so a script can tell "no such symbol"
/// apart from an indexed project with zero rows.
const EMPTY_MATCH_HINT: &str = "no exact or prefix match; try the full name, or use `codegraph agent context` for semantic search";

/// Fast direct graph queries.
///
/// The four `agent` commands always run a full ReAct workflow (measured 22s–2.5min
/// per question, dominated by LLM latency, with the graph lookup itself under
/// 100ms). For everyday questions — "where is this symbol", "who calls it" —
/// that round-trip is the bottleneck. These subcommands query the store directly
/// through [`codegraph_graph::storage::IndexStorage`] and return in milliseconds.
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
        help = "Maximum results per section (clamped to 1..=1000; 0 means 1000)"
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

/// The compact JSON printed when a name lookup finds nothing.
fn empty_match_output(symbol: &str) -> Value {
    json!({"symbol": symbol, "matches": 0, "hint": EMPTY_MATCH_HINT})
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

    let backend = StorageBackend::from_env()?;

    // Only the embedded SurrealDB store needs an on-disk existence check; a server
    // can hold the project even when there is no local `.codegraph` directory.
    // Keep it so an unindexed project still fails loudly instead of silently
    // creating an empty embedded store and reporting "no match".
    #[cfg(feature = "surrealdb")]
    if backend == StorageBackend::Surreal {
        let config = codegraph_graph::SurrealDbConfig::for_project(&project_root);
        if let Some(embedded_path) = config.connection.strip_prefix("surrealkv://") {
            if !std::path::Path::new(embedded_path).exists() {
                bail!(
                    "project not indexed: embedded store {} does not exist. Run `codegraph index {}` first, or set CODEGRAPH_SURREALDB_URL to the server that holds this project.",
                    embedded_path,
                    project_root.display()
                );
            }
        }
    }

    let storage = open_storage(backend, &project_root, StorageMode::ReadOnly).await?;

    // The trait takes an `i64` and clamps it to `1..=1000`, so `--limit 0` now means
    // 1000 rows rather than zero.
    let limit = i64::try_from(args.limit).unwrap_or(i64::MAX);

    // --- Symbol lookup -------------------------------------------------
    let matches = storage
        .find_nodes_by_name(&project_id, &args.symbol, limit)
        .await
        .context("symbol lookup")?;

    if matches.is_empty() {
        println!("{}", empty_match_output(&args.symbol));
        return Ok(());
    }

    // --- Callers / callees over `calls` edges ---------------------------
    let mut relations = serde_json::Map::new();
    if args.callers || args.callees {
        let target = matches
            .first()
            .map(|row| row.name.clone())
            .unwrap_or_default();

        if args.callers {
            let rows = storage
                .callers_of_name(&project_id, &target, limit)
                .await
                .context("callers lookup")?;
            relations.insert("callers".into(), serde_json::to_value(&rows)?);
        }

        if args.callees {
            let rows = storage
                .callees_of_name(&project_id, &target, limit)
                .await
                .context("callees lookup")?;
            relations.insert("callees".into(), serde_json::to_value(&rows)?);
        }
    }

    let output = json!({
        "symbol": args.symbol,
        "matches": serde_json::to_value(&matches)?,
        "relations": relations,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_match_output_keeps_the_script_contract() {
        let output = empty_match_output("nope");
        assert_eq!(output["symbol"], "nope");
        assert_eq!(output["matches"], 0);
        assert_eq!(output["hint"], EMPTY_MATCH_HINT);
        assert_eq!(
            output.as_object().map(|object| object.len()),
            Some(3),
            "the empty-result JSON is exactly symbol/matches/hint"
        );
    }
}
