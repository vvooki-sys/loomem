//! Partition only the mutable control state; model and process identity stay shared.
use super::*;
use std::collections::HashSet;

#[derive(Clone)]
pub(crate) struct EngineSeed {
    root: PathBuf,
    config: Config,
    embedder: Option<Arc<LocalEmbedder>>,
    instance: String,
    holds: Arc<Holds>,
}

impl EngineSeed {
    pub(crate) fn leaf(&self, scope: &str) -> SharedEngine {
        SharedEngine {
            root: self.root.clone(),
            config: self.config.clone(),
            embedder: self.embedder.clone(),
            instance: self.instance.clone(),
            holds: self.holds.clone(),
            scopes: HashMap::new(),
            epochs: HashMap::new(),
            jobs: HashMap::new(),
            next_job: 0,
            bound_scope: Some(scope.to_owned()),
        }
    }
}

impl SharedEngine {
    pub(super) async fn restore_scope(&mut self, request: Request) -> Result<Value> {
        let context = request.context()?;
        let generation = request.generation()?;
        let source = self.restore_source(&request)?;
        self.advance_epoch(&context)?;
        self.replace_scope(&context.scope).await?;
        let root = self.scope_root(&context, &generation)?;
        copy_checkpoint(source, root.clone()).await?;
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

    /// Preserve even a pre-populated engine passed to the concurrent front end.
    pub(crate) fn into_leaves(mut self) -> (EngineSeed, HashMap<String, SharedEngine>) {
        let seed = EngineSeed {
            root: self.root.clone(),
            config: self.config.clone(),
            embedder: self.embedder.clone(),
            instance: self.instance.clone(),
            holds: self.holds.clone(),
        };
        let mut names: HashSet<String> = self.epochs.keys().cloned().collect();
        names.extend(self.scopes.keys().cloned());
        names.extend(self.jobs.values().map(|job| job.context.scope.clone()));
        let mut leaves: HashMap<_, _> = names
            .into_iter()
            .map(|name| {
                let mut leaf = seed.leaf(&name);
                leaf.next_job = self.next_job;
                (name, leaf)
            })
            .collect();
        for (name, state) in self.scopes.drain() {
            leaves
                .entry(name.clone())
                .or_insert_with(|| seed.leaf(&name))
                .scopes
                .insert(name, state);
        }
        for (name, epoch) in self.epochs.drain() {
            leaves
                .entry(name.clone())
                .or_insert_with(|| seed.leaf(&name))
                .epochs
                .insert(name, epoch);
        }
        for (key, job) in self.jobs.drain() {
            let name = job.context.scope.clone();
            leaves
                .entry(name.clone())
                .or_insert_with(|| seed.leaf(&name))
                .jobs
                .insert(key, job);
        }
        (seed, leaves)
    }

    pub(crate) fn open_contexts(&self) -> usize {
        self.scopes.len()
    }

    pub(super) fn ensure_bound_context(&self, context: &ScopeContext) -> Result<()> {
        if self
            .bound_scope
            .as_ref()
            .is_some_and(|scope| scope != &context.scope)
        {
            bail!("request does not belong to this scope engine")
        }
        Ok(())
    }
}

/// Copying a checkpoint must not occupy a Tokio executor thread.
pub(super) async fn copy_checkpoint(source: PathBuf, root: PathBuf) -> Result<()> {
    super::super::state::native_io(move || {
        std::fs::create_dir_all(&root).context("create Hosted restore root")?;
        copy_tree(&source, &root.join("rocksdb"))
    })
    .await
}
