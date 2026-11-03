//! Stable scope slots: the registry lock never spans engine work or an await.
use super::super::engine::{EngineSeed, SharedEngine};
use super::super::protocol::Request;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

type Slot = Arc<AsyncMutex<SharedEngine>>;

pub(super) struct Scopes {
    seed: EngineSeed,
    slots: Mutex<HashMap<String, Slot>>,
    opened: AtomicUsize,
    closing: AtomicBool,
}

impl Scopes {
    pub(super) fn new(engine: SharedEngine) -> Self {
        let (seed, leaves) = engine.into_leaves();
        let opened = leaves.values().map(SharedEngine::open_contexts).sum();
        Self {
            seed,
            slots: Mutex::new(
                leaves
                    .into_iter()
                    .map(|(name, leaf)| (name, Arc::new(AsyncMutex::new(leaf))))
                    .collect(),
            ),
            opened: AtomicUsize::new(opened),
            closing: AtomicBool::new(false),
        }
    }

    fn slot(&self, request: &Request) -> Result<Slot> {
        ensure_scoped_command(&request.cmd)?;
        let context = request.context()?;
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| anyhow::anyhow!("scope registry poisoned"))?;
        if let Some(slot) = slots.get(&context.scope) {
            return Ok(slot.clone());
        }
        // Only lifecycle establishment may allocate a permanent slot. Reads,
        // job commands and close must not retain state for unknown scopes.
        // Revoke is deliberately included: its epoch tombstone must reject
        // a later stale open even when this scope has never been opened.
        anyhow::ensure!(
            matches!(
                request.cmd.as_str(),
                "open_scope" | "restore_scope" | "revoke"
            ),
            "scope not open"
        );
        anyhow::ensure!(!self.closing.load(Ordering::SeqCst), "engine is closing");
        Ok(slots
            .entry(context.scope.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(self.seed.leaf(&context.scope))))
            .clone())
    }

    pub(super) async fn handle(&self, request: Request) -> Result<Value> {
        let slot = self.slot(&request)?;
        let mut engine = slot.lock().await;
        let before = engine.open_contexts();
        let mut result = engine.handle(request).await;
        self.record_opened(before, engine.open_contexts());
        if let Ok(reply) = &mut result {
            if reply.get("contexts").is_some() {
                reply["contexts"] = json!(self.opened.load(Ordering::SeqCst));
            }
        }
        result
    }

    pub(super) async fn await_job(&self, request: Request) -> Result<Value> {
        let slot = self.slot(&request)?;
        let pending = slot.lock().await.begin_await(&request)?;
        let awaited = pending.outcome().await;
        // Reuse this exact slot, including its epoch tombstone after close/revoke.
        let result = slot.lock().await.finish_await(awaited);
        result
    }

    pub(super) async fn shutdown(&self) -> Result<()> {
        let slots: Vec<_> = {
            let slots = self
                .slots
                .lock()
                .map_err(|_| anyhow::anyhow!("scope registry poisoned"))?;
            self.closing.store(true, Ordering::SeqCst);
            slots.values().cloned().collect()
        };
        for slot in slots {
            let request: Request = serde_json::from_value(json!({"cmd": "shutdown"}))
                .context("build scope shutdown request")?;
            let mut engine = slot.lock().await;
            let before = engine.open_contexts();
            let result = engine.handle(request).await;
            self.record_opened(before, engine.open_contexts());
            result?;
        }
        Ok(())
    }

    fn record_opened(&self, before: usize, after: usize) {
        if after > before {
            self.opened.fetch_add(after - before, Ordering::SeqCst);
        } else {
            self.opened.fetch_sub(before - after, Ordering::SeqCst);
        }
    }
}

fn ensure_scoped_command(command: &str) -> Result<()> {
    if matches!(
        command,
        "open_scope"
            | "restore_scope"
            | "tool_async"
            | "worker_async"
            | "await"
            | "quiesce"
            | "tools/list"
            | "revoke"
            | "status"
            | "prepare"
            | "build_async"
            | "search"
            | "read"
            | "close_scope"
    ) {
        Ok(())
    } else {
        bail!("unknown shared-engine command: {command}")
    }
}

#[cfg(test)]
mod tests;
