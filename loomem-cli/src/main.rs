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

    #[arg(long, default_value = "http://localhost:3030")]
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
    }

    Ok(())
}
