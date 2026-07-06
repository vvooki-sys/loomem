use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Parser)]
#[command(name = "loomem-cli")]
#[command(about = "Loomem CLI - interact with Loomem memory server", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, default_value = "http://localhost:3030", global = true)]
    url: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Store content in Loomem
    Store {
        /// Content to store
        content: String,

        /// Stream ID (e.g., "100" for legacy-agent raw)
        #[arg(long)]
        stream: Option<String>,

        /// Namespace name (resolves to stream ID from config)
        #[arg(long)]
        ns: Option<String>,

        /// User ID
        #[arg(long)]
        user_id: Option<String>,

        /// App ID
        #[arg(long)]
        app_id: Option<String>,

        /// Level (0=raw, 1=compressed, 2=semantic)
        #[arg(long)]
        level: Option<i32>,

        /// Mark chunk as persistent (skip decay)
        #[arg(long)]
        persistent: bool,
    },

    /// Search for content in Loomem
    Search {
        /// Query string
        query: String,

        /// Number of results to return
        #[arg(long, default_value = "5")]
        top_k: usize,

        /// Stream ID to filter by
        #[arg(long)]
        stream: Option<String>,

        /// Namespace name (resolves to stream ID; use "all" to search all namespaces)
        #[arg(long)]
        ns: Option<String>,

        /// User ID to filter by
        #[arg(long)]
        user_id: Option<String>,

        /// Filter results to only those from this source agent (e.g. "legacy-agent", "claude-code")
        #[arg(long)]
        source: Option<String>,

        /// Exclude results from these source agents (comma-separated, e.g. "legacy-agent,claude-code")
        #[arg(long)]
        exclude_source: Option<String>,
    },

    /// Delete a single memory by UUID
    Delete {
        /// Memory UUID to delete
        id: String,

        /// Namespace name (optional, for context in audit log)
        #[arg(long)]
        ns: Option<String>,
    },

    /// Purge all memories in a namespace
    PurgeNs {
        /// Stream ID or namespace name
        #[arg(long)]
        stream: Option<String>,

        /// Namespace name (resolves to stream ID)
        #[arg(long)]
        ns: Option<String>,

        /// Dry run (show what would be deleted without deleting)
        #[arg(long)]
        dry_run: bool,

        /// Confirm deletion (required for actual deletion)
        #[arg(long)]
        confirmed: bool,
    },

    /// Get Loomem server status
    Status,

    /// Show per-stream statistics (health, distributions, activity, extraction)
    Stats {
        /// Stream ID to inspect. With --admin, selects that stream; omit to
        /// aggregate every stream. Without --admin it is ignored (your own
        /// stream is used).
        #[arg(long)]
        stream: Option<String>,

        /// Query the admin endpoint (/v1/admin/stream-stats) instead of your
        /// own (/v1/my/stream-stats). Needs an admin token.
        #[arg(long)]
        admin: bool,

        /// Bearer token for the server (falls back to $LOOMEM_AUTH_TOKEN).
        #[arg(long)]
        token: Option<String>,
    },

    /// Ingest a conversation and extract memories
    IngestConversation {
        /// Stream ID
        #[arg(long)]
        stream: Option<String>,

        /// Namespace name (resolves to stream ID)
        #[arg(long)]
        ns: Option<String>,
    },

    /// Generate user profile from memories
    Profile {
        /// Stream ID
        #[arg(long)]
        stream: Option<String>,

        /// Namespace name (resolves to stream ID)
        #[arg(long)]
        ns: Option<String>,

        /// Force regeneration (ignore cache)
        #[arg(long)]
        refresh: bool,

        /// Output format: json or markdown
        #[arg(long, default_value = "json")]
        format: String,
    },

    /// Pack context for system prompt injection
    ContextPack {
        /// Query for relevant memories
        query: Option<String>,

        /// Stream ID
        #[arg(long)]
        stream: Option<String>,

        /// Namespace name
        #[arg(long)]
        ns: Option<String>,

        /// Token budget
        #[arg(long, default_value = "4000")]
        budget: usize,

        /// Sections to include (comma-separated: profile,relevant,recent)
        #[arg(long, default_value = "profile,relevant,recent")]
        sections: String,
    },

    /// Show version history chain for a memory
    History {
        /// Chunk ID to trace history for
        id: String,

        /// Maximum chain length
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Bridge a stdio MCP client (the Claude desktop app, Cowork, Cursor, …) to
    /// the server's HTTP `/mcp` endpoint. Reads newline-delimited JSON-RPC on
    /// stdin, forwards each message to `<url>/mcp`, and writes replies to stdout.
    /// This is a native replacement for `npx mcp-remote` — no Node required.
    McpStdio {
        /// Bearer token for the server (falls back to $LOOMEM_AUTH_TOKEN).
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Serialize)]
struct StoreRequest {
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    persistent: Option<bool>,
}

#[derive(Deserialize)]
struct StoreResponse {
    id: String,
    status: String,
}

#[derive(Serialize)]
struct SearchRequest {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    streams: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude_source_agents: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct NamespacesResponse {
    namespaces: HashMap<String, String>,
}

// ── stream-stats mirror types (server: loomem_core::stream_stats) ──────────
// The CLI pulls no loomem-core, so the response shape is mirrored locally.
// Only fields shown in the table are declared; unknown fields are ignored.

#[derive(Deserialize)]
struct CliStreamStats {
    stream_id: String,
    health: CliHealth,
    retrieval: CliRetrieval,
    consolidation: CliConsolidation,
    distribution: CliDistribution,
    activity: CliActivity,
    extraction: CliExtraction,
    meta: CliMeta,
}

#[derive(Deserialize)]
struct CliHealth {
    memory_count: u64,
    deleted_count: u64,
    superseded_count: u64,
    l0_count: u64,
    l1_count: u64,
    oldest_chunk_at: Option<u64>,
    newest_chunk_at: Option<u64>,
    last_ingest_at: Option<u64>,
    last_search_at: Option<u64>,
}

#[derive(Deserialize)]
struct CliRetrieval {
    embedded_count: u64,
    embeddings_pending: u64,
    tantivy_indexed_count: Option<u64>,
    undecodable_count: u64,
}

#[derive(Deserialize)]
struct CliConsolidation {
    chunks_awaiting_consolidation: u64,
    min_chunks_to_consolidate: u64,
    runs_total_global: u64,
    last_at_global: Option<u64>,
}

#[derive(Deserialize)]
struct CliDistribution {
    fact_types: CliFactTypes,
    attribution: CliAttribution,
    trust_tier: CliTrust,
}

#[derive(Deserialize)]
struct CliFactTypes {
    preference_or_decision: u64,
    project_state: u64,
    fact: u64,
    event: u64,
    experience: u64,
    unclassified: u64,
}

#[derive(Deserialize)]
struct CliAttribution {
    user_authored: u64,
    assistant_authored: u64,
    unattributed: u64,
}

#[derive(Deserialize)]
struct CliTrust {
    a1: u64,
    a2: u64,
    b: u64,
}

#[derive(Deserialize)]
struct CliActivity {
    ingests: CliWindow,
    searches: CliWindow,
}

#[derive(Deserialize)]
struct CliWindow {
    last_24h: u64,
    last_7d: u64,
    last_30d: u64,
}

#[derive(Deserialize)]
struct CliExtraction {
    avg_facts_per_ingest_24h: f64,
    avg_facts_per_ingest_7d: f64,
    empty_extractions_24h: u64,
    empty_extractions_7d: u64,
    llm_failures_recent_global: CliLlmFailures,
}

#[derive(Deserialize)]
struct CliLlmFailures {
    extraction: u64,
    ner: u64,
    embedding: u64,
    consolidation: u64,
    extraction_empty: u64,
    window_secs: u64,
}

#[derive(Deserialize)]
struct CliMeta {
    generated_at: u64,
    event_log_enabled: bool,
    scanned_rows: u64,
}

/// Admin all-streams response (`?stream` omitted): per-stream map + `_total`.
#[derive(Deserialize)]
struct CliAllStreamStats {
    streams: std::collections::BTreeMap<String, CliStreamStats>,
    #[serde(rename = "_total")]
    total: CliStreamStats,
}

/// Render an optional count/timestamp, showing `n/a` when absent.
fn fmt_opt(v: Option<u64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |t| t.to_string())
}

/// Render one stream's stats as an aligned, section-grouped table. Numbers and
/// timestamps only — the server never sends chunk content.
fn print_stream_table(s: &CliStreamStats) {
    let h = &s.health;
    let r = &s.retrieval;
    let c = &s.consolidation;
    let ft = &s.distribution.fact_types;
    let at = &s.distribution.attribution;
    let tt = &s.distribution.trust_tier;
    let a = &s.activity;
    let ex = &s.extraction;
    let lf = &ex.llm_failures_recent_global;

    println!("── stream {} ──", s.stream_id);
    println!("  health");
    println!("    live memories       {}", h.memory_count);
    println!("    deleted             {}", h.deleted_count);
    println!("    superseded          {}", h.superseded_count);
    println!("    level L0 / L1       {} / {}", h.l0_count, h.l1_count);
    println!(
        "    oldest / newest     {} / {}",
        fmt_opt(h.oldest_chunk_at),
        fmt_opt(h.newest_chunk_at)
    );
    println!(
        "    last ingest/search  {} / {}",
        fmt_opt(h.last_ingest_at),
        fmt_opt(h.last_search_at)
    );
    println!("  retrieval");
    println!(
        "    embedded / pending  {} / {}",
        r.embedded_count, r.embeddings_pending
    );
    println!(
        "    bm25 indexed        {}",
        fmt_opt(r.tantivy_indexed_count)
    );
    println!("    undecodable         {}", r.undecodable_count);
    println!("  consolidation");
    println!(
        "    awaiting L0         {} (threshold {})",
        c.chunks_awaiting_consolidation, c.min_chunks_to_consolidate
    );
    println!(
        "    runs total (global) {} (last {})",
        c.runs_total_global,
        fmt_opt(c.last_at_global)
    );
    println!("  fact types");
    println!(
        "    pref/proj/fact      {} / {} / {}",
        ft.preference_or_decision, ft.project_state, ft.fact
    );
    println!(
        "    event/exp/unclass   {} / {} / {}",
        ft.event, ft.experience, ft.unclassified
    );
    println!(
        "  attribution         user {} / assistant {} / none {}",
        at.user_authored, at.assistant_authored, at.unattributed
    );
    println!(
        "  trust tier          a1 {} / a2 {} / b {}",
        tt.a1, tt.a2, tt.b
    );
    println!("  activity (24h/7d/30d)");
    println!(
        "    ingests             {} / {} / {}",
        a.ingests.last_24h, a.ingests.last_7d, a.ingests.last_30d
    );
    println!(
        "    searches            {} / {} / {}",
        a.searches.last_24h, a.searches.last_7d, a.searches.last_30d
    );
    println!("  extraction");
    println!(
        "    avg facts/ingest    {:.2} (24h) / {:.2} (7d)",
        ex.avg_facts_per_ingest_24h, ex.avg_facts_per_ingest_7d
    );
    println!(
        "    empty extractions   {} (24h) / {} (7d)",
        ex.empty_extractions_24h, ex.empty_extractions_7d
    );
    println!(
        "    llm failures ~{}m    extraction {} / ner {} / embedding {} / consolidation {} / empty {}",
        lf.window_secs / 60,
        lf.extraction,
        lf.ner,
        lf.embedding,
        lf.consolidation,
        lf.extraction_empty
    );
    println!(
        "  meta                event_log={} scanned_rows={} generated_at={}",
        s.meta.event_log_enabled, s.meta.scanned_rows, s.meta.generated_at
    );
}

/// Resolve namespace to stream ID(s) by querying the server's /v1/namespaces endpoint.
/// Returns (single_stream, multi_streams) — exactly one will be Some.
async fn resolve_namespace(
    client: &reqwest::Client,
    base_url: &str,
    ns: &str,
) -> Result<(Option<String>, Option<Vec<String>>)> {
    let url = format!("{}/v1/namespaces", base_url);
    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch namespaces from server")?;

    if !response.status().is_success() {
        // Fallback: try LOOMEM_NAMESPACES env var (format: "name=id,name=id,...")
        if let Ok(env_val) = std::env::var("LOOMEM_NAMESPACES") {
            let namespaces: HashMap<String, String> = env_val
                .split(',')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    match (parts.next(), parts.next()) {
                        (Some(k), Some(v)) => Some((k.trim().to_string(), v.trim().to_string())),
                        _ => None,
                    }
                })
                .collect();

            if ns == "all" {
                let all_streams: Vec<String> = namespaces.into_values().collect();
                return Ok((None, Some(all_streams)));
            }
            match namespaces.get(ns) {
                Some(stream_id) => return Ok((Some(stream_id.clone()), None)),
                None => anyhow::bail!(
                    "Unknown namespace '{}'. Available: {:?}",
                    ns,
                    namespaces.keys().collect::<Vec<_>>()
                ),
            }
        }
        anyhow::bail!(
            "Failed to fetch namespaces from server (status {})",
            response.status()
        );
    }

    let result: NamespacesResponse = response
        .json()
        .await
        .context("Failed to parse namespaces response")?;

    if ns == "all" {
        let all_streams: Vec<String> = result.namespaces.into_values().collect();
        return Ok((None, Some(all_streams)));
    }

    match result.namespaces.get(ns) {
        Some(stream_id) => Ok((Some(stream_id.clone()), None)),
        None => anyhow::bail!(
            "Unknown namespace '{}'. Available: {:?}",
            ns,
            result.namespaces.keys().collect::<Vec<_>>()
        ),
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
    took_ms: u64,
}

#[derive(Deserialize)]
struct SearchResult {
    id: String,
    content: String,
    score: f64,
    metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StatusResponse {
    status: String,
    uptime_secs: u64,
    config_summary: serde_json::Value,
}

#[derive(Serialize)]
struct DeleteRequest {
    id: String,
}

#[derive(Deserialize)]
struct DeleteResponse {
    status: String,
    id: String,
}

#[derive(Serialize)]
struct PurgeNamespaceRequest {
    stream: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    confirmed: bool,
}

#[derive(Deserialize)]
struct PurgeNamespaceResponse {
    status: String,
    stream: String,
    #[allow(dead_code)]
    dry_run: bool,
    deleted_count: usize,
    deleted_ids: Option<Vec<String>>,
}

/// Native stdio <-> HTTP MCP bridge. Mirrors what `npx mcp-remote --allow-http`
/// does, but as a single Rust binary so desktop clients need no Node toolchain.
///
/// loomem-server's `/mcp` transport is plain request/response JSON over POST
/// (no server-initiated SSE, no GET stream), so this is a faithful proxy:
///   stdin line (one JSON-RPC message) -> POST <url>/mcp -> stdout line (reply).
/// The `mcp-session-id` header returned on `initialize` is captured and echoed
/// on every subsequent request. Notifications (no `id`) get no reply.
async fn run_mcp_stdio(
    client: &reqwest::Client,
    base_url: &str,
    token: Option<String>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let endpoint = format!("{}/mcp", base_url.trim_end_matches('/'));
    let token = token.or_else(|| std::env::var("LOOMEM_AUTH_TOKEN").ok());
    let session_header = reqwest::header::HeaderName::from_static("mcp-session-id");
    let mut session_id: Option<String> = None;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Inspect the JSON-RPC envelope: requests carry a non-null `id` and expect
        // a reply; notifications don't. A batch (array) is assumed to expect one.
        let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("loomem mcp-stdio: skipping invalid JSON on stdin: {e}");
                continue;
            }
        };
        let req_id = parsed.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let expects_reply = parsed.is_array() || !req_id.is_null();

        let mut req = client
            .post(&endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .body(trimmed.to_owned());
        if let Some(ref sid) = session_id {
            req = req.header(session_header.clone(), sid.clone());
        }
        if let Some(ref t) = token {
            req = req.bearer_auth(t);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("loomem mcp-stdio: request to {endpoint} failed: {e}");
                if expects_reply {
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": { "code": -32000, "message": format!("loomem bridge: {e}") }
                    });
                    write_line(&mut stdout, &err.to_string()).await?;
                }
                continue;
            }
        };

        if let Some(sid) = resp
            .headers()
            .get(&session_header)
            .and_then(|v| v.to_str().ok())
        {
            session_id = Some(sid.to_string());
        }

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !expects_reply {
            continue; // notification: server returns an empty body, nothing to deliver
        }

        // Re-serialize compactly so each reply is exactly one newline-delimited line.
        let out = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v.to_string(),
            Err(_) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {
                    "code": -32000,
                    "message": format!("loomem bridge: HTTP {status}: {text}")
                }
            })
            .to_string(),
        };
        write_line(&mut stdout, &out).await?;
    }

    Ok(())
}

async fn write_line(stdout: &mut tokio::io::Stdout, s: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    stdout.write_all(s.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
        Commands::Store {
            content,
            stream,
            ns,
            user_id,
            app_id,
            level,
            persistent,
        } => {
            // Resolve --ns to stream ID (--ns takes precedence over --stream)
            let effective_stream = if let Some(ref ns_name) = ns {
                let (single, _) = resolve_namespace(&client, &cli.url, ns_name).await?;
                single
            } else {
                stream
            };

            let req = StoreRequest {
                content,
                stream: effective_stream,
                user_id,
                app_id,
                level,
                persistent: if persistent { Some(true) } else { None },
            };

            let url = format!("{}/v1/store", cli.url);
            let response = client
                .post(&url)
                .json(&req)
                .send()
                .await
                .context("Failed to send store request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Store request failed: {} - {}", status, body);
            }

            let result: StoreResponse = response
                .json()
                .await
                .context("Failed to parse store response")?;

            println!("✓ Stored: id={}, status={}", result.id, result.status);
        }

        Commands::Search {
            query,
            top_k,
            stream,
            ns,
            user_id,
            source,
            exclude_source,
        } => {
            // Resolve --ns to stream ID(s) (--ns takes precedence over --stream)
            let (effective_stream, effective_streams) = if let Some(ref ns_name) = ns {
                resolve_namespace(&client, &cli.url, ns_name).await?
            } else {
                (stream, None)
            };

            let req = SearchRequest {
                query,
                top_k: Some(top_k),
                stream: effective_stream,
                streams: effective_streams,
                user_id,
                source_agent: source,
                exclude_source_agents: exclude_source
                    .map(|s| s.split(',').map(|a| a.trim().to_string()).collect()),
            };

            let url = format!("{}/v1/search", cli.url);
            let response = client
                .post(&url)
                .json(&req)
                .send()
                .await
                .context("Failed to send search request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Search request failed: {} - {}", status, body);
            }

            let result: SearchResponse = response
                .json()
                .await
                .context("Failed to parse search response")?;

            println!(
                "Found {} results in {}ms:",
                result.results.len(),
                result.took_ms
            );
            for (i, r) in result.results.iter().enumerate() {
                println!("\n[{}] score={:.4} id={}", i + 1, r.score, r.id);
                println!("    {}", r.content);
                if let Some(meta) = &r.metadata {
                    println!("    metadata: {}", serde_json::to_string_pretty(meta)?);
                }
            }
        }

        Commands::Delete { id, ns } => {
            // If --ns is provided, use the new brief-compliant API endpoint
            // Otherwise, use the legacy /v1/delete endpoint
            if let Some(ns_name) = ns {
                // Use DELETE /api/memories/:id with ?ns=<namespace>
                let url = format!("{}/api/memories/{}?ns={}", cli.url, id, ns_name);
                let response = client
                    .delete(&url)
                    .send()
                    .await
                    .context("Failed to send delete request")?;

                if response.status().is_success() {
                    #[derive(Deserialize)]
                    struct ApiDeleteResponse {
                        #[allow(dead_code)]
                        deleted: bool,
                        id: String,
                    }
                    let result: ApiDeleteResponse = response
                        .json()
                        .await
                        .context("Failed to parse delete response")?;
                    println!("✓ Deleted: id={}", result.id);
                } else if response.status() == 404 {
                    println!("⚠ Not found: id={}", id);
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("Delete request failed: {} - {}", status, body);
                }
            } else {
                // Use legacy POST /v1/delete endpoint
                let req = DeleteRequest { id: id.clone() };

                let url = format!("{}/v1/delete", cli.url);
                let response = client
                    .post(&url)
                    .json(&req)
                    .send()
                    .await
                    .context("Failed to send delete request")?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("Delete request failed: {} - {}", status, body);
                }

                let result: DeleteResponse = response
                    .json()
                    .await
                    .context("Failed to parse delete response")?;

                if result.status == "deleted" {
                    println!("✓ Deleted: id={}", result.id);
                } else if result.status == "not_found" {
                    println!("⚠ Not found: id={}", result.id);
                } else {
                    println!("Status: {} for id={}", result.status, result.id);
                }
            }
        }

        Commands::PurgeNs {
            stream,
            ns,
            dry_run,
            confirmed,
        } => {
            // Resolve --ns to stream ID (--ns takes precedence over --stream)
            let effective_stream = if let Some(ref ns_name) = ns {
                let (single, _) = resolve_namespace(&client, &cli.url, ns_name).await?;
                single.ok_or_else(|| anyhow::anyhow!("Cannot purge 'all' namespace"))?
            } else {
                stream.ok_or_else(|| anyhow::anyhow!("Either --stream or --ns must be provided"))?
            };

            let req = PurgeNamespaceRequest {
                stream: effective_stream.clone(),
                dry_run,
                confirmed,
            };

            let url = format!("{}/v1/purge-namespace", cli.url);
            let response = client
                .post(&url)
                .json(&req)
                .send()
                .await
                .context("Failed to send purge request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Purge request failed: {} - {}", status, body);
            }

            let result: PurgeNamespaceResponse = response
                .json()
                .await
                .context("Failed to parse purge response")?;

            match result.status.as_str() {
                "confirmation_required" => {
                    println!("⚠ Confirmation required!");
                    println!(
                        "To purge namespace '{}', run with --confirmed flag",
                        result.stream
                    );
                }
                "dry_run" => {
                    println!(
                        "🔍 Dry run: would delete {} memories from stream '{}'",
                        result.deleted_count, result.stream
                    );
                    if let Some(ids) = result.deleted_ids {
                        if !ids.is_empty() {
                            println!("\nIDs that would be deleted:");
                            for (i, id) in ids.iter().enumerate().take(10) {
                                println!("  {}: {}", i + 1, id);
                            }
                            if ids.len() > 10 {
                                println!("  ... and {} more", ids.len() - 10);
                            }
                        }
                    }
                }
                "purged" => {
                    println!(
                        "✓ Purged {} memories from stream '{}'",
                        result.deleted_count, result.stream
                    );
                }
                _ => {
                    println!(
                        "Status: {} for stream '{}' ({} items)",
                        result.status, result.stream, result.deleted_count
                    );
                }
            }
        }

        Commands::IngestConversation { stream, ns } => {
            let effective_stream = if let Some(ref ns_name) = ns {
                let (single, _) = resolve_namespace(&client, &cli.url, ns_name).await?;
                single
            } else {
                stream
            };

            // Read conversation from stdin
            use std::io::Read;
            let mut content = String::new();
            std::io::stdin()
                .read_to_string(&mut content)
                .context("Failed to read conversation from stdin")?;

            if content.trim().is_empty() {
                anyhow::bail!("No conversation text provided. Pipe text via stdin.");
            }

            let url = format!("{}/v1/ingest-conversation", cli.url);
            let response = client
                .post(&url)
                .json(&serde_json::json!({
                    "content": content,
                    "stream": effective_stream,
                }))
                .send()
                .await
                .context("Failed to send ingest-conversation request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Ingest request failed: {} - {}", status, body);
            }

            let result: serde_json::Value = response
                .json()
                .await
                .context("Failed to parse ingest response")?;

            println!("Extracted: {} memories", result["extracted"]);
            println!("Stored: {}", result["stored"]);
            println!("Skipped (dedup): {}", result["skipped_dedup"]);
        }

        Commands::Profile {
            stream,
            ns,
            refresh,
            format,
        } => {
            let effective_stream = if let Some(ref ns_name) = ns {
                let (single, _) = resolve_namespace(&client, &cli.url, ns_name).await?;
                single
            } else {
                stream
            };

            let mut url = format!("{}/v1/profile?format={}", cli.url, format);
            if let Some(ref s) = effective_stream {
                url.push_str(&std::format!("&stream={}", s));
            }
            if refresh {
                url.push_str("&refresh=true");
            }

            let response = client
                .get(&url)
                .send()
                .await
                .context("Failed to send profile request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Profile request failed: {} - {}", status, body);
            }

            let result: serde_json::Value = response
                .json()
                .await
                .context("Failed to parse profile response")?;

            if format == "markdown" || format == "md" {
                println!("{}", result["content"].as_str().unwrap_or(""));
            } else {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
        }

        Commands::ContextPack {
            query,
            stream,
            ns,
            budget,
            sections,
        } => {
            let effective_stream = if let Some(ref ns_name) = ns {
                let (single, _) = resolve_namespace(&client, &cli.url, ns_name).await?;
                single
            } else {
                stream
            };

            let sections_vec: Vec<String> =
                sections.split(',').map(|s| s.trim().to_string()).collect();

            let url = format!("{}/v1/context-pack", cli.url);
            let response = client
                .post(&url)
                .json(&serde_json::json!({
                    "query": query,
                    "stream": effective_stream,
                    "budget_tokens": budget,
                    "sections": sections_vec,
                    "format": "markdown",
                }))
                .send()
                .await
                .context("Failed to send context-pack request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Context-pack request failed: {} - {}", status, body);
            }

            let result: serde_json::Value = response
                .json()
                .await
                .context("Failed to parse context-pack response")?;

            // Print the packed context directly (ready for prompt injection)
            println!("{}", result["context"].as_str().unwrap_or(""));
            eprintln!(
                "\n---\nTokens: {}, Coverage: {:.0}%, Sources: {}",
                result["token_count"],
                result["coverage_score"].as_f64().unwrap_or(0.0) * 100.0,
                result["sources"].as_array().map(|a| a.len()).unwrap_or(0)
            );
        }

        Commands::History { id, limit } => {
            let url = format!("{}/v1/memory-chain/{}?limit={}", cli.url, id, limit);
            let response = client
                .get(&url)
                .send()
                .await
                .context("Failed to send history request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("History request failed: {} - {}", status, body);
            }

            let result: serde_json::Value = response
                .json()
                .await
                .context("Failed to parse history response")?;

            let chain = result["chain"].as_array();
            if let Some(chain) = chain {
                println!("Version chain for {} ({} versions):", id, chain.len());
                for (i, entry) in chain.iter().enumerate() {
                    let marker = if entry["is_latest"].as_bool() == Some(true) {
                        " [CURRENT]"
                    } else {
                        ""
                    };
                    println!("\n  v{}{}", entry["version"], marker);
                    println!("  id: {}", entry["id"].as_str().unwrap_or("?"));
                    println!("  content: {}", entry["content"].as_str().unwrap_or("?"));
                    if i < chain.len() - 1 {
                        println!("    |");
                        println!("    v");
                    }
                }
            } else {
                println!("No version chain found for {}", id);
            }
        }

        Commands::McpStdio { token } => {
            run_mcp_stdio(&client, &cli.url, token).await?;
        }

        Commands::Status => {
            let url = format!("{}/v1/status", cli.url);
            let response = client
                .get(&url)
                .send()
                .await
                .context("Failed to send status request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Status request failed: {} - {}", status, body);
            }

            let result: StatusResponse = response
                .json()
                .await
                .context("Failed to parse status response")?;

            println!("Status: {}", result.status);
            println!("Uptime: {}s", result.uptime_secs);
            println!(
                "Config: {}",
                serde_json::to_string_pretty(&result.config_summary)?
            );
        }

        Commands::Stats {
            stream,
            admin,
            token,
        } => {
            let token = token.or_else(|| std::env::var("LOOMEM_AUTH_TOKEN").ok());
            if stream.is_some() && !admin {
                eprintln!("note: --stream is ignored without --admin; showing your own stream");
            }
            let path = if admin {
                "/v1/admin/stream-stats"
            } else {
                "/v1/my/stream-stats"
            };
            let mut url = reqwest::Url::parse(&format!("{}{}", cli.url, path))
                .context("invalid server url")?;
            if admin {
                if let Some(s) = &stream {
                    // append_pair percent-encodes, so stream ids with reserved
                    // chars (& # ? = space) reach the server as one value.
                    url.query_pairs_mut().append_pair("stream", s);
                }
            }

            let mut req = client.get(url);
            if let Some(t) = &token {
                req = req.bearer_auth(t);
            }
            let response = req.send().await.context("Failed to send stats request")?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Stats request failed: {} - {}", status, body);
            }

            let value: serde_json::Value = response
                .json()
                .await
                .context("Failed to parse stats response")?;
            if value.get("streams").is_some() {
                // Admin all-streams shape: per-stream map + _total.
                let all: CliAllStreamStats =
                    serde_json::from_value(value).context("Failed to decode all-streams stats")?;
                for s in all.streams.values() {
                    print_stream_table(s);
                    println!();
                }
                print_stream_table(&all.total);
            } else {
                let s: CliStreamStats =
                    serde_json::from_value(value).context("Failed to decode stream stats")?;
                print_stream_table(&s);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stats --admin --stream X` parses into the right variant/fields.
    #[test]
    fn stats_command_parses_flags() {
        let cli = Cli::try_parse_from(["loomem-cli", "stats", "--admin", "--stream", "s1"])
            .expect("stats args parse");
        match cli.command {
            Commands::Stats {
                stream,
                admin,
                token,
            } => {
                assert_eq!(stream.as_deref(), Some("s1"));
                assert!(admin);
                assert!(token.is_none());
            }
            _ => panic!("expected Stats command"),
        }
    }

    /// A representative server `StreamStats` JSON (snake_case, hierarchical)
    /// decodes into the CLI mirror types — guards against schema drift between
    /// `loomem_core::stream_stats` and this file.
    #[test]
    fn stream_stats_json_decodes_into_mirror() {
        let json = r#"{
            "stream_id": "s1",
            "health": {"memory_count": 3, "deleted_count": 1, "superseded_count": 2,
                "l0_count": 2, "l1_count": 1, "oldest_chunk_at": 100, "newest_chunk_at": 900,
                "last_ingest_at": null, "last_search_at": 900},
            "retrieval": {"embedded_count": 2, "embeddings_pending": 1,
                "tantivy_indexed_count": 3, "undecodable_count": 0},
            "consolidation": {"chunks_awaiting_consolidation": 2, "min_chunks_to_consolidate": 3,
                "runs_total_global": 5, "last_at_global": 800},
            "distribution": {
                "fact_types": {"preference_or_decision": 1, "project_state": 0, "fact": 1,
                    "event": 1, "experience": 0, "unclassified": 0},
                "attribution": {"user_authored": 2, "assistant_authored": 1, "unattributed": 0},
                "trust_tier": {"a1": 2, "a2": 1, "b": 0}
            },
            "activity": {"ingests": {"last_24h": 1, "last_7d": 2, "last_30d": 3},
                "searches": {"last_24h": 0, "last_7d": 1, "last_30d": 1}},
            "extraction": {"avg_facts_per_ingest_24h": 1.5, "avg_facts_per_ingest_7d": 2.0,
                "empty_extractions_24h": 0, "empty_extractions_7d": 1,
                "llm_failures_recent_global": {"extraction": 0, "ner": 0, "embedding": 0,
                    "consolidation": 0, "extraction_empty": 0, "window_secs": 3600}},
            "meta": {"generated_at": 1000, "event_log_enabled": true, "scanned_rows": 6}
        }"#;
        let s: CliStreamStats = serde_json::from_str(json).expect("decode StreamStats");
        assert_eq!(s.stream_id, "s1");
        assert_eq!(s.health.memory_count, 3);
        assert_eq!(s.retrieval.tantivy_indexed_count, Some(3));
        assert_eq!(s.distribution.trust_tier.a1, 2);
        assert_eq!(s.activity.ingests.last_30d, 3);
        assert!((s.extraction.avg_facts_per_ingest_24h - 1.5).abs() < 1e-9);
        // Smoke: rendering must not panic.
        print_stream_table(&s);
    }
}
