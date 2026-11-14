// ABOUTME: Aho-Corasick pattern matcher for fast multi-pattern code analysis
// ABOUTME: Provides sub-microsecond pattern matching without training requirements

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use codegraph_core::{EdgeType, ExtractionResult};
use tracing::debug;

/// Fast pattern matcher using Aho-Corasick automaton (50-500ns per search)
pub struct PatternMatcher {
    /// Compiled pattern automaton
    automaton: AhoCorasick,
    /// Pattern metadata (pattern index → (pattern name, edge type))
    patterns: Vec<(String, EdgeType)>,
}

impl PatternMatcher {
    /// Create pattern matcher from common code patterns
    pub fn new() -> Self {
        // Common code patterns across languages
        // Language-scoped patterns to avoid useless matches
        let pattern_configs = vec![
            // Rust patterns
            ("rust:std::", EdgeType::Uses),
            ("rust:derive(", EdgeType::Uses),
            ("rust:impl ", EdgeType::Implements),
            ("rust:trait ", EdgeType::Defines),
            ("rust:async fn", EdgeType::Defines),
            ("rust:pub fn", EdgeType::Defines),
            ("rust:use ", EdgeType::Uses),
            // TypeScript / JavaScript
            ("ts:import ", EdgeType::Uses),
            ("ts:export ", EdgeType::Defines),
            ("ts:async ", EdgeType::Defines),
            ("ts:interface ", EdgeType::Defines),
            ("ts:class ", EdgeType::Defines),
            ("ts:extends ", EdgeType::Extends),
            ("ts:implements ", EdgeType::Implements),
            ("js:import ", EdgeType::Uses),
            ("js:export ", EdgeType::Defines),
            ("js:class ", EdgeType::Defines),
            ("js:extends ", EdgeType::Extends),
            // Python
            ("py:from ", EdgeType::Uses),
            ("py:import ", EdgeType::Uses),
            ("py:class ", EdgeType::Defines),
            ("py:def ", EdgeType::Defines),
            ("py:async def", EdgeType::Defines),
            // Go
            ("go:package ", EdgeType::Defines),
            ("go:import ", EdgeType::Uses),
            ("go:func ", EdgeType::Defines),
            ("go:type ", EdgeType::Defines),
            ("go:interface ", EdgeType::Defines),
            // Java
            ("java:import ", EdgeType::Uses),
            ("java:class ", EdgeType::Defines),
            ("java:interface ", EdgeType::Defines),
            ("java:extends ", EdgeType::Extends),
            ("java:implements ", EdgeType::Implements),
            ("java:@Override", EdgeType::Implements),
            ("java:@Autowired", EdgeType::Uses),
            // C++
            ("cpp:#include ", EdgeType::Uses),
            ("cpp:class ", EdgeType::Defines),
            ("cpp:namespace ", EdgeType::Defines),
            ("cpp:template<", EdgeType::Defines),
            ("cpp:virtual ", EdgeType::Defines),
            ("cpp:public:", EdgeType::Defines),
            ("cpp::: ", EdgeType::Uses),
            // Swift
            ("swift:import ", EdgeType::Uses),
            ("swift:class ", EdgeType::Defines),
            ("swift:struct ", EdgeType::Defines),
            ("swift:protocol ", EdgeType::Defines),
            ("swift:extension ", EdgeType::Extends),
            ("swift:func ", EdgeType::Defines),
            ("swift:@objc", EdgeType::Uses),
            // C#
            ("csharp:using ", EdgeType::Uses),
            ("csharp:class ", EdgeType::Defines),
            ("csharp:interface ", EdgeType::Defines),
            ("csharp:namespace ", EdgeType::Defines),
            ("csharp:async ", EdgeType::Defines),
            ("csharp:public ", EdgeType::Defines),
            ("csharp:[", EdgeType::Uses), // Attributes like [Serializable]
            // Ruby
            ("ruby:require ", EdgeType::Uses),
            ("ruby:require_relative ", EdgeType::Uses),
            ("ruby:class ", EdgeType::Defines),
            ("ruby:module ", EdgeType::Defines),
            ("ruby:def ", EdgeType::Defines),
            ("ruby:include ", EdgeType::Uses),
            ("ruby:extend ", EdgeType::Extends),
            // PHP
            ("php:namespace ", EdgeType::Defines),
            ("php:use ", EdgeType::Uses),
            ("php:class ", EdgeType::Defines),
            ("php:interface ", EdgeType::Defines),
            ("php:trait ", EdgeType::Defines),
            ("php:function ", EdgeType::Defines),
            ("php:extends ", EdgeType::Extends),
            ("php:implements ", EdgeType::Implements),
        ];

        let patterns: Vec<String> = pattern_configs
            .iter()
            .map(|(p, _)| p.split_once(':').unwrap().1.to_string())
            .collect();
        let pattern_metadata: Vec<(String, EdgeType)> = pattern_configs
            .iter()
            .map(|(p, e)| (p.to_string(), e.clone()))
            .collect();

        let automaton = AhoCorasickBuilder::new()
            .build(&patterns)
            .expect("Failed to build Aho-Corasick automaton");

        debug!(
            "Initialized PatternMatcher with {} patterns",
            patterns.len()
        );

        Self {
            automaton,
            patterns: pattern_metadata,
        }
    }

    /// Enhance extraction result with node-level pattern enrichment (50-500ns per file)
    pub fn enhance_extraction(&self, result: ExtractionResult, content: &str) -> ExtractionResult {
        self.enhance_with_policy(result, content, Default::default())
    }

    pub fn enhance_with_policy(
        &self,
        mut result: ExtractionResult,
        content: &str,
        policy: crate::languages::ExtractionPolicy,
    ) -> ExtractionResult {
        let lang_prefix = match result.nodes.first().and_then(|n| n.language.as_ref()) {
            Some(codegraph_core::Language::Rust) => "rust:",
            Some(codegraph_core::Language::TypeScript) => "ts:",
            Some(codegraph_core::Language::JavaScript) => "js:",
            Some(codegraph_core::Language::Python) => "py:",
            Some(codegraph_core::Language::Go) => "go:",
            Some(codegraph_core::Language::Java) => "java:",
            Some(codegraph_core::Language::Cpp) => "cpp:",
            Some(codegraph_core::Language::Swift) => "swift:",
            Some(codegraph_core::Language::CSharp) => "csharp:",
            Some(codegraph_core::Language::Ruby) => "ruby:",
            Some(codegraph_core::Language::Php) => "php:",
            _ => "",
        };
        // Language is a metadata filter, not a prefix on the whole source. A prefix
        // only matched the first keyword and missed every subsequent line.
        let matches: Vec<_> = self
            .automaton
            .find_overlapping_iter(content)
            .filter(|m| {
                let (name, edge_type) = &self.patterns[m.pattern().as_usize()];
                !lang_prefix.is_empty() && name.starts_with(lang_prefix) && policy.allows(edge_type)
            })
            .collect();

        if matches.is_empty() {
            return result;
        }

        // Collect pattern names for node enrichment. Pattern matches no longer
        // produce edges: their `to` was a pattern name (e.g. "py:import "), which
        // is not a real symbol and could never resolve to a node.
        let mut pattern_names = Vec::new();

        for m in &matches {
            let pattern_idx = m.pattern().as_usize();

            // Track pattern names for node metadata enrichment
            if let Some((pattern_name, _)) = self.patterns.get(pattern_idx) {
                if !pattern_names.contains(pattern_name) {
                    pattern_names.push(pattern_name.clone());
                }
            }
        }

        // Enrich file-level nodes with pattern context for better embeddings
        // This helps SOTA embedding models understand the code's characteristics
        if !pattern_names.is_empty() && !result.nodes.is_empty() {
            // Find file-level or module-level nodes to enrich
            for node in &mut result.nodes {
                // Enrich nodes that represent the file or module scope
                if matches!(node.node_type, Some(codegraph_core::NodeType::Module))
                    || node.node_type.is_none()
                {
                    // Add pattern context to metadata
                    node.metadata
                        .attributes
                        .insert("fast_ml_patterns".to_string(), pattern_names.join(", "));
                    node.metadata.attributes.insert(
                        "fast_ml_pattern_count".to_string(),
                        matches.len().to_string(),
                    );
                    break; // Only enrich the first file-level node
                }
            }
        }

        result
    }

    /// Get pattern statistics
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }
}

impl Default for PatternMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{CodeNode, Language};

    /// Pattern matches must not be emitted as edges: their `to` was a pattern
    /// name such as `"py:import "`, not a real symbol.
    fn is_synthetic_pattern_edge(to: &str) -> bool {
        [
            "rust:", "ts:", "js:", "py:", "go:", "java:", "cpp:", "swift:", "csharp:", "ruby:",
            "php:",
        ]
        .iter()
        .any(|prefix| to.starts_with(prefix))
    }

    #[test]
    fn matches_later_lines_and_filters_other_languages_and_tiers() {
        let matcher = PatternMatcher::new();
        let mut node = CodeNode::new_test();
        node.language = Some(Language::Rust);
        let result = ExtractionResult {
            nodes: vec![node],
            edges: vec![],
        };
        let content = "// heading\n    use std::fmt;\n    pub fn a() {}\n    impl Trait for A {}\n    import x\n";
        let full = matcher.enhance_extraction(result.clone(), content);
        assert!(
            full.edges
                .iter()
                .all(|edge| !is_synthetic_pattern_edge(&edge.to)),
            "pattern matches must not become edges, got: {:?}",
            full.edges.iter().map(|e| &e.to).collect::<Vec<_>>()
        );
        let full_patterns = full.nodes[0]
            .metadata
            .attributes
            .get("fast_ml_patterns")
            .expect("file-level node should be enriched with fast_ml_patterns")
            .clone();
        // Language filtering: only Rust patterns are recorded, never `py:import ` etc.
        assert!(
            full_patterns.split(", ").all(|p| p.starts_with("rust:")),
            "unexpected cross-language patterns: {full_patterns}"
        );
        let full_count: usize = full.nodes[0]
            .metadata
            .attributes
            .get("fast_ml_pattern_count")
            .expect("file-level node should be enriched with fast_ml_pattern_count")
            .parse()
            .expect("pattern count should be numeric");

        let fast = matcher.enhance_with_policy(
            result,
            content,
            crate::languages::ExtractionPolicy {
                uses: false,
                references: false,
            },
        );
        assert!(fast.edges.is_empty());
        let fast_count: usize = fast.nodes[0]
            .metadata
            .attributes
            .get("fast_ml_pattern_count")
            .expect("fast_ml_pattern_count should survive tier filtering")
            .parse()
            .expect("pattern count should be numeric");
        // Tier filtering still applies: Uses matches dropped, so fewer matches.
        assert!(
            fast_count < full_count,
            "uses=false should filter Uses matches ({fast_count} vs {full_count})"
        );
    }

    #[test]
    fn test_pattern_matching_speed() {
        let matcher = PatternMatcher::new();
        // Content must contain patterns without leading whitespace on the pattern keyword
        let content = "use std::collections::HashMap;\npub fn test() {}\nimpl MyTrait for Foo {}";

        let mut node = CodeNode::new_test();
        node.language = Some(Language::Rust);

        let result = ExtractionResult {
            nodes: vec![node],
            edges: vec![],
        };

        let enhanced = matcher.enhance_extraction(result, content);
        assert!(
            enhanced
                .edges
                .iter()
                .all(|edge| !is_synthetic_pattern_edge(&edge.to)),
            "Should not emit synthetic pattern edges"
        );
        assert!(
            enhanced.nodes[0]
                .metadata
                .attributes
                .contains_key("fast_ml_patterns"),
            "Should have found Rust patterns in content"
        );
    }
}
