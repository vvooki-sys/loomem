# Security overview

*Last updated: 2026-06-16*

This document describes Loomem's security model: authentication, encryption at rest, data in transit, PII handling, and logging. Where Loomem does not have a control in place, the gap is named explicitly under [Known limitations](#known-limitations).

---

## Threat model summary

**With at-rest encryption enabled (see below), Loomem blocks:**

- **Storage volume snapshot or full-disk dump leaking memory content.** Chunk content and entity-graph entries are encrypted at the row level in RocksDB before the storage layer sees the bytes.
- **RocksDB backup file leaks** (e.g., a checkpoint copied off the host). The chunk and entity-graph rows inside the checkpoint are ciphertext.
- **Decommissioned or reassigned storage hardware.** Application-layer ciphertext provides protection independent of any host-level disk encryption.

**Loomem does NOT block:**

- **Runtime compromise.** If an attacker obtains the running `loomem-server` process memory (environment access, core dumps), the master key is readable in process memory and all stream-level DEKs decrypt.
- **Operator access.** Whoever controls the host environment holds the master key and can decrypt all stored content. Loomem is not zero-knowledge, operator-blind, or end-to-end encrypted: plaintext crosses the trust boundary at the server process, which decrypts to serve responses.
- **LLM provider reads.** Memory content is sent in plaintext to the configured LLM provider (e.g., OpenAI) for embedding generation, consolidation, and contradiction detection. Review your provider's data-retention policy.
- **Full-text index and embedding exposure.** The Tantivy index requires plaintext tokens for BM25 search, and embedding vectors are stored unencrypted (they are required for cosine search and are partially invertible per published research). See [Known limitations](#known-limitations).

---

## Authentication

Loomem uses a single API key:

- Set the environment variable named by `server.auth_token_env` in `config.toml` (default: `LOOMEM_AUTH_TOKEN`).
- All requests except `GET /health` must carry `Authorization: Bearer <key>`.
- If no key is configured, the server runs in **local passthrough mode**: every request is accepted with admin privileges. Only use this for local development on a trusted machine — never expose an unauthenticated instance to a network.

MCP clients that cannot send custom headers can use the built-in OAuth 2.0 flow (`/oauth/register`, `/oauth/authorize`, `/oauth/token`): the user enters the API key once during authorization, and the resulting access token is equivalent to the key.

Recommendations:

- Generate a long random key (64+ hex characters).
- Store it only in environment variables — never in `config.toml` or version control.
- Rotate it if you suspect exposure; rotation is just changing the env var and restarting.

---

## Data at rest

### Without encryption enabled (default)

| Surface | Plaintext? | Notes |
|---|---|---|
| RocksDB chunks (`chunk:L*:{id}`) | yes | Compressed (LZ4/Snappy) but not encrypted at the application layer. |
| RocksDB entity / relation / graph records | yes | Same. |
| Tantivy full-text index | yes | Plaintext is a functional requirement for BM25 search. |
| Embedding vectors (`embeddings` column family) | yes | Required for cosine search. |
| WAL / intent log | n/a | Contains operation type + chunk id only — no content. |

If your hosting provider encrypts the storage volume at the host layer, you get the at-rest protection typical of a managed platform — but a leaked database checkpoint is readable.

### With encryption enabled

Set `LOOMEM_AT_REST_MASTER_KEY` (32-byte, base64-encoded) to enable application-layer envelope encryption:

- **Algorithm:** AES-256-GCM.
- **Key hierarchy:**

  ```
  master_key (env var LOOMEM_AT_REST_MASTER_KEY)
     └─ wraps → per-stream data-encryption key (DEK)
          └─ encrypts → chunk content, entity names, relation data
  ```

  Per-stream DEKs are generated lazily on first encrypted write and persisted as wrapped blobs in a dedicated RocksDB column family. Master-key rotation re-wraps the DEKs (fast), not every chunk.

- **Encrypted:** chunk content and metadata, entity/relation value blobs, graph entity names and aliases.
- **Not encrypted (by design):** embedding vectors and the Tantivy index (functional requirements for search), and routing metadata (chunk id, level, stream id, timestamps, supersede chain) needed for filtering, retention, and decay.
- **Legacy data:** plaintext rows written before encryption was enabled are recognized by the absence of a magic prefix and remain readable. `POST /v1/admin/backfill/encrypt-at-rest` walks existing records and encrypts them idempotently; check progress at `GET /v1/admin/backfill/encrypt-at-rest/status`.
- **Status endpoint:** `GET /v1/encryption/status` reports whether encryption is active and the master-key fingerprint (a one-way digest, safe to record alongside backups).
- **Fail-closed expectation:** set `LOOMEM_AT_REST_EXPECT_ENABLED=1` to make the server refuse to start without a master key — protects against accidentally booting an encrypted dataset in plaintext mode.

**Back up the master key separately from the data.** An encrypted checkpoint without the matching master key is unrecoverable. See [Backup and Restore](backup-and-restore.md).

---

## Data in transit

- Loomem itself serves plain HTTP; run it behind a TLS-terminating reverse proxy or a platform that provides HTTPS.
- Outbound HTTPS to the LLM provider uses `reqwest` with `rustls`.
- The MCP transport is JSON-RPC over HTTPS on `/mcp`, Bearer-authenticated. No custom encryption layer beyond TLS.

---

## PII filtering

When `[pii]` is enabled in `config.toml`, phone numbers, email addresses, national ID numbers, and blocklisted terms are redacted **before every LLM API call** (consolidation, extraction, dream). Redaction replaces matches with `[PHONE]`, `[EMAIL]`, `[ID]`, `[REDACTED]` tokens. See [Configuration](configuration.md#pii).

Note that ingest-time sanitization (HTML stripping, instruction-injection detection) logs suspicious input but does not block it — see [Architecture](architecture.md).

---

## Secrets management

Environment variables that may contain secrets:

| Env var | Purpose |
|---|---|
| `OPENAI_API_KEY` (or the var named by `llm.api_key_env`) | LLM API (embeddings + extraction) |
| `LOOMEM_AUTH_TOKEN` (or the var named by `server.auth_token_env`) | API Bearer key |
| `LOOMEM_AT_REST_MASTER_KEY` | Envelope-encryption master key |
| `TELEGRAM_BOT_TOKEN`, `LOOMEM_TELEGRAM_CHAT_ID` | Optional cost-alert webhook |

Keep all of these in your platform's secret store or environment configuration — never in `config.toml`, the repository, or build artifacts.

---

## Logging

Loomem logs to **stdout** (12-factor); it does not ship, store, or alert on logs itself.

- Set `LOOMEM_LOG_FORMAT=json` for structured JSON output in production. The default is a compact human-readable format for local development.
- Security-relevant events (auth failures, deletes, admin actions) are tagged with `target: "audit"` — a log shipper can filter on this target to isolate the security stream.
- If you run Loomem in production, wire stdout to a log sink with a retention policy, and consider alerting on spikes of `target=audit` errors and repeated auth failures.

---

## Known limitations

1. **Runtime compromise reveals the master key.** Process-memory access decrypts everything. Mitigation would require a TEE-style deployment, which Loomem does not currently support.
2. **Tantivy index content is plaintext.** Functional requirement for BM25 full-text search.
3. **Embedding vectors are plaintext.** Functional requirement for vector search; embeddings are partially invertible.
4. **LLM provider sees plaintext** at ingest and consolidation time.
5. **Backup encryption is inherited, not added.** RocksDB checkpoints rely on the at-rest row encryption above (when enabled) plus whatever volume encryption your host provides; Loomem does not add a separate backup encryption layer.

Loomem ships no compliance attestations (SOC 2, HIPAA, ISO 27001). If you need them, they are properties of your deployment and organization, not of this software.

---

## Reporting security issues

If you discover a security issue in Loomem, please report it privately to the project maintainers (for example via your code host's private vulnerability reporting feature) rather than filing a public issue with reproduction steps.
