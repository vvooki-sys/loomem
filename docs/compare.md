# Loomem vs Mem0, Zep, Letta, and cognee

Loomem vs Mem0, Zep, Letta, and cognee
Loomem is the open-source context layer for AI agents that runs as a single Rust binary with no external services. The other open-source memory layers are strong tools, but every one of them needs at least one external datastore to run. This page lays them side by side on the axes that matter when you self-host: runtime dependencies, interface, licensing, and portability. Facts are current as of July 2026 — verify against each project's own docs before quoting.

At a glance

[see table on the page]

The projects, briefly
Mem0
Apache-2.0, self-hostable as a library or a server. Mem0 is not itself a database — it orchestrates a vector store (Qdrant by default) plus Postgres for history, and supports 20+ vector backends. Great if you want broad framework and vector-store integrations and are happy running that infrastructure.
Zep / Graphiti
Zep is built on Graphiti, a temporal knowledge-graph engine with bi-temporal edges (valid-from / valid-until). The Graphiti core is open source; the Zep Community Edition has been deprecated, so self-hosting means running Graphiti directly against your own graph database (Neo4j, FalkorDB, or Kuzu). The right pick if graph-first temporal reasoning at team scale is the point.
Letta (MemGPT)
Apache-2.0, the framework formerly known as MemGPT. It gives agents an OS-inspired memory hierarchy (core / recall / archival) that the model edits in its own loop, and stores everything in Postgres + pgvector. Strong when you want self-editing stateful agents rather than a standalone context store.
cognee
Apache-2.0 memory platform that combines vector embeddings, graph reasoning, and ontology generation for GraphRAG over documents. Self-hostable via Docker or on-prem, with a newer Rust core for lighter deployments. Fits ontology-heavy, document-centric knowledge work.

Where Loomem fits
Choose Loomem when you want one person's context to follow them across every model and tool with zero ops: one Rust binary, no external database to run or secure, MCP-native so Claude, ChatGPT, Codex, or Cursor connect directly, and local ONNX embeddings so your first entry needs no internet. Retrieval is a weighted hybrid of BM25 (Tantivy) + vector + entity-graph signals, facts are bitemporal, and a background consolidation loop keeps context sharp instead of bloated.
Choose another when your need is a different shape: Mem0 for broad vector-store / framework breadth, Zep/Graphiti for team-scale graph-temporal reasoning, Letta for self-editing agent memory, cognee for ontology/GraphRAG over large document sets. Loomem is deliberately memory-only and single-user — it is not an agent framework or a multi-tenant platform.

See the numbers on the benchmarks page, or get started from the home page.
Sources: project documentation and repositories for Mem0, Zep / Graphiti, Letta, and cognee (accessed July 2026).