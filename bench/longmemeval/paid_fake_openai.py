"""Loopback stand-in for the OpenAI chat-completions endpoint (tests and dry-run only).

Binds 127.0.0.1 on a random port, never anything else. Each request is logged
and answered according to a per-role scenario; the role is resolved from the
system-message SHA-256 (``role_by_system_sha256``), else from the wire model
alias when it names exactly one role, else ``default``.

Scenarios: ok | empty_facts | length | http500 | timeout | model_drift |
missing_usage | malformed_json | bad_request_400 | usage_over_bound | redirect |
capped | capped_failed_attempt | length_http400 | rejected_4xx.
``rejected_4xx`` answers HTTP 429 with OpenAI's ``insufficient_quota`` error (``INSUFFICIENT_QUOTA_429``).
``length_http400`` answers a strict json_schema request with OpenAI's output-limit HTTP 400
(``OUTPUT_LIMIT_400``) and any other request with an ordinary ``length``.
``timeout`` closes the connection without any response, which is what a client
sees as an unknown outcome; no real sleeping happens anywhere.
Usage is deterministic from byte counts (tokens <= bytes, as P1 assumes).
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
import socket
import threading

SCENARIOS = ("ok", "empty_facts", "length", "http500", "timeout", "model_drift",
             "missing_usage", "malformed_json", "bad_request_400", "usage_over_bound", "redirect",
             "capped", "capped_failed_attempt", "length_http400", "rejected_4xx")
CAPPED_FACTS = 22  # above the frozen max_facts_per_chunk = 20: the engine keeps 20 and discards 2
PATH = "/v1/chat/completions"
# Verbatim error OpenAI returned for a strict json_schema selection at max_completion_tokens 512 (holdout r2).
OUTPUT_LIMIT_400 = {"error": {"message": "Could not finish the message because max_tokens or model output limit "
                                         "was reached. Please try again with higher max_tokens.",
                              "type": "invalid_request_error", "param": None, "code": None}}
# The shape OpenAI returns when the account's credit is exhausted (holdout r3 stopped on it, 2026-11-25).
INSUFFICIENT_QUOTA_429 = {"error": {"message": "You exceeded your current quota, please check your plan and "
                                               "billing details.",
                                    "type": "insufficient_quota", "param": None, "code": "insufficient_quota"}}
# OpenAI Responses API token counter (developers.openai.com/api/docs/api-reference/
# responses/input-tokens, read 2026-11-22): POST {model, input} ->
# {"object": "response.input_tokens", "input_tokens": N}. The fake counts with the
# same deterministic rule as `usage.prompt_tokens`, so a preflight count equals the
# billed prompt size of the matching chat call; `count_over_bound` (per role, via
# `count_scenarios`) answers with an absurd count to exercise the pre-send stop.
INPUT_TOKENS_PATH = "/v1/responses/input_tokens"


def _tokens(data):
    return len(data.encode("utf-8")) // 4 + 1


def _first_ids(user, key, limit):
    try:
        records = json.loads(user).get(key) or []
    except (ValueError, AttributeError):
        return []
    return [r["record_id"] for r in records[:limit] if isinstance(r, dict) and "record_id" in r]


def content_for(role, user, scenario):
    """Schema-shaped text per stage; `empty_facts` yields an extraction with zero facts, `capped` one with
    CAPPED_FACTS distinct facts, `capped_failed_attempt` the same plus a fake-engine-only outcome override."""
    if role == "extraction":
        fact = {"content": "A synthetic fact.", "fact_type": "fact", "subject": None, "event_date": None,
                "event_date_context": None, "confidence": 0.9, "topic": None, "attributed_to": None}
        if scenario in ("capped", "capped_failed_attempt"):
            body = {"facts": [{**fact, "content": f"Synthetic fact {i}."} for i in range(CAPPED_FACTS)]}
            if scenario == "capped_failed_attempt":
                body["_fake_outcome"] = {"failed_attempts": 1}
            return json.dumps(body)
        return json.dumps({"facts": [] if scenario == "empty_facts" else [fact]})
    if role == "diagnostic":
        return json.dumps({"answer": "Synthetic answer.", "coverage": "complete", "missing_parts": [],
                           "conflict_fact_ids": [], "supporting_fact_ids": _first_ids(user, "facts", 1)})
    if role == "audit":
        return json.dumps({"claims_entailed": True, "requested_scope_covered": True,
                           "internally_consistent": True, "freshness_supported": True, "issues": []})
    if role == "selection":
        return json.dumps({"selected_ids": _first_ids(user, "original_records", 16)})
    if role == "judge":
        try:
            candidate = json.loads(user).get("candidate", "")
        except (ValueError, AttributeError):
            candidate = ""
        return json.dumps({"status": "scored", "correct": True, "answer_span": candidate[:20],
                           "reason": "synthetic verdict"})
    if role == "review":
        return json.dumps(_review_for(user))
    return "Synthetic answer."


def _review_for(user):
    """A valid plan §8C review: agree with a scored judge, else propose correct; quote the reference."""
    try:
        payload = json.loads(user)
    except ValueError:
        payload = {}
    score = (payload.get("judge_verdict") or {}).get("score")
    reference = (payload.get("reference") or {}).get("reference_answer")
    quote = reference[:20] if isinstance(reference, str) else ""
    quote = quote if quote.strip() else ""
    evidence = {"kind": "reference_atom" if quote else "none", "source_id": None, "quote": quote}
    agrees = score in ("correct", "incorrect")
    return {"agrees_with_judge": agrees, "proposed_correction": "keep" if agrees else "correct",
            "critical_error_types": [], "evidence": evidence, "rationale": "synthetic review"}


class FakeOpenAI:
    def __init__(self, scenarios, *, model_by_role, role_by_system_sha256=None, token=None,
                 count_scenarios=None):
        flat = [s for value in scenarios.values() for s in (value if isinstance(value, list) else [value])]
        unknown = set(flat) - set(SCENARIOS)
        if unknown or any(isinstance(v, list) and not v for v in scenarios.values()):
            raise ValueError(f"unknown or empty scenarios {sorted(unknown)}")
        if any(v not in ("ok", "count_over_bound") for v in (count_scenarios or {}).values()):
            raise ValueError("count_scenarios values must be ok or count_over_bound")
        self.scenarios = dict(scenarios)
        self.count_scenarios = dict(count_scenarios or {})
        self.model_by_role = dict(model_by_role)
        self.role_by_system = dict(role_by_system_sha256 or {})
        self.token = token
        self.log = []
        self.count_log = []
        self.raw_log = []  # exact request bytes per path and role (blindness tests, evidence)
        self._lock = threading.Lock()
        self._server = None
        self._thread = None

    # -- lifecycle -----------------------------------------------------
    def __enter__(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                fake._handle(self)

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request, client_address):
                pass  # a deliberately dropped connection is a scenario, not a server bug

        self._server = Server(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.02},
                                        daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *_):
        self._server.shutdown()
        self._server.server_close()
        self._thread.join()

    @property
    def url(self):
        return f"http://127.0.0.1:{self._server.server_address[1]}/v1"

    # -- role and scenario resolution ----------------------------------
    def role_of(self, wire):
        system = wire["messages"][0]["content"]
        digest = hashlib.sha256(system.encode("utf-8")).hexdigest()
        if digest in self.role_by_system:
            return self.role_by_system[digest]
        # The engine's extraction prompt embeds the session's conversation_date; the
        # proxy freezes the date-normalised fingerprint, so a registration under that
        # value must match every session (lazy import: the proxy imports this module's peers).
        from paid_provider_proxy import system_fingerprint
        normalised = system_fingerprint(system)
        if normalised in self.role_by_system:
            return self.role_by_system[normalised]
        matches = [r for r, m in self.model_by_role.items() if m == wire.get("model")]
        if len(matches) == 1:
            return matches[0]
        return "default"

    def scenario_of(self, role):
        value = self.scenarios.get(role, self.scenarios.get("default", "ok"))
        if isinstance(value, list):  # FIFO queue; the last entry repeats
            value = value.pop(0) if len(value) > 1 else value[0]
        return value

    # -- request handling ------------------------------------------------
    def _handle(self, handler):
        if self.token is not None and handler.headers.get("Authorization") != "Bearer " + self.token:
            return _send(handler, 401, {"error": {"message": "invalid token", "type": "invalid_request_error"}})
        if handler.path not in (PATH, INPUT_TOKENS_PATH):
            return _send(handler, 404, {"error": {"message": "not found", "type": "invalid_request_error"}})
        length = int(handler.headers.get("Content-Length") or 0)
        raw = handler.rfile.read(length)
        try:
            wire = json.loads(raw.decode("utf-8"))
        except ValueError:
            return _send(handler, 400, {"error": {"message": "invalid json", "type": "invalid_request_error"}})
        if handler.path == INPUT_TOKENS_PATH:
            return self._count(handler, wire, raw)
        if "max_tokens" in wire or "max_completion_tokens" not in wire or "reasoning_effort" not in wire:
            return _send(handler, 400, {"error": {"message": "legacy or incomplete wire", "type": "invalid_request_error"}})
        role = self.role_of(wire)
        with self._lock:
            scenario = self.scenario_of(role)
            self.log.append({"role": role, "scenario": scenario, "wire": wire, "bytes": len(raw)})
            self.raw_log.append({"path": PATH, "role": role, "raw": raw})
        self._respond(handler, role, scenario, wire)

    def _count(self, handler, body, raw=b""):
        """Responses token counter: `input` is a string or a list of {role, content} items."""
        items = body.get("input")
        if isinstance(items, str):
            items = [{"role": "user", "content": items}]
        if (not isinstance(body.get("model"), str) or not isinstance(items, list)
                or any(not isinstance(i, dict) or not isinstance(i.get("content"), str) for i in items)):
            return _send(handler, 400, {"error": {"message": "invalid input", "type": "invalid_request_error"}})
        role = self.role_of({"messages": [{"content": items[0]["content"]}], "model": body["model"]}) if items else "default"
        count = sum(_tokens(i["content"]) for i in items)
        if self.count_scenarios.get(role, self.count_scenarios.get("default", "ok")) == "count_over_bound":
            count = 10 ** 7
        with self._lock:
            self.count_log.append({"role": role, "model": body["model"], "input_tokens": count})
            self.raw_log.append({"path": INPUT_TOKENS_PATH, "role": role, "raw": raw})
        return _send(handler, 200, {"object": "response.input_tokens", "input_tokens": count})

    def _respond(self, handler, role, scenario, wire):
        if scenario == "http500":
            return _send(handler, 500, {"error": {"message": "upstream failure", "type": "server_error"}})
        if scenario == "bad_request_400":
            return _send(handler, 400, {"error": {"message": "rejected", "type": "invalid_request_error"}})
        if scenario == "rejected_4xx":
            return _send(handler, 429, INSUFFICIENT_QUOTA_429, {"x-request-id": f"fake-req-{len(self.log)}"})
        if scenario == "length_http400":
            response_format = wire.get("response_format") or {}
            if response_format.get("type") == "json_schema" and (response_format.get("json_schema") or {}).get("strict"):
                return _send(handler, 400, OUTPUT_LIMIT_400, {"x-request-id": f"fake-req-{len(self.log)}"})
            scenario = "length"
        if scenario == "timeout":
            # No response at all: the client sees a closed connection (unknown outcome).
            handler.close_connection = True
            handler.connection.shutdown(socket.SHUT_RDWR)
            return None
        if scenario == "malformed_json":
            return _send_raw(handler, 200, b'{"choices": [')
        if scenario == "redirect":  # a compliant client must refuse to follow it
            return _send_raw(handler, 302, b"", {"Location": "http://127.0.0.1:9/v1/chat/completions"})
        system, user = wire["messages"][0]["content"], wire["messages"][1]["content"]
        content = "" if scenario == "length" else content_for(role, user, scenario)
        model = self.model_by_role.get(role, wire["model"])
        if scenario == "model_drift":
            model += "-drifted"
        prompt = 10 ** 7 if scenario == "usage_over_bound" else _tokens(system) + _tokens(user)
        completion = wire["max_completion_tokens"] if scenario == "length" else _tokens(content)
        body = {"id": f"chatcmpl-fake-{len(self.log)}", "object": "chat.completion", "model": model,
                "service_tier": "default",
                "choices": [{"index": 0, "finish_reason": "length" if scenario == "length" else "stop",
                             "message": {"role": "assistant", "content": content}}],
                "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
                          "total_tokens": prompt + completion,
                          "completion_tokens_details": {"reasoning_tokens": 0},
                          "prompt_tokens_details": {"cached_tokens": 0}}}
        if scenario == "missing_usage":
            del body["usage"]
        return _send(handler, 200, body, {"x-request-id": f"fake-req-{len(self.log)}"})


def _send(handler, status, obj, extra_headers=None):
    _send_raw(handler, status, json.dumps(obj).encode("utf-8"), extra_headers)


def _send_raw(handler, status, data, extra_headers=None):
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(data)))
    for key, value in (extra_headers or {}).items():
        handler.send_header(key, value)
    handler.end_headers()
    handler.wfile.write(data)
