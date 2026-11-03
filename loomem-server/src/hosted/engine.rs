//! One process, independently addressed original engine states.
//!
//! Known limitation (brief 048 critique, finding 1; kept deliberately in
//! brief 053 stage A): `revoke` cancels only jobs that have not entered the
//! sticky durable write phase. `memory_ingest` persists its source through
//! `persist_chunk`, which enters that phase before the LLM extraction call, so
//! an in-flight extraction cannot be aborted and `revoke` waits for it until
//! the engine's LLM timeout before acknowledging the new epoch. The stale
//! epoch can never publish into the next generation; only the revocation
//! acknowledgement is delayed. Revocation latency is therefore bounded by the
//! extraction timeout for every in-flight ingest.
mod compat;
mod hold;

use super::protocol::{Request, ScopeContext};
use super::state::ScopeRuntime;
use crate::auth::{AuthContext, KeyScope};
use anyhow::{bail, Context, Result};
use loomem_core::local_embeddings::LocalEmbedder;
use loomem_core::storage::UserRole;
use loomem_core::Config;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct ScopeEntry {
    context: ScopeContext,
    runtime: ScopeRuntime,
}

struct Job {
    context: ScopeContext,
    /// Distinguishes this job from a later one registered under the same key.
    serial: u64,
    /// Cancels the job itself; the wrapper task observes the cancellation.
    abort: tokio::task::AbortHandle,
    /// Finishes once the job's outcome has been delivered to `outcome`.
    task: JoinHandle<()>,
    /// Taken by the quick phase of `await`; `None` while a client waits on it.
    outcome: Option<oneshot::Receiver<Result<Value>>>,
    write_phase: Arc<crate::handlers::ingest::write_phase::WritePhase>,
}

/// An `await` whose job outcome is still pending; waiting on it needs no
/// engine lock, so other scopes keep being served meanwhile.
pub struct PendingAwait {
    context: ScopeContext,
    operation: String,
    serial: u64,
    outcome: oneshot::Receiver<Result<Value>>,
}

impl PendingAwait {
    pub async fn outcome(self) -> AwaitedJob {
        let outcome = match self.outcome.await {
            Ok(outcome) => outcome,
            Err(_) => Err(anyhow::anyhow!("scoped job dropped before completion")),
        };
        AwaitedJob {
            context: self.context,
            operation: self.operation,
            serial: self.serial,
            outcome,
        }
    }
}

/// Outcome of a job, ready for the epoch check under the engine lock.
pub struct AwaitedJob {
    context: ScopeContext,
    operation: String,
    serial: u64,
    outcome: Result<Value>,
}

/// One original tool call, dispatched on its own task for a scope.
struct ToolJob {
    state: Arc<crate::AppState>,
    name: String,
    args: Value,
    stream: String,
    auth: AuthContext,
    failpoint: Option<String>,
    hold: Option<hold::HoldGate>,
}

impl ToolJob {
    async fn run(self) -> Result<Value> {
        if let Some(hold) = self.hold {
            hold.wait().await?;
        }
        let dispatch = crate::mcp::dispatcher::dispatch_tool(
            &self.state,
            &self.name,
            self.args,
            &self.stream,
            &self.auth,
        );
        #[cfg(feature = "evaluation")]
        let failpoint = self
            .failpoint
            .as_deref()
            .and_then(loomem_core::storage::persist::EvaluationFailpoint::parse);
        #[cfg(feature = "evaluation")]
        let result =
            loomem_core::storage::persist::with_evaluation_failpoint(failpoint, dispatch).await;
        #[cfg(not(feature = "evaluation"))]
        let result = {
            let _ = self.failpoint;
            dispatch.await
        };
        let result = result.map_err(|error| anyhow::anyhow!("tool dispatch failed: {error:?}"))?;
        serde_json::to_value(result).context("serialize original tool result")
    }
}

pub struct SharedEngine {
    root: PathBuf,
    config: Config,
    embedder: Option<Arc<LocalEmbedder>>,
    instance: String,
    scopes: HashMap<String, ScopeEntry>,
    epochs: HashMap<String, i64>,
    jobs: HashMap<String, Job>,
    next_job: u64,
    holds: hold::Holds,
}

impl SharedEngine {
    pub fn new(root: &Path, config: Config) -> Result<Self> {
        enforce_llm_key_policy(
            &config,
            std::env::var("LOOMEM_ALLOW_KEYLESS").as_deref() == Ok("1"),
        )?;
        std::fs::create_dir_all(root).context("create shared engine root")?;
        let embedder = crate::local_embedding::load(&config.llm)?;
        Ok(Self {
            root: root.canonicalize()?,
            config,
            embedder,
            instance: uuid::Uuid::new_v4().to_string(),
            scopes: HashMap::new(),
            epochs: HashMap::new(),
            jobs: HashMap::new(),
            next_job: 0,
            holds: hold::Holds::new(),
        })
    }

    /// Identity of this process lifetime, echoed in every reply.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub async fn handle(&mut self, request: Request) -> Result<Value> {
        if compat::handles(&request) {
            return compat::handle(self, request).await;
        }
        match request.cmd.as_str() {
            "tool_async" | "worker_async" | "await" | "quiesce" => self.handle_job(request).await,
            "open_scope" | "restore_scope" => self.handle_scope(request).await,
            "tools/list" => {
                let context = request.context()?;
                self.ensure_live(&context)?;
                Ok(self.reply(
                    context,
                    json!({"tools": crate::mcp::tools::tool_definitions(&self.config.mcp)}),
                ))
            }
            "revoke" => {
                let context = request.context()?;
                self.revoke(context.clone()).await?;
                Ok(self.reply(context, json!({"revoked": true})))
            }
            "status" => {
                let context = request.context()?;
                self.status(context)
            }
            "shutdown" => {
                self.shutdown().await?;
                Ok(self.process_reply(json!({})))
            }
            "release_holds" => {
                let released = self.holds.release()?;
                Ok(self.process_reply(json!({"released": released})))
            }
            other => bail!("unknown shared-engine command: {other}"),
        }
    }

    async fn handle_scope(&mut self, request: Request) -> Result<Value> {
        if request.cmd == "restore_scope" {
            return self.restore_scope(request).await;
        }
        let context = request.context()?;
        self.open_scope(context, request.generation()?, false).await
    }

    async fn handle_job(&mut self, request: Request) -> Result<Value> {
        match request.cmd.as_str() {
            "tool_async" => self.tool_async(request).await,
            "worker_async" => self.worker_async(request).await,
            "await" => self.await_job(request).await,
            "quiesce" => self.quiesce_scope(request).await,
            other => bail!("unknown scoped job command: {other}"),
        }
    }

    async fn open_scope(
        &mut self,
        context: ScopeContext,
        generation: String,
        rebuild: bool,
    ) -> Result<Value> {
        self.advance_epoch(&context)?;
        if self.scopes.get(&context.scope).is_some_and(|entry| {
            entry.context == context && entry.runtime.root.ends_with(&generation)
        }) {
            if !self.scopes[&context.scope].runtime.accepts_writes() {
                bail!("scope generation is closing or quiesced")
            }
            return Ok(self.reply(context, json!({"opened": true, "generation": generation})));
        }
        self.replace_scope(&context.scope).await?;
        let root = self.root.join(&context.scope).join(&generation);
        let runtime =
            ScopeRuntime::open(self.config.clone(), &root, self.embedder.clone(), rebuild).await?;
        self.epochs.insert(context.scope.clone(), context.epoch);
        self.scopes.insert(
            context.scope.clone(),
            ScopeEntry {
                context: context.clone(),
                runtime,
            },
        );
        Ok(self.reply(
            context,
            json!({"opened": true, "generation": generation, "root": root}),
        ))
    }

    async fn restore_scope(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        let generation = request.generation()?;
        let source = self.restore_source(&request)?;
        self.advance_epoch(&context)?;
        self.replace_scope(&context.scope).await?;
        let root = self.scope_root(&context, &generation)?;
        std::fs::create_dir_all(&root).context("create Hosted restore root")?;
        copy_tree(&source, &root.join("rocksdb"))?;
        let runtime =
            ScopeRuntime::open(self.config.clone(), &root, self.embedder.clone(), true).await?;
        self.epochs.insert(context.scope.clone(), context.epoch);
        self.scopes.insert(
            context.scope.clone(),
            ScopeEntry {
                context: context.clone(),
                runtime,
            },
        );
        Ok(self.reply(
            context,
            json!({"restored": true, "generation": generation, "root": root}),
        ))
    }

    fn restore_source(&self, request: &Request) -> Result<PathBuf> {
        let checkpoint = request
            .checkpoint
            .as_deref()
            .context("missing checkpoint")?;
        let source = PathBuf::from(checkpoint)
            .canonicalize()
            .context("canonicalize scope checkpoint")?;
        if !source.starts_with(&self.root) || !source.is_dir() {
            bail!("checkpoint escapes shared engine root")
        }
        Ok(source)
    }

    async fn tool_async(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        let operation = request.operation()?;
        self.ensure_job_available(&operation, &context)?;
        let name = request.name()?.to_owned();
        let args = request.args.unwrap_or_else(|| json!({}));
        let (hold, failpoint) = self.holds.gate(request.failpoint.as_deref());
        if !loomem_core::storage::persist::evaluation_failpoint_allowed(failpoint) {
            bail!("invalid synthetic failpoint")
        }
        let job = ToolJob {
            state: self.live_state(&context)?,
            name,
            args,
            stream: context.scope.clone(),
            auth: AuthContext::single_stream(
                context.scope.clone(),
                UserRole::Admin,
                KeyScope::Private,
                Some(context.actor.clone()),
                true,
            ),
            failpoint: failpoint.map(str::to_owned),
            hold,
        };
        let write_phase = Arc::new(crate::handlers::ingest::write_phase::WritePhase::new());
        let task = tokio::spawn(crate::handlers::ingest::write_phase::track(
            write_phase.clone(),
            job.run(),
        ));
        self.insert_job(operation.clone(), context.clone(), task, write_phase);
        Ok(self.reply(context, json!({"accepted": true, "op_id": operation})))
    }

    async fn worker_async(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        let operation = request.operation()?;
        self.ensure_job_available(&operation, &context)?;
        let name = request.name()?.to_owned();
        let scheduler = self.live_scheduler(&context)?;
        let task = tokio::spawn(async move {
            scheduler.trigger_worker(&name)?;
            scheduler.wait_for_idle().await;
            Ok(json!({"worker": name, "completed": true}))
        });
        let write_phase = Arc::new(crate::handlers::ingest::write_phase::WritePhase::new());
        self.insert_job(operation.clone(), context.clone(), task, write_phase);
        Ok(self.reply(context, json!({"accepted": true, "op_id": operation})))
    }

    /// Serial form of `await`: holds the engine while the job finishes. The
    /// concurrent front end uses `begin_await` and `finish_await` instead.
    async fn await_job(&mut self, request: Request) -> Result<Value> {
        let pending = self.begin_await(&request)?;
        let awaited = pending.outcome().await;
        self.finish_await(awaited)
    }

    /// Quick phase of `await`: validates the request and detaches the job's
    /// outcome channel. The job stays registered so `revoke` can still cancel
    /// it while the caller waits without the engine lock.
    pub fn begin_await(&mut self, request: &Request) -> Result<PendingAwait> {
        let context = request.context()?;
        self.ensure_epoch(&context)?;
        let operation = request.operation()?;
        let job = self
            .jobs
            .get_mut(&job_key(&context, &operation))
            .context("unknown scoped job")?;
        if job.context != context {
            bail!("job context mismatch")
        }
        let outcome = job
            .outcome
            .take()
            .context("scoped job is already being awaited")?;
        Ok(PendingAwait {
            context,
            operation,
            serial: job.serial,
            outcome,
        })
    }

    /// Final phase of `await`: publishes the outcome only while the scope
    /// epoch that accepted the job is still current.
    pub fn finish_await(&mut self, awaited: AwaitedJob) -> Result<Value> {
        let AwaitedJob {
            context,
            operation,
            serial,
            outcome,
        } = awaited;
        let key = job_key(&context, &operation);
        // A replacement job may already hold this key (the old one was drained
        // by a same-epoch reopen); only the awaited job itself is removed.
        if self.jobs.get(&key).is_some_and(|job| job.serial == serial) {
            self.jobs.remove(&key);
        }
        self.ensure_epoch(&context)?;
        let output = outcome?;
        Ok(self.reply(context, json!({"op_id": operation, "result": output})))
    }

    async fn quiesce_scope(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        self.ensure_scope_context(&context)?;
        if self
            .jobs
            .values()
            .any(|job| job.context.scope == context.scope)
        {
            bail!("scope still has accepted jobs; await or revoke them first")
        }
        let entry = self
            .scopes
            .get_mut(&context.scope)
            .context("scope absent")?;
        entry.runtime.quiesce().await?;
        let root = entry.runtime.root.clone();
        let backups = backup_names(&root)?;
        Ok(self.reply(
            context,
            json!({"quiesced": true, "root": root, "backups": backups}),
        ))
    }

    fn status(&self, context: ScopeContext) -> Result<Value> {
        self.ensure_epoch(&context)?;
        let entry = self.scopes.get(&context.scope).context("scope absent")?;
        let active_jobs = self
            .jobs
            .values()
            .filter(|job| job.context.scope == context.scope)
            .count();
        Ok(self.reply(
            context,
            json!({
                "root": entry.runtime.root,
                "quiesced": entry.runtime.is_quiesced(),
                "active_jobs": active_jobs,
                "vector_enabled": self.config.storage.vector_enabled,
                "embedding_provider": self.config.llm.embedding_provider,
                "embedding_dim": self.config.llm.embedding_dim,
                "rerank_loaded": entry.runtime.rerank_loaded(),
                "retrieval": entry.runtime.state.retrieval_counters.snapshot(),
                "queues": entry.runtime.queue_depths(),
            }),
        ))
    }

    async fn revoke(&mut self, context: ScopeContext) -> Result<()> {
        let current = self.epochs.get(&context.scope).copied().unwrap_or(-1);
        if context.epoch <= current {
            bail!("revocation must advance scope epoch")
        }
        // Cancel and close every old-epoch writer before acknowledging the new
        // epoch. A late response still fails its own epoch check afterwards.
        self.abort_jobs(&context.scope).await;
        if let Some(entry) = self.scopes.get(&context.scope) {
            entry.runtime.state.deferred.abort().await;
        }
        self.replace_scope(&context.scope).await?;
        self.epochs.insert(context.scope, context.epoch);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        let scopes: Vec<String> = self.scopes.keys().cloned().collect();
        for scope in scopes {
            self.replace_scope(&scope).await?;
        }
        Ok(())
    }

    async fn replace_scope(&mut self, scope: &str) -> Result<()> {
        self.drain_jobs(scope).await;
        if let Some(entry) = self.scopes.get_mut(scope) {
            entry.runtime.quiesce().await?;
        }
        self.scopes.remove(scope);
        Ok(())
    }

    async fn drain_jobs(&mut self, scope: &str) {
        let keys: Vec<String> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.context.scope == scope)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some(job) = self.jobs.remove(&key) {
                let _ = job.task.await;
            }
        }
    }

    async fn abort_jobs(&mut self, scope: &str) {
        let keys: Vec<String> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.context.scope == scope)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some(job) = self.jobs.remove(&key) {
                if job.write_phase.revoke() {
                    job.abort.abort();
                }
                let _ = job.task.await;
            }
        }
    }

    fn ensure_job_available(&self, operation: &str, context: &ScopeContext) -> Result<()> {
        self.ensure_live(context)?;
        if self.jobs.contains_key(&job_key(context, operation)) {
            bail!("duplicate scoped op_id")
        }
        Ok(())
    }

    fn insert_job(
        &mut self,
        operation: String,
        context: ScopeContext,
        task: JoinHandle<Result<Value>>,
        write_phase: Arc<crate::handlers::ingest::write_phase::WritePhase>,
    ) {
        let serial = self.next_job;
        self.next_job += 1;
        let abort = task.abort_handle();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let outcome = task
                .await
                .context("join scoped job")
                .and_then(|result| result);
            let _ = sender.send(outcome);
        });
        self.jobs.insert(
            job_key(&context, &operation),
            Job {
                context,
                serial,
                abort,
                task,
                outcome: Some(receiver),
                write_phase,
            },
        );
    }

    fn live_state(&self, context: &ScopeContext) -> Result<Arc<crate::AppState>> {
        self.ensure_live(context)?;
        Ok(self.scopes[&context.scope].runtime.state.clone())
    }

    fn live_scheduler(
        &self,
        context: &ScopeContext,
    ) -> Result<Arc<loomem_core::scheduler::Scheduler>> {
        self.ensure_live(context)?;
        Ok(self.scopes[&context.scope].runtime.scheduler.clone())
    }

    fn ensure_live(&self, context: &ScopeContext) -> Result<()> {
        self.ensure_scope_context(context)?;
        let entry = self.scopes.get(&context.scope).context("scope not open")?;
        if !entry.runtime.accepts_writes() {
            bail!("scope generation is closing or quiesced")
        }
        Ok(())
    }

    fn ensure_scope_context(&self, context: &ScopeContext) -> Result<()> {
        self.ensure_epoch(context)?;
        let entry = self.scopes.get(&context.scope).context("scope not open")?;
        if entry.context != *context {
            bail!("scope context does not match the open runtime")
        }
        Ok(())
    }

    fn ensure_epoch(&self, context: &ScopeContext) -> Result<()> {
        if self.epochs.get(&context.scope) != Some(&context.epoch) {
            bail!("stale scope epoch")
        }
        Ok(())
    }

    fn advance_epoch(&self, context: &ScopeContext) -> Result<()> {
        if self
            .epochs
            .get(&context.scope)
            .is_some_and(|epoch| context.epoch < *epoch)
        {
            bail!("stale scope epoch")
        }
        Ok(())
    }

    fn scope_root(&self, context: &ScopeContext, generation: &str) -> Result<PathBuf> {
        let root = self.root.join(&context.scope).join(generation);
        if root.exists() {
            bail!("scope generation already exists")
        }
        Ok(root)
    }

    fn process_reply(&self, fields: Value) -> Value {
        let mut response = json!({
            "ok": true,
            "engine_pid": std::process::id(),
            "engine_instance": self.instance,
        });
        if let (Some(response), Some(fields)) = (response.as_object_mut(), fields.as_object()) {
            response.extend(fields.clone());
        }
        response
    }

    fn reply(&self, context: ScopeContext, fields: Value) -> Value {
        let mut response = json!({
            "ok": true,
            "engine_pid": std::process::id(),
            "engine_instance": self.instance,
            "context": context,
        });
        if let (Some(response), Some(fields)) = (response.as_object_mut(), fields.as_object()) {
            response.extend(fields.clone());
        }
        response
    }
}

fn enforce_llm_key_policy(config: &Config, allow_keyless: bool) -> Result<()> {
    let keyless_subsystems = config.llm_dependent_enabled_without_key();
    if keyless_subsystems.is_empty() {
        return Ok(());
    }
    if allow_keyless {
        tracing::warn!(
            "Running Hosted WITHOUT an LLM API key while [{}] are enabled (LOOMEM_ALLOW_KEYLESS=1)",
            keyless_subsystems.join(", ")
        );
        return Ok(());
    }
    bail!(
        "[{}] enabled but no LLM API key found: export {} (or set the [llm] api_key config field), or start with LOOMEM_ALLOW_KEYLESS=1 to accept degraded keyless mode",
        keyless_subsystems.join(", "),
        config.llm.api_key_env
    )
}

fn job_key(context: &ScopeContext, operation: &str) -> String {
    format!(
        "{}:{}:{}:{operation}",
        context.actor, context.scope, context.epoch
    )
}

fn backup_names(root: &Path) -> Result<Vec<String>> {
    let path = root.join("backups");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() || source.symlink_metadata()?.file_type().is_symlink() {
        bail!("checkpoint must be a real directory")
    }
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        copy_entry(&entry?, destination)?;
    }
    Ok(())
}

fn copy_entry(entry: &std::fs::DirEntry, destination: &Path) -> Result<()> {
    let kind = entry.file_type()?;
    let target = destination.join(entry.file_name());
    if kind.is_symlink() {
        bail!("checkpoint symlink rejected")
    }
    if kind.is_dir() {
        copy_tree(&entry.path(), &target)?;
    } else if kind.is_file() {
        std::fs::copy(entry.path(), target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_identity_contains_complete_context() {
        let context = ScopeContext {
            actor: "a".into(),
            scope: "s".into(),
            epoch: 7,
        };
        assert_eq!(job_key(&context, "op"), "a:s:7:op");
    }
}
