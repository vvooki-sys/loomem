"""Exact pre-send input count: ``POST <base_url>/responses/input_tokens`` (plan §2, owner 2026-11-22).

``InputTokenCounter`` is the ``precount`` callable of ``paid_completion``: it is
consulted after the wire checks and before any reservation, so a count above
R_i stops the run without spending. The endpoint is documented without a
price (docs read 2026-11-22) and treated as unbilled, but every probe leaves a
create-only record under ``<campaign>/counts/`` so the report can list them.

Same transport rules as ``paid_openai_wire.Transport``: the host must be the
campaign's allowed host, https unless loopback, no proxy, no redirects, the
Bearer key comes from the environment variable named by the campaign and is
never written anywhere. The response is parsed strictly; anything that is not
``{"object": "response.input_tokens", "input_tokens": <int>}`` is ``WireError``.
"""
import hashlib
import json
import os
import socket
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import ProxyHandler, Request, build_opener

from paid_call_journal import JournalError, canonical_bytes, sha256_hex, write_create_only
from paid_openai_wire import BODY_CAP, LOOPBACK_HOSTS, WireError, _NoRedirect, wire_bytes

INPUT_TOKENS_PATH = "/responses/input_tokens"
OBJECT = "response.input_tokens"
CONTEXT_KEYS = ("stage", "arm", "question_id", "history_id", "session_id", "r_tokens")


# The record id excludes the binding's attempt: a --retry-rejected attempt reuses the earlier probe record,
# so report.input_tokens.probes does not count retries.
def count_record_id(context, wire_sha256):
    return sha256_hex(canonical_bytes({"context": context, "wire_sha256": wire_sha256}))


def parse_input_tokens(body):
    """Strict parse: a JSON object with the documented object tag and a nonnegative int."""
    try:
        data = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise WireError("input_tokens response is not JSON") from exc
    if not isinstance(data, dict) or data.get("object") != OBJECT:
        raise WireError("input_tokens response lacks object=response.input_tokens")
    count = data.get("input_tokens")
    if type(count) is not int or count < 0:
        raise WireError("input_tokens must be a nonnegative int")
    return count


class InputTokenCounter:
    def __init__(self, campaign, directory, base_url, key_env, *, context, opener=None, clock=time.time,
                 stage_by_system_sha256=None):
        parts = urlsplit(base_url)
        allowed = campaign.spec["provider"]["allowed_host"]
        if parts.hostname != allowed and parts.hostname not in LOOPBACK_HOSTS:
            raise WireError(f"input_tokens host {parts.hostname!r} is not the campaign's allowed host {allowed!r}")
        if parts.scheme != "https" and parts.hostname not in LOOPBACK_HOSTS:
            raise WireError("non-loopback input_tokens endpoints require https")
        if not isinstance(context, dict):
            raise WireError("context must be a dict")
        self.campaign = campaign
        self.directory = directory
        self.url = base_url.rstrip("/") + INPUT_TOKENS_PATH
        self.host = parts.hostname
        self.key_env = key_env
        self.opener = opener or build_opener(ProxyHandler({}), _NoRedirect())
        self.clock = clock
        self.context = {key: context.get(key) for key in CONTEXT_KEYS}
        self.stage_by_system = dict(stage_by_system_sha256 or {})
        self.probes = []

    def set_context(self, **fields):
        """stage/arm/question_id/history_id/session_id/r_tokens for the next records."""
        unknown = set(fields) - set(CONTEXT_KEYS)
        if unknown:
            raise WireError(f"unknown context fields {sorted(unknown)}")
        self.context.update(fields)

    # -- the probe -------------------------------------------------------
    def __call__(self, wire):
        if not isinstance(wire, dict) or not isinstance(wire.get("messages"), list) or "model" not in wire:
            raise WireError("precount needs a wire with model and messages")
        body = canonical_bytes({"model": wire["model"], "input": wire["messages"]})
        count = parse_input_tokens(self._post(body))
        context = self._context_for(wire)
        wire_sha = sha256_hex(wire_bytes(wire))
        record = {**context, "model": wire["model"], "input_tokens": count, "wire_sha256": wire_sha,
                  "at": self.clock()}
        self._record(count_record_id(context, wire_sha), record)
        self.probes.append(record)
        return count

    def _context_for(self, wire):
        context = dict(self.context)
        system = wire["messages"][0].get("content") if wire["messages"] else None
        if isinstance(system, str):
            digest = hashlib.sha256(system.encode("utf-8")).hexdigest()
            if digest in self.stage_by_system:
                context["stage"] = self.stage_by_system[digest]
        return context

    def _post(self, body):
        key = os.environ.get(self.key_env)
        if not key:
            raise WireError(f"missing credential in {self.key_env}")
        request = Request(self.url, data=body, method="POST",
                          headers={"Content-Type": "application/json", "Authorization": "Bearer " + key})
        timeout_s = self.campaign.spec["timeout_s"]
        try:
            with self.opener.open(request, timeout=timeout_s) as response:
                status, raw = response.status, response.read(BODY_CAP + 1)
        except HTTPError as error:
            with error:
                status, raw = error.code, error.read(BODY_CAP + 1)
        except (URLError, socket.timeout, OSError, ConnectionError) as exc:
            raise WireError(f"input_tokens probe failed before any reservation: {type(exc).__name__}") from exc
        if len(raw) > BODY_CAP:
            raise WireError("input_tokens response larger than 4 MiB")
        if status != 200:
            raise WireError(f"input_tokens probe answered HTTP {status}")
        return raw

    def _record(self, record_id, record):
        path = self.directory / "counts" / f"{record_id}.json"
        try:
            write_create_only(path, record)
        except JournalError:
            # The same frozen wire in the same context was counted before (a crash
            # between the probe and the reservation): the earlier record stands
            # when it agrees; a different count for identical input is a stop.
            previous = json.loads(path.read_text())
            if previous.get("input_tokens") != record["input_tokens"]:
                raise WireError(f"input_tokens differ from the earlier probe {record_id[:12]}")


def load_counts(directory):
    """All recorded probes of a campaign, sorted by time (reporting and preflight)."""
    counts = directory / "counts"
    if not counts.is_dir():
        return []
    records = [json.loads(p.read_text()) for p in counts.glob("*.json") if not p.name.startswith(".")]
    return sorted(records, key=lambda r: (r.get("at") or 0, r.get("wire_sha256") or ""))
