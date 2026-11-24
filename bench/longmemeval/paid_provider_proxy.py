"""Loopback provider proxy between the evaluation engine and the paid wire.

The engine's ``[llm.providers.bench]`` entry points at this proxy, so every
LLM request the engine makes (knowledge extraction, nothing else is enabled)
passes through ``paid_completion`` and is reserved in the durable ledger
BEFORE the upstream provider sees it. Only the frozen extraction profile is
admitted: model, ``reasoning_effort``, ``max_completion_tokens`` exactly as
the campaign profile, native keys only (legacy ``max_tokens`` / ``temperature``
are refused), two messages, and one system fingerprint frozen on the first
valid request. Anything else — another prompt (NER, consolidation, ...), a
request outside a bound session window or beyond its cap, a concurrent
request, a body over the cap — halts the proxy permanently: every later
request is answered 400, which the engine records as a failed chunk.

The fingerprint normalises the one line of the extraction system prompt that
``memory_extractor.rs`` substitutes per session (``The conversation_date is:
<date>``), so the same prompt is recognised across sessions while any other
system text differs.
"""
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import re
import secrets
import threading
import time

from budget import BudgetError
from paid_call_journal import JournalError, sha256_hex
from paid_campaign import CampaignError
from paid_openai_wire import BODY_CAP, RunStopped, WireError, paid_completion
from paid_price_sheet import SheetError

KEY_ENV = "LOOMEM_BENCH_PROXY_TOKEN"
PATH = "/v1/chat/completions"
NATIVE_KEYS = frozenset({"model", "messages", "max_completion_tokens", "reasoning_effort"})
LEGACY_KEYS = frozenset({"max_tokens", "temperature"})
PROFILE_KEYS = frozenset({"model", "reasoning_effort", "max_completion_tokens"})
DATE_LINE = re.compile(r"^The conversation_date is: (?:\d{4}-\d{2}-\d{2}|unknown)$", re.M)
DATE_PLACEHOLDER = "The conversation_date is: {conversation_date}"
MONEY_PATH_ERRORS = (RunStopped, BudgetError, CampaignError, WireError, JournalError, SheetError)


class ProxyHalted(RuntimeError):
    """The proxy refused a request and will refuse every later one."""


def system_fingerprint(system):
    """SHA-256 of the system prompt with its per-session date line normalised."""
    return sha256_hex(DATE_LINE.sub(DATE_PLACEHOLDER, system, count=1).encode("utf-8"))


def _decode(body, body_cap):
    if isinstance(body, (bytes, bytearray)):
        if len(body) > body_cap:
            raise ValueError(f"request body exceeds {body_cap} bytes")
        try:
            body = json.loads(bytes(body).decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise ValueError("request body is not JSON") from exc
    if not isinstance(body, dict):
        raise ValueError("request body is not an object")
    return body


def validate_extraction_body(body, profile, system_sha256, body_cap=BODY_CAP):
    """Admit only the frozen extraction request; return (messages, system fingerprint)."""
    if not isinstance(profile, dict) or set(profile) != PROFILE_KEYS:
        raise ValueError("extraction profile must have exactly model, reasoning_effort, max_completion_tokens")
    body = _decode(body, body_cap)
    legacy = LEGACY_KEYS & set(body)
    if legacy:
        raise ValueError(f"legacy wire keys {sorted(legacy)}: the engine must send max_completion_tokens without temperature")
    if set(body) != NATIVE_KEYS:
        raise ValueError(f"wire keys {sorted(body)} differ from {sorted(NATIVE_KEYS)}")
    if body["model"] != profile["model"]:
        raise ValueError(f"model {body['model']!r} differs from the extraction profile {profile['model']!r}")
    limit = body["max_completion_tokens"]
    if type(limit) is not int or limit != profile["max_completion_tokens"]:
        raise ValueError(f"max_completion_tokens {limit!r} differs from the profile {profile['max_completion_tokens']}")
    if body["reasoning_effort"] != profile["reasoning_effort"]:
        raise ValueError(f"reasoning_effort {body['reasoning_effort']!r} differs from the profile")
    messages = body["messages"]
    if not isinstance(messages, list) or len(messages) != 2:
        raise ValueError("exactly two messages (system, user) are allowed")
    for message, role in zip(messages, ("system", "user")):
        if (not isinstance(message, dict) or set(message) != {"role", "content"}
                or message["role"] != role or not isinstance(message["content"], str)):
            raise ValueError("message shape must be {role, content:str} in system,user order")
    fingerprint = system_fingerprint(messages[0]["content"])
    if system_sha256 is not None and fingerprint != system_sha256:
        raise ValueError("system prompt differs from the frozen extraction fingerprint")
    return [{"role": m["role"], "content": m["content"]} for m in messages], fingerprint


def _error_body(message, kind):
    return {"error": {"message": message, "type": kind}}


def _send(handler, status, obj):
    data = json.dumps(obj, ensure_ascii=False).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(data)))
    handler.end_headers()
    handler.wfile.write(data)


class ProviderProxy:
    def __init__(self, campaign, journal, sheet, transport, *, extraction_profile, manifest_sha256,
                 system_sha256=None, precount=None, acknowledged=frozenset(), clock=time.time):
        if not isinstance(extraction_profile, dict) or set(extraction_profile) != PROFILE_KEYS:
            raise ValueError("extraction_profile must have exactly model, reasoning_effort, max_completion_tokens")
        if not isinstance(manifest_sha256, str) or len(manifest_sha256) != 64:
            raise ValueError("manifest_sha256 must be a 64-hex SHA-256")
        self.campaign, self.journal, self.sheet, self.transport = campaign, journal, sheet, transport
        self.profile = dict(extraction_profile)
        self.manifest_sha256 = manifest_sha256
        self.system_sha256 = system_sha256
        self.precount, self.acknowledged, self.clock = precount, frozenset(acknowledged), clock
        self.token = secrets.token_hex(32)
        self.key_env = KEY_ENV
        self.halted, self.halt_reason = False, None
        self.records = []
        self._window, self._session_records = None, []
        self._busy = threading.Lock()
        self._state = threading.RLock()
        self._server = self._thread = None

    # -- lifecycle -----------------------------------------------------------
    def __enter__(self):
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                proxy._handle(self)

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request, client_address):
                pass  # a client that drops the connection is not a proxy fault

        self._server = Server(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.02},
                                        daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *_):
        # Never tear the server down under a request in flight: a paid call that has
        # reached `sent` must settle (or be recorded uncertain) before the socket goes.
        grace = float(self.campaign.spec.get("timeout_s", 0)) + 5.0
        acquired = self._busy.acquire(timeout=grace)
        try:
            self._server.shutdown()
            self._server.server_close()
            self._thread.join()
        finally:
            if acquired:
                self._busy.release()

    @property
    def url(self):
        return f"http://127.0.0.1:{self._server.server_address[1]}/v1"

    # -- session windows -----------------------------------------------------
    def bind(self, history_id, session_id, max_requests):
        if type(max_requests) is not int or max_requests < 1:
            raise ValueError("max_requests must be a positive int")
        if not history_id or not session_id:
            raise ValueError("history_id and session_id are required")
        with self._state:
            if self._window is not None:
                raise ValueError("a session window is already bound; unbind() first")
            self._window = {"history_id": history_id, "session_id": session_id,
                            "max_requests": max_requests, "ordinal": 0}
            self._session_records = []

    def unbind(self):
        with self._state:
            records, self._session_records = self._session_records, []
            self._window = None
            return records

    def _halt(self, reason):
        with self._state:
            if not self.halted:
                self.halted, self.halt_reason = True, reason

    def _record(self, ordinal, outcome, *, request_id=None, usage=None, wire_bytes=0, error=None):
        with self._state:
            window = self._window or {}
            record = {"history_id": window.get("history_id"), "session_id": window.get("session_id"),
                      "ordinal": ordinal, "request_id": request_id, "outcome": outcome, "usage": usage,
                      "wire_bytes": wire_bytes, "system_sha256": self.system_sha256,
                      "error": None if error is None else f"{type(error).__name__}: {error}"}
            self.records.append(record)
            if self._window is not None:
                self._session_records.append(record)
            return record

    # -- requests ------------------------------------------------------------
    def _handle(self, handler):
        if not self._busy.acquire(blocking=False):
            reason = ProxyHalted("concurrent request: the engine must call the provider serially")
            self._halt(reason)
            self._record(None, "halted", error=reason)
            return _send(handler, 400, _error_body(str(reason), "invalid_request_error"))
        try:
            return self._serve(handler)
        finally:
            self._busy.release()

    def _refuse(self, handler, ordinal, reason, wire_bytes=0):
        """Non-conforming traffic: halt permanently and answer 400."""
        self._halt(reason)
        self._record(ordinal, "halted", wire_bytes=wire_bytes, error=reason)
        handler.close_connection = True
        return _send(handler, 400, _error_body(str(reason), "invalid_request_error"))

    def _serve(self, handler):
        bearer = handler.headers.get("Authorization") or ""
        if not hmac.compare_digest(bearer.encode("utf-8"), ("Bearer " + self.token).encode("utf-8")):
            self._record(None, "unauthorized", error=ValueError("bad bearer token"))
            return _send(handler, 401, _error_body("invalid token", "invalid_request_error"))
        if self.halted:
            self._record(None, "refused_after_halt", error=self.halt_reason)
            return _send(handler, 400, _error_body(f"proxy halted: {self.halt_reason}", "invalid_request_error"))
        if handler.path != PATH:
            return self._refuse(handler, None, ProxyHalted(f"unexpected path {handler.path!r}"))
        try:
            length = int(handler.headers.get("Content-Length") or 0)
        except ValueError:
            length = -1
        if length < 0:
            return self._refuse(handler, None, ProxyHalted("Content-Length is not a nonnegative integer"))
        if length > BODY_CAP:
            return self._refuse(handler, None, ProxyHalted(f"request body {length} bytes exceeds the cap"))
        raw = handler.rfile.read(length)
        with self._state:
            window = self._window
            if window is None:
                return self._refuse(handler, None, ProxyHalted("request outside a bound session window"), len(raw))
            ordinal, window["ordinal"] = window["ordinal"], window["ordinal"] + 1
            if ordinal >= window["max_requests"]:
                return self._refuse(handler, ordinal, ProxyHalted(
                    f"request {ordinal} beyond max_requests {window['max_requests']} of session {window['session_id']}"), len(raw))
            try:
                messages, fingerprint = validate_extraction_body(raw, self.profile, self.system_sha256)
            except ValueError as exc:
                return self._refuse(handler, ordinal, ProxyHalted(f"non-conforming extraction request: {exc}"), len(raw))
            if self.system_sha256 is None:
                self.system_sha256 = fingerprint
            return self._forward(handler, window, ordinal, messages, len(raw))

    def _forward(self, handler, window, ordinal, messages, wire_bytes):
        user = messages[1]["content"]
        binding = {"history_id": window["history_id"], "session_id": window["session_id"], "question_id": None,
                   "arm": None, "stage": "extraction", "attempt": 1, "ordinal": ordinal,
                   "manifest_sha256": self.manifest_sha256, "context_sha256": sha256_hex(user.encode("utf-8")),
                   "prompt_sha256": self.system_sha256}
        try:
            bound = self.sheet.extraction_bound(len(user.encode("utf-8")))
            completion = paid_completion(
                self.campaign, self.journal, self.sheet, letter="E", stage="extraction", binding=binding,
                messages=messages, response_format=None, transport=self.transport, bound=bound,
                acknowledged=self.acknowledged, clock=self.clock, precount=self.precount)
        except MONEY_PATH_ERRORS as exc:
            return self._stop(handler, ordinal, exc, wire_bytes)
        except Exception as exc:  # fail closed on anything unexpected, never answer a fabricated completion
            return self._stop(handler, ordinal, exc, wire_bytes)
        if completion.usage is None:  # output-limit HTTP 400 settled as length: no completion to forward
            return self._stop(handler, ordinal, ProxyHalted(
                f"request {completion.request_id} ended as an output-limit HTTP 400 (length, no usage)"), wire_bytes)
        self._record(ordinal, completion.outcome, request_id=completion.request_id, usage=completion.usage,
                     wire_bytes=wire_bytes)
        return _send(handler, 200, self._chat_completion(completion))

    def _stop(self, handler, ordinal, exc, wire_bytes):
        """The paid path refused or stopped: keep the exception, halt, answer 502."""
        self._halt(exc)
        self._record(ordinal, "halted", wire_bytes=wire_bytes, error=exc)
        handler.close_connection = True
        return _send(handler, 502, _error_body(f"{type(exc).__name__}: {exc}", type(exc).__name__))

    def _chat_completion(self, completion):
        usage = completion.usage
        return {"id": "chatcmpl-proxy-" + completion.request_id[:24], "object": "chat.completion",
                "created": int(self.clock()), "model": completion.model,
                "choices": [{"index": 0, "finish_reason": completion.finish_reason,
                             "message": {"role": "assistant", "content": completion.text}}],
                "usage": {"prompt_tokens": usage["prompt_tokens"], "completion_tokens": usage["completion_tokens"],
                          "total_tokens": usage["total_tokens"]}}
