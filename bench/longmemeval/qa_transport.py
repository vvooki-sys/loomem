"""Single-attempt OpenAI transport and durable budget-bound completion cache."""
from decimal import Decimal
import json
import os
from pathlib import Path
import tempfile
from urllib.request import Request, build_opener, HTTPRedirectHandler, ProxyHandler

from validate import canonical_hash, decode


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Completion redirects forbidden")


class OpenAITransport:
    def __init__(self, key_env="OPENAI_API_KEY", opener=None):
        self.key_env = key_env
        self.opener = opener or build_opener(ProxyHandler({}), NoRedirect())

    def __call__(self, payload, timeout_seconds):
        key = os.environ.get(self.key_env)
        if not key:
            raise ValueError("Missing completion API credential")
        request = Request("https://api.openai.com/v1/chat/completions",
                          data=json.dumps(payload, allow_nan=False).encode(),
                          headers={"Content-Type": "application/json",
                                   "Authorization": "Bearer " + key})
        with self.opener.open(request, timeout=timeout_seconds) as response:
            if response.status != 200:
                raise ValueError("Unexpected completion HTTP status")
            body = response.read(4 * 1024 * 1024 + 1)
        if len(body) > 4 * 1024 * 1024:
            raise ValueError("Completion response too large")
        return decode(body.decode(), "completion")


def validate_model_config(config):
    required = {"model", "context_window_tokens", "max_completion_tokens",
                "input_usd_per_million", "output_usd_per_million",
                "temperature", "seed", "timeout_seconds"}
    if not isinstance(config, dict) or set(config) != required:
        raise ValueError("Completion config requires exact frozen fields")
    if not isinstance(config["model"], str) or not config["model"].strip():
        raise ValueError("Missing frozen model")
    for key in ("context_window_tokens", "max_completion_tokens", "timeout_seconds"):
        if type(config[key]) is not int or config[key] <= 0:
            raise ValueError("Completion limits must be positive integers")
    if config["max_completion_tokens"] >= config["context_window_tokens"]:
        raise ValueError("Completion limit exceeds model context")
    if type(config["seed"]) is not int:
        raise ValueError("Seed must be an integer")
    if type(config["temperature"]) not in (int, float) or not 0 <= config["temperature"] <= 2:
        raise ValueError("Temperature must be finite and within [0,2]")
    for key in ("input_usd_per_million", "output_usd_per_million"):
        if not isinstance(config[key], str):
            raise ValueError("Prices require explicit decimal strings")
        value = Decimal(config[key])
        if not value.is_finite() or value <= 0:
            raise ValueError("Prices must be finite and positive")
    # Explicit trusted floor, verified by coordinator against official model docs.
    # New models/prices require a reviewed change, never user-configured underpricing.
    if (config["model"] != "gpt-4.1-2025-04-14"
            or config["context_window_tokens"] < 1047576
            or Decimal(config["input_usd_per_million"]) < Decimal("2")
            or Decimal(config["output_usd_per_million"]) < Decimal("8")):
        raise ValueError("Model/context/prices below trusted reservation bounds")


def cost(config, input_tokens, output_tokens):
    return (Decimal(config["input_usd_per_million"]) * input_tokens
            + Decimal(config["output_usd_per_million"]) * output_tokens) / Decimal(1000000)


def atomic_json(path, value):
    atomic_text(path, json.dumps(value, ensure_ascii=False, allow_nan=False, sort_keys=True) + "\n")


def atomic_text(path, text):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=".qa-", suffix=".tmp")
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(name, path)
        directory_fd = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def parse_response(response, model):
    if not isinstance(response, dict) or response.get("model") != model:
        raise ValueError("Completion model mismatch")
    usage = response.get("usage", {})
    counts = {key: usage.get(key) for key in ("prompt_tokens", "completion_tokens", "total_tokens")}
    if any(type(value) is not int or value < 0 for value in counts.values()):
        raise ValueError("Missing or invalid completion usage")
    if counts["total_tokens"] != counts["prompt_tokens"] + counts["completion_tokens"]:
        raise ValueError("Inconsistent completion usage")
    choices = response.get("choices")
    if not isinstance(choices, list) or len(choices) != 1:
        raise ValueError("Expected one completion")
    text = choices[0].get("message", {}).get("content")
    if not isinstance(text, str) or not text.strip():
        raise ValueError("Missing completion text")
    return {"text": text, "usage": counts,
            "finish_reason": choices[0].get("finish_reason"), "model": model}


def complete(messages, config, binding, ledger, cache_dir, transport):
    """Never retry a request; an uncertain call retains its full reservation."""
    validate_model_config(config)
    payload = {key: config[key] for key in ("model", "temperature", "seed", "max_completion_tokens")}
    payload["messages"] = messages
    request_hash = canonical_hash({"payload": payload, "config": config, "binding": binding})
    path = Path(cache_dir) / (request_hash + ".json")
    if path.exists():
        entry = decode(path.read_text(), "completion cache")
        if entry.get("request_sha256") != request_hash:
            raise ValueError("Completion cache binding mismatch")
        validate_cached(entry, request_hash, config, ledger)
        return settle_response(entry, path, ledger)
    try:
        token = ledger.reserve(request_hash, cost(config, config["context_window_tokens"],
                                                 config["max_completion_tokens"]))
    except Exception as exc:
        return {"status": "failed", "error_type": type(exc).__name__, "request_sha256": request_hash}
    try:
        result = parse_response(transport(payload, config["timeout_seconds"]), config["model"])
        actual = cost(config, result["usage"]["prompt_tokens"], result["usage"]["completion_tokens"])
        entry = {"status": "response_received", "request_sha256": request_hash,
                 "reservation_token": token, "actual_usd": str(actual), **result}
    except Exception as exc:
        entry = {"status": "failed", "request_sha256": request_hash,
                 "reservation_token": token, "error_type": type(exc).__name__}
    write_cache(path, entry)
    return settle_response(entry, path, ledger)


def settle_response(entry, path, ledger):
    if "actual_usd" in entry:
        ledger.settle(entry["reservation_token"], entry["actual_usd"])
    if entry.get("status") != "response_received":
        return entry
    entry["status"] = "completed" if entry["finish_reason"] == "stop" else "failed"
    if entry["status"] == "failed":
        entry["error_type"] = "IncompleteCompletion"
    write_cache(path, entry)
    return entry


def write_cache(path, entry):
    atomic_json(path, {**entry, "entry_sha256": canonical_hash(entry)})


def validate_cached(entry, request_hash, config, ledger):
    digest = entry.pop("entry_sha256", None)
    if digest != canonical_hash(entry):
        raise ValueError("Completion cache integrity mismatch")
    if entry.get("status") not in {"completed", "failed", "response_received"}:
        raise ValueError("Invalid cached completion status")
    ledger.verify_reservation(entry.get("reservation_token"), request_hash,
                              cost(config, config["context_window_tokens"], config["max_completion_tokens"]))
    base = {"status", "request_sha256", "reservation_token"}
    if "actual_usd" not in entry:
        if set(entry) != base | {"error_type"} or entry["status"] != "failed":
            raise ValueError("Invalid cached failure")
        return
    fields = base | {"actual_usd", "text", "usage", "finish_reason", "model"}
    if entry["status"] == "failed":
        fields.add("error_type")
    if set(entry) != fields:
        raise ValueError("Invalid cached completion shape")
    parse_response({"model": entry["model"], "usage": entry["usage"], "choices": [
        {"message": {"content": entry["text"]}, "finish_reason": entry["finish_reason"]}]}, config["model"])
    actual = cost(config, entry["usage"]["prompt_tokens"], entry["usage"]["completion_tokens"])
    if Decimal(entry["actual_usd"]) != actual:
        raise ValueError("Cached usage cost mismatch")
    if entry["status"] == "completed" and entry["finish_reason"] != "stop":
        raise ValueError("Invalid cached completion finish reason")
