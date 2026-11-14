// ABOUTME: MCP server entry (stdio/http) using core and tools crates
// ABOUTME: Thin runtime layer wiring transports and handlers

pub mod agent_hooks;
#[cfg(feature = "server-http")]
pub mod http_config;
#[cfg(feature = "server-http")]
pub mod http_server;
pub mod official_server;
pub mod plan_cli;
pub mod project_init;
pub mod prompts;
pub mod query_cli;

pub use codegraph_mcp_core::analysis::AnalysisType;
#[cfg(feature = "server-http")]
pub use http_config::*;
#[cfg(feature = "server-http")]
pub use http_server::*;
pub use official_server::*;
