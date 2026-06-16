# Backup and Restore

## What is backed up

RocksDB checkpoint — a point-in-time snapshot of all data:
- All chunks (L0, L1)
- Embeddings (stored in RocksDB column family)
- Knowledge graph (entities, edges)
- Cost tracking data
- Wrapped per-stream encryption keys (when at-rest encryption is enabled)

**Not in the checkpoint** (auto-rebuilt on startup):
- Tantivy full-text index — rebuilt from RocksDB if schema version mismatches
- WAL/intent log — replayed on startup for crash recovery

## Encrypted-instance backup is JOINT (master key required)

When at-rest encryption is enabled (`LOOMEM_AT_REST_MASTER_KEY` set), the checkpoint
contains only **wrapped** per-scope DEKs (in the `keys` column family) and **ciphertext** for
the encrypted row classes (chunk content, entity, relation, graph entity, audit). The RocksDB
checkpoint **alone cannot decrypt them** — it does not contain the master key that unwraps the
DEKs.

A usable backup of an encrypted instance is therefore **joint**:

> **checkpoint** + a **confirmed master-key escrow** whose **fingerprint matches** the data.

- **Record the fingerprint beside every checkpoint.** Capture it at backup time so a future
  restore knows exactly which escrowed key the data was encrypted under:

  ```bash
  curl -s http://localhost:3030/v1/encryption/status \
    -H "Authorization: Bearer <admin_token>" | jq -r '.master_key_fingerprint' \
    > {data_dir}/backups/checkpoint-<ts>/master-key-fingerprint.txt
  ```

  The fingerprint is a one-way digest — safe to store in plaintext beside the
  checkpoint. The key itself is **never** stored here; it lives only in escrow.

- **Automated (12h-worker) checkpoints are still covered.** The fingerprint changes *only* when
  the key changes (rotation), so every checkpoint taken during a key's lifetime
  shares that key's single fingerprint. The escrow record (key + fingerprint + activation date)
  therefore identifies the correct key for **any** checkpoint by its timestamp, even one the
  worker wrote without a per-checkpoint fingerprint file. The `master-key-fingerprint.txt` step
  above is a convenience for fast lookup, not a recovery prerequisite; automating it as a
  post-backup hook is a possible future nicety, not a gap that blocks recovery.

- **Without the matching master key, an encrypted checkpoint is unrecoverable.** Routing,
  embeddings, and Tantivy stay readable, but every encrypted row class fails closed.

Keep at least one copy of the master key (with its fingerprint and activation date) in a
secret store that is **separate from the data volume and the backups** — a password manager,
cloud secret manager, or printed escrow. Verify after every rotation that the escrowed value
matches the running instance's fingerprint (`GET /v1/encryption/status`).

## Where backups are stored

```
{data_dir}/backups/checkpoint-{YYYY-MM-DD-HHMMSS}/
```

- **Local:** `./data/backups/`
- **Container deployments:** `/data/backups/` (on the persistent volume)

## Configuration

```toml
[worker.backup]
enabled = true
interval_secs = 43200    # every 12 hours
max_copies = 2           # keep last 2 (1 day coverage)
```

## Manual backup

Checkpoints are consistent point-in-time copies; for an out-of-band backup simply copy the
latest `{data_dir}/backups/checkpoint-*` directory (or snapshot the volume) — this is safe
while the server is running.

## Restore procedure

### 1. Stop Loomem

```bash
pkill -f loomem-server
```

### 2. Replace RocksDB data

```bash
# Move current data aside
mv {data_dir}/rocksdb {data_dir}/rocksdb.old

# Copy checkpoint
cp -r {data_dir}/backups/checkpoint-2026-04-04-120000 {data_dir}/rocksdb
```

### 3. (Encrypted instances) Confirm the master key is set

If the checkpoint is from an encrypted instance, the matching master key must be present
**before** start, or every encrypted row fails closed. Restore `LOOMEM_AT_REST_MASTER_KEY`
from escrow and confirm its fingerprint matches `master-key-fingerprint.txt` saved beside the
checkpoint (after start, `GET /v1/encryption/status` reports the active fingerprint). For
plaintext instances (no key), skip this step.

### 4. Start Loomem

```bash
./target/release/loomem-server
```

On startup:
- RocksDB opens from the checkpoint
- Tantivy detects schema mismatch → auto-rebuilds index from RocksDB
- WAL replays any pending operations
- All data is restored

### 5. Verify

```bash
curl http://localhost:3030/v1/status
```

Check that `rocksdb_keys`, `tantivy_docs`, and `embeddings_count` match expected values.

## Data retention and hard purge

Deleted memories are soft-deleted (marked with `deleted_at` timestamp) and remain in storage for a recovery window. After the window expires, the hard-purge worker permanently removes them.

```toml
[retention]
soft_delete_days = 30           # 30-day recovery window
hard_purge_interval_secs = 86400  # purge worker runs daily
```

**Hard purge removes:** chunk data, embeddings, entity/relation metadata, and graph references. Purged data cannot be recovered from backups made after the purge ran.

**To recover a soft-deleted memory before purge:** restore from a backup taken before deletion.

## Recovery metrics

| Metric | Value |
|--------|-------|
| RPO (Recovery Point Objective) | 12 hours (backup interval) |
| RTO (Recovery Time Objective) | ~5 minutes (copy + restart + Tantivy rebuild) |

## Notes for container platforms (e.g. Railway)

- A persistent volume mounted at `/data` survives redeployments
- Backups live inside the same volume — single point of failure
- For off-site backup: use your platform's volume snapshot feature, or periodically download a checkpoint
- Before any destructive migration, verify a recent backup exists
