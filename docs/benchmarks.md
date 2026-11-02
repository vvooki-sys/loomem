# Loomem benchmarks

**Loomem scored 85.4% (427 / 500) on LongMemEval-S (cleaned)** in a complete diagnostic run on 29 October 2026. The run used the self-hosted Loomem memory engine with local embeddings and Kimi `k3-256k` as both reader and judge. This is a modified strict-evaluator result, not an official leaderboard score or a direct comparison with other vendors.

A machine-readable summary is available in [`benchmarks/longmemeval-2026-10-29-summary.json`](benchmarks/longmemeval-2026-10-29-summary.json).

## What LongMemEval measures

LongMemEval is a public benchmark for long-term memory in chat assistants. Across long, multi-session histories it tests information extraction, multi-session reasoning, temporal reasoning, knowledge updates and abstention. We evaluated all 500 questions from LongMemEval-S (cleaned), using the `xiaowu0162/longmemeval-cleaned` variant.

## Result by question type

| Question type | Correct | Accuracy |
|---|---:|---:|
| single-session-assistant | 54 / 56 | 96.4% |
| knowledge-update | 74 / 78 | 94.9% |
| single-session-user | 66 / 70 | 94.3% |
| temporal-reasoning | 115 / 133 | 86.5% |
| single-session-preference | 25 / 30 | 83.3% |
| multi-session | 93 / 133 | 69.9% |
| **All questions** | **427 / 500** | **85.4%** |

All 500 reader calls and all 500 judge calls completed after documented technical retries. The final aggregate has no missing answers and no technical errors. The original first 100-question run scored 77/100 with nine truncated reader calls; retrying only those failures with the larger output limit produced 80/100. A separate HTTP failure interrupted the third hundred; only its failed and unattempted questions were resumed, producing 77/100.

## Configuration

| Setting | Value |
|---|---|
| Engine | Loomem E1 fragments evaluation binary |
| Embeddings | Local `multilingual-e5-small`, 384 dimensions |
| Reader and judge | Kimi `k3-256k` |
| Reader / judge output limits | 8000 / 512 tokens |
| Reasoning effort / temperature / seed | `low` / `1` / `42` |
| Returned evidence | 60 records per question |
| Reader evidence | Contract v2, 8000-token evidence budget |
| Packing | Rank-stable, full-record, first-fit |
| Evaluation protocol | `loomem-longmemeval-strict-v2-context-v2` |
| Dataset SHA-256 | `d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442` |
| Engine binary SHA-256 | `2f16b74bc4a72a659314b494e7df6fb46bbfe3048ab73cf5baf17347e568eed4` |

The reader receives the current question and returned memory evidence, never reference answers, question categories or evaluation labels. Reference answers go only to the separate judge. The judge templates follow the pinned upstream LongMemEval evaluator, while parsing is stricter: only an exact `yes` or `no` is accepted.

## Retrieval and reader-context diagnostics

Retrieval diagnostics are measured offline from source labels and are not QA accuracy. Session or turn provenance also does not prove that a fragment preserves the required semantic content.

| Diagnostic | Result |
|---|---:|
| Macro answer-session recall | 99.62% |
| Questions with all answer sessions returned | 496 / 500 |
| Macro labelled-turn recall before packing | 92.07% |
| Questions with all labelled turns returned | 421 / 479 |
| Macro labelled-turn recall after packing | 86.32% |
| Questions with all labelled turns sent to the reader | 380 / 479 |
| Questions that lost at least one retrieved labelled turn during packing | 53 / 479 |

## Multi-session error analysis

Multi-session is the main current weakness: it accounts for 40 of the 73 incorrect answers. A manual review of those answers, combined with provenance analysis before and after context packing, gives the following exclusive diagnostic buckets:

| Finding | Cases |
|---|---:|
| At least one retrieved labelled turn was dropped during reader-context packing | 22 |
| Retrieval coverage was incomplete, with no additional labelled-turn loss during packing | 11 |
| All labelled turns reached the reader, but the answer was incorrect | 4 |
| Abstention cases, including possible strict-judge or evidence-interpretation issues | 3 |

These buckets identify where to investigate; they do not prove a single cause for each wrong answer. The first improvement priority is reader-side evidence selection for questions that require combining facts across sessions. The next priorities are retrieval coverage for the remaining multi-session questions and temporal or arithmetic reasoning when the necessary evidence is already present.

## How to read this result

- **Self-run diagnostic evaluation.** This is not a third-party leaderboard result. The 85.4% score belongs to the exact configuration above and should not be presented as a model-independent property of Loomem.
- **The reader and judge matter.** Kimi `k3-256k` performed both roles. No independent semantic audit has closed this report, and the score is not directly comparable with the earlier GPT-4.1 configuration or with other vendors' published numbers.
- **This was not an unseen holdout.** LongMemEval had been used during earlier development. The result does not by itself establish generalisation to new histories or real user conversations.
- **Local embeddings, external reader.** The memory engine and embeddings ran locally; the Kimi reader and judge used their available API path. The full evaluation was therefore not an entirely offline run.
- **Previous result.** The earlier 26 June 2026 configuration reported 75.0% (375 / 500) with GPT-4.1 as reader and judge. The numerical improvement is encouraging, but protocol and model differences prevent treating it as a controlled ablation.

Loomem is open source under Apache-2.0. LongMemEval: Wu et al., “LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory”. Dataset: `xiaowu0162/longmemeval-cleaned`.
