//! Trusted-owner JSONL adapter for the full original tool and worker set.
use anyhow::{Context, Result};
use loomem_core::Config;
use loomem_server::hosted::{Request, SharedEngine};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

fn main() -> Result<()> {
    // Tokio's I/O driver also starts the signal driver, which creates an IPC
    // socket pair. The fully isolated Hosted worker has no network and denies
    // that operation; unsandboxed evaluation retains normal loopback I/O.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_time();
    if std::env::var_os("LOOMEM_HOSTED_SANDBOXED").is_none() {
        builder.enable_io();
    }
    builder
        .build()
        .context("build Hosted shared-engine runtime")?
        .block_on(run())
}

async fn run() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    let (root, config) = arguments()?;
    let mut engine = SharedEngine::new(Path::new(&root), Config::load(&config)?)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let (response, shutdown) = process_line(&mut engine, &line?).await;
        write_reply(&mut stdout, response)?;
        if shutdown {
            break;
        }
    }
    Ok(())
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

async fn process_line(engine: &mut SharedEngine, line: &str) -> (Value, bool) {
    let mut request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => return (json!({"ok": false, "error": error.to_string()}), false),
    };
    let context = request.context.clone();
    let request_id = request.request_id.take();
    let shutdown = request.cmd == "shutdown";
    let mut response = match engine.handle(request).await {
        Ok(response) => response,
        Err(error) => json!({
            "ok": false,
            "error": format!("{error:#}"),
            "engine_pid": std::process::id(),
            "context": context,
        }),
    };
    if let (Some(object), Some(request_id)) = (response.as_object_mut(), request_id) {
        object.insert("request_id".into(), Value::String(request_id));
    }
    (response, shutdown)
}

fn write_reply(output: &mut impl Write, response: Value) -> Result<()> {
    serde_json::to_writer(&mut *output, &response)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
