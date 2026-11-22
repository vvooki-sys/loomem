"""OpenAI chat-completions wire: build, check, transport, parse, and the one paid call.

``paid_completion`` is the only function that may send money-costing traffic.
Its order is fixed (plan §2): check wire -> sublimit -> ``ledger.reserve`` ->
``journal.intent`` -> ``journal.sent`` -> HTTP -> parse -> pin model ->
``journal.settled`` + ``ledger.settle`` -> bound check. Nothing is retried. A
request whose outcome is unknown (timeout, connection error, 5xx, unreadable
body, missing usage) is recorded ``uncertain``, keeps its full reservation and
stops the run; the next call refuses until the ID is explicitly acknowledged.
"""
from dataclasses import dataclass
from decimal import Decimal
import json
import os
import socket
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

from budget import BudgetError
from paid_call_journal import ModelDrift, canonical_bytes, request_id, sha256_hex
from paid_price_sheet import PriceUnknown

PROFILE_KEYS = frozenset({"model", "reasoning_effort", "max_completion_tokens"})
FORBIDDEN_WIRE_KEYS = frozenset({"max_tokens", "temperature", "seed", "top_p", "n", "tools",
                                 "functions", "stream_options", "logprobs"})
LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1"})
BODY_CAP = 4 * 1024 * 1024


class WireError(ValueError):
    """The request or response is malformed; refused before money or recorded uncertain."""


class RunStopped(RuntimeError):
    """A fail-closed stop: uncertain request, frozen input changed, provider rejection."""


class Rejected4xx(RunStopped):
    """The provider rejected the request with an error object (no charge recorded)."""


class BoundBreach(RunStopped):
    """Billed usage exceeded R_i/O_i; the call is paid and recorded, the run stops."""


@dataclass(frozen=True)
class Completion:
    request_id: str
    text: str
    usage: dict
    finish_reason: str
    model: str
    provider_request_id: str | None
    outcome: str            # ok | length
    actual_usd: str
    replayed: bool = False


# -- building ------------------------------------------------------------
def build_wire(profile, messages, response_format=None):
    if not isinstance(profile, dict) or set(profile) != PROFILE_KEYS:
        raise WireError(f"profile must have exactly {sorted(PROFILE_KEYS)}")
    if type(profile["max_completion_tokens"]) is not int or profile["max_completion_tokens"] <= 0:
        raise WireError("max_completion_tokens must be a positive int")
    if profile["reasoning_effort"] not in ("low", "medium", "high"):
        raise WireError("reasoning_effort must be low|medium|high")
    if not isinstance(messages, list) or len(messages) != 2:
        raise WireError("exactly two messages (system, user) are allowed")
    for message, role in zip(messages, ("system", "user")):
        if (not isinstance(message, dict) or set(message) != {"role", "content"}
                or message["role"] != role or not isinstance(message["content"], str)):
            raise WireError("message shape must be {role, content:str} in system,user order")
    wire = {"model": profile["model"],
            "messages": [{"role": m["role"], "content": m["content"]} for m in messages],
            "max_completion_tokens": profile["max_completion_tokens"],
            "reasoning_effort": profile["reasoning_effort"],
            "service_tier": "default", "stream": False, "store": False}
    if response_format is not None:
        if not isinstance(response_format, dict):
            raise WireError("response_format must be an object")
        wire["response_format"] = json.loads(json.dumps(response_format, allow_nan=False))
    return wire


def wire_bytes(wire):
    return canonical_bytes(wire)


def check_wire(wire, bound, sheet, policy):
    """Refuse before any reservation: legacy keys, wrong model/limit, bytes over R_i."""
    if not isinstance(wire, dict):
        raise WireError("wire must be an object")
    bad = FORBIDDEN_WIRE_KEYS & set(wire)
    if bad:
        raise WireError(f"forbidden wire keys {sorted(bad)} (legacy max_tokens/temperature are rejected)")
    for key in ("max_completion_tokens", "reasoning_effort"):
        if key not in wire:
            raise WireError(f"wire lacks {key!r}")
    if wire.get("store") is not False or wire.get("stream") is not False:
        raise WireError("wire must carry store:false and stream:false")
    if wire["model"] != bound.model:
        raise WireError(f"wire model {wire['model']!r} differs from bound model {bound.model!r}")
    if wire["max_completion_tokens"] != bound.o_tokens:
        raise WireError(f"max_completion_tokens {wire['max_completion_tokens']} != O_i {bound.o_tokens}")
    if len(wire.get("messages", [])) != 2:
        raise WireError("exactly two messages are allowed")
    size = len(wire_bytes(wire)) + sheet.framing
    if policy == "p1":
        if bound.r_tokens > sheet.standard_tier_limit(bound.model):
            raise PriceUnknown(f"P1: R_i {bound.r_tokens} exceeds the standard tier of {bound.model}")
        if size > bound.r_tokens:
            raise WireError(f"wire bytes + FRAMING ({size}) exceed R_i {bound.r_tokens}")
    elif policy == "p2":
        window = sheet.context_window(bound.model)
        if window is None:
            raise PriceUnknown(f"P2: context window of {bound.model} is not published")
        if size > window:
            raise WireError(f"wire bytes + FRAMING ({size}) exceed the context window {window}")
    else:
        raise WireError(f"unknown bound policy {policy!r}")


# -- transport -----------------------------------------------------------
class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise WireError("completion redirects are forbidden")


class TransportError(RuntimeError):
    """No usable HTTP response: the request outcome is unknown."""


class Transport:
    """HTTPS POST to <base_url>/chat/completions; host allow-listed, no proxy, no redirects."""

    def __init__(self, base_url, key_env, allow_hosts, opener=None):
        parts = urlsplit(base_url)
        if parts.hostname not in allow_hosts:
            raise WireError(f"host {parts.hostname!r} is not in the allow-list")
        if parts.scheme != "https" and parts.hostname not in LOOPBACK_HOSTS:
            raise WireError("non-loopback providers require https")
        self.url = base_url.rstrip("/") + "/chat/completions"
        self.host = parts.hostname
        self.key_env = key_env
        self.opener = opener or build_opener(ProxyHandler({}), _NoRedirect())

    def __call__(self, body, timeout_s):
        key = os.environ.get(self.key_env)
        if not key:
            raise WireError(f"missing credential in {self.key_env}")
        request = Request(self.url, data=body, method="POST",
                          headers={"Content-Type": "application/json", "Authorization": "Bearer " + key})
        try:
            with self.opener.open(request, timeout=timeout_s) as response:
                return response.status, dict(response.headers), _read(response)
        except HTTPError as error:
            with error:
                return error.code, dict(error.headers), _read(error)
        except (URLError, socket.timeout, OSError, ConnectionError) as exc:
            raise TransportError(f"{type(exc).__name__}: {exc}") from exc


def _read(response):
    body = response.read(BODY_CAP + 1)
    if len(body) > BODY_CAP:
        raise TransportError("response larger than 4 MiB")
    return body


# -- parsing -------------------------------------------------------------
def _unique(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise WireError("duplicate key in response")
        out[key] = value
    return out


def parse_completion(body):
    """Strict parse of a 200 body: usage, model and exactly one choice are required."""
    try:
        data = json.loads(body.decode("utf-8"), object_pairs_hook=_unique)
    except (UnicodeDecodeError, ValueError) as exc:
        raise WireError("response is not JSON") from exc
    if not isinstance(data, dict):
        raise WireError("response is not an object")
    model = data.get("model")
    if not isinstance(model, str) or not model:
        raise WireError("response lacks model")
    text, finish = _choice_of(data)
    return {"text": text, "usage": _usage_of(data), "finish_reason": finish, "model": model,
            "outcome": "length" if finish == "length" else "ok"}


def _usage_of(data):
    usage = data.get("usage")
    if not isinstance(usage, dict):
        raise WireError("response lacks usage")
    counts = {k: usage.get(k) for k in ("prompt_tokens", "completion_tokens", "total_tokens")}
    if any(type(v) is not int or v < 0 for v in counts.values()):
        raise WireError("usage counts missing or invalid")
    if counts["total_tokens"] != counts["prompt_tokens"] + counts["completion_tokens"]:
        raise WireError("usage total inconsistent")
    details = usage.get("completion_tokens_details")
    counts["reasoning_tokens"] = details.get("reasoning_tokens") if isinstance(details, dict) else None
    cached = usage.get("prompt_tokens_details")
    counts["cached_tokens"] = cached.get("cached_tokens") if isinstance(cached, dict) else None
    return counts


def _choice_of(data):
    choices = data.get("choices")
    if not isinstance(choices, list) or len(choices) != 1 or not isinstance(choices[0], dict):
        raise WireError("expected exactly one choice")
    finish = choices[0].get("finish_reason")
    if not isinstance(finish, str):
        raise WireError("choice lacks finish_reason")
    text = (choices[0].get("message") or {}).get("content")
    if not isinstance(text, str):
        raise WireError("choice lacks message.content")
    return text, finish


def _error_object(body):
    try:
        data = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return None
    if isinstance(data, dict) and isinstance(data.get("error"), dict):
        return data["error"]
    return None


def actual_cost(sheet, model, usage, max_usd):
    """Billed cost at the tier of the observed prompt size; unpriced tier = full reservation."""
    pin, pout, threshold, long_in, long_out = sheet.require_priced(model)
    prompt, completion = usage["prompt_tokens"], usage["completion_tokens"]
    if prompt > threshold:
        if long_in is None or long_out is None:
            return Decimal(max_usd)
        pin, pout = long_in, long_out
    return (Decimal(prompt) * Decimal(str(pin)) + Decimal(completion) * Decimal(str(pout))) / Decimal(1_000_000)


# -- the paid call -------------------------------------------------------
def paid_completion(campaign, journal, sheet, *, letter, stage, binding, messages, response_format,
                    transport, bound, acknowledged=frozenset(), clock=time.time):
    state = journal.state
    blocking = state.blocking(acknowledged)
    if blocking:
        raise RunStopped(f"unsettled or uncertain requests block new calls: {blocking[:3]}")
    profile = campaign.profile(stage)
    wire = build_wire(profile, messages, response_format)
    check_wire(wire, bound, sheet, campaign.policy)
    body = wire_bytes(wire)
    wire_sha = sha256_hex(body)
    rid = request_id(campaign.campaign_id, binding, wire_sha)
    max_usd = sheet.max_usd(bound, campaign.policy)
    replay = journal.replay(rid)
    if replay is not None:
        return _replayed(campaign, journal, rid, replay, max_usd)
    if rid in state.intents:
        raise RunStopped(f"request {rid} was attempted before without settlement; use a new attempt")
    campaign.check_sublimit(letter, state.committed_by_letter()[letter], max_usd)
    token = campaign.ledger.reserve(rid, str(max_usd))
    journal.intent({"schema": 1, "request_id": rid, "campaign_id": campaign.campaign_id,
                    "created_at": clock(), "letter": letter, "stage": stage, "binding": binding,
                    "profile": {**profile, "service_tier": "default"}, "bound_policy": campaign.policy,
                    "r_tokens": bound.r_tokens, "o_tokens": bound.o_tokens, "max_usd": str(max_usd),
                    "wire_sha256": wire_sha, "wire_bytes": len(body), "ledger_token": token}, wire)
    timeout_s = campaign.spec["timeout_s"]
    journal.sent(rid, clock(), timeout_s, getattr(transport, "host", "injected"))
    started = clock()
    try:
        status, headers, raw = transport(body, timeout_s)
    except Exception as exc:  # any transport failure leaves the outcome unknown
        journal.uncertain(rid, clock(), type(exc).__name__, str(exc))
        raise RunStopped(f"uncertain request {rid}: {type(exc).__name__}") from exc
    exchange = {"rid": rid, "token": token, "status": status, "raw": raw, "max_usd": max_usd,
                "model_alias": profile["model"], "bound": bound, "started": started,
                "provider_rid": headers.get("x-request-id") if isinstance(headers, dict) else None}
    return _settle_exchange(campaign, journal, sheet, exchange, clock)


def _settle_exchange(campaign, journal, sheet, ex, clock):
    """Everything after the socket: classify, parse, pin, settle money, then check bounds."""
    rid, raw, status = ex["rid"], ex["raw"], ex["status"]
    record = {"status": status, "x_request_id": ex["provider_rid"], "body_text": raw.decode("utf-8", errors="replace")}
    base = {"http_status": status, "provider_request_id": ex["provider_rid"], "response_sha256": sha256_hex(raw)}
    if 400 <= status < 500 and _error_object(raw) is not None:
        journal.settled(rid, {**base, "settled_at": clock(), "model_observed": None, "usage": None,
                              "finish_reason": None, "outcome": "rejected_4xx", "actual_usd": "0",
                              "elapsed_ms": _ms(ex["started"], clock())}, record)
        campaign.ledger.settle(ex["token"], "0")
        raise Rejected4xx(f"provider rejected request {rid} with HTTP {status}")
    if status != 200:
        journal.uncertain(rid, clock(), f"http_{status}", record["body_text"][:500])
        raise RunStopped(f"uncertain request {rid}: HTTP {status}")
    try:
        parsed = parse_completion(raw)
    except WireError as exc:
        journal.uncertain(rid, clock(), "unreadable_response", str(exc))
        raise RunStopped(f"uncertain request {rid}: {exc}") from exc
    actual = actual_cost(sheet, ex["model_alias"], parsed["usage"], ex["max_usd"])
    drift = None
    try:
        journal.pin_model(ex["model_alias"], parsed["model"], rid, clock())
    except ModelDrift as exc:
        drift = exc
    journal.settled(rid, {**base, "settled_at": clock(), "model_observed": parsed["model"],
                          "usage": parsed["usage"], "finish_reason": parsed["finish_reason"],
                          "outcome": "model_drift" if drift else parsed["outcome"],
                          "actual_usd": str(actual), "elapsed_ms": _ms(ex["started"], clock())}, record)
    try:
        campaign.ledger.settle(ex["token"], str(actual))
    except BudgetError as exc:
        raise BoundBreach(f"request {rid}: {exc}") from exc
    if drift:
        raise drift
    usage, bound = parsed["usage"], ex["bound"]
    if usage["prompt_tokens"] > bound.r_tokens or usage["completion_tokens"] > bound.o_tokens:
        raise BoundBreach(f"request {rid}: usage {usage['prompt_tokens']}/{usage['completion_tokens']} "
                          f"exceeds bounds R={bound.r_tokens} O={bound.o_tokens} (paid, run stopped)")
    return Completion(rid, parsed["text"], usage, parsed["finish_reason"], parsed["model"],
                      ex["provider_rid"], parsed["outcome"], str(actual))


def _replayed(campaign, journal, rid, replay, max_usd):
    """Identical frozen input already settled: no transport, no new reservation."""
    settled, response = replay
    token = journal.state.intents[rid]["ledger_token"]
    campaign.ledger.verify_reservation(token, rid, str(max_usd))
    if settled["outcome"] not in ("ok", "length"):
        raise RunStopped(f"request {rid} settled with outcome {settled['outcome']!r}; cannot replay")
    parsed = parse_completion(response["body_text"].encode("utf-8"))
    return Completion(rid, parsed["text"], settled["usage"], settled["finish_reason"],
                      settled["model_observed"], settled.get("provider_request_id"),
                      settled["outcome"], settled["actual_usd"], replayed=True)


def _ms(started, now):
    return int(round((now - started) * 1000))
