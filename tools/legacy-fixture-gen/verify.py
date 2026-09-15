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


NONCE_SIZE = 12
TAG_SIZE = 16
DEK_SIZE = 32
CHUNK_HEADER_SIZE = 4 + 1 + 4  # MAGIC || version || dek_id


def check_dek_row(dek):
    f = dek["fields"]
    blob = bytes.fromhex(f["wrapped_blob_hex"])
    nonce = bytes.fromhex(f["wrap_nonce_hex"])
    tag = bytes.fromhex(f["wrap_tag_hex"])
    # Fixed-width fields of WrappedStreamDek: a row with any other widths could
    # never be decoded into the type, whatever row_bytes_hex says.
    assert len(nonce) == NONCE_SIZE, f"wrap_nonce must be {NONCE_SIZE} bytes, got {len(nonce)}"
    assert len(tag) == TAG_SIZE, f"wrap_tag must be {TAG_SIZE} bytes, got {len(tag)}"
    assert len(blob) == DEK_SIZE, f"wrapped_blob must be {DEK_SIZE} bytes (AES-256 DEK), got {len(blob)}"
    assert len(bytes.fromhex(dek["synthetic_master_key_hex"])) == DEK_SIZE, "master key width"
    assert len(bytes.fromhex(dek["synthetic_dek_hex"])) == DEK_SIZE, "dek width"
    enc = (
        struct.pack("<I", f["dek_id"])
        + struct.pack("<B", f["master_key_version"])
        + struct.pack("<Q", len(blob))
        + blob
        + nonce
        + tag
        + struct.pack("<q", f["created_at"])
    )
    assert enc.hex() == dek["row_bytes_hex"], "WrappedStreamDek: independent encoding differs"
    print(f"  dek row bytes={len(enc)} OK")
    return blob


def check_chunk_layout(dek):
    """Structural check of the encrypted chunk, independent of any crypto library:
    MAGIC(4) || version(1) || dek_id(4 LE) || nonce(12) || ciphertext || tag(16).
    The header is not authenticated, so the DEK ID is compared explicitly."""
    chunk = bytes.fromhex(dek["encrypted_chunk"]["blob_hex"])
    plaintext = dek["encrypted_chunk"]["plaintext_utf8"].encode()
    assert chunk[:4] == bytes([0xFF, 0x4C, 0x4F, 0x4F]), "chunk magic"
    assert chunk[4] == 1, "chunk encryption version"
    (dek_id,) = struct.unpack("<I", chunk[5:9])
    assert dek_id == dek["fields"]["dek_id"], f"chunk dek_id {dek_id} != row dek_id {dek['fields']['dek_id']}"
    expected_len = CHUNK_HEADER_SIZE + NONCE_SIZE + len(plaintext) + TAG_SIZE
    assert len(chunk) == expected_len, f"chunk length {len(chunk)} != {expected_len}"
    print(f"  chunk layout OK (dek_id={dek_id}, {len(chunk)} bytes)")
    return chunk


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
    # Layout already verified by check_chunk_layout; the header carries no associated data.
    nonce = chunk[CHUNK_HEADER_SIZE : CHUNK_HEADER_SIZE + NONCE_SIZE]
    body = chunk[CHUNK_HEADER_SIZE + NONCE_SIZE :]
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
    check_chunk_layout(fx["wrapped_stream_dek"])
    check_crypto(fx["wrapped_stream_dek"], blob)
    print("ALL CHECKS PASSED")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
