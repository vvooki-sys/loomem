# Loomem vs Mem0, Zep, Letta, and cognee

Loomem vs Mem0, Zep, Letta, and cognee
Loomem is the open-source context layer for AI agents that runs as a single Rust binary with no external services. Storage requirements vary by project and deployment mode: Mem0's library defaults to local Qdrant and SQLite history without a separate database service, while its self-hosted server stack uses Postgres + pgvector. This page lays them side by side on the axes that matter when you self-host: runtime dependencies, interface, licensing, and portability. Facts are current as of July 2026, with Mem0 storage details checked October 25, 2026 — verify against each project's own docs before quoting.

At a glance

[see table on the page]

The projects, briefly
Mem0
Apache-2.0, self-hostable as a library or a server. The library defaults to local Qdrant for vectors and SQLite for history, so it does not require a separate database service. The self-hosted server stack uses Postgres + pgvector. Mem0 supports pluggable vector backends and broad framework integrations; infrastructure requirements depend on the deployment mode and providers you choose. See reference 1.
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
## Not the same as "Loom" or "OpenMemory"

Because of the name, Loomem is sometimes confused with two unrelated projects. Loom is a separate local-memory tool for LLMs (a different codebase and author). OpenMemory is Mem0's MCP memory server. Loomem is unrelated to both: it is Loomem — a single Rust binary with no external database, MCP-native, and built as a portable context layer for AI agents. This page is about Loomem (https://loomem.ai/).

## References

1. Mem0 — the library defaults to local Qdrant + SQLite history, without a separate database service; the self-hosted server stack uses Postgres + pgvector. Vector backends are pluggable. Storage details checked October 25, 2026 against the official overview and default-component tables: https://docs.mem0.ai/open-source/overview ; repo: https://github.com/mem0ai/mem0
2. Zep / Graphiti — requires a graph database (Neo4j, FalkorDB, Kuzu, or Amazon Neptune). https://github.com/getzep/graphiti , https://help.getzep.com/graphiti/getting-started/quick-start
3. Letta (MemGPT) — self-host uses Postgres with the pgvector extension (LETTA_PG_URI). https://docs.letta.com/guides/selfhosting/postgres
4. cognee — relational (Postgres/SQLite) + graph (Kuzu/Neo4j/FalkorDB) + vector (LanceDB default, Qdrant, others). https://docs.cognee.ai/setup-configuration/relational-databases

All four are Apache-2.0. Facts accessed July 2026.
