"""Preflight probes that touch the machine: sandbox, engine artifacts, tokenizer, gold scan.

Helpers for ``paid_preflight.run_preflight``; each returns a ``(passed, detail)``
pair and never raises. Nothing here spends money or opens a non-loopback socket:
the sandbox probe proves that a child under ``sandbox-exec`` CANNOT reach a
routable address (immediate EPERM) while loopback still works, and the o200k
tokenizer is loaded only from an existing ``TIKTOKEN_CACHE_DIR`` file (a cache
miss is a FAIL, never a download).
"""
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import tomllib

from paid_holdout import leaks_gold
from smoke_local_e1 import PROFILE, loopback_only

SANDBOX = "/usr/bin/sandbox-exec"
O200K_URL = "https://openaipublic.blob.core.windows.net/encodings/o200k_base.tiktoken"
PROBE_SCRIPT = (
    "import socket, sys, time\n"
    "port = int(sys.argv[1])\n"
    "t = time.monotonic(); s = socket.socket(); s.settimeout(1)\n"
    "try:\n    s.connect(('10.255.255.1', 9)); print('CONNECTED')\n"
    "except OSError as e:\n    print('OSERROR', e.errno, int((time.monotonic() - t) * 1000))\n"
    "finally:\n    s.close()\n"
    "l = socket.socket(); l.settimeout(2)\n"
    "try:\n    l.connect(('127.0.0.1', port)); print('LOOPBACK_OK')\n"
    "except OSError as e:\n    print('LOOPBACK_FAIL', e.errno)\n"
    "finally:\n    l.close()\n")
# The stages whose requests carry gold by design (judge; the plan §8C review): not leak-scanned.
GOLD_STAGES = ("judge", "review")
ARTIFACT_KEYS = ("binary", "binary_sha256", "template", "template_sha256", "model_dir",
                 "model_onnx_sha256", "tokenizer_sha256")
# harness_sha256 covers every paid_*.py plus these runtime dependencies of the paid path.
HARNESS_FIXED = ("fragment_projection.py", "evidence_gate.py", "gate_verifier.py", "extraction_receipt.py",
                 "ingest_receipt.py", "readiness_proof.py", "budget.py", "runner_local.py",
                 "runner_local_e1.py", "qa_context_v2.py", "qa_context.py", "evidence_v2.py", "smoke_local_e1.py")


def sha256_text(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def harness_sha256(directory=None):
    """sha256 of every paid_*.py plus HARNESS_FIXED (test modules excluded); pinned in campaign.json."""
    directory = Path(directory or Path(__file__).parent)
    names = sorted({p.name for p in directory.glob("paid_*.py")} | set(HARNESS_FIXED))
    return {name: sha256_file(directory / name) for name in names if (directory / name).is_file()}


# -- sandbox -----------------------------------------------------------------
def sandbox_probe(sandbox=None, timeout_s=10):
    """Run the EPERM + loopback probe under sandbox-exec; (passed, detail)."""
    sandbox = sandbox or SANDBOX
    if platform.system() != "Darwin" or not Path(sandbox).is_file():
        return False, f"sandbox-exec absent ({sandbox}); platform {platform.system()}"
    listener = socket.socket()
    try:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        port = listener.getsockname()[1]
        try:
            done = subprocess.run([sandbox, "-p", PROFILE, sys.executable, "-c", PROBE_SCRIPT, str(port)],
                                  capture_output=True, text=True, timeout=timeout_s, check=False)
        except (OSError, subprocess.SubprocessError) as exc:
            return False, f"probe did not run: {type(exc).__name__}: {exc}"
        try:
            listener.settimeout(0.5)
            connection, _ = listener.accept()
            connection.close()
        except OSError:
            pass
    finally:
        listener.close()
    return _judge_probe(done.stdout.split(), done.returncode, done.stderr)


def _judge_probe(words, returncode, stderr):
    if returncode != 0 or len(words) < 3:
        return False, f"probe exit {returncode}: {' '.join(words)} {stderr[:200]}"
    if words[0] != "OSERROR":
        return False, f"routable connect was not refused: {' '.join(words[:3])}"
    code, elapsed = int(words[1]), int(words[2])
    if code != errno.EPERM:
        return False, f"routable connect failed with errno {code}, not EPERM {errno.EPERM}"
    if elapsed > 500:
        return False, f"EPERM arrived after {elapsed} ms (not immediate)"
    if "LOOPBACK_OK" not in words:
        return False, f"loopback connect failed under the sandbox: {' '.join(words[3:])}"
    return True, f"EPERM in {elapsed} ms; loopback reachable; profile sha256 {sha256_text(PROFILE)[:12]}"


# -- engine artifacts ----------------------------------------------------------
def artifact_check(engine):
    if not isinstance(engine, dict) or not set(ARTIFACT_KEYS) <= set(engine):
        return False, f"engine section lacks {sorted(set(ARTIFACT_KEYS) - set(engine or {}))}"
    pairs = [(engine["binary"], engine["binary_sha256"]), (engine["template"], engine["template_sha256"]),
             (str(Path(engine["model_dir"]) / "model.onnx"), engine["model_onnx_sha256"]),
             (str(Path(engine["model_dir"]) / "tokenizer.json"), engine["tokenizer_sha256"])]
    bad = []
    for path, expected in pairs:
        if not Path(path).is_absolute() or not Path(path).is_file():
            bad.append(f"missing {path}")
        elif sha256_file(path) != expected:
            bad.append(f"sha mismatch {Path(path).name}")
    if bad:
        return False, "; ".join(bad)
    if engine.get("sandbox_profile_sha256") != sha256_text(PROFILE):
        return False, "sandbox_profile_sha256 differs from the harness PROFILE"
    return True, "binary/template/model.onnx/tokenizer.json/sandbox profile match the registry"


def _toml_problems(config, extraction, levers=None):
    def get(*keys):
        node = config
        for key in keys:
            if not isinstance(node, dict) or key not in node:
                return None
            node = node[key]
        return node
    expected = [("knowledge_extraction.enabled", get("knowledge_extraction", "enabled"), True),
                ("knowledge_extraction.max_tokens", get("knowledge_extraction", "max_tokens"), extraction["max_completion_tokens"]),
                ("knowledge_extraction.input_chunk_tokens", get("knowledge_extraction", "input_chunk_tokens"), extraction["input_chunk_tokens"]),
                ("knowledge_extraction.split_on_truncation", get("knowledge_extraction", "split_on_truncation"), extraction["split_on_truncation"]),
                ("knowledge_extraction.max_facts_per_chunk", get("knowledge_extraction", "max_facts_per_chunk"), extraction["max_facts_per_chunk"]),
                ("knowledge_extraction.max_transcript_tokens", get("knowledge_extraction", "max_transcript_tokens"), extraction["max_transcript_tokens"]),
                ("knowledge_extraction.contradiction_check", get("knowledge_extraction", "contradiction_check"), False),
                ("entity_extraction.enabled", get("entity_extraction", "enabled"), False),
                ("llm.provider", get("llm", "provider"), "bench"),
                ("llm.providers.bench.completion_tokens_floor", get("llm", "providers", "bench", "completion_tokens_floor"), extraction["completion_tokens_floor"]),
                ("llm.providers.bench.reasoning_effort", get("llm", "providers", "bench", "reasoning_effort"), "low"),
                ("llm.providers.bench.api_key_env", get("llm", "providers", "bench", "api_key_env"), "LOOMEM_BENCH_PROXY_TOKEN"),
                ("server.host", get("server", "host"), "127.0.0.1"),
                ("search.rerank_enabled", get("search", "rerank_enabled"), False),
                ("search.multi_query_enabled", get("search", "multi_query_enabled"), False),
                ("fragments.enabled", get("fragments", "enabled"), True),
                ("search.evidence_fields", get("search", "evidence_fields"), True),
                ("dream.schedule_enabled", get("dream", "schedule_enabled"), False),
                ("dream.auto_trigger_threshold", get("dream", "auto_trigger_threshold"), 0)]
    from paid_engine_config import DISABLED_SECTIONS, W0_FLAGS
    expected += [(f"{section}.enabled", get(*section.split("."), "enabled"), False) for section in DISABLED_SECTIONS]
    expected += [(f"{section}.{key}", get(section, key), want) for section, key, want in W0_FLAGS]
    problems = [f"{name}={value!r} (expected {want!r})" for name, value, want in expected
                if value != want or type(value) is not type(want)]
    problems += _lever_problems(get, levers)
    port = get("server", "port")
    if port in (3035, 3037, 3038):
        problems.append(f"server.port={port} is a reserved product port")
    return problems


def _lever_problems(get, levers):
    """Every ENGINE_LEVERS entry: a key absent from the TOML counts as its default; a present key must
    equal the requested value (the default when the campaign did not set it) with the same type."""
    from paid_engine_config import ENGINE_LEVERS, EngineError, check_levers
    try:
        levers = check_levers(levers)
    except EngineError as exc:
        return [f"engine.levers rejected: {exc}"]
    problems = []
    for section, key, default in ENGINE_LEVERS:
        want = levers.get(f"{section}.{key}", default)
        value = get(section, key)
        if value is None and want == default and type(want) is type(default):
            continue
        if value != want or type(value) is not type(want):
            problems.append(f"{section}.{key}={value!r} (expected {want!r})")
    return problems


def effective_config_check(engine, extraction):
    """Render the evaluation config at the fingerprint placeholders and assert the TOML; the
    rendered text's sha256 is the engine's ``effective_config_sha256`` and must equal the
    registry's when the registry carries one. ``engine["levers"]`` (campaign.json, only when the
    campaign set a lever) enters the rendering and the assertions."""
    try:
        from paid_engine_config import FINGERPRINT_PORT, FINGERPRINT_PROXY, FINGERPRINT_WORK, evaluation_config
    except ImportError as exc:
        return False, f"paid_engine unavailable: {exc}"
    artifacts = {key: engine[key] for key in ARTIFACT_KEYS}
    levers = engine.get("levers")
    try:
        text = evaluation_config(artifacts, FINGERPRINT_WORK, port=FINGERPRINT_PORT, proxy_url=FINGERPRINT_PROXY,
                                 extraction=extraction, levers=levers)
        config = tomllib.loads(text)
    except Exception as exc:  # any rendering failure is a FAIL with its reason
        return False, f"effective config not renderable: {type(exc).__name__}: {exc}"
    problems = _toml_problems(config, extraction, levers)
    digest = sha256_text(text)
    pinned = engine.get("effective_config_sha256")
    if pinned is not None and pinned != digest:
        problems.append(f"effective_config_sha256 {digest[:12]} differs from the registry {str(pinned)[:12]}")
    if problems:
        return False, "TOML assertions failed: " + "; ".join(problems)
    set_levers = "" if not levers else "; engine levers " + json.dumps(levers, sort_keys=True)
    return True, (f"TOML: NER off, provider bench, extraction {extraction['max_completion_tokens']}/"
                  f"{extraction['input_chunk_tokens']}/split/{extraction['max_facts_per_chunk']}, floor recorded, "
                  f"every mutator section off, fragments+evidence on, dream schedule off, W0 levers off/neutral, port free; effective_config_sha256 {digest[:12]}"
                  + set_levers)


# -- tokenizer -------------------------------------------------------------------
def o200k_counter(env):
    """A ``text -> int`` counter from the cached o200k_base, or raise RuntimeError (no download)."""
    cache = env.get("TIKTOKEN_CACHE_DIR")
    if not cache or not Path(cache).is_dir():
        raise RuntimeError("TIKTOKEN_CACHE_DIR unset or not a directory")
    key = hashlib.sha1(O200K_URL.encode("utf-8")).hexdigest()
    if not (Path(cache) / key).is_file():
        raise RuntimeError(f"o200k_base not in cache {cache} (would require a download)")
    previous = os.environ.get("TIKTOKEN_CACHE_DIR")
    os.environ["TIKTOKEN_CACHE_DIR"] = cache
    try:
        with loopback_only():
            import tiktoken
            encoding = tiktoken.get_encoding("o200k_base")
    except Exception as exc:
        raise RuntimeError(f"tokenizer unavailable: {type(exc).__name__}: {exc}") from exc
    finally:
        if previous is None:
            os.environ.pop("TIKTOKEN_CACHE_DIR", None)
        else:
            os.environ["TIKTOKEN_CACHE_DIR"] = previous
    return lambda text: len(encoding.encode(text, disallowed_special=()))


def transcript_tokens_check(env, holdout, histories, limit, render):
    try:
        counter = o200k_counter(env)
    except RuntimeError as exc:
        return False, f"tokenizer unavailable: {exc}"
    largest, where = 0, None
    for hid in histories:
        for session in holdout.histories[hid]["sessions"]:
            count = counter(render(session))
            if count > largest:
                largest, where = count, f"{hid}/{session['session_id']}"
    if type(limit) is not int or largest > limit:
        return False, f"max_transcript_tokens {limit} < largest session {largest} o200k tokens ({where})"
    return True, f"largest session {largest} o200k tokens ({where}) <= max_transcript_tokens {limit}"


# -- gold leak scan ----------------------------------------------------------------
def _request_texts(raw):
    """The saved request as the provider sees it: every message's DECODED content plus the raw
    file. The file escapes quotes inside content (`\\"reference_answer\\"`), so the JSON-key
    markers only match on the decoded strings."""
    texts = [raw]
    try:
        wire = json.loads(raw)
        for message in wire.get("messages") or []:
            if isinstance(message, dict) and isinstance(message.get("content"), str):
                texts.append(message["content"])
    except (ValueError, AttributeError):
        pass
    return texts


def gold_scan(directory, journal_state, holdout):
    from paid_holdout import KEY_MARKERS, PRIVATE_FIELDS
    markers, excluded = list(KEY_MARKERS), {field: 0 for field in PRIVATE_FIELDS}
    for qid in holdout.questions:
        markers.extend(holdout.gold_markers(qid))
        for field, counts in holdout.gold_marker_report(qid).items():
            excluded[field] += counts["excluded_as_source"]
    leaks, scanned = [], 0
    for rid, intent in journal_state.intents.items():
        if intent.get("stage") in GOLD_STAGES:
            continue
        path = Path(directory) / "calls" / f"{rid}.request.json"
        if not path.is_file():
            continue
        scanned += 1
        hits = set()
        for text in _request_texts(path.read_text(encoding="utf-8")):
            hits.update(leaks_gold(text, markers))
        if hits:
            leaks.append(f"{rid[:12]} ({intent.get('stage')}): {len(hits)} marker(s)")
    skipped = ", ".join(f"{field}={n}" for field, n in excluded.items() if n)
    if leaks:
        return False, "gold markers in non-judge requests: " + "; ".join(leaks[:5])
    return True, (f"{scanned} non-judge request(s) scanned against {len(markers)} gold marker(s) "
                  f"(incl. {len(KEY_MARKERS)} key names): 0 hits; excluded as source text: {skipped or 'none'}")


def write_json(path, obj):
    Path(path).write_text(json.dumps(obj, sort_keys=True, indent=1) + "\n", encoding="utf-8")
