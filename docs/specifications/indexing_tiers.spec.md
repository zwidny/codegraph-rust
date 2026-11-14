ABOUTME: Defines the indexing tier presets that trade speed/storage for graph richness.
ABOUTME: Establishes default tier and required analyzer/edge behavior per tier.

# Specification: Indexing Tiers

## Intent

Give users a simple, explicit way to control indexing thoroughness, trading speed and storage for richer graph output. The default tier prioritizes speed.

## Contract

### Tier Values

Supported tiers:
- `fast` (default)
- `balanced`
- `full`

### Default

If no tier is configured, indexing MUST run in `fast` mode.

### Configuration Surface

Users can set the tier via:
- CLI flag: `--index-tier <fast|balanced|full>`
- Environment variable: `CODEGRAPH_INDEX_TIER`
- Config file: `[indexing] tier = "fast|balanced|full"`

CLI > env > config defaults.

### Analyzer Behavior by Tier

| Tier | Build Context | SCIP Enrichment | Enrichment (docs/api) | Module Linking | Dataflow | Docs/Contracts | Architecture |
|------|----------------|-----------------|-----------------------|----------------|----------|----------------|--------------|
| fast | off            | if index supplied | off                 | off            | off      | off            | off          |
| balanced | on         | if index supplied | on                  | on             | off      | on             | off          |
| full | on             | if index supplied | on                    | on             | on       | on             | on           |

SCIP enrichment is not tier-gated: whenever `CODEGRAPH_SCIP_INDEX` provides an index it is
imported on every tier. The tier decides which AST analyzer stages run and which edge types
are retained.

### Edge Filtering by Tier

The tier controls which edge types from AST extraction are retained:

- `fast`: keep `Calls`, `Defines`, `Imports`, `Contains`, `Extends`, `Implements`, and `Other`
- `balanced`: keep `fast` + `Uses`, and `Other`
- `full`: keep all edge types (no filtering)

### Logging

Indexing logs MUST record the active tier and enabled analyzer toggles at the start of indexing.

## Acceptance Criteria

1. Default config loads `fast` tier when none specified.
2. CLI/env/config overrides map to the correct tier.
3. Each tier gates analyzers and edge types as specified.
4. Tests enforce tier parsing and edge gating.

