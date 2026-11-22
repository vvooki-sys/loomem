"""Durable per-request journal for paid calls (plan §2).

Every paid request leaves create-only files under ``<campaign>/calls/``:

  <rid>.intent.json    reservation made; wire frozen (plus <rid>.request.json)
  <rid>.sent.json      written and fsynced BEFORE the socket opens (no return)
  <rid>.settled.json   provider usage received (plus <rid>.response.json)
  <rid>.uncertain.json timeout / connection error / unreadable response / 5xx
  <rid>.void.json      intent that never reached ``sent`` (resume only)

Files are never overwritten: a write goes to a temp file, is fsynced, then
hard-linked to its final name (EEXIST = duplicate = stop) and the directory is
fsynced. ``request_id`` is derived from the campaign, the binding and the wire
hash, so the same frozen input always maps to the same durable record and the
ledger refuses a second reservation for it.

The model observed in each response is pinned per configured alias in
``pins/<alias>.json``; any later difference is ``ModelDrift``.
"""
from dataclasses import dataclass, field
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path

LETTERS = ("E", "F", "S", "A", "R")
_SUFFIXES = ("intent", "request", "sent", "response", "settled", "uncertain", "void")


class JournalError(RuntimeError):
    """The journal is inconsistent or the write would duplicate a record."""


class ModelDrift(JournalError):
    """The provider answered with a different model than the pinned one."""


def canonical_bytes(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def request_id(campaign_id, binding, wire_sha256):
    if not isinstance(campaign_id, str) or not campaign_id:
        raise JournalError("campaign_id required")
    if not isinstance(binding, dict) or not binding:
        raise JournalError("binding required")
    if not isinstance(wire_sha256, str) or len(wire_sha256) != 64:
        raise JournalError("wire_sha256 must be a 64-hex SHA-256")
    return sha256_hex(canonical_bytes({"campaign_id": campaign_id, "binding": binding,
                                       "wire_sha256": wire_sha256}))


def write_create_only(path, obj):
    """Atomic create-only JSON write: temp + fsync -> os.link -> unlink -> dir fsync."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp.{os.getpid()}")
    data = json.dumps(obj, sort_keys=True, ensure_ascii=False, allow_nan=False, indent=1) + "\n"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        try:
            os.link(temporary, path)
        except FileExistsError as exc:
            raise JournalError(f"journal record already exists: {path.name}") from exc
    finally:
        if temporary.exists():
            os.unlink(temporary)
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def _read(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError) as exc:
        raise JournalError(f"unreadable journal record {Path(path).name}") from exc


@dataclass
class JournalState:
    intents: dict = field(default_factory=dict)       # rid -> intent
    sent: set = field(default_factory=set)
    settled: dict = field(default_factory=dict)       # rid -> settled
    uncertain: dict = field(default_factory=dict)     # rid -> uncertain
    void: dict = field(default_factory=dict)          # rid -> void
    pins: dict = field(default_factory=dict)          # alias -> pin

    @property
    def pending_intents(self):
        """intent without sent and not voided: never sent, reservation still held."""
        return sorted(r for r in self.intents if r not in self.sent and r not in self.void)

    @property
    def unsettled_sent(self):
        """sent but neither settled nor marked uncertain (crash mid-request)."""
        return sorted(r for r in self.sent if r not in self.settled and r not in self.uncertain)

    @property
    def has_uncertain(self):
        return bool(self.uncertain)

    def blocking(self, acknowledged=frozenset()):
        """Request IDs that forbid any new paid call until explicitly acknowledged."""
        return sorted((set(self.unsettled_sent) | set(self.uncertain)) - set(acknowledged))

    def committed_by_letter(self):
        """max_usd for unsettled intents, actual_usd for settled: basis of sublimits."""
        totals = {letter: Decimal(0) for letter in LETTERS}
        for rid, intent in self.intents.items():
            letter = intent["letter"]
            settled = self.settled.get(rid)
            amount = settled["actual_usd"] if settled and settled["actual_usd"] is not None else intent["max_usd"]
            totals[letter] += Decimal(amount)
        return totals

    def committed_total(self):
        return sum(self.committed_by_letter().values(), Decimal(0))


class CallJournal:
    def __init__(self, directory):
        self.directory = Path(directory)
        self.calls = self.directory / "calls"
        self.pins_dir = self.directory / "pins"
        self.calls.mkdir(parents=True, exist_ok=True)
        self.pins_dir.mkdir(parents=True, exist_ok=True)
        self.state = self.load()

    # -- paths ---------------------------------------------------------
    def path(self, rid, suffix):
        if suffix not in _SUFFIXES:
            raise JournalError(f"unknown journal suffix {suffix!r}")
        return self.calls / f"{rid}.{suffix}.json"

    # -- writes (each advances the in-memory state only after the durable write) --
    def intent(self, entry, request_body):
        for key in ("request_id", "letter", "stage", "max_usd", "wire_sha256", "ledger_token", "profile"):
            if key not in entry:
                raise JournalError(f"intent lacks {key!r}")
        rid = entry["request_id"]
        if entry["letter"] not in LETTERS:
            raise JournalError(f"unknown letter {entry['letter']!r}")
        Decimal(entry["max_usd"])
        write_create_only(self.path(rid, "request"), request_body)
        write_create_only(self.path(rid, "intent"), entry)
        self.state.intents[rid] = entry

    def sent(self, rid, sent_at, timeout_s, host):
        self._require_intent(rid)
        write_create_only(self.path(rid, "sent"),
                          {"request_id": rid, "sent_at": sent_at, "timeout_s": timeout_s, "host": host})
        self.state.sent.add(rid)

    def settled(self, rid, result, response_body):
        self._require_sent(rid)
        for key in ("http_status", "model_observed", "usage", "finish_reason", "outcome", "actual_usd"):
            if key not in result:
                raise JournalError(f"settled lacks {key!r}")
        if result["actual_usd"] is not None:
            Decimal(result["actual_usd"])
        write_create_only(self.path(rid, "response"), response_body)
        write_create_only(self.path(rid, "settled"), {"request_id": rid, **result})
        self.state.settled[rid] = {"request_id": rid, **result}

    def uncertain(self, rid, at, error_type, detail=None):
        self._require_sent(rid)
        record = {"request_id": rid, "at": at, "error_type": error_type, "detail": detail}
        write_create_only(self.path(rid, "uncertain"), record)
        self.state.uncertain[rid] = record

    def void(self, rid, at, reason="intent_without_sent"):
        self._require_intent(rid)
        if rid in self.state.sent:
            raise JournalError("cannot void a sent request")
        record = {"request_id": rid, "at": at, "reason": reason}
        write_create_only(self.path(rid, "void"), record)
        self.state.void[rid] = record

    def void_unsent(self, at):
        """Resume step: every intent that never reached ``sent`` is voided (reservation stays)."""
        voided = []
        for rid in self.state.pending_intents:
            self.void(rid, at)
            voided.append(rid)
        return voided

    def pin_model(self, alias, observed, rid, at):
        """First observed value per alias is pinned; any other value is ModelDrift."""
        if not isinstance(observed, str) or not observed:
            raise ModelDrift(f"response for {alias!r} carries no model")
        pin = self.state.pins.get(alias)
        if pin is None:
            pin = {"alias": alias, "model": observed, "first_request_id": rid, "at": at}
            write_create_only(self.pins_dir / f"{alias}.json", pin)
            self.state.pins[alias] = pin
            return pin
        if pin["model"] != observed:
            raise ModelDrift(f"model for {alias!r} changed: pinned {pin['model']!r}, observed {observed!r}")
        return pin

    # -- reads ---------------------------------------------------------
    def load(self):
        state = JournalState()
        for file in sorted(self.calls.glob("*.json")):
            if file.name.startswith("."):
                continue
            stem = file.name[:-5]
            if "." not in stem:
                raise JournalError(f"unexpected journal file {file.name}")
            rid, suffix = stem.rsplit(".", 1)
            if suffix == "intent":
                state.intents[rid] = _read(file)
            elif suffix == "sent":
                state.sent.add(rid)
            elif suffix == "settled":
                state.settled[rid] = _read(file)
            elif suffix == "uncertain":
                state.uncertain[rid] = _read(file)
            elif suffix == "void":
                state.void[rid] = _read(file)
            elif suffix not in ("request", "response"):
                raise JournalError(f"unexpected journal file {file.name}")
        for rid in state.sent | set(state.settled) | set(state.uncertain) | set(state.void):
            if rid not in state.intents:
                raise JournalError(f"journal record without intent: {rid}")
        for rid in set(state.settled) | set(state.uncertain):
            if rid not in state.sent:
                raise JournalError(f"settlement without sent record: {rid}")
        if set(state.settled) & set(state.uncertain):
            raise JournalError("request both settled and uncertain")
        for file in sorted(self.pins_dir.glob("*.json")):
            pin = _read(file)
            state.pins[pin["alias"]] = pin
        return state

    def replay(self, rid):
        """Settled result and stored response for an identical frozen request, or None."""
        settled = self.state.settled.get(rid)
        if settled is None:
            return None
        return settled, _read(self.path(rid, "response"))

    def _require_intent(self, rid):
        if rid not in self.state.intents:
            raise JournalError(f"no intent for {rid}")

    def _require_sent(self, rid):
        if rid not in self.state.sent:
            raise JournalError(f"request {rid} was never sent")


def reconcile(ledger_totals, state):
    """The ledger is the only truth about money; the journal must agree with it exactly."""
    spent = sum((Decimal(s["actual_usd"]) for s in state.settled.values()
                 if s["actual_usd"] is not None), Decimal(0))
    reserved = sum((Decimal(i["max_usd"]) for rid, i in state.intents.items()
                    if rid not in state.settled or state.settled[rid]["actual_usd"] is None), Decimal(0))
    problems = []
    if ledger_totals["request_count"] != len(state.intents):
        problems.append("orphaned reservation: ledger request count differs from journal intents")
    if ledger_totals["spent_usd"] != spent:
        problems.append(f"spent mismatch: ledger {ledger_totals['spent_usd']} journal {spent}")
    if ledger_totals["reserved_usd"] != reserved:
        problems.append(f"reserved mismatch: ledger {ledger_totals['reserved_usd']} journal {reserved}")
    if problems:
        raise JournalError("; ".join(problems))
