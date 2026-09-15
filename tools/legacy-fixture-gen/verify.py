#!/usr/bin/env python3
"""Independent cross-check of a persisted_codec golden fixture.

Re-derives every `bytes_hex` / `row_bytes_hex` from the recorded field values
with a from-scratch encoding of the bincode-1 default layout (`<Q` length
prefix, `<I` f32 bit patterns, `<I`/`<B`/`<q` for the DEK row) — no bincode, no
loomem code — and compares byte for byte. Also decrypts the wrapped DEK and the
sample chunk when the `cryptography` package is available.

Usage: python3 tools/legacy-fixture-gen/verify.py loomem-core/tests/fixtures/legacy_bincode_v1.json
"""
import hashlib
import json
import struct
import sys


def check_vectors(vectors):
    for v in vectors:
        n = v["len"]
        bits = v["values_bits_hex"]
        assert len(bits) == 8 * n, f"{v['name']}: bits length"
        words = [int(bits[i : i + 8], 16) for i in range(0, len(bits), 8)]
        enc = struct.pack("<Q", n) + b"".join(struct.pack("<I", w) for w in words)
        assert enc.hex() == v["bytes_hex"], f"{v['name']}: independent encoding differs"
        print(f"  vector {v['name']:16s} len={n:5d} bytes={len(enc)} OK")


def check_dek_row(dek):
    f = dek["fields"]
    blob = bytes.fromhex(f["wrapped_blob_hex"])
    enc = (
        struct.pack("<I", f["dek_id"])
        + struct.pack("<B", f["master_key_version"])
        + struct.pack("<Q", len(blob))
        + blob
        + bytes.fromhex(f["wrap_nonce_hex"])
        + bytes.fromhex(f["wrap_tag_hex"])
        + struct.pack("<q", f["created_at"])
    )
    assert enc.hex() == dek["row_bytes_hex"], "WrappedStreamDek: independent encoding differs"
    print(f"  dek row bytes={len(enc)} OK")
    return blob


def check_crypto(dek, blob):
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    except ImportError:
        print("  (cryptography package not installed; skipping AES-GCM decrypt check)")
        return
    f = dek["fields"]
    master = bytes.fromhex(dek["synthetic_master_key_hex"])
    # wrap_dek: AES-256-GCM over the DEK, no associated data, tag stored separately.
    unwrapped = AESGCM(master).decrypt(
        bytes.fromhex(f["wrap_nonce_hex"]), blob + bytes.fromhex(f["wrap_tag_hex"]), None
    )
    assert unwrapped.hex() == dek["synthetic_dek_hex"], "unwrapped DEK differs from synthetic DEK"
    print("  unwrap_dek OK")
    chunk = bytes.fromhex(dek["encrypted_chunk"]["blob_hex"])
    # encrypt_blob: MAGIC(4) || version(1) || dek_id(4 LE) || nonce(12) || ciphertext || tag(16);
    # the header is not authenticated (no associated data).
    assert chunk[:4] == bytes([0xFF, 0x4C, 0x4F, 0x4F]) and chunk[4] == 1, "chunk header"
    nonce, body = chunk[9:21], chunk[21:]
    plaintext = AESGCM(unwrapped).decrypt(nonce, body, None)
    assert plaintext == dek["encrypted_chunk"]["plaintext_utf8"].encode(), "chunk plaintext differs"
    print("  decrypt_blob OK")


def main(path):
    data = open(path, "rb").read()
    fx = json.loads(data)
    print(f"{path}: sha256 {hashlib.sha256(data).hexdigest()}")
    print(f"  source_sha {fx['source_sha']} bincode {fx['bincode_version']}")
    check_vectors(fx["vectors"])
    blob = check_dek_row(fx["wrapped_stream_dek"])
    check_crypto(fx["wrapped_stream_dek"], blob)
    print("ALL CHECKS PASSED")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
