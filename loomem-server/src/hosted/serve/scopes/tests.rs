use super::*;
use crate::hosted::test_support::new_engine;
use std::time::Duration;

fn request(command: &str, scope: &str, epoch: i64) -> Request {
    serde_json::from_value(json!({
        "cmd": command,
        "context": {"actor": "actor", "scope": scope, "epoch": epoch},
        "generation": "live",
    }))
    .expect("request")
}

#[tokio::test]
async fn unknown_or_missing_context_does_not_allocate_a_scope() {
    let (_root, engine) = new_engine().await;
    let scopes = Scopes::new(engine);
    assert!(scopes.slot(&request("unknown", "a", 1)).is_err());
    let mut absent = request("status", "a", 1);
    absent.context = None;
    assert!(scopes.slot(&absent).is_err());
    assert!(scopes.slots.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_open_uses_one_slot_and_process_identity() {
    let (_root, engine) = new_engine().await;
    let instance = engine.instance().to_owned();
    let scopes = Scopes::new(engine);
    let (a, again) = tokio::join!(
        scopes.handle(request("open_scope", "a", 1)),
        scopes.handle(request("open_scope", "a", 1)),
    );
    let a = a.unwrap();
    let again = again.unwrap();
    let b = scopes.handle(request("open_scope", "b", 1)).await.unwrap();
    assert_eq!(a["engine_instance"], json!(instance));
    assert_eq!(a["engine_instance"], again["engine_instance"]);
    assert_eq!(a["engine_instance"], b["engine_instance"]);
    assert_eq!(a["engine_pid"], b["engine_pid"]);
    assert_eq!(scopes.opened.load(Ordering::SeqCst), 2);
    assert_eq!(scopes.slots.lock().unwrap().len(), 2);
    scopes.shutdown().await.unwrap();
    assert_eq!(scopes.opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn closing_and_revoking_preserves_the_epoch_tombstone() {
    let (_root, engine) = new_engine().await;
    let scopes = Scopes::new(engine);
    scopes.handle(request("open_scope", "a", 2)).await.unwrap();
    let original = scopes.slot(&request("status", "a", 2)).unwrap();
    scopes.handle(request("close_scope", "a", 2)).await.unwrap();
    assert!(scopes.handle(request("open_scope", "a", 1)).await.is_err());
    scopes.handle(request("revoke", "a", 3)).await.unwrap();
    assert!(scopes.handle(request("open_scope", "a", 2)).await.is_err());
    scopes.handle(request("open_scope", "a", 3)).await.unwrap();
    let current = scopes.slot(&request("status", "a", 3)).unwrap();
    assert!(Arc::ptr_eq(&original, &current));
    scopes.shutdown().await.unwrap();
}

#[tokio::test]
async fn populated_input_engine_keeps_open_scopes_and_epochs() {
    let (_root, mut engine) = new_engine().await;
    engine.handle(request("open_scope", "a", 7)).await.unwrap();
    engine.handle(request("revoke", "b", 9)).await.unwrap();
    let scopes = Scopes::new(engine);
    assert_eq!(scopes.opened.load(Ordering::SeqCst), 1);
    assert_eq!(scopes.slots.lock().unwrap().len(), 2);
    scopes.handle(request("status", "a", 7)).await.unwrap();
    assert!(scopes.handle(request("open_scope", "b", 8)).await.is_err());
    scopes.shutdown().await.unwrap();
}

#[tokio::test]
async fn leaf_rejects_a_context_for_another_scope() {
    let (_root, engine) = new_engine().await;
    let scopes = Scopes::new(engine);
    let slot = scopes.slot(&request("open_scope", "a", 1)).unwrap();
    let mut leaf = slot.lock().await;
    assert!(leaf.handle(request("open_scope", "b", 1)).await.is_err());
    assert_eq!(leaf.open_contexts(), 0);
    assert!(leaf.begin_await(&request("await", "b", 1)).is_err());
}

#[tokio::test]
async fn shutdown_snapshot_refuses_new_scope_slots() {
    let (_root, engine) = new_engine().await;
    let scopes = Scopes::new(engine);
    scopes.handle(request("open_scope", "a", 1)).await.unwrap();
    scopes.shutdown().await.unwrap();
    assert!(scopes.slot(&request("open_scope", "b", 1)).is_err());
    assert_eq!(scopes.slots.lock().unwrap().len(), 1);
}

#[cfg(feature = "evaluation")]
#[tokio::test]
async fn waiting_reopen_of_a_does_not_block_b_on_a_single_executor_thread() {
    let (_root, engine) = new_engine().await;
    let holds = engine.holds();
    let scopes = Arc::new(Scopes::new(engine));
    scopes.handle(request("open_scope", "a", 1)).await.unwrap();
    scopes.handle(request("open_scope", "b", 1)).await.unwrap();
    let held: Request = serde_json::from_value(json!({
        "cmd": "tool_async", "context": {"actor": "actor", "scope": "a", "epoch": 1},
        "op_id": "held", "name": "memory_status", "args": {}, "failpoint": "hosted_hold",
    }))
    .unwrap();
    scopes.handle(held).await.unwrap();
    let a = scopes.slot(&request("status", "a", 1)).unwrap();
    let worker_scopes = scopes.clone();
    let worker = tokio::spawn(async move {
        let mut reopen = request("open_scope", "a", 1);
        reopen.generation = Some("next".into());
        worker_scopes.handle(reopen).await
    });
    // Do not rely on request scheduling order or sleeps: observe the actual
    // lifecycle slot locked by the reopen while it waits for the held job.
    tokio::time::timeout(Duration::from_secs(5), async {
        while a.try_lock().is_ok() {
            assert!(!worker.is_finished(), "reopen unexpectedly finished");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reopen entered its scope lock");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        scopes.handle(request("status", "b", 1)),
    )
    .await;
    // Shutdown can time out behind an active durable lifecycle without
    // cancelling that independently owned reopen or deleting its slot.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), scopes.shutdown())
            .await
            .is_err()
    );
    assert!(!worker.is_finished());
    assert!(scopes.slot(&request("open_scope", "new", 1)).is_err());
    assert!(Arc::ptr_eq(
        &a,
        &scopes.slot(&request("status", "a", 1)).unwrap()
    ));
    holds.release().unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(
        result.expect("B answered before releasing A").unwrap()["ok"],
        json!(true)
    );
    scopes.shutdown().await.unwrap();
}

#[tokio::test]
async fn reader_context_count_is_process_wide_and_tracks_failed_reopen() {
    let (root, mut engine) = new_engine().await;
    engine.handle(request("open_scope", "a", 1)).await.unwrap();
    engine.handle(request("quiesce", "a", 1)).await.unwrap();
    engine.handle(request("shutdown", "a", 1)).await.unwrap();
    let reader_parent = root.path().join("engine/a");
    let marker = json!({"actor": "actor", "scope": "a", "epoch": 1});
    std::fs::write(
        reader_parent.join(".loomem-hosted-scope.json"),
        marker.to_string(),
    )
    .unwrap();
    let scopes = Scopes::new(engine);
    scopes.handle(request("open_scope", "b", 1)).await.unwrap();
    let mut reader = request("open_scope", "a", 1);
    reader.root = Some(reader_parent.to_string_lossy().into_owned());
    let opened = scopes.handle(reader.clone()).await.unwrap();
    assert_eq!(opened["contexts"], json!(2));
    scopes.handle(request("close_scope", "a", 1)).await.unwrap();
    assert_eq!(scopes.opened.load(Ordering::SeqCst), 1);
    let opened = scopes.handle(reader).await.unwrap();
    assert_eq!(opened["contexts"], json!(2));
    // The externally held DB prevents replacement after the old reader was
    // closed. Count must describe actual open runtimes even on error.
    let occupied = root.path().join("engine/a/occupied/rocksdb");
    let config = loomem_core::Config::load(&root.path().join("config.toml"))
        .expect("load fixture RocksDB configuration");
    let db = loomem_core::RocksDbStore::open(&occupied, &config.storage.rocksdb);
    if let Ok(_held_db) = db {
        let mut replacement = request("open_scope", "a", 1);
        replacement.generation = Some("occupied".into());
        assert!(scopes.handle(replacement).await.is_err());
        assert_eq!(scopes.opened.load(Ordering::SeqCst), 1);
    } else {
        panic!("fixture must hold the replacement RocksDB lock");
    }
    scopes.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_unknown_scope_lookups_do_not_retain_slots() {
    let (_root, engine) = new_engine().await;
    let scopes = Scopes::new(engine);
    for index in 0..32 {
        let scope = format!("unknown-{index}");
        for command in [
            "status",
            "tools/list",
            "tool_async",
            "worker_async",
            "quiesce",
            "prepare",
            "build_async",
            "search",
            "read",
            "close_scope",
        ] {
            assert!(scopes.handle(request(command, &scope, 1)).await.is_err());
        }
        let mut awaiting = request("await", &scope, 1);
        awaiting.op_id = Some("unknown-job".into());
        assert!(scopes.await_job(awaiting).await.is_err());
        assert!(scopes.slots.lock().unwrap().is_empty());
    }
    assert_eq!(scopes.opened.load(Ordering::SeqCst), 0);
    scopes
        .handle(request("open_scope", "unknown-0", 1))
        .await
        .unwrap();
    scopes
        .handle(request("status", "unknown-0", 1))
        .await
        .unwrap();
    scopes
        .handle(request("tools/list", "unknown-0", 1))
        .await
        .unwrap();
    assert_eq!(scopes.slots.lock().unwrap().len(), 1);
    scopes.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_revoke_establishes_a_stable_epoch_tombstone() {
    let (_root, engine) = new_engine().await;
    let scopes = Scopes::new(engine);
    scopes.handle(request("revoke", "a", 7)).await.unwrap();
    let tombstone = scopes.slot(&request("status", "a", 7)).unwrap();
    assert!(scopes.handle(request("status", "a", 7)).await.is_err());
    assert!(scopes.handle(request("open_scope", "a", 6)).await.is_err());
    assert_eq!(scopes.slots.lock().unwrap().len(), 1);
    scopes.handle(request("open_scope", "a", 7)).await.unwrap();
    assert!(Arc::ptr_eq(
        &tombstone,
        &scopes.slot(&request("status", "a", 7)).unwrap()
    ));
    scopes.handle(request("status", "a", 7)).await.unwrap();
    scopes.shutdown().await.unwrap();
}
