use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct TraceEvent {
    pub ts: String,
    pub event: &'static str,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

pub struct TraceLog {
    path: PathBuf,
}

impl TraceLog {
    pub fn new(dir: &str) -> Self {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let path = PathBuf::from(dir).join(format!("backfill-{}.jsonl", date));
        Self { path }
    }

    pub fn emit(&self, event: &'static str, data: serde_json::Value) {
        let entry = TraceEvent {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            event,
            data,
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}
