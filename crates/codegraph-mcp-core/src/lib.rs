// ABOUTME: Core MCP types and utilities shared by the server, tools, and daemon crates
// ABOUTME: Exposes protocol-facing data structures without server runtime

pub mod debug_logger;
pub mod error;
pub mod message;
pub mod process;
pub mod protocol;
pub mod version;

pub use codegraph_core::{CodeGraphError, NodeId};
pub use debug_logger::*;
pub use error::{McpError, Result};
pub use message::*;
pub use process::*;
pub use protocol::*;
pub use version::*;
