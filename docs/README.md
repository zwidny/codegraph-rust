# CodeGraph Documentation

Welcome to the comprehensive documentation for the CodeGraph project.

## Guides

- [How CodeGraph works (overview)](OVERVIEW.md)
- [Installation & setup](INSTALLATION_GUIDE.md)
- [Usage guide (for AI assistants)](USAGE_GUIDE.md)
- [Incremental indexing (single file / subdirectory)](INCREMENTAL_INDEXING.md)
- [Indexing pipeline: stages, timings and controls](INDEXING_PIPELINE.md)
- [CLI and direct graph queries](USAGE_GUIDE.md#cli-access-for-code-agents)
- [AI provider configuration (embeddings)](AI_PROVIDERS.md)
- [Supported languages](SUPPORTED_LANGUAGES.md)

## Architecture Visualization
Explore the interactive [Architecture Diagram](architecture-visualization.html) to understand the system components and data flow.

## Crate Documentation

### Core & Data
- [codegraph-core](crates/codegraph-core/README.md): Fundamental types and models (`CodeNode`, `ExtractionResult`).
- [codegraph-graph](crates/codegraph-graph/README.md): Database Access Layer (SurrealDB).
- [codegraph-zerocopy](crates/codegraph-zerocopy/README.md): Zero-copy serialization utilities.

### Processing & AI
- [codegraph-parser](crates/codegraph-parser/README.md): Tree-sitter parsing and unified extraction.
- [codegraph-vector](crates/codegraph-vector/README.md): Embeddings and chunking.
- [codegraph-cache](crates/codegraph-cache/README.md): Caching and read-ahead mechanisms.
- [codegraph-concurrent](crates/codegraph-concurrent/README.md): Concurrency primitives.

### MCP Ecosystem
- [codegraph-mcp](crates/codegraph-mcp/README.md): Main Orchestrator and Indexer.
- [codegraph-mcp-server](crates/codegraph-mcp-server/README.md): Server binary and setup.
- [codegraph-mcp-daemon](crates/codegraph-mcp-daemon/README.md): Background service and file watching.
- [codegraph-mcp-tools](crates/codegraph-mcp-tools/README.md): Specific tool implementations.
- [codegraph-mcp-core](crates/codegraph-mcp-core/README.md): Shared MCP traits.

## Specifications
Detailed specs can be found in the [specifications](specifications/) directory.
