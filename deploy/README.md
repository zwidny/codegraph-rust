# Deploying SurrealDB for CodeGraph

This directory hosts the reference Docker Compose setup for the SurrealDB server that
CodeGraph indexes into. It is optional: CodeGraph also works against an embedded
`surrealkv://` store, but that serialises indexing and queries in one process.

## Why this file exists

CodeGraph does **not** run SurrealDB itself. The server is the indexing backend, and
its storage engine has its own failure mode under sustained writes. A production
incident showed both halves of the problem:

| Time (CST) | Event |
|---|---|
| 2026-11-10 17:20 | `codegraph index` starts writing ~38k nodes to `EasyTier_8a11a0febb5c` |
| 2026-11-10 17:32 | SurrealDB logs `query ... exceeded the timeout: 1m` |
| 2026-11-10 17:52 | `systemd-journald: Under memory pressure` starts repeating |
| 2026-11-10 17:32 → 11-11 00:21 | the same timeout recurs ~15 times |
| 2026-11-11 00:21 → 05:45 | SurrealDB logs nothing at all |
| 2026-11-11 05:45 | host is force-rebooted (journal has no shutdown sequence) |
| 2026-11-11 08:36 | restart; SurrealKV replays 6350 WAL batches and reports **5,030,185 memtable entries** |

Root cause found by controlled reproduction: with an 8k–38k node graph already in the
store, a rewrite (`--force`) stalls inside SurrealKV's **memtable flush → SST write**.
On restart the engine reports an `orphaned SST file` (manifest shows `table_id=131`,
the stalled flush was writing `132`). No OOM kill was recorded; the flush simply never
finished, and every query then waited on it.

Two things amplify the incident, and neither is SurrealDB's:

1. CodeGraph's node/edge upsert has **no client-side timeout**, so one stalled server
   query parks the whole index run forever (see `crates/codegraph-graph/src/surrealdb_storage.rs`).
2. A container without a memory cap let SurrealKV reserve a **15.6GB block cache** and a
   **1GB memtable** on a 31GB host.

This compose file fixes (2). Fix (1) lives in the Rust code:

- `upsert_nodes_batch` / `upsert_edges_batch` now wrap the query in a deadline
  (`CODEGRAPH_DB_WRITE_TIMEOUT_SECS`, default 120s) and report
  `... timed out after 120s (N items) ... check the server's memtable flush/compaction`.
- The error is classified as **not retryable**, so the index run fails loudly instead of
  retrying into the same stall.
- The indexer logs `Persisted X/Y nodes (batch a/b)` and the writer logs
  `graph writer: N rows persisted` every ~5k rows, so progress survives `nohup`
  (the interactive progress bars do not).

## What the memory limit buys

SurrealDB v3.3.0 exposes **no** flag or environment variable for the block cache or the
memtable size; SurrealKV derives both from the memory it can see. A cgroup limit is
therefore the only lever. Measured on this host:

| `mem_limit` | block cache | memtable |
|---|---|---|
| none (31GB host) | 15.6 GB | 1 GB |
| `8g` | 3 GB | 256 MB |
| `12g` (default) | 5 GB | 256 MB |

Verify it after any change:

```bash
docker logs codegraph-db 2>&1 | grep -E "block cache capacity|max memtable size"
```

If the numbers do not match the table, the limit is not being applied (for example the
container was started without `mem_limit`, or an old container is still running).

## Usage

```bash
cd deploy
cp .env.example .env
$EDITOR .env                     # set SURREALDB_PASSWORD
docker compose config            # validate
docker compose up -d
docker logs codegraph-db 2>&1 | grep -E "block cache|Startup complete"
```

The default volume name is `cg-server-data`, which is what the previous non-compose
deployment created, so `docker compose up -d` reuses the existing data. Stop the old
container first (`docker rm -f codegraph-db`) or Compose will fail on the name clash.

Point CodeGraph at it with:

```bash
CODEGRAPH_SURREALDB_URL=ws://127.0.0.1:8010
CODEGRAPH_SURREALDB_NAMESPACE=codegraph
CODEGRAPH_SURREALDB_USERNAME=root
CODEGRAPH_SURREALDB_PASSWORD=...
```

## Pin SurrealDB 4.x, not 3.x

**4.x is not optional for large projects.** The 3.x datastore bounds memtable residency
by entry count instead of bytes, so a first-time index of a large project grows the
memtable past the container limit and the kernel OOM-kills the server. Measured on the
same EasyTier tree (38057 nodes / 72940 edges) with the same 12g cap:

| Version | Peak RSS | Result |
|---|---|---|
| 3.3 | 11.95 GiB | OOM-killed, 39 `Transaction write conflict`, 5 restarts |
| **4.0.1** | **0.86 GiB** | exit 0, 0 conflicts, 0 WAL rotations |

4.x also makes the block cache demand-allocated, so a 5 GB configured cache does not
mean 5 GB resident.

RocksDB is **no longer an option**: 4.x removed the RocksDB and IndxDB engines
(`surrealdb` commit `aa5a4fea0`); `rocksdb:*` settings are ignored with a warning.

### Data directories are not compatible across the major version

A 4.x server refuses to open a directory written by 3.x unless
`SURREAL_EXPERIMENTAL_UPGRADE_FROM_3X=true` is set, and the conversion is one-way — a
3.x release cannot open the result afterwards. The supported path is a **fresh volume
plus re-indexing**:

```bash
docker volume create cg-server-data-v4
SURREALDB_IMAGE_TAG=v4 SURREALDB_DATA_VOLUME=cg-server-data-v4 docker compose up -d
# per project database, apply schema/codegraph_v2.surql, then:
cd /path/to/project && codegraph index . --index-tier balanced
```

Keep the old volume until the new one is verified; it is the only rollback path.

## Notes

- The image runs as `nonroot`. Named volumes created by Docker are writable; bind mounts
  are not, unless you fix ownership.
- The image ships a healthcheck (`surreal isready`); no extra configuration is needed.
- `memswap_limit` equals `mem_limit`, which disables swap for the container so a stalled
  flush cannot silently spill to disk and hide the problem.
- Log rotation is capped at 5 x 50MB. Long incident windows previously filled the
  default json-file log.
