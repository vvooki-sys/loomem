"""Effective engine config, artifact pins and sandbox wrapper for ``paid_engine``.

Config = the frozen ``config.toml`` template through ``runner_local.
effective_config`` plus the owner-frozen overrides: knowledge extraction ON
(chunked, ``[llm] provider = "bench"`` pointing at the provider proxy, key env
``LOOMEM_BENCH_PROXY_TOKEN``), fragments ON, ``[search] evidence_fields``
ON, every other LLM mutator OFF, local 384-d embeddings, loopback host and a
random port (never the product ports). Pure functions, no process here.

Engine levers (``ENGINE_LEVERS``) are the retrieval/extraction switches a campaign
may set explicitly: ``levers`` is a dict ``{"section.key": value}`` checked against
the table (known key, same type as the r5 default, ``prompt_variant`` in its enum).
Only a lever whose value differs from its default is written into the TOML, so a
campaign without levers (or with every lever at its default) renders byte-identical
text and keeps its ``effective_config_sha256``; a non-default lever adds or replaces
exactly one ``key = value`` line.
"""
import hashlib
from pathlib import Path
import re
import shlex
import socket

from paid_provider_proxy import KEY_ENV
from runner_local import effective_config, replace_setting
from smoke_local_e1 import PROFILE, SANDBOX
from validate import ValidationError, sha256

FORBIDDEN_PORTS = frozenset({3035, 3037, 3038})
ARTIFACT_KEYS = frozenset({"binary", "binary_sha256", "template", "template_sha256", "model_dir",
                           "model_onnx_sha256", "tokenizer_sha256"})
EXTRACTION_KEYS = frozenset({"max_completion_tokens", "input_chunk_tokens", "split_on_truncation",
                             "max_facts_per_chunk", "completion_tokens_floor", "max_transcript_tokens"})
DISABLED_SECTIONS = ("scheduler", "entity_extraction", "memory_generator", "contradiction",
                     "conversation_extraction", "profile", "dream", "content_type", "worker.backup",
                     "search.cache", "search.tier1", "associator")
PROVIDER, MODEL = "bench", "gpt-6-luna"
# W0 search/MCP levers (origin/main b605c05f, PR #176): frozen at their off/neutral
# values so the evaluation config names them explicitly (coordinator decision 2026-11-23);
# preflight asserts exactly these values in the rendered TOML.
W0_FLAGS = (("search", "rerank_date_prefix", False), ("search", "rerank_llm_max_chars", 200),
            ("search", "rerank_onnx_max_tokens", 0), ("search", "rerank_sync", False),
            ("search", "rerank_sync_timeout_ms", 2000), ("search", "event_date_proximity_boost", 0.0),
            ("mcp", "search_include_predecessor", False), ("mcp", "predecessor_max_chars", 400),
            ("mcp", "search_group_by", "none"))
# Engine levers with their r5 defaults (holdout-200-2026-11-25-r5, effective config aa7510a0...): a
# campaign sets them explicitly through --engine-lever and campaign.json records the non-default ones
# under engine.levers. The state_followup_* keys and prompt_variant values are the engine's names.
ENGINE_LEVERS = (
    ("search", "vector_fill_missing", False),
    ("knowledge_extraction", "prompt_variant", "default"),
    ("search", "state_followup_enabled", False),
    ("search", "state_followup_max_seeds", 3),
    ("search", "state_followup_hits_per_seed", 60),
    ("search", "state_followup_fact_types", ["project_state", "event"]),
    ("search", "state_followup_weight", 1.0),
    ("search", "state_followup_with_date_filter", False),
)
LEVER_DEFAULTS = {f"{section}.{key}": default for section, key, default in ENGINE_LEVERS}
PROMPT_VARIANTS = ("default", "relations", "relations_v2")
# The effective-config fingerprint renders the same config with the per-process
# binding values (work dir, engine port, proxy port) at fixed placeholders.
FINGERPRINT_WORK, FINGERPRINT_PORT, FINGERPRINT_PROXY = Path("/loomem-evaluation-work"), 1024, "http://127.0.0.1:1/v1"


class EngineError(RuntimeError):
    """The engine, its config or one of its answers cannot be trusted; the run stops."""


def check_port(port):
    if type(port) is not int or not 1024 <= port <= 65535 or port in FORBIDDEN_PORTS:
        raise EngineError(f"port {port!r} is not allowed (1024-65535, never {sorted(FORBIDDEN_PORTS)})")
    return port


def free_port():
    while True:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        if port >= 1024 and port not in FORBIDDEN_PORTS:
            return port


def check_artifacts(artifacts):
    if not isinstance(artifacts, dict) or set(artifacts) != ARTIFACT_KEYS:
        raise EngineError(f"artifacts must have exactly {sorted(ARTIFACT_KEYS)}")
    model = Path(artifacts["model_dir"])
    pairs = [(artifacts["binary"], artifacts["binary_sha256"]), (artifacts["template"], artifacts["template_sha256"]),
             (model / "model.onnx", artifacts["model_onnx_sha256"]), (model / "tokenizer.json", artifacts["tokenizer_sha256"])]
    for path, expected in pairs:
        path = Path(path)
        if not path.is_absolute() or not path.is_file():
            raise EngineError(f"engine artifact missing or relative: {path}")
        if sha256(path) != expected:
            raise EngineError(f"engine artifact hash mismatch: {path}")
    return dict(artifacts)


def check_extraction(extraction):
    if not isinstance(extraction, dict) or set(extraction) != EXTRACTION_KEYS:
        raise EngineError(f"extraction settings must have exactly {sorted(EXTRACTION_KEYS)}")
    for key in EXTRACTION_KEYS - {"split_on_truncation"}:
        if type(extraction[key]) is not int or extraction[key] < 0:
            raise EngineError(f"extraction.{key} must be a nonnegative int")
    if type(extraction["split_on_truncation"]) is not bool:
        raise EngineError("extraction.split_on_truncation must be a bool")
    for key in ("max_completion_tokens", "input_chunk_tokens", "max_facts_per_chunk", "max_transcript_tokens"):
        if extraction[key] == 0:
            raise EngineError(f"extraction.{key} must be positive")
    return dict(extraction)


def _lever_type_ok(value, default):
    if isinstance(default, bool):
        return type(value) is bool
    if isinstance(default, int):
        return type(value) is int
    if isinstance(default, float):
        return type(value) in (int, float)
    if isinstance(default, list):
        return isinstance(value, list) and all(type(item) is str for item in value)
    return type(value) is str


def check_levers(levers):
    """``{"section.key": value}`` (None or empty = no levers) -> the same dict with every key known,
    every value of the default's type (an int is accepted for a float lever and stored as float) and
    ``prompt_variant`` in PROMPT_VARIANTS; keys sorted. Default-valued entries are kept: the caller
    decides what to emit."""
    if levers is None:
        return {}
    if not isinstance(levers, dict):
        raise EngineError("levers must be a dict {\"section.key\": value}")
    out = {}
    for name in sorted(levers):
        if name not in LEVER_DEFAULTS:
            raise EngineError(f"unknown engine lever {name!r}; known: {sorted(LEVER_DEFAULTS)}")
        default, value = LEVER_DEFAULTS[name], levers[name]
        if not _lever_type_ok(value, default):
            raise EngineError(f"engine lever {name} must be a {type(default).__name__}, got {value!r}")
        if isinstance(default, float):
            value = float(value)
        if name == "knowledge_extraction.prompt_variant" and value not in PROMPT_VARIANTS:
            raise EngineError(f"engine lever {name} must be one of {PROMPT_VARIANTS}, got {value!r}")
        out[name] = list(value) if isinstance(value, list) else value
    return out


def lever_overrides(levers):
    """``(section, key, value)`` for every lever that differs from its default, in ENGINE_LEVERS order."""
    levers = check_levers(levers)
    return [(section, key, levers[f"{section}.{key}"]) for section, key, default in ENGINE_LEVERS
            if f"{section}.{key}" in levers and levers[f"{section}.{key}"] != default]


def evaluation_config(artifacts, work, *, port, proxy_url, extraction, levers=None):
    """The effective TOML text for one engine process (see module docstring)."""
    check_port(port)
    artifacts = check_artifacts(artifacts)
    extraction = check_extraction(extraction)
    levers = lever_overrides(levers)
    if not re.fullmatch(r"http://127\.0\.0\.1:\d+/v1", proxy_url or ""):
        raise EngineError(f"proxy_url must be http://127.0.0.1:<port>/v1, got {proxy_url!r}")
    work = Path(work)
    if not work.is_absolute():
        raise EngineError("work directory must be absolute")
    try:
        text = effective_config({"endpoint": f"http://127.0.0.1:{port}", "local_engine": artifacts}, work)
    except ValidationError as exc:
        raise EngineError(f"engine artifacts rejected: {exc}") from exc
    entry = f"llm.providers.{PROVIDER}"
    overrides = [("llm", "provider", PROVIDER), ("llm", "compression_model", MODEL),
                 (entry, "base_url", proxy_url), (entry, "api_key_env", KEY_ENV), (entry, "omit_temperature", True),
                 (entry, "max_completion_tokens", True),
                 (entry, "completion_tokens_floor", extraction["completion_tokens_floor"]),
                 (entry, "reasoning_effort", "low"),
                 ("knowledge_extraction", "enabled", True),
                 ("knowledge_extraction", "max_tokens", extraction["max_completion_tokens"]),
                 ("knowledge_extraction", "input_chunk_tokens", extraction["input_chunk_tokens"]),
                 ("knowledge_extraction", "split_on_truncation", extraction["split_on_truncation"]),
                 ("knowledge_extraction", "max_facts_per_chunk", extraction["max_facts_per_chunk"]),
                 ("knowledge_extraction", "max_transcript_tokens", extraction["max_transcript_tokens"]),
                 ("knowledge_extraction", "contradiction_check", False),
                 ("fragments", "enabled", True), ("fragments", "provenance_role", "claim"),
                 ("fragments", "token_counter", "embedder"), ("search", "evidence_fields", True),
                 ("search", "rerank_enabled", False), ("search", "multi_query_enabled", False),
                 # PR #177 keys: the dream schedule and its auto trigger stay off even on a build that
                 # knows them (the config has no deny_unknown_fields, so this is harmless before #177).
                 ("dream", "schedule_enabled", False), ("dream", "auto_trigger_threshold", 0),
                 *W0_FLAGS, *levers]
    for section, key, value in overrides:
        text = replace_setting(text, section, key, value)
    for section in DISABLED_SECTIONS:
        text = replace_setting(text, section, "enabled", False)
    return text


def config_fingerprint(artifacts, extraction, levers=None):
    """SHA-256 of the effective config with placeholder work dir, port and proxy port."""
    text = evaluation_config(artifacts, FINGERPRINT_WORK, port=FINGERPRINT_PORT, proxy_url=FINGERPRINT_PROXY,
                             extraction=extraction, levers=levers)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def sandbox_wrapper(directory, binary):
    """`exec sandbox-exec -p PROFILE <binary>`: outbound loopback only."""
    wrapper = Path(directory) / "server-offline"
    wrapper.write_text("#!/bin/sh\nexec " + shlex.join([SANDBOX, "-p", PROFILE, str(binary)]) + "\n")
    wrapper.chmod(0o700)
    return wrapper
