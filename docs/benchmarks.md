# Loomem benchmarks

Loomem benchmarks
Loomem scores 75.0% (375 / 500) on LongMemEval-S, a public long-term-memory question-answering benchmark — running fully self-hosted, with on-device embeddings and no external database. This page documents the exact setup so the result is reproducible rather than a headline number.

What LongMemEval measures
LongMemEval is a public benchmark for long-term memory in chat assistants: across long, multi-session histories it tests information extraction, multi-session reasoning, temporal reasoning, knowledge updates, and abstention. We evaluate on LongMemEval-S (cleaned) — the xiaowu0162/longmemeval-cleaned variant — so the questions match, one-to-one, the set used in published Mem0 and Letta write-ups.

The result

[see table on the page]

Configuration (for reproducibility)

[see table on the page]

How to read this

Self-run, single configuration. This is our own run, not a third-party leaderboard. The config above is everything needed to reproduce it against the public dataset.
The reader model matters. LongMemEval scores blend retrieval quality with the reader model and its prompt. We report the reader/judge (GPT-4.1) so the comparison is apples-to-apples; a terse reader prompt in particular can penalise preference-style questions where the answer must be applied, not just stated.
Embeddings were fully local. The 75.0% is with on-device e5-small embeddings, so it reflects the default offline setup rather than a cloud-embedding best case.
LongMemEval is saturating. Top systems now cluster near the ceiling, so we are also evaluating on newer suites (LongMemEval-V2, MemoryArena) and will publish those as they stabilise.

Loomem is open source (Apache-2.0); see how it compares to other memory layers on the comparison page.
Benchmark run 2026-06-26. LongMemEval: Wu et al., “LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory”. Dataset: xiaowu0162/longmemeval-cleaned.