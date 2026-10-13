use anyhow::Result;
use loomem_core::config::RocksDbConfig;
use loomem_core::{storage::Chunk, RocksDbStore};

#[test]
fn superseded_chunks_do_not_consume_consolidation_scan_budget() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = RocksDbStore::open(
        temp.path(),
        &RocksDbConfig {
            max_open_files: 50,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )?;
    for index in 0..25 {
        let chunk: Chunk = serde_json::from_value(serde_json::json!({
            "id": format!("a-{index:02}"), "content": "old synthetic preference",
            "stream": "test", "level": 0, "score": 1.0, "timestamp": 0,
            "consolidated": false, "dormant": false, "in_progress": false,
            "is_latest": false
        }))?;
        store.store_chunk(&chunk)?;
    }
    let live: Chunk = serde_json::from_value(serde_json::json!({
        "id": "z-live", "content": "current synthetic preference", "stream": "test",
        "level": 0, "score": 1.0, "timestamp": 0, "consolidated": false,
        "dormant": false, "in_progress": false
    }))?;
    store.store_chunk(&live)?;
    let found = store.scan_l0_unconsolidated(0, 1)?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "z-live");
    Ok(())
}
