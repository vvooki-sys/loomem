# Agent conventions — full rationale

Companion to [`CLAUDE.md`](../CLAUDE.md). `CLAUDE.md` carries the load-bearing rules and loads into context every session; this file holds the longer rationale, complexity carve-outs, and edge cases that don't need to be in front of the model on every turn. When the two disagree, `CLAUDE.md` wins.

---

## Agent behavior (Karpathy principles)

Four habits that address the most common LLM coding failures. They favor caution over speed; for trivial work (a typo, an obvious one-liner) use judgment, not full ceremony.

**Think before coding.** Don't guess, don't hide doubt, show trade-offs. Name assumptions explicitly; if you're unsure, ask. Surface multiple interpretations when there's ambiguity instead of silently picking one. Push back when a simpler approach exists. Stop when confused and name what's unclear.

**Simplicity first.** Minimal code that solves the problem, nothing speculative. No features beyond what was asked, no abstractions for single-use code, no "flexibility"/"configurability" nobody requested, no error handling for impossible cases. If you wrote 200 lines where 50 would do, rewrite. Test: would a senior engineer call it over-engineered? This is the floor; the complexity limits below are the ceiling.

**Surgical changes.** Touch only what you must, clean up only after yourself. Don't "improve" neighboring code, comments, or formatting; don't refactor things that aren't broken; match the existing style even if you'd do it differently. Report unrelated dead code, don't delete it. Only remove imports/vars/functions *your* change made unused. Test: every changed line must trace to a specific request.

**Goal-driven execution.** Define success criteria, then loop until verified. Turn "add validation" into "write tests for invalid inputs, then make them pass"; "fix the bug" into "write a test reproducing it, then make it pass." For multi-step work, state a short plan with a verification check per step.

Signals it's working: smaller diffs, less rewriting from over-engineering, clarifying questions *before* implementation, clean minimal PRs with no drive-by refactors.

---

## Complexity limits — detail

Per function: CC ≤ 10, COG ≤ 15, SLOC ≤ 100 (excl. doc comments), args ≤ 6 (more → a `Request`/`Params` struct). Per file: SLOC ≤ 700, MI ≥ 40. A function or file already over a limit must not get worse — your change must reduce the metric, not raise it.

**Orchestrator vs helper (during refactors).** Helpers extracted while iteratively refactoring may exceed limits in the first pass *if*: the orchestrator hits the hard limits (CC ≤ 10, COG ≤ 15, MI ≥ 40); each over-limit helper has an explicit follow-up declared at cycle close; and the helper stays within relaxed limits (CC ≤ 35, COG ≤ 50, MI ≥ 30, args ≤ 6). A helper that busts even the relaxed limits means the refactor isn't finished.

**Carve-out: declarative tables (CC = 1).** A function whose body is a *declarative registration chain of uniform shape* — `Router::new().route(p1,h1).route(p2,h2)…`, a `match` with one-line arms, a fixture list, an error-message catalog — with measured CC = 1 and zero branching is exempt from SLOC ≤ 100, provided: `lizard` reports CC = 1; every non-setup line is a repeated entry of the same shape; and there's no `if`/`match`/`for`/`while`/`?`/`&&`/`||` in the body. Requires the annotation `// §complexity carve-out: declarative table, CC=1`. If CC ever rises above 1, the carve-out no longer applies. *Not* a carve-out: any function with imperative logic (CC > 1), even if it "looks declarative."

> A note on carve-outs: they're legitimate, but they also hand the model ready-made language for rationalizing a limit breach ("it's a declarative table"). The CC=1 / annotation requirement is the guard. If you find yourself reaching for a carve-out, double-check `lizard` actually reports CC = 1 — the rule is hard, not vibes.

---

## Enforcement reality (aspirational vs actual)

**Actually enforced (CI + pre-commit hook), with `RUSTFLAGS: "-D warnings"`:**

```bash
cargo check  --workspace
cargo test   --workspace --lib        # lib only, no integration tests
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo audit                           # "audit" job
```

That clippy run uses **default lints minus `too_many_arguments`** (the one entry in `[workspace.lints.clippy]`). It does **not** include pedantic/nursery/cargo, nor deny rules for `unwrap_used` / `expect_used` / `panic` / `todo` / `unimplemented` / `as_conversions`. The pre-commit hook (`scripts/hooks/pre-commit`) is byte-identical to the CI fmt + clippy commands.

**Not checked by CI — enforced by review only:**

- **Complexity limits** — there is no `check-complexity.sh` in this repo (Engram has one; Loomem doesn't yet). A critic pass verifies CC/COG/MI/SLOC with `lizard` / `rust-code-analysis-cli`. *Follow-up: port the complexity script + add gates to CI.*
- **Error/cast antipatterns** — a critic greps the diff of new lines (`git diff origin/main...HEAD -- <file>`), not the whole file. There's no block on pre-existing antipatterns sitting in `main`; a cycle that touches such a file must not make them worse, but isn't obligated to fix the neighborhood (fix bug = fix bug; refactor = separate change).
- **clippy pedantic/nursery** — a long-term goal, blocked by pre-existing `.unwrap()` in prod code (turning on `unwrap_used = "deny"` would instantly red the CI). Path forward: per-module upgrade cycles.

**One-off aspirational scan** (use as a tactical per-module scan, output is long):

```bash
cargo clippy --workspace --all-targets -- \
  -D clippy::unwrap_used -D clippy::expect_used \
  -D clippy::panic -D clippy::todo -D clippy::unimplemented
```

---

## Error handling — detail

No `.unwrap()` in production code; allowed only in `#[cfg(test)]`, `tests/`, `benches/`, `examples/`, and at the very end of `main()` after a full error. Same for `.expect("…")` — if something truly can't be `None`/`Err`, document why in a comment. No `panic!` / `todo!` / `unimplemented!` in production; TODOs go to the issue tracker, not the code. Propagate with `?` **plus context** (`.context("failed to parse user query")?`), not a bare `?`. New error types: `thiserror` in the domain layer (`loomem-core`), `anyhow` in handlers — follow whatever the crate already uses.

**Refactor scope:** antipattern checks apply to the diff only, not the whole file. Relocating a pre-existing antipattern into an extracted helper without changing semantics is allowed if documented at cycle close.

---

## Live smoke test — hot-path refactors

A refactor touching a hot path (HTTP handler, `mcp/`, search, storage, consolidation, crypto) needs, before merge:

- **Tier A (static):** all CI gates green locally + full `cargo test --workspace`.
- **Tier B (sanity):** release build, server started against a live DB, ≥ 3 fixture queries across different branches (simple, temporal/time-aware, complex/multi-query). Expect HTTP 200, valid JSON, zero panics in the log, sensible non-empty results.
- **Tier C (identity):** build `main` on a separate port, run the same queries, `diff` the top-k IDs after normalizing out `latency_ms` / `cache_hit` / `trace_id`. **Top-k IDs identical in count and order.** Only float-ULP / `time_decay` differences (time elapsed between runs) are acceptable.

Skip B/C for docs-only and internal cycles (test refactors, clippy batches, behavior-neutral dependency bumps). **ACCEPT-WITH-NOTES** when Tier C isn't feasible (no live DB/fixtures): do Tier B at minimum and flag it explicitly at cycle close.

---

## Schema evolution / serde backward-compat

`Chunk`, `ExtractionMeta`, `FactType` and friends are serialized into RocksDB. Existing databases hold old serializations. Therefore **every new field on a persisted struct must be `#[serde(default)]`** (or `Option<T>`), or deserializing an existing DB fails. This is already the pattern in `config.rs` and `audit.rs` — follow it. When in doubt about whether a struct is persisted, check whether it crosses the storage boundary in `loomem-core/src/storage.rs`.

---

## Architecture decisions

Loomem has no formal ADR directory yet. Look for decision context in this order: `CHANGELOG.md` → `docs/architecture.md` and `docs/architecture/` → git history (`git log -p <file>`). Rule: if a decision you don't understand is documented, don't override it without owner confirmation — say so explicitly ("this reverts loomem/005, the file-layer removal — continue?"). For significant new decisions (storage, search pipeline, data format, product scope), propose a short rationale entry in `CHANGELOG.md` or `docs/architecture.md`.

---

*Companion to `CLAUDE.md`. Same change discipline applies: deliberate edits, not drive-by.*
