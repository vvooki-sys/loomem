//! Read-only native temporal-veto oracle: JSON array of queries on stdin.
use std::io::{self, Read, Write};

use anyhow::{ensure, Context, Result};
use loomem_core::search::semantic_context::requires_original_order;

const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

fn main() -> Result<()> {
    let limit = u64::try_from(MAX_INPUT_BYTES + 1).context("input limit conversion failed")?;
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(limit)
        .read_to_end(&mut input)
        .context("failed to read query array from stdin")?;
    ensure!(
        input.len() <= MAX_INPUT_BYTES,
        "query array exceeds 16 MiB input limit"
    );
    let queries: Vec<String> =
        serde_json::from_slice(&input).context("stdin must be a JSON array of strings")?;
    let decisions: Vec<_> = queries
        .iter()
        .map(|query| requires_original_order(query))
        .collect();
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &decisions).context("failed to write veto decisions")?;
    writeln!(output).context("failed to terminate veto decisions")?;
    Ok(())
}
