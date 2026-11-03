//! The concurrent front end preserves process, scope, actor and epoch identity.
//! Uses real temporary RocksDB/Tantivy; this is not an ONNX capacity benchmark.
#![cfg(feature = "hosted-engine")]

use loomem_core::Config;
use loomem_server::hosted::{serve, ServeLimits, SharedEngine};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

fn engine(root: &Path) -> SharedEngine {
    let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.toml");
    let mut config = Config::load(&reference).expect("reference config");
    let entities = root.join("entities.toml");
    std::fs::write(&entities, "").unwrap();
    config.llm.embedding_provider = "openai".into();
    config.storage.vector_enabled = false;
    config.search.entities_file = entities.to_string_lossy().into_owned();
    config.search.synonyms_file = root.join("synonyms.toml").to_string_lossy().into_owned();
    config.search.rerank_enabled = false;
    config.scheduler.enabled = false;
    config.entity_extraction.enabled = false;
    config.knowledge_extraction.enabled = false;
    config.dream.enabled = false;
    config.pii.blocklist_file = root.join("blocklist.txt").to_string_lossy().into_owned();
    SharedEngine::new(&root.join("engine"), config).expect("synthetic engine")
}

struct Client {
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<Value>,
    next: u64,
}

impl Client {
    async fn call(&mut self, mut request: Value) -> Value {
        self.next += 1;
        let id = self.next.to_string();
        request["request_id"] = json!(id);
        self.tx.send(request.to_string()).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(30), self.rx.recv())
            .await
            .expect("bounded reply")
            .expect("reply");
        assert_eq!(reply["request_id"], json!(id));
        reply
    }
}

fn context(actor: &str, scope: &str, epoch: i64) -> Value {
    json!({"actor": actor, "scope": scope, "epoch": epoch})
}

#[tokio::test]
async fn independent_scopes_preserve_actor_job_and_epoch_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(root.path());
    let (tx, incoming) = mpsc::channel(16);
    let (outgoing, rx) = mpsc::channel(16);
    let task = tokio::spawn(serve(engine, incoming, outgoing, ServeLimits::default()));
    let mut client = Client { tx, rx, next: 0 };
    let a = context("alice", "a", 1);
    let peer = context("bob", "a", 1);
    let b = context("carol", "b", 1);
    let opened_a = client
        .call(json!({"cmd":"open_scope","context":a,"generation":"live"}))
        .await;
    let opened_b = client
        .call(json!({"cmd":"open_scope","context":b,"generation":"live"}))
        .await;
    assert_eq!(opened_a["ok"], json!(true));
    assert_eq!(opened_b["ok"], json!(true));
    assert_eq!(opened_a["engine_pid"], opened_b["engine_pid"]);
    assert_eq!(opened_a["engine_instance"], opened_b["engine_instance"]);
    let accepted = client
        .call(json!({"cmd":"tool_async","context":a,
        "op_id":"one","name":"memory_status","args":{}}))
        .await;
    assert_eq!(accepted["accepted"], json!(true));
    for other in [&peer, &b] {
        let denied = client
            .call(json!({"cmd":"await","context":other,"op_id":"one"}))
            .await;
        assert_eq!(denied["ok"], json!(false));
    }
    let result = client
        .call(json!({"cmd":"await","context":a,"op_id":"one"}))
        .await;
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["context"], a);
    let revoked = client
        .call(json!({"cmd":"revoke","context":context("alice","a",2)}))
        .await;
    assert_eq!(revoked["ok"], json!(true));
    let stale = client
        .call(json!({"cmd":"open_scope","context":a,"generation":"live"}))
        .await;
    assert_eq!(stale["ok"], json!(false));
    let unaffected = client.call(json!({"cmd":"status","context":b})).await;
    assert_eq!(unaffected["ok"], json!(true));
    let closed = client.call(json!({"cmd":"shutdown"})).await;
    assert_eq!(closed["ok"], json!(true));
    assert_eq!(closed["in_flight_drained"], json!(true));
    task.await.unwrap().unwrap();
    assert!(client.rx.recv().await.is_none());
}
