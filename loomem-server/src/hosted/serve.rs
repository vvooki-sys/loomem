//! Concurrent JSONL front end of one shared engine process.
//!
//! Lock discipline: every command takes the engine mutex only for its quick
//! phase. `await` detaches the job's outcome channel under the lock, waits on
//! it with the lock released, then re-takes the lock for the epoch check and
//! the reply. `open_scope`, `restore_scope`, `quiesce`, `revoke`, `shutdown`
//! and the synthetic compatibility commands hold the lock for their whole
//! duration: they are rare, bounded by the scope they touch, and other scopes
//! wait on the lock for that time instead of on the job itself. Jobs never
//! take the lock.
//!
//! Replies leave in completion order, not request order; the owner correlates
//! them by `request_id`. A request without `request_id` is still served for
//! single-request clients but cannot be correlated, so it is unsafe for any
//! client with more than one request in flight. The caller feeds parsed lines
//! through one channel and drains replies through another, so one writer
//! serializes stdout and lines never interleave.
use super::engine::{Holds, SharedEngine};
use super::protocol::{Request, ScopeContext};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore};

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
    engine: Mutex<SharedEngine>,
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
            engine: Mutex::new(engine),
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
            "await" => self.await_job(request).await,
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
            _ => self.engine.lock().await.handle(request).await,
        };
        match outcome {
            Ok(mut reply) => {
                stamp(&mut reply, request_id);
                reply
            }
            Err(error) => self.error_reply(format!("{error:#}"), context, request_id),
        }
    }

    async fn await_job(&self, request: Request) -> Result<Value> {
        let pending = self.engine.lock().await.begin_await(&request)?;
        let awaited = pending.outcome().await;
        self.engine.lock().await.finish_await(awaited)
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
mod tests {
    use super::super::test_support::new_engine_with;
    use super::*;
    use std::time::Instant;
    use tokio::task::JoinHandle;

    const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

    struct Harness {
        lines: mpsc::Sender<String>,
        replies: mpsc::Receiver<Value>,
        task: JoinHandle<Result<()>>,
        last_reply_at: Instant,
        _root: tempfile::TempDir,
    }

    impl Harness {
        async fn start(limits: ServeLimits) -> Self {
            Self::start_with(limits, |_| {}).await
        }

        async fn start_with(
            limits: ServeLimits,
            adjust: impl FnOnce(&mut loomem_core::Config),
        ) -> Self {
            let (root, engine) = new_engine_with(adjust).await;
            let (lines, line_rx) = mpsc::channel(16);
            let (reply_tx, replies) = mpsc::channel(16);
            let task = tokio::spawn(serve(engine, line_rx, reply_tx, limits));
            Self {
                lines,
                replies,
                task,
                last_reply_at: Instant::now(),
                _root: root,
            }
        }

        async fn send(&self, request_id: &str, mut request: Value) {
            request["request_id"] = Value::String(request_id.into());
            self.lines
                .send(request.to_string())
                .await
                .expect("serve loop accepts lines");
        }

        async fn reply(&mut self) -> (String, Value, Instant) {
            let reply = tokio::time::timeout(REPLY_TIMEOUT, self.replies.recv())
                .await
                .expect("reply within timeout")
                .expect("serve loop still replying");
            let request_id = reply["request_id"]
                .as_str()
                .expect("reply carries request_id")
                .to_owned();
            self.last_reply_at = Instant::now();
            (request_id, reply, self.last_reply_at)
        }

        /// Next `count` replies keyed by request_id, in whatever order they land.
        async fn replies_by_id(
            &mut self,
            count: usize,
        ) -> std::collections::HashMap<String, Value> {
            let mut replies = std::collections::HashMap::new();
            for _ in 0..count {
                let (id, reply, _) = self.reply().await;
                assert!(replies.insert(id, reply).is_none(), "duplicate reply id");
            }
            replies
        }

        async fn expect(&mut self, request_id: &str) -> Value {
            let (id, reply, _) = self.reply().await;
            assert_eq!(id, request_id, "unexpected reply order: {reply}");
            reply
        }

        async fn expect_ok(&mut self, request_id: &str) -> Value {
            let reply = self.expect(request_id).await;
            assert_eq!(reply["ok"], Value::Bool(true), "{reply}");
            reply
        }

        async fn open(&mut self, context: &Value, generation: &str) {
            let id = format!("open-{generation}");
            self.send(
                &id,
                json!({"cmd": "open_scope", "context": context, "generation": generation}),
            )
            .await;
            self.expect_ok(&id).await;
        }

        async fn hold(&mut self, request_id: &str, context: &Value, op_id: &str) {
            self.send(
                request_id,
                json!({
                    "cmd": "tool_async", "context": context, "op_id": op_id,
                    "name": "memory_status", "args": {}, "failpoint": "hosted_hold",
                }),
            )
            .await;
            self.expect_ok(request_id).await;
        }

        /// Runs one original tool to completion for `context`.
        async fn tool(&mut self, id: &str, context: &Value, name: &str, args: Value) -> Value {
            self.send(
                id,
                json!({"cmd": "tool_async", "context": context, "op_id": id, "name": name, "args": args}),
            )
            .await;
            self.expect_ok(id).await;
            let await_id = format!("{id}-await");
            self.send(
                &await_id,
                json!({"cmd": "await", "context": context, "op_id": id}),
            )
            .await;
            let reply = self.expect_ok(&await_id).await;
            assert_ne!(reply["result"]["isError"], Value::Bool(true), "{reply}");
            reply["result"].clone()
        }

        async fn close(mut self) {
            drop(self.lines);
            while self.replies.recv().await.is_some() {}
            self.task
                .await
                .expect("serve task joins")
                .expect("serve loop ends cleanly");
        }
    }

    fn context(actor: &str, scope: &str, epoch: i64) -> Value {
        json!({"actor": actor, "scope": scope, "epoch": epoch})
    }

    #[cfg(feature = "evaluation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn await_on_one_scope_does_not_block_another_scope() {
        let mut harness = Harness::start(ServeLimits::default()).await;
        let a = context("actor-a", "scope-a", 1);
        let b = context("actor-b", "scope-b", 1);
        harness.open(&a, "gen-a").await;
        harness.open(&b, "gen-b").await;
        harness.hold("hold-a", &a, "slow").await;
        harness
            .send(
                "await-a",
                json!({"cmd": "await", "context": a, "op_id": "slow"}),
            )
            .await;
        harness
            .send("status-b", json!({"cmd": "status", "context": b}))
            .await;
        let (first, status, status_at) = harness.reply().await;
        assert_eq!(first, "status-b", "{status}");
        assert_eq!(status["ok"], Value::Bool(true));
        assert_eq!(status["active_jobs"], json!(0));
        harness
            .send("release", json!({"cmd": "release_holds"}))
            .await;
        // The released job and the release itself reply in either order.
        let replies = harness.replies_by_id(2).await;
        assert_eq!(replies["release"]["ok"], Value::Bool(true));
        let awaited = &replies["await-a"];
        assert_eq!(awaited["ok"], Value::Bool(true), "{awaited}");
        assert!(status_at <= harness.last_reply_at);
        harness.close().await;
    }

    #[cfg(feature = "evaluation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn revoke_cancels_awaited_job_without_waiting_for_other_scopes() {
        let mut harness = Harness::start(ServeLimits::default()).await;
        let a = context("actor-a", "scope-a", 1);
        let b = context("actor-b", "scope-b", 1);
        harness.open(&a, "gen-a").await;
        harness.open(&b, "gen-b").await;
        harness.hold("hold-a", &a, "slow-a").await;
        harness.hold("hold-b", &b, "slow-b").await;
        harness
            .send(
                "await-a",
                json!({"cmd": "await", "context": a, "op_id": "slow-a"}),
            )
            .await;
        let revoked = context("actor-a", "scope-a", 2);
        harness
            .send("revoke-a", json!({"cmd": "revoke", "context": revoked}))
            .await;
        let replies = harness.replies_by_id(2).await;
        let revoke = &replies["revoke-a"];
        assert_eq!(revoke["ok"], Value::Bool(true), "{revoke}");
        let awaited = &replies["await-a"];
        assert_eq!(awaited["ok"], Value::Bool(false), "{awaited}");
        assert!(
            awaited["error"]
                .as_str()
                .is_some_and(|error| error.contains("stale scope epoch")),
            "{awaited}"
        );
        // Scope B's job was never touched: it is still held and finishes on release.
        harness
            .send("release", json!({"cmd": "release_holds"}))
            .await;
        harness.expect_ok("release").await;
        harness
            .send(
                "await-b",
                json!({"cmd": "await", "context": b, "op_id": "slow-b"}),
            )
            .await;
        harness.expect_ok("await-b").await;
        harness.close().await;
    }

    #[cfg(feature = "evaluation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_beyond_the_in_flight_bound_fail_fast_as_busy() {
        let limits = ServeLimits {
            max_in_flight: 1,
            drain_timeout: Duration::from_millis(200),
        };
        let mut harness = Harness::start(limits).await;
        let a = context("actor-a", "scope-a", 1);
        harness.open(&a, "gen-a").await;
        harness.hold("hold-a", &a, "slow").await;
        harness
            .send(
                "await-a",
                json!({"cmd": "await", "context": a, "op_id": "slow"}),
            )
            .await;
        harness
            .send("status-a", json!({"cmd": "status", "context": a}))
            .await;
        let busy = harness.expect("status-a").await;
        assert_eq!(busy["ok"], Value::Bool(false));
        assert_eq!(busy["error"], json!("engine_busy"));
        assert_eq!(busy["retryable"], Value::Bool(true));
        assert_eq!(busy["context"], a);
        // The held job never finishes, so shutdown reports the bounded drain
        // as incomplete instead of hanging behind it.
        harness.send("shutdown", json!({"cmd": "shutdown"})).await;
        let shutdown = harness.expect("shutdown").await;
        assert_eq!(
            shutdown["in_flight_drained"],
            Value::Bool(false),
            "{shutdown}"
        );
        harness
            .task
            .await
            .expect("serve task joins")
            .expect("clean exit");
    }

    #[cfg(feature = "evaluation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_drains_in_flight_requests_before_replying() {
        let mut harness = Harness::start(ServeLimits::default()).await;
        let a = context("actor-a", "scope-a", 1);
        harness.open(&a, "gen-a").await;
        harness.hold("hold-a", &a, "slow").await;
        harness
            .send(
                "await-a",
                json!({"cmd": "await", "context": a, "op_id": "slow"}),
            )
            .await;
        harness
            .send("release", json!({"cmd": "release_holds"}))
            .await;
        harness.send("shutdown", json!({"cmd": "shutdown"})).await;
        let mut order = Vec::new();
        for _ in 0..3 {
            let (id, reply, _) = harness.reply().await;
            assert_eq!(reply["ok"], Value::Bool(true), "{reply}");
            if id == "shutdown" {
                assert_eq!(reply["in_flight_drained"], Value::Bool(true));
            }
            order.push(id);
        }
        assert_eq!(
            order.last().map(String::as_str),
            Some("shutdown"),
            "{order:?}"
        );
        assert!(
            harness.replies.recv().await.is_none(),
            "no replies after shutdown"
        );
        harness
            .task
            .await
            .expect("serve task joins")
            .expect("clean exit");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn malformed_and_unknown_requests_are_answered_not_fatal() {
        let mut harness = Harness::start(ServeLimits::default()).await;
        harness
            .lines
            .send(r#"{"cmd": "status", "request_id": "bad", "bogus": 1}"#.into())
            .await
            .expect("line accepted");
        let bad = harness.expect("bad").await;
        assert_eq!(bad["ok"], Value::Bool(false));
        assert!(bad["engine_instance"].is_string());
        harness.send("unknown", json!({"cmd": "nope"})).await;
        let unknown = harness.expect("unknown").await;
        assert!(unknown["error"]
            .as_str()
            .is_some_and(|error| error.contains("unknown shared-engine command")));
        harness.close().await;
    }

    #[cfg(feature = "evaluation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_job_has_a_single_awaiter() {
        let mut harness = Harness::start(ServeLimits::default()).await;
        let a = context("actor-a", "scope-a", 1);
        harness.open(&a, "gen-a").await;
        harness.hold("hold-a", &a, "slow").await;
        harness
            .send(
                "await-1",
                json!({"cmd": "await", "context": a, "op_id": "slow"}),
            )
            .await;
        harness
            .send(
                "await-2",
                json!({"cmd": "await", "context": a, "op_id": "slow"}),
            )
            .await;
        // Each request runs on its own task, so either await may win the lock;
        // the loser is rejected at once, the winner completes after release.
        let (rejected_id, rejected, _) = harness.reply().await;
        assert_eq!(rejected["ok"], Value::Bool(false), "{rejected}");
        assert!(
            rejected["error"]
                .as_str()
                .is_some_and(|error| error.contains("scoped job is already being awaited")),
            "{rejected}"
        );
        let winner = if rejected_id == "await-1" {
            "await-2"
        } else {
            "await-1"
        };
        harness
            .send("release", json!({"cmd": "release_holds"}))
            .await;
        let replies = harness.replies_by_id(2).await;
        assert_eq!(replies["release"]["ok"], Value::Bool(true));
        assert_eq!(
            replies[winner]["ok"],
            Value::Bool(true),
            "{}",
            replies[winner]
        );
        harness.close().await;
    }

    #[cfg(feature = "evaluation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_actor_of_a_scope_shares_the_open_runtime() {
        let mut harness = Harness::start(ServeLimits::default()).await;
        let first = context("actor-1", "shared", 1);
        let second = context("actor-2", "shared", 1);
        harness.open(&first, "live").await;
        harness
            .send("status-1", json!({"cmd": "status", "context": first}))
            .await;
        let before = harness.expect_ok("status-1").await;
        harness.hold("hold-1", &first, "slow").await;
        // The second actor neither reopens nor drains the scope.
        harness
            .send(
                "open-2",
                json!({"cmd": "open_scope", "context": second, "generation": "live"}),
            )
            .await;
        let reopened = harness.expect_ok("open-2").await;
        assert_eq!(reopened["opened"], Value::Bool(true));
        for (index, actor) in [&first, &second, &first, &second].into_iter().enumerate() {
            let id = format!("store-{index}");
            let content = format!("shared fact {index} from {}", actor["actor"]);
            harness
                .tool(&id, actor, "memory_store", json!({"content": content}))
                .await;
        }
        harness
            .tool(
                "search-2",
                &second,
                "memory_search",
                json!({"query": "shared fact"}),
            )
            .await;
        harness
            .send("status-2", json!({"cmd": "status", "context": second}))
            .await;
        let after = harness.expect_ok("status-2").await;
        assert_eq!(after["root"], before["root"]);
        assert_eq!(after["engine_instance"], before["engine_instance"]);
        assert_eq!(after["context"], second);
        assert_eq!(after["active_jobs"], json!(1), "{after}");
        harness
            .send("release", json!({"cmd": "release_holds"}))
            .await;
        harness.expect_ok("release").await;
        harness
            .send(
                "await-1",
                json!({"cmd": "await", "context": first, "op_id": "slow"}),
            )
            .await;
        harness.expect_ok("await-1").await;
        let stale = context("actor-2", "shared", 0);
        harness
            .send("stale-2", json!({"cmd": "status", "context": stale}))
            .await;
        let rejected = harness.expect("stale-2").await;
        assert_eq!(rejected["ok"], Value::Bool(false));
        assert!(
            rejected["error"]
                .as_str()
                .is_some_and(|error| error.contains("stale scope epoch")),
            "{rejected}"
        );
        harness.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_scope_fails_closed_without_the_configured_reranker() {
        let mut harness = Harness::start_with(ServeLimits::default(), |config| {
            config.search.rerank_enabled = true;
            config.search.rerank_model_dir = Some("/nonexistent/hosted-reranker".into());
        })
        .await;
        let a = context("actor-a", "scope-a", 1);
        harness
            .send(
                "open-a",
                json!({"cmd": "open_scope", "context": a, "generation": "live"}),
            )
            .await;
        let refused = harness.expect("open-a").await;
        assert_eq!(refused["ok"], Value::Bool(false), "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|error| error.contains("reranker")),
            "{refused}"
        );
        harness.close().await;
    }
}
