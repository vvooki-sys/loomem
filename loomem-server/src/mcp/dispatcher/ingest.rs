//! MCP transcript sources and derived facts share an explicit provenance contract.
use super::*;
use loomem_core::memory_extractor::{ExtractedFact, ExtractionChat, ExtractionOutcome};
use loomem_core::storage::{Chunk, ExtractionMeta, ProvenanceRole};

trait Clock {
    fn timestamp(&self) -> i64;
}

struct SystemClock;

impl Clock for SystemClock {
    fn timestamp(&self) -> i64 {
        crate::evaluation_clock::now().timestamp()
    }
}

#[cfg(test)]
struct FixedClock;

#[cfg(test)]
impl Clock for FixedClock {
    fn timestamp(&self) -> i64 {
        1_792_432_800
    }
}

#[derive(Deserialize)]
pub(super) struct IngestArgs {
    content: String,
    conversation_date: Option<String>,
}

struct IngestContext<'a> {
    state: &'a Arc<AppState>,
    source_id: String,
    content: String,
    conversation_date: Option<String>,
    stream: &'a str,
    user_id: Option<String>,
    timestamp: u64,
}

pub(super) trait IngestPersistence {
    async fn persist(
        &self,
        state: &Arc<AppState>,
        chunk: Chunk,
    ) -> Result<String, handlers::AppError>;
}

pub(super) struct RealPersistence;

impl IngestPersistence for RealPersistence {
    async fn persist(
        &self,
        state: &Arc<AppState>,
        chunk: Chunk,
    ) -> Result<String, handlers::AppError> {
        let content = chunk.content.clone();
        let stream = chunk.stream.clone();
        let level = chunk.level;
        let timestamp = i64::try_from(chunk.timestamp).map_err(|_| {
            handlers::AppError::Internal(anyhow::anyhow!("Ingest timestamp out of range"))
        })?;
        handlers::ingest::persist_chunk(
            state, chunk, &content, "default", "default", &stream, level, timestamp, None, None,
        )
        .await
    }
}

pub(super) async fn tool_ingest(
    state: &Arc<AppState>,
    args: Value,
    stream: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    let context = prepare(state, args, stream, user_id, &SystemClock)?;
    let source = match store_source(&context, &RealPersistence).await {
        Ok(source) => source,
        Err(result) => return Ok(result),
    };
    let report = fragments::store(&context, &source, &RealPersistence).await;
    let result = extract_with_configured_chat(&context).await?;
    Ok(fragments::append_report(result, report.as_ref()))
}

async fn extract_with_configured_chat(
    context: &IngestContext<'_>,
) -> Result<ToolResult, JsonRpcError> {
    let state = context.state;
    if !state.config.knowledge_extraction.enabled {
        return Ok(ToolResult::text(format!(
            "Stored source transcript (id: {}). Knowledge extraction disabled.",
            context.source_id
        )));
    }
    let Some(key) = state.config.llm.get_api_key() else {
        return Ok(ToolResult::error(with_source(
            context,
            "Extraction failed: API key unavailable. No facts stored.",
        )));
    };
    let chat = loomem_core::memory_extractor::HttpExtractionChat::new(&state.http_client, key);
    extract(&chat, &RealPersistence, context).await
}

#[cfg(test)]
pub(super) async fn tool_ingest_with(
    chat: &impl ExtractionChat,
    state: &Arc<AppState>,
    args: Value,
    stream: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    tool_ingest_with_sink(chat, &RealPersistence, state, args, stream, user_id).await
}

#[cfg(test)]
pub(super) async fn tool_ingest_with_sink(
    chat: &impl ExtractionChat,
    sink: &impl IngestPersistence,
    state: &Arc<AppState>,
    args: Value,
    stream: &str,
    user_id: Option<String>,
) -> Result<ToolResult, JsonRpcError> {
    let context = prepare(state, args, stream, user_id, &FixedClock)?;
    let source = match store_source(&context, sink).await {
        Ok(source) => source,
        Err(result) => return Ok(result),
    };
    let report = fragments::store(&context, &source, sink).await;
    let result = extract(chat, sink, &context).await?;
    Ok(fragments::append_report(result, report.as_ref()))
}

fn prepare<'a>(
    state: &'a Arc<AppState>,
    args: Value,
    stream: &'a str,
    user_id: Option<String>,
    clock: &impl Clock,
) -> Result<IngestContext<'a>, JsonRpcError> {
    let args: IngestArgs = serde_json::from_value(args)
        .map_err(|_| JsonRpcError::invalid_params("Invalid ingest arguments"))?;
    if let Some(date) = args.conversation_date.as_deref() {
        let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| JsonRpcError::invalid_params("conversation_date must be YYYY-MM-DD"))?;
        if parsed.format("%Y-%m-%d").to_string() != date {
            return Err(JsonRpcError::invalid_params(
                "conversation_date must be YYYY-MM-DD",
            ));
        }
    }
    let timestamp = u64::try_from(clock.timestamp())
        .map_err(|_| JsonRpcError::invalid_params("Ingest timestamp out of range"))?;
    Ok(IngestContext {
        state,
        source_id: uuid::Uuid::new_v4().to_string(),
        content: state.pii_filter.redact_for_sink(&args.content),
        conversation_date: args.conversation_date,
        stream,
        user_id,
        timestamp,
    })
}

/// Persist the canonical source; on success returns the chunk exactly as it
/// was handed to the sink, so fragments can be cut from the same content.
async fn store_source(
    context: &IngestContext<'_>,
    sink: &impl IngestPersistence,
) -> Result<Chunk, ToolResult> {
    let mut source = base_chunk(
        context,
        context.source_id.clone(),
        context.content.clone(),
        "source",
    );
    source.consolidated = true;
    source.provenance_role = ProvenanceRole::Evidence;
    source.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    sink.persist(context.state, source.clone()).await.map(|_| source).map_err(|_| {
        ToolResult::error(format!(
            "Source persistence failed (id: {}). Source may exist; no extraction or fact writes started.",
            context.source_id))
    })
}

async fn extract(
    chat: &impl ExtractionChat,
    sink: &impl IngestPersistence,
    context: &IngestContext<'_>,
) -> Result<ToolResult, JsonRpcError> {
    let reference_date = context.conversation_date.as_deref().unwrap_or("unknown");
    let outcome = loomem_core::memory_extractor::extract_knowledge_with(
        chat,
        &context.state.config.knowledge_extraction,
        &context.content,
        reference_date,
    )
    .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            let (status, reason) = error.status_and_reason();
            return Ok(ToolResult::error(with_source(
                context,
                &format!("Extraction failed ({status}): {reason}. No facts stored."),
            )));
        }
    };
    let mut stored = 0;
    let mut skipped = 0;
    for fact in &outcome.facts {
        match sink.persist(context.state, fact_chunk(context, fact)).await {
            Ok(_) => stored += 1,
            Err(_) => skipped += 1,
        }
    }
    Ok(ToolResult::text(with_source(
        context,
        &ingest_summary(&outcome, stored, skipped),
    )))
}

fn with_source(context: &IngestContext<'_>, message: &str) -> String {
    format!("Source stored (id: {}). {message}", context.source_id)
}

fn marker(context: &IngestContext<'_>, kind: &str) -> Value {
    serde_json::json!({"_loomem_ingest": {"version":1,"kind":kind,
        "conversation_date":context.conversation_date}})
}

fn base_chunk(context: &IngestContext<'_>, id: String, content: String, kind: &str) -> Chunk {
    Chunk {
        id,
        content,
        stream: context.stream.to_string(),
        level: 0,
        score: 1.0,
        timestamp: context.timestamp,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: Some(marker(context, kind)),
        importance: Some(1.0),
        persistent: true,
        last_implicit_boost: None,
        access_count: 0,
        source: Some(SourceTag::from_agent("mcp-ingest")),
        created_by: Some("mcp".into()),
        updated_at: Some(context.timestamp),
        valid_from: None,
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: Some("a2".into()),
        ingester_user_id: context.user_id.clone(),
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: ProvenanceRole::Claim,
        fragment: None,
    }
}

fn fact_chunk(context: &IngestContext<'_>, fact: &ExtractedFact) -> Chunk {
    let mut chunk = base_chunk(
        context,
        uuid::Uuid::new_v4().to_string(),
        context.state.pii_filter.redact_for_sink(&fact.content),
        "fact",
    );
    let meta = fact_meta(context, fact);
    chunk.level = 1;
    chunk.source_ids = Some(vec![context.source_id.clone()]);
    chunk.importance = Some(if fact.fact_type == "preference_or_decision" {
        2.0_f64.max(fact.confidence)
    } else {
        fact.confidence
    });
    chunk.valid_from = meta.event_date_unix();
    chunk.extraction_meta = Some(meta);
    chunk.memory_type = Some(
        match fact.fact_type.as_str() {
            "fact" | "preference_or_decision" | "experience" => "static",
            _ => "dynamic",
        }
        .to_string(),
    );
    chunk
}

fn fact_meta(context: &IngestContext<'_>, fact: &ExtractedFact) -> ExtractionMeta {
    let mut meta = fact.to_extraction_meta(
        Some(context.source_id.clone()),
        &context.state.config.knowledge_extraction.model,
    );
    for value in [
        &mut meta.subject,
        &mut meta.event_date,
        &mut meta.event_date_context,
        &mut meta.topic,
        &mut meta.attributed_to,
        &mut meta.extraction_model,
    ]
    .into_iter()
    .flatten()
    {
        *value = context.state.pii_filter.redact_for_sink(value);
    }
    meta.event_date = meta.event_date.filter(|date| {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .is_some_and(|parsed| parsed.format("%Y-%m-%d").to_string() == *date)
    });
    meta
}

fn ingest_summary(outcome: &ExtractionOutcome, stored: usize, skipped: usize) -> String {
    let mut message = format!(
        "Extracted {} facts from conversation, stored {}, skipped {}.",
        outcome.facts.len(),
        stored,
        skipped
    );
    if let Some(first) = outcome.failures.first() {
        message.push_str(&format!(" Warning: {} extraction chunk(s) failed; facts may be incomplete. First error ({}): {}",
            outcome.failures.len(), first.status_label(), first.reason));
    }
    if skipped > 0 {
        message
            .push_str(" Unconfirmed fact writes may exist after partial storage/index failures.");
    }
    message
}

mod fragments;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod fragment_tests;
#[cfg(test)]
mod tests;
