//! Writes the golden fixtures for `loomem_core::persisted_codec`, encoded by
//! the reference implementation (`bincode` 1.3.3, default options) rather
//! than by the codec under test.
//!
//! The vectors are fully deterministic and must reproduce the committed
//! `legacy_bincode_v1.json` byte for byte. The wrapped-DEK row and the
//! encrypted chunk draw random AES-GCM nonces, so a fresh run yields a
//! different — equally valid — row; `verify.py` cross-checks the layout
//! independently of both this tool and the codec.
//!
//! Usage:
//!   cargo run --manifest-path tools/legacy-fixture-gen/Cargo.toml -- \
//!       <out.json> [--source-sha <sha>] [--generated-at <YYYY-MM-DD>]
//!
//! Never overwrite `legacy_bincode_v1.json`: its value is that it was
//! produced by the pre-codec engine. Write a new `v2` file instead.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use loomem_core::crypto::{encrypt_blob, wrap_dek, WrappedStreamDek};
use serde_json::{json, Value};

const SCOPE: &str = "__fixture_scope__";
const PLAINTEXT: &[u8] = b"loomem legacy persisted-codec fixture v1: synthetic chunk plaintext";
const CREATED_AT: i64 = 1_700_000_000;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn vector_entry(name: &str, values: &[f32]) -> Result<Value> {
    let bytes = bincode::serialize(&values.to_vec()).context("bincode serialize Vec<f32>")?;
    let bits: String = values
        .iter()
        .map(|f| format!("{:08x}", f.to_bits()))
        .collect();
    Ok(json!({
        "name": name,
        "len": values.len(),
        "values_bits_hex": bits,
        "bytes_hex": hex(&bytes),
    }))
}

/// The five vectors: edge cases first, then the two real embedding widths.
fn vectors() -> Result<Vec<Value>> {
    let special: Vec<f32> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        f32::MIN_POSITIVE,
        f32::from_bits(0x0000_0001), // smallest subnormal
        f32::MAX,
        f32::MIN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::from_bits(0x7fc0_0000), // quiet NaN
        f32::from_bits(0x7fa0_0001), // NaN with payload (signalling pattern)
        f32::from_bits(0xffc0_1234), // negative NaN with payload
        1.5,
        -2.5,
        3.25,
        1.0e-3,
        f32::from_bits(0x47f1_2065), // 123456.789 rounded to f32 (bit-exact, lint-free)
    ];
    let dim384: Vec<f32> = (0..384u16).map(|i| f32::from(i) * 0.001 - 0.19).collect();
    let dim1536: Vec<f32> = (0..1536u16)
        .map(|i| f32::from(i * 37 % 101) / 101.0 - 0.5)
        .collect();
    Ok(vec![
        vector_entry("empty", &[])?,
        vector_entry("single", &[1.0])?,
        vector_entry("special_floats", &special)?,
        vector_entry("dim384_ramp", &dim384)?,
        vector_entry("dim1536_mod", &dim1536)?,
    ])
}

fn synthetic_key(mul: u8, add: u8) -> [u8; 32] {
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        let i = u8::try_from(i).unwrap_or(u8::MAX);
        *b = i.wrapping_mul(mul).wrapping_add(add);
    }
    key
}

fn wrapped_dek_entry() -> Result<Value> {
    let master_key = synthetic_key(7, 3);
    let dek = synthetic_key(13, 5);
    let dek_id: u32 = 1;
    let master_key_version: u8 = 1;
    let mut wrapped: WrappedStreamDek =
        wrap_dek(&master_key, &dek, dek_id, master_key_version).context("wrap_dek")?;
    wrapped.created_at = CREATED_AT; // fixed; not covered by the AEAD tag
    let row_bytes = bincode::serialize(&wrapped).context("bincode serialize WrappedStreamDek")?;
    let blob = encrypt_blob(&dek, dek_id, PLAINTEXT).context("encrypt_blob")?;
    Ok(json!({
        "scope": SCOPE,
        "synthetic_master_key_hex": hex(&master_key),
        "synthetic_dek_hex": hex(&dek),
        "fields": {
            "dek_id": wrapped.dek_id,
            "master_key_version": wrapped.master_key_version,
            "wrapped_blob_hex": hex(&wrapped.wrapped_blob),
            "wrap_nonce_hex": hex(&wrapped.wrap_nonce),
            "wrap_tag_hex": hex(&wrapped.wrap_tag),
            "created_at": wrapped.created_at,
        },
        "row_bytes_hex": hex(&row_bytes),
        "encrypted_chunk": {
            "plaintext_utf8": String::from_utf8_lossy(PLAINTEXT),
            "blob_hex": hex(&blob),
        },
    }))
}

fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), out.status);
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

struct Args {
    out: PathBuf,
    source_sha: Option<String>,
    generated_at: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().context(
        "usage: legacy-fixture-gen <out.json> [--source-sha SHA] [--generated-at DATE]",
    )?);
    let mut parsed = Args {
        out,
        source_sha: None,
        generated_at: None,
    };
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--source-sha" => parsed.source_sha = Some(value),
            "--generated-at" => parsed.generated_at = Some(value),
            other => bail!("unknown flag {other}"),
        }
    }
    Ok(parsed)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if args
        .out
        .file_name()
        .is_some_and(|n| n == "legacy_bincode_v1.json")
        && args.out.exists()
    {
        bail!("refusing to overwrite legacy_bincode_v1.json; write a new version instead");
    }
    let source_sha = match args.source_sha {
        Some(sha) => sha,
        None => command_output("git", &["rev-parse", "HEAD"])?,
    };
    let generated_at = match args.generated_at {
        Some(date) => date,
        None => command_output("date", &["+%Y-%m-%d"])?,
    };

    let doc = json!({
        "_comment": format!(
            "Golden fixtures for loomem-core/src/persisted_codec.rs. Generated by the OLD engine \
             (loomem-core @ {source_sha}, bincode 1.3.3 from Cargo.lock, rustc 1.97.0) via \
             bincode::serialize on {generated_at}; see fixtures/legacy_bincode_v1.md for the exact \
             procedure. All keys are synthetic. Never regenerate with the new codec: the file exists \
             to prove byte-compatibility with data written before the codec change."
        ),
        "source_sha": source_sha,
        "generated_at": generated_at,
        "bincode_version": "1.3.3",
        "vectors": vectors()?,
        "wrapped_stream_dek": wrapped_dek_entry()?,
    });

    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    std::fs::write(&args.out, text).with_context(|| format!("write {}", args.out.display()))?;
    println!("wrote {}", args.out.display());
    Ok(())
}
