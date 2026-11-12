// ABOUTME: Imports compiler-produced SCIP identities without starting one language server per run.
// ABOUTME: Streams documents in two passes and validates source identity and position encoding.
use anyhow::{Context, Result, anyhow, bail};
use codegraph_core::{CodeNode, EdgeRelationship, EdgeType, NodeId, Span};
use codegraph_parser::{SourceSnapshot, SourceSnapshots};
use scip::types::{Document, Occurrence};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};
use tracing::{debug, warn};

/// Outcome of one SCIP import. `edges_resolved` counts bindings that either
/// re-pointed an existing edge or materialised a `References` edge; `nodes_enriched`
/// counts distinct nodes that received compiler symbol/doc metadata.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportStats {
    pub documents: usize,
    pub documents_matched: usize,
    pub documents_with_nodes: usize,
    pub nodes_enriched: usize,
    pub edges_resolved: usize,
    /// Project-relative posix keys of every document this index matched to a
    /// captured source. Used to skip LSP per-file enrichment for files the
    /// compiler index already covers. Files that matched but produced no nodes
    /// are included: the index did analyse them, they simply have no symbols.
    pub covered_files: BTreeSet<String>,
}

/// Project-relative posix key, identical to `CodeNode::location.file_path`.
/// SCIP `relative_path` values and captured snapshot paths use different shapes
/// (`src/a.ts`, `./src/a.ts`, absolute, Windows separators), so every lookup goes
/// through the same normalisation instead of comparing raw `PathBuf`s.
fn relative_key(root: &Path, path: &Path) -> String {
    crate::indexer::normalize_file_path(root, &path.to_string_lossy())
}

fn document_key(relative_path: &str) -> String {
    // `Path::join` keeps `.` segments, and SCIP writers spell paths as `./a.ts`,
    // `a//b.ts` or `a\b.ts`, so collapse those instead of only stripping a leading
    // `./`; otherwise a prefixed document can never match its snapshot key.
    let mut parts = Vec::new();
    let normalized = relative_path.replace('\\', "/");
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        parts.push(part);
    }
    parts.join("/")
}

/// Apply a per-index directory prefix with `Path::join` semantics, so the
/// separator between the prefix and the document path is never lost (plain string
/// concatenation would produce `easytier-gohost.go`).
fn joined_document_path(relative_path: &str, path_prefix: Option<&str>) -> PathBuf {
    match path_prefix {
        Some(prefix) => Path::new(prefix).join(relative_path),
        None => PathBuf::from(relative_path),
    }
}

/// Parse `CODEGRAPH_SCIP_MAX_BYTES`. `0` (the default) disables the size check:
/// documents are decoded one at a time, so the file size does not set the memory
/// budget. A positive value is an opt-in safety valve for pathological artifacts.
fn max_bytes_from_env(raw: Option<&str>) -> Result<u64> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(0);
    }
    let value = raw
        .parse::<u64>()
        .map_err(|error| anyhow!("invalid CODEGRAPH_SCIP_MAX_BYTES={raw:?}: {error}"))?;
    Ok(value)
}

fn documents_with_limit(
    path: &Path,
    max_bytes: u64,
    mut visit: impl FnMut(Document) -> Result<()>,
) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    if max_bytes > 0
        && let Ok(metadata) = file.metadata()
        && metadata.len() > max_bytes
    {
        bail!(
            "SCIP artifact is {} bytes, above the configured CODEGRAPH_SCIP_MAX_BYTES={} safety limit",
            metadata.len(),
            max_bytes
        );
    }
    // `Index.documents` is a repeated field; decode one length-delimited document
    // at a time and drop it before reading the next, so peak memory tracks the
    // largest single document instead of the artifact size.
    let mut input = protobuf::CodedInputStream::new(&mut file);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        if tag == 18 {
            visit(input.read_message()?)?;
        } else {
            input.skip_field(
                protobuf::rt::WireType::new(tag & 7)
                    .ok_or_else(|| anyhow!("Invalid SCIP wire type"))?,
            )?;
        }
    }
    Ok(())
}

fn documents(path: &Path, visit: impl FnMut(Document) -> Result<()>) -> Result<()> {
    let max_bytes = max_bytes_from_env(std::env::var("CODEGRAPH_SCIP_MAX_BYTES").ok().as_deref())?;
    documents_with_limit(path, max_bytes, visit)
}

fn symbol_key(document: &str, symbol: &str) -> String {
    if symbol.starts_with("local ") {
        format!("{document}:{symbol}")
    } else {
        symbol.to_owned()
    }
}
fn range(occurrence: &Occurrence) -> Result<[i32; 4]> {
    use scip::types::occurrence::Typed_range;
    let range = match &occurrence.typed_range {
        Some(Typed_range::SingleLineRange(range)) => [
            range.line,
            range.start_character,
            range.line,
            range.end_character,
        ],
        Some(Typed_range::MultiLineRange(range)) => [
            range.start_line,
            range.start_character,
            range.end_line,
            range.end_character,
        ],
        Some(_) => bail!("Unsupported SCIP range variant"),
        None => match occurrence.range.as_slice() {
            [line, start, end] => [*line, *start, *line, *end],
            [line, start, end_line, end] => [*line, *start, *end_line, *end],
            _ => bail!("Invalid SCIP occurrence range"),
        },
    };
    if range.iter().any(|value| *value < 0) || (range[0], range[1]) > (range[2], range[3]) {
        bail!("Invalid SCIP source coordinates");
    }
    Ok(range)
}
/// Warn when the artifact predates the captured sources: positions recorded by the
/// compiler then point at different text, which surfaces as an opaque
/// "SCIP column is invalid" error further down.
fn warn_if_index_predates_sources(index: &Path, sources: &SourceSnapshots) {
    let Ok(index_mtime) = std::fs::metadata(index).and_then(|meta| meta.modified()) else {
        return;
    };
    let mut newer = 0usize;
    let mut total = 0usize;
    for snapshot in sources.iter() {
        total += 1;
        if snapshot.modified_at > index_mtime {
            newer += 1;
        }
    }
    if newer > 0 {
        warn!(
            "SCIP index {} is older than {} of {} captured sources; recorded positions may not match the current text. Regenerate it (`rust-analyzer scip .`, `scip-typescript index`, or `scip-go`) to avoid position errors",
            index.display(),
            newer,
            total
        );
    }
}

/// Message used when a recorded SCIP position cannot be mapped onto the captured text.
fn position_error_context(relative_path: &str, range: &[i32; 4], encoding: i32) -> String {
    let [line, start, end_line, end] = *range;
    format!(
        "SCIP position {relative_path}:{line}:{start} (end {end_line}:{end}, encoding {encoding}) does not fit the captured source; the index is probably stale or built from different sources"
    )
}

fn byte_position(
    source: &str,
    starts: &[usize],
    line: i32,
    column: i32,
    encoding: i32,
) -> Result<usize> {
    let start = *starts
        .get(line as usize)
        .ok_or_else(|| anyhow!("SCIP line outside source"))?;
    let end = starts
        .get(line as usize + 1)
        .copied()
        .unwrap_or(source.len());
    let text = &source[start..end];
    let column = column as usize;
    if encoding == 1 {
        if column <= text.len() && text.is_char_boundary(column) {
            return Ok(start + column);
        }
    } else if encoding == 2 || encoding == 3 {
        let mut units = 0;
        for (offset, character) in text.char_indices() {
            if units == column {
                return Ok(start + offset);
            }
            units += if encoding == 2 {
                character.len_utf16()
            } else {
                1
            };
        }
        if units == column {
            return Ok(end);
        }
    }
    bail!("SCIP column is invalid or document position encoding is unspecified")
}
fn source<'a>(
    document: &Document,
    path_prefix: Option<&str>,
    by_relative: &'a BTreeMap<String, &'a SourceSnapshot>,
    manifest: &BTreeMap<String, String>,
    trust_source: bool,
    missing_manifest: &mut usize,
) -> Result<Option<&'a SourceSnapshot>> {
    // The prefix is part of the document's project-relative path, so it must be
    // applied before the snapshot lookup normalises the path.
    let path = joined_document_path(&document.relative_path, path_prefix);
    // SCIP writers can emit documents outside the indexed directory (`scip-go`
    // records generated files as `../root/.cache/go-build/...`). They can never
    // match a project snapshot, so skip them instead of failing the whole import.
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Ok(None);
    }
    let key = document_key(&path.to_string_lossy());
    let Some(source) = by_relative.get(&key) else {
        return Ok(None);
    };
    if !document.text.is_empty() {
        if document.text.as_str() != source.contents()?.as_ref() {
            bail!("SCIP source is stale: {}", document.relative_path);
        }
    } else if !trust_source {
        match manifest
            .get(&document.relative_path)
            .or_else(|| manifest.get(&key))
        {
            Some(expected) if *expected != source.content_hash => bail!(
                "SCIP source hash is stale for {}: index.sources.json records {}, source is {}; regenerate the compiler index or set CODEGRAPH_SCIP_TRUST_SOURCE=1",
                document.relative_path,
                expected,
                source.content_hash
            ),
            Some(_) => {}
            None => {
                // scip-typescript never writes index.sources.json and leaves
                // Document.text empty, so cannot be hash-validated. Trust the
                // compiler index for missing entries but report it, never silently.
                *missing_manifest += 1;
            }
        }
    }
    Ok(Some(source))
}
fn containing(nodes: &[CodeNode], indices: &[usize], byte: usize) -> Option<usize> {
    let mut candidates: Vec<_> = indices
        .iter()
        .copied()
        .filter(|index| {
            nodes[*index].span.as_ref().is_some_and(|span| {
                span.start_byte as usize <= byte && byte < span.end_byte as usize
            })
        })
        .collect();
    candidates.sort_by_key(|index| {
        let span = nodes[*index].span.as_ref().unwrap();
        span.end_byte - span.start_byte
    });
    match candidates.as_slice() {
        [first] => Some(*first),
        [first, second, ..]
            if nodes[*first].span.as_ref().unwrap().end_byte
                - nodes[*first].span.as_ref().unwrap().start_byte
                != nodes[*second].span.as_ref().unwrap().end_byte
                    - nodes[*second].span.as_ref().unwrap().start_byte =>
        {
            Some(*first)
        }
        _ => None,
    }
}

pub fn import(
    path: &Path,
    path_prefix: Option<&str>,
    root: &Path,
    sources: &SourceSnapshots,
    nodes: &mut [CodeNode],
    edges: &mut Vec<EdgeRelationship>,
    references: bool,
) -> Result<ImportStats> {
    let manifest: BTreeMap<String, String> =
        match std::fs::read(path.with_extension("sources.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
    let trust_source = std::env::var("CODEGRAPH_SCIP_TRUST_SOURCE").as_deref() == Ok("1");
    // Index snapshots by the same project-relative posix key used for nodes. Raw
    // `sources.get(root.join(relative_path))` silently missed every document when the
    // indexer walked `.` (snapshot keys `./src/a.ts`) or when SCIP paths used `\`.
    let mut by_relative = BTreeMap::<String, &SourceSnapshot>::new();
    for snapshot in sources.iter() {
        by_relative
            .entry(relative_key(root, &snapshot.path))
            .or_insert(snapshot);
    }
    let mut by_file = BTreeMap::<String, Vec<usize>>::new();
    for (index, node) in nodes.iter().enumerate() {
        by_file
            .entry(relative_key(root, Path::new(&node.location.file_path)))
            .or_default()
            .push(index);
    }
    let mut stats = ImportStats {
        documents: 0,
        ..Default::default()
    };
    let mut missing_manifest = 0usize;
    let mut unmatched = 0usize;
    let mut unmatched_example: Option<String> = None;
    let mut enriched = BTreeSet::<usize>::new();
    let mut definitions = BTreeMap::<String, BTreeSet<NodeId>>::new();
    warn_if_index_predates_sources(path, sources);
    documents(path, |document| {
        stats.documents += 1;
        let Some(snapshot) = source(
            &document,
            path_prefix,
            &by_relative,
            &manifest,
            trust_source,
            &mut missing_manifest,
        )?
        else {
            unmatched += 1;
            unmatched_example.get_or_insert_with(|| document.relative_path.clone());
            return Ok(());
        };
        stats.documents_matched += 1;
        let text = snapshot.contents()?;
        let starts: Vec<_> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        let file_key = relative_key(root, &snapshot.path);
        stats.covered_files.insert(file_key.clone());
        let Some(indices) = by_file.get(&file_key) else {
            return Ok(());
        };
        stats.documents_with_nodes += 1;
        let docs: BTreeMap<_, _> = document
            .symbols
            .iter()
            .map(|info| (info.symbol.as_str(), &info.documentation))
            .collect();
        for occurrence in &document.occurrences {
            if occurrence.symbol.is_empty() || occurrence.symbol_roles & 1 == 0 {
                continue;
            }
            let range = range(occurrence)?;
            let encoding = document.position_encoding.value();
            let byte =
                byte_position(&text, &starts, range[0], range[1], encoding).with_context(|| {
                    position_error_context(&document.relative_path, &range, encoding)
                })?;
            let end =
                byte_position(&text, &starts, range[2], range[3], encoding).with_context(|| {
                    position_error_context(&document.relative_path, &range, encoding)
                })?;
            let name = text[byte..end].trim_start_matches("r#");
            let matching: Vec<_> = indices
                .iter()
                .copied()
                .filter(|index| nodes[*index].name.as_str().rsplit("::").next() == Some(name))
                .collect();
            if let Some(index) = containing(nodes, &matching, byte) {
                definitions
                    .entry(symbol_key(&document.relative_path, &occurrence.symbol))
                    .or_default()
                    .insert(nodes[index].id);
                nodes[index]
                    .metadata
                    .attributes
                    .insert("scip_symbol".into(), occurrence.symbol.clone());
                enriched.insert(index);
                if let Some(documentation) = docs.get(occurrence.symbol.as_str())
                    && !documentation.is_empty()
                {
                    nodes[index]
                        .metadata
                        .attributes
                        .insert("doc".into(), documentation.join("\n\n"));
                }
            }
        }
        Ok(())
    })?;
    // Index existing edges by (caller, start byte): SCIP emits millions of reference
    // occurrences, and rescanning every edge per occurrence is quadratic.
    let mut edges_by_caller_start = BTreeMap::<(NodeId, u32), Vec<usize>>::new();
    let mut edges_by_caller = BTreeMap::<NodeId, Vec<usize>>::new();
    for (index, edge) in edges.iter().enumerate() {
        edges_by_caller.entry(edge.from).or_default().push(index);
        if let Some(span) = &edge.span {
            edges_by_caller_start
                .entry((edge.from, span.start_byte))
                .or_default()
                .push(index);
        }
    }
    let mut resolved = 0;
    let mut second_pass_missing_manifest = 0usize;
    documents(path, |document| {
        let Some(snapshot) = source(
            &document,
            path_prefix,
            &by_relative,
            &manifest,
            trust_source,
            &mut second_pass_missing_manifest,
        )?
        else {
            return Ok(());
        };
        let text = snapshot.contents()?;
        let starts: Vec<_> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        let file_key = relative_key(root, &snapshot.path);
        let Some(indices) = by_file.get(&file_key) else {
            return Ok(());
        };
        for occurrence in &document.occurrences {
            if occurrence.symbol.is_empty() || occurrence.symbol_roles & 1 != 0 {
                continue;
            }
            let Some(targets) =
                definitions.get(&symbol_key(&document.relative_path, &occurrence.symbol))
            else {
                continue;
            };
            if targets.len() != 1 {
                continue;
            }
            let target = *targets.first().unwrap();
            let range = range(occurrence)?;
            let encoding = document.position_encoding.value();
            let start =
                byte_position(&text, &starts, range[0], range[1], encoding).with_context(|| {
                    position_error_context(&document.relative_path, &range, encoding)
                })?;
            let end =
                byte_position(&text, &starts, range[2], range[3], encoding).with_context(|| {
                    position_error_context(&document.relative_path, &range, encoding)
                })?;
            let Some(caller) = containing(nodes, indices, start) else {
                continue;
            };
            let mut matched = false;
            let name = text[start..end].trim_start_matches("r#");
            let candidates = edges_by_caller_start
                .get(&(nodes[caller].id, start as u32))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            for &index in candidates {
                let edge = &mut edges[index];
                if edge
                    .to
                    .rsplit([':', '.'])
                    .next()
                    .map(|name| name.trim_start_matches("r#"))
                    != Some(name)
                    || !edge
                        .span
                        .as_ref()
                        .is_some_and(|span| end <= span.end_byte as usize)
                {
                    continue;
                }
                edge.metadata
                    .insert("target_node_id".into(), target.to_string());
                edge.metadata.insert("analyzer".into(), "scip".into());
                edge.metadata
                    .insert("analyzer_confidence".into(), "1.0".into());
                matched = true;
                resolved += 1;
            }
            // Member/receiver calls record the callee identifier inside a wider span that
            // starts at the receiver (`obj.method()`), so the exact start-byte index misses
            // them. Fall back to the narrowest same-name span containing the identifier.
            if !matched && let Some(candidates) = edges_by_caller.get(&nodes[caller].id) {
                let mut ranked: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|&index| {
                        let edge = &edges[index];
                        edge.to
                            .rsplit([':', '.'])
                            .next()
                            .map(|value| value.trim_start_matches("r#"))
                            == Some(name)
                            && edge.span.as_ref().is_some_and(|span| {
                                span.start_byte as usize <= start && end <= span.end_byte as usize
                            })
                    })
                    .collect();
                ranked.sort_by_key(|&index| {
                    edges[index]
                        .span
                        .as_ref()
                        .map_or(u32::MAX, |span| span.end_byte - span.start_byte)
                });
                if let Some(&index) = ranked.first() {
                    let edge = &mut edges[index];
                    edge.metadata
                        .insert("target_node_id".into(), target.to_string());
                    edge.metadata.insert("analyzer".into(), "scip".into());
                    edge.metadata
                        .insert("analyzer_confidence".into(), "1.0".into());
                    matched = true;
                    resolved += 1;
                }
            }
            if !matched && references {
                edges.push(EdgeRelationship {
                    from: nodes[caller].id,
                    to: occurrence.symbol.clone(),
                    edge_type: EdgeType::References,
                    metadata: std::collections::HashMap::from([
                        ("target_node_id".into(), target.to_string()),
                        ("analyzer".into(), "scip".into()),
                        (
                            "source_file".into(),
                            snapshot.path.to_string_lossy().into_owned(),
                        ),
                    ]),
                    span: Some(Span {
                        start_byte: start as u32,
                        end_byte: end as u32,
                    }),
                });
                resolved += 1;
            }
        }
        Ok(())
    })?;
    stats.nodes_enriched = enriched.len();
    stats.edges_resolved = resolved;
    if stats.documents == 0 {
        bail!(
            "SCIP index {} contains no documents; regenerate it with a supported SCIP indexer",
            path.display()
        );
    }
    if stats.documents_matched == 0 {
        bail!(
            "SCIP index {} matched 0 of {} documents to indexed sources; the artifact was built against a different project root or path layout",
            path.display(),
            stats.documents
        );
    }
    if missing_manifest > 0 {
        warn!(
            "SCIP: {} of {} documents have no index.sources.json hash; trusting the compiler index for them (set CODEGRAPH_SCIP_TRUST_SOURCE=1 to silence)",
            missing_manifest, stats.documents
        );
    }
    if unmatched > 0 {
        warn!(
            "SCIP: {} of {} documents had no indexed source snapshot and were skipped (first: {})",
            unmatched,
            stats.documents,
            unmatched_example.as_deref().unwrap_or("<unknown>")
        );
    }
    debug!(
        "SCIP import: documents={} matched={} with_nodes={} nodes_enriched={} edges_resolved={}",
        stats.documents,
        stats.documents_matched,
        stats.documents_with_nodes,
        stats.nodes_enriched,
        stats.edges_resolved
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;

    #[test]
    fn scip_max_bytes_is_opt_in_and_parsed() {
        assert_eq!(max_bytes_from_env(None).unwrap(), 0);
        assert_eq!(max_bytes_from_env(Some("")).unwrap(), 0);
        assert_eq!(max_bytes_from_env(Some(" 0 ")).unwrap(), 0);
        assert_eq!(max_bytes_from_env(Some("1048576")).unwrap(), 1 << 20);
        assert!(max_bytes_from_env(Some("512MiB")).is_err());
    }

    /// Documents are decoded one at a time, so an artifact larger than the legacy
    /// 512 MiB threshold must import; only an explicit positive limit rejects it.
    #[test]
    fn documents_stream_without_a_default_size_cap() -> Result<()> {
        let root = tempfile::tempdir()?;
        let artifact = root.path().join("index.scip");
        let index = scip::types::Index {
            documents: vec![
                Document {
                    relative_path: "a.rs".into(),
                    ..Default::default()
                },
                Document {
                    relative_path: "b.rs".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        std::fs::write(&artifact, index.write_to_bytes()?)?;
        let len = std::fs::metadata(&artifact)?.len();
        let mut seen = Vec::new();
        documents_with_limit(&artifact, 0, |document| {
            seen.push(document.relative_path);
            Ok(())
        })?;
        assert_eq!(seen, ["a.rs", "b.rs"]);
        let mut count = 0;
        documents_with_limit(&artifact, len, |_| {
            count += 1;
            Ok(())
        })?;
        assert_eq!(count, 2);
        assert!(documents_with_limit(&artifact, len - 1, |_| Ok(())).is_err());
        Ok(())
    }

    #[test]
    fn unicode_positions_reject_split_surrogates_and_unspecified_encoding() {
        let text = "🚀 café\n";
        assert_eq!(byte_position(text, &[0, text.len()], 0, 3, 2).unwrap(), 5);
        assert!(byte_position(text, &[0], 0, 1, 2).is_err());
        assert!(byte_position(text, &[0], 0, 1, 1).is_err());
        assert!(byte_position(text, &[0], 0, 0, 0).is_err());
        assert_ne!(symbol_key("a.rs", "local 0"), symbol_key("b.rs", "local 0"));
    }

    #[test]
    fn position_context_names_path_and_encoding() {
        let message = position_error_context("src/lib.rs", &[10, 2, 10, 8], 1);
        assert!(message.contains("src/lib.rs:10:2"), "{message}");
        assert!(message.contains("encoding 1"), "{message}");
        assert!(message.contains("stale"), "{message}");
    }

    #[test]
    fn position_context_includes_end_position() {
        let message = position_error_context("src/lib.rs", &[3, 0, 5, 4], 1);
        assert!(message.contains("end 5:4"), "{message}");
    }

    #[tokio::test]
    async fn stale_warning_does_not_panic_without_index() {
        let sources = SourceSnapshots::capture(&[], 1024, 1).await.unwrap();
        warn_if_index_predates_sources(Path::new("/nonexistent/x.scip"), &sources);
    }

    #[tokio::test]
    async fn compiler_definitions_bind_calls_and_stale_artifacts_fail() -> Result<()> {
        use codegraph_core::{Language, Location, NodeType};
        let root = tempfile::tempdir()?;
        let path = root.path().join("a.rs");
        let text = "fn target() {}\nfn caller() { target(); }\n";
        std::fs::write(&path, text)?;
        let sources =
            SourceSnapshots::capture(&[(path.clone(), text.len() as u64)], 1024, 1).await?;
        let make_node = |name: &str, line, start, end| {
            let mut node = CodeNode::new(
                name,
                Some(NodeType::Function),
                Some(Language::Rust),
                Location {
                    file_path: path.to_string_lossy().into_owned(),
                    line,
                    column: 0,
                    end_line: Some(line),
                    end_column: None,
                },
            );
            node.span = Some(Span {
                start_byte: start,
                end_byte: end,
            });
            node
        };
        let mut nodes = vec![
            make_node("target", 1, 0, 14),
            make_node("caller", 2, 15, text.len() as u32),
        ];
        let mut edges = vec![EdgeRelationship {
            from: nodes[1].id,
            to: "target".into(),
            edge_type: EdgeType::Calls,
            metadata: Default::default(),
            span: Some(Span {
                start_byte: 29,
                end_byte: 37,
            }),
        }];
        let document = Document {
            relative_path: "a.rs".into(),
            text: text.into(),
            position_encoding: scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart
                .into(),
            occurrences: vec![
                Occurrence {
                    symbol: "compiler target".into(),
                    symbol_roles: 1,
                    range: vec![0, 3, 9],
                    ..Default::default()
                },
                Occurrence {
                    symbol: "compiler target".into(),
                    range: vec![1, 14, 20],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let artifact = root.path().join("index.scip");
        let mut index = scip::types::Index {
            documents: vec![document],
            ..Default::default()
        };
        std::fs::write(&artifact, index.write_to_bytes()?)?;
        assert_eq!(
            import(
                &artifact,
                None,
                root.path(),
                &sources,
                &mut nodes,
                &mut edges,
                false
            )?,
            ImportStats {
                documents: 1,
                documents_matched: 1,
                documents_with_nodes: 1,
                nodes_enriched: 1,
                edges_resolved: 1,
                covered_files: BTreeSet::from(["a.rs".to_string()]),
            }
        );
        assert_eq!(edges[0].metadata["target_node_id"], nodes[0].id.to_string());
        index.documents[0].text = "old source".into();
        std::fs::write(&artifact, index.write_to_bytes()?)?;
        assert!(
            import(
                &artifact,
                None,
                root.path(),
                &sources,
                &mut nodes,
                &mut edges,
                false
            )
            .is_err()
        );
        Ok(())
    }

    /// scip-typescript writes empty `Document.text` and no `index.sources.json`, while
    /// nodes carry project-relative paths. The importer must still bind against the
    /// captured snapshots instead of silently returning zero.
    #[tokio::test]
    async fn project_relative_paths_without_manifest_still_import() -> Result<()> {
        use codegraph_core::{Language, Location, NodeType};
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("src"))?;
        let path = root.path().join("src/a.ts");
        let text = "export function target() {}\nexport function caller() { target(); }\n";
        std::fs::write(&path, text)?;
        let sources =
            SourceSnapshots::capture(&[(path.clone(), text.len() as u64)], 1024, 1).await?;
        let make_node = |name: &str, start, end| {
            let mut node = CodeNode::new(
                name,
                Some(NodeType::Function),
                Some(Language::TypeScript),
                Location {
                    file_path: "src/a.ts".into(),
                    line: 1,
                    column: 0,
                    end_line: Some(1),
                    end_column: None,
                },
            );
            node.span = Some(Span {
                start_byte: start,
                end_byte: end,
            });
            node
        };
        let target_start = text.find("target").unwrap() as u32;
        let call_start = text.rfind("target").unwrap() as u32;
        let second_line = text.find('\n').unwrap() as u32 + 1;
        let mut nodes = vec![
            make_node("target", target_start, target_start + 6),
            make_node("caller", second_line, text.len() as u32),
        ];
        // The span starts at the line, not at the identifier, mirroring receiver calls.
        let mut edges = vec![EdgeRelationship {
            from: nodes[1].id,
            to: "target".into(),
            edge_type: EdgeType::Calls,
            metadata: Default::default(),
            span: Some(Span {
                start_byte: second_line,
                end_byte: text.len() as u32,
            }),
        }];
        let line = text[..call_start as usize].matches('\n').count() as i32;
        let column =
            (call_start as usize) - text[..call_start as usize].rfind('\n').map_or(0, |i| i + 1);
        let document = Document {
            relative_path: "src/a.ts".into(),
            position_encoding: scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart
                .into(),
            occurrences: vec![
                Occurrence {
                    symbol: "scip typescript . src/a.ts target".into(),
                    symbol_roles: 1,
                    range: vec![
                        0,
                        text.find("target").unwrap() as i32,
                        text.find("target").unwrap() as i32 + 6,
                    ],
                    ..Default::default()
                },
                Occurrence {
                    symbol: "scip typescript . src/a.ts target".into(),
                    range: vec![line, column as i32, column as i32 + 6],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let artifact = root.path().join("index.scip");
        let index = scip::types::Index {
            documents: vec![document],
            ..Default::default()
        };
        std::fs::write(&artifact, index.write_to_bytes()?)?;
        let stats = import(
            &artifact,
            None,
            root.path(),
            &sources,
            &mut nodes,
            &mut edges,
            false,
        )?;
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.documents_matched, 1);
        assert_eq!(stats.documents_with_nodes, 1);
        assert_eq!(stats.edges_resolved, 1);
        assert_eq!(edges[0].metadata["target_node_id"], nodes[0].id.to_string());

        // A manifest that exists but disagrees with the captured source is stale.
        std::fs::write(
            root.path().join("index.sources.json"),
            serde_json::to_vec(&BTreeMap::from([(
                "src/a.ts".to_string(),
                "deadbeef".to_string(),
            )]))?,
        )?;
        assert!(
            import(
                &artifact,
                None,
                root.path(),
                &sources,
                &mut nodes,
                &mut edges,
                false
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn document_key_collapses_join_artifacts() {
        assert_eq!(document_key("a.rs"), "a.rs");
        assert_eq!(document_key("./a.rs"), "a.rs");
        assert_eq!(document_key("a//b.rs"), "a/b.rs");
        assert_eq!(document_key("a/./b.rs"), "a/b.rs");
        assert_eq!(document_key("a\\b.rs"), "a/b.rs");
        assert_eq!(
            joined_document_path("host.go", None),
            PathBuf::from("host.go")
        );
        assert_eq!(
            joined_document_path("host.go", Some("easytier-go")),
            PathBuf::from("easytier-go/host.go")
        );
        assert_eq!(
            joined_document_path("./host.go", Some("easytier-go/")),
            PathBuf::from("easytier-go/./host.go")
        );
    }

    /// A Go index generated inside `easytier-go/` records `host.go`, while the
    /// project-relative node path is `easytier-go/host.go`. The prefix must be
    /// joined before normalisation or nothing matches.
    #[tokio::test]
    async fn path_prefix_binds_documents_from_a_subdirectory_index() -> Result<()> {
        use codegraph_core::{Language, Location, NodeType};
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("easytier-go"))?;
        let path = root.path().join("easytier-go/host.go");
        let text = "package main\nfunc target() {}\nfunc caller() { target() }\n";
        std::fs::write(&path, text)?;
        let sources =
            SourceSnapshots::capture(&[(path.clone(), text.len() as u64)], 1024, 1).await?;
        let make_node = |name: &str, start, end| {
            let mut node = CodeNode::new(
                name,
                Some(NodeType::Function),
                Some(Language::Go),
                Location {
                    file_path: "easytier-go/host.go".into(),
                    line: 2,
                    column: 0,
                    end_line: Some(2),
                    end_column: None,
                },
            );
            node.span = Some(Span {
                start_byte: start,
                end_byte: end,
            });
            node
        };
        let target_start = text.find("target() {}").unwrap() as u32;
        let call_start = text.rfind("target() }").unwrap() as u32;
        let mut nodes = vec![
            make_node("target", target_start, target_start + 6),
            make_node(
                "caller",
                text.find("func caller").unwrap() as u32,
                text.len() as u32,
            ),
        ];
        let mut edges = vec![EdgeRelationship {
            from: nodes[1].id,
            to: "target".into(),
            edge_type: EdgeType::Calls,
            metadata: Default::default(),
            span: Some(Span {
                start_byte: call_start,
                end_byte: call_start + 8,
            }),
        }];
        let line_start = text[..call_start as usize].rfind('\n').unwrap() + 1;
        let column = (call_start as usize - line_start) as i32;
        let line = text[..call_start as usize].matches('\n').count() as i32;
        let document = Document {
            relative_path: "host.go".into(),
            position_encoding: scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart
                .into(),
            occurrences: vec![
                Occurrence {
                    symbol: "scip go . host.go target".into(),
                    symbol_roles: 1,
                    range: vec![1, 5, 11],
                    ..Default::default()
                },
                Occurrence {
                    symbol: "scip go . host.go target".into(),
                    range: vec![line, column, column + 6],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let artifact = root.path().join("go.scip");
        let index = scip::types::Index {
            documents: vec![document],
            ..Default::default()
        };
        std::fs::write(&artifact, index.write_to_bytes()?)?;

        // Without the prefix the document cannot be matched.
        let mut unmatched_nodes = nodes.clone();
        let mut unmatched_edges = edges.clone();
        let unmatched = import(
            &artifact,
            None,
            root.path(),
            &sources,
            &mut unmatched_nodes,
            &mut unmatched_edges,
            false,
        );
        assert!(unmatched.is_err());

        let stats = import(
            &artifact,
            Some("easytier-go"),
            root.path(),
            &sources,
            &mut nodes,
            &mut edges,
            false,
        )?;
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.documents_matched, 1);
        assert_eq!(stats.documents_with_nodes, 1);
        assert_eq!(stats.nodes_enriched, 1);
        assert_eq!(stats.edges_resolved, 1);
        assert_eq!(edges[0].metadata["target_node_id"], nodes[0].id.to_string());
        Ok(())
    }
}
