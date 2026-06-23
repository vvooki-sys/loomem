use serde_json::{json, Value};

pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_store",
            "description": "Store a single fact, decision, or preference in long-term memory.\n\nWhen to use vs siblings: use memory_store for one explicit fact the user just stated (name, preference, decision). Use memory_ingest instead when you have a full conversation transcript — it extracts multiple typed facts at once with contradiction detection. Do not use memory_store to dump raw conversation text.\n\nReturns: plain text confirmation — \"Stored: \\\"<first 80 chars>...\\\" (id: <uuid>)\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The information to remember. Be specific and self-contained. Include context, dates, and reasoning when available."
                    },
                    "source": {
                        "type": "string",
                        "description": "Origin of this memory (e.g. 'user-stated', 'inferred', 'conversation'). Default: 'mcp'."
                    },
                    "subject": {
                        "type": "string",
                        "description": "Who or what this memory is about (e.g. user's name, project name). Default: 'user'."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Optional key-value metadata to attach to the memory."
                    },
                    "relations": {
                        "type": "array",
                        "description": "Optional caller-declared graph relations. Use this when you already know a relation between two entities and want it in the knowledge graph deterministically and synchronously, instead of relying on dictionary coverage or async LLM extraction. Each edge is deduplicated; 'relation' is mapped to a known relation type (an unrecognized value becomes 'related_to'). Names converge onto existing graph nodes via dictionary aliases — names not in the dictionary become their own node.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": { "type": "string", "description": "Source entity name." },
                                "relation": { "type": "string", "description": "Relation type (mapped to a known type; unknown -> 'related_to')." },
                                "object": { "type": "string", "description": "Target entity name." },
                                "subject_type": { "type": "string", "description": "Optional entity-type hint for subject (e.g. 'Person', 'Project'). Default 'Concept'." },
                                "object_type": { "type": "string", "description": "Optional entity-type hint for object. Default 'Concept'." }
                            },
                            "required": ["subject", "relation", "object"]
                        }
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "memory_search",
            "description": "Search long-term memory using hybrid BM25 + vector + knowledge-graph retrieval with reranking.\n\nWhen to use vs siblings: use memory_search when you need specific facts matching a query. Use memory_context instead at the start of a session or complex task to get a single pre-formatted context block. Use memory_graph when you need to explore entity relationships rather than retrieve text facts. Use memory_associate to surface non-obvious connections. The response includes a chunk_id for each hit (`(id: <uuid>)`), usable directly with memory_history, memory_delete, or memory_feedback.\n\nReturns: numbered plain-text list — each entry: \"N. [YYYY-MM-DD] <content> (score: X.XX) (id: <uuid>)\". Items superseding older versions are annotated \"[UPDATED — supersedes older version]\". Returns \"No relevant memories found.\" when empty. Note: supports time_filter (ISO date lower bound), valid_at (unix timestamp for bitemporal time-travel), and include_superseded to see historical versions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language search query. Be specific — include names, dates, topics."
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Number of results to return (default: 5, max: 20)."
                    },
                    "time_filter": {
                        "type": "string",
                        "description": "Optional ISO date. Only return memories from after this date."
                    },
                    "valid_at": {
                        "type": "integer",
                        "description": "Optional bitemporal time-travel: unix timestamp (seconds). Returns only chunks whose [valid_from, valid_until] interval covers this point in time."
                    },
                    "include_superseded": {
                        "type": "boolean",
                        "description": "Optional. If true, include older/superseded versions in results. Default false (latest-only)."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "memory_context",
            "description": "Build a token-budgeted context block from memory, formatted as markdown ready for prompt injection.\n\nWhen to use vs siblings: use memory_context at the start of a complex task or new session to load relevant background without iterating over individual chunks. Use memory_search instead when you want per-chunk scoring, time-filtered queries, or to surface superseded versions. memory_context is faster and returns a single formatted block; memory_search gives per-chunk scores. Note: memory_context returns a single formatted block; for per-chunk ids, use memory_search.\n\nReturns: plain markdown text — a single formatted block combining profile summary, relevant memories, and recent memories (controlled by the sections parameter). Returns an empty string or minimal block when no relevant memories exist.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What context do you need? Describe the task or topic."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Maximum tokens for the context block (default: 2000, max: 8000)."
                    },
                    "sections": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Which sections to include: 'profile', 'relevant', 'recent'. Default: all."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "memory_profile",
            "description": "Retrieve the user's synthesized profile — a summary of identity, preferences, work style, and key facts, automatically maintained from stored memories.\n\nWhen to use vs siblings: use memory_profile to get a high-level user summary at session start. Use memory_search or memory_context for specific facts or task-scoped context. memory_profile is coarser but stable; it does not return individual chunk_id values.\n\nReturns: formatted profile text (markdown by default, JSON when format='json') covering name, preferences, frequent topics, and key biographical facts. Returns an error message if profile generation fails.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "format": {
                        "type": "string",
                        "enum": ["json", "markdown"],
                        "description": "Output format. Default: 'markdown'."
                    },
                    "refresh": {
                        "type": "boolean",
                        "description": "Force regenerate profile from memories, ignoring cache. Default: false."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "memory_status",
            "description": "Return engine health metrics for the caller's stream.\n\nWhen to use vs siblings: use memory_status to check whether the stream is reachable and to see chunk counts before deciding whether to run memory_reflect. Not a retrieval tool — returns no memory content.\n\nReturns: plain text with fields: engine status (\"ok\"), stream_id, memory count for the stream, global embedding count, associator status (active/enabled/disabled with cluster count), and log-loss counters — plus undecodable-chunk count from the last full scan and recent LLM-failure counters (extraction/ner/embedding/consolidation, 1h window).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "memory_reflect",
            "description": "Analyze memory quality and return a health report with actionable cleanup suggestions.\n\nWhen to use vs siblings: use memory_reflect periodically or when memory quality seems poor (stale facts, too many raw transcripts). It does not modify anything — call memory_dream to actually consolidate. Do not call memory_reflect on every turn; it scans up to 200 chunks and is slow.\n\nReturns: markdown quality report with fields: Score (0-100%), total chunks analyzed, breakdown counts (structured facts, subject tags, low-confidence, raw transcripts, very short chunks), by-level distribution, by-source distribution, by-fact-type distribution, and a Suggestions section with recommended actions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_chunks": {
                        "type": "integer",
                        "description": "How many chunks to analyze (default: 200). Higher = more thorough but slower."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "memory_ingest",
            "description": "Ingest a full conversation transcript and extract structured typed facts (preferences, decisions, project states, biographical facts) via LLM extraction.\n\nWhen to use vs siblings: use memory_ingest at the end of a conversation to process the full transcript into multiple searchable facts. Use memory_store instead for a single explicit user-stated fact during the session. memory_ingest is more powerful — it extracts multiple facts, annotates them temporally, and detects contradictions with existing memories. When extraction is disabled in config, falls back to storing the raw content.\n\nReturns: plain text — \"Extracted N facts from conversation, stored N, skipped N.\" when extraction is enabled; \"Stored raw transcript (id: <uuid>). Knowledge extraction disabled.\" otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Full conversation text to extract facts from."
                    },
                    "conversation_date": {
                        "type": "string",
                        "description": "ISO date of the conversation (e.g. '2026-04-02'). Used to resolve relative dates like 'yesterday'."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "memory_dream",
            "description": "Trigger consolidation: merge related memories, resolve contradictions, and produce synthesized up-to-date facts.\n\nWhen to use vs siblings: use memory_dream after a long session or when memory_reflect reports low quality or many contradictions. It is a write operation (Admin-only on shared streams). Do not call memory_dream on every turn — it runs an LLM pipeline and incurs cost. Use memory_reflect first to diagnose whether consolidation is needed.\n\nReturns: plain text summary — \"Dream consolidation complete:\" followed by fields: chunks processed, subject groups, facts merged, contradictions resolved, cost in USD. Includes a warning if the cost cap was reached.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "memory_history",
            "description": "Show the full version chain of a memory — how a fact evolved over time, with each version's content and ID.\n\nWhen to use vs siblings: use memory_history when you have a specific chunk_id and need to trace how that fact changed — e.g., \"what tool did the user use before switching to X?\". Not a search tool; requires a known chunk_id. Obtain chunk_ids from the id field returned by memory_store (\"Stored: \\\"...\\\" (id: <uuid>)\") or memory_ingest, or the `(id: <uuid>)` on each memory_search hit. Use memory_search with include_superseded=true to retrieve historical versions without a known ID.\n\nReturns: plain text version chain — header \"Version history (N versions):\" followed by entries in the format \"vN [CURRENT|superseded]: <content>\\n  id: <uuid>\" with \"  ↓\" separators between versions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_id": {
                        "type": "string",
                        "description": "ID of the memory chunk to trace history for."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum versions to return (default: 20)."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["chunk_id"]
            }
        }),
        json!({
            "name": "memory_graph",
            "description": "Explore the knowledge graph — look up an entity by name and return its type, aliases, linked chunk count, and connections to other entities.\n\nWhen to use vs siblings: use memory_graph when the question is relational — \"who does user X work with?\", \"what projects is technology Y used in?\" Use memory_search instead for full-text retrieval of facts. memory_graph returns structural connections, not ranked text chunks.\n\nReturns: plain text — entity name and type, optional aliases, linked chunk count, then a \"Connections:\" list of \"  → <entity> (<relation_type>) [<entity_type>]\" lines. Returns \"Entity '<name>' not found in knowledge graph.\" when the entity does not exist.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entity": {
                        "type": "string",
                        "description": "Entity name to look up (person, project, technology)."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["entity"]
            }
        }),
        json!({
            "name": "memory_namespaces",
            "description": "List all memory streams the caller can access, with stream_id, display name, type, access level, and which is the default.\n\nWhen to use vs siblings: call memory_namespaces once at session start to discover available streams before using any stream= parameter. Required before targeting a non-default stream — passing an unknown stream_id to other tools returns an access-denied error. Not a retrieval tool; returns no memory content.\n\nReturns: plain text list — \"Your namespaces:\\n\\n\" followed by lines: \"- <display_name> (stream_id: <id>, type: <user_private|organization_shared|project>, access: <owner|admin|write|read>[, default])\" for each accessible stream. Project streams show their project name; private streams show a generic label (never a person/email).",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }),
        json!({
            "name": "memory_delete",
            "description": "Permanently delete a specific memory chunk by its chunk_id.\n\nWhen to use vs siblings: use memory_delete only when the user explicitly asks to forget a specific memory. Obtain the chunk_id from the id field returned by memory_store (\"Stored: \\\"...\\\" (id: <uuid>)\"), memory_ingest, or the `(id: <uuid>)` on each memory_search hit. On shared streams, memory_delete requires Admin role. Deletion is hard (removes from store, search index, embeddings, and graph); it cannot be undone.\n\nReturns: plain text — \"Deleted memory <id>.\" on success, \"Memory <id> not found.\" if the chunk does not exist, or \"Deleted memory <id> (partial: failed steps = [<names>]; retry recommended)\" if some downstream steps failed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_id": {
                        "type": "string",
                        "description": "ID of the memory chunk to delete."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["chunk_id"]
            }
        }),
        json!({
            "name": "memory_associate",
            "description": "Surface serendipitous associations — memories that are surprisingly relevant but not obvious — using graph walks, temporal co-occurrence, and semantic adjacent-possible mechanisms.\n\nWhen to use vs siblings: use memory_associate when looking for unexpected connections or creative links across topics (e.g., \"what past projects relate to this new idea?\"). Use memory_search for direct fact retrieval. memory_associate scores lower-relevance, higher-novelty results; if no clustering has run yet, results may be empty — call memory_dream first. Note: write operation on shared streams (requires Writer role or above).\n\nReturns: plain text numbered list — \"Found N associations (took Nms):\\n\" followed by entries: \"N. [<mechanism>] (score: X.XXX) <content>\" with an optional explanation line. Returns a hint to run memory_dream if results are empty.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Topic or question to find associations for."
                    },
                    "mechanisms": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Which mechanisms to use: 'graph', 'temporal', 'adjacent'. Default: all."
                    },
                    "count": {
                        "type": "integer",
                        "description": "Maximum associations to return (default: 3, max: 10)."
                    },
                    "stream": {
                        "type": "string",
                        "description": "Optional stream_id. Omit to use your default stream. Call memory_namespaces to discover accessible streams."
                    }
                },
                "required": ["query"]
            }
        }),
        // ── Cycle /113: Feedback tool ─────────────────────────────────
        json!({
            "name": "memory_feedback",
            "description": "Rate the usefulness of one memory chunk, providing reinforcement signal to the ranking system.\n\nWhen to use vs siblings: call memory_feedback after completing a task where a specific memory chunk was helpful or harmful. Requires a chunk_id — obtain it from the `(id: <uuid>)` on each memory_search hit, or from the id field returned by memory_store (\"Stored: \\\"...\\\" (id: <uuid>)\") or memory_ingest (memory_context does not emit chunk_ids). Only rate chunks where the usefulness is clear (very helpful or actively misleading). Do not use this as a general memory write tool; use memory_store or memory_ingest for storing facts.\n\nUSEFULNESS SCALE (pick one integer):\n  4 — Crucial. Without this chunk you could not have completed the task.\n  3 — Important. Significantly reduced effort or resolved ambiguity.\n  2 — Helpful. Useful context, but not critical.\n  1 — Marginal. Minor context; manageable without it.\n  0 — Not useful. Did not contribute.\n\nHARMFUL FLAG: set to true only when the chunk contained incorrect or misleading information that led you in the wrong direction. Not for merely unhelpful chunks (use usefulness=0, harmful=false).\n\nReturns: JSON — {ok: true, accepted: 1, rejected: []} on success; {ok: true, accepted: 0, rejected: [{chunk_id, reason}]} when the rating is rejected (e.g. chunk not found, validation error).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_id": {
                        "type": "string",
                        "description": "UUID of the chunk being rated. Must be from a memory_context or memory_search result you retrieved earlier in this conversation."
                    },
                    "usefulness": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 4,
                        "description": "Graded usefulness 0-4 per scale in tool description. Required."
                    },
                    "harmful": {
                        "type": "boolean",
                        "description": "Set true ONLY when chunk contained wrong/misleading info. Default false. Requires non-empty justification when true."
                    },
                    "justification": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 500,
                        "description": "Required. 1-500 chars declarative explanation of the rating."
                    },
                    "model_version": {
                        "type": "string",
                        "description": "Self-reported model identifier (e.g. 'claude-opus-4-7', 'claude-sonnet-4-6'). Required."
                    },
                    "prompt_version": {
                        "type": "string",
                        "description": "Self-reported prompt/instruction version. Defaults to 'loomem-feedback-v1' if omitted."
                    },
                    "trajectory_id": {
                        "type": "string",
                        "description": "Optional task/trajectory grouping UUID. Use the same trajectory_id across multiple feedback calls for the same task."
                    }
                },
                "required": ["chunk_id", "usefulness", "harmful", "justification", "model_version"]
            }
        }),
    ]
}
