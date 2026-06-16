# Contributing to Loomem

Thanks for considering a contribution. A few ground rules keep the project healthy.

## Developer Certificate of Origin (DCO)

All contributions must be signed off, certifying the [Developer Certificate of Origin](https://developercertificate.org/):

```bash
git commit -s -m "feat: add X"
```

The `-s` flag appends a `Signed-off-by:` trailer with your name and email. PRs with unsigned commits will be asked to rebase.

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add temporal filter to memory_search
fix: handle empty stream id in purge handler
chore: bump tantivy to 0.25
docs: clarify encryption key rotation
refactor: extract scoring helpers from search handler
```

- Imperative mood, first line ≤ 72 chars.
- Body explains *why*, not just *what*.

## Before you open a PR

All of these must pass locally — CI enforces the same gates:

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo audit
```

Tips:

- Install the pre-commit hook: `scripts/install-hooks.sh` (runs fmt + clippy before every commit).
- No `.unwrap()` / `.expect()` / `panic!` in production code paths — propagate errors with `?` and `.context(...)`.
- New public functions need unit tests; new HTTP handlers need an integration test.
- Keep changes surgical: one PR = one logical change. Avoid drive-by refactors.
- New dependencies need justification in the PR description (why it can't reasonably be written in-tree, maintenance status, download count).

## Reporting bugs

Open a GitHub issue with: what you did, what you expected, what happened, and the server log around the failure (`LOOMEM_LOG_FORMAT=json` output is easiest to read back).

For security issues, **do not open a public issue** — see [SECURITY.md](SECURITY.md).
