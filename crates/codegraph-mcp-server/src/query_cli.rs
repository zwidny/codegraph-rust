// ABOUTME: Direct graph queries without the LLM agent workflow — milliseconds, not minutes.
// ABOUTME: Backs `codegraph query`; read-only, no schema writes, no providers contacted.
#[cfg(feature = "surrealdb")]
use anyhow::bail;
use anyhow::{Context, Result};
use clap::Args;
use codegraph_graph::storage::SymbolMatch;
use codegraph_mcp::{StorageBackend, StorageMode, open_storage};
use serde_json::{Value, json};

/// Printed when a lookup matches nothing, so a script can tell "no such symbol"
/// apart from an indexed project with zero rows.
const EMPTY_MATCH_HINT: &str = "no exact or prefix match; try the full name, or use `codegraph agent context` for semantic search";

/// Row ceiling for the `--file` name lookup.
///
/// `--file` must see **every** definition sharing the name before it can filter
/// by path, so the caller's `--limit` cannot be pushed into the lookup. The value
/// is the same as `codegraph_graph::storage::LOOKUP_LIMIT_MAX`, which the backend
/// clamps to anyway; that constant is crate-private, so it is mirrored here and
/// the two must stay in sync.
const FILE_LOOKUP_LIMIT: i64 = 1000;

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

    /// Only consider the definition in this file (exact path or path suffix),
    /// and scope --callers/--callees to that exact node.
    #[arg(long)]
    pub file: Option<String>,
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

/// The compact JSON printed when `--file` filtered every match away.
///
/// Same three keys as [`empty_match_output`], but the hint names the file so the
/// caller can tell "the symbol exists elsewhere" from "no such symbol".
fn empty_match_output_for_file(symbol: &str, file: &str) -> Value {
    json!({
        "symbol": symbol,
        "matches": 0,
        "hint": format!("no definition named {symbol} in {file}"),
    })
}

/// Does `file_path` denote `wanted`, either exactly or as a path suffix?
///
/// `\` and `/` are normalised first, so a Windows-indexed path still matches a
/// `--file alacritty_terminal/src/term/cell.rs` suffix. Suffix matching is
/// component-aligned: `b/c.rs` matches `a/b/c.rs`, but `c.rs` never matches
/// `abc.rs`.
fn matches_file(file_path: &str, wanted: &str) -> bool {
    let normalise = |path: &str| path.replace('\\', "/");
    let file_path = normalise(file_path);
    let wanted = normalise(wanted);
    file_path == wanted || file_path.ends_with(&format!("/{wanted}"))
}

/// Filter by `--file`, then apply the user's `--limit`.
///
/// Order matters: the lookup hands back *every* same-named definition (up to
/// [`FILE_LOOKUP_LIMIT`]) precisely so the file filter can run before the row
/// limit. Truncating first would inspect only the first N `new`s and silently
/// miss the one in the requested file.
///
/// `limit` is the raw `--limit`: `0` means the 1000-row ceiling (the same rule
/// the backends' `clamp_lookup_limit` applies), so it must not truncate to zero.
fn select_matches(
    mut matches: Vec<SymbolMatch>,
    file: Option<&str>,
    limit: usize,
) -> Vec<SymbolMatch> {
    if let Some(wanted) = file {
        matches.retain(|row| matches_file(&row.file_path, wanted));
    }
    let effective_limit = if limit == 0 {
        FILE_LOOKUP_LIMIT as usize
    } else {
        limit
    };
    matches.truncate(effective_limit);
    matches
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
    // `--file` pins one definition, so the name lookup has to see every
    // definition before filtering: passing the caller's limit would inspect only
    // the first N same-named nodes and could miss the one in the requested file.
    let lookup_limit = if args.file.is_some() {
        FILE_LOOKUP_LIMIT
    } else {
        limit
    };
    let fetched = storage
        .find_nodes_by_name(&project_id, &args.symbol, lookup_limit)
        .await
        .context("symbol lookup")?;

    // Pin the lookup to one definition file, supporting a full path or a
    // suffix such as `src/term/cell.rs`. Afterwards `matches` is non-empty and
    // `--callers`/`--callees` must go by node id rather than by name.
    let matches = select_matches(fetched, args.file.as_deref(), args.limit);

    if matches.is_empty() {
        match args.file.as_deref() {
            Some(wanted) => println!("{}", empty_match_output_for_file(&args.symbol, wanted)),
            None => println!("{}", empty_match_output(&args.symbol)),
        }
        return Ok(());
    }

    // --- Callers / callees over `calls` edges ---------------------------
    let mut relations = serde_json::Map::new();
    if args.callers || args.callees {
        let first = matches.first();
        let target = first.map(|row| row.name.clone()).unwrap_or_default();
        // `--file` has already narrowed `matches` to one definition, so its id
        // pins the exact node; without `--file` the historical name lookup is
        // kept unchanged (every definition sharing the name).
        let node_id = first.map(|row| row.id.clone()).unwrap_or_default();
        let by_id = args.file.is_some();

        if args.callers {
            let rows = if by_id {
                storage
                    .callers_of_id(&project_id, &node_id, limit)
                    .await
                    .context("callers lookup")?
            } else {
                storage
                    .callers_of_name(&project_id, &target, limit)
                    .await
                    .context("callers lookup")?
            };
            relations.insert("callers".into(), serde_json::to_value(&rows)?);
        }

        if args.callees {
            let rows = if by_id {
                storage
                    .callees_of_id(&project_id, &node_id, limit)
                    .await
                    .context("callees lookup")?
            } else {
                storage
                    .callees_of_name(&project_id, &target, limit)
                    .await
                    .context("callees lookup")?
            };
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
    fn file_filter_runs_before_the_limit() {
        // Four same-named definitions elsewhere, the wanted one last: with
        // `--limit 2` a truncate-then-filter would never see it.
        let m = |file: &str| SymbolMatch {
            id: format!("id:{file}"),
            name: "new".to_string(),
            node_type: Some("Function".to_string()),
            language: Some("Rust".to_string()),
            file_path: file.to_string(),
            start_line: Some(1),
            end_line: Some(2),
        };
        let matches = vec![
            m("a/one.rs"),
            m("a/two.rs"),
            m("a/three.rs"),
            m("a/four.rs"),
            m("alacritty_terminal/src/term/cell.rs"),
        ];

        let selected = select_matches(
            matches.clone(),
            Some("alacritty_terminal/src/term/cell.rs"),
            2,
        );
        assert_eq!(selected.len(), 1, "the file filter must run before --limit");
        assert_eq!(selected[0].file_path, "alacritty_terminal/src/term/cell.rs");

        // Without `--file` the limit still applies to the front of the list.
        let plain = select_matches(matches.clone(), None, 2);
        assert_eq!(
            plain
                .iter()
                .map(|row| row.file_path.as_str())
                .collect::<Vec<_>>(),
            vec!["a/one.rs", "a/two.rs"]
        );

        // `--limit 0` means the 1000-row ceiling, not zero rows.
        assert_eq!(select_matches(matches.clone(), None, 0).len(), 5);

        // Three definitions in the wanted file truncate to the user limit.
        let three = vec![m("a/b/c.rs"), m("a/b/c.rs"), m("a/b/c.rs")];
        assert_eq!(select_matches(three, Some("c.rs"), 2).len(), 2);
    }

    #[test]
    fn file_filter_accepts_full_path_and_suffix() {
        assert!(matches_file("a/b/c.rs", "a/b/c.rs"), "exact path");
        assert!(matches_file("a/b/c.rs", "b/c.rs"), "component suffix");
        assert!(matches_file("a/b/c.rs", "c.rs"), "bare file name");
        assert!(!matches_file("a/b/d.rs", "c.rs"), "different file");
        assert!(
            !matches_file("a/bc.rs", "c.rs"),
            "suffix matching must stay component-aligned"
        );
    }

    #[test]
    fn file_filter_normalises_separators() {
        assert!(
            matches_file("b\\c.rs", "b/c.rs"),
            "a Windows-indexed path matches a forward-slash --file"
        );
        assert!(
            matches_file("b/c.rs", "b\\c.rs"),
            "a Windows --file matches a forward-slash path"
        );
    }

    #[test]
    fn empty_match_output_for_file_keeps_the_contract() {
        let output = empty_match_output_for_file("new", "a/b.rs");
        assert_eq!(output["symbol"], "new");
        assert_eq!(output["matches"], 0);
        assert_eq!(output["hint"], "no definition named new in a/b.rs");
        assert_eq!(
            output.as_object().map(|object| object.len()),
            Some(3),
            "the empty-result JSON is exactly symbol/matches/hint"
        );
    }

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
