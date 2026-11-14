# Supported languages

This document describes which languages CodeGraph can index today, and what “support” means (AST extraction vs analyzer enrichment).

## What counts as “supported”

CodeGraph’s indexing pipeline has multiple layers:

1. **AST extraction** (Tree-sitter): parses files and emits initial nodes/edges.
2. **FastML** enhancement: fast heuristics that can improve recall.
3. **Analyzers** (optional but enabled by default): build context, module linking, SCIP import, docs/contracts, architecture, dataflow (Rust-local).

So a language can be:

- **AST-supported**: Tree-sitter parsing + baseline node extraction works.
- **Analyzer-supported**: additional enrichers run (module linker, SCIP, etc.).

## AST-supported languages (Tree-sitter)

These languages are registered in `crates/codegraph-parser/src/language.rs`:

| Language | File extensions |
|---|---|
| Rust | `rs` |
| TypeScript | `ts`, `tsx` |
| JavaScript | `js`, `jsx` |
| Python | `py`, `pyi` |
| Go | `go` |
| Java | `java` |
| C/C++ | `c`, `h`, `cc`, `cpp`, `cxx`, `hpp`, `hxx` |
| Swift | `swift` |
| C# | `cs` |
| Ruby | `rb`, `rake`, `gemspec` |
| PHP | `php`, `phtml`, `php3`, `php4`, `php5` |

## Analyzer support by language

### SCIP-backed enrichment

CodeGraph enriches nodes/edges from a pre-built SCIP artifact. There is no language-server
process: `scripts/scip-index.sh` selects the right indexer per language and prints the
`CODEGRAPH_SCIP_INDEX` value.

| Language | SCIP indexer | Notes |
|---|---|---|
| Rust | `rust-analyzer scip` | Adds qualified names and definition/reference edges |
| TypeScript | `scip-typescript index` | Needs a TypeScript project |
| JavaScript | `scip-typescript index` | Same indexer as TS |
| Go | `scip-go` | |

Languages without a SCIP indexer (Python, Java, C/C++, Swift, C#, Ruby, PHP, Kotlin) are
**AST-only**: they are parsed and linked, but receive no compiler-derived definition edges.
A SCIP index built by other tooling can still be supplied for them through
`CODEGRAPH_SCIP_INDEX`.

You can disable the analyzer stages entirely via:

- `CODEGRAPH_ANALYZERS=false`

### Module linking

Module/import linking currently targets:

- TypeScript
- JavaScript
- Python
- Go

### Rust-specific analyzers

Some analyzers are Rust-only today:

- Build context (Cargo workspace graph + feature edges)
- Rustdoc/API surface enrichment
- Local dataflow enrichment

## Kotlin and Dart status

CodeGraph’s core language enum includes Kotlin and Dart, and file-extension detection exists in parts of the pipeline, but Tree-sitter parsing for these languages is currently disabled due to Tree-sitter version conflicts:

- `tree-sitter-kotlin` and `tree-sitter-dart` are not enabled in `crates/codegraph-parser/Cargo.toml`

Contributions to restore Kotlin/Dart parsing support are welcome (see `CONTRIBUTING.md`).
