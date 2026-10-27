# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.4] - 2026-10-27

Catch-up release for everything that landed on `main` since v0.5.3: the embedded
dashboard, the MCP 2026-07-28 stateless path, Windows builds, fail-closed startup
without an LLM key or a local embedding model, source fragments with an evidence
contract, named OpenAI-compatible LLM providers, and a run of dependency advisories.
Existing databases and configurations load unchanged.

### Added

- Embedded dashboard v1 (#60): the React SPA in `loomem-dashboard/` is compiled
  into `loomem-server` (rust-embed) and served at `/` on every instance. Screens:
  Connect (MCP endpoint from the page origin, unlock-token reveal, per-client
  recipes for Claude Desktop / Claude Code / ChatGPT / Cursor, live `/v1/status`
  probe), Memory (server-paginated list, substring and semantic search, inline
  edit/delete, version history, related entities) and Settings. Login is a token
  unlock over the existing auth middleware and `/v1/whoami`. New
  `GET /api/dashboard/memory` and `GET /v1/memory-chain/:id`. The release workflow
  and the Dockerfile build the SPA before `cargo`; with an unbuilt `dist/` the
  server still compiles and `/` answers 404.
- MCP 2026-07-28 revision (#63): a stateless protocol path selected by the
  `MCP-Protocol-Version: 2026-07-28` header — no sessions read or minted,
  per-request `_meta` validation, mirrored `Mcp-Method` / `Mcp-Name` header
  checks (SEP-2243) and `server/discover` carrying supported versions,
  capabilities, server info and instructions. Requests without the header, or
  with an initialize-era version, take the existing session path byte-identically.
- OAuth (#62): RFC 9207 `iss` on the authorization success redirect, advertised
  through `authorization_response_iss_parameter_supported` in the AS metadata,
  and `application_type` (web / native) accepted, validated and echoed by dynamic
  client registration.
- Windows support (#83, #85): `loomem-server` builds and runs on Windows (shutdown
  now goes through `tokio::signal`, same behaviour on Unix); `INSTALL_WINDOWS.md`
  documents the build. Release archives still cover Linux and macOS only.
- Search WS-1 (#59): `[search] vector_similarity_floor` (raw-cosine floor for
  the vector channel, `0.0` = off), `[search] decay_match_relief` (a strong
  lexical/vector match relieves the age penalty) and `[search.tier1]` direct
  entity→attribute lookup that answers a factual query from the entity chunk
  index without embeddings, fusion or an LLM, stamping `"tier": 1` (REST) /
  `[tier:1]` (MCP). After the cycle/015 freshness gate `decay_match_relief`
  ships at `0.70` (#75); the floor and Tier-1 stay off.
- `memory_store` accepts an optional `supersedes: <chunk_id>` (#76): the target
  must be alive, in the caller's stream and the head of its chain; a chunk that
  already has a newer version is refused with that id instead of forking the
  chain.
- `/health` reports `llm_key_present: bool` (never the key or a prefix) so
  operators and benchmark pre-flights can detect keyless mode with one GET (#73).
- Named completion providers: `[llm.providers.<name>]` with `base_url` and an
  optional `api_key_env`, selected by `llm.provider`. Every provider uses the
  OpenAI chat-completions format. `api_key_env` set → that env var must hold
  the key; omitted → the provider takes no key (e.g. Ollama, vLLM, a
  gateway). Without the table, `"openai"` keeps resolving to
  `https://api.openai.com/v1` with the existing `api_key` / `api_key_env`.
  This change adds the config, validation and `LlmConfig::active_provider()`
  only; LLM calls still go to OpenAI until the call sites move to it. The
  startup log shows the active provider and where its key comes from (never
  the key itself) (#95).
- `fragments` domain module: deterministic segmentation of a canonical
  conversation source into turn-level fragments (long turns split further at
  paragraph, sentence and whitespace boundaries) that carry the author role,
  the conversation date and an exact byte range into the source. Line-prefix
  turn markers are trusted only when at least two marker lines are present
  and one of them is `user`/`human`; a lone `AI:` bullet inside prose stays
  one unlabelled turn. New `[fragments]` config section (`enabled = false`
  by default) and an optional `fragment` provenance field on stored chunks;
  existing databases and configurations load unchanged. `Config::validate`
  rejects `max_fragment_tokens = 0` and, with `embedding_provider = "local"`,
  a `max_fragment_tokens + prefix_reserve_tokens` above the local embedder's
  512-token window. (`prefix_reserve_tokens` is headroom above the text
  budget — the embedder adds only its two special tokens; Loomem prepends
  no `passage:` prefix.)
- Opt-in fragment persistence on `memory_ingest` (`[fragments] enabled = true`):
  right after the canonical source is stored, its fragments are persisted as
  level-0 chunks (`"{source_id}:f{index}"`, marker kind `fragment`, linked to
  the source through `source_ids` and `fragment`) through the same sink,
  before extraction. Byte ranges index into the source *as persisted*: the
  ingest content is sanitised to a fixpoint up front (this applies to every
  `memory_ingest` call; input that keeps changing after four passes, e.g.
  deeply nested HTML entity encoding, is refused with nothing stored, and
  extraction now sees exactly the persisted text), the source is read back
  from storage before
  segmentation, and a slice the sink would trim is stored with its range
  narrowed to the surviving bytes. Fragments are additive: a failed fragment
  write is reported in the tool text (`Stored N fragments, M fragment writes
  failed.`) and never blocks fact extraction. Like the source, fragments are
  `consolidated` from birth, so raw turns never enter L0 consolidation, the
  pending-L0 counter or the `memory_context` "recent" section; their index
  event date is unknown (never the storage timestamp) and their `memory_search`
  line carries the source's `observed:`/`recorded:` date. New keys
  `provenance_role` (`"claim"` default | `"evidence"` | `"cue"`, the ranking
  knob for fragment chunks; it affects ranking only on fused (vector)
  retrieval — BM25-only queries, i.e. under 5 words without a routing keyword
  or `vector_enabled = false`, apply no provenance multiplier) and
  `token_counter` (`"embedder"` default — the loaded local embedding
  model's own tokenizer, the only counter whose budget is the embedder's;
  with fragments enabled it requires `embedding_provider = "local"` at
  config load, and it fails segmentation loudly instead of falling back if
  no local model is loaded | `"approx"` | `"tiktoken"`, the model-free
  counters for non-local embedding providers and offline tests); unknown
  values fail config load. In search, the content-prefix dedup never
  collapses a fragment against its own source (the source begins with turn
  0, so the two share a prefix) nor fragments of different sources against
  each other. Fragments
  share the source's lifecycle: deleting the source tombstones them with the
  facts, and the startup event-date index repair covers them. With fragments
  disabled (the default) the ingest path performs exactly the writes and
  returns exactly the text it did before.
- Opt-in knowledge extraction controls in `[knowledge_extraction]`:
  `input_chunk_tokens` splits a transcript at line boundaries into smaller
  per-call chunks, and `split_on_truncation` re-extracts a chunk whose
  completion hit the output token limit as two line-aligned halves (at most
  four extra calls per chunk). Both default off; `max_tokens` is now spelled
  out in `config.toml` at its existing default. Extraction outcomes report
  attempted, completed and truncated calls. A truncated reply is still never
  admitted.
- Shared evidence renderer behind the new `[search] evidence_fields` flag
  (default `false`, responses unchanged). When enabled, every `/v1/search`
  hit (cold and cached) and `GET /api/memories/:id` item carries an
  additive `evidence` object with exactly `author`, `conversation_date`,
  `event_date`, `event_date_provenance`, `source_id`, `byte_start` and
  `byte_end` (unknowns are `null`, never a substituted date; only
  calendar-valid ISO dates are emitted), and `memory_search` lines carry
  the same facts, minus the byte range, as a compact bracketed tag before
  the score.
  `event_date_provenance` is `resolved` when the engine recorded the
  temporal cue the date came from and `unknown` otherwise; `explicit` is
  reserved until the extractor persists that distinction.
- Stage diagnostics: a traced search (`trace: true`) now reports
  `trace_metadata.rejections`, one `{chunk_id, stage, reason}` per candidate
  dropped between the existing evidence stages (soft delete, event-date,
  bitemporal and supersession filters, extraction-metadata filters, prefix
  and aggregation dedup, source-agent filters, top-k truncation). The
  `evidence` block itself is unchanged.

### Changed

- **`llm.provider` must name a known provider** (breaking for misconfigured
  setups only). Previously any value was accepted and ignored — completions
  always went to OpenAI. Now a value other than `"openai"` must match a
  `[llm.providers.<name>]` entry, or the config fails to load with
  `llm.provider "<name>" is not defined in [llm.providers]`. Configs with
  `provider = "openai"` (the shipped default) load unchanged. The startup log
  line `LLM: provider=…, model=…` is replaced by `LLM completions: …` and
  `LLM embeddings: …`; the old line printed the embedding model as the
  completion model (#95).
- Startup fails closed without an LLM key (#73, #69, #87): when any of
  `entity_extraction`, `knowledge_extraction` or `dream` is enabled and no API key
  resolves (an empty `OPENAI_API_KEY` counts as missing), the server refuses to
  start and names the env var to export. `LOOMEM_ALLOW_KEYLESS=1` downgrades the
  refusal to a WARN; in that mode `memory_ingest` returns a missing-key error and
  stores nothing, entity extraction is off and dream/consolidation fail, while
  `memory_store` and search keep working. The warning and the config comments no
  longer claim a regex fallback — `fallback_to_regex` only covers consolidation
  compression when the completions call itself fails.
- Startup fails closed when `embedding_provider = "local"` and the model does not
  load ("refusing cloud fallback"), with or without an API key (#89). Neither
  `install.sh`, the release archive nor the Docker image ships the model, so the
  install and deployment docs now include the model fetch or set
  `LOOMEM_EMBEDDING_PROVIDER=openai` / `LOOMEM_EMBEDDING_DIM=1536` (#92).
- `bincode` replaced by an in-tree persisted-row codec (#79, #81): the three
  fixed-shape rows it encoded (embedding vectors, associator centroids, wrapped
  per-scope DEKs) keep the exact wire format, so every row written by earlier
  releases decodes and new rows are byte-identical. No migration. The fixture
  generator lives outside the workspace in `tools/legacy-fixture-gen`.
- `tantivy` is consumed as a pinned git dependency on the `vvooki-sys` fork
  (0.25.0 plus dependency bumps) because no crates.io release clears the `lru`
  and `zstd` advisories (#80, #96); `deny.toml` allows that one git source.
- `GET /api/dashboard/memory` hides superseded versions by default;
  `include_superseded=true` opts in and rows carry `is_latest` /
  `superseded_by` (#76).
- BM25 date-range and entity lanes apply stream scoping at index-query time
  like the vector, graph and plain-BM25 lanes, with a fail-closed guard behind
  them (#61).
- HTML tag stripping in the ingress sanitizer no longer matches across lines.
  Previously a lone `<` early in a multi-turn transcript and a `>` many turns
  later were treated as one tag and everything between them was deleted from
  the stored content. This applies on the default path to every ingress —
  `memory_ingest`, `/v1/store` and every other write or LLM-input path that
  goes through the sanitizer. Single-line tags (`<b>bold</b>`) are stripped as before; a tag
  broken across lines (`<div\nclass="x">`) is now left in place, an accepted
  trade-off since stored content is read by LLMs, not browsers. Entity
  decoding, whitespace collapsing and the `html_stripped` flag are unchanged.
- Knowledge extraction rejects explicitly truncated or refused model completions
  before admitting facts, while retaining the stored conversation source and
  reporting partial extraction failures. Completion metadata omitted by legacy
  compatible providers retains its existing behavior.

- MCP conversation ingestion now preserves a sanitized, searchable source before
  extraction and links each derived fact to it. Extraction failure or cancellation
  retains that source; responses report its ID and any incomplete fact writes.
  Source-linked content and provenance are immutable: correct them by deleting
  the source and ingesting the corrected conversation.
- New ingestion records distinguish event dates, conversation dates (`observed`),
  and storage dates (`recorded`). Missing event dates remain unknown; unresolved
  relative dates without a conversation date are not anchored to the current day.
  Existing records retain their previous date rendering. Date-range searches for
  recognized versioned ingestion records require known event dates, including
  cached, vector and graph results. Legacy unmarked records retain existing event
  dates and their recorded-time fallback when undated. Startup repairs older
  versioned index entries in place without resetting the index; repair failures
  prevent serving stale temporal data. The separate `valid_at` scope is unchanged.
- Deleting an ingestion source atomically tombstones its same-stream descendants
  before retryable index cleanup. Deleting one derived fact retains the source
  and sibling facts and reports that retention. Tombstones retain the existing
  recovery/purge lifecycle; this operation does not promise immediate physical erasure.
  Existing databases require no schema migration; historical provenance is not inferred.
- Cached search results, associations, and context packing check current record
  visibility before returning text, so stale derived indexes cannot override tombstones.
- Graph lookup and display require live per-memory contributions for names,
  aliases and relations, bound to their owner, stream and content generation and
  stored through the existing encryption provider. Legacy support uses explicit
  per-memory entity names and relation tuples; unproven aliases stay hidden until
  re-extraction, and legacy entity types may appear as Unknown.
- Content or stream edits invalidate entity, relation and extraction-completion
  caches. Startup removes obsolete raw JSON extraction-completion hints without
  deleting canonical memories or making model calls. New completion hints contain
  only a static marker.
- Existing encryption backfill covers graph label and relation sidecars; upgrading
  from plaintext still requires the separate name-index rekey step. Physical purge
  removes associated graph sidecars in batched namespace scans. Source-family
  deletion still uses full-store discovery; no reverse index or source-family deletion speedup
  is claimed.

### Fixed

- Local embedder: input is truncated to the checkpoint's 512-token window and a
  failing chunk no longer aborts its whole batch; one oversized chunk used to
  cost up to 50 chunks their embedding, the mechanism behind the "buried entity"
  incident (#68).
- `memory_history` / `GET /v1/memory-chain/:id` traverse soft-deleted versions
  for linkage but never return them, and the chain always contains the queried
  id (the old root-first walk dropped side branches) (#76).
- The reprocess-legacy path persisted the original chunk instead of the linked
  successor returned by `apply_supersede`, leaving a dangling half-link (#77).
- Phone redaction spares whitespace-separated numeric series (chart data,
  measurement columns) and drops the last global replaces (#74).
- Date-range searches apply the authoritative unknown-event-date rule to source
  fragments in vector admission and final filtering, matching the lexical index.
  Conversation and storage dates do not substitute for unknown event dates;
  unfiltered and legacy-record behavior is unchanged (#94).
- Traced search's final `response` snapshot is captured after operation-policy
  finalization and the last event-date check. Returned IDs and order match the
  actual response; restored records are not reported as rejected. The existing
  four-stage evidence contract is preserved, with an additive
  `operation_finalization` rejection reason for displaced primary results (#94).
- Benchmark E1 adapter: the readiness deadline scales with the fragment corpus
  (45 s floor plus 0.5 s per expected chunk) and polls every 500 ms (#93).
- Clippy 1.98 lints and the Audit job unblocked on `main` (#72).

### Security

- `tokio` locked to 1.51.6, RUSTSEC-2026-0386 (`AsyncFd` I/O safety), without
  suppressing the dependency audit.
- `tract` family 0.22.1 → 0.22.2, RUSTSEC-2026-0217 (#64).
- `rustls` 0.23.38 → 0.23.45 (RUSTSEC-2026-0285) and `rand` 0.8.5 → 0.8.8
  (RUSTSEC-2026-0097), lockfile only (#78).
- `lru` ≥ 0.18.2 via the tantivy fork, RUSTSEC-2026-0002 / -0253 (#80).
- `rustls` 0.23.46, `quinn-proto` 0.11.19, `rustls-webpki` 0.103.16,
  `webpki-root-certs` 1.0.10 (RUSTSEC-2026-0338..0349, -0382, -0383), lockfile
  only; `anymap2` accepted as unmaintained (#86).
- `zstd` 0.14.3 / `zstd-safe` 8.2 / `zstd-sys` 2.1.2 via the tantivy fork,
  RUSTSEC-2026-0391 and related (#96).

### Docs

- Ring mark v2: triple weave-gradient ring across the docs site, 3D hero
  animation aligned with the headline (#55, #56, #57).
- `memory_*` tool count synced to 15 across the landing page, MCP instructions,
  tools reference and `llms.txt`; `memory_stats` added to the reference.
- README, landing page, configuration, deployment and `llms-full.txt` no longer
  claim that LLM steps fall back to regex without an API key (#88, #92).
- Install, Docker, Docker Compose and Railway instructions aligned with the
  fail-closed model and key checks (#92); Mem0 comparison clarifies library vs
  server storage requirements.

## [0.5.3] - 2026-07-07

Search-quality release: rare-term guarantee lane with per-channel diagnostics.

### Added

- Rare-term guarantee lane plus per-channel search diagnostics (#53).

### Changed

- Two-tone wordmark as the primary logo, with weave-C gradient ramps warming the ink surfaces (#51, #52).

### Fixed

- Phone-redaction guard against digit runs inside identifiers (#50).

### Security

- Bump `crossbeam-epoch` past RUSTSEC-2026-0204 (#54).

## [0.5.2] - 2026-07-06

Per-stream statistics endpoint.

### Added

- Per-stream statistics endpoint with REST (admin + user), MCP tool `memory_stats`, and CLI `stats` subcommand (#48). Metrics span **health** (chunk/entity/edge counts, index freshness), **retrieval** (search-surface sizing), **consolidation** (dream/merge activity), **distribution** (fact-type and stream spread), **activity** (ingest/access recency), **extraction** (facts-per-chunk yield), and **meta** (schema/version) — all counts and aggregates. Privacy invariant: **no chunk content is ever returned.**

## [0.5.1] - 2026-07-04

Second security-audit remediation round (2026-07-03 audit @96db3a9): findings
F1–F6 fixed, plus supply-chain and container hardening.

### Security

- **OAuth authorization-code flow hardened** (audit F1). `redirect_uri` must exact-match the registered allowlist at authorize and token time, PKCE is mandatory (S256 only, `plain` removed), the auth code is bound to `client_id` + `redirect_uri`, and confidential clients must present their secret (constant-time compare). Registration rejects unsupported `token_endpoint_auth_method` values and empty `redirect_uris`. (#37)
- **Unauthenticated Dynamic Client Registration is bounded** (audit F2). The client store is hard-capped (1000 clients) with LRU-by-last-use eviction and a 10-minute mid-flow grace period matching the auth-code lifetime: at capacity stale clients are evicted, a store full of fresh clients sheds load with 429, redirect validation and the last-used refresh happen atomically, and `/oauth/register` is rate-limited per source (20/min). Behind a reverse proxy, set `LOOMEM_TRUST_FORWARDED_FOR=1` to key the limiter on the proxy-appended `X-Forwarded-For` client instead of the proxy's own address (see the deployment checklist). (#39)
- **Stored content is delimited as data in LLM prompts** (audit F3). Extraction and consolidation wrap chunk text in explicit data/instruction boundaries, and the regex consolidation fallback strips `[CHUNK]` markers, blunting second-order prompt injection via memory content. (#42)
- **`stream_id` charset validation** prevents key-prefix collisions in storage keys (audit F5); MCP ingress is guarded and the charset covers email-derived ids. (#40)
- **Installer fails closed** when the release checksum cannot be verified, instead of warning and installing anyway (audit F4). (#38)
- **Docker image drops privileges**: the server runs as non-root via a gosu entrypoint drop (volumes stay root-owned for existing fleets), the builder image is pinned, and command overrides route through the drop. (#36)
- **`memory_store` preview echoes redacted content**, not the raw argument. (#35)
- **CI actions pinned to full commit SHAs** with an explicit `toolchain: stable` (audit F6). (#41)

### Added

- `cargo-deny` supply-chain policy (`deny.toml`), with advisory ignores aligned to `.cargo/audit.toml`. (#43, #44)
- Forgetting-correctness baseline eval (cycle/010 Phase A): supersede, purge and retention-release behavior locked in fixtures. (#34)

## [0.5.0] - 2026-07-01

Security hardening release — implements items 1–7 of the 2026-07-01 security
& performance audit (PR #32).

### Security

- **Startup fail-safe: an unauthenticated non-loopback bind now refuses to start.** The Docker image binds `0.0.0.0`, so a container without `LOOMEM_AUTH_TOKEN` used to expose the full admin API; it now fails fast with an actionable error. Deliberate opt-out: `LOOMEM_ALLOW_UNAUTH=1` (e.g. behind an authenticating reverse proxy). (#32)
- **No more silent plaintext at rest.** A missing master key logs a prominent PLAINTEXT warning, and the Docker image sets `LOOMEM_AT_REST_EXPECT_ENABLED=1` — networked deployments must provide `LOOMEM_AT_REST_MASTER_KEY` or explicitly opt out. (#32)
- **Per-stream rate limiting on hot paths.** The previously dead `RateLimiter` is wired into `/v1`/`/api` (429 + `Retry-After`) and the MCP dispatcher (tool error): `[rate_limit]` in `config.toml`, off by default, enabled in the Docker image. Applies to every caller — no admin exemption (single-user: everyone is admin). (#32)
- **Constant-time bearer-token comparison** (`subtle::ConstantTimeEq`) closes a theoretical timing side-channel on the API key. (#32)
- **Master key and cached DEKs are zeroized** on provider drop, FIFO eviction, and `flush_cache` (defense-in-depth for after-teardown memory). (#32)

### Added

- Vector search now warns loudly when stored embeddings mismatch the query dimension (silent BM25-only degradation before), naming the `--reembed` remedy. (#32)

### Changed

- **HTTP connection-pool settings for the shared OpenAI client are now config-driven** (`[llm].pool_max_idle_per_host`, `pool_idle_timeout_secs`, `tcp_keepalive_secs`) instead of hardcoded, and a single `LlmConfig::build_http_client` is used by both the server and the CLI re-embed path. The idle pool is bounded and actively recycled so long-running instances don't accumulate stale keep-alive sockets (silent ingest degradation). **Note:** the idle-timeout/keep-alive defaults are 30s/30s — more aggressive recycling than the previous hardcoded 90s/60s (#24); deployments upgrading without the new config keys will pick up the shorter intervals via `#[serde(default)]`. (#29)
- Extraction now separates a successful-but-empty result (2xx, zero facts) from a real transport/parse failure: a windowed `extraction_empty` counter (surfaced in `memory_status` / `llm_failures_recent`, excluded from the failure total) plus a debug log carrying HTTP status and response body length, so silent "Extracted 0 facts" degradation is visible. (#29)

## [0.4.1] - 2026-06-23

### Security

- **MCP callers can no longer self-elevate trust.** `memory_store` derived the trust tier from the caller-supplied `source` with no authority check (introduced in 0.4.0, #9), so any client — including a prompt-injected LLM — could request full-trust `a1`. MCP writes are now clamped to at most `a2` unless the instance opts in via the new `server.honor_caller_trust_source` setting (default `false`). Single-user/dogfood instances can enable it; cloud/multi-client stays safe by default. (#12)

### Fixed

- `memory_search` now emits a `[trust:?]` sentinel when a result's chunk cannot be loaded (read error / index-vs-store desync), instead of silently dropping the trust tag. (#12)
- Filled `provenance_role` in integration-test fixtures missed by #6, so `cargo test -p <crate>` compiles cleanly. (#12)

## [0.4.0] - 2026-06-23

### Added

- **`relations` parameter on `memory_store`** — callers can supply explicit subject/relation/object triples that are injected as deterministic graph edges (graph-only, capped, aliases resolved from `entities.toml`), instead of relying solely on extraction. (#10)
- **MCP trust tier derived from source** — `memory_store` now derives the trust level from the write source rather than hardcoding it, and `memory_search` surfaces each result's trust tier and score multiplier, so full-trust memories visibly outrank derived ones. (#9)

### Fixed

- **Consolidated chunks are now embedded** — both background consolidation (`L1:` chunks) and `memory_dream` (`dream:` chunks) route the merged chunk through the configured embedder / shared embedding queue, so consolidated facts receive a vector instead of being stuck permanently "pending" in `memory_status` (retrievable by BM25 only). The embedder is selected the same way as everywhere else (local-first, OpenAI fallback), keeping the vector dimension consistent with the index. (#7)
- **`/v1/embed-missing` back-fill works without an OpenAI key** — the back-fill now embeds through the configured embedder and requires an API key only when no local embedder is available, so chunks left without a vector (e.g. previously-stuck consolidated chunks) can be recovered on keyless/local instances. (#8, #11)

## [0.3.0] - 2026-06-23

### Added

- **MemIR trust-provenance scoring** — chunks now carry a `ProvenanceRole` (`Claim`/`Evidence`/`Cue`, default `Claim`), and hybrid retrieval applies a trust + provenance multiplier to the fused score before ranking, so full-trust (`a1`) memories outrank derived (`a2`) ones for equally relevant content. Backward-compatible: existing databases deserialize unchanged via `#[serde(default)]`, no migration. (#6)
- **Auto-dream consolidation trigger** — opt-in background consolidation fires after a configurable number of new chunks per stream; the configured shared stream is excluded and a cooldown prevents thrashing. (#3)
- **`chunk_id` exposed in search results** plus per-stream embedding readiness in `memory_status`, so callers can trace, rate, and inspect specific chunks. (#4)
- **Environment overrides** for the embedding provider/dimension and the consolidation interval, easing per-deployment configuration.
- **Branded OAuth authorize page** rendered in the Loomem layout.

### Fixed

- Orphan entity nodes are pruned on `memory_delete`, keeping the entity graph GDPR-correct.
- Auto-dream is opt-in and excludes the configured shared stream.
- Cheaper embedding-existence probe and a standalone id suffix in search results.

### Changed

- Bumped `quinn-proto` and `memmap2` to address RUSTSEC-2026-0185 and -0186.
- Added `CLAUDE.md` + agent-conventions for AI code generators; refreshed landing/Quickstart/ChatGPT-Apps documentation.

## [0.2.2] - 2026-06-17

### Added

- **Custom extraction topics are filterable** — operator-configured `[knowledge_extraction].topics` keys (e.g. `risk_item`, `contact`) are preserved in the new `ExtractionMeta.topic` field instead of silently collapsing to `fact`, and the `/v1/search` `fact_type` filter matches them. (Greptile P1)

### Fixed

- **Consolidation prompt** now advertises the `experience` type, so procedural lessons keep `FactType::Experience` through L1 consolidation instead of being relabeled `event`/`fact`/`preference`. (Greptile P1)
- **Docs** — added Codex to the "Which LLM clients" FAQ (README + landing page, visible and JSON-LD) to match the hero/persona/architecture lists; made the installation guide public-first and demoted the private-fork `gh`/`GH_TOKEN` flow to an optional note. (Greptile P2)

## [0.2.1] - 2026-06-17

### Added

- **Native stdio↔HTTP MCP bridge** — `loomem-cli mcp-stdio --url http://127.0.0.1:<port>` proxies a stdio MCP client (the Claude desktop app, Cowork, Cursor, …) to the server's HTTP `/mcp` endpoint, with **no Node/npx required** (a drop-in replacement for `npx mcp-remote`). It reads newline-delimited JSON-RPC on stdin, POSTs each message to `/mcp`, captures and echoes the `mcp-session-id`, and writes replies to stdout; notifications get no reply. `--url` is now a global flag.
- **Installer port selection** — `install.sh` and the macOS turnkey installer detect a busy port, suggest a free one, and prompt for it (works under `curl | sh` via `/dev/tty`); set `LOOMEM_PORT` to skip the prompt. The chosen port is written into `config.toml`.

### Fixed

- **Robust `/dev/tty` probe** in the installers: the port prompt no longer prints a spurious error when there is no controlling terminal (CI, pipes), and cleanly falls back to auto-selecting a free port.

### Changed

- **Docs** — the Claude desktop app / Cowork connection is now documented as a stdio bridge (native `loomem-cli mcp-stdio` recommended, `mcp-remote` as the universal fallback) across the README, landing page, quick-start, and user guide, clarifying that the desktop app cannot use a bare `http://localhost` connector.

## [0.2.0] - 2026-06-16

First public release.

### Removed

- **Dashboard** (cycle/004): the React web UI, `/api/dashboard/*` REST endpoints, static file serving, and the Node build stage. Loomem is now a headless engine (MCP + REST + CLI). The dashboard will return as a separate project.
- **File registry** (cycle/005): the 8 `file_*` MCP tools, `/v1/files*` REST endpoints, the `loomem-parsers` crate, memory↔file links (`file_refs`), and the `doc_abstract` search signal. MCP now exposes 14 `memory_*` tools. Existing databases remain readable (the removed fields are ignored on deserialization).

### Added

- **Release pipeline** (cycle/006): tagged releases (`v*`) publish prebuilt binaries for Linux (x86_64, aarch64) and macOS (aarch64, x86_64) with SHA256SUMS.
- **`install.sh`** (cycle/006): one-line installer for prebuilt binaries (no sudo, `~/.loomem` by default).
- README: install matrix and MCP client recipes (Claude Code, claude.ai remote connector, ChatGPT custom connector, OpenClaw).
- **Private-repo install support** (cycle/009): `install.sh` picks a download source automatically — `LOOMEM_BASE_URL` mirror → `GH_TOKEN`/`GITHUB_TOKEN` (GitHub API assets) → logged-in `gh` CLI → public URLs. Verified end-to-end against a simulated release.
- **Docs** (cycle/009): full [installation guide](docs/installation.md) (verify, PATH, upgrade, uninstall, troubleshooting) and a maintainer [release runbook](docs/release-runbook.md).
- **Local embeddings, keyless by default** (cycle/010): fresh installs compute embeddings on-device via a bundled ONNX model (`multilingual-e5-small`, 384-dim) using the pure-Rust `tract` runtime — no API key, nothing leaves the machine. New `[llm].embedding_provider` (`"local"`/`"openai"`) separates embeddings from the completions provider; `embedding_model_path` and `scripts/fetch-embedding-model.sh` (sha256-verified) manage the model. The completions LLM stays optional (regex fallback without a key).
- **Embedding-dimension guard** (cycle/010): the server records the dimension a database was built with and refuses to start on a mismatch (e.g. switching provider/model) instead of silently mixing vector sizes.
- **`loomem-server --reembed`** (cycle/010): offline maintenance mode that recomputes all vectors with the configured provider and records the new dimension.
- **macOS turnkey installer** (cycle/011): a self-extracting single-file `.command` for Apple Silicon — no terminal, no API key. Extracts binaries + the int8 local embedding model (~113 MB) to `~/.loomem`, runs the server as a background LaunchAgent (auto-start at login), and wires up Claude Code. Builder `scripts/make-macos-installer.sh`, stub, LaunchAgent template, and `~/.loomem/uninstall.command` (keeps `data/` by default). See [docs/macos-install.md](docs/macos-install.md). Not yet codesigned/notarized — install uses a one-time right-click→Open.

### Changed

- **Default embedding provider is now `local`** (cycle/010), with `embedding_dim = 384`. **Breaking for existing databases:** a database built with OpenAI embeddings (1536-dim) will refuse to start under the new default until re-embedded (`loomem-server --reembed`) or until the config is set back to `embedding_provider = "openai"` / `embedding_dim = 1536`. Configs that predate the `embedding_provider` key are read as `"openai"`, so in-place upgrades keep their current behavior unless the config is changed.

### Fixed

- **Release workflow** (cycle/009): Intel macOS builds moved from the retired `macos-13` runner to `macos-15-intel` (would have failed on first live run); Linux builds pinned to `ubuntu-22.04` so binaries run on glibc ≥ 2.35 (24.04 builds required `GLIBC_2.39`, verified live with v0.2.0-rc1); checksums use `sha256sum` when available; `-rc`/`-beta`/`-alpha` tags are published as prereleases so unpinned installs never receive them.
- **First run** (cycle/009): the installer seeds `entities.toml` from the example — the server requires it at startup, so a fresh install now boots without manual steps.

### Changed

- `scripts/railway-entrypoint.sh` renamed to `scripts/docker-entrypoint.sh`; `railway.toml` removed from the repo (inline example remains in the deployment docs).
- `synonyms.toml` is now gitignored (personal vocabulary); a generic `synonyms.toml.example` ships instead.
