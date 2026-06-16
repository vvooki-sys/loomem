# Release Runbook (maintainers)

How to cut a Loomem release. The pipeline is tag-driven: pushing a `v*` tag builds binaries for 4 targets and publishes a GitHub Release with `SHA256SUMS`.

## Versioning

- Single source of truth: `version` in the workspace `Cargo.toml` (`[workspace.package]`).
- Tags: `v<version>` (`v0.2.0`), release candidates: `v<version>-rc<N>` (`v0.2.0-rc1`).
- Tags containing `-rc` / `-beta` / `-alpha` are auto-marked **prerelease** — excluded from `/releases/latest`, so `install.sh` users never get them unless they pin `LOOMEM_VERSION` explicitly.

## Pre-flight checklist

1. CI green on `main` (check + test + clippy + fmt).
2. `CHANGELOG.md`: move `[Unreleased]` entries under the new version heading with today's date.
3. Workspace `Cargo.toml` `version` matches the tag you're about to push.
4. No uncommitted changes you'd be tagging accidentally (`git status`).

## Cut a release candidate

```bash
git tag v0.2.0-rc1
git push origin v0.2.0-rc1
gh run watch --repo vvooki-sys/loomem      # ~15-30 min on first (uncached) build
```

### Verify the rc

```bash
gh release view v0.2.0-rc1 --repo vvooki-sys/loomem --json assets \
  --jq '.assets[].name'
```

Expected: 4 archives + `SHA256SUMS`:

```
SHA256SUMS
loomem-0.2.0-rc1-aarch64-apple-darwin.tar.gz
loomem-0.2.0-rc1-aarch64-unknown-linux-gnu.tar.gz
loomem-0.2.0-rc1-x86_64-apple-darwin.tar.gz
loomem-0.2.0-rc1-x86_64-unknown-linux-gnu.tar.gz
```

End-to-end install test on at least one platform:

```bash
LOOMEM_VERSION=v0.2.0-rc1 sh install.sh
~/.loomem/bin/loomem-server &        # then: curl http://localhost:3030/health
```

## Promote to final

Same commit, final tag:

```bash
git tag v0.2.0 v0.2.0-rc1^{}     # tag the same commit the rc pointed at
git push origin v0.2.0
```

After the workflow finishes, `install.sh` with no pinned version picks it up (it's now `/releases/latest`). Optionally delete rc releases/tags to reduce noise:

```bash
gh release delete v0.2.0-rc1 --repo vvooki-sys/loomem --cleanup-tag
```

## Pulling a bad release

```bash
gh release delete v0.2.0 --repo vvooki-sys/loomem --cleanup-tag
```

`/releases/latest` falls back to the previous release automatically. If the flaw is in published binaries (not just metadata), note it in `CHANGELOG.md` and ship a patch release (`v0.2.1`) rather than re-tagging the same version.

## Known pipeline facts

- **Linux builds must stay on the oldest supported GA ubuntu image** (currently `ubuntu-22.04`/`ubuntu-22.04-arm`): glibc requirements of the binaries follow the build host. Building on 24.04 produced `GLIBC_2.39`-requiring binaries that fail on Ubuntu 22.04/Debian 12 (verified live with v0.2.0-rc1). When GitHub retires the 22.04 images, either bump the documented glibc baseline in `installation.md` or move to musl/zigbuild (follow-up).

- Intel macOS builds run on `macos-15-intel` (`macos-13` was retired 2025-12-08; Intel runners go away entirely when macOS 15 images retire, expected fall 2027 — at that point drop the `x86_64-apple-darwin` target or move it to cross-compilation).
- First build per target is slow (RocksDB from source, no cache); `Swatinem/rust-cache` makes subsequent tags much faster.
- The release job needs no checkout — it only aggregates build artifacts.
