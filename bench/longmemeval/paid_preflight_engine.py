"""Live engine probe for preflight (0 USD, loopback only): the binary is an evaluation build,
the engine's real extraction system prompt fits the sheet's ``SYSTEM["extraction"]`` (otherwise
the proxy's P1 wire check refuses the very first paid extraction request), and the prompt's
date-normalised fingerprint equals the campaign pin ``extraction_system_sha256`` (recorded by
the dry-run, passed to ``create-campaign --extraction-system-sha256``).

A throw-away engine is started on a temporary work directory whose "provider" is a capture
server that records the request and answers HTTP 400, so nothing is ever forwarded; the probe
transcript is synthetic and never scored. ``engine_factory(proxy_url, proxy_token)`` is
injectable for tests; the engine must expose ``readiness_probe()`` (HTTP status of a bad
``ingest-readiness-v2`` body), ``start``, ``ingest_session`` and ``close``.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import secrets
import tempfile
import threading
from pathlib import Path

from paid_openai_wire import build_wire, wire_bytes
from paid_provider_proxy import system_fingerprint

PROBE_SESSION = {"session_id": "preflight-probe", "conversation_date": "2026-01-01",
                 "turns": [{"role": "user", "content": "Preflight probe transcript. It is synthetic and never scored."},
                           {"role": "assistant", "content": "Noted; nothing to remember."}]}
ARTIFACT_KEYS = ("binary", "binary_sha256", "template", "template_sha256", "model_dir", "model_onnx_sha256",
                 "tokenizer_sha256")
EVALUATION_REJECTIONS = (400, 422)   # what the evaluation route answers to a deliberately bad body


class CaptureProvider:
    """Loopback stand-in for the proxy: records every POST body, answers 400 (never forwards)."""

    def __init__(self):
        self.bodies, self._lock = [], threading.Lock()
        self._server = self._thread = None

    def __enter__(self):
        capture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    length = int(self.headers.get("Content-Length") or 0)
                except ValueError:
                    length = 0
                raw = self.rfile.read(max(length, 0))
                with capture._lock:
                    capture.bodies.append(raw)
                data = json.dumps({"error": {"message": "preflight probe: never forwarded",
                                             "type": "invalid_request_error"}}).encode("utf-8")
                self.send_response(400)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request, client_address):
                pass

        self._server = Server(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *_):
        self._server.shutdown()
        self._server.server_close()
        self._thread.join()

    @property
    def url(self):
        return f"http://127.0.0.1:{self._server.server_address[1]}/v1"


def default_engine_factory(spec):
    def factory(proxy_url, proxy_token):
        from paid_engine import EvaluationEngine
        artifacts = {k: spec["engine"][k] for k in ARTIFACT_KEYS}
        return EvaluationEngine(artifacts, proxy_url=proxy_url, proxy_token=proxy_token,
                                extraction=spec["extraction"], top_k=spec["search"]["top_k"],
                                extraction_timeout_s=int(spec["timeout_s"]))
    return factory


def _extraction_overhead(body, profile):
    """Wire bytes of the engine's request minus its user content: what the proxy's P1 check compares
    with the sheet's SYSTEM["extraction"] (wire + FRAMING <= SYSTEM + user bytes + FRAMING)."""
    messages = body.get("messages")
    if not isinstance(messages, list) or len(messages) != 2:
        raise ValueError("extraction request does not carry exactly two messages")
    clean = [{"role": m["role"], "content": m["content"]} for m in messages]
    return len(wire_bytes(build_wire(profile, clean))) - len(clean[1]["content"].encode("utf-8"))


def probe_engine(spec, sheet, *, mode, engine_factory=None):
    """Return {"passed", "problems", "observed"}; never raises for an engine failure (it is a FAIL)."""
    factory = engine_factory or default_engine_factory(spec)
    observed, problems = {}, []
    with tempfile.TemporaryDirectory(prefix="loomem-preflight-probe-") as directory, CaptureProvider() as capture:
        engine = factory(capture.url, secrets.token_hex(32))
        try:
            engine.start(Path(directory) / "engine", reuse=False)
            status = engine.readiness_probe()
            observed["readiness_bad_body_status"] = status
            # axum rejects a body that fails deserialisation with 422 (400 for malformed JSON);
            # only a build without `--features evaluation` has no such route at all (404).
            if status == 404:
                problems.append("binary is not an evaluation build (ingest-readiness-v2 answers 404)")
            elif status not in EVALUATION_REJECTIONS:
                problems.append(f"ingest-readiness-v2 answered HTTP {status} to a bad body (expected 400/422)")
            try:
                engine.ingest_session(PROBE_SESSION)
            except Exception:  # the capture provider answers 400: the engine's extraction fails by design
                pass
        except Exception as exc:
            problems.append(f"engine probe failed: {type(exc).__name__}: {exc}")
        finally:
            engine.close()
        bodies = list(capture.bodies)
    if not bodies:
        problems.append("the engine made no extraction request to the provider")
        return {"passed": False, "problems": problems, "observed": observed}
    try:
        body = json.loads(bodies[0].decode("utf-8"))
        overhead = _extraction_overhead(body, spec["profiles"]["extraction"])
        fingerprint = system_fingerprint(body["messages"][0]["content"])
    except (ValueError, KeyError, TypeError) as exc:
        problems.append(f"extraction request unreadable: {type(exc).__name__}: {exc}")
        return {"passed": False, "problems": problems, "observed": observed}
    budget = int(sheet.module.SYSTEM["extraction"])
    observed.update(extraction_requests=len(bodies), system_sha256=fingerprint,
                    system_bytes=len(body["messages"][0]["content"].encode("utf-8")),
                    wire_overhead_bytes=overhead, sheet_system_extraction=budget)
    if overhead > budget:
        problems.append(f"extraction wire overhead {overhead} B exceeds the sheet's SYSTEM[\"extraction\"] {budget}: "
                        "the proxy would refuse every extraction request (raise the sheet entry)")
    pin = spec.get("extraction_system_sha256")
    if pin is None:
        if mode == "paid":
            problems.append("extraction_system_sha256 is not pinned in campaign.json (pass "
                            "--extraction-system-sha256 from the dry-run)")
    elif pin != fingerprint:
        problems.append(f"extraction system fingerprint {fingerprint[:12]} differs from the pin {str(pin)[:12]}")
    return {"passed": not problems, "problems": problems, "observed": observed}


def chunking_matches_sheet(sheet, extraction):
    """Sheet E_CHUNK_TOKENS must equal what the engine actually chunks by: min(input_chunk_tokens, max_transcript_tokens)."""
    engine_chunk = min(int(extraction["input_chunk_tokens"]), int(extraction["max_transcript_tokens"]))
    sheet_chunk = int(sheet.module.E_CHUNK_TOKENS)
    if sheet_chunk != engine_chunk:
        return False, f"sheet E_CHUNK_TOKENS {sheet_chunk} != engine chunk tokens {engine_chunk}"
    return True, f"sheet E_CHUNK_TOKENS {sheet_chunk} == min(input_chunk_tokens, max_transcript_tokens)"


def format_probe(result):
    observed = result["observed"]
    if result["problems"]:
        return "; ".join(result["problems"])
    return (f"evaluation build (bad readiness body -> {observed['readiness_bad_body_status']}); "
            f"extraction system {observed['system_bytes']} B, "
            f"wire overhead {observed['wire_overhead_bytes']} <= SYSTEM[\"extraction\"] {observed['sheet_system_extraction']}; "
            f"fingerprint {observed['system_sha256'][:12]}")
