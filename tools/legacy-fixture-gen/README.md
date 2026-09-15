# legacy-fixture-gen

Evidence tool for `loomem-core/src/persisted_codec.rs`. It writes the golden
fixture file the codec tests assert against, using the **reference encoder**
(`bincode` 1.3.3, default options) rather than the codec itself, so the
fixtures always describe what bincode wrote — the format every pre-codec
release persisted.

It is deliberately **not** a workspace member (`exclude` in the root
`Cargo.toml`, own `[workspace]` table, own untracked `Cargo.lock`): `bincode`
must never re-enter the engine's dependency graph or its audit.

## Run

```sh
cargo run --manifest-path tools/legacy-fixture-gen/Cargo.toml -- \
    /tmp/legacy_bincode_vN.json [--source-sha <sha>] [--generated-at YYYY-MM-DD]
python3 tools/legacy-fixture-gen/verify.py /tmp/legacy_bincode_vN.json
```

`--source-sha` defaults to `git rev-parse HEAD`, `--generated-at` to today.
The tool refuses to overwrite `legacy_bincode_v1.json`.

## What is deterministic and what is not

- The five `Vec<f32>` vectors are fully deterministic: a re-run must produce
  the committed `bytes_hex` values byte for byte. Any difference means the
  reference encoding changed, which must never happen silently.
- The wrapped-DEK row and the encrypted chunk use synthetic keys
  (`i*7+3`, `i*13+5`) but random AES-GCM nonces, so each run yields a
  different, equally valid row and ciphertext. `verify.py` re-derives the row
  layout independently (Python `struct`, no bincode, no loomem code) and, when
  the `cryptography` package is installed, unwraps the DEK and decrypts the
  chunk to prove the recorded bytes are consistent.

## Versioning rule

`legacy_bincode_v1.json` was produced by the pre-codec engine (`5b42ad8`) and
is the byte-compatibility proof for existing databases. Never regenerate it.
If the wire format ever changes on purpose, write a `v2` file next to it and
keep `v1` in the test suite.
