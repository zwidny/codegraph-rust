// ABOUTME: Runs analyzer phases that enrich indexing beyond AST extraction
// ABOUTME: Manages analyzer execution settings and enrichment statistics

pub mod architecture;
pub mod build_context;
pub mod dataflow;
pub mod docs_contracts;
pub mod enrichment;
pub mod module_linker;
pub mod scip;

/// How much enrichment a run actually applied.
///
/// Producers were language servers in the past; today only the SCIP importer
/// fills this in.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnrichmentStats {
    pub nodes_enriched: usize,
    pub edges_resolved: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalyzerSettings {
    pub build_context: bool,
    pub enrichment: bool,
    pub module_linking: bool,
    pub dataflow: bool,
    pub docs_contracts: bool,
    pub architecture: bool,
}

impl AnalyzerSettings {
    pub fn for_tier(tier: codegraph_core::config_manager::IndexingTier) -> Self {
        match tier {
            codegraph_core::config_manager::IndexingTier::Fast => Self {
                build_context: false,
                enrichment: false,
                module_linking: false,
                dataflow: false,
                docs_contracts: false,
                architecture: false,
            },
            codegraph_core::config_manager::IndexingTier::Balanced => Self {
                build_context: true,
                enrichment: true,
                module_linking: true,
                dataflow: false,
                docs_contracts: true,
                architecture: false,
            },
            codegraph_core::config_manager::IndexingTier::Full => Self {
                build_context: true,
                enrichment: true,
                module_linking: true,
                dataflow: true,
                docs_contracts: true,
                architecture: true,
            },
        }
    }

    pub fn from_env(tier: codegraph_core::config_manager::IndexingTier) -> Self {
        let enabled = !matches!(
            std::env::var("CODEGRAPH_ANALYZERS").as_deref(),
            Ok("0" | "false" | "off")
        );
        Self::configured(tier, enabled)
    }

    fn configured(tier: codegraph_core::config_manager::IndexingTier, enabled: bool) -> Self {
        if !enabled {
            return Self {
                build_context: false,
                enrichment: false,
                module_linking: false,
                dataflow: false,
                docs_contracts: false,
                architecture: false,
            };
        }
        Self::for_tier(tier)
    }

    pub fn any_enabled(&self) -> bool {
        self.build_context
            || self.enrichment
            || self.module_linking
            || self.dataflow
            || self.docs_contracts
            || self.architecture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrichment_stats_is_default_zero() {
        let stats = EnrichmentStats::default();
        assert_eq!(stats.nodes_enriched, 0);
        assert_eq!(stats.edges_resolved, 0);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn disabled_analyzers_stay_disabled_after_tier_upgrade() {
        for tier in [
            codegraph_core::config_manager::IndexingTier::Fast,
            codegraph_core::config_manager::IndexingTier::Balanced,
            codegraph_core::config_manager::IndexingTier::Full,
        ] {
            assert!(!AnalyzerSettings::configured(tier, false).any_enabled());
        }
        assert!(
            AnalyzerSettings::configured(codegraph_core::config_manager::IndexingTier::Full, true)
                .any_enabled()
        );
        assert!(
            !AnalyzerSettings::for_tier(codegraph_core::config_manager::IndexingTier::Fast)
                .any_enabled()
        );
    }
}
