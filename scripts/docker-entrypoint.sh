#!/bin/sh
# Container entrypoint (works on any container platform).
#
# If LOOMEM_MIGRATE_GRAPH_STREAMS_ON_START=1, run the graph entity stream
# migration (re-stamp graph:entity:* and graph:edge:* whose stream_id
# disagrees with the stream of the chunks they reference). Opt-in write:
#   LOOMEM_MIGRATE_GRAPH_STREAMS_COMMIT=1       — set to mutate (default dry-run).
#   LOOMEM_MIGRATE_GRAPH_STREAMS_MANIFEST_DIR   — optional, defaults to /data/graph-migration-plans.
# Idempotent (loomem-migrate skips AlreadyCorrect entities). Mixed-streams
# and cross-stream edges are SKIPPED + flagged in manifest for manual review
# (not mutated). Backup checkpoint created before first write when --commit.
#
# After a successful migration, operators should unset the ON_START flag
# and redeploy so subsequent container restarts take the no-op fast path.
set -e

if [ "${LOOMEM_MIGRATE_GRAPH_STREAMS_ON_START}" = "1" ]; then
  COMMIT_FLAG=""
  if [ "${LOOMEM_MIGRATE_GRAPH_STREAMS_COMMIT}" = "1" ]; then
    COMMIT_FLAG="--commit"
  fi
  MANIFEST_DIR="${LOOMEM_MIGRATE_GRAPH_STREAMS_MANIFEST_DIR:-/data/graph-migration-plans}"
  mkdir -p "${MANIFEST_DIR}"
  # Deliberate path choice: `--db /data/rocksdb`, not `--db /data`. loomem-migrate
  # opens the given path directly as a RocksDB dir; loomem-server reads
  # config.toml's data_dir ("/data") and appends "rocksdb" internally, so the
  # live DB lives at /data/rocksdb. Passing `--db /data` would spin up an empty
  # shadow DB in the volume root and migrate nothing.
  echo ">>> Running loomem-migrate --migrate-graph-entity-streams (commit=${LOOMEM_MIGRATE_GRAPH_STREAMS_COMMIT:-0}, manifest_dir=${MANIFEST_DIR})"
  ./loomem-migrate --migrate-graph-entity-streams --db /data/rocksdb --manifest-dir "${MANIFEST_DIR}" ${COMMIT_FLAG}
  echo ">>> Graph entity stream migration done."
else
  echo ">>> Skipping graph-entity-stream migration (LOOMEM_MIGRATE_GRAPH_STREAMS_ON_START not set)."
fi

exec ./loomem-server
