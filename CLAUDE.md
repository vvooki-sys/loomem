# CLAUDE.md — rules for AI code generators in the Loomem repo

Hard rules every AI assistant (Claude Code, Cowork, Copilot, Cursor, …) must follow when touching this repo. Some are enforced by CI; the rest are enforced in review. Deep rationale, complexity carve-outs and edge cases live in [`docs/agent-conventions.md`](docs/agent-conventions.md) — this file stays short on purpose, because it loads into context every session.

**What Loomem is.** An open-core, single-user memory engine (Apache-2.0, `vvooki-sys/loomem`), forked from Engram. The repo is **public** — assume external readers and their agents. Public positioning: *"the open-source context layer for LLM agents."* It is **memory-only** (the `memory_*` MCP tool set, no file layer) and **single-user** (no accounts, RBAC, SSO, multi-tenancy). Those two product boundaries are frozen (see § Product boundaries).

---

## Router — read the subset that applies

- **Changing Rust code** → § Five hard rules + § Build & run + § Repo map + § Rust rules.
- **Docs / landing / content / GEO** → § Docs & site + hard rule 5 (surgical). A docs task never touches Rust.
- **Dashboard (`loomem-dashboard/`, React SPA)** → § Build & run (dashboard bullet) + hard rule 5. A dashboard task touches no Rust beyond `loomem-server/src/spa.rs`.
- **Cutting a release** → § Releases.
- **Always** → § Five hard rules, especially live-state (rule 4), product boundaries (rule 3), secrets (rule 1).

---

## Five hard rules (never break these)

1. **Secrets & personal data never get committed.** No API keys, tokens, passwords, production endpoints — and no real names or private data in `entities.toml` / `synonyms.toml` (gitignored; only `*.example` variants are tracked). Any user content that gets persisted *anywhere*, including derived/audit fields like `original_content`, must pass through `sanitizer` + the PII filter — not just the visible `content`. `.gitleaks.toml` catches some leaks; you are the first gate. See something sensitive in a diff → stop and tell the user.

2. **The local gate must be green.** `cargo fmt --check && cargo clippy --workspace -- -D warnings` is byte-identical to CI and to the pre-commit hook. If it isn't green, the work isn't done. **Never use `--no-verify`.**

3. **Product boundaries are frozen.** Loomem is **memory-only** (the file layer was removed in loomem/005 — do not re-add file tools, parsers, or `/v1/files`) and **single-user** (no accounts, RBAC, SSO, magic-link, transactional email, or multi-tenant "workspaces"; streams organize one person's memory, they are not tenancy). A brief asking for any of these = **STOP and ask the owner** (team features live in Engram).

4. **Trust live state, not memory.** Cross-conversation memory (Engram/Loomem MCP, Cowork `MEMORY.md`) describes "what was true when written," not "what is now." Before relying on any file, function, flag, or default a memory names, **verify it still exists in the repo.** When memory and `git log` / `gh pr list` disagree, trust live. Likewise, docs adapt to code — never copy a value from one doc page to another; verify it against `config.toml`/source.

5. **Surgical and compatible changes only.** Touch only what the task needs; no drive-by refactors, no "while I'm here" cleanups (report unrelated dead code, don't delete it). Match the file's existing style. And: any new field on a struct persisted to RocksDB (`Chunk`, `ExtractionMeta`, `FactType`, …) **must** be `#[serde(default)]`, or you break deserialization of existing databases.

---

## Build & run

Crates: `loomem-core` (domain + storage), `loomem-server` (HTTP/MCP), `loomem-cli` (stdio↔HTTP MCP bridge + ops), `loomem-migrate` (probes/migrations). Not a crate: `loomem-dashboard/` (React/Vite SPA, embedded into `loomem-server` by `rust-embed` from `loomem-dashboard/dist/`, see `loomem-server/src/spa.rs`).

- **`loomem-core` and `loomem-server` compile RocksDB (C++) and need `libclang`.** Light/sandboxed environments often can't build them — don't assume a clean `cargo build` everywhere. CI installs `libclang-dev`.
- **Dashboard build order: front end first, then cargo.** `cd loomem-dashboard && npm ci && npm run build` writes `dist/`; `cargo build -p loomem-server` then embeds it. With an empty `dist/` the server still compiles and `/` answers 404. `dist/` is gitignored — never commit it. The Rust gate does not cover the SPA: CI has a separate job (`npm run lint`, `npm test`, `npm run build`), so run those locally before pushing dashboard changes.
- **Fail-closed startup.** With the default config the server **refuses to start** without the local embedding model (`embedding_provider = "local"`, no cloud fallback) and without an LLM key while LLM-backed subsystems are enabled. `LOOMEM_ALLOW_KEYLESS=1` opts into degraded keyless mode (`memory_ingest` stores nothing, extraction off, dreaming fails; `memory_store` and search work). Expect this when running the server locally or in tests — don't "fix" it by weakening the check.
- **Windows:** `loomem-server` builds and runs on Windows (LLVM for `libclang`, `LIBCLANG_PATH`; see `INSTALL_WINDOWS.md`). Release archives still cover Linux and macOS only.
- **`loomem-cli` pulls no RocksDB/Tantivy** (just tokio/reqwest/clap) — it compiles in seconds. Use it for fast bridge/CLI iteration.
- **Fast feedback loop:** `cargo check -p <crate>` and `cargo test -p loomem-core --lib` run in seconds. A full `cargo build --workspace` (release especially) is slow — don't run it in a loop.
- **macOS aarch64 release gotcha:** the release build SIGSEGVs rustc (LLVM SLP vectorizer, ThinLTO) without `-C no-vectorize-slp`. That flag is already pinned for the Mac target in `.cargo/config.toml` — don't remove it. Linux/Docker/Railway are unaffected.
- **Binary location:** if a maintainer's global `~/.cargo/config.toml` redirects `target-dir`, release binaries are **not** under `./target/release`. Check the effective target dir before hunting for a binary.

---

## Repo map (where things live)

- **Storage / domain:** `loomem-core/src/storage.rs` (RocksDB, `Chunk`), `tantivy_index.rs`, `hybrid_search.rs`, `graph/`, `crypto/` (+ `encrypt_backfill/`) for at-rest encryption.
- **LLM prompts & extraction:** `loomem-core/src/llm.rs`, `memory_extractor.rs` (extraction prompt), `dream.rs`, `consolidation.rs`, `advisor.rs`.
- **MCP surface:** `loomem-server/src/mcp/` — `dispatcher.rs` (tool dispatch, source of truth for the tool set), `router.rs`, `handler.rs`, `tools.rs`. Canonical handshake text: `loomem-server/mcp_instructions.md`.
- **HTTP handlers:** `loomem-server/src/handlers/` (`search.rs`, `ingest.rs`, `admin.rs`, `purge.rs`, …).
- **Config:** `config.rs` composes per-module sub-configs (`manifest/config.rs`, `access_audit/config.rs`, …). Runtime config: `config.toml`.

---

## Rust rules

Full tables, carve-outs, and the "aspirational vs actually enforced" breakdown are in [`docs/agent-conventions.md`](docs/agent-conventions.md). The short version:

- **Complexity (review-enforced, not CI):** per function CC ≤ 10, COG ≤ 15, SLOC ≤ 100, args ≤ 6; per file SLOC ≤ 700, MI ≥ 40. Already-over files must not get worse. Before adding an `if` to a CC ≥ 10 function, **stop and extract**. (There is no `check-complexity.sh` yet — a critic pass verifies with `lizard`.)
- **Error handling:** no `.unwrap()` / `.expect()` / `panic!` / `todo!` / `unimplemented!` in production paths (test/bench/example code and a final `main()` are exempt). Propagate with `?` and `.context("…")`. Domain errors → `thiserror` (in `loomem-core`); handler errors → `anyhow`.
- **Numeric conversions:** no `as` for numeric casts — use `u32::try_from(x).context(…)?`. `as` only for documented `// truncation intentional: …`, FFI, or same-type.
- **Embedding dim-guard:** `embedding_dim` must match the provider (`local`/multilingual-e5-small = 384, `openai`/text-embedding-3-small = 1536). Read it from config; a mismatch with the existing index corrupts search.
- **Layers (non-negotiable):** `[HTTP/MCP handler] → [loomem-core domain] → [storage/adapter]`. Handlers don't call storage directly; storage doesn't import HTTP types; `loomem-core` doesn't import axum (inject I/O traits instead).
- **Config:** one `*Config` struct per module, declared next to its code; the root `Config` only composes sub-configs. `config.toml` is "all settings required, no hardcoded defaults" — new param goes in both `config.toml` and the loader.
- **Tests:** new public fn → unit test; new handler/MCP tool → integration test with a JSON fixture. No storage mocks in integration tests (use tempdir/in-memory trait impls). Deterministic only (no `sleep` > 1ms, no system clock without a `Clock` trait). CI runs `cargo test --workspace --lib`; run full `cargo test --workspace` locally before pushing.
- **Dependencies:** no new crate without justification (why it can't be ~50 lines in-tree) + `cargo audit` clean + recent maintenance. Pin versions centrally in `[workspace.dependencies]`.

---

## Docs & site

A docs/landing/content task **never touches Rust** (and never `search.rs` / fusion / retrieval). Where things live:

- `docs/*.md` — source of truth for guide content.
- `docs/guide/*.html` — **hand-maintained HTML mirrors of `docs/*.md`. There is no generator** — when you change a `.md`, sync the matching `.html` by hand.
- `docs/index.html` — hand-written landing page.
- Web-root / GEO files live under `docs/` (the site root), not the repo root: `docs/robots.txt`, `docs/sitemap.xml`, `docs/llms.txt`, `docs/llms-full.txt`, `docs/og-image.png`, JSON-LD inside `docs/index.html`, `docs/assets/` (banner, logo). There is no `CNAME` file in the repo.
- `loomem-server/mcp_instructions.md` — canonical MCP handshake text.

**Anti-drift (this bit earns its keep):** before writing any value into docs — a default, a config key name, a behavior — verify it against `config.toml` or the source. **Never copy a value from another docs page**; page-to-page copying is the root cause of drift (we have hit: backup retention, "RRF" vs weighted fusion, `provider` vs `embedding_provider`, OpenAI vs local embedding default). A user-facing claim (clients, install steps, feature list) must be synced across **all** surfaces at once: `README.md` + `docs/index.html` + `docs/guide/*.html` + `docs/*.md`.

**Security page is dated — keep it current.** The security page (`docs/SECURITY.md` + its mirror `docs/guide/security.html`, served at `loomem.ai/guide/security`) carries a `Last updated: YYYY-MM-DD` line under the H1. **Any change to its security content must bump that date — in both files, to the same value** (the date of the change, not a copied old one). A pure typo/markup fix that changes no security claim may leave it; when in doubt, bump. The date lives only on the security page — don't spread it to other guide pages unless asked.

**Naming canon:** brand is **Loomem**. The category label is **"context layer"**; **"memory"** is allowed as a benefit/bridge word in marketing copy — the canonical hero is H1 **"One memory. Every AI tool."** with the mantra *"Swap the model, switch the tool — your context follows."* as sub-headline. *"memory engine"* stays reserved for the one-line technical/GEO definition. Team tier is **"Loomem T"**. **"Engram" never appears in public-facing copy.**

**Tool-set caveat:** Loomem exposes the **`memory_*` set only** — the dispatcher (`loomem-server/src/mcp/dispatcher.rs`) is the source of truth for which tools exist; don't hardcode a count in prose that can drift. There is **no file layer** in Loomem. `loomem.ai` and `try.loomem.ai` are marketing/trial pages, not an engine endpoint. Never "discover" a tool set against any live server and document it as Loomem features — a hosted instance may be *Engram* (which does expose `file_*`); the dispatcher is the only truth.

---

## Commits, releases, workflow

**Commits** — Conventional Commits + DCO sign-off (see `CONTRIBUTING.md`):

```
feat: add temporal filter to memory_search
fix:  handle empty stream id in purge handler
```

- `type: subject`, imperative, ≤ 72 chars; blank line; body explains **why**. Sign off with `git commit -s` (DCO; unsigned PRs get rebased). `Fixes #123` / `Refs #123` to close issues.
- **Do not add `Co-Authored-By: Claude`** unless explicitly asked.
- A no-op delivery is valid but must be documented (one line: "NO-OP, pre-count confirms no work, because …"). Deviating from a brief needs a one-line rationale in the body.

**Releases (SemVer):** in one commit, bump `[workspace.package] version` in `Cargo.toml`, add a `CHANGELOG.md` entry, and tag `vX.Y.Z`. Pushing the tag triggers `release.yml`, which builds the dashboard SPA and then 4 targets (x86_64/aarch64 Linux, aarch64/x86_64 macOS; no Windows archive). **`main` should never be ahead of the latest tag** — tag the release commit itself.

**Workflow reality:** solo repo, but **engine changes go through a PR on a feature branch** — since 2026-08 direct-to-`main` engine commits were reverted as a process violation. Only docs, `chore`, `ci` and release commits go straight to `main`. The `protect-main` ruleset blocks deletion and force-push only, so the PR rule is convention — follow it anyway. The **pre-commit hook (fmt + clippy) is the real local gate** — install it once with `scripts/install-hooks.sh`; CI re-runs the same checks on push and PR. The numbered-cycle / critic-pass discipline (`multi-agent-workflow-lite`) is **optional**, used per cycle for risky engine work; cycle artifacts are not committed. PR + critic is mandatory for external contributions.

**Memory dogfooding:** Claude Code/Cowork now have a `loomem` MCP connector pointed at the product. Don't pollute product memory with dev-chatter — use a dedicated dev stream or skip storing ephemeral build/session notes.

---

## When in doubt — stop and ask

- Breaking up a function > 500 SLOC (architectural, not operational).
- Touching a **god file** (large inherited files; hottest are `handlers/search.rs`, `core/storage.rs`, `handlers/admin.rs`, `mcp/dispatcher.rs`, `server/main.rs`, `core/graph/mod.rs` — run `wc -l` for current sizes). Each modification needs a `Critical file rationale:` line in the commit (why it's minimal-risk: doesn't change control flow/behavior + what was verified).
- Touching crypto / at-rest encryption (`loomem-core/src/crypto/`, `encrypt_backfill/`) — a mistake here loses or leaks data.
- Adding a dependency, or being unsure which layer (handler / core / storage) code belongs in.
- An existing test starts failing and you don't understand why.
- Code you don't understand that you're tempted to leave alone — **don't leave it, ask.**

---

*This file is load-bearing: it shapes every agent session. Treat changes to it (and to `docs/agent-conventions.md`) as deliberate, not drive-by — especially for external contributors.*
