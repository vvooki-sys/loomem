//! Trusted-owner JSONL adapter for the full original tool and worker set.
//!
//! Requests are served concurrently (see `loomem_server::hosted::serve`): a
//! reader thread feeds stdin lines to the serving loop, which answers every
//! request on its own task, and one writer thread serializes replies to
//! stdout so lines never interleave. Replies leave in completion order and
//! carry the request's `request_id`.
use anyhow::{Context, Result};
use loomem_core::Config;
use loomem_server::hosted::{serve, ServeLimits, SharedEngine};
use serde_json::Value;
use std::io::{BufRead, Write};
use std::path::Path;
use tokio::sync::mpsc;

/// Lines parsed ahead of execution; backpressure stops the reader beyond it.
const LINE_QUEUE: usize = 256;
/// Replies waiting for stdout; backpressure stops dispatch beyond it.
const REPLY_QUEUE: usize = 256;

fn main() -> Result<()> {
    // Tokio's I/O driver also starts the signal driver, which creates an IPC
    // socket pair. The fully isolated Hosted worker has no network and denies
    // that operation; unsandboxed evaluation retains normal loopback I/O.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_time();
    if std::env::var_os("LOOMEM_HOSTED_SANDBOXED").is_none() {
        builder.enable_io();
    }
    let runtime = builder
        .build()
        .context("build Hosted shared-engine runtime")?;
    let (replies, reply_rx) = mpsc::channel::<Value>(REPLY_QUEUE);
    let writer = std::thread::spawn(move || write_replies(reply_rx));
    let served = runtime.block_on(run(replies));
    // Dropping the runtime cancels any request that outlived the drain
    // timeout, which releases the last reply senders and ends the writer.
    drop(runtime);
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("reply writer panicked"))??;
    served
}

async fn run(replies: mpsc::Sender<Value>) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    let (root, config) = arguments()?;
    let engine = SharedEngine::new(Path::new(&root), Config::load(&config)?)?;
    let (lines, line_rx) = mpsc::channel::<String>(LINE_QUEUE);
    std::thread::spawn(move || read_lines(lines));
    serve(engine, line_rx, replies, ServeLimits::default()).await
}

fn arguments() -> Result<(String, String)> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .context("usage: hosted_shared_engine ROOT CONFIG")?;
    let config = args
        .next()
        .context("usage: hosted_shared_engine ROOT CONFIG")?;
    if args.next().is_some() {
        anyhow::bail!("usage: hosted_shared_engine ROOT CONFIG")
    }
    Ok((root, config))
}

/// Feeds stdin to the serving loop; a read error or EOF ends the input, which
/// the loop treats as a request to drain and exit.
fn read_lines(lines: mpsc::Sender<String>) {
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                tracing::error!(%error, "stdin read failed; closing input");
                return;
            }
        };
        if lines.blocking_send(line).is_err() {
            return;
        }
    }
}

fn write_replies(mut replies: mpsc::Receiver<Value>) -> Result<()> {
    let mut output = std::io::stdout().lock();
    while let Some(reply) = replies.blocking_recv() {
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}
