// ABOUTME: Deterministic, context-aware symbol catalog used before semantic inference.
// ABOUTME: Preserves ambiguous aliases and indexes lexical candidates by character trigrams.

use codegraph_core::{CodeNode, EdgeRelationship, NodeId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Clone)]
struct Symbol {
    id: NodeId,
    file: String,
    qualified: String,
}

struct LexicalCandidate {
    alias: String,
    normalized: String,
    #[cfg(any(test, feature = "ai-enhanced"))]
    gram_count: usize,
    characters: usize,
}

/// A symbol loaded from storage to seed a partial re-index's resolution catalog.
#[derive(Debug, Clone)]
pub(crate) struct ExternalSymbol {
    pub id: NodeId,
    pub file: String,
    pub name: String,
    pub kind: Option<String>,
    pub qualified: Option<String>,
}

/// Diagnostic counters for the fuzzy fallback, aggregated across the parallel
/// resolution loop. `nanos` sums per-call wall time over every rayon worker, so
/// it is total CPU time spent in `fuzzy`, not elapsed wall time.
///
/// Phase timings are recorded once per `fuzzy` call (never per candidate): the
/// timer itself would otherwise dominate the sub-nanosecond work it measures.
/// `lev_nanos` is the whole traversal/scoring loop, which is where the
/// Levenshtein calls happen; the loop also runs the length prune and `choose`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FuzzyStats {
    pub calls: u64,
    pub nanos: u64,
    pub candidates: u64,
    /// Candidates dropped by the length-ratio prune inside
    /// `ranked_candidate_indices`, before the shortlist cap is applied.
    pub len_pruned: u64,
    /// Attempts to score a candidate with `normalized_similarity_with_args`.
    pub levenshtein_calls: u64,
    /// Scores at or above the current `best` that reached `choose()`.
    pub above_cutoff: u64,
    pub trigram_nanos: u64,
    pub counts_nanos: u64,
    pub lev_nanos: u64,
    /// Effective candidate cap; `0` means unlimited.
    pub limit: usize,
}

pub(crate) struct SymbolCatalog {
    aliases: BTreeMap<String, Vec<NodeId>>,
    symbols: HashMap<NodeId, Symbol>,
    lexical: Vec<LexicalCandidate>,
    trigrams: HashMap<String, Vec<usize>>,
    fuzzy_calls: AtomicU64,
    fuzzy_nanos: AtomicU64,
    fuzzy_candidates: AtomicU64,
    fuzzy_len_pruned: AtomicU64,
    fuzzy_levenshtein_calls: AtomicU64,
    fuzzy_above_cutoff: AtomicU64,
    fuzzy_trigram_nanos: AtomicU64,
    fuzzy_counts_nanos: AtomicU64,
    fuzzy_lev_nanos: AtomicU64,
    fuzzy_limit: usize,
}

pub(crate) fn aliases(node: &CodeNode) -> BTreeSet<String> {
    let kind = node.node_type.as_ref().map(|kind| format!("{kind:?}"));
    let mut names = aliases_for(
        &node.location.file_path,
        node.name.as_str(),
        kind.as_deref(),
        node.metadata
            .attributes
            .get("qualified_name")
            .map(String::as_str),
    );
    for attribute in ["method_of", "implements_trait"] {
        if let Some(owner) = node.metadata.attributes.get(attribute) {
            names.insert(format!("{owner}::{}", node.name));
        }
    }
    names
}

/// Aliases derivable from stored symbol columns, used for external symbols loaded
/// from the project catalog instead of a parsed `CodeNode`.
pub(crate) fn aliases_for(
    file: &str,
    name: &str,
    kind: Option<&str>,
    qualified: Option<&str>,
) -> BTreeSet<String> {
    let mut names = BTreeSet::from([name.to_string(), format!("{file}::{name}")]);
    if let Some(qualified) = qualified {
        names.insert(qualified.to_string());
    }
    if let Some(kind) = kind {
        names.insert(format!("{kind}::{name}"));
    }
    if let Some(short) = name.rsplit("::").next() {
        names.insert(short.to_string());
    }
    names
}

fn trigrams(name: &str) -> BTreeSet<String> {
    let chars: Vec<_> = name.chars().collect();
    if chars.len() < 3 {
        return BTreeSet::from([name.to_string()]);
    }
    chars
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

/// Upper bound on how many trigram-overlap candidates `fuzzy` scores per lookup.
///
/// `0` keeps every candidate that survives the length-ratio prune. The cap is
/// opt-in through `CODEGRAPH_FUZZY_CANDIDATE_LIMIT`: on the 1.9M-relationship
/// paperclip catalog a hard 256 cap dropped 25,970 otherwise-resolvable edges,
/// so the default must not truncate.
fn fuzzy_limit_for(value: Option<&str>) -> usize {
    value
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0)
}

/// Resolve the effective fuzzy candidate cap from the environment.
///
/// Unset, empty and `0` all mean "unlimited". A value that fails to parse also
/// degrades to unlimited, with a debug note, rather than failing the index.
fn configured_fuzzy_limit() -> usize {
    let value = std::env::var("CODEGRAPH_FUZZY_CANDIDATE_LIMIT").ok();
    let limit = fuzzy_limit_for(value.as_deref());
    if let Some(raw) = value.as_deref().map(str::trim)
        && !raw.is_empty()
        && raw.parse::<usize>().is_err()
    {
        tracing::debug!(
            "CODEGRAPH_FUZZY_CANDIDATE_LIMIT={raw:?} is not a non-negative integer; \
             fuzzy candidate cap disabled (0 = unlimited)"
        );
    }
    limit
}

impl SymbolCatalog {
    pub(crate) fn new(nodes: &[CodeNode]) -> Self {
        Self::build(nodes, &[])
    }

    /// Build the catalog with symbols loaded from storage appended.
    ///
    /// A partial re-index only parses the scanned files, so its edges can only resolve
    /// if the rest of the project's symbols are available as a base.
    pub(crate) fn new_with_external(nodes: &[CodeNode], external: &[ExternalSymbol]) -> Self {
        Self::build(nodes, external)
    }

    fn build(nodes: &[CodeNode], external: &[ExternalSymbol]) -> Self {
        let mut catalog = Self {
            aliases: BTreeMap::new(),
            symbols: HashMap::new(),
            lexical: Vec::new(),
            trigrams: HashMap::new(),
            fuzzy_calls: AtomicU64::new(0),
            fuzzy_nanos: AtomicU64::new(0),
            fuzzy_candidates: AtomicU64::new(0),
            fuzzy_len_pruned: AtomicU64::new(0),
            fuzzy_levenshtein_calls: AtomicU64::new(0),
            fuzzy_above_cutoff: AtomicU64::new(0),
            fuzzy_trigram_nanos: AtomicU64::new(0),
            fuzzy_counts_nanos: AtomicU64::new(0),
            fuzzy_lev_nanos: AtomicU64::new(0),
            fuzzy_limit: configured_fuzzy_limit(),
        };
        for node in nodes {
            catalog.insert_symbol(
                node.id,
                &node.location.file_path,
                node.name.as_str(),
                node.metadata
                    .attributes
                    .get("qualified_name")
                    .map(String::as_str),
            );
            for alias in aliases(node) {
                catalog.add_alias(alias, node.id);
            }
        }
        for symbol in external {
            let seen = catalog.symbols.contains_key(&symbol.id);
            catalog.insert_symbol(
                symbol.id,
                &symbol.file,
                &symbol.name,
                symbol.qualified.as_deref(),
            );
            if !seen {
                for alias in aliases_for(
                    &symbol.file,
                    &symbol.name,
                    symbol.kind.as_deref(),
                    symbol.qualified.as_deref(),
                ) {
                    catalog.add_alias(alias, symbol.id);
                }
            }
        }
        catalog.finalize()
    }

    fn insert_symbol(&mut self, id: NodeId, file: &str, name: &str, qualified: Option<&str>) {
        self.symbols.insert(
            id,
            Symbol {
                id,
                file: file.to_string(),
                qualified: qualified
                    .map(str::to_string)
                    .unwrap_or_else(|| name.to_string()),
            },
        );
    }

    fn add_alias(&mut self, alias: String, id: NodeId) {
        let ids = self.aliases.entry(alias).or_default();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    fn finalize(mut self) -> Self {
        for ids in self.aliases.values_mut() {
            ids.sort();
        }
        for alias in self.aliases.keys() {
            let normalized = alias.to_lowercase();
            let index = self.lexical.len();
            let grams = trigrams(&normalized);
            for gram in &grams {
                self.trigrams.entry(gram.clone()).or_default().push(index);
            }
            self.lexical.push(LexicalCandidate {
                alias: alias.clone(),
                characters: normalized.chars().count(),
                normalized,
                #[cfg(any(test, feature = "ai-enhanced"))]
                gram_count: grams.len(),
            });
        }
        self
    }

    /// Legacy consumers receive only aliases with exactly one target.
    pub(crate) fn unique_aliases(&self) -> HashMap<String, NodeId> {
        self.aliases
            .iter()
            .filter(|(_, ids)| ids.len() == 1)
            .map(|(alias, ids)| (alias.clone(), ids[0]))
            .collect()
    }

    pub(crate) fn ambiguous(&self, alias: &str) -> bool {
        self.aliases.get(alias).is_some_and(|ids| ids.len() > 1)
    }

    #[cfg(any(test, feature = "ai-enhanced"))]
    pub(crate) fn semantic_candidates(&self, target: &str, limit: usize) -> Vec<String> {
        let target = target.to_lowercase();
        let grams = trigrams(&target);
        let target_len = target.chars().count().max(1) as f32;
        self.candidate_indices(&grams, limit)
            .into_iter()
            .filter_map(|(index, common)| {
                let candidate = &self.lexical[index];
                if self.aliases[&candidate.alias].len() != 1 {
                    return None;
                }
                let name_len = candidate.characters.max(1) as f32;
                if (target_len / name_len).min(name_len / target_len) < 0.5 {
                    return None;
                }
                let union = grams.len() + candidate.gram_count - common;
                (common as f32 / union.max(1) as f32 >= 0.2).then(|| candidate.alias.clone())
            })
            .collect()
    }

    fn choose(&self, ids: &[NodeId], from: NodeId) -> Option<NodeId> {
        if ids.len() == 1 {
            return ids.first().copied();
        }
        let source = self.symbols.get(&from)?;
        let local: Vec<_> = ids
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| symbol.file == source.file)
            .collect();
        if local.len() == 1 {
            return Some(local[0].id);
        }
        let source_scope = source.qualified.rsplit_once("::").map(|(scope, _)| scope)?;
        let scoped: Vec<_> = local
            .iter()
            .filter(|symbol| {
                symbol.qualified.rsplit_once("::").map(|(scope, _)| scope) == Some(source_scope)
            })
            .collect();
        (scoped.len() == 1).then(|| scoped[0].id)
    }

    pub(crate) fn exact(&self, edge: &EdgeRelationship, variants: &[String]) -> Option<NodeId> {
        if let Some(id) = edge
            .metadata
            .get("target_node_id")
            .and_then(|id| NodeId::parse_str(id).ok())
            && self.symbols.contains_key(&id)
        {
            return Some(id);
        }
        for alias in std::iter::once(edge.to.as_str()).chain(variants.iter().map(String::as_str)) {
            if let Some(ids) = self.aliases.get(alias)
                && let Some(id) = self.choose(ids, edge.from)
            {
                return Some(id);
            }
        }
        None
    }

    /// Number of shared trigrams per catalog entry. Shared by the sorted shortlist
    /// and the unsorted fuzzy shortlist.
    fn candidate_counts(&self, grams: &BTreeSet<String>) -> HashMap<usize, usize> {
        let mut counts = HashMap::<usize, usize>::new();
        for gram in grams {
            if let Some(indices) = self.trigrams.get(gram) {
                for index in indices {
                    *counts.entry(*index).or_default() += 1;
                }
            }
        }
        counts
    }

    /// A deterministic shortlist; a zero limit retains every overlapping candidate.
    fn candidate_indices(&self, grams: &BTreeSet<String>, limit: usize) -> Vec<(usize, usize)> {
        let mut ranked: Vec<_> = self.candidate_counts(grams).into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
            .into_iter()
            .take(if limit == 0 { usize::MAX } else { limit })
            .collect()
    }

    /// The fuzzy shortlist: highest trigram overlap first, capped at `limit`.
    ///
    /// `select_nth_unstable_by` splits off the top `limit` without sorting the
    /// rest; only the retained window is sorted, so the cap turns an O(n log n)
    /// full sort into O(n). The comparison is a total order (count desc, index
    /// asc), so the retained set is the same one a full sort would keep.
    ///
    /// Candidates whose length ratio cannot reach `min_ratio` are dropped before
    /// the cap is applied: edit-distance similarity is bounded by the ratio of
    /// the shorter to the longer string, so such a candidate can never beat the
    /// initial `best` and would only consume a shortlist slot.
    fn ranked_candidate_indices(
        &self,
        grams: &BTreeSet<String>,
        target_length: usize,
        min_ratio: f64,
        limit: usize,
    ) -> Vec<(usize, usize)> {
        let counts = self.candidate_counts(grams);
        let overlapping = counts.len();
        let mut ranked: Vec<_> = counts
            .into_iter()
            .filter(|(index, _)| {
                let candidate = &self.lexical[*index];
                (target_length.min(candidate.characters) as f64)
                    / (target_length.max(candidate.characters).max(1) as f64)
                    + f64::EPSILON
                    >= min_ratio
            })
            .collect();
        if ranked.len() < overlapping {
            self.fuzzy_len_pruned
                .fetch_add((overlapping - ranked.len()) as u64, Ordering::Relaxed);
        }
        if limit != 0 && ranked.len() > limit {
            ranked.select_nth_unstable_by(limit, |a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked.truncate(limit);
        }
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
    }

    pub(crate) fn fuzzy_stats(&self) -> FuzzyStats {
        FuzzyStats {
            calls: self.fuzzy_calls.load(Ordering::Relaxed),
            nanos: self.fuzzy_nanos.load(Ordering::Relaxed),
            candidates: self.fuzzy_candidates.load(Ordering::Relaxed),
            len_pruned: self.fuzzy_len_pruned.load(Ordering::Relaxed),
            levenshtein_calls: self.fuzzy_levenshtein_calls.load(Ordering::Relaxed),
            above_cutoff: self.fuzzy_above_cutoff.load(Ordering::Relaxed),
            trigram_nanos: self.fuzzy_trigram_nanos.load(Ordering::Relaxed),
            counts_nanos: self.fuzzy_counts_nanos.load(Ordering::Relaxed),
            lev_nanos: self.fuzzy_lev_nanos.load(Ordering::Relaxed),
            limit: self.fuzzy_limit,
        }
    }

    #[cfg(test)]
    pub(crate) fn candidates(&self, target: &str, limit: usize) -> Vec<String> {
        self.candidate_indices(&trigrams(&target.to_lowercase()), limit)
            .into_iter()
            .map(|(index, _)| self.lexical[index].alias.clone())
            .collect()
    }

    pub(crate) fn fuzzy(&self, edge: &EdgeRelationship) -> Option<(NodeId, f64)> {
        let started = Instant::now();
        self.fuzzy_calls.fetch_add(1, Ordering::Relaxed);
        let target = edge.to.to_lowercase();
        let target_length = target.chars().count();
        let scorer = rapidfuzz::distance::levenshtein::BatchComparator::new(target.chars());
        let mut best = 0.85f64;
        let mut matches = HashSet::new();
        let trigram_started = Instant::now();
        let grams = trigrams(&target);
        self.fuzzy_trigram_nanos.fetch_add(
            trigram_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
        let counts_started = Instant::now();
        let candidates =
            self.ranked_candidate_indices(&grams, target_length, best, self.fuzzy_limit);
        self.fuzzy_counts_nanos.fetch_add(
            counts_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
        self.fuzzy_candidates
            .fetch_add(candidates.len() as u64, Ordering::Relaxed);
        let lev_started = Instant::now();
        for (index, _) in candidates {
            let candidate = &self.lexical[index];
            // Edit-distance similarity cannot exceed the ratio of the shorter length.
            if (target_length.min(candidate.characters) as f64)
                / (target_length.max(candidate.characters).max(1) as f64)
                + f64::EPSILON
                < best
            {
                continue;
            }
            self.fuzzy_levenshtein_calls.fetch_add(1, Ordering::Relaxed);
            let Some(score) = scorer.normalized_similarity_with_args(
                candidate.normalized.chars(),
                &rapidfuzz::distance::levenshtein::Args::default().score_cutoff(best),
            ) else {
                continue;
            };
            if score < best {
                continue;
            }
            self.fuzzy_above_cutoff.fetch_add(1, Ordering::Relaxed);
            let Some(id) = self.choose(&self.aliases[&candidate.alias], edge.from) else {
                continue;
            };
            if score > best {
                best = score;
                matches.clear();
            }
            matches.insert(id);
        }
        self.fuzzy_lev_nanos
            .fetch_add(lev_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.fuzzy_nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        (matches.len() == 1).then(|| (*matches.iter().next().unwrap(), best))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{EdgeType, Language, Location, NodeType};

    fn node(file: &str, name: &str) -> CodeNode {
        let mut node = CodeNode::new(
            name.to_string(),
            Some(NodeType::Function),
            Some(Language::Rust),
            Location {
                file_path: file.into(),
                line: 1,
                column: 1,
                end_line: None,
                end_column: None,
            },
        );
        node.set_deterministic_id("test");
        node
    }
    fn edge(from: NodeId, to: &str) -> EdgeRelationship {
        EdgeRelationship {
            from,
            to: to.into(),
            edge_type: EdgeType::Calls,
            metadata: Default::default(),
            span: None,
        }
    }

    #[test]
    fn ambiguous_names_are_scoped_and_direct_definitions_remain_authoritative() {
        let a = node("a.rs", "duplicate");
        let b = node("b.rs", "duplicate");
        let caller = node("b.rs", "caller");
        for nodes in [
            vec![a.clone(), b.clone(), caller.clone()],
            vec![caller.clone(), b.clone(), a.clone()],
        ] {
            let catalog = SymbolCatalog::new(&nodes);
            assert!(!catalog.unique_aliases().contains_key("duplicate"));
            assert_eq!(
                catalog.exact(&edge(caller.id, "duplicate"), &[]),
                Some(b.id)
            );
            assert_eq!(
                catalog.exact(&edge(NodeId::new_v4(), "duplicate"), &[]),
                None
            );
            let mut direct = edge(caller.id, "duplicate");
            direct
                .metadata
                .insert("target_node_id".into(), a.id.to_string());
            assert_eq!(catalog.exact(&direct, &[]), Some(a.id));
        }
    }

    #[test]
    fn lexical_candidates_are_deterministic_and_unicode_safe() {
        let target = node("a.rs", "calculate_result");
        let catalog = SymbolCatalog::new(&[target.clone(), node("b.rs", "unrelated")]);
        assert_eq!(
            catalog
                .fuzzy(&edge(target.id, "calculate_reslt"))
                .map(|(id, _)| id),
            Some(target.id)
        );
        assert!(!catalog.candidates("calculate", 2).is_empty());
        assert!(!catalog.semantic_candidates("calculate_reslt", 0).is_empty());
        assert!(catalog.candidates("🦀", 0).is_empty());
    }

    #[test]
    fn fuzzy_shortlist_keeps_the_full_ranking_prefix_without_sorting_everything() {
        let nodes: Vec<_> = [
            "calculate_result",
            "calculate_result_extra",
            "calculate_response",
            "calculate",
            "recalculate_result",
            "unrelated",
        ]
        .into_iter()
        .map(|name| node("a.rs", name))
        .collect();
        let catalog = SymbolCatalog::new(&nodes);
        let target = "calculate_reslt";
        let grams = trigrams(target);
        let full = catalog.candidate_indices(&grams, 0);
        assert!(full.len() > 4);
        for limit in 1..=4 {
            let shortlist =
                catalog.ranked_candidate_indices(&grams, target.chars().count(), 0.0, limit);
            assert_eq!(shortlist, full[..limit].to_vec());
        }
    }

    #[test]
    fn fuzzy_shortlist_drops_candidates_below_the_length_ratio_floor() {
        let nodes = vec![
            node("a.rs", "xyz"),
            node("b.rs", "xyzabcdef"),
            node("c.rs", "xyzabcdefgh"),
        ];
        let catalog = SymbolCatalog::new(&nodes);
        let target = "xyzabcdefgh";
        let grams = trigrams(target);
        let length = target.chars().count();
        let unpruned = catalog.ranked_candidate_indices(&grams, length, 0.0, 0);
        let pruned = catalog.ranked_candidate_indices(&grams, length, 0.85, 0);
        assert!(unpruned.len() > pruned.len());
        assert!(unpruned.iter().any(|(index, _)| {
            let characters = catalog.lexical[*index].characters;
            (length.min(characters) as f64) / (length.max(characters) as f64) < 0.85
        }));
        assert!(pruned.iter().all(|(index, _)| {
            let characters = catalog.lexical[*index].characters;
            (length.min(characters) as f64) / (length.max(characters) as f64) + f64::EPSILON >= 0.85
        }));
    }

    #[test]
    fn fuzzy_records_call_and_candidate_diagnostics() {
        let target = node("a.rs", "calculate_result");
        let catalog = SymbolCatalog::new(&[target.clone()]);
        assert_eq!(catalog.fuzzy_stats().calls, 0);
        catalog.fuzzy(&edge(target.id, "calculate_reslt"));
        let stats = catalog.fuzzy_stats();
        assert_eq!(stats.calls, 1);
        assert!(stats.candidates > 0);
        assert!(stats.nanos > 0);
        assert!(stats.levenshtein_calls > 0);
        assert!(stats.levenshtein_calls <= stats.candidates);
        assert!(stats.above_cutoff > 0);
        assert!(stats.above_cutoff <= stats.levenshtein_calls);
        assert!(stats.trigram_nanos > 0);
        assert!(stats.counts_nanos > 0);
        assert!(stats.lev_nanos > 0);
        assert!(stats.trigram_nanos + stats.counts_nanos + stats.lev_nanos <= stats.nanos);
    }

    #[test]
    fn fuzzy_candidate_limit_parsing_defaults_to_unlimited() {
        assert_eq!(fuzzy_limit_for(None), 0);
        assert_eq!(fuzzy_limit_for(Some("")), 0);
        assert_eq!(fuzzy_limit_for(Some("   ")), 0);
        assert_eq!(fuzzy_limit_for(Some("0")), 0);
        assert_eq!(fuzzy_limit_for(Some("256")), 256);
        assert_eq!(fuzzy_limit_for(Some(" 128 ")), 128);
        assert_eq!(fuzzy_limit_for(Some("abc")), 0);
        assert_eq!(fuzzy_limit_for(Some("-1")), 0);
    }

    #[test]
    fn zero_limit_keeps_every_length_pruned_candidate() {
        let nodes: Vec<_> = [
            "calculate_result",
            "calculate_result_extra",
            "calculate_response",
            "calculate",
            "recalculate_result",
            "unrelated",
        ]
        .into_iter()
        .map(|name| node("a.rs", name))
        .collect();
        let catalog = SymbolCatalog::new(&nodes);
        let target = "calculate_reslt";
        let grams = trigrams(target);
        let length = target.chars().count();
        let all = catalog.ranked_candidate_indices(&grams, length, 0.85, 0);
        assert!(!all.is_empty(), "unlimited shortlist must not be empty");
        let expected = catalog
            .candidate_counts(&grams)
            .into_iter()
            .filter(|(index, _)| {
                let characters = catalog.lexical[*index].characters;
                (length.min(characters) as f64) / (length.max(characters).max(1) as f64)
                    + f64::EPSILON
                    >= 0.85
            })
            .count();
        assert_eq!(all.len(), expected);
        assert_eq!(
            catalog.ranked_candidate_indices(&grams, length, 0.85, expected + 10),
            all,
            "a cap wider than the candidate set must retain it unchanged"
        );
    }
}
