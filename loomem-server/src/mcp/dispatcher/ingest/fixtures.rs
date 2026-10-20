//! Synthetic transport fixtures backed by the normal temporary RocksDB app.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use loomem_core::memory_extractor::{ChatHttpReply, ExtractionChat};
use loomem_core::storage::Chunk;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::AppState;

pub(super) struct Chat {
    pub requests: Mutex<Vec<Value>>,
    replies: Mutex<VecDeque<anyhow::Result<ChatHttpReply>>>,
}

impl Chat {
    pub fn facts(facts: Vec<Value>) -> Self {
        Self::sequence(vec![Ok(reply(facts))])
    }

    pub fn sequence(replies: Vec<anyhow::Result<ChatHttpReply>>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        }
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl ExtractionChat for Chat {
    async fn chat(&self, request: &Value) -> anyhow::Result<ChatHttpReply> {
        self.requests.lock().unwrap().push(request.clone());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("synthetic reply sequence exhausted")))
    }
}

pub(super) struct BlockedChat {
    pub entered: Arc<Notify>,
    pub release: Arc<Notify>,
}

impl ExtractionChat for BlockedChat {
    async fn chat(&self, _request: &Value) -> anyhow::Result<ChatHttpReply> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(reply(vec![fact(None)]))
    }
}

pub(super) fn reply(facts: Vec<Value>) -> ChatHttpReply {
    ChatHttpReply {
        status: 200,
        body: json!({"choices":[{"message":{"content":json!({"facts":facts}).to_string()}}]})
            .to_string(),
    }
}

pub(super) fn fact(date: Option<&str>) -> Value {
    json!({"content":"The user prefers quiet accommodation.",
        "fact_type":"preference_or_decision","subject":"accommodation",
        "event_date":date,"event_date_context":null,"attributed_to":"user","confidence":0.9})
}

pub(super) fn fixture(chunk_tokens: usize) -> (axum::Router, Arc<AppState>) {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.knowledge_extraction.enabled = true;
    config.knowledge_extraction.max_transcript_tokens = chunk_tokens;
    config.knowledge_extraction.contradiction_check = false;
    config.llm.api_key = None;
    config.llm.api_key_env = "P15_ABSENT".into();
    crate::tests::make_test_app_cfg(config)
}

pub(super) fn chunks(state: &AppState, stream: &str) -> Vec<Chunk> {
    state
        .store
        .get_all_chunks()
        .unwrap()
        .into_iter()
        .filter(|chunk| chunk.stream == stream)
        .collect()
}

pub(super) fn kind(chunk: &Chunk) -> &str {
    chunk
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("_loomem_ingest"))
        .and_then(|marker| marker.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub(super) fn args(content: &str) -> Value {
    json!({"content":content,"conversation_date":"2024-03-20"})
}

pub(super) fn assert_link(fact: &Chunk, source: &Chunk) {
    assert_eq!(fact.stream, source.stream);
    assert_eq!(
        fact.source_ids.as_deref(),
        Some([source.id.clone()].as_slice())
    );
    assert_eq!(
        fact.extraction_meta
            .as_ref()
            .unwrap()
            .extracted_from
            .as_deref(),
        Some(source.id.as_str())
    );
    assert!(source.deleted_at.is_none());
}

pub(super) async fn get(app: &axum::Router, id: &str) -> (axum::http::StatusCode, Value) {
    let request = axum::http::Request::builder()
        .uri(format!("/api/memories/{id}"))
        .header("Authorization", "Bearer admin-token")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, bytes) = crate::tests::send(app.clone(), request).await;
    (status, serde_json::from_slice(&bytes).unwrap())
}
