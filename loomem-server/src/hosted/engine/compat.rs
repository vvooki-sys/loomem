//! Compatibility with the synthetic Hosted journal and sealed-reader flow.
//! Writes still pass through the original `memory_store` implementation.
use super::*;
use crate::mcp::types::ToolResult;
use loomem_core::vector_search;
use serde::Serialize;
use std::collections::HashSet;

const READER_CONTEXT_FILE: &str = ".loomem-hosted-scope.json";

pub(super) fn handles(request: &Request) -> bool {
    matches!(
        request.cmd.as_str(),
        "prepare" | "build_async" | "search" | "read" | "close_scope"
    ) || (request.cmd == "open_scope" && request.root.is_some())
}

pub(super) async fn handle(engine: &mut SharedEngine, request: Request) -> Result<Value> {
    match request.cmd.as_str() {
        "open_scope" => open_reader(engine, request).await,
        "prepare" => prepare(engine, &request),
        "build_async" => build_async(engine, request).await,
        "search" => search(engine, request).await,
        "read" => read(engine, &request),
        "close_scope" => close_scope(engine, request).await,
        other => bail!("unknown compatibility command: {other}"),
    }
}

#[derive(Serialize)]
struct Score {
    id: String,
    score: f32,
}

pub(super) fn prepare(engine: &SharedEngine, request: &Request) -> Result<Value> {
    let context = request.context()?;
    let generation = request.generation()?;
    let operation = request.operation()?;
    ensure_current_generation(engine, &context, &generation)?;
    let content = request.content.as_deref().context("missing content")?;
    let embedder = engine
        .embedder
        .as_ref()
        .context("Hosted vector search requires the configured local embedder")?;
    let embedding = embedder.embed(content)?;
    ensure_embedding(engine, &embedding)?;
    Ok(engine.reply(
        context,
        json!({"generation": generation, "op_id": operation, "embedding": embedding}),
    ))
}

pub(super) async fn build_async(engine: &mut SharedEngine, request: Request) -> Result<Value> {
    let context = request.context()?;
    let generation = request.generation()?;
    ensure_current_generation(engine, &context, &generation)?;
    engine.ensure_job_available(&generation, &context)?;
    let records = request.records.context("missing records")?;
    validate_records(engine, &records)?;
    validate_failpoint(request.failpoint.as_deref())?;
    let state = engine.live_state(&context)?;
    let write_phase = Arc::new(crate::handlers::ingest::write_phase::WritePhase::new());
    let task_phase = write_phase.clone();
    let task = tokio::spawn(crate::handlers::ingest::write_phase::track(
        task_phase,
        store_records(
            state,
            records,
            context.scope.clone(),
            context.actor.clone(),
            request.failpoint,
            generation.clone(),
        ),
    ));
    engine.insert_job(generation.clone(), context.clone(), task, write_phase);
    Ok(engine.reply(context, json!({"pending": true, "job": generation})))
}

fn validate_failpoint(failpoint: Option<&str>) -> Result<()> {
    if !loomem_core::storage::persist::evaluation_failpoint_allowed(failpoint) {
        bail!("invalid synthetic failpoint")
    }
    Ok(())
}

async fn store_records(
    state: Arc<crate::AppState>,
    records: Vec<super::super::protocol::HostedRecord>,
    stream: String,
    actor: String,
    failpoint: Option<String>,
    generation: String,
) -> Result<Value> {
    let count = records.len();
    #[cfg(feature = "evaluation")]
    let failpoint = failpoint
        .as_deref()
        .and_then(loomem_core::storage::persist::EvaluationFailpoint::parse);
    #[cfg(not(feature = "evaluation"))]
    let _ = failpoint;
    for record in records {
        let store = crate::mcp::dispatcher::hosted_store(
            &state,
            json!({"content": record.content}),
            &stream,
            Some(actor.clone()),
            record.id,
        );
        #[cfg(feature = "evaluation")]
        let result =
            loomem_core::storage::persist::with_evaluation_failpoint(failpoint, store).await;
        #[cfg(not(feature = "evaluation"))]
        let result = store.await;
        let result =
            result.map_err(|error| anyhow::anyhow!("original memory_store failed: {error:?}"))?;
        ensure_tool_success(result)?;
    }
    Ok(json!({"generation": generation, "count": count}))
}

pub(super) async fn open_reader(engine: &mut SharedEngine, request: Request) -> Result<Value> {
    let context = request.context()?;
    let generation = request.generation()?;
    let root = reader_root(&request, &context, &generation)?;
    engine.advance_epoch(&context)?;
    engine.replace_scope(&context.scope).await?;
    let runtime =
        ScopeRuntime::open_reader(engine.config.clone(), &root, engine.embedder.clone()).await?;
    engine.epochs.insert(context.scope.clone(), context.epoch);
    engine.scopes.insert(
        context.scope.clone(),
        ScopeEntry {
            context: context.clone(),
            runtime,
        },
    );
    let contexts = engine.scopes.len();
    Ok(engine.reply(
        context,
        json!({"opened": true, "contexts": contexts, "generation": generation, "root": root}),
    ))
}

fn reader_root(request: &Request, context: &ScopeContext, generation: &str) -> Result<PathBuf> {
    let parent = PathBuf::from(request.root.as_deref().context("missing reader root")?)
        .canonicalize()
        .context("canonicalize Hosted reader root")?;
    let marker: ScopeContext = serde_json::from_slice(
        &std::fs::read(parent.join(READER_CONTEXT_FILE)).context("read Hosted reader marker")?,
    )?;
    if marker != *context {
        bail!("reader context mismatch")
    }
    let root = parent
        .join(generation)
        .canonicalize()
        .context("canonicalize Hosted reader generation")?;
    if root.parent() != Some(parent.as_path()) || !root.is_dir() {
        bail!("reader generation escapes its accepted root")
    }
    Ok(root)
}

pub(super) async fn close_scope(engine: &mut SharedEngine, request: Request) -> Result<Value> {
    let context = request.context()?;
    engine.ensure_live(&context)?;
    engine.replace_scope(&context.scope).await?;
    Ok(engine.reply(context, json!({"closed": true})))
}

pub(super) async fn search(engine: &SharedEngine, request: Request) -> Result<Value> {
    let context = request.context()?;
    let generation = request.generation()?;
    ensure_current_generation(engine, &context, &generation)?;
    let query = request.query.as_deref().context("missing query")?;
    let embedding = request.embedding.as_deref().context("missing embedding")?;
    ensure_embedding(engine, embedding)?;
    let state = engine.live_state(&context)?;
    let results = search_results(&state, query, embedding, &context.scope).await?;
    Ok(engine.reply(
        context,
        json!({
            "generation": generation,
            "vector": results["vector"],
            "bm25": results["bm25"],
            "hybrid": results["hybrid"],
        }),
    ))
}

async fn search_results(
    state: &Arc<crate::AppState>,
    query: &str,
    embedding: &[f32],
    scope: &str,
) -> Result<Value> {
    let embeddings = state.store.get_all_embeddings()?;
    let lexical = state.tantivy.lock().await.search(query, 20)?;
    let hybrid = state.hybrid_search.search_with_vector(
        lexical.clone(),
        &embeddings,
        embedding,
        Some(&state.store),
        Some(&[scope.to_string()]),
    )?;
    let vector: Vec<Score> = vector_search::vector_search(&embeddings, embedding, 20)
        .into_iter()
        .map(|(id, score)| Score { id, score })
        .collect();
    let bm25 = lexical
        .into_iter()
        .map(|result| Score {
            id: result.id,
            score: result.score,
        })
        .collect::<Vec<_>>();
    Ok(json!({"vector": vector, "bm25": bm25, "hybrid": hybrid}))
}

pub(super) fn read(engine: &SharedEngine, request: &Request) -> Result<Value> {
    let context = request.context()?;
    let generation = request.generation()?;
    ensure_current_generation(engine, &context, &generation)?;
    let id = request.id.as_deref().context("missing id")?;
    if !super::super::protocol::safe_name(id) {
        bail!("invalid id")
    }
    let state = engine.live_state(&context)?;
    let chunk = state.store.get_chunk(id)?;
    Ok(engine.reply(context, json!({"generation": generation, "chunk": chunk})))
}

fn ensure_current_generation(
    engine: &SharedEngine,
    context: &ScopeContext,
    generation: &str,
) -> Result<()> {
    engine.ensure_live(context)?;
    let entry = engine.scopes.get(&context.scope).context("scope absent")?;
    if entry
        .runtime
        .root
        .file_name()
        .and_then(|name| name.to_str())
        != Some(generation)
    {
        bail!("generation does not belong to the open scope")
    }
    Ok(())
}

fn ensure_embedding(engine: &SharedEngine, embedding: &[f32]) -> Result<()> {
    if embedding.len() != engine.config.llm.embedding_dim
        || embedding.iter().any(|value| !value.is_finite())
    {
        bail!("invalid embedding")
    }
    Ok(())
}

fn validate_records(
    engine: &SharedEngine,
    records: &[super::super::protocol::HostedRecord],
) -> Result<()> {
    let mut ids = HashSet::new();
    let mut operations = HashSet::new();
    for record in records {
        if !ids.insert(&record.id)
            || !operations.insert(&record.op_id)
            || !super::super::protocol::safe_name(&record.id)
            || !super::super::protocol::safe_name(&record.op_id)
        {
            bail!("invalid or duplicate Hosted record")
        }
        ensure_embedding(engine, &record.embedding)?;
    }
    Ok(())
}

fn ensure_tool_success(result: ToolResult) -> Result<()> {
    if result.is_error == Some(true) {
        let detail = result
            .content
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        bail!("original memory_store rejected Hosted record: {detail}")
    }
    Ok(())
}
