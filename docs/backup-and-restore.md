# Backup and Restore

## What is backed up

RocksDB checkpoint — a point-in-time snapshot of all data:
- All chunks (L0, L1)
- Embeddings (stored in RocksDB column family)
- Knowledge graph (entities, edges)
- Cost tracking data
- Wrapped per-stream encryption keys (when at-rest encryption is enabled)

**Not in the RocksDB checkpoint:**
- The Tantivy full-text index. A schema mismatch or count drift may trigger a rebuild, but neither proves that an existing index matches this checkpoint. Rebuild explicitly after a restore.
- Loomem's separate intent log, normally `{data_dir}/wal`. This is not the RocksDB WAL inside a checkpoint. Replaying an intent log from a different point in time can change the restored data; follow the restore procedure below.

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

- **Automated (12h-worker) checkpoints are still covered.** Every checkpoint taken with the
  same master key shares its fingerprint. The escrow record (key + fingerprint + activation date)
  therefore identifies the correct key for **any** checkpoint by its timestamp, even one the
  worker wrote without a per-checkpoint fingerprint file. The `master-key-fingerprint.txt` step
  above is a convenience for fast lookup, not a recovery prerequisite; automating it as a
  post-backup hook is a possible future nicety, not a gap that blocks recovery.

- **Without the matching master key, an encrypted checkpoint is unrecoverable.** Routing,
  embeddings, and Tantivy stay readable, but every encrypted row class fails closed.

Keep at least one copy of the master key (with its fingerprint and activation date) in a
secret store that is **separate from the data volume and the backups** — a password manager,
cloud secret manager, or printed escrow. Verify that the escrowed value matches the running
instance's fingerprint (`GET /v1/encryption/status`). Master-key rotation is not currently
supported: changing the key does not re-wrap existing DEKs and makes the encrypted data
unreadable. See [Security overview](SECURITY.md).

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

For an out-of-band backup, copy a completed `{data_dir}/backups/checkpoint-*` directory.
The checkpoint is a point-in-time copy of RocksDB; it does not establish cross-component
consistency with the separate intent log.

The RocksDB checkpoint is not a transactionally paired snapshot of Loomem's separate intent log. An automatic checkpoint can be taken while a delete or purge is still in progress. A checkpoint alone therefore does not prove that those operations finished. After restoring it without a matched intent log, explicitly verify that expected deletions have taken effect before reconnecting clients.

## Restore procedure

### 1. Stop Loomem

Disconnect clients and stop the server completely. For the restore, keep the server isolated from clients and temporarily set `enabled = false` in the `[scheduler]` section of its configuration. Resume normal access and scheduler settings only after verification.

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

### 2b. Set aside the separate intent log

With the server stopped, move the existing intent-log directory to a safe, uniquely named location outside the active data directory. The default is `{data_dir}/wal`, but check `[storage.intent_log].dir` in the actual configuration. Do not reuse the old intent log with this checkpoint: pending delete or purge intents can change the restored RocksDB on startup. Preserve the old log for investigation; the server creates an empty intent log when enabled. Do **not** remove RocksDB's own WAL files inside the checkpoint.

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

RocksDB opens from the checkpoint. Startup may rebuild Tantivy for a schema mismatch, rebuild flag, or configured count drift, but none of these guarantees that an existing index matches the restored data.

### 5. Rebuild the full-text index

Keep clients disconnected and the scheduler disabled. After startup, call the rebuild endpoint using an admin token if authentication is enabled (omit the header if it is disabled):

```bash
curl -fsS -X POST http://localhost:3030/v1/rebuild-tantivy \
  -H "Authorization: Bearer <admin_token>"
```

HTTP 202 means the rebuild started in the background, not that it succeeded. Check the server log for `Tantivy rebuild: starting` followed by `Tantivy rebuild complete`; if the log reports `Tantivy rebuild failed`, stop and resolve the failure before restoring service. The rebuild replaces the index from the restored RocksDB and can take time.

### 6. Verify

```bash
curl -fsS http://localhost:3030/v1/status \
  -H "Authorization: Bearer <admin_token>"
```

Omit the header if authentication is disabled. Check the checkpoint's expected data and representative searches, including records expected to be present or absent. Check `undecodable_chunks` is zero and investigate errors in the logs; a missing key or corrupt rows can make a rebuild incomplete. The counts in `config_summary` are diagnostic only: `rocksdb_keys` is an estimate, and matching counts do not prove that the index contains the right documents. Once verification succeeds, stop the server, restore the previous `scheduler.enabled` setting, restart the server, and then reconnect clients.

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
| RTO (Recovery Time Objective) | Depends on data size, storage speed, rebuild and verification; measure for your deployment. |

## Notes for container platforms (e.g. Railway)

- A persistent volume mounted at `/data` survives redeployments
- Backups live inside the same volume — single point of failure
- For off-site backup: use your platform's volume snapshot feature, or periodically download a checkpoint
- Before any destructive migration, verify a recent backup exists
