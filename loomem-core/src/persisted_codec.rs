//! Byte-exact codec for the fixed-layout rows Loomem persists in RocksDB.
//!
//! Three row kinds use a fixed binary layout instead of JSON: embedding
//! vectors (`embeddings` CF), clustering centroids (`assoc:centroid:*` in
//! the default CF) and per-scope wrapped DEKs (`keys` CF,
//! [`WrappedStreamDek`]). They were originally written with `bincode 1.x`
//! default options: fixed-width little-endian integers, a `u64` length
//! prefix for `Vec<T>`, no prefix for fixed-size arrays, and `f32` stored as
//! its raw IEEE-754 bit pattern. bincode is unmaintained
//! (RUSTSEC-2025-0141), so this module reproduces exactly that wire format
//! for exactly those shapes — no new dependency, no format change: every
//! row written before this module existed still decodes, and every row it
//! writes is identical to what bincode produced. Golden fixtures generated
//! by the pre-codec engine live in `tests/fixtures/legacy_bincode_v1.json`
//! and are asserted in the tests below.
//!
//! Two deliberate differences from bincode's legacy decoder, both stricter:
//! trailing bytes after a value are rejected (bincode silently accepted
//! them), and every length prefix is bounded *before* any allocation, so a
//! corrupt row cannot request a multi-gigabyte buffer. Well-formed legacy
//! rows never carry trailing bytes, so the stricter rules only turn silent
//! acceptance of corruption into an error.

use thiserror::Error;

use crate::crypto::at_rest::{WrappedStreamDek, NONCE_SIZE, TAG_SIZE};

/// Upper bound on the element count of a persisted `f32` vector (256 KiB of
/// payload). The largest embedding dimension in use is 1536; the headroom
/// covers larger providers while still refusing absurd length prefixes.
pub const MAX_F32_VEC_LEN: usize = 1 << 16;

/// Upper bound on the wrapped-DEK ciphertext length. The wrapped blob is the
/// 32-byte DEK ciphertext today (tag stored separately); the limit only has
/// to exceed that comfortably.
pub const MAX_WRAPPED_BLOB_LEN: usize = 1024;

/// Size of the `u64` length prefix bincode 1.x put in front of every `Vec`.
const LEN_PREFIX_SIZE: usize = 8;

/// Size of one encoded `f32`.
const F32_SIZE: usize = 4;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("input too short: needed {needed} byte(s) at offset {offset}, {remaining} left")]
    UnexpectedEnd {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    #[error("{what} length {len} exceeds the limit of {max}")]
    LengthOverLimit {
        what: &'static str,
        len: u64,
        max: usize,
    },
    #[error("{trailing} trailing byte(s) after the value")]
    TrailingBytes { trailing: usize },
}

/// Bounds-checked cursor over an input slice. Every read either returns the
/// requested bytes or a `CodecError::UnexpectedEnd`; nothing indexes the
/// buffer directly.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn unexpected_end(&self, needed: usize) -> CodecError {
        CodecError::UnexpectedEnd {
            offset: self.pos,
            needed,
            remaining: self.buf.len().saturating_sub(self.pos),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| self.unexpected_end(n))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| self.unexpected_end(n))?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let bytes = self.take(N)?;
        <[u8; N]>::try_from(bytes).map_err(|_| self.unexpected_end(N))
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }

    fn u32_le(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    fn u64_le(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }

    fn i64_le(&mut self) -> Result<i64, CodecError> {
        Ok(i64::from_le_bytes(self.array::<8>()?))
    }

    /// Read a `u64` length prefix and validate it against `max` before the
    /// caller allocates anything for it.
    fn len_prefix(&mut self, what: &'static str, max: usize) -> Result<usize, CodecError> {
        let raw = self.u64_le()?;
        usize::try_from(raw)
            .ok()
            .filter(|len| *len <= max)
            .ok_or(CodecError::LengthOverLimit {
                what,
                len: raw,
                max,
            })
    }

    fn finish(self) -> Result<(), CodecError> {
        let trailing = self.buf.len().saturating_sub(self.pos);
        if trailing == 0 {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes { trailing })
        }
    }
}

/// The `u64` LE length prefix for `len`, or `LengthOverLimit` when `len`
/// exceeds `max`. Callers check this before reserving any output buffer.
fn len_prefix_bytes(
    what: &'static str,
    len: usize,
    max: usize,
) -> Result<[u8; LEN_PREFIX_SIZE], CodecError> {
    match u64::try_from(len).ok().filter(|_| len <= max) {
        Some(raw) => Ok(raw.to_le_bytes()),
        None => Err(CodecError::LengthOverLimit {
            what,
            len: u64::try_from(len).unwrap_or(u64::MAX),
            max,
        }),
    }
}

/// Encode an `f32` vector as `u64 LE length || f32 LE bit patterns`.
/// Errors only when the vector exceeds [`MAX_F32_VEC_LEN`], which the decoder
/// would refuse to read back.
pub fn encode_f32_vec(values: &[f32]) -> Result<Vec<u8>, CodecError> {
    // Check the limit before reserving anything, so an over-long input is
    // refused without allocating for it (mirrors the decoder's order).
    let prefix = len_prefix_bytes("f32 vector", values.len(), MAX_F32_VEC_LEN)?;
    let mut out = Vec::with_capacity(LEN_PREFIX_SIZE + values.len() * F32_SIZE);
    out.extend_from_slice(&prefix);
    for value in values {
        out.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    Ok(out)
}

/// Decode an `f32` vector written by [`encode_f32_vec`] or by the legacy
/// bincode 1.x path. Bit patterns are preserved exactly (signed zero, NaN
/// payloads, subnormals).
pub fn decode_f32_vec(bytes: &[u8]) -> Result<Vec<f32>, CodecError> {
    let mut reader = Reader::new(bytes);
    let len = reader.len_prefix("f32 vector", MAX_F32_VEC_LEN)?;
    let body_len = len
        .checked_mul(F32_SIZE)
        .ok_or(CodecError::LengthOverLimit {
            what: "f32 vector",
            len: u64::try_from(len).unwrap_or(u64::MAX),
            max: MAX_F32_VEC_LEN,
        })?;
    // Validate the payload is present before allocating for it.
    let body = reader.take(body_len)?;
    reader.finish()?;
    let (chunks, rest) = body.as_chunks::<F32_SIZE>();
    if !rest.is_empty() {
        // Unreachable for a well-formed prefix (`body_len` is a multiple of
        // `F32_SIZE`), kept as an error rather than a panic path.
        return Err(CodecError::UnexpectedEnd {
            offset: LEN_PREFIX_SIZE + chunks.len() * F32_SIZE,
            needed: F32_SIZE,
            remaining: rest.len(),
        });
    }
    Ok(chunks
        .iter()
        .map(|bits| f32::from_bits(u32::from_le_bytes(*bits)))
        .collect())
}

/// Encode a wrapped stream DEK row: `dek_id u32 LE || master_key_version u8
/// || u64 LE blob length || blob || nonce[12] || tag[16] || created_at i64
/// LE` — field order and widths of the serde struct as bincode 1.x wrote it.
pub fn encode_wrapped_stream_dek(wrapped: &WrappedStreamDek) -> Result<Vec<u8>, CodecError> {
    let blob_prefix = len_prefix_bytes(
        "wrapped DEK blob",
        wrapped.wrapped_blob.len(),
        MAX_WRAPPED_BLOB_LEN,
    )?;
    let mut out = Vec::with_capacity(
        4 + 1 + LEN_PREFIX_SIZE + wrapped.wrapped_blob.len() + NONCE_SIZE + TAG_SIZE + 8,
    );
    out.extend_from_slice(&wrapped.dek_id.to_le_bytes());
    out.push(wrapped.master_key_version);
    out.extend_from_slice(&blob_prefix);
    out.extend_from_slice(&wrapped.wrapped_blob);
    out.extend_from_slice(&wrapped.wrap_nonce);
    out.extend_from_slice(&wrapped.wrap_tag);
    out.extend_from_slice(&wrapped.created_at.to_le_bytes());
    Ok(out)
}

/// Decode a wrapped stream DEK row written by [`encode_wrapped_stream_dek`]
/// or by the legacy bincode 1.x path.
pub fn decode_wrapped_stream_dek(bytes: &[u8]) -> Result<WrappedStreamDek, CodecError> {
    let mut reader = Reader::new(bytes);
    let dek_id = reader.u32_le()?;
    let master_key_version = reader.u8()?;
    let blob_len = reader.len_prefix("wrapped DEK blob", MAX_WRAPPED_BLOB_LEN)?;
    let wrapped_blob = reader.take(blob_len)?.to_vec();
    let wrap_nonce = reader.array::<NONCE_SIZE>()?;
    let wrap_tag = reader.array::<TAG_SIZE>()?;
    let created_at = reader.i64_le()?;
    reader.finish()?;
    Ok(WrappedStreamDek {
        dek_id,
        master_key_version,
        wrapped_blob,
        wrap_nonce,
        wrap_tag,
        created_at,
    })
}

/// Golden fixtures generated by the pre-codec engine (bincode 1.3.3). Shared
/// with the storage and crypto tests so every persisted row kind is checked
/// against the same recorded bytes.
#[cfg(test)]
pub(crate) mod fixture {
    use serde::Deserialize;

    const LEGACY_V1_JSON: &str = include_str!("../tests/fixtures/legacy_bincode_v1.json");

    #[derive(Deserialize)]
    struct RawVector {
        name: String,
        len: usize,
        values_bits_hex: String,
        bytes_hex: String,
    }

    #[derive(Deserialize)]
    struct RawFields {
        dek_id: u32,
        master_key_version: u8,
        wrapped_blob_hex: String,
        wrap_nonce_hex: String,
        wrap_tag_hex: String,
        created_at: i64,
    }

    #[derive(Deserialize)]
    struct RawChunk {
        plaintext_utf8: String,
        blob_hex: String,
    }

    #[derive(Deserialize)]
    struct RawDek {
        scope: String,
        synthetic_master_key_hex: String,
        synthetic_dek_hex: String,
        fields: RawFields,
        row_bytes_hex: String,
        encrypted_chunk: RawChunk,
    }

    #[derive(Deserialize)]
    struct RawFixture {
        source_sha: String,
        vectors: Vec<RawVector>,
        wrapped_stream_dek: RawDek,
    }

    pub(crate) struct VectorFixture {
        pub name: String,
        /// Expected decoded values, compared by bit pattern.
        pub values: Vec<f32>,
        /// Bytes as written by bincode 1.3.3.
        pub bytes: Vec<u8>,
    }

    pub(crate) struct DekFixture {
        pub scope: String,
        pub master_key: [u8; 32],
        pub dek: [u8; 32],
        pub dek_id: u32,
        pub master_key_version: u8,
        pub wrapped_blob: Vec<u8>,
        pub wrap_nonce: [u8; 12],
        pub wrap_tag: [u8; 16],
        pub created_at: i64,
        /// `keys` CF row bytes as written by bincode 1.3.3.
        pub row_bytes: Vec<u8>,
        /// A chunk encrypted under `dek` by the pre-codec engine.
        pub encrypted_blob: Vec<u8>,
        pub plaintext: String,
    }

    pub(crate) struct LegacyFixture {
        pub source_sha: String,
        pub vectors: Vec<VectorFixture>,
        pub dek: DekFixture,
    }

    pub(crate) fn unhex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2), "odd hex length");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
            .collect()
    }

    fn array<const N: usize>(bytes: Vec<u8>) -> [u8; N] {
        <[u8; N]>::try_from(bytes).expect("fixture array length")
    }

    pub(crate) fn legacy_v1() -> LegacyFixture {
        let raw: RawFixture = serde_json::from_str(LEGACY_V1_JSON).expect("fixture json");
        let vectors = raw
            .vectors
            .into_iter()
            .map(|v| {
                let bits = unhex(&v.values_bits_hex);
                assert_eq!(bits.len(), v.len * 4, "fixture {}: bits length", v.name);
                let values = bits
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_bits(u32::from_be_bytes(*c)))
                    .collect();
                VectorFixture {
                    name: v.name,
                    values,
                    bytes: unhex(&v.bytes_hex),
                }
            })
            .collect();
        let d = raw.wrapped_stream_dek;
        let dek = DekFixture {
            scope: d.scope,
            master_key: array(unhex(&d.synthetic_master_key_hex)),
            dek: array(unhex(&d.synthetic_dek_hex)),
            dek_id: d.fields.dek_id,
            master_key_version: d.fields.master_key_version,
            wrapped_blob: unhex(&d.fields.wrapped_blob_hex),
            wrap_nonce: array(unhex(&d.fields.wrap_nonce_hex)),
            wrap_tag: array(unhex(&d.fields.wrap_tag_hex)),
            created_at: d.fields.created_at,
            row_bytes: unhex(&d.row_bytes_hex),
            encrypted_blob: unhex(&d.encrypted_chunk.blob_hex),
            plaintext: d.encrypted_chunk.plaintext_utf8,
        };
        LegacyFixture {
            source_sha: raw.source_sha,
            vectors,
            dek,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::legacy_v1;
    use super::*;
    use crate::crypto::at_rest;

    fn bits(values: &[f32]) -> Vec<u32> {
        values.iter().map(|v| v.to_bits()).collect()
    }

    // ── golden: bincode 1.3.3 output from the pre-codec engine ──

    #[test]
    fn fixture_records_its_source_revision() {
        let fx = legacy_v1();
        assert_eq!(fx.source_sha.len(), 40, "full git SHA expected");
        assert_eq!(fx.vectors.len(), 5);
    }

    #[test]
    fn golden_vectors_decode_bit_exactly() {
        for v in legacy_v1().vectors {
            let decoded = decode_f32_vec(&v.bytes).unwrap_or_else(|e| panic!("{}: {e}", v.name));
            assert_eq!(bits(&decoded), bits(&v.values), "fixture {}", v.name);
        }
    }

    #[test]
    fn golden_vectors_encode_byte_identically() {
        for v in legacy_v1().vectors {
            let encoded = encode_f32_vec(&v.values).expect("encode");
            assert_eq!(encoded, v.bytes, "fixture {}", v.name);
        }
    }

    #[test]
    fn golden_special_floats_keep_signed_zero_and_nan_payloads() {
        let fx = legacy_v1();
        let special = fx
            .vectors
            .iter()
            .find(|v| v.name == "special_floats")
            .expect("special_floats fixture");
        let decoded = decode_f32_vec(&special.bytes).expect("decode");
        assert_eq!(decoded[0].to_bits(), 0x0000_0000, "+0.0");
        assert_eq!(decoded[1].to_bits(), 0x8000_0000, "-0.0 keeps its sign bit");
        assert_eq!(decoded[10].to_bits(), 0x7fc0_0000, "quiet NaN");
        assert_eq!(decoded[11].to_bits(), 0x7fa0_0001, "NaN payload survives");
        assert_eq!(
            decoded[12].to_bits(),
            0xffc0_1234,
            "negative NaN payload survives"
        );
        assert_eq!(decoded[5].to_bits(), 0x0000_0001, "smallest subnormal");
        assert!(decoded[8].is_infinite() && decoded[8].is_sign_positive());
        assert!(decoded[9].is_infinite() && decoded[9].is_sign_negative());
    }

    #[test]
    fn golden_wrapped_dek_row_decodes_and_re_encodes_byte_identically() {
        let fx = legacy_v1().dek;
        let decoded = decode_wrapped_stream_dek(&fx.row_bytes).expect("decode row");
        assert_eq!(decoded.dek_id, fx.dek_id);
        assert_eq!(decoded.master_key_version, fx.master_key_version);
        assert_eq!(decoded.wrapped_blob, fx.wrapped_blob);
        assert_eq!(decoded.wrap_nonce, fx.wrap_nonce);
        assert_eq!(decoded.wrap_tag, fx.wrap_tag);
        assert_eq!(decoded.created_at, fx.created_at);
        assert_eq!(
            encode_wrapped_stream_dek(&decoded).expect("encode"),
            fx.row_bytes
        );
    }

    #[test]
    fn golden_wrapped_dek_unwraps_and_decrypts_legacy_chunk() {
        let fx = legacy_v1().dek;
        let decoded = decode_wrapped_stream_dek(&fx.row_bytes).expect("decode row");
        let dek = at_rest::unwrap_dek(&fx.master_key, &decoded)
            .expect("unwrap with synthetic master key");
        assert_eq!(dek, fx.dek, "unwrapped DEK must equal the synthetic DEK");
        let plaintext =
            at_rest::decrypt_blob(&dek, &fx.encrypted_blob).expect("decrypt legacy chunk");
        assert_eq!(plaintext, fx.plaintext.as_bytes());
    }

    // ── wire format, hand-assembled ──

    #[test]
    fn empty_vector_is_eight_zero_bytes() {
        assert_eq!(encode_f32_vec(&[]).expect("encode"), vec![0u8; 8]);
        assert!(decode_f32_vec(&[0u8; 8]).expect("decode").is_empty());
    }

    #[test]
    fn vector_layout_is_u64_len_then_le_bit_patterns() {
        let encoded = encode_f32_vec(&[1.0, -2.5]).expect("encode");
        let expected: Vec<u8> = [2u8, 0, 0, 0, 0, 0, 0, 0] // u64 LE length
            .into_iter()
            .chain([0x00, 0x00, 0x80, 0x3f]) // 1.0
            .chain([0x00, 0x00, 0x20, 0xc0]) // -2.5
            .collect();
        assert_eq!(encoded, expected);
        assert_eq!(decode_f32_vec(&expected).expect("decode"), vec![1.0, -2.5]);
    }

    #[test]
    fn vector_roundtrip_preserves_every_bit_pattern() {
        let values: Vec<f32> = (0u32..2048)
            .map(|i| f32::from_bits(i.wrapping_mul(0x9e37_79b9)))
            .collect();
        let decoded = decode_f32_vec(&encode_f32_vec(&values).expect("encode")).expect("decode");
        assert_eq!(bits(&decoded), bits(&values));
    }

    // ── bounds and malformed input ──

    #[test]
    fn vector_rejects_short_prefix() {
        for n in 0..8 {
            assert!(matches!(
                decode_f32_vec(&vec![0u8; n]),
                Err(CodecError::UnexpectedEnd { .. })
            ));
        }
    }

    #[test]
    fn vector_rejects_length_prefix_over_limit_before_allocating() {
        let mut buf = u64::MAX.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            decode_f32_vec(&buf),
            Err(CodecError::LengthOverLimit { .. })
        ));
        let mut buf = (u64::try_from(MAX_F32_VEC_LEN).expect("fits") + 1)
            .to_le_bytes()
            .to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            decode_f32_vec(&buf),
            Err(CodecError::LengthOverLimit { .. })
        ));
    }

    #[test]
    fn vector_at_limit_encodes_and_decodes_and_above_limit_refuses_to_encode() {
        let at_limit = vec![0.5f32; MAX_F32_VEC_LEN];
        let encoded = encode_f32_vec(&at_limit).expect("encode at limit");
        assert_eq!(
            decode_f32_vec(&encoded).expect("decode").len(),
            MAX_F32_VEC_LEN
        );
        let over = vec![0.5f32; MAX_F32_VEC_LEN + 1];
        assert!(matches!(
            encode_f32_vec(&over),
            Err(CodecError::LengthOverLimit { .. })
        ));
    }

    #[test]
    fn vector_rejects_payload_shorter_than_prefix_promises() {
        let mut buf = 5u64.to_le_bytes().to_vec(); // promises 5 floats
        buf.extend_from_slice(&1.0f32.to_bits().to_le_bytes()); // delivers 1
        assert!(matches!(
            decode_f32_vec(&buf),
            Err(CodecError::UnexpectedEnd {
                offset: 8,
                needed: 20,
                remaining: 4
            })
        ));
    }

    #[test]
    fn vector_rejects_trailing_bytes() {
        let mut buf = encode_f32_vec(&[1.0, 2.0]).expect("encode");
        buf.push(0x00);
        assert_eq!(
            decode_f32_vec(&buf),
            Err(CodecError::TrailingBytes { trailing: 1 })
        );
        buf.extend_from_slice(&[0u8; 3]); // a whole extra float's worth, still rejected
        assert_eq!(
            decode_f32_vec(&buf),
            Err(CodecError::TrailingBytes { trailing: 4 })
        );
    }

    #[test]
    fn wrapped_dek_rejects_every_truncation() {
        let row = legacy_v1().dek.row_bytes;
        for cut in 0..row.len() {
            assert!(
                decode_wrapped_stream_dek(&row[..cut]).is_err(),
                "truncated to {cut} bytes must not decode"
            );
        }
    }

    #[test]
    fn wrapped_dek_rejects_trailing_bytes_and_oversized_blob() {
        let fx = legacy_v1().dek;
        let mut row = fx.row_bytes.clone();
        row.push(0xff);
        assert_eq!(
            decode_wrapped_stream_dek(&row),
            Err(CodecError::TrailingBytes { trailing: 1 })
        );

        // Blob length prefix claims more than the limit: refused before any read.
        let mut row = fx.row_bytes.clone();
        let over = u64::try_from(MAX_WRAPPED_BLOB_LEN).expect("fits") + 1;
        row[5..13].copy_from_slice(&over.to_le_bytes());
        assert!(matches!(
            decode_wrapped_stream_dek(&row),
            Err(CodecError::LengthOverLimit { .. })
        ));

        let oversized = WrappedStreamDek {
            dek_id: 1,
            master_key_version: 1,
            wrapped_blob: vec![0u8; MAX_WRAPPED_BLOB_LEN + 1],
            wrap_nonce: [0u8; NONCE_SIZE],
            wrap_tag: [0u8; TAG_SIZE],
            created_at: 0,
        };
        assert!(matches!(
            encode_wrapped_stream_dek(&oversized),
            Err(CodecError::LengthOverLimit { .. })
        ));
    }

    #[test]
    fn wrapped_dek_roundtrip_with_fresh_wrap() {
        let master = [0x42u8; 32];
        let dek = [0x24u8; 32];
        let wrapped = at_rest::wrap_dek(&master, &dek, 7, 2).expect("wrap");
        let encoded = encode_wrapped_stream_dek(&wrapped).expect("encode");
        assert_eq!(encoded.len(), 4 + 1 + 8 + 32 + 12 + 16 + 8);
        let decoded = decode_wrapped_stream_dek(&encoded).expect("decode");
        assert_eq!(decoded, wrapped);
        assert_eq!(at_rest::unwrap_dek(&master, &decoded).expect("unwrap"), dek);
    }
}
