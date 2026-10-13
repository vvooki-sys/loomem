#!/usr/bin/env python3
"""Budget-bound, resumable reader/judge QA; only explicit CLI invokes paid calls."""
import argparse
import json
from pathlib import Path

from budget import BudgetLedger
from qa_context import pack_context, count_text
from qa_prompts import (reader_messages, judge_messages, reader_prompt_sha256,
                        judge_prompt_sha256, parse_judge_label, is_abstention,
                        prompt_provenance)
from qa_transport import OpenAITransport, complete, atomic_json, atomic_text, validate_model_config
from runner import load_context_artifact
from validate import canonical_hash, check_manifest, decode


class QARunner:
    def __init__(self, config, manifest, ledger, cache_dir, *, transport=None,
                 packer=pack_context, token_counter=count_text):
        if manifest.get("config_sha256") != canonical_hash(config):
            raise ValueError("QA config is not manifest-bound")
        if (config.get("reader_prompt_sha256") != reader_prompt_sha256()
                or config.get("judge_prompt_sha256") != judge_prompt_sha256()):
            raise ValueError("Frozen prompt hash mismatch")
        if ledger.totals()["limit_usd"] > 100:
            raise ValueError("QA ledger exceeds authorized USD100 cap")
        self.config = json.loads(json.dumps(config, allow_nan=False))
        self.manifest_hash = canonical_hash(manifest)
        self.settings = self.config["qa"]
        if set(self.settings) != {"reader", "judge"}:
            raise ValueError("QA settings require reader/judge configurations")
        for role in ("reader", "judge"):
            validate_model_config(self.settings[role])
            if self.settings[role]["model"] != config.get(role + "_model"):
                raise ValueError("Manifest model mismatch")
        if type(config.get("context_budget_tokens")) is not int or config["context_budget_tokens"] <= 0:
            raise ValueError("Invalid context budget")
        self.ledger, self.cache_dir = ledger, Path(cache_dir)
        self.transport = transport or OpenAITransport()
        self.packer, self.token_counter = packer, token_counter

    def stage(self, role, messages, binding):
        settings = self.settings[role]
        # This counts model-tokenized message content, not undocumented protocol
        # overhead. Monetary reservation instead covers the entire model window.
        text_tokens = sum(self.token_counter(m["content"], settings["model"]) for m in messages)
        if text_tokens + settings["max_completion_tokens"] >= settings["context_window_tokens"]:
            raise ValueError("Rendered message content exceeds context capacity")
        return complete(messages, settings, {**binding, "stage": role}, self.ledger,
                        self.cache_dir, self.transport)

    def run_question(self, row, context_path, trace):
        record = {"question_id": row["question_id"], "question_type": row["question_type"],
                  "manifest_sha256": self.manifest_hash, "status": "failed"}
        phase = "retrieval"
        try:
            if trace.get("question_id") != row["question_id"]:
                raise ValueError("Question/trace mismatch")
            artifact = load_context_artifact(context_path, trace, self.manifest_hash)
            packed = self.packer(artifact["records"], self.config["context_budget_tokens"],
                                 self.settings["reader"]["model"])
            record["context_sha256"] = trace["context_sha256"]
            record["packing"] = {key: value for key, value in packed.items() if key != "context"}
            binding = {"manifest_sha256": self.manifest_hash,
                       "question_id": row["question_id"], "context_sha256": trace["context_sha256"],
                       "reader_prompt_sha256": reader_prompt_sha256(),
                       "judge_prompt_sha256": judge_prompt_sha256()}
            phase = "reader"
            reader = self.stage("reader", reader_messages(row["question"], row["question_date"],
                                                           packed["context"]), binding)
            record["reader"] = reader
            if reader["status"] != "completed":
                record["failure_stage"] = phase
                return record
            record["hypothesis"] = reader["text"]
            phase = "judge"
            messages = judge_messages(row["question_type"], row["question"], row["answer"],
                                      reader["text"], abstention=is_abstention(row["question_id"]))
            judge = self.stage("judge", messages, binding)
            record["judge"] = judge
            if judge["status"] != "completed":
                record["failure_stage"] = phase
                return record
            record["correct"] = parse_judge_label(judge["text"])
            record["status"] = "completed"
            return record
        except Exception as exc:
            # Never persist exception text: HTTP/provider errors may contain
            # credentials, request content, or a private URL.
            record.update(failure_stage=phase, error_type=type(exc).__name__)
            return record


def output_identity(output, manifest_hash):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    path = output / "run.json"
    expected = {"manifest_sha256": manifest_hash}
    if path.exists():
        if decode(path.read_text(), "output identity") != expected:
            raise ValueError("Output directory belongs to another run")
    elif any(output.iterdir()):
        raise ValueError("Nonempty output directory lacks run identity")
    else:
        atomic_json(path, expected)


def aggregate_outputs(output, rows, manifest_hash):
    records = []
    for row in rows:
        path = Path(output) / (canonical_hash(row["question_id"]) + ".json")
        if not path.exists():
            continue
        record = decode(path.read_text(), "QA result")
        if (record.get("manifest_sha256") != manifest_hash
                or record.get("question_id") != row["question_id"]
                or record.get("question_type") != row["question_type"]):
            raise ValueError("QA output binding mismatch")
        if record.get("status") == "completed":
            if type(record.get("correct")) is not bool:
                raise ValueError("Completed output requires boolean correctness")
        elif record.get("status") != "failed" or "correct" in record or not record.get("failure_stage"):
            raise ValueError("Invalid failed output")
        records.append(record)
    return records


def run_cli(args):
    config = decode(Path(args.config).read_text(), "config")
    manifest = decode(Path(args.manifest).read_text(), "manifest")
    check_manifest(args.manifest, args.dataset, args.config)
    rows = decode(Path(args.dataset).read_text(), "dataset")
    traces = {}
    for line in Path(args.traces).read_text().splitlines():
        trace = decode(line, "retrieval trace")
        if trace["question_id"] in traces:
            raise ValueError("Duplicate trace question")
        traces[trace["question_id"]] = trace
    runner = QARunner(config, manifest, BudgetLedger(args.ledger, "100"), args.cache,
                      transport=OpenAITransport(args.key_env))
    output_identity(args.output, runner.manifest_hash)
    aggregate_outputs(args.output, rows, runner.manifest_hash)
    selected = rows if not args.question_id else [row for row in rows if row["question_id"] in args.question_id]
    if args.question_id and len(selected) != len(set(args.question_id)):
        raise ValueError("Unknown selected question ID")
    records = []
    for row in selected:
        trace = traces.get(row["question_id"], {})
        digest = trace.get("context_sha256", "missing")
        record = runner.run_question(row, Path(args.contexts) / (digest + ".json"), trace)
        records.append(record)
        # Per-question artifacts survive a process interruption. API calls also
        # have independent durable caches; repeated CLI runs never resubmit them.
        atomic_json(Path(args.output) / (canonical_hash(row["question_id"]) + ".json"), record)
        print(json.dumps({"question_id": row["question_id"], "status": record["status"],
                          "failure_stage": record.get("failure_stage")}))
    invocation_count = len(records)
    records = aggregate_outputs(args.output, rows, runner.manifest_hash)
    atomic_text(Path(args.output) / "results.jsonl", "".join(
        json.dumps(record, ensure_ascii=False, allow_nan=False) + "\n" for record in records))
    summary = {"expected_count": len(rows), "attempted_count": len(records),
               "invocation_count": invocation_count, "missing_count": len(rows) - len(records),
               "completed_count": sum(r["status"] == "completed" for r in records),
               "correct_count": sum(r.get("correct", False) for r in records),
               "accuracy": sum(r.get("correct", False) for r in records) / len(rows),
               "manifest_sha256": runner.manifest_hash, "protocol": prompt_provenance(),
               "budget": {k: str(v) for k, v in runner.ledger.totals().items()}}
    atomic_json(Path(args.output) / "summary.json", summary)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for field in ("dataset", "config", "manifest", "traces", "contexts", "ledger", "cache", "output"):
        parser.add_argument("--" + field, required=True)
    parser.add_argument("--key-env", default="OPENAI_API_KEY")
    parser.add_argument("--question-id", action="append")
    args = parser.parse_args()
    try:
        summary = run_cli(args)
        if summary["completed_count"] < summary["attempted_count"]:
            parser.exit(1, "QA contains failed requests; inspect bound results\n")
    except Exception as exc:
        parser.exit(2, "QA refused: " + type(exc).__name__ + "\n")


if __name__ == "__main__":
    main()
