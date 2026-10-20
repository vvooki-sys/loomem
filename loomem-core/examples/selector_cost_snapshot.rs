//! Read-only private evaluation aid. Run only after closing the owning server.
//! Usage: selector_cost_snapshot DATABASE_PATH [SNAPSHOT_DATE]
use anyhow::{Context, Result};
use rocksdb::{IteratorMode, Options, DB};
use serde_json::{json, Map, Value};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().context("Database path required")?;
    let date = args
        .next()
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    let options = Options::default();
    let families = DB::list_cf(&options, &path).context("List database column families")?;
    let db = DB::open_cf_for_read_only(&options, &path, families, false)
        .context("Open accounting database read-only")?;
    println!("{}", read_snapshot(&db, &date)?);
    Ok(())
}

fn read_snapshot(db: &DB, date: &str) -> Result<Value> {
    let cf = db
        .cf_handle("costs")
        .context("Costs column family missing")?;
    let snapshot = db.snapshot();
    let mut costs = Map::new();
    let mut pending = Map::new();
    for item in snapshot.iterator_cf(&cf, IteratorMode::Start) {
        let (key, bytes) = item.context("Read cost snapshot")?;
        let key = String::from_utf8(key.to_vec()).context("Invalid cost key")?;
        if key.starts_with("cost:") {
            let value: loomem_core::cost_tracker::DailyCost =
                serde_json::from_slice(&bytes).context("Invalid daily cost")?;
            costs.insert(key, serde_json::to_value(value)?);
        } else if key.starts_with("selector:pending:") {
            let value: f64 = serde_json::from_slice(&bytes).context("Invalid pending amount")?;
            pending.insert(key, Value::from(value));
        }
    }
    Ok(json!({"snapshot_date":date,"daily_costs":costs,"pending":pending}))
}
