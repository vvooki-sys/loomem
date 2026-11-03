//! Concurrent JSONL front end of one shared engine process.
//!
//! Every scope has a stable engine slot. Its lifecycle operations may hold
//! that slot while awaiting jobs or I/O; unrelated scopes never need it.
//! The registry lock only resolves slots, never spans an await. `await`
//! detaches its outcome under its own slot lock and rechecks the epoch in
//! that same slot on completion. Model and process identity remain shared.
//!
//! Replies leave in completion order, not request order; the owner correlates
//! them by `request_id`. A request without `request_id` is accepted and
//! served exactly like any other (backwards compatibility with clients that
//! wait for each reply before sending the next request); its reply simply
//! carries no `request_id`, so such a client must never have more than one
//! request in flight. The caller feeds parsed lines through one channel and
//! drains replies through another, so one writer serializes stdout and lines
//! never interleave.
use super::engine::{Holds, SharedEngine};
use super::protocol::{Request, ScopeContext};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};

mod scopes;
use scopes::Scopes;

/// Bounds of one serving loop.
#[derive(Clone, Copy, Debug)]
pub struct ServeLimits {
    /// Requests executing at once; further requests fail fast with
    /// `engine_busy` instead of queueing behind long jobs.
    pub max_in_flight: u32,
    /// How long `shutdown` and stdin EOF wait for in-flight requests before
    /// the process stops anyway.
    pub drain_timeout: Duration,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            max_in_flight: 64,
            drain_timeout: Duration::from_secs(30),
        }
    }
}

/// Serves `lines` until `shutdown` or the end of input, sending every reply
/// to `replies`. Returns after in-flight requests drained (bounded by
/// `limits.drain_timeout`).
pub async fn serve(
    engine: SharedEngine,
    mut lines: mpsc::Receiver<String>,
    replies: mpsc::Sender<Value>,
    limits: ServeLimits,
) -> Result<()> {
    let server = Arc::new(Server::new(engine, limits));
    while let Some(line) = lines.recv().await {
        let request = match server.parse(&line) {
            Ok(request) => request,
            Err(reply) => {
                send(&replies, reply).await?;
                continue;
            }
        };
        if request.cmd == "shutdown" {
            let reply = server.shutdown(request).await;
            return send(&replies, reply).await;
        }
        server.dispatch(request, replies.clone()).await?;
    }
    server.drain().await;
    Ok(())
}

async fn send(replies: &mpsc::Sender<Value>, reply: Value) -> Result<()> {
    replies
        .send(reply)
        .await
        .map_err(|_| anyhow::anyhow!("reply writer closed"))
}

struct Server {
    scopes: Scopes,
    holds: Arc<Holds>,
    instance: String,
    permits: Arc<Semaphore>,
    limits: ServeLimits,
}

impl Server {
    fn new(engine: SharedEngine, limits: ServeLimits) -> Self {
        let permits = usize::try_from(limits.max_in_flight).unwrap_or(usize::MAX);
        Self {
            instance: engine.instance().to_owned(),
            holds: engine.holds(),
            scopes: Scopes::new(engine),
            permits: Arc::new(Semaphore::new(permits)),
            limits,
        }
    }

    fn parse(&self, line: &str) -> Result<Request, Value> {
        serde_json::from_str(line).map_err(|error| {
            let request_id = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| value.get("request_id")?.as_str().map(str::to_owned));
            self.error_reply(error.to_string(), None, request_id)
        })
    }

    /// Runs the request on its own task, or answers `engine_busy` at once
    /// when `max_in_flight` requests are already executing.
    async fn dispatch(
        self: &Arc<Self>,
        request: Request,
        replies: mpsc::Sender<Value>,
    ) -> Result<()> {
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            let mut reply = self.error_reply(
                "engine_busy".into(),
                request.context.clone(),
                request.request_id.clone(),
            );
            reply["retryable"] = Value::Bool(true);
            return send(&replies, reply).await;
        };
        let server = self.clone();
        tokio::spawn(async move {
            let reply = server.execute_guarded(request).await;
            let _ = replies.send(reply).await;
            drop(permit);
        });
        Ok(())
    }

    /// A panicking request becomes an error reply instead of a lost line.
    async fn execute_guarded(self: Arc<Self>, request: Request) -> Value {
        let context = request.context.clone();
        let request_id = request.request_id.clone();
        let server = self.clone();
        match tokio::spawn(async move { server.execute(request).await }).await {
            Ok(reply) => reply,
            Err(error) => {
                self.error_reply(format!("request task failed: {error}"), context, request_id)
            }
        }
    }

    async fn execute(&self, mut request: Request) -> Value {
        let context = request.context.clone();
        let request_id = request.request_id.take();
        let outcome = match request.cmd.as_str() {
            "await" => self.scopes.await_job(request).await,
            "shutdown" => self.scopes.shutdown().await.map(|()| {
                json!({
                    "ok": true,
                    "engine_pid": std::process::id(),
                    "engine_instance": self.instance,
                })
            }),
            // Released without the engine lock: a lock-holding command such
            // as `revoke` may be waiting on the very apply section a hold
            // keeps open.
            "release_holds" => self.holds.release().map(|released| {
                json!({
                    "ok": true,
                    "engine_pid": std::process::id(),
                    "engine_instance": self.instance,
                    "released": released,
                })
            }),
            _ => self.scopes.handle(request).await,
        };
        match outcome {
            Ok(mut reply) => {
                stamp(&mut reply, request_id);
                reply
            }
            Err(error) => self.error_reply(format!("{error:#}"), context, request_id),
        }
    }

    /// Waits for in-flight requests, then closes every scope. A request that
    /// outlives `drain_timeout` is reported, not waited for.
    async fn shutdown(&self, request: Request) -> Value {
        let drained = self.drain().await;
        let context = request.context.clone();
        let request_id = request.request_id.clone();
        let mut reply =
            match tokio::time::timeout(self.limits.drain_timeout, self.execute(request)).await {
                Ok(reply) => reply,
                Err(_) => self.error_reply(
                    "engine shutdown timed out behind an unfinished request".into(),
                    context,
                    request_id,
                ),
            };
        reply["in_flight_drained"] = Value::Bool(drained);
        reply
    }

    async fn drain(&self) -> bool {
        tokio::time::timeout(
            self.limits.drain_timeout,
            self.permits.acquire_many(self.limits.max_in_flight),
        )
        .await
        .is_ok_and(|permits| permits.is_ok())
    }

    fn error_reply(
        &self,
        error: String,
        context: Option<ScopeContext>,
        request_id: Option<String>,
    ) -> Value {
        let mut reply = json!({
            "ok": false,
            "error": error,
            "engine_pid": std::process::id(),
            "engine_instance": self.instance,
            "context": context,
        });
        stamp(&mut reply, request_id);
        reply
    }
}

fn stamp(reply: &mut Value, request_id: Option<String>) {
    if let (Some(object), Some(request_id)) = (reply.as_object_mut(), request_id) {
        object.insert("request_id".into(), Value::String(request_id));
    }
}

#[cfg(test)]
mod tests;
