"""Opt-in strict-v3-bounded LongMemEval reader/judge runner.

This adapter is deliberately separate from the frozen strict-v2 ``qa.py``.
It performs no retries and adds a strict response format only to judge calls.
"""

import json
from dataclasses import dataclass
from pathlib import Path

from qa_context import count_text, pack_context
from qa_prompts_strict_v3 import (
    PROTOCOL,
    is_abstention,
    judge_messages,
    judge_prompt_sha256,
    judge_response_format,
    parse_judge_label,
    prompt_provenance,
    reader_messages,
    reader_prompt_sha256,
    response_format_sha256,
)
from qa_transport import (
    CompletionOptions,
    OpenAITransport,
    complete_with_options,
    validate_model_config,
)
from runner import load_context_artifact
from validate import canonical_hash


@dataclass(frozen=True)
class RunnerDependencies:
    transport: object = None
    packer: object = pack_context
    token_counter: object = count_text


def _validate_protocol_binding(config, manifest, ledger):
    if manifest.get("config_sha256") != canonical_hash(config):
        raise ValueError("QA config is not manifest-bound")
    expected = {
        "protocol": PROTOCOL,
        "reader_prompt_sha256": reader_prompt_sha256(),
        "judge_prompt_sha256": judge_prompt_sha256(),
        "judge_response_format_sha256": response_format_sha256(),
    }
    if any(config.get(key) != value for key, value in expected.items()):
        raise ValueError("Strict-v3 protocol binding mismatch")
    if ledger.totals()["limit_usd"] > 100:
        raise ValueError("QA ledger exceeds authorized USD100 cap")


def _validated_settings(config):
    settings = config["qa"]
    if set(settings) != {"reader", "judge"}:
        raise ValueError("QA settings require reader/judge configurations")
    for role in ("reader", "judge"):
        validate_model_config(settings[role])
        if settings[role]["model"] != config.get(role + "_model"):
            raise ValueError("Manifest model mismatch")
    if settings["reader"]["max_completion_tokens"] != 512:
        raise ValueError("Strict-v3 reader requires the frozen 512-token ceiling")
    if type(config.get("context_budget_tokens")) is not int or config["context_budget_tokens"] <= 0:
        raise ValueError("Invalid context budget")
    return settings


class ProtocolQARunner:
    """Protocol-injected runner for the bounded reader and enum judge."""

    def __init__(
        self,
        config,
        manifest,
        ledger,
        cache_dir,
        dependencies=None,
    ):
        _validate_protocol_binding(config, manifest, ledger)
        dependencies = dependencies or RunnerDependencies()
        self.config = json.loads(json.dumps(config, allow_nan=False))
        self.manifest_hash = canonical_hash(manifest)
        self.settings = _validated_settings(self.config)
        self.ledger = ledger
        self.cache_dir = Path(cache_dir)
        self.transport = dependencies.transport or OpenAITransport()
        self.packer = dependencies.packer
        self.token_counter = dependencies.token_counter

    def stage(self, role, messages, binding):
        settings = self.settings[role]
        text_tokens = sum(self.token_counter(message["content"], settings["model"])
                          for message in messages)
        if text_tokens + settings["max_completion_tokens"] >= settings["context_window_tokens"]:
            raise ValueError("Rendered message content exceeds context capacity")
        response_format = judge_response_format() if role == "judge" else None
        stage_binding = {
            **binding,
            "stage": role,
            "response_format_sha256": response_format_sha256() if response_format else "none",
        }
        return complete_with_options(
            messages,
            settings,
            stage_binding,
            self.ledger,
            self.cache_dir,
            CompletionOptions(self.transport, response_format),
        )

    def run_question(self, row, context_path, trace):
        record = {
            "question_id": row["question_id"],
            "question_type": row["question_type"],
            "manifest_sha256": self.manifest_hash,
            "protocol": PROTOCOL,
            "status": "failed",
        }
        phase = "retrieval"
        try:
            if trace.get("question_id") != row["question_id"]:
                raise ValueError("Question/trace mismatch")
            artifact = load_context_artifact(context_path, trace, self.manifest_hash)
            packed = self.packer(
                artifact["records"],
                self.config["context_budget_tokens"],
                self.settings["reader"]["model"],
            )
            record["context_sha256"] = trace["context_sha256"]
            record["packing"] = {key: value for key, value in packed.items() if key != "context"}
            binding = {
                "manifest_sha256": self.manifest_hash,
                "question_id": row["question_id"],
                "context_sha256": trace["context_sha256"],
                "protocol": PROTOCOL,
                "reader_prompt_sha256": reader_prompt_sha256(),
                "judge_prompt_sha256": judge_prompt_sha256(),
                "judge_response_format_sha256": response_format_sha256(),
            }
            phase = "judge_preflight"
            judge_messages(
                row["question_type"],
                row["question"],
                row["answer"],
                "",
                abstention=is_abstention(row["question_id"]),
            )
            phase = "reader"
            messages = reader_messages(row["question"], row["question_date"], packed["context"])
            record["reader_messages_sha256"] = canonical_hash(messages)
            reader = self.stage("reader", messages, binding)
            record["reader"] = reader
            if reader["status"] != "completed":
                record["failure_stage"] = phase
                return record
            record["hypothesis"] = reader["text"]
            phase = "judge"
            messages = judge_messages(
                row["question_type"],
                row["question"],
                row["answer"],
                reader["text"],
                abstention=is_abstention(row["question_id"]),
            )
            record["judge_messages_sha256"] = canonical_hash(messages)
            judge = self.stage("judge", messages, binding)
            record["judge"] = judge
            if judge["status"] != "completed":
                record["failure_stage"] = phase
                return record
            record["correct"] = parse_judge_label(judge["text"])
            record["status"] = "completed"
            return record
        except Exception as exc:
            record.update(failure_stage=phase, error_type=type(exc).__name__)
            return record

    @staticmethod
    def provenance():
        return prompt_provenance()
