# `legacy_bincode_v1.json` — golden fixtures for `persisted_codec`

Purpose: prove that `loomem-core/src/persisted_codec.rs` is byte-compatible
with the rows the engine persisted before the codec existed, when they were
written by `bincode 1.3.3` (`bincode::serialize`, default options). The
fixtures are consumed by the unit tests in `persisted_codec.rs`,
`storage.rs`, `associator/clustering.rs` and `crypto/provider.rs`.

**Never regenerate this file with the new codec.** Its only value is that it
was produced by the *old* code path. If the wire format ever changes on
purpose, add a `v2` fixture next to it and keep this one.

## Provenance

- Engine revision: `loomem-core` @ `5b42ad856c17657100cdb0ff273d14e4298713ec`
  (the last commit with `bincode` in the dependency graph).
- Serializer: `bincode 1.3.3` resolved from that revision's `Cargo.lock`;
  `rustc 1.97.0 (2d8144b78 2026-07-07)`, aarch64-apple-darwin.
- Generated: 2026-09-15, by the throwaway predecessor of
  `tools/legacy-fixture-gen` run inside a clean worktree of that revision.
  The committed tool reproduces the same procedure and the same vector bytes
  (`cargo run --manifest-path tools/legacy-fixture-gen/Cargo.toml -- <out>`),
  keeping `bincode` as its reference encoder outside the engine workspace.
  Essential body of the generator:

  ```rust
  use loomem_core::crypto::{encrypt_blob, wrap_dek, WrappedStreamDek};
  // Vec<f32> fixtures: bincode::serialize(&Vec<f32>) for
  //   empty, [1.0], an 18-value special-float list (±0, ±1, MIN_POSITIVE,
  //   smallest subnormal, MAX, MIN, ±INF, quiet NaN, two NaNs with payloads,
  //   1.5, -2.5, 3.25, 1e-3, 123456.789),
  //   (0..384).map(|i| i as f32 * 0.001 - 0.19),
  //   (0..1536).map(|i| (i * 37 % 101) as f32 / 101.0 - 0.5).
  // Wrapped DEK: synthetic master key (i*7+3), synthetic DEK (i*13+5),
  //   wrap_dek(master, dek, 1, 1) with created_at forced to 1_700_000_000,
  //   then bincode::serialize(&wrapped) -> row_bytes_hex.
  // Encrypted chunk: encrypt_blob(&dek, 1, b"loomem legacy persisted-codec
  //   fixture v1: synthetic chunk plaintext") -> blob_hex.
  ```

  `wrap_dek` and `encrypt_blob` draw random nonces, so a re-run yields
  different (equally valid) nonce/tag/ciphertext bytes; the recorded values
  are the ones this file was generated with. All key material is synthetic
  and exists only in this fixture.

- Float values are recorded as IEEE-754 bit patterns (`values_bits_hex`,
  8 hex chars per element, big-endian hex of `f32::to_bits`) so NaN payloads
  and signed zero are compared exactly.

## Independent cross-check

`tools/legacy-fixture-gen/verify.py` re-derives every `bytes_hex` and the
81-byte DEK row from the recorded field values with a Python `struct`
re-implementation of the bincode-1 layout (`<Q` length prefix, `<I` float
bits, `<I`/`<B`/`<q` for the DEK row fields) — no bincode, no loomem code —
and, with the `cryptography` package installed, unwraps the DEK and decrypts
the sample chunk. Run it against this file after any change to the codec:

```sh
python3 tools/legacy-fixture-gen/verify.py loomem-core/tests/fixtures/legacy_bincode_v1.json
```

## Hashes

- `legacy_bincode_v1.json`: sha256
  `67c2b95044ef5b932594d52313bf6a981fb6179ae0cbfaf7c3a8ebd40576ba28`
- raw generator output before reshaping: sha256
  `31b8d5e90c8ea77c2398e871a68564b2e9f9687b7a7b2bb5719908c8b772b6a4`
