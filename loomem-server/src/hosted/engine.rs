//! One process, independently addressed original engine states.
mod compat;

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
use tokio::task::JoinHandle;

struct ScopeEntry {
    context: ScopeContext,
    runtime: ScopeRuntime,
}

struct Job {
    context: ScopeContext,
    task: JoinHandle<Result<Value>>,
}

pub struct SharedEngine {
    root: PathBuf,
    config: Config,
    embedder: Option<Arc<LocalEmbedder>>,
    instance: String,
    scopes: HashMap<String, ScopeEntry>,
    epochs: HashMap<String, i64>,
    jobs: HashMap<String, Job>,
}

impl SharedEngine {
    pub fn new(root: &Path, config: Config) -> Result<Self> {
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
        })
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
                self.revoke(context.clone())?;
                Ok(self.reply(context, json!({"revoked": true})))
            }
            "status" => {
                let context = request.context()?;
                self.status(context)
            }
            "shutdown" => {
                self.shutdown().await?;
                Ok(
                    json!({"ok": true, "engine_pid": std::process::id(), "engine_instance": self.instance}),
                )
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
            if self.scopes[&context.scope].runtime.is_quiesced() {
                bail!("quiesced generation cannot be reopened")
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
        let name = request.name()?.to_owned();
        let args = request.args.unwrap_or_else(|| json!({}));
        let state = self.live_state(&context)?;
        let auth = AuthContext::single_stream(
            context.scope.clone(),
            UserRole::Admin,
            KeyScope::Private,
            Some(context.actor.clone()),
            true,
        );
        let stream = context.scope.clone();
        let task = tokio::spawn(async move {
            let result = crate::mcp::dispatcher::dispatch_tool(&state, &name, args, &stream, &auth)
                .await
                .map_err(|error| anyhow::anyhow!("tool dispatch failed: {error:?}"))?;
            serde_json::to_value(result).context("serialize original tool result")
        });
        self.insert_job(operation.clone(), context.clone(), task)?;
        Ok(self.reply(context, json!({"accepted": true, "op_id": operation})))
    }

    async fn worker_async(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        let operation = request.operation()?;
        let name = request.name()?.to_owned();
        let scheduler = self.live_scheduler(&context)?;
        let task = tokio::spawn(async move {
            scheduler.trigger_worker(&name)?;
            scheduler.wait_for_idle().await;
            Ok(json!({"worker": name, "completed": true}))
        });
        self.insert_job(operation.clone(), context.clone(), task)?;
        Ok(self.reply(context, json!({"accepted": true, "op_id": operation})))
    }

    async fn await_job(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        let operation = request.operation()?;
        let key = job_key(&context, &operation);
        let job = self.jobs.remove(&key).context("unknown scoped job")?;
        if job.context != context {
            bail!("job context mismatch")
        }
        let output = job.task.await.context("join scoped job")??;
        self.ensure_epoch(&context)?;
        Ok(self.reply(context, json!({"op_id": operation, "result": output})))
    }

    async fn quiesce_scope(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        self.ensure_live(&context)?;
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
            }),
        ))
    }

    fn revoke(&mut self, context: ScopeContext) -> Result<()> {
        let current = self.epochs.get(&context.scope).copied().unwrap_or(-1);
        if context.epoch <= current {
            bail!("revocation must advance scope epoch")
        }
        self.epochs.insert(context.scope, context.epoch);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        let scopes: Vec<String> = self.scopes.keys().cloned().collect();
        for scope in scopes {
            self.drain_jobs(&scope).await;
            if let Some(mut entry) = self.scopes.remove(&scope) {
                entry.runtime.quiesce().await?;
            }
        }
        Ok(())
    }

    async fn replace_scope(&mut self, scope: &str) -> Result<()> {
        self.drain_jobs(scope).await;
        if let Some(mut entry) = self.scopes.remove(scope) {
            entry.runtime.quiesce().await?;
        }
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

    fn insert_job(
        &mut self,
        operation: String,
        context: ScopeContext,
        task: JoinHandle<Result<Value>>,
    ) -> Result<()> {
        self.ensure_live(&context)?;
        let key = job_key(&context, &operation);
        if self.jobs.contains_key(&key) {
            bail!("duplicate scoped op_id")
        }
        self.jobs.insert(key, Job { context, task });
        Ok(())
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
        self.ensure_epoch(context)?;
        let entry = self.scopes.get(&context.scope).context("scope not open")?;
        if entry.runtime.is_quiesced() {
            bail!("scope generation is quiesced")
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
